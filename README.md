# Strymek
Strymek By Robert Rymarczyk

Licence: MIT


Stream individual Linux applications to a browser: one app per window, one URL per app, on one fixed port.

```
https://10.66.0.1:10000/            dashboard: running apps + installed programs
https://10.66.0.1:10000/firefox1010 one running Firefox, streamed on its own
```

Each app runs on its own private, invisible X display on the workstation. Menus and dialogs are drawn inside that display, so they just work, and nothing appears on the physical 4K screen. The Mac needs nothing installed: every app is a web page you can add to the Dock.

## How it works

| Part | What it does |
| --- | --- |
| `strymek serve` | One Rust binary: HTTPS + WebSocket server on one port, login, dashboard, app launcher |
| Private display per app | `Xtigervnc` used only as a resizable headless X server (all VNC listeners disabled) |
| Built-in window manager | Main windows fill the browser window; dialogs are centred; follows browser resizes |
| Capture | XDamage: only changed rectangles are read and sent; an idle app costs nothing |
| Compression | Small updates (typing): lossless WebP (≈300 bytes per keystroke). Large updates (scrolling): lossy WebP, then sharpened to lossless ~0.3 s after they stop |
| Flow control | The browser acknowledges each frame; at most 2 unacknowledged frames, so slow links drop frames instead of building lag |
| Input | Keyboard and mouse injected with XTest; Cmd acts as Ctrl (switchable); your Mac keyboard layout's characters are typed as shown |
| Clipboard | Cmd+V pastes your Mac clipboard into the app; Cmd+C / Cmd+X copy from the app to your Mac |
| HiDPI | Apps render at 2× on a Retina Mac, so text is sharp |

## Install (Ubuntu 24.04 workstation)

```bash
tar xzf strymek-0.3.0.tar.gz && cd strymek
./install.sh                       # binds to your wg0 address if it exists
# or: ./install.sh --bind 10.66.0.1:10000
```

The installer:

1. installs `build-essential`, `pkg-config`, `tigervnc-standalone-server` and `dbus`, plus Rust if missing;
2. builds and installs `~/.local/bin/strymek`;
3. writes `~/.config/strymek/config.toml` and asks for a login password (12+ characters);
4. installs and starts a systemd user service, and enables linger so it keeps running while you are logged out.

Manual equivalent:

```bash
sudo apt install tigervnc-standalone-server build-essential pkg-config
cargo build --release && install -Dm755 target/release/strymek ~/.local/bin/strymek
strymek init --bind 10.66.0.1:10000
strymek passwd
strymek serve
```

## On the Mac (one time)

1. Connect WireGuard.
2. Open `https://10.66.0.1:10000/ca.pem`. Double-click the downloaded file, open **Keychain Access**, find **Strymek local CA**, and set **Trust → Always Trust**. After that there are no certificate warnings. The CA is created on the workstation and never leaves it except as this public certificate.
3. Open `https://10.66.0.1:10000/` and sign in.

## Daily use

- **Dashboard:** click a program to start it; it opens in its own window. **Running** shows every live app with a preview, its address, and Open / Copy URL / Stop.
- **Make it an app:** in Safari, with the app's window in front, choose **File → Add to Dock**. The Dock icon always opens that program, reusing the running copy or starting a new one.
- **Closing the window** does not stop the app; reopen its URL and it is exactly as you left it. **Stop** on the dashboard, or quitting the app itself, ends it.
- **Menu:** the small tab in the top-right corner of each app window toggles Cmd-as-Ctrl, types your clipboard as keystrokes (for apps that ignore paste), redraws, and goes full screen.
- Browsers keep a few shortcuts for themselves (Cmd+Q, Cmd+W). Use the app's own menu for those.

## Apps already open on your desktop

The dashboard's **On your desktop** section lists what is running on the workstation's own screen. There are two ways to take it over:

| | Move here | Stream window (live) |
| --- | --- | --- |
| What happens | Strymek closes the app on the desktop and reopens it in Strymek with your normal profile | Strymek streams the actual window as it is, and your input goes into it |
| Keeps | Firefox/Chrome tabs and logins, VS Code workspace and unsaved edits (hot exit), LibreOffice document recovery | Everything: running terminals, unsaved work, any app |
| Works with | Firefox, Chrome/Chromium/Brave/Edge, VS Code, LibreOffice | Any window |
| Desktop session | Wayland or Xorg; the screen can stay locked | **Xorg only** ("Ubuntu on Xorg" at the login screen) |
| Physical monitor | Shows nothing | Shows what you do while unlocked |

**Full desktop.** The **Full desktop** button (dashboard header, or the menu of any live window) streams the whole workstation screen, scaled down to fit your Mac window. Use it to type your password on the lock screen, or to work across several windows. The stream menu switches between **Fit to window** and **Actual size (scroll)**. The screen keeps its own resolution; only the view on the Mac is scaled. Full desktop checks the screen for changes several times a second while you use it, since the lock screen does not report its own redraws. That costs some CPU on the workstation, so stop it when you are done.

**Live windows and the lock screen.** A locked desktop shows the lock screen in a live window. Open **Full desktop** and type your password there, or use **Unlock** on the dashboard (or the stream menu). Strymek then locks the desktop again once no live window has been watched for `relock_after_secs` (default 60). Unlocking uses `loginctl unlock-session`, so your Strymek login is what protects the desktop: keep Strymek behind WireGuard.

When a live stream starts, the window is resized to fit your browser window, and it is put back where it was when you press **Stop streaming**. The window is brought to the front whenever you click or type into it.

**Switching to Xorg (for live windows):** log out, click your name, click the gear at the bottom right, choose **Ubuntu on Xorg**, and log in. Ubuntu remembers the choice.

## Browsers and single-instance apps

Firefox, Chrome/Chromium/Brave/Edge, VS Code and LibreOffice only allow one copy per profile. Strymek gives each app it starts from **Programs** its own persistent profile slot (`~/.local/share/strymek/profiles/<app>/p1`, `p2`…; snap apps use `~/snap/<name>/common/strymek-profiles/`). Logins and settings in a slot are kept for next time. VS Code's first profile copies your `settings.json` and `keybindings.json`.

**Move here** uses your normal profile instead, with your real D-Bus session and keyring, so saved passwords and cookies keep working.

Other apps get a private D-Bus session (`isolate_dbus = true`), so GNOME Terminal, Files and similar open here instead of on the physical desktop.

## Security

| Layer | Control |
| --- | --- |
| Network | Bind to the WireGuard address only (`bind = "10.66.0.1:10000"`); forward only WireGuard's UDP port on the router |
| Transport | TLS via rustls; certificate from a local CA created on first start (or your own via `tls_cert` / `tls_key`) |
| Login | Argon2id password hash; 5 failures lock that address out for 5 minutes |
| Session | Random 256-bit token in a `Secure; HttpOnly; SameSite=Strict` cookie; ends after `session_idle_hours` without activity |
| Requests | State-changing API calls need the same-origin `Origin` header plus an `x-strymek` header; WebSockets check session and `Origin` |
| URLs | A stream URL is a name, not a credential: without a session it redirects to the login page |
| Browser | Strict CSP (no inline script, no third-party anything), `frame-ancestors 'none'`, HSTS |
| X displays | Each display has its own random MIT cookie **and** only accepts processes of your Unix user (server-interpreted `localuser`); no TCP; VNC ports disabled |
| Process | Runs as your user, never root; apps run as you |
| Desktop | Remote unlock only on request; automatic re-lock after `relock_after_secs` without a live window being watched |

Audit trail: `journalctl --user -u strymek` logs logins, failed logins, launches, attaches and stops with the source address.

## Configuration

See `config.example.toml`. Useful commands:

```bash
strymek apps     # what the dashboard will list
strymek ca       # path of the CA certificate
strymek passwd   # change the password
systemctl --user restart strymek
journalctl --user -u strymek -f
```

Per-stream logs: `$XDG_RUNTIME_DIR/strymek/app-<slug>.log` and `x-<slug>.log`.

## Limitations (v0.1)

- Live streaming of existing desktop windows needs an Xorg session; on Wayland only **Move here** is available.
- In a live window, menus that open outside the window's area are cut off at its edge.
- Video uses WebP tiles. Scrolling a full Retina window costs roughly 1–2 MB/s while it moves. Hardware H.264 is planned for Phase 2.
- No audio, file transfer, or drag-and-drop yet.
- Very large clipboard contents (over 1 MB, which X transfers in chunks) are not bridged.
- An app with several main windows stacks them in one browser window; the most recently used is on top.

## Tested

On Ubuntu 24.04 with headless Chromium, including a simulated 4K Xorg desktop (Xvfb + openbox): live window streaming with resize and restore, typing into a desktop terminal, "Move here" for LibreOffice. Also: login and lockout; launching XTerm and LibreOffice Writer; typing, including shifted, accented and non-Latin characters; menus and dialogs; clipboard in both directions; live resize; the app exit ending its stream; Stop; the API refusing requests without a session or the CSRF header; and other Unix users refused by the X server.
