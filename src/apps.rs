//! Discovers launchable programs from freedesktop `.desktop` files.

use crate::config::Config;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize)]
pub struct AppInfo {
    /// Desktop file id without extension, e.g. "firefox_firefox" or "org.gnome.Terminal".
    pub id: String,
    pub name: String,
    pub comment: String,
    /// Short lowercase name used as the URL slug prefix, e.g. "firefox".
    pub short: String,
    #[serde(skip)]
    pub argv: Vec<String>,
    #[serde(skip)]
    pub icon_path: Option<PathBuf>,
    pub has_icon: bool,
}

fn application_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    // Lowest priority first; later entries override earlier ones with the same id.
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    let mut sys: Vec<PathBuf> = data_dirs.split(':').filter(|s| !s.is_empty()).map(PathBuf::from).collect();
    sys.reverse();
    for d in sys {
        dirs.push(d.join("applications"));
    }
    dirs.push("/var/lib/flatpak/exports/share/applications".into());
    dirs.push("/var/lib/snapd/desktop/applications".into());
    if let Some(h) = dirs::home_dir() {
        dirs.push(h.join(".local/share/flatpak/exports/share/applications"));
    }
    if let Some(d) = dirs::data_dir() {
        dirs.push(d.join("applications"));
    }
    dirs.dedup();
    dirs
}

/// Split an Exec line into arguments, handling double quotes and dropping field codes.
pub fn parse_exec(exec: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut had = false;
    let mut chars = exec.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                had = true;
            }
            '\\' if in_quotes => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            c if c.is_whitespace() && !in_quotes => {
                if had || !cur.is_empty() {
                    args.push(std::mem::take(&mut cur));
                    had = false;
                }
            }
            c => cur.push(c),
        }
    }
    if had || !cur.is_empty() {
        args.push(cur);
    }
    args.into_iter()
        .filter_map(|a| {
            if a.len() == 2 && a.starts_with('%') {
                return if a == "%%" { Some("%".into()) } else { None };
            }
            Some(a.replace("%%", "%"))
        })
        .collect()
}

fn short_name(argv: &[String], id: &str) -> String {
    // Skip "env VAR=x" style prefixes to find the real program.
    let mut prog = argv.first().map(String::as_str).unwrap_or(id);
    if Path::new(prog).file_name().and_then(|s| s.to_str()) == Some("env") {
        let mut i = 1;
        while i < argv.len() && argv[i].contains('=') {
            i += 1;
        }
        prog = argv.get(i).map(String::as_str).unwrap_or(id);
    }
    let base = Path::new(prog).file_name().and_then(|s| s.to_str()).unwrap_or(id);
    let mut s: String = base
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    while s.ends_with(|c: char| c.is_ascii_digit() || c == '-') {
        s.pop();
    }
    let s = s.trim_start_matches('-').to_string();
    if s.is_empty() { "app".into() } else { s.chars().take(24).collect() }
}

pub fn find_icon(name: &str) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    let p = Path::new(name);
    if p.is_absolute() {
        return p.exists().then(|| p.to_path_buf());
    }
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(d) = dirs::data_dir() {
        roots.push(d.join("icons"));
    }
    roots.push("/usr/share/icons".into());
    roots.push("/usr/local/share/icons".into());
    roots.push("/var/lib/flatpak/exports/share/icons".into());
    let themes = ["hicolor", "Yaru", "Adwaita", "gnome"];
    let sizes = ["256x256", "128x128", "96x96", "64x64", "48x48", "scalable", "512x512", "32x32"];
    for root in &roots {
        for theme in themes {
            for size in sizes {
                for ext in ["png", "svg"] {
                    let f = root.join(theme).join(size).join("apps").join(format!("{name}.{ext}"));
                    if f.exists() {
                        return Some(f);
                    }
                }
            }
        }
    }
    for ext in ["png", "svg"] {
        let f = PathBuf::from(format!("/usr/share/pixmaps/{name}.{ext}"));
        if f.exists() {
            return Some(f);
        }
    }
    None
}

fn parse_desktop_file(path: &Path) -> Option<AppInfo> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut in_entry = false;
    let mut kv: BTreeMap<String, String> = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            kv.entry(k.trim().to_string()).or_insert_with(|| v.trim().to_string());
        }
    }
    let is = |k: &str| kv.get(k).map(|v| v == "true").unwrap_or(false);
    if kv.get("Type").map(String::as_str) != Some("Application") || is("NoDisplay") || is("Hidden") || is("Terminal") {
        return None;
    }
    if let Some(try_exec) = kv.get("TryExec") {
        if !program_exists(try_exec) {
            return None;
        }
    }
    let argv = parse_exec(kv.get("Exec")?);
    if argv.is_empty() || !program_exists(&argv[0]) {
        return None;
    }
    let id = path.file_stem()?.to_string_lossy().to_string();
    let icon_path = kv.get("Icon").and_then(|i| find_icon(i));
    Some(AppInfo {
        short: short_name(&argv, &id),
        name: kv.get("Name").cloned().unwrap_or_else(|| id.clone()),
        comment: kv.get("Comment").cloned().unwrap_or_default(),
        has_icon: icon_path.is_some(),
        icon_path,
        argv,
        id,
    })
}

pub fn program_exists(prog: &str) -> bool {
    let p = Path::new(prog);
    if p.is_absolute() {
        return p.exists();
    }
    std::env::var("PATH")
        .unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin:/snap/bin".into())
        .split(':')
        .chain(["/snap/bin"])
        .any(|d| Path::new(d).join(prog).exists())
}

/// All launchable apps, sorted by name.
pub fn discover(cfg: &Config) -> Vec<AppInfo> {
    let mut by_id: BTreeMap<String, AppInfo> = BTreeMap::new();
    for dir in application_dirs() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("desktop") {
                continue;
            }
            if let Some(app) = parse_desktop_file(&p) {
                by_id.insert(app.id.clone(), app);
            }
        }
    }
    for c in &cfg.apps {
        let argv: Vec<String> = c.exec.split_whitespace().map(String::from).collect();
        if argv.is_empty() {
            continue;
        }
        let icon_path = c.icon.as_deref().and_then(find_icon);
        by_id.insert(
            c.id.clone(),
            AppInfo {
                id: c.id.clone(),
                name: c.name.clone(),
                comment: String::new(),
                short: short_name(&argv, &c.id),
                has_icon: icon_path.is_some(),
                icon_path,
                argv,
            },
        );
    }
    for h in &cfg.hidden_apps {
        by_id.remove(h);
    }
    let mut v: Vec<AppInfo> = by_id.into_values().collect();
    v.sort_by_key(|a| a.name.to_lowercase());
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exec_parsing() {
        assert_eq!(parse_exec("/snap/bin/firefox %u"), vec!["/snap/bin/firefox"]);
        assert_eq!(parse_exec("code --new-window %F"), vec!["code", "--new-window"]);
        assert_eq!(parse_exec("sh -c \"echo hi there\""), vec!["sh", "-c", "echo hi there"]);
        assert_eq!(parse_exec("app 100%%"), vec!["app", "100%"]);
    }
    #[test]
    fn shorts() {
        assert_eq!(short_name(&["/snap/bin/firefox".into()], "x"), "firefox");
        assert_eq!(short_name(&["gnome-terminal".into()], "x"), "gnome-terminal");
        assert_eq!(short_name(&["env".into(), "A=1".into(), "python3".into()], "x"), "python");
    }
}
