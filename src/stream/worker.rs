//! The per-stream X11 worker thread.
//!
//! It owns the connection to one app's private X display and acts as:
//! - a minimal window manager (app windows fill the display, dialogs are centred),
//! - the capture loop (XDamage → changed rectangles → encoder thread),
//! - the input injector (XTest keyboard and pointer),
//! - the cursor and clipboard bridge,
//! - the display resizer (RandR modes on the X server).

use super::encoder::{self, Job, Tile};
use super::rect::{merge, Rect};
use super::{ClientMsg, Cmd, Out, Shared};
use crate::keymap;
use anyhow::{anyhow, Context, Result};
use base64::Engine;
use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;
use x11rb::connection::Connection;
use x11rb::protocol::damage::{self, ConnectionExt as _};
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, *};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::protocol::Event;
use x11rb::rust_connection::{DefaultStream, RustConnection};
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{CURRENT_TIME, NONE};

const MAX_INFLIGHT: u32 = 2;
const MODIFIER_CODES: [&str; 10] = [
    "ShiftLeft", "ShiftRight", "ControlLeft", "ControlRight", "AltLeft", "AltRight", "MetaLeft", "MetaRight", "OSLeft",
    "OSRight",
];
const LOSSLESS_MAX_AREA: i64 = 256 * 256;
const REFINE_AFTER: Duration = Duration::from_millis(300);
const REFINE_BUDGET: i64 = 1_000_000;

x11rb::atom_manager! {
    pub Atoms: AtomsCookie {
        CLIPBOARD,
        PRIMARY,
        UTF8_STRING,
        TARGETS,
        TEXT,
        STRING,
        INCR,
        STRYMEK_SEL,
        WM_STATE,
        WM_PROTOCOLS,
        WM_DELETE_WINDOW,
        WM_TRANSIENT_FOR,
        WM_NAME,
        RESOURCE_MANAGER,
        _NET_WM_NAME,
        _NET_WM_WINDOW_TYPE,
        _NET_WM_WINDOW_TYPE_DIALOG,
        _NET_WM_WINDOW_TYPE_UTILITY,
        _NET_WM_WINDOW_TYPE_SPLASH,
        _NET_WM_WINDOW_TYPE_TOOLBAR,
        _NET_WM_WINDOW_TYPE_NOTIFICATION,
        _NET_WM_STATE,
        _NET_WM_STATE_MAXIMIZED_VERT,
        _NET_WM_STATE_MAXIMIZED_HORZ,
        _NET_WM_STATE_FULLSCREEN,
        _NET_ACTIVE_WINDOW,
        _NET_SUPPORTED,
        _NET_SUPPORTING_WM_CHECK,
        _NET_CLIENT_LIST,
        _NET_FRAME_EXTENTS,
        _NET_WORKAREA,
        _NET_MOVERESIZE_WINDOW,
        CARDINAL,
        ATOM,
        WINDOW,
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Normal,
    Floating,
}

struct Managed {
    win: Window,
    kind: Kind,
    geom: Rect,
}

struct Client {
    id: u64,
    tx: UnboundedSender<Out>,
    sent_seq: u32,
    acked_seq: u32,
    hello: bool,
}

pub struct Setup {
    pub socket: std::path::PathBuf,
    pub cookie: Vec<u8>,
    pub scale: u32,
    pub max_fps: u32,
    pub quality: f32,
    /// Stream one existing window of your real desktop instead of a private display.
    pub desktop_window: Option<u32>,
}

/// Live streaming of one window on the real desktop (Xorg session).
struct Desk {
    win: Window,
    /// Window's client area in root coordinates.
    ox: i32,
    oy: i32,
    root_w: i32,
    root_h: i32,
    /// Geometry and maximised state before we resized it, restored on stop.
    orig: Rect,
    was_max: bool,
    resized: bool,
    active: bool,
    last_activate: Option<Instant>,
    /// Desktop UI scale (from Xft.dpi): device pixels per logical pixel.
    scale: u32,
    pending_since: Option<Instant>,
    /// The whole screen rather than one window.
    full: bool,
    /// Last polled screen contents (visible rect + BGRX pixels), to find
    /// changes the damage events miss (lock screen, compositor effects).
    snap: Option<(Rect, Vec<u8>)>,
    last_poll: Instant,
    last_input: Instant,
}

pub struct Worker {
    conn: RustConnection,
    root: Window,
    atoms: Atoms,
    sw: i32,
    sh: i32,
    scale: u32,
    damage: damage::Damage,
    region: xfixes::Region,
    helper: Window,
    keysyms: HashMap<u32, (u8, u8)>,
    spare_keycodes: Vec<u8>,
    spare_next: usize,
    keys_down: HashMap<String, u8>,
    buttons_down: Vec<u8>,
    cmd_as_ctrl: bool,
    managed: Vec<Managed>,
    focused: Option<Window>,
    title: String,
    client: Option<Client>,
    damaged: bool,
    pending: Vec<Rect>,
    lossy: Vec<Rect>,
    last_damage: Instant,
    last_send: Instant,
    frame_interval: Duration,
    quality: f32,
    enc_tx: Sender<Job>,
    clip_ours: Option<String>,
    clip_last_remote: String,
    pending_resize: Option<(i32, i32, Instant)>,
    created_modes: Vec<randr::Mode>,
    shared: Arc<Mutex<Shared>>,
    cursor_msg: Option<String>,
    last_release: Option<(u8, Instant)>,
    remapped_pending: bool,
    desk: Option<Desk>,
    quit: bool,
}

fn connect(setup: &Setup) -> Result<RustConnection> {
    let stream = UnixStream::connect(&setup.socket)
        .with_context(|| format!("connecting to {}", setup.socket.display()))?;
    let (stream, _) = DefaultStream::from_unix_stream(stream)?;
    let (name, data) = if setup.cookie.is_empty() {
        (Vec::new(), Vec::new())
    } else {
        (b"MIT-MAGIC-COOKIE-1".to_vec(), setup.cookie.clone())
    };
    let conn = RustConnection::connect_to_stream_with_auth_info(stream, 0, name, data)?;
    Ok(conn)
}

impl Worker {
    pub fn new(setup: &Setup, shared: Arc<Mutex<Shared>>, enc_tx: Sender<Job>) -> Result<Self> {
        let conn = connect(setup)?;
        let screen = conn.setup().roots[0].clone();
        let root = screen.root;
        let atoms = Atoms::new(&conn)?.reply()?;

        conn.damage_query_version(1, 1)?.reply()?;
        conn.xfixes_query_version(5, 0)?.reply()?;
        conn.xtest_get_version(2, 2)?.reply()?;
        let desktop = setup.desktop_window;
        if desktop.is_none() {
        conn.randr_query_version(1, 5)?.reply()?;

        // Become the window manager for this display.
        conn.change_window_attributes(
            root,
            &ChangeWindowAttributesAux::new().event_mask(
                EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE,
            ),
        )?
        .check()
        .map_err(|e| anyhow!("another window manager is running on this display: {e:?}"))?;
        }

        let helper = conn.generate_id()?;
        conn.create_window(
            0,
            helper,
            root,
            -10,
            -10,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            0,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )?;
        if desktop.is_none() {
        conn.change_property32(PropMode::REPLACE, root, atoms._NET_SUPPORTING_WM_CHECK, atoms.WINDOW, &[helper])?;
        conn.change_property32(PropMode::REPLACE, helper, atoms._NET_SUPPORTING_WM_CHECK, atoms.WINDOW, &[helper])?;
        conn.change_property8(PropMode::REPLACE, helper, atoms._NET_WM_NAME, atoms.UTF8_STRING, b"Strymek")?;
        conn.change_property32(
            PropMode::REPLACE,
            root,
            atoms._NET_SUPPORTED,
            atoms.ATOM,
            &[
                atoms._NET_SUPPORTED,
                atoms._NET_SUPPORTING_WM_CHECK,
                atoms._NET_ACTIVE_WINDOW,
                atoms._NET_CLIENT_LIST,
                atoms._NET_WM_NAME,
                atoms._NET_WM_STATE,
                atoms._NET_WM_STATE_MAXIMIZED_VERT,
                atoms._NET_WM_STATE_MAXIMIZED_HORZ,
                atoms._NET_WM_STATE_FULLSCREEN,
                atoms._NET_WM_WINDOW_TYPE,
                atoms._NET_FRAME_EXTENTS,
                atoms._NET_WORKAREA,
            ],
        )?;

        // Let processes running as this same Unix user connect even when they
        // cannot read the cookie file (snap/flatpak sandboxes). The X server
        // checks the peer's uid on the local socket; nobody else gets in.
        if let Some(user) = current_user() {
            let mut addr = b"localuser\0".to_vec();
            addr.extend_from_slice(user.as_bytes());
            let _ = conn.change_hosts(HostMode::INSERT, Family::SERVER_INTERPRETED, &addr)?.check();
        }

        // HiDPI: apps read Xft.dpi from RESOURCE_MANAGER.
        let dpi = 96 * setup.scale.max(1);
        let res = format!("Xft.dpi:\t{dpi}\nXft.antialias:\t1\nXft.hinting:\t1\nXft.hintstyle:\thintslight\nXft.rgba:\tnone\n");
        conn.change_property8(PropMode::REPLACE, root, atoms.RESOURCE_MANAGER, atoms.STRING, res.as_bytes())?;
        }

        // Damage. Private display: one region for the whole screen. Real desktop:
        // the compositor draws the screen, so watch every top-level window instead.
        let damage = conn.generate_id()?;
        if desktop.is_none() {
            conn.damage_create(damage, root, damage::ReportLevel::NON_EMPTY)?;
        } else {
            conn.change_window_attributes(
                root,
                &ChangeWindowAttributesAux::new().event_mask(EventMask::SUBSTRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE),
            )?;
            for child in conn.query_tree(root)?.reply()?.children {
                let d = conn.generate_id()?;
                let _ = conn.damage_create(d, child, damage::ReportLevel::RAW_RECTANGLES);
            }
        }
        let region = conn.generate_id()?;
        conn.xfixes_create_region(region, &[])?;
        conn.xfixes_select_cursor_input(root, xfixes::CursorNotifyMask::DISPLAY_CURSOR)?;
        conn.xfixes_select_selection_input(
            helper,
            atoms.CLIPBOARD,
            xfixes::SelectionEventMask::SET_SELECTION_OWNER,
        )?;

        let (sw, sh) = (screen.width_in_pixels as i32, screen.height_in_pixels as i32);
        let mut w = Worker {
            conn,
            root,
            atoms,
            sw,
            sh,
            scale: setup.scale.max(1),
            damage,
            region,
            helper,
            keysyms: HashMap::new(),
            spare_keycodes: Vec::new(),
            spare_next: 0,
            keys_down: HashMap::new(),
            buttons_down: Vec::new(),
            cmd_as_ctrl: true,
            managed: Vec::new(),
            focused: None,
            title: String::new(),
            client: None,
            damaged: false,
            pending: Vec::new(),
            lossy: Vec::new(),
            last_damage: Instant::now(),
            last_send: Instant::now(),
            frame_interval: Duration::from_millis(1000 / setup.max_fps.clamp(1, 120) as u64),
            quality: setup.quality,
            enc_tx,
            clip_ours: None,
            clip_last_remote: String::new(),
            pending_resize: None,
            created_modes: Vec::new(),
            shared,
            cursor_msg: None,
            last_release: None,
            remapped_pending: false,
            desk: None,
            quit: false,
        };
        w.load_keymap()?;
        if let Some(win) = desktop {
            w.init_desktop(win, screen.width_in_pixels as i32, screen.height_in_pixels as i32)?;
        } else {
            w.set_workarea()?;
        }
        w.conn.flush()?;
        w.update_shared();
        Ok(w)
    }

    fn load_keymap(&mut self) -> Result<()> {
        let (min, max) = (self.conn.setup().min_keycode, self.conn.setup().max_keycode);
        let km = self.conn.get_keyboard_mapping(min, max - min + 1)?.reply()?;
        let per = km.keysyms_per_keycode as usize;
        self.keysyms.clear();
        self.spare_keycodes.clear();
        for level in 0..2usize.min(per) {
            for i in 0..=(max - min) as usize {
                let ks = km.keysyms[i * per + level];
                if ks != 0 {
                    self.keysyms.entry(ks).or_insert((min + i as u8, level as u8));
                }
            }
        }
        for i in 0..=(max - min) as usize {
            if km.keysyms[i * per..(i + 1) * per].iter().all(|&k| k == 0) {
                self.spare_keycodes.push(min + i as u8);
            }
        }
        // Keep a handful near the top of the range for on-the-fly remapping.
        let n = self.spare_keycodes.len();
        if n > 8 {
            self.spare_keycodes.drain(..n - 8);
        }
        Ok(())
    }

    fn set_workarea(&self) -> Result<()> {
        self.conn.change_property32(
            PropMode::REPLACE,
            self.root,
            self.atoms._NET_WORKAREA,
            self.atoms.CARDINAL,
            &[0, 0, self.sw as u32, self.sh as u32],
        )?;
        Ok(())
    }

    fn update_shared(&self) {
        let mut s = self.shared.lock().unwrap();
        let prev = s.windows;
        s.windows = if self.desk.is_some() { 1 } else { self.managed.len() };
        if s.windows > 0 || prev > 0 {
            s.ever_had_window = true;
            s.last_window_seen = Some(Instant::now());
        }
        s.title = self.title.clone();
        s.width = self.sw as u32;
        s.height = self.sh as u32;
        s.attached = self.client.is_some();
    }

    // ---------- main loop ----------

    pub fn run(mut self, rx: Receiver<Cmd>, wake_fd: i32) {
        if let Err(e) = self.run_inner(rx, wake_fd) {
            tracing::warn!("stream worker ended: {e:#}");
        }
        if self.desk.is_some() {
            let _ = self.release_all();
            let _ = self.desk_restore();
            let _ = self.conn.flush();
        }
        if let Some(c) = &self.client {
            let _ = c.tx.send(Out::Close);
        }
        self.shared.lock().unwrap().dead = true;
    }

    fn run_inner(&mut self, rx: Receiver<Cmd>, wake_fd: i32) -> Result<()> {
        let xfd = self.conn.stream().as_raw_fd();
        loop {
            // Drain events already buffered by earlier replies before sleeping.
            let mut busy = self.drain_events()?;
            let timeout_ms = if busy { 0 } else { self.next_timeout_ms() };
            let mut fds = [
                libc::pollfd { fd: xfd, events: libc::POLLIN, revents: 0 },
                libc::pollfd { fd: wake_fd, events: libc::POLLIN, revents: 0 },
            ];
            unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout_ms) };
            if fds[1].revents & libc::POLLIN != 0 {
                let mut buf = [0u8; 8];
                unsafe { libc::read(wake_fd, buf.as_mut_ptr() as *mut _, 8) };
            }
            loop {
                match rx.try_recv() {
                    Ok(Cmd::Shutdown) => return Ok(()),
                    Ok(cmd) => {
                        busy = true;
                        if let Err(e) = self.handle_cmd(cmd) {
                            tracing::debug!("command failed: {e:#}");
                        }
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => return Ok(()),
                }
            }
            let _ = busy;
            self.drain_events()?;
            if let Some((w, h, at)) = self.pending_resize {
                if Instant::now() >= at {
                    self.pending_resize = None;
                    if let Err(e) = self.apply_resize(w, h) {
                        tracing::warn!("resize to {w}x{h} failed: {e:#}");
                    }
                }
            }
            self.maybe_send_frame()?;
            self.conn.flush()?;
            if self.quit {
                return Ok(());
            }
        }
    }

    fn next_timeout_ms(&self) -> i32 {
        let mut t = 1000i64;
        if let (Some(d), true) = (&self.desk, self.client.is_some()) {
            let due = self.poll_interval().saturating_sub(d.last_poll.elapsed());
            t = t.min(due.as_millis() as i64 + 1);
        }
        if let Some((_, _, at)) = self.pending_resize {
            t = t.min(at.saturating_duration_since(Instant::now()).as_millis() as i64);
        }
        if self.client.is_some() && (self.damaged || !self.pending.is_empty() || !self.lossy.is_empty()) {
            t = t.min(self.frame_interval.as_millis() as i64 / 2).max(1);
            if !self.lossy.is_empty() && !self.damaged && self.pending.is_empty() {
                t = t.min(REFINE_AFTER.as_millis() as i64 / 3);
            }
        }
        t.max(0) as i32
    }

    fn drain_events(&mut self) -> Result<bool> {
        let mut any = false;
        while let Some(ev) = self.conn.poll_for_event()? {
            any = true;
            if let Err(e) = self.handle_event(ev) {
                tracing::debug!("event handling error: {e:#}");
            }
        }
        Ok(any)
    }

    // ---------- client commands ----------

    fn send(&self, msg: Out) {
        if let Some(c) = &self.client {
            let _ = c.tx.send(msg);
        }
    }

    fn send_json(&self, v: serde_json::Value) {
        self.send(Out::Text(v.to_string()));
    }

    fn send_config(&self) {
        let mut v = serde_json::json!({"t":"cfg","w":self.sw,"h":self.sh,"scale":self.scale,"title":self.title});
        if let Some(d) = &self.desk {
            // Pixels per CSS pixel: desktop apps draw at the desktop's own scale.
            v["ppc"] = serde_json::json!(d.scale);
            v["desktop"] = serde_json::json!(true);
            v["fit"] = serde_json::json!(d.full);
        }
        self.send_json(v);
    }

    fn full_refresh(&mut self) {
        self.pending = vec![Rect::new(0, 0, self.sw, self.sh)];
        self.lossy.clear();
    }

    fn handle_cmd(&mut self, cmd: Cmd) -> Result<()> {
        match cmd {
            Cmd::Attach { id, tx } => {
                if let Some(old) = self.client.take() {
                    let _ = old.tx.send(Out::Text(r#"{"t":"bye","reason":"This app was opened in another window."}"#.into()));
                    let _ = old.tx.send(Out::Close);
                }
                self.client = Some(Client { id, tx, sent_seq: 0, acked_seq: 0, hello: false });
                self.update_shared();
            }
            Cmd::Detach { id } => {
                if self.client.as_ref().map(|c| c.id) == Some(id) {
                    self.client = None;
                    self.release_all()?;
                    self.update_shared();
                }
            }
            Cmd::Thumb { reply } => {
                let _ = reply.send(self.thumbnail().ok());
            }
            Cmd::Client { id, msg } => {
                if self.client.as_ref().map(|c| c.id) != Some(id) {
                    return Ok(());
                }
                self.handle_client(msg)?;
            }
            Cmd::Shutdown => {}
        }
        Ok(())
    }

    fn handle_client(&mut self, msg: ClientMsg) -> Result<()> {
        match msg {
            ClientMsg::Hello { w, h, cmd_ctrl, dpr } => {
                if let Some(v) = cmd_ctrl {
                    self.cmd_as_ctrl = v;
                }
                if let Some(c) = &mut self.client {
                    c.hello = true;
                }
                let (w, h) = self.requested_size(w, h, dpr);
                if (w, h) != (self.sw, self.sh) {
                    self.apply_resize(w, h)?;
                } else {
                    self.send_config();
                    self.full_refresh();
                }
                if let Some(c) = &self.cursor_msg {
                    self.send(Out::Text(c.clone()));
                } else {
                    self.send_cursor()?;
                }
            }
            ClientMsg::Resize { w, h, dpr } => {
                let (w, h) = self.requested_size(w, h, dpr);
                self.pending_resize = Some((w, h, Instant::now() + Duration::from_millis(120)));
            }
            ClientMsg::Ack { s } => {
                if let Some(c) = &mut self.client {
                    if s > c.acked_seq && s <= c.sent_seq {
                        c.acked_seq = s;
                    }
                }
            }
            ClientMsg::Refresh => self.full_refresh(),
            ClientMsg::Options { cmd_ctrl } => self.cmd_as_ctrl = cmd_ctrl,
            ClientMsg::Move { x, y } => self.motion(x, y)?,
            ClientMsg::Button { b, d, x, y } => {
                self.motion(x, y)?;
                if d {
                    self.focus_at(x, y)?;
                }
                self.button(b, d)?;
            }
            ClientMsg::Wheel { v, h, x, y } => {
                self.motion(x, y)?;
                let (vb, hb) = (if v > 0 { 5 } else { 4 }, if h > 0 { 7 } else { 6 });
                for _ in 0..v.unsigned_abs().min(20) {
                    self.button(vb, true)?;
                    self.button(vb, false)?;
                }
                for _ in 0..h.unsigned_abs().min(20) {
                    self.button(hb, true)?;
                    self.button(hb, false)?;
                }
            }
            ClientMsg::Key { code, key, d } => self.key(&code, &key, d)?,
            ClientMsg::Text { text } => {
                for ch in text.chars().take(4096) {
                    self.type_char(ch)?;
                }
            }
            ClientMsg::ReleaseAll => self.release_all()?,
            ClientMsg::Clip { text } => self.set_clipboard(text)?,
        }
        Ok(())
    }

    // ---------- input ----------

    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) -> Result<()> {
        self.fake_delayed(kind, detail, x, y, 0)
    }

    /// XTest can delay an event by `delay_ms`; the server holds our later requests
    /// until then, so ordering is kept.
    fn fake_delayed(&self, kind: u8, detail: u8, x: i16, y: i16, delay_ms: u32) -> Result<()> {
        self.conn.xtest_fake_input(kind, detail, delay_ms, self.root, x, y, 0)?;
        Ok(())
    }

    fn motion(&mut self, x: i32, y: i32) -> Result<()> {
        if let Some(d) = &mut self.desk {
            d.last_input = Instant::now();
        }
        let (ox, oy) = self.origin();
        let x = (ox + x.clamp(0, self.sw - 1)) as i16;
        let y = (oy + y.clamp(0, self.sh - 1)) as i16;
        self.fake(xproto::MOTION_NOTIFY_EVENT, 0, x, y)
    }

    fn origin(&self) -> (i32, i32) {
        self.desk.as_ref().map(|d| (d.ox, d.oy)).unwrap_or((0, 0))
    }

    fn button(&mut self, b: u8, down: bool) -> Result<()> {
        if b == 0 {
            return Ok(());
        }
        if down {
            if !self.buttons_down.contains(&b) {
                self.buttons_down.push(b);
            }
        } else {
            self.buttons_down.retain(|&x| x != b);
        }
        let kind = if down { xproto::BUTTON_PRESS_EVENT } else { xproto::BUTTON_RELEASE_EVENT };
        self.fake(kind, b, 0, 0)
    }

    fn key_event(&mut self, kc: u8, down: bool) -> Result<()> {
        let kind = if down { xproto::KEY_PRESS_EVENT } else { xproto::KEY_RELEASE_EVENT };
        let mut delay = 0;
        if down {
            // Toolkits treat release+press of one key at the same timestamp as
            // auto-repeat and may drop it; nudge fast double letters apart.
            if let Some((last, at)) = self.last_release {
                if last == kc && at.elapsed() < Duration::from_millis(5) {
                    delay = 2;
                }
            }
            // After remapping a keycode, give apps time to see the new mapping.
            if self.remapped_pending {
                self.remapped_pending = false;
                delay = delay.max(25);
            }
        } else {
            self.last_release = Some((kc, Instant::now()));
        }
        self.fake_delayed(kind, kc, 0, 0, delay)
    }

    /// Map a keysym onto a spare keycode so it can be typed.
    fn remap_spare(&mut self, ks: u32) -> Result<Option<u8>> {
        if self.spare_keycodes.is_empty() {
            return Ok(None);
        }
        let kc = self.spare_keycodes[self.spare_next % self.spare_keycodes.len()];
        self.spare_next += 1;
        self.conn.change_keyboard_mapping(1, kc, 2, &[ks, ks])?.check()?;
        self.keysyms.retain(|_, v| v.0 != kc);
        self.keysyms.insert(ks, (kc, 0));
        self.remapped_pending = true;
        Ok(Some(kc))
    }

    fn key(&mut self, code: &str, key: &str, down: bool) -> Result<()> {
        if code == "CapsLock" {
            return Ok(()); // macOS reports Caps Lock as a toggle, not press/release; ignore it.
        }
        if !down {
            if let Some(kc) = self.keys_down.remove(code) {
                self.key_event(kc, false)?;
            }
            return Ok(());
        }
        self.desk_ensure_active()?;
        // Printable characters (sent by the browser only when no Ctrl/Alt/Cmd is held)
        // go by character, so any Mac keyboard layout types what its keys show.
        // Everything else goes by physical key position.
        let mut chars = key.chars();
        let single = match (chars.next(), chars.next()) {
            (Some(c), None) if !c.is_control() => Some(c),
            _ => None,
        };
        let by_char = single.is_some() && !MODIFIER_CODES.contains(&code);
        let ks = match single {
            Some(c) if by_char => Some(keymap::char_to_keysym(c)),
            _ => keymap::code_to_keysym(code, self.cmd_as_ctrl),
        };
        let Some(ks) = ks else { return Ok(()) };
        let (kc, level) = match self.keysyms.get(&ks) {
            Some(&v) => v,
            None => match self.remap_spare(ks)? {
                Some(kc) => (kc, 0),
                None => return Ok(()),
            },
        };
        let shift_kc = self.keysyms.get(&keymap::SHIFT_L).map(|v| v.0);
        let shift_held = self.keys_down.keys().any(|k| k.starts_with("Shift"));
        let temp_shift = by_char && level == 1 && !shift_held && shift_kc.is_some();
        if temp_shift {
            self.key_event(shift_kc.unwrap(), true)?;
        }
        self.keys_down.insert(code.to_string(), kc);
        self.key_event(kc, true)?;
        if temp_shift {
            self.key_event(shift_kc.unwrap(), false)?;
        }
        Ok(())
    }

    fn type_char(&mut self, c: char) -> Result<()> {
        let ks = keymap::char_to_keysym(c);
        let (kc, level) = match self.keysyms.get(&ks) {
            Some(&v) => v,
            None => match self.remap_spare(ks)? {
                Some(kc) => (kc, 0),
                None => return Ok(()),
            },
        };
        let shift = self.keysyms.get(&keymap::SHIFT_L).map(|v| v.0);
        let need_shift = level == 1 && shift.is_some();
        if need_shift {
            self.key_event(shift.unwrap(), true)?;
        }
        self.key_event(kc, true)?;
        self.key_event(kc, false)?;
        if need_shift {
            self.key_event(shift.unwrap(), false)?;
        }
        Ok(())
    }

    fn release_all(&mut self) -> Result<()> {
        let keys: Vec<u8> = self.keys_down.drain().map(|(_, kc)| kc).collect();
        for kc in keys {
            self.key_event(kc, false)?;
        }
        let buttons = std::mem::take(&mut self.buttons_down);
        for b in buttons {
            self.fake(xproto::BUTTON_RELEASE_EVENT, b, 0, 0)?;
        }
        Ok(())
    }

    // ---------- window management ----------

    fn classify(&self, win: Window) -> Result<Kind> {
        let tr = self
            .conn
            .get_property(false, win, self.atoms.WM_TRANSIENT_FOR, AtomEnum::WINDOW, 0, 1)?
            .reply()?;
        if tr.value_len > 0 {
            return Ok(Kind::Floating);
        }
        let ty = self
            .conn
            .get_property(false, win, self.atoms._NET_WM_WINDOW_TYPE, AtomEnum::ATOM, 0, 16)?
            .reply()?;
        if let Some(types) = ty.value32() {
            for t in types {
                let a = &self.atoms;
                if [
                    a._NET_WM_WINDOW_TYPE_DIALOG,
                    a._NET_WM_WINDOW_TYPE_UTILITY,
                    a._NET_WM_WINDOW_TYPE_SPLASH,
                    a._NET_WM_WINDOW_TYPE_TOOLBAR,
                    a._NET_WM_WINDOW_TYPE_NOTIFICATION,
                ]
                .contains(&t)
                {
                    return Ok(Kind::Floating);
                }
            }
        }
        Ok(Kind::Normal)
    }

    fn place(&self, kind: Kind, want: Rect) -> Rect {
        match kind {
            Kind::Normal => Rect::new(0, 0, self.sw, self.sh),
            Kind::Floating => {
                let w = want.w.clamp(1, self.sw);
                let h = want.h.clamp(1, self.sh);
                Rect::new((self.sw - w) / 2, (self.sh - h) / 2, w, h)
            }
        }
    }

    fn apply_geom(&self, win: Window, r: Rect) -> Result<()> {
        self.conn.configure_window(
            win,
            &ConfigureWindowAux::new()
                .x(r.x)
                .y(r.y)
                .width(r.w.max(1) as u32)
                .height(r.h.max(1) as u32)
                .border_width(0),
        )?;
        Ok(())
    }

    fn manage(&mut self, win: Window) -> Result<()> {
        let kind = self.classify(win).unwrap_or(Kind::Normal);
        let g = self.conn.get_geometry(win)?.reply()?;
        let geom = self.place(kind, Rect::new(g.x as i32, g.y as i32, g.width as i32, g.height as i32));
        self.conn.change_window_attributes(
            win,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )?;
        self.apply_geom(win, geom)?;
        let a = &self.atoms;
        self.conn.change_property32(PropMode::REPLACE, win, a.WM_STATE, a.WM_STATE, &[1, 0])?;
        self.conn.change_property32(PropMode::REPLACE, win, a._NET_FRAME_EXTENTS, a.CARDINAL, &[0, 0, 0, 0])?;
        if kind == Kind::Normal {
            self.conn.change_property32(
                PropMode::REPLACE,
                win,
                a._NET_WM_STATE,
                a.ATOM,
                &[a._NET_WM_STATE_MAXIMIZED_VERT, a._NET_WM_STATE_MAXIMIZED_HORZ],
            )?;
        }
        self.conn.map_window(win)?;
        self.managed.retain(|m| m.win != win);
        self.managed.push(Managed { win, kind, geom });
        self.raise_focus(win)?;
        self.update_client_list()?;
        Ok(())
    }

    fn unmanage(&mut self, win: Window) -> Result<()> {
        let before = self.managed.len();
        self.managed.retain(|m| m.win != win);
        if self.managed.len() == before {
            return Ok(());
        }
        if self.focused == Some(win) {
            self.focused = None;
            if let Some(top) = self.managed.last().map(|m| m.win) {
                self.raise_focus(top)?;
            } else {
                self.set_title(String::new());
            }
        }
        self.update_client_list()?;
        self.update_shared();
        Ok(())
    }

    fn update_client_list(&self) -> Result<()> {
        let list: Vec<u32> = self.managed.iter().map(|m| m.win).collect();
        self.conn
            .change_property32(PropMode::REPLACE, self.root, self.atoms._NET_CLIENT_LIST, self.atoms.WINDOW, &list)?;
        Ok(())
    }

    fn raise_focus(&mut self, win: Window) -> Result<()> {
        self.conn
            .configure_window(win, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE))?;
        if let Some(pos) = self.managed.iter().position(|m| m.win == win) {
            let m = self.managed.remove(pos);
            self.managed.push(m);
        }
        self.conn.set_input_focus(InputFocus::PARENT, win, CURRENT_TIME)?;
        self.conn
            .change_property32(PropMode::REPLACE, self.root, self.atoms._NET_ACTIVE_WINDOW, self.atoms.WINDOW, &[win])?;
        self.focused = Some(win);
        let t = self.read_title(win).unwrap_or_default();
        // Dialogs often have no title; keep the main window's.
        if !t.is_empty() || self.managed.iter().find(|m| m.win == win).map(|m| m.kind) == Some(Kind::Normal) {
            self.set_title(t);
        }
        self.update_shared();
        Ok(())
    }

    fn focus_at(&mut self, x: i32, y: i32) -> Result<()> {
        if self.desk.is_some() {
            return self.desk_ensure_active();
        }
        let hit = self
            .managed
            .iter()
            .rev()
            .find(|m| x >= m.geom.x && x < m.geom.x + m.geom.w && y >= m.geom.y && y < m.geom.y + m.geom.h)
            .map(|m| m.win);
        if let Some(w) = hit {
            if self.focused != Some(w) {
                self.raise_focus(w)?;
            }
        }
        Ok(())
    }

    fn read_title(&self, win: Window) -> Result<String> {
        let r = self
            .conn
            .get_property(false, win, self.atoms._NET_WM_NAME, self.atoms.UTF8_STRING, 0, 1024)?
            .reply()?;
        if !r.value.is_empty() {
            return Ok(String::from_utf8_lossy(&r.value).to_string());
        }
        let r = self
            .conn
            .get_property(false, win, AtomEnum::WM_NAME, AtomEnum::ANY, 0, 1024)?
            .reply()?;
        Ok(String::from_utf8_lossy(&r.value).to_string())
    }

    fn set_title(&mut self, t: String) {
        if t != self.title {
            self.title = t;
            self.send_json(serde_json::json!({"t":"title","v":self.title}));
            self.update_shared();
        }
    }

    fn relayout(&mut self) -> Result<()> {
        let items: Vec<(Window, Kind, Rect)> = self.managed.iter().map(|m| (m.win, m.kind, m.geom)).collect();
        for (win, kind, g) in items {
            let ng = self.place(kind, g);
            self.apply_geom(win, ng)?;
            if let Some(m) = self.managed.iter_mut().find(|m| m.win == win) {
                m.geom = ng;
            }
        }
        Ok(())
    }

    // ---------- events ----------

    fn handle_event(&mut self, ev: Event) -> Result<()> {
        match ev {
            Event::DamageNotify(e) => {
                self.last_damage = Instant::now();
                if let Some(d) = &mut self.desk {
                    // Raw rectangles from one top-level window, in root coordinates.
                    let r = Rect::new(
                        e.geometry.x as i32 + e.area.x as i32 - d.ox,
                        e.geometry.y as i32 + e.area.y as i32 - d.oy,
                        e.area.width as i32,
                        e.area.height as i32,
                    );
                    if r.intersects(&Rect::new(0, 0, self.sw, self.sh)) {
                        d.pending_since.get_or_insert_with(Instant::now);
                        self.pending.push(r);
                        if self.pending.len() > 256 {
                            self.pending = merge(std::mem::take(&mut self.pending), self.sw, self.sh);
                        }
                    }
                } else {
                    self.damaged = true;
                }
            }
            Event::CreateNotify(e) if self.desk.is_some() && e.parent == self.root => {
                let d = self.conn.generate_id()?;
                let _ = self.conn.damage_create(d, e.window, damage::ReportLevel::RAW_RECTANGLES);
            }
            Event::ConfigureNotify(_) if self.desk.is_some() => self.desk_refresh_geometry()?,
            Event::DestroyNotify(e) if self.desk.as_ref().map(|d| d.win) == Some(e.window) => {
                tracing::info!("desktop window closed; ending its stream");
                self.quit = true;
            }
            Event::MapRequest(e) => self.manage(e.window)?,
            Event::ConfigureRequest(e) => self.configure_request(e)?,
            Event::UnmapNotify(e) => {
                if e.event == self.root {
                    self.unmanage(e.window)?;
                }
            }
            Event::DestroyNotify(e) => self.unmanage(e.window)?,
            Event::ClientMessage(e) => {
                if e.type_ == self.atoms._NET_ACTIVE_WINDOW && self.managed.iter().any(|m| m.win == e.window) {
                    self.raise_focus(e.window)?;
                }
            }
            Event::PropertyNotify(e) if self.desk.is_some() => {
                let a = self.atoms._NET_ACTIVE_WINDOW;
                let (win, root) = (self.desk.as_ref().unwrap().win, self.root);
                if e.window == root && e.atom == a {
                    let active = self.active_window().unwrap_or(0) == win;
                    self.desk.as_mut().unwrap().active = active;
                } else if e.window == win
                    && (e.atom == self.atoms._NET_WM_NAME || e.atom == u32::from(AtomEnum::WM_NAME))
                {
                    let t = self.read_title(win).unwrap_or_default();
                    self.set_title(t);
                }
            }
            Event::PropertyNotify(e) => {
                if Some(e.window) == self.focused
                    && (e.atom == self.atoms._NET_WM_NAME || e.atom == u32::from(AtomEnum::WM_NAME))
                {
                    let t = self.read_title(e.window).unwrap_or_default();
                    self.set_title(t);
                }
            }
            Event::XfixesCursorNotify(_) => self.send_cursor()?,
            Event::XfixesSelectionNotify(e) => {
                if e.owner != self.helper && e.owner != NONE && e.selection == self.atoms.CLIPBOARD {
                    self.conn.convert_selection(
                        self.helper,
                        self.atoms.CLIPBOARD,
                        self.atoms.UTF8_STRING,
                        self.atoms.STRYMEK_SEL,
                        e.selection_timestamp,
                    )?;
                }
            }
            Event::SelectionNotify(e) => {
                if e.requestor == self.helper && e.property != NONE {
                    self.read_remote_clipboard()?;
                }
            }
            Event::SelectionRequest(e) => self.selection_request(e)?,
            Event::SelectionClear(_) => {}
            Event::MappingNotify(_) => {
                // Our own keymap changes also land here; reloading keeps the table in sync.
                let spare = (self.spare_keycodes.clone(), self.spare_next);
                self.load_keymap()?;
                if self.spare_keycodes.is_empty() {
                    self.spare_keycodes = spare.0;
                }
                self.spare_next = spare.1;
            }
            Event::Error(e) => tracing::debug!("X error: {e:?}"),
            _ => {}
        }
        Ok(())
    }

    fn configure_request(&mut self, e: ConfigureRequestEvent) -> Result<()> {
        let idx = self.managed.iter().position(|m| m.win == e.window);
        match idx {
            None => {
                // Not managed yet: let the app size itself before mapping.
                let mut aux = ConfigureWindowAux::new();
                let m = e.value_mask;
                if m.contains(ConfigWindow::X) { aux = aux.x(e.x as i32); }
                if m.contains(ConfigWindow::Y) { aux = aux.y(e.y as i32); }
                if m.contains(ConfigWindow::WIDTH) { aux = aux.width(e.width as u32); }
                if m.contains(ConfigWindow::HEIGHT) { aux = aux.height(e.height as u32); }
                if m.contains(ConfigWindow::BORDER_WIDTH) { aux = aux.border_width(e.border_width as u32); }
                if m.contains(ConfigWindow::STACK_MODE) { aux = aux.stack_mode(e.stack_mode); }
                self.conn.configure_window(e.window, &aux)?;
            }
            Some(i) if self.managed[i].kind == Kind::Floating => {
                let g = self.managed[i].geom;
                let m = e.value_mask;
                let w = if m.contains(ConfigWindow::WIDTH) { e.width as i32 } else { g.w };
                let h = if m.contains(ConfigWindow::HEIGHT) { e.height as i32 } else { g.h };
                let ng = self.place(Kind::Floating, Rect::new(0, 0, w, h));
                self.managed[i].geom = ng;
                self.apply_geom(e.window, ng)?;
            }
            Some(i) => {
                // Main windows always fill the display; tell the app where it really is.
                let g = self.managed[i].geom;
                let ev = ConfigureNotifyEvent {
                    response_type: xproto::CONFIGURE_NOTIFY_EVENT,
                    sequence: 0,
                    event: e.window,
                    window: e.window,
                    above_sibling: NONE,
                    x: g.x as i16,
                    y: g.y as i16,
                    width: g.w as u16,
                    height: g.h as u16,
                    border_width: 0,
                    override_redirect: false,
                };
                self.conn.send_event(false, e.window, EventMask::STRUCTURE_NOTIFY, ev)?;
            }
        }
        Ok(())
    }

    // ---------- cursor ----------

    fn send_cursor(&mut self) -> Result<()> {
        let img = self.conn.xfixes_get_cursor_image()?.reply()?;
        let (w, h) = (img.width as u32, img.height as u32);
        if w == 0 || h == 0 {
            return Ok(());
        }
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for &p in &img.cursor_image {
            let a = (p >> 24) & 0xff;
            let un = |c: u32| if a == 0 { 0 } else { ((c * 255 + a / 2) / a).min(255) as u8 };
            rgba.extend_from_slice(&[un((p >> 16) & 0xff), un((p >> 8) & 0xff), un(p & 0xff), a as u8]);
        }
        let mut png_bytes = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut png_bytes, w, h);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut wr = enc.write_header()?;
            wr.write_image_data(&rgba)?;
        }
        let msg = serde_json::json!({
            "t": "cur",
            "d": base64::engine::general_purpose::STANDARD.encode(&png_bytes),
            "x": img.xhot, "y": img.yhot, "w": w, "h": h,
        })
        .to_string();
        self.cursor_msg = Some(msg.clone());
        self.send(Out::Text(msg));
        Ok(())
    }

    // ---------- clipboard ----------

    fn set_clipboard(&mut self, text: String) -> Result<()> {
        if text.len() > 4 * 1024 * 1024 {
            return Ok(());
        }
        self.clip_last_remote = text.clone();
        self.clip_ours = Some(text);
        for sel in [self.atoms.CLIPBOARD, self.atoms.PRIMARY] {
            self.conn.set_selection_owner(self.helper, sel, CURRENT_TIME)?;
        }
        Ok(())
    }

    fn selection_request(&mut self, e: SelectionRequestEvent) -> Result<()> {
        let prop = if e.property == NONE { e.target } else { e.property };
        let a = &self.atoms;
        let mut ok = false;
        if let Some(text) = &self.clip_ours {
            if e.target == a.TARGETS {
                self.conn.change_property32(
                    PropMode::REPLACE,
                    e.requestor,
                    prop,
                    a.ATOM,
                    &[a.TARGETS, a.UTF8_STRING, a.STRING, a.TEXT],
                )?;
                ok = true;
            } else if e.target == a.UTF8_STRING || e.target == a.TEXT || e.target == a.STRING {
                let ty = if e.target == a.STRING { a.STRING } else { a.UTF8_STRING };
                self.conn.change_property8(PropMode::REPLACE, e.requestor, prop, ty, text.as_bytes())?;
                ok = true;
            }
        }
        let ev = SelectionNotifyEvent {
            response_type: xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: e.time,
            requestor: e.requestor,
            selection: e.selection,
            target: e.target,
            property: if ok { prop } else { NONE },
        };
        self.conn.send_event(false, e.requestor, EventMask::NO_EVENT, ev)?;
        Ok(())
    }

    fn read_remote_clipboard(&mut self) -> Result<()> {
        let r = self
            .conn
            .get_property(true, self.helper, self.atoms.STRYMEK_SEL, AtomEnum::ANY, 0, 1024 * 1024)?
            .reply()?;
        if r.type_ == self.atoms.INCR {
            return Ok(()); // very large selections are not bridged
        }
        let text = String::from_utf8_lossy(&r.value).to_string();
        if !text.is_empty() && text != self.clip_last_remote {
            self.clip_last_remote = text.clone();
            self.send_json(serde_json::json!({"t":"clip","v":text}));
        }
        Ok(())
    }

    // ---------- capture ----------

    fn capture(&self, r: Rect) -> Result<Vec<u8>> {
        let (ox, oy) = self.origin();
        let img = self
            .conn
            .get_image(ImageFormat::Z_PIXMAP, self.root, (ox + r.x) as i16, (oy + r.y) as i16, r.w as u16, r.h as u16, !0)?
            .reply()?;
        Ok(img.data)
    }

    fn inflight(&self) -> u32 {
        self.client.as_ref().map(|c| c.sent_seq - c.acked_seq).unwrap_or(0)
    }

    fn collect_damage(&mut self) -> Result<()> {
        if !self.damaged {
            return Ok(());
        }
        self.damaged = false;
        self.conn.damage_subtract(self.damage, NONE, self.region)?;
        let rects = self.conn.xfixes_fetch_region(self.region)?.reply()?.rectangles;
        for r in rects {
            self.pending.push(Rect::new(r.x as i32, r.y as i32, r.width as i32, r.height as i32));
        }
        Ok(())
    }

    fn maybe_send_frame(&mut self) -> Result<()> {
        let ready = matches!(&self.client, Some(c) if c.hello);
        if !ready {
            // Keep the damage region from growing while nobody watches.
            if self.damaged {
                self.damaged = false;
                self.conn.damage_subtract(self.damage, NONE, NONE)?;
            }
            if let Some(d) = &mut self.desk {
                self.pending.clear();
                d.pending_since = None;
            }
            return Ok(());
        }
        if self.inflight() >= MAX_INFLIGHT || self.last_send.elapsed() < self.frame_interval {
            return Ok(());
        }
        if let Some(d) = &self.desk {
            // Give the compositor a moment to put the new pixels on screen.
            if let Some(since) = d.pending_since {
                if self.last_damage.elapsed() < Duration::from_millis(15) && since.elapsed() < Duration::from_millis(60) {
                    return Ok(());
                }
            }
            self.desk_refresh_geometry()?;
            if self.pending.is_empty() && d_poll_due(self.desk.as_ref().unwrap(), self.poll_interval()) {
                let tiles = self.poll_changes()?;
                if !tiles.is_empty() {
                    return self.send_tiles(tiles);
                }
            }
        } else {
            self.collect_damage()?;
        }
        let mut tiles = Vec::new();
        if !self.pending.is_empty() {
            let rects: Vec<Rect> = merge(std::mem::take(&mut self.pending), self.sw, self.sh)
                .into_iter()
                .map(|r| self.clip_visible(r))
                .filter(|r| !r.is_empty())
                .collect();
            if let Some(d) = &mut self.desk {
                d.pending_since = None;
            }
            for r in rects {
                let lossless = r.area() <= LOSSLESS_MAX_AREA;
                // On the real desktop, always resend once things settle, in case
                // the compositor had not finished drawing when we captured.
                if !lossless || self.desk.is_some() {
                    self.lossy.push(r);
                }
                tiles.push(Tile { x: r.x, y: r.y, w: r.w, h: r.h, bgrx: self.capture(r)?, lossless });
            }
            if self.lossy.len() > 32 {
                let u = self.lossy.iter().skip(1).fold(self.lossy[0], |a, b| a.union(b));
                self.lossy = vec![u];
            }
        } else if !self.lossy.is_empty() && self.last_damage.elapsed() >= REFINE_AFTER && self.inflight() == 0 {
            // Idle: replace blurry lossy regions with lossless pixels, a budget at a time.
            let mut budget = REFINE_BUDGET;
            let mut rest = Vec::new();
            let lossy: Vec<Rect> = merge(std::mem::take(&mut self.lossy), self.sw, self.sh)
                .into_iter()
                .map(|r| self.clip_visible(r))
                .filter(|r| !r.is_empty())
                .collect();
            for r in lossy {
                for piece in r.split(512) {
                    if budget > 0 {
                        budget -= piece.area();
                        tiles.push(Tile {
                            x: piece.x,
                            y: piece.y,
                            w: piece.w,
                            h: piece.h,
                            bgrx: self.capture(piece)?,
                            lossless: true,
                        });
                    } else {
                        rest.push(piece);
                    }
                }
            }
            self.lossy = rest;
        }
        if tiles.is_empty() {
            return Ok(());
        }
        self.send_tiles(tiles)
    }

    fn send_tiles(&mut self, tiles: Vec<Tile>) -> Result<()> {
        // A full refresh seeds the polling snapshot.
        let visible = self.clip_visible(Rect::new(0, 0, self.sw, self.sh));
        if let Some(d) = &mut self.desk {
            if d.snap.is_none() && tiles.len() == 1 && Rect::new(tiles[0].x, tiles[0].y, tiles[0].w, tiles[0].h) == visible {
                d.snap = Some((visible, tiles[0].bgrx.clone()));
            }
        }
        // Keep the polling snapshot in step with what the browser now shows.
        if let Some(Desk { snap: Some((v, buf)), .. }) = &mut self.desk {
            for t in &tiles {
                let r = Rect::new(t.x, t.y, t.w, t.h);
                if r.x < v.x || r.y < v.y || r.x + r.w > v.x + v.w || r.y + r.h > v.y + v.h {
                    continue;
                }
                let row = (t.w * 4) as usize;
                for yy in 0..t.h as usize {
                    let src = yy * row;
                    let dst = (((r.y - v.y) as usize + yy) * v.w as usize + (r.x - v.x) as usize) * 4;
                    if src + row <= t.bgrx.len() && dst + row <= buf.len() {
                        buf[dst..dst + row].copy_from_slice(&t.bgrx[src..src + row]);
                    }
                }
            }
        }
        let c = self.client.as_mut().unwrap();
        c.sent_seq += 1;
        let job = Job { seq: c.sent_seq, tiles, tx: c.tx.clone(), quality: self.quality };
        self.last_send = Instant::now();
        self.enc_tx.send(job).map_err(|_| anyhow!("encoder stopped"))?;
        Ok(())
    }

    /// Polling interval for desktop streams: quick while you are using it.
    fn poll_interval(&self) -> Duration {
        match &self.desk {
            Some(d) if d.last_input.elapsed() < Duration::from_secs(3) => Duration::from_millis(120),
            Some(d) if d.full => Duration::from_millis(400),
            _ => Duration::from_millis(700),
        }
    }

    /// Capture the visible area, compare it with the last snapshot in 64×64
    /// blocks, and return the changed parts as tiles.
    fn poll_changes(&mut self) -> Result<Vec<Tile>> {
        let v = self.clip_visible(Rect::new(0, 0, self.sw, self.sh));
        if v.is_empty() {
            return Ok(vec![]);
        }
        let data = self.capture(v)?;
        let d = self.desk.as_mut().unwrap();
        d.last_poll = Instant::now();
        let prev = d.snap.take();
        let (vw, vh) = (v.w as usize, v.h as usize);
        let mut changed = Vec::new();
        match &prev {
            Some((pv, pbuf)) if *pv == v && pbuf.len() == data.len() => {
                const B: usize = 64;
                for by in (0..vh).step_by(B) {
                    for bx in (0..vw).step_by(B) {
                        let bw = B.min(vw - bx);
                        let bh = B.min(vh - by);
                        let differs = (by..by + bh).any(|y| {
                            let o = (y * vw + bx) * 4;
                            data[o..o + bw * 4] != pbuf[o..o + bw * 4]
                        });
                        if differs {
                            changed.push(Rect::new(v.x + bx as i32, v.y + by as i32, bw as i32, bh as i32));
                        }
                    }
                }
            }
            // First poll (or the area changed): nothing to compare yet.
            _ => {}
        }
        let mut tiles = Vec::new();
        for r in merge(changed, self.sw, self.sh) {
            let mut bgrx = Vec::with_capacity((r.w * r.h * 4) as usize);
            for y in r.y..r.y + r.h {
                let o = (((y - v.y) as usize) * vw + (r.x - v.x) as usize) * 4;
                bgrx.extend_from_slice(&data[o..o + r.w as usize * 4]);
            }
            let lossless = r.area() <= LOSSLESS_MAX_AREA;
            if !lossless {
                self.lossy.push(r);
            }
            tiles.push(Tile { x: r.x, y: r.y, w: r.w, h: r.h, bgrx, lossless });
        }
        self.desk.as_mut().unwrap().snap = Some((v, data));
        Ok(tiles)
    }

    fn thumbnail(&self) -> Result<Vec<u8>> {
        let full = match &self.desk {
            // The window's own contents: correct even when other windows cover it.
            Some(d) => self
                .conn
                .get_image(ImageFormat::Z_PIXMAP, d.win, 0, 0, self.sw as u16, self.sh as u16, !0)?
                .reply()?
                .data,
            None => self.capture(Rect::new(0, 0, self.sw, self.sh))?,
        };
        let tw = 480.min(self.sw as usize);
        let th = ((self.sh as usize * tw) / self.sw as usize).max(1);
        let mut rgb = vec![0u8; tw * th * 3];
        for y in 0..th {
            let sy = y * self.sh as usize / th;
            for x in 0..tw {
                let sx = x * self.sw as usize / tw;
                let s = (sy * self.sw as usize + sx) * 4;
                let d = (y * tw + x) * 3;
                rgb[d] = full[s + 2];
                rgb[d + 1] = full[s + 1];
                rgb[d + 2] = full[s];
            }
        }
        encoder::encode_webp(&rgb, tw as u32, th as u32, false, 70.0).ok_or_else(|| anyhow!("encode failed"))
    }

    // ---------- resize ----------

    fn apply_resize(&mut self, w: i32, h: i32) -> Result<()> {
        if self.desk.is_some() {
            return self.desk_resize(w, h);
        }
        if (w, h) == (self.sw, self.sh) {
            self.send_config();
            self.full_refresh();
            return Ok(());
        }
        let res = self.conn.randr_get_screen_resources_current(self.root)?.reply()?;
        let output = *res.outputs.first().ok_or_else(|| anyhow!("no RandR output"))?;
        let crtc = *res.crtcs.first().ok_or_else(|| anyhow!("no RandR crtc"))?;
        let cts = res.config_timestamp;

        // Reuse an existing mode of this size, or create one.
        let existing = res.modes.iter().find(|m| m.width as i32 == w && m.height as i32 == h).map(|m| m.id);
        let mode = match existing {
            Some(m) => m,
            None => {
                let name = format!("strymek-{w}x{h}");
                let (wu, hu) = (w as u16, h as u16);
                let info = randr::ModeInfo {
                    id: 0,
                    width: wu,
                    height: hu,
                    dot_clock: (w as u32 + 3) * (h as u32 + 3) * 60,
                    hsync_start: wu + 1,
                    hsync_end: wu + 2,
                    htotal: wu + 3,
                    hskew: 0,
                    vsync_start: hu + 1,
                    vsync_end: hu + 2,
                    vtotal: hu + 3,
                    name_len: name.len() as u16,
                    mode_flags: randr::ModeFlag::from(0u32),
                };
                let m = self.conn.randr_create_mode(self.root, info, name.as_bytes())?.reply()?.mode;
                self.created_modes.push(m);
                m
            }
        };
        let _ = self.conn.randr_add_output_mode(output, mode)?.check();

        let dpi = 96.0 * self.scale as f64;
        let mm = |px: i32| ((px as f64) * 25.4 / dpi).round().max(1.0) as u32;
        let (cw, ch) = (self.sw, self.sh);
        let (bw, bh) = (w.max(cw), h.max(ch));
        // Grow the screen first so the CRTC always fits, then set the mode, then shrink.
        if (bw, bh) != (cw, ch) {
            self.conn.randr_set_screen_size(self.root, bw as u16, bh as u16, mm(bw), mm(bh))?.check()?;
        }
        let r = self
            .conn
            .randr_set_crtc_config(crtc, CURRENT_TIME, cts, 0, 0, mode, randr::Rotation::ROTATE0, &[output])?
            .reply()?;
        if r.status != randr::SetConfig::SUCCESS {
            return Err(anyhow!("RandR refused the mode ({:?})", r.status));
        }
        if (bw, bh) != (w, h) {
            self.conn.randr_set_screen_size(self.root, w as u16, h as u16, mm(w), mm(h))?.check()?;
        }
        // Drop modes we created earlier that are no longer in use.
        let old: Vec<randr::Mode> = self.created_modes.iter().copied().filter(|&m| m != mode).collect();
        for m in old {
            let _ = self.conn.randr_delete_output_mode(output, m)?.check();
            let _ = self.conn.randr_destroy_mode(m)?.check();
        }
        self.created_modes.retain(|&m| m == mode);

        self.sw = w;
        self.sh = h;
        self.set_workarea()?;
        self.relayout()?;
        self.conn.flush()?;
        self.send_config();
        self.full_refresh();
        self.update_shared();
        tracing::debug!("display resized to {w}x{h}");
        Ok(())
    }
}


impl Worker {
    // ---------- real desktop (Xorg) ----------

    fn init_desktop(&mut self, win: Window, root_w: i32, root_h: i32) -> Result<()> {
        let full = win == 0;
        let win = if full { self.root } else { win };
        let g = self.conn.get_geometry(win)?.reply().map_err(|_| anyhow!("that window no longer exists"))?;
        let t = self.conn.translate_coordinates(win, self.root, 0, 0)?.reply()?;
        if !full {
            self.conn.change_window_attributes(
                win,
                &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE),
            )?;
        }
        let states: Vec<u32> = self
            .conn
            .get_property(false, win, self.atoms._NET_WM_STATE, AtomEnum::ATOM, 0, 32)?
            .reply()?
            .value32()
            .map(|v| v.collect())
            .unwrap_or_default();
        let was_max = states.contains(&self.atoms._NET_WM_STATE_MAXIMIZED_VERT)
            && states.contains(&self.atoms._NET_WM_STATE_MAXIMIZED_HORZ);
        // Desktop scale from Xft.dpi (GNOME sets 192 at 200%).
        let res = self
            .conn
            .get_property(false, self.root, self.atoms.RESOURCE_MANAGER, AtomEnum::STRING, 0, 16384)?
            .reply()?;
        let dpi = String::from_utf8_lossy(&res.value)
            .lines()
            .find_map(|l| l.strip_prefix("Xft.dpi:").map(|v| v.trim().parse::<f32>().unwrap_or(96.0)))
            .unwrap_or(96.0);
        let scale = ((dpi / 96.0).round() as u32).clamp(1, 4);
        self.sw = g.width as i32;
        self.sh = g.height as i32;
        self.scale = scale;
        self.desk = Some(Desk {
            win,
            ox: t.dst_x as i32,
            oy: t.dst_y as i32,
            root_w,
            root_h,
            orig: Rect::new(t.dst_x as i32, t.dst_y as i32, g.width as i32, g.height as i32),
            was_max,
            resized: false,
            active: false,
            last_activate: None,
            scale,
            pending_since: None,
            full,
            snap: None,
            last_poll: Instant::now(),
            last_input: Instant::now(),
        });
        self.desk.as_mut().unwrap().active = full || self.active_window().unwrap_or(0) == win;
        self.title = if full { "Desktop".into() } else { self.read_title(win).unwrap_or_default() };
        self.update_shared();
        Ok(())
    }

    fn active_window(&self) -> Result<Window> {
        let r = self
            .conn
            .get_property(false, self.root, self.atoms._NET_ACTIVE_WINDOW, AtomEnum::WINDOW, 0, 1)?
            .reply()?;
        Ok(r.value32().and_then(|mut v| v.next()).unwrap_or(0))
    }

    fn client_message(&self, win: Window, ty: Atom, data: [u32; 5]) -> Result<()> {
        let ev = ClientMessageEvent::new(32, win, ty, data);
        self.conn.send_event(
            false,
            self.root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            ev,
        )?;
        Ok(())
    }

    /// Bring the streamed window to the front (and un-minimise it) before input.
    fn desk_ensure_active(&mut self) -> Result<()> {
        let Some(d) = &mut self.desk else { return Ok(()) };
        d.last_input = Instant::now();
        if d.active || d.full {
            return Ok(());
        }
        if d.last_activate.map(|t| t.elapsed() < Duration::from_millis(250)).unwrap_or(false) {
            return Ok(());
        }
        let win = d.win;
        self.client_message(win, self.atoms._NET_ACTIVE_WINDOW, [2, CURRENT_TIME, 0, 0, 0])?;
        self.conn.flush()?;
        if let Some(d) = &mut self.desk {
            d.last_activate = Some(Instant::now());
        }
        // Wait briefly for the window manager so the first click lands on the right window.
        let deadline = Instant::now() + Duration::from_millis(150);
        while Instant::now() < deadline {
            if self.active_window().unwrap_or(0) == win {
                if let Some(d) = &mut self.desk {
                    d.active = true;
                }
                self.full_refresh();
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }

    fn desk_refresh_geometry(&mut self) -> Result<()> {
        let Some(win) = self.desk.as_ref().map(|d| d.win) else { return Ok(()) };
        let Ok(g) = self.conn.get_geometry(win)?.reply() else {
            self.quit = true;
            return Ok(());
        };
        let t = self.conn.translate_coordinates(win, self.root, 0, 0)?.reply()?;
        let (nx, ny, nw, nh) = (t.dst_x as i32, t.dst_y as i32, g.width as i32, g.height as i32);
        let d = self.desk.as_mut().unwrap();
        if d.full {
            d.root_w = nw;
            d.root_h = nh;
        }
        let moved = (nx, ny) != (d.ox, d.oy);
        let resized = (nw, nh) != (self.sw, self.sh);
        d.ox = nx;
        d.oy = ny;
        if resized {
            self.sw = nw;
            self.sh = nh;
            self.send_config();
            self.update_shared();
        }
        if moved || resized {
            self.full_refresh();
        }
        Ok(())
    }

    /// The part of rectangle `r` (window coordinates) that is on the screen.
    fn clip_visible(&self, r: Rect) -> Rect {
        match &self.desk {
            None => r.clip(self.sw, self.sh),
            Some(d) => {
                let x0 = r.x.max(-d.ox).max(0);
                let y0 = r.y.max(-d.oy).max(0);
                let x1 = (r.x + r.w).min(d.root_w - d.ox).min(self.sw);
                let y1 = (r.y + r.h).min(d.root_h - d.oy).min(self.sh);
                Rect::new(x0, y0, x1 - x0, y1 - y0)
            }
        }
    }

    fn requested_size(&self, w: i32, h: i32, dpr: Option<f32>) -> (i32, i32) {
        match &self.desk {
            None => norm_size(w, h),
            Some(d) if d.full => (self.sw, self.sh),
            Some(d) => {
                // Browser pixels → desktop pixels at the desktop's own scale.
                let dpr = dpr.unwrap_or(1.0).max(0.5);
                let w = (w as f32 / dpr * d.scale as f32).round() as i32;
                let h = (h as f32 / dpr * d.scale as f32).round() as i32;
                (w.clamp(320, d.root_w), h.clamp(240, d.root_h))
            }
        }
    }

    /// Window-manager decoration sizes (left, right, top, bottom).
    fn frame_extents(&self, win: Window) -> (i32, i32, i32, i32) {
        self.conn
            .get_property(false, win, self.atoms._NET_FRAME_EXTENTS, AtomEnum::CARDINAL, 0, 4)
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| r.value32().map(|v| v.collect::<Vec<u32>>()))
            .filter(|v| v.len() == 4)
            .map(|v| (v[0] as i32, v[1] as i32, v[2] as i32, v[3] as i32))
            .unwrap_or((0, 0, 0, 0))
    }

    /// Resize (and optionally move) a desktop window the way apps resize
    /// themselves: the window manager receives it as a ConfigureRequest.
    fn move_resize(&self, win: Window, r: Rect, mv: bool) -> Result<()> {
        let mut aux = ConfigureWindowAux::new().width(r.w.max(1) as u32).height(r.h.max(1) as u32);
        if mv {
            aux = aux.x(r.x).y(r.y);
        }
        self.conn.configure_window(win, &aux)?;
        Ok(())
    }

    fn set_maximized(&self, win: Window, on: bool) -> Result<()> {
        let a = &self.atoms;
        self.client_message(
            win,
            a._NET_WM_STATE,
            [if on { 1 } else { 0 }, a._NET_WM_STATE_MAXIMIZED_VERT, a._NET_WM_STATE_MAXIMIZED_HORZ, 2, 0],
        )
    }

    /// Resize the real window to fit the browser window, keeping it on screen.
    fn desk_resize(&mut self, w: i32, h: i32) -> Result<()> {
        let d = self.desk.as_ref().unwrap();
        let (win, rw, rh) = (d.win, d.root_w, d.root_h);
        if d.full || (w, h) == (self.sw, self.sh) {
            self.send_config();
            self.full_refresh();
            return Ok(());
        }
        let (ox, oy, was_max, resized) = (d.ox, d.oy, d.was_max, d.resized);
        // Window managers place windows by their frame (title bar included).
        let (l, r, t, b) = self.frame_extents(win);
        let (fx, fy) = (ox - l, oy - t);
        let nx = fx.clamp(0, (rw - (w + l + r)).max(0));
        let ny = fy.clamp(0, (rh - (h + t + b)).max(0));
        if was_max && !resized {
            self.set_maximized(win, false)?;
        }
        // Only move it when it would otherwise stick out of the screen.
        self.move_resize(win, Rect::new(nx, ny, w, h), (nx, ny) != (fx, fy))?;
        self.conn.flush()?;
        if let Some(d) = &mut self.desk {
            d.resized = true;
        }
        // Wait for the window manager to apply it, then report the real size.
        let deadline = Instant::now() + Duration::from_millis(400);
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
            if let Ok(g) = self.conn.get_geometry(win)?.reply() {
                if (g.width as i32, g.height as i32) == (w, h) {
                    break;
                }
            }
        }
        self.desk_refresh_geometry()?;
        self.send_config();
        self.full_refresh();
        Ok(())
    }

    /// Put the window back where it was before streaming started.
    fn desk_restore(&mut self) -> Result<()> {
        let Some(d) = &self.desk else { return Ok(()) };
        if !d.resized {
            return Ok(());
        }
        let (win, orig, was_max, moved) = (d.win, d.orig, d.was_max, (d.ox, d.oy) != (d.orig.x, d.orig.y));
        tracing::info!(window = win, x = orig.x, y = orig.y, w = orig.w, h = orig.h, "restoring desktop window geometry");
        let (l, _, t, _) = self.frame_extents(win);
        self.move_resize(win, Rect::new(orig.x - l, orig.y - t, orig.w, orig.h), moved)?;
        if was_max {
            self.set_maximized(win, true)?;
        }
        Ok(())
    }
}

fn d_poll_due(d: &Desk, interval: Duration) -> bool {
    d.last_poll.elapsed() >= interval
}

fn current_user() -> Option<String> {
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if pw.is_null() || (*pw).pw_name.is_null() {
            return std::env::var("USER").ok();
        }
        Some(std::ffi::CStr::from_ptr((*pw).pw_name).to_string_lossy().to_string())
    }
}

fn norm_size(w: i32, h: i32) -> (i32, i32) {
    let w = w.clamp(320, 8192) & !1;
    let h = h.clamp(240, 8192) & !1;
    (w, h)
}

/// Waker for the worker's poll loop (an eventfd).
pub struct Waker(pub i32);

impl Waker {
    pub fn new() -> Result<Self> {
        let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if fd < 0 {
            return Err(anyhow!("eventfd failed"));
        }
        Ok(Self(fd))
    }
    pub fn wake(&self) {
        let one: u64 = 1;
        unsafe { libc::write(self.0, &one as *const u64 as *const _, 8) };
    }
}

impl Drop for Waker {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}
