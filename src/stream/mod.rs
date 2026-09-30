//! Streams: one running app on its own private X display.

pub mod encoder;
pub mod launch;
pub mod rect;
pub mod worker;

use crate::apps::{self, AppInfo};
use crate::desktop;
use crate::config::{ensure_private_dir, runtime_dir, Config};
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::process::Child;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::mpsc::UnboundedSender;
use worker::Waker;

/// A message to one browser client.
#[derive(Debug)]
pub enum Out {
    Text(String),
    Bin(Vec<u8>),
    Close,
}

/// Messages from the browser (JSON text frames).
#[derive(Debug, Deserialize)]
#[serde(tag = "t")]
pub enum ClientMsg {
    #[serde(rename = "hello")]
    Hello { w: i32, h: i32, cmd_ctrl: Option<bool>, dpr: Option<f32> },
    #[serde(rename = "rs")]
    Resize { w: i32, h: i32, dpr: Option<f32> },
    #[serde(rename = "ack")]
    Ack { s: u32 },
    #[serde(rename = "kf")]
    Refresh,
    #[serde(rename = "opt")]
    Options { cmd_ctrl: bool },
    #[serde(rename = "pm")]
    Move { x: i32, y: i32 },
    #[serde(rename = "pb")]
    Button { b: u8, d: bool, x: i32, y: i32 },
    #[serde(rename = "wh")]
    Wheel { v: i32, h: i32, x: i32, y: i32 },
    #[serde(rename = "key")]
    Key { code: String, key: String, d: bool },
    #[serde(rename = "txt")]
    Text { text: String },
    #[serde(rename = "rel")]
    ReleaseAll,
    #[serde(rename = "clip")]
    Clip { text: String },
}

pub enum Cmd {
    Attach { id: u64, tx: UnboundedSender<Out> },
    Detach { id: u64 },
    Client { id: u64, msg: ClientMsg },
    Thumb { reply: std::sync::mpsc::Sender<Option<Vec<u8>>> },
    Shutdown,
}

/// State the worker publishes for the dashboard and the lifecycle monitor.
#[derive(Default)]
pub struct Shared {
    pub windows: usize,
    pub title: String,
    pub width: u32,
    pub height: u32,
    pub attached: bool,
    pub ever_had_window: bool,
    pub last_window_seen: Option<Instant>,
    pub dead: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// An app Strymek started on its own private display.
    App,
    /// A window on your real desktop, streamed live.
    Desktop,
}

pub struct Stream {
    pub slug: String,
    pub app: AppInfo,
    pub kind: Kind,
    pub display: u32,
    pub scale: u32,
    pub real_profile: bool,
    pub started: SystemTime,
    started_at: Instant,
    x_child: Mutex<Option<Child>>,
    app_child: Mutex<Option<Child>>,
    cmd_tx: Mutex<std::sync::mpsc::Sender<Cmd>>,
    waker: Arc<Waker>,
    pub shared: Arc<Mutex<Shared>>,
    profile_slot: Option<(String, u32)>,
    xauth: Option<PathBuf>,
}

#[derive(Serialize)]
pub struct StreamInfo {
    pub slug: String,
    pub app_id: String,
    pub name: String,
    pub title: String,
    pub kind: Kind,
    pub real_profile: bool,
    pub started: u64,
    pub width: u32,
    pub height: u32,
    pub windows: usize,
    pub attached: bool,
    pub has_icon: bool,
    pub display: u32,
    pub scale: u32,
}

impl Stream {
    pub fn send(&self, cmd: Cmd) {
        let _ = self.cmd_tx.lock().unwrap().send(cmd);
        self.waker.wake();
    }

    pub fn info(&self) -> StreamInfo {
        let s = self.shared.lock().unwrap();
        StreamInfo {
            slug: self.slug.clone(),
            app_id: self.app.id.clone(),
            name: self.app.name.clone(),
            title: s.title.clone(),
            kind: self.kind,
            real_profile: self.real_profile,
            started: self.started.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
            width: s.width,
            height: s.height,
            windows: s.windows,
            attached: s.attached,
            has_icon: self.app.has_icon,
            display: self.display,
            scale: self.scale,
        }
    }

    /// Ask the worker for a small WebP screenshot.
    pub fn thumbnail(&self) -> Option<Vec<u8>> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.send(Cmd::Thumb { reply: tx });
        rx.recv_timeout(Duration::from_secs(3)).ok().flatten()
    }
}

struct WorkerHandles {
    shared: Arc<Mutex<Shared>>,
    waker: Arc<Waker>,
    cmd_tx: std::sync::mpsc::Sender<Cmd>,
}

fn spawn_worker(setup: worker::Setup, slug: &str) -> Result<WorkerHandles> {
    let shared = Arc::new(Mutex::new(Shared::default()));
    let waker = Arc::new(Waker::new()?);
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<Cmd>();
    let (enc_tx, enc_rx) = std::sync::mpsc::channel::<encoder::Job>();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();
    {
        let shared = shared.clone();
        let waker = waker.clone();
        std::thread::Builder::new().name(format!("x-{slug}")).spawn(move || {
            match worker::Worker::new(&setup, shared.clone(), enc_tx) {
                Ok(w) => {
                    let _ = ready_tx.send(Ok(()));
                    w.run(cmd_rx, waker.0);
                    drop(waker);
                }
                Err(e) => {
                    shared.lock().unwrap().dead = true;
                    let _ = ready_tx.send(Err(e));
                }
            }
        })?;
        std::thread::Builder::new().name(format!("enc-{slug}")).spawn(move || encoder::run(enc_rx))?;
    }
    match ready_rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(())) => Ok(WorkerHandles { shared, waker, cmd_tx }),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(anyhow!("timed out connecting to the X display")),
    }
}

fn slug_part(s: &str) -> String {
    let mut out: String = s
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    while out.ends_with(|c: char| c.is_ascii_digit() || c == '-') {
        out.pop();
    }
    let out = out.trim_start_matches('-').chars().take(24).collect::<String>();
    if out.is_empty() { "window".into() } else { out }
}

pub struct Manager {
    pub cfg: Config,
    streams: Mutex<BTreeMap<String, Arc<Stream>>>,
    apps: Mutex<(Instant, Vec<AppInfo>)>,
    next_instance: AtomicU32,
    next_client: AtomicU64,
    slots: Mutex<HashSet<(String, u32)>>,
    run_dir: PathBuf,
    /// Set when Strymek unlocked the desktop; it is locked again once no live
    /// desktop window has been watched for a while.
    unlocked_by_us: Mutex<Option<Instant>>,
}

impl Manager {
    pub fn new(cfg: Config) -> Result<Arc<Self>> {
        let run_dir = runtime_dir();
        ensure_private_dir(&run_dir)?;
        let apps = apps::discover(&cfg);
        Ok(Arc::new(Self {
            cfg,
            streams: Mutex::new(BTreeMap::new()),
            apps: Mutex::new((Instant::now(), apps)),
            next_instance: AtomicU32::new(1000),
            next_client: AtomicU64::new(1),
            slots: Mutex::new(HashSet::new()),
            run_dir,
            unlocked_by_us: Mutex::new(None),
        }))
    }

    pub fn apps(&self) -> Vec<AppInfo> {
        let mut a = self.apps.lock().unwrap();
        if a.0.elapsed() > Duration::from_secs(30) {
            *a = (Instant::now(), apps::discover(&self.cfg));
        }
        a.1.clone()
    }

    pub fn app(&self, id: &str) -> Option<AppInfo> {
        self.apps().into_iter().find(|a| a.id == id)
    }

    /// The installed app to use for a family (e.g. the Firefox .desktop entry).
    pub fn app_for_family(&self, fam: launch::Family) -> Option<AppInfo> {
        let mut c: Vec<AppInfo> = self.apps().into_iter().filter(|a| launch::app_family(a) == fam).collect();
        c.sort_by_key(|a| (a.id != "libreoffice-startcenter", a.argv.len(), a.id.clone()));
        c.into_iter().next()
    }

    /// The installed app a desktop window belongs to, by process family or window class.
    pub fn app_for_window(&self, fam: Option<launch::Family>, class: &str) -> Option<AppInfo> {
        fam.and_then(|f| self.app_for_family(f)).or_else(|| {
            let class = class.to_lowercase();
            if class.is_empty() {
                return None;
            }
            self.apps().into_iter().find(|a| a.short == class || a.name.to_lowercase() == class || a.id.to_lowercase().ends_with(&class))
        })
    }

    pub fn get(&self, slug: &str) -> Option<Arc<Stream>> {
        self.streams.lock().unwrap().get(slug).cloned()
    }

    pub fn list(&self) -> Vec<StreamInfo> {
        self.streams.lock().unwrap().values().map(|s| s.info()).collect()
    }

    pub fn client_id(&self) -> u64 {
        self.next_client.fetch_add(1, Ordering::Relaxed)
    }

    fn take_slot(&self, app: &AppInfo) -> Option<(String, u32)> {
        if !launch::needs_profile(app) {
            return None;
        }
        let mut slots = self.slots.lock().unwrap();
        let slot = (1..).find(|n| !slots.contains(&(app.short.clone(), *n))).unwrap();
        slots.insert((app.short.clone(), slot));
        Some((app.short.clone(), slot))
    }

    fn next_slug(&self, base: &str) -> String {
        format!("{}{}", base, self.next_instance.fetch_add(1, Ordering::Relaxed))
    }

    /// Start an app on a new private display. Blocking: call from spawn_blocking.
    pub fn launch(&self, app_id: &str, w: i32, h: i32, scale: u32) -> Result<String> {
        let app = self.app(app_id).ok_or_else(|| anyhow!("unknown app {app_id}"))?;
        self.launch_app(app, w, h, scale, false)
    }

    fn launch_app(&self, app: AppInfo, w: i32, h: i32, scale: u32, real_profile: bool) -> Result<String> {
        let scale = if self.cfg.scale > 0 { self.cfg.scale } else { scale.clamp(1, 3) };
        let (w, h) = ((w.clamp(320, 8192)) & !1, (h.clamp(240, 8192)) & !1);
        let slug = self.next_slug(&app.short);
        let display = launch::pick_display()?;
        let xauth = self.run_dir.join(format!("xauth-{display}"));
        let cookie = launch::write_xauth(&xauth, display)?;

        let mut x_child = launch::spawn_x_server(&self.cfg, display, w, h, scale, &xauth, &self.run_dir, &slug)?;
        let setup = worker::Setup {
            socket: launch::socket_path(display),
            cookie,
            scale,
            max_fps: self.cfg.max_fps,
            quality: self.cfg.lossy_quality,
            desktop_window: None,
        };
        let h = match spawn_worker(setup, &slug) {
            Ok(h) => h,
            Err(e) => {
                launch::kill_group(&x_child, libc::SIGTERM);
                let _ = x_child.wait();
                return Err(e.context("connecting to the private X display"));
            }
        };

        let profile_slot = if real_profile { None } else { self.take_slot(&app) };
        let profile = profile_slot.as_ref().map(|(_, n)| launch::profile_dir(&app, *n));
        let spec = launch::LaunchSpec {
            app: &app,
            display,
            xauth: &xauth,
            scale,
            profile,
            run_dir: &self.run_dir,
            slug: &slug,
            real_profile,
        };
        let app_child = match launch::spawn_app(&self.cfg, &spec) {
            Ok(c) => c,
            Err(e) => {
                let _ = h.cmd_tx.send(Cmd::Shutdown);
                h.waker.wake();
                launch::kill_group(&x_child, libc::SIGTERM);
                let _ = x_child.wait();
                if let Some(s) = profile_slot {
                    self.slots.lock().unwrap().remove(&s);
                }
                return Err(e);
            }
        };
        let disp_no = display;
        tracing::info!(%slug, x_display = disp_no, app = %app.id, real_profile, "stream started");
        let stream = Arc::new(Stream {
            slug: slug.clone(),
            app,
            kind: Kind::App,
            display,
            scale,
            real_profile,
            started: SystemTime::now(),
            started_at: Instant::now(),
            x_child: Mutex::new(Some(x_child)),
            app_child: Mutex::new(Some(app_child)),
            cmd_tx: Mutex::new(h.cmd_tx),
            waker: h.waker,
            shared: h.shared,
            profile_slot,
            xauth: Some(xauth),
        });
        self.streams.lock().unwrap().insert(slug.clone(), stream);
        Ok(slug)
    }

    /// Close an app on the desktop and reopen it here with your normal profile,
    /// so tabs, logins and workspaces come back. Blocking.
    pub fn move_here(&self, fam: launch::Family, w: i32, h: i32, scale: u32) -> Result<String> {
        if let Some(s) = self.list().into_iter().find(|s| s.real_profile && self.get(&s.slug).map(|x| launch::app_family(&x.app)) == Some(fam)) {
            return Ok(s.slug); // already moved
        }
        let app = self
            .app_for_family(fam)
            .ok_or_else(|| anyhow!("no installed app found for {}", desktop::family_label(fam)))?;
        if let Some(running) = desktop::running_apps().into_iter().find(|a| a.family == fam) {
            tracing::info!(app = running.label, pids = ?running.pids, "closing on the desktop to move into Strymek");
            desktop::close_app(&running, Duration::from_secs(20))?;
        }
        if fam == launch::Family::Firefox {
            desktop::firefox_resume_once();
        }
        std::thread::sleep(Duration::from_millis(500));
        self.launch_app(app, w, h, scale, true)
    }

    /// Stream one window of the real desktop (Xorg). Blocking.
    /// `win` 0 streams the whole screen.
    pub fn stream_desktop_window(&self, win: u32) -> Result<String> {
        if let Some(s) = self.streams.lock().unwrap().values().find(|s| s.kind == Kind::Desktop && s.display == win) {
            return Ok(s.slug.clone());
        }
        let sess = desktop::session();
        if !sess.live_windows_supported() {
            return Err(anyhow!(
                "live desktop streaming needs an Xorg desktop session (this one is '{}')",
                if sess.kind.is_empty() { "not logged in" } else { &sess.kind }
            ));
        }
        if win == 0 {
            let app = AppInfo {
                id: "desktop-full".into(),
                name: "Full desktop".into(),
                comment: String::new(),
                short: "desktop".into(),
                argv: vec![],
                icon_path: None,
                has_icon: false,
            };
            return self.start_desktop_stream(&sess, 0, app, "desktop");
        }
        let info = desktop::list_windows(&sess)?
            .into_iter()
            .find(|w| w.id == win)
            .ok_or_else(|| anyhow!("that window is no longer open"))?;
        let matched = self.app_for_window(info.family, &info.class);
        let app = match matched {
            Some(mut a) => {
                a.name = if info.title.is_empty() { a.name } else { format!("{} (desktop)", a.name) };
                a
            }
            None => AppInfo {
                id: "desktop-window".into(),
                name: if info.class.is_empty() { "Desktop window".into() } else { info.class.clone() },
                comment: String::new(),
                short: slug_part(&info.class),
                argv: vec![],
                icon_path: None,
                has_icon: false,
            },
        };
        let base = slug_part(if info.class.is_empty() { &app.short } else { &info.class });
        tracing::info!(window = win, title = %info.title, "starting live desktop window stream");
        self.start_desktop_stream(&sess, win, app, &base)
    }

    fn start_desktop_stream(&self, sess: &desktop::Session, win: u32, app: AppInfo, base: &str) -> Result<String> {
        let slug = self.next_slug(base);
        let setup = worker::Setup {
            socket: desktop::socket_for(sess.display.as_deref().unwrap_or(":0")),
            cookie: desktop::cookie_for(sess),
            scale: 1,
            max_fps: self.cfg.max_fps,
            quality: self.cfg.lossy_quality,
            desktop_window: Some(win),
        };
        let h = spawn_worker(setup, &slug).map_err(|e| e.context("connecting to your desktop's X display"))?;
        tracing::info!(%slug, window = win, "live desktop stream started");
        let stream = Arc::new(Stream {
            slug: slug.clone(),
            app,
            kind: Kind::Desktop,
            display: win,
            scale: 1,
            real_profile: false,
            started: SystemTime::now(),
            started_at: Instant::now(),
            x_child: Mutex::new(None),
            app_child: Mutex::new(None),
            cmd_tx: Mutex::new(h.cmd_tx),
            waker: h.waker,
            shared: h.shared,
            profile_slot: None,
            xauth: None,
        });
        self.streams.lock().unwrap().insert(slug.clone(), stream);
        Ok(slug)
    }

    pub fn note_unlocked_by_us(&self) {
        *self.unlocked_by_us.lock().unwrap() = Some(Instant::now());
    }

    /// Lock the desktop again when we unlocked it and nobody has been
    /// watching a live desktop window for `relock_after_secs`. Blocking.
    pub fn relock_check(&self) {
        if self.cfg.relock_after_secs == 0 {
            return;
        }
        let mut guard = self.unlocked_by_us.lock().unwrap();
        let Some(since) = *guard else { return };
        let watching = self
            .streams
            .lock()
            .unwrap()
            .values()
            .any(|s| s.kind == Kind::Desktop && s.shared.lock().unwrap().attached);
        if watching {
            *guard = Some(Instant::now());
            return;
        }
        if since.elapsed() >= Duration::from_secs(self.cfg.relock_after_secs) {
            *guard = None;
            drop(guard);
            let sess = desktop::session();
            if sess.locked == Some(false) {
                let _ = desktop::lock(&sess);
                tracing::info!("desktop locked again: no live desktop window was being watched");
            }
        }
    }

    /// Stop a stream. App streams: close the app, then its display. Desktop
    /// streams: detach and put the window back. Blocking.
    pub fn stop(&self, slug: &str) -> bool {
        let Some(s) = self.streams.lock().unwrap().remove(slug) else { return false };
        s.send(Cmd::Shutdown);
        if let Some(app) = s.app_child.lock().unwrap().as_mut() {
            launch::kill_group(app, libc::SIGTERM);
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                if matches!(app.try_wait(), Ok(Some(_))) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            launch::kill_group(app, libc::SIGKILL);
            let _ = app.wait();
        }
        if let Some(x) = s.x_child.lock().unwrap().as_mut() {
            launch::kill_group(x, libc::SIGTERM);
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                if matches!(x.try_wait(), Ok(Some(_))) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            let _ = x.kill();
            let _ = x.wait();
        }
        if let Some(slot) = &s.profile_slot {
            self.slots.lock().unwrap().remove(slot);
        }
        if let Some(x) = &s.xauth {
            let _ = std::fs::remove_file(x);
        }
        tracing::info!(slug, "stream stopped");
        true
    }

    /// Streams whose app or window has gone away. Called every couple of seconds.
    pub fn finished(&self) -> Vec<String> {
        let now = Instant::now();
        let mut done = Vec::new();
        for (slug, s) in self.streams.lock().unwrap().iter() {
            let sh_dead = s.shared.lock().unwrap().dead;
            if s.kind == Kind::Desktop {
                if sh_dead {
                    done.push(slug.clone());
                }
                continue;
            }
            let x_dead = s.x_child.lock().unwrap().as_mut().map(|c| matches!(c.try_wait(), Ok(Some(_)))).unwrap_or(true);
            let app_exited =
                s.app_child.lock().unwrap().as_mut().map(|c| matches!(c.try_wait(), Ok(Some(_)))).unwrap_or(true);
            let sh = s.shared.lock().unwrap();
            let idle_for = sh.last_window_seen.map(|t| now.duration_since(t)).unwrap_or(now.duration_since(s.started_at));
            let finished = x_dead
                || sh_dead
                || (app_exited && sh.windows == 0 && idle_for > Duration::from_secs(3))
                || (sh.ever_had_window && sh.windows == 0 && idle_for > Duration::from_secs(15))
                || (!sh.ever_had_window && now.duration_since(s.started_at) > Duration::from_secs(120));
            if finished {
                done.push(slug.clone());
            }
        }
        done
    }

    pub fn stop_all(&self) {
        let slugs: Vec<String> = self.streams.lock().unwrap().keys().cloned().collect();
        for s in slugs {
            self.stop(&s);
        }
    }
}
