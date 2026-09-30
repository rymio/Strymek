//! Your real desktop session: find it, lock/unlock it, and find apps already
//! running on it so they can be moved into Strymek or streamed live.

use crate::stream::launch::{family_of_program, Family};
use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::rust_connection::{DefaultStream, RustConnection};

#[derive(Debug, Clone, Serialize, Default)]
pub struct Session {
    /// "x11", "wayland", "tty" or "" when you are not logged in graphically.
    pub kind: String,
    pub id: Option<String>,
    pub locked: Option<bool>,
    /// X display of an Xorg session, e.g. ":0".
    pub display: Option<String>,
    #[serde(skip)]
    pub xauthority: Option<PathBuf>,
}

impl Session {
    pub fn live_windows_supported(&self) -> bool {
        self.kind == "x11" && self.display.is_some()
    }
}

fn uid() -> u32 {
    unsafe { libc::getuid() }
}

fn loginctl(args: &[&str]) -> Option<String> {
    let out = Command::new("loginctl").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn proc_owned_by_me(pid: u32) -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(format!("/proc/{pid}")).map(|m| m.uid() == uid()).unwrap_or(false)
}

fn my_pids() -> Vec<u32> {
    let Ok(rd) = std::fs::read_dir("/proc") else { return vec![] };
    rd.flatten()
        .filter_map(|e| e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()))
        .filter(|&p| proc_owned_by_me(p))
        .collect()
}

fn read_environ(pid: u32) -> HashMap<String, String> {
    std::fs::read(format!("/proc/{pid}/environ"))
        .map(|b| {
            b.split(|&c| c == 0)
                .filter_map(|kv| {
                    let s = String::from_utf8_lossy(kv);
                    s.split_once('=').map(|(k, v)| (k.to_string(), v.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn cmdline(pid: u32) -> Vec<String> {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|b| b.split(|&c| c == 0).filter(|a| !a.is_empty()).map(|a| String::from_utf8_lossy(a).to_string()).collect())
        .unwrap_or_default()
}

fn comm(pid: u32) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/comm")).map(|s| s.trim().to_string()).unwrap_or_default()
}

fn exe_name(pid: u32) -> String {
    std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .and_then(|p| p.file_name().map(|f| f.to_string_lossy().to_string()))
        .unwrap_or_default()
}

fn ppid(pid: u32) -> u32 {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| {
            let after = s.rsplit_once(')')?.1;
            after.split_whitespace().nth(1)?.parse().ok()
        })
        .unwrap_or(0)
}

/// Find the graphical session of this user.
pub fn session() -> Session {
    // Testing hook: treat an existing X display as the desktop session.
    if let Ok(d) = std::env::var("STRYMEK_TEST_DESKTOP") {
        return Session { kind: "x11".into(), id: None, locked: None, display: Some(d), xauthority: None };
    }
    let mut s = Session::default();
    s.id = loginctl(&["show-user", &uid().to_string(), "-p", "Display", "--value"]);
    if let Some(id) = &s.id {
        s.kind = loginctl(&["show-session", id, "-p", "Type", "--value"]).unwrap_or_default();
        s.locked = loginctl(&["show-session", id, "-p", "LockedHint", "--value"]).map(|v| v == "yes");
    }
    // On Xorg, the X server's own command line names the display and its cookie file.
    for pid in my_pids() {
        let c = comm(pid);
        if c == "Xorg" || c == "X" {
            let args = cmdline(pid);
            s.display = args.iter().find(|a| a.starts_with(':')).cloned();
            s.xauthority = args.iter().position(|a| a == "-auth").and_then(|i| args.get(i + 1)).map(PathBuf::from);
            if s.kind.is_empty() {
                s.kind = "x11".into();
            }
            break;
        }
    }
    // Otherwise take DISPLAY/XAUTHORITY from gnome-shell (covers other display managers).
    if s.display.is_none() && s.kind == "x11" {
        for pid in my_pids() {
            if comm(pid) == "gnome-shell" {
                let env = read_environ(pid);
                s.display = env.get("DISPLAY").cloned();
                s.xauthority = env.get("XAUTHORITY").map(PathBuf::from);
                break;
            }
        }
    }
    s
}

pub fn lock(sess: &Session) -> Result<()> {
    let id = sess.id.as_deref().ok_or_else(|| anyhow!("no desktop session"))?;
    let st = Command::new("loginctl").args(["lock-session", id]).status()?;
    if !st.success() {
        return Err(anyhow!("loginctl lock-session failed"));
    }
    tracing::info!(session = id, "desktop locked");
    Ok(())
}

pub fn unlock(sess: &Session) -> Result<()> {
    let id = sess.id.as_deref().ok_or_else(|| anyhow!("no desktop session"))?;
    let st = Command::new("loginctl").args(["unlock-session", id]).status()?;
    if !st.success() {
        return Err(anyhow!("loginctl unlock-session failed"));
    }
    tracing::info!(session = id, "desktop unlocked remotely");
    Ok(())
}

// ---------- apps running on the desktop ----------

#[derive(Debug, Clone, Serialize)]
pub struct RunningApp {
    pub family: Family,
    pub label: &'static str,
    pub pids: Vec<u32>,
    #[serde(skip)]
    pub roots: Vec<u32>,
}

pub fn family_label(f: Family) -> &'static str {
    match f {
        Family::Firefox => "Firefox",
        Family::Chromium => "Chrome / Chromium",
        Family::VsCode => "VS Code",
        Family::LibreOffice => "LibreOffice",
        Family::Other => "Other",
    }
}

fn process_family(pid: u32) -> Option<Family> {
    let exe = exe_name(pid);
    let fam = family_of_program(&exe);
    if fam != Family::Other {
        return Some(fam);
    }
    // Wrapper scripts and snaps: fall back to argv[0].
    let args = cmdline(pid);
    let f = args.first().map(|a| family_of_program(a)).unwrap_or(Family::Other);
    (f != Family::Other && !exe.starts_with("bash") && !exe.starts_with("sh")).then_some(f)
}

/// Apps of the families Strymek can move, running outside Strymek.
pub fn running_apps() -> Vec<RunningApp> {
    let mut by_family: HashMap<Family, Vec<u32>> = HashMap::new();
    for pid in my_pids() {
        let Some(f) = process_family(pid) else { continue };
        if read_environ(pid).contains_key("STRYMEK_SLUG") {
            continue; // started by Strymek
        }
        by_family.entry(f).or_default().push(pid);
    }
    let mut out: Vec<RunningApp> = by_family
        .into_iter()
        .map(|(family, pids)| {
            let set: HashSet<u32> = pids.iter().copied().collect();
            let roots = pids.iter().copied().filter(|p| !set.contains(&ppid(*p))).collect();
            RunningApp { family, label: family_label(family), pids, roots }
        })
        .collect();
    out.sort_by_key(|a| a.label);
    out
}

/// Close a desktop app gracefully (like choosing Quit) and wait for it to exit.
pub fn close_app(app: &RunningApp, timeout: Duration) -> Result<()> {
    for &p in &app.roots {
        unsafe { libc::kill(p as i32, libc::SIGTERM) };
    }
    let start = Instant::now();
    while start.elapsed() < timeout {
        let alive = app.pids.iter().any(|&p| Path::new(&format!("/proc/{p}")).exists() && proc_owned_by_me(p));
        if !alive {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    Err(anyhow!(
        "{} is still running after {}s; it may be asking about unsaved work on the desktop",
        app.label,
        timeout.as_secs()
    ))
}

/// Ask Firefox to restore the windows and tabs it just closed, once.
pub fn firefox_resume_once() {
    let Some(home) = dirs::home_dir() else { return };
    let roots = [home.join(".mozilla/firefox"), home.join("snap/firefox/common/.mozilla/firefox")];
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for root in roots {
        let Ok(rd) = std::fs::read_dir(&root) else { continue };
        for e in rd.flatten() {
            let prefs = e.path().join("prefs.js");
            if let Ok(t) = std::fs::metadata(&prefs).and_then(|m| m.modified()) {
                if newest.as_ref().map(|(n, _)| t > *n).unwrap_or(true) {
                    newest = Some((t, prefs));
                }
            }
        }
    }
    if let Some((_, prefs)) = newest {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&prefs) {
            let _ = writeln!(f, "user_pref(\"browser.sessionstore.resume_session_once\", true);");
            tracing::info!("Firefox will restore its last session ({})", prefs.display());
        }
    }
}

// ---------- X11 desktop access (Xorg sessions) ----------

/// Read the MIT cookie for display `n` from an Xauthority file.
pub fn read_cookie(path: &Path, n: &str) -> Option<Vec<u8>> {
    let data = std::fs::read(path).ok()?;
    let mut i = 0usize;
    let field = |d: &[u8], i: &mut usize| -> Option<Vec<u8>> {
        let len = u16::from_be_bytes([*d.get(*i)?, *d.get(*i + 1)?]) as usize;
        let v = d.get(*i + 2..*i + 2 + len)?.to_vec();
        *i += 2 + len;
        Some(v)
    };
    while i + 2 <= data.len() {
        i += 2; // family
        let _addr = field(&data, &mut i)?;
        let num = field(&data, &mut i)?;
        let name = field(&data, &mut i)?;
        let cookie = field(&data, &mut i)?;
        if name == b"MIT-MAGIC-COOKIE-1" && (num.is_empty() || num == n.as_bytes()) {
            return Some(cookie);
        }
    }
    None
}

pub fn display_number(display: &str) -> String {
    display.trim_start_matches(':').split('.').next().unwrap_or("0").to_string()
}

pub fn socket_for(display: &str) -> PathBuf {
    PathBuf::from(format!("/tmp/.X11-unix/X{}", display_number(display)))
}

pub fn cookie_for(sess: &Session) -> Vec<u8> {
    match (&sess.xauthority, &sess.display) {
        (Some(p), Some(d)) => read_cookie(p, &display_number(d)).unwrap_or_default(),
        _ => Vec::new(),
    }
}

pub fn connect(sess: &Session) -> Result<RustConnection> {
    let display = sess.display.as_deref().ok_or_else(|| anyhow!("no X display"))?;
    let sock = socket_for(display);
    let cookie = cookie_for(sess);
    let stream = UnixStream::connect(&sock).with_context(|| format!("connecting to {}", sock.display()))?;
    let (stream, _) = DefaultStream::from_unix_stream(stream)?;
    let (name, data) = if cookie.is_empty() { (vec![], vec![]) } else { (b"MIT-MAGIC-COOKIE-1".to_vec(), cookie) };
    Ok(RustConnection::connect_to_stream_with_auth_info(stream, 0, name, data)?)
}

#[derive(Debug, Clone, Serialize)]
pub struct DesktopWindow {
    pub id: u32,
    pub title: String,
    pub class: String,
    pub pid: Option<u32>,
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub family: Option<Family>,
    pub minimized: bool,
}

fn atom(conn: &RustConnection, name: &str) -> Result<Atom> {
    Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
}

/// Normal application windows on the desktop (what the task switcher shows).
pub fn list_windows(sess: &Session) -> Result<Vec<DesktopWindow>> {
    let conn = connect(sess)?;
    let root = conn.setup().roots[0].root;
    let client_list = atom(&conn, "_NET_CLIENT_LIST")?;
    let wm_state = atom(&conn, "_NET_WM_STATE")?;
    let skip = atom(&conn, "_NET_WM_STATE_SKIP_TASKBAR")?;
    let hidden = atom(&conn, "_NET_WM_STATE_HIDDEN")?;
    let wtype = atom(&conn, "_NET_WM_WINDOW_TYPE")?;
    let dock = atom(&conn, "_NET_WM_WINDOW_TYPE_DOCK")?;
    let desk = atom(&conn, "_NET_WM_WINDOW_TYPE_DESKTOP")?;
    let net_name = atom(&conn, "_NET_WM_NAME")?;
    let utf8 = atom(&conn, "UTF8_STRING")?;
    let net_pid = atom(&conn, "_NET_WM_PID")?;
    let list = conn.get_property(false, root, client_list, AtomEnum::WINDOW, 0, 1024)?.reply()?;
    let mut out = Vec::new();
    for win in list.value32().into_iter().flatten() {
        let states: Vec<u32> = conn
            .get_property(false, win, wm_state, AtomEnum::ATOM, 0, 32)?
            .reply()
            .ok()
            .and_then(|r| r.value32().map(|v| v.collect()))
            .unwrap_or_default();
        if states.contains(&skip) {
            continue;
        }
        let types: Vec<u32> = conn
            .get_property(false, win, wtype, AtomEnum::ATOM, 0, 16)?
            .reply()
            .ok()
            .and_then(|r| r.value32().map(|v| v.collect()))
            .unwrap_or_default();
        if types.contains(&dock) || types.contains(&desk) {
            continue;
        }
        // Dialogs belong to their main window; stream that instead.
        let transient = conn.get_property(false, win, AtomEnum::WM_TRANSIENT_FOR, AtomEnum::WINDOW, 0, 1)?.reply();
        if transient.map(|r| r.value_len > 0).unwrap_or(false) {
            continue;
        }
        let Ok(g) = conn.get_geometry(win)?.reply() else { continue };
        let Ok(t) = conn.translate_coordinates(win, root, 0, 0)?.reply() else { continue };
        let mut title = conn
            .get_property(false, win, net_name, utf8, 0, 512)?
            .reply()
            .map(|r| String::from_utf8_lossy(&r.value).to_string())
            .unwrap_or_default();
        if title.is_empty() {
            title = conn
                .get_property(false, win, AtomEnum::WM_NAME, AtomEnum::ANY, 0, 512)?
                .reply()
                .map(|r| String::from_utf8_lossy(&r.value).to_string())
                .unwrap_or_default();
        }
        let class = conn
            .get_property(false, win, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 256)?
            .reply()
            .map(|r| {
                let parts: Vec<&[u8]> = r.value.split(|&c| c == 0).filter(|p| !p.is_empty()).collect();
                parts.last().map(|p| String::from_utf8_lossy(p).to_string()).unwrap_or_default()
            })
            .unwrap_or_default();
        let pid = conn
            .get_property(false, win, net_pid, AtomEnum::CARDINAL, 0, 1)?
            .reply()
            .ok()
            .and_then(|r| r.value32().and_then(|mut v| v.next()));
        let family = pid.and_then(process_family);
        out.push(DesktopWindow {
            id: win,
            title,
            class,
            pid,
            x: t.dst_x as i32,
            y: t.dst_y as i32,
            w: g.width as u32,
            h: g.height as u32,
            family,
            minimized: states.contains(&hidden),
        });
    }
    Ok(out)
}

/// A small WebP preview of one desktop window (works even when it is covered).
pub fn window_thumbnail(sess: &Session, win: u32) -> Result<Vec<u8>> {
    let conn = connect(sess)?;
    let g = conn.get_geometry(win)?.reply()?;
    let (w, h) = (g.width as usize, g.height as usize);
    let mut img = conn.get_image(ImageFormat::Z_PIXMAP, win, 0, 0, g.width, g.height, !0)?.reply()?;
    // Without a compositor some windows read back black; use what is on screen instead.
    if img.data.chunks(4).all(|p| p[0] == 0 && p[1] == 0 && p[2] == 0) {
        let root = conn.setup().roots[0].clone();
        let t = conn.translate_coordinates(win, root.root, 0, 0)?.reply()?;
        let (x, y) = (t.dst_x.max(0), t.dst_y.max(0));
        let w2 = (g.width as i32).min(root.width_in_pixels as i32 - x as i32).max(1) as u16;
        let h2 = (g.height as i32).min(root.height_in_pixels as i32 - y as i32).max(1) as u16;
        if let Ok(r) = conn.get_image(ImageFormat::Z_PIXMAP, root.root, x, y, w2, h2, !0)?.reply() {
            if (w2, h2) == (g.width, g.height) {
                img = r;
            }
        }
    }
    let tw = 480.min(w).max(1);
    let th = (h * tw / w.max(1)).max(1);
    let mut rgb = vec![0u8; tw * th * 3];
    for y in 0..th {
        let sy = y * h / th;
        for x in 0..tw {
            let s = (sy * w + x * w / tw) * 4;
            let d = (y * tw + x) * 3;
            if s + 2 < img.data.len() {
                rgb[d] = img.data[s + 2];
                rgb[d + 1] = img.data[s + 1];
                rgb[d + 2] = img.data[s];
            }
        }
    }
    crate::stream::encoder::encode_webp(&rgb, tw as u32, th as u32, false, 70.0).ok_or_else(|| anyhow!("encode failed"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cookie_parsing() {
        let dir = std::env::temp_dir().join(format!("strymek-xa-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("xa");
        let c = crate::stream::launch::write_xauth(&p, 7).unwrap();
        assert_eq!(read_cookie(&p, "7"), Some(c));
        assert_eq!(display_number(":0.0"), "0");
    }
}
