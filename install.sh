#!/usr/bin/env bash
# Strymek installer for Ubuntu 24.04. Run as your normal user (not root):
#   ./install.sh [--bind 10.66.0.1:10000]
set -euo pipefail

BIND=""
if [[ "${1:-}" == "--bind" && -n "${2:-}" ]]; then BIND="$2"; fi
if [[ $EUID -eq 0 ]]; then
  echo "Run this as your normal user; it will ask for sudo when needed." >&2
  exit 1
fi
cd "$(dirname "$0")"

echo "==> Installing system packages (sudo)"
sudo apt-get update -qq
sudo apt-get install -y -qq build-essential pkg-config curl tigervnc-standalone-server dbus

if ! command -v cargo >/dev/null 2>&1; then
  echo "==> Installing the Rust toolchain (rustup)"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
  # shellcheck disable=SC1091
  source "$HOME/.cargo/env"
fi

echo "==> Building Strymek (release)"
cargo build --release
install -Dm755 target/release/strymek "$HOME/.local/bin/strymek"

echo "==> Configuring"
if [[ -z "$BIND" ]]; then
  WG_IP=$(ip -4 -o addr show wg0 2>/dev/null | awk '{print $4}' | cut -d/ -f1 || true)
  if [[ -n "$WG_IP" ]]; then
    BIND="$WG_IP:10000"
    echo "    Found WireGuard address $WG_IP; binding to $BIND"
  else
    BIND="0.0.0.0:10000"
    echo "    No wg0 interface found; binding to $BIND (restrict it with a firewall!)"
  fi
fi
"$HOME/.local/bin/strymek" init --bind "$BIND"
if ! grep -q '^password_hash = "\$argon2' "$HOME/.config/strymek/config.toml"; then
  "$HOME/.local/bin/strymek" passwd
fi
"$HOME/.local/bin/strymek" ca >/dev/null

echo "==> Installing the systemd user service"
install -Dm644 packaging/strymek.service "$HOME/.config/systemd/user/strymek.service"
systemctl --user daemon-reload
systemctl --user enable strymek.service
systemctl --user restart strymek.service
# Keep it running when you are logged out (e.g. while travelling).
sudo loginctl enable-linger "$USER"

PORT="${BIND##*:}"
HOST="${BIND%:*}"
[[ "$HOST" == "0.0.0.0" ]] && HOST=$(hostname -I | awk '{print $1}')
if command -v ufw >/dev/null && sudo ufw status | grep -q "Status: active"; then
  if ip link show wg0 >/dev/null 2>&1; then
    sudo ufw allow in on wg0 to any port "$PORT" proto tcp >/dev/null
    echo "    ufw: allowed port $PORT on wg0 only"
  fi
fi

cat <<MSG

Strymek is running:  https://$HOST:$PORT/

On your Mac:
  1. Open https://$HOST:$PORT/ca.pem, double-click the downloaded file, then in
     Keychain Access set "Strymek local CA" to Always Trust (one time).
  2. Open https://$HOST:$PORT/ and sign in.
Logs: journalctl --user -u strymek -f
MSG
