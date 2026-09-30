//! Starting a private X display and an application inside it.

use crate::apps::{program_exists, AppInfo};
use crate::config::{data_dir, Config};
use anyhow::{anyhow, Context, Result};
use rand::RngCore;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub fn pick_display() -> Result<u32> {
    for n in 100..600 {
        let sock = PathBuf::from(format!("/tmp/.X11-unix/X{n}"));
        let lock = PathBuf::from(format!("/tmp/.X{n}-lock"));
        if !sock.exists() && !lock.exists() {
            return Ok(n);
        }
    }
    Err(anyhow!("no free X display number between :100 and :599"))
}

pub fn socket_path(n: u32) -> PathBuf {
    PathBuf::from(format!("/tmp/.X11-unix/X{n}"))
}

/// Write an Xauthority file holding one wildcard MIT-MAGIC-COOKIE-1 entry.
pub fn write_xauth(path: &Path, display: u32) -> Result<Vec<u8>> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut cookie = vec![0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut cookie);
    let mut buf = Vec::new();
    let mut field = |b: &[u8]| {
        buf.extend_from_slice(&(b.len() as u16).to_be_bytes());
        buf.extend_from_slice(b);
    };
    let mut out = Vec::new();
    out.extend_from_slice(&0xffffu16.to_be_bytes()); // FamilyWild
    field(b"");
    field(display.to_string().as_bytes());
    field(b"MIT-MAGIC-COOKIE-1");
    field(&cookie);
    out.extend_from_slice(&buf);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    std::io::Write::write_all(&mut f, &out)?;
    Ok(cookie)
}

fn log_file(dir: &Path, name: &str) -> Stdio {
    std::fs::File::create(dir.join(name)).map(Stdio::from).unwrap_or_else(|_| Stdio::null())
}

pub fn spawn_x_server(
    cfg: &Config,
    n: u32,
    w: i32,
    h: i32,
    scale: u32,
    xauth: &Path,
    run_dir: &Path,
    slug: &str,
) -> Result<Child> {
    let dpi = (96 * scale.max(1)).to_string();
    let mut cmd = Command::new(&cfg.x_server);
    cmd.arg(format!(":{n}"));
    let is_vnc = Path::new(&cfg.x_server)
        .file_name()
        .map(|f| f.to_string_lossy().to_lowercase().contains("vnc"))
        .unwrap_or(false);
    if is_vnc {
        // Used purely as a resizable headless X server: every VNC listener is disabled.
        cmd.args(["-geometry", &format!("{w}x{h}"), "-depth", "24", "-rfbport", "-1"]);
        cmd.args(["-SecurityTypes", "None", "-localhost", "-desktop", slug]);
    } else {
        cmd.args(["-screen", "0", &format!("{w}x{h}x24")]);
    }
    cmd.args(["-nolisten", "tcp", "-dpi", &dpi, "-auth"]).arg(xauth);
    cmd.stdin(Stdio::null())
        .stdout(log_file(run_dir, &format!("x-{slug}.log")))
        .stderr(log_file(run_dir, &format!("x-{slug}.log")))
        .process_group(0);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("starting {} (is it installed? sudo apt install tigervnc-standalone-server)", cfg.x_server))?;
    let sock = socket_path(n);
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        if sock.exists() {
            return Ok(child);
        }
        if let Some(st) = child.try_wait()? {
            return Err(anyhow!("X server exited early ({st}); see {}", run_dir.join(format!("x-{slug}.log")).display()));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    Err(anyhow!("X server did not start within 10s"))
}

/// Apps that allow only one running copy per profile, and need special handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Family {
    Firefox,
    Chromium,
    VsCode,
    LibreOffice,
    Other,
}

/// Classify a program path or executable name.
pub fn family_of_program(prog: &str) -> Family {
    let b = Path::new(prog).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    match b.as_str() {
        "firefox" | "firefox-bin" | "firefox-esr" | "librewolf" => Family::Firefox,
        "google-chrome" | "google-chrome-stable" | "google-chrome-beta" | "chrome" | "chromium" | "chromium-browser"
        | "brave-browser" | "brave" | "microsoft-edge" | "microsoft-edge-stable" | "msedge" | "vivaldi"
        | "vivaldi-stable" | "vivaldi-bin" => Family::Chromium,
        "code" | "code-insiders" | "codium" | "cursor" => Family::VsCode,
        "libreoffice" | "soffice" | "soffice.bin" | "oosplash" | "localc" | "lowriter" | "loimpress" => {
            Family::LibreOffice
        }
        _ => Family::Other,
    }
}

fn family(prog: &str) -> Family {
    family_of_program(prog)
}

/// Family of an app's program (after any `env VAR=x` prefix).
pub fn app_family(app: &AppInfo) -> Family {
    app.argv.get(program_index(&app.argv)).map(|p| family(p)).unwrap_or(Family::Other)
}

/// The real program in an argv that may start with `env VAR=x`.
fn program_index(argv: &[String]) -> usize {
    if argv.first().map(|p| p.ends_with("env")).unwrap_or(false) {
        let mut i = 1;
        while i < argv.len() && argv[i].contains('=') {
            i += 1;
        }
        i
    } else {
        0
    }
}

/// Whether this app needs its own profile directory to run a second instance.
pub fn needs_profile(app: &AppInfo) -> bool {
    matches!(app_family(app), Family::Firefox | Family::Chromium | Family::VsCode | Family::LibreOffice)
}

/// Directory for profile slot `slot` of this app. Snap apps can only write under ~/snap/<name>/.
pub fn profile_dir(app: &AppInfo, slot: u32) -> PathBuf {
    let i = program_index(&app.argv);
    let prog = app.argv.get(i).cloned().unwrap_or_default();
    if let Some(snap) = prog.strip_prefix("/snap/bin/") {
        let name = snap.split('.').next().unwrap_or(snap);
        if let Some(h) = dirs::home_dir() {
            return h.join("snap").join(name).join("common").join("strymek-profiles").join(format!("p{slot}"));
        }
    }
    data_dir().join("profiles").join(&app.short).join(format!("p{slot}"))
}

fn seed_vscode_profile(dir: &Path) {
    let user = dir.join("User");
    if user.exists() {
        return;
    }
    let _ = std::fs::create_dir_all(&user);
    if let Some(cfg) = dirs::config_dir() {
        for f in ["settings.json", "keybindings.json"] {
            let src = cfg.join("Code").join("User").join(f);
            if src.exists() {
                let _ = std::fs::copy(&src, user.join(f));
            }
        }
    }
}

pub struct LaunchSpec<'a> {
    pub app: &'a AppInfo,
    pub display: u32,
    pub xauth: &'a Path,
    pub scale: u32,
    pub profile: Option<PathBuf>,
    pub run_dir: &'a Path,
    pub slug: &'a str,
    /// Use your normal profile (moved from the desktop): real D-Bus session and
    /// keyring, and restore the previous session.
    pub real_profile: bool,
}

pub fn spawn_app(cfg: &Config, s: &LaunchSpec) -> Result<Child> {
    let mut argv = s.app.argv.clone();
    let pi = program_index(&argv);
    let fam = argv.get(pi).map(|p| family(p)).unwrap_or(Family::Other);
    if let Some(dir) = &s.profile {
        std::fs::create_dir_all(dir)?;
        let d = dir.to_string_lossy().to_string();
        let extra: Vec<String> = match fam {
            Family::Firefox => vec!["--no-remote".into(), "--new-instance".into(), "--profile".into(), d],
            Family::Chromium => vec![
                format!("--user-data-dir={d}"),
                "--password-store=basic".into(),
                "--no-first-run".into(),
                "--no-default-browser-check".into(),
                "--ozone-platform=x11".into(),
            ],
            Family::VsCode => {
                seed_vscode_profile(dir);
                vec!["--user-data-dir".into(), d, "--new-window".into(), "--wait".into(), "--password-store=basic".into()]
            }
            // LibreOffice hands new windows to any running instance of the same
            // profile (possibly on the physical desktop); a separate profile avoids that.
            Family::LibreOffice => vec![format!("-env:UserInstallation=file://{d}")],
            Family::Other => vec![],
        };
        argv.splice(pi + 1..pi + 1, extra);
    } else if s.real_profile {
        let extra: Vec<String> = match fam {
            Family::Chromium => vec!["--restore-last-session".into(), "--ozone-platform=x11".into()],
            Family::VsCode => vec!["--wait".into()],
            _ => vec![],
        };
        argv.splice(pi + 1..pi + 1, extra);
    }
    let isolate = cfg.isolate_dbus && !s.real_profile;
    if isolate && program_exists("dbus-run-session") {
        argv.splice(0..0, ["dbus-run-session".to_string(), "--".to_string()]);
    }
    let scale = s.scale.max(1);
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("WAYLAND_SOCKET")
        .env("DISPLAY", format!(":{}", s.display))
        .env("XAUTHORITY", s.xauth)
        .env("XDG_SESSION_TYPE", "x11")
        .env("GDK_BACKEND", "x11")
        .env("QT_QPA_PLATFORM", "xcb")
        .env("SDL_VIDEODRIVER", "x11")
        .env("MOZ_ENABLE_WAYLAND", "0")
        .env("ELECTRON_OZONE_PLATFORM_HINT", "x11")
        .env("GDK_SCALE", scale.to_string())
        .env("GDK_DPI_SCALE", format!("{}", 1.0 / scale as f64))
        .env("STRYMEK_SLUG", s.slug)
        .stdin(Stdio::null())
        .stdout(log_file(s.run_dir, &format!("app-{}.log", s.slug)))
        .stderr(log_file(s.run_dir, &format!("app-{}.log", s.slug)))
        .process_group(0);
    if isolate {
        cmd.env_remove("DBUS_SESSION_BUS_ADDRESS");
    }
    if let Some(h) = dirs::home_dir() {
        cmd.current_dir(h);
    }
    cmd.spawn().with_context(|| format!("starting {}", argv.join(" ")))
}

/// Send a signal to a whole process group.
pub fn kill_group(child: &Child, sig: i32) {
    let pid = child.id() as i32;
    if pid > 0 {
        unsafe { libc::kill(-pid, sig) };
    }
}
