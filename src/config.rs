//! Configuration file and well-known paths.
//!
//! Config lives at `~/.config/strymek/config.toml`. Everything is optional
//! except `password_hash`, which `strymek passwd` writes.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Address and port to listen on. Bind to your WireGuard address
    /// (e.g. "10.66.0.1:10000") so the server is only reachable through the tunnel.
    pub bind: SocketAddr,
    /// Argon2id hash of the login password. Set it with `strymek passwd`.
    pub password_hash: String,
    /// Log a session out after this many hours without activity.
    pub session_idle_hours: u64,
    /// Host names and IP addresses the TLS certificate is valid for.
    pub tls_names: Vec<String>,
    /// Use your own certificate instead of the generated one (PEM files).
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    /// X server binary used for each app's private display. Must support RandR resizing.
    pub x_server: String,
    /// Run each app on its own private D-Bus session bus (stops single-instance apps
    /// from opening their window on the physical desktop).
    pub isolate_dbus: bool,
    /// Force a UI scale (1 or 2). 0 = follow the browser's devicePixelRatio.
    pub scale: u32,
    /// Maximum frames per second sent to the browser.
    pub max_fps: u32,
    /// Quality (0-100) for lossy WebP used on large, fast-changing regions.
    pub lossy_quality: f32,
    /// Extra programs to list on the dashboard, in addition to installed .desktop apps.
    #[serde(rename = "app")]
    pub apps: Vec<CustomApp>,
    /// .desktop ids (e.g. "org.gnome.Settings") to hide from the dashboard.
    pub hidden_apps: Vec<String>,
    /// After Strymek unlocks your desktop, lock it again once no live desktop
    /// window has been watched for this many seconds (0 = never).
    pub relock_after_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomApp {
    pub id: String,
    pub name: String,
    /// Command line, split on whitespace (quote-free).
    pub exec: String,
    #[serde(default)]
    pub icon: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:10000".parse().unwrap(),
            password_hash: String::new(),
            session_idle_hours: 12,
            tls_names: Vec::new(),
            tls_cert: None,
            tls_key: None,
            x_server: "Xtigervnc".into(),
            isolate_dbus: true,
            scale: 0,
            max_fps: 30,
            lossy_quality: 80.0,
            apps: Vec::new(),
            hidden_apps: Vec::new(),
            relock_after_secs: 60,
        }
    }
}

pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("strymek")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

/// Persistent data: browser profiles, etc.
pub fn data_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("strymek")
}

/// Private runtime files (X authority cookies). Prefers $XDG_RUNTIME_DIR (tmpfs, 0700).
pub fn runtime_dir() -> PathBuf {
    let base = dirs::runtime_dir().unwrap_or_else(|| {
        dirs::cache_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("strymek-run")
    });
    base.join("strymek")
}

pub fn ensure_private_dir(p: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(p).with_context(|| format!("creating {}", p.display()))?;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        ensure_private_dir(&config_dir())?;
        let path = config_path();
        let text = format!(
            "# Strymek configuration. See README.md for every option.\n{}",
            toml::to_string_pretty(self)?
        );
        std::fs::write(&path, text)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        Ok(())
    }

    /// Fill tls_names with this host's name and addresses if the list is empty.
    pub fn default_tls_names() -> Vec<String> {
        let mut names = vec!["localhost".to_string()];
        if let Ok(h) = std::fs::read_to_string("/etc/hostname") {
            let h = h.trim();
            if !h.is_empty() {
                names.push(h.to_string());
            }
        }
        if let Ok(ifs) = if_addrs::get_if_addrs() {
            for i in ifs {
                if let std::net::IpAddr::V4(v4) = i.ip() {
                    names.push(v4.to_string());
                }
            }
        }
        names.sort();
        names.dedup();
        names
    }
}
