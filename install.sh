#!/usr/bin/env bash
# tunna installer: downloads the release tarball, verifies it, and puts
# everything where systemd expects it. Run as root, e.g.:
#
#   curl -fsSL https://raw.githubusercontent.com/denyryn/tunna/main/install.sh | sudo bash
#   curl -fsSL ... | sudo bash -s -- 0.1.0          # pinned version
#   curl -fsSL ... | sudo TUNNA_BIN_DIR=/opt/tunna/bin bash -s --   # custom paths
#   curl -fsSL ... | sudo TUNNA_SKIP_TRAY=1 bash -s --   # server only, no tray app
#
# Layout (all overridable):
#   binary            -> $TUNNA_BIN_DIR               (default /usr/local/bin)
#   units             -> $TUNNA_UNIT_DIR              (default /etc/systemd/system)
#   route data        -> $TUNNA_STATE_DIR             (default /var/lib/tunna)
#   env (token, etc.) -> $TUNNA_ENV                   (default /etc/tunna/tunna.env)
set -euo pipefail

TUNNA_BIN_DIR=${TUNNA_BIN_DIR:-/usr/local/bin}
TUNNA_UNIT_DIR=${TUNNA_UNIT_DIR:-/etc/systemd/system}
TUNNA_STATE_DIR=${TUNNA_STATE_DIR:-/var/lib/tunna}
TUNNA_ENV=${TUNNA_ENV:-/etc/tunna/tunna.env}
TUNNA_AUTOSTART_DIR=${TUNNA_AUTOSTART_DIR:-/etc/xdg/autostart}
TUNNA_VERSION=${1:-latest}
TUNNA_SKIP_TRAY=${TUNNA_SKIP_TRAY:-}

case "$(id -u)" in
  0) ;;
  *) echo "run me as root: curl -fsSL <url> | sudo bash" >&2; exit 1 ;;
esac

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
cd "$TMP"

# ---- fetch + verify ------------------------------------------------------

if [ "$TUNNA_VERSION" = latest ]; then
  TUNNA_VERSION="$(curl -fsSL https://api.github.com/repos/denyryn/tunna/releases/latest |
    sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -1)"
  [ -n "$TUNNA_VERSION" ] || { echo "could not read the latest tag" >&2; exit 1; }
fi

echo "tunna $TUNNA_VERSION -> $TUNNA_BIN_DIR, units -> $TUNNA_UNIT_DIR"
curl -fsSL -o t.tar.gz "https://github.com/denyryn/tunna/releases/download/$TUNNA_VERSION/tunna-x86_64-linux.tar.gz"
tar -xzf t.tar.gz
cd dist

[ "$(cut -d' ' -f1 tunna.sha256)" = "$(sha256sum tunna | cut -d' ' -f1)" ] ||
  { echo "checksum mismatch, aborting" >&2; exit 1; }
echo "checksum ok"

install -Dm755 tunna "$TUNNA_BIN_DIR/tunna"

# ---- tray app (desktop companion, skipped with TUNNA_SKIP_TRAY=1) --------

install_tray() {
  echo "installing tunna-tray (tray app for the desktop)"
  if [ ! -e tunna-tray-x86_64-linux.tar.gz ]; then
    echo "tunna-tray tarball is missing from the release, skipping" >&2
    return 0
  fi
  tar -xzf tunna-tray-x86_64-linux.tar.gz
  [ "$(cut -d' ' -f1 dist/tunna-tray.sha256)" = "$(sha256sum dist/tunna-tray | cut -d' ' -f1)" ] ||
    { echo "tray checksum mismatch, aborting" >&2; exit 1; }
  install -Dm755 dist/tunna-tray "$TUNNA_BIN_DIR/tunna-tray"
  # autostart on desktop login (system-wide, honors XDG autostart)
  mkdir -p "$TUNNA_AUTOSTART_DIR"
  cat > "$TUNNA_AUTOSTART_DIR/tunna-tray.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=tunna tray
Exec=$TUNNA_BIN_DIR/tunna-tray
Terminal=false
X-GNOME-Autostart-enabled=true
DESKTOP
  echo "tray app installed, autostart on login"
}

if [ -n "$TUNNA_SKIP_TRAY" ]; then
  echo "skipping tunna-tray (TUNNA_SKIP_TRAY=1)"
else
  # fetch the tray tarball from the same release, tolerate an absent artifact
  curl -fsSL -o tunna-tray-x86_64-linux.tar.gz \
    "https://github.com/denyryn/tunna/releases/download/$TUNNA_VERSION/tunna-tray-x86_64-linux.tar.gz" \
    || rm -f tunna-tray-x86_64-linux.tar.gz
  install_tray
fi

# ---- helper binaries ----------------------------------------------------

if command -v cloudflared >/dev/null 2>&1; then
  CLOUDFLARED="$(command -v cloudflared)"
else
  echo "cloudflared not found, installing the static binary to $TUNNA_BIN_DIR"
  curl -fsSL -o "$TUNNA_BIN_DIR/cloudflared" \
    https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-amd64
  chmod 755 "$TUNNA_BIN_DIR/cloudflared"
  CLOUDFLARED="$TUNNA_BIN_DIR/cloudflared"
fi

# ---- env file (never overwrite an existing one) -------------------------

mkdir -p "$(dirname "$TUNNA_ENV")" "$TUNNA_STATE_DIR"
if [ ! -e "$TUNNA_ENV" ]; then
  cat > "$TUNNA_ENV" <<'ENV'
# TUNNEL_TOKEN from the Cloudflare dashboard: Zero Trust -> Networks -> Tunnels
TUNNEL_TOKEN=
BASE_DOMAIN=example.com
# LISTEN_PORT=8088
# MANAGER_PORT=8080
ENV
  NEEDS_TOKEN=1
elif grep -q '^TUNNEL_TOKEN=..*' "$TUNNA_ENV"; then
  NEEDS_TOKEN=0
else
  NEEDS_TOKEN=1
fi

# ---- units --------------------------------------------------------------

cat > "$TUNNA_UNIT_DIR/tunna.service" <<UNIT
[Unit]
Description=tunna - cloudflare tunnel reverse proxy + route manager
After=network.target

[Service]
EnvironmentFile=$TUNNA_ENV
Environment=DATA_DIR=$TUNNA_STATE_DIR
WorkingDirectory=$TUNNA_STATE_DIR
ExecStart=$TUNNA_BIN_DIR/tunna
Restart=on-failure
RestartSec=2

[Install]
WantedBy=multi-user.target
UNIT

cat > "$TUNNA_UNIT_DIR/tunnel-cloudflared.service" <<UNIT
[Unit]
Description=Cloudflare tunnel connector for tunna
Requires=tunna.service
After=tunna.service

[Service]
EnvironmentFile=$TUNNA_ENV
ExecStart=$CLOUDFLARED tunnel --no-autoupdate run --token \$TUNNEL_TOKEN
Restart=on-failure
RestartSec=2

[Install]
WantedBy=multi-user.target
UNIT

systemctl daemon-reload

echo
if [ "$NEEDS_TOKEN" = 1 ]; then
  echo "tunna $TUNNA_VERSION installed."
  echo "One step left: put your tunnel token in $TUNNA_ENV (TUNNEL_TOKEN=...),"
  echo "then run:  sudo systemctl enable --now tunna.service tunnel-cloudflared.service"
  echo "Open the web UI at http://127.0.0.1:8080 after that."
else
  echo "tunna $TUNNA_VERSION installed and starting."
  systemctl enable tunna.service tunnel-cloudflared.service
  systemctl daemon-reload
  systemctl restart tunna.service tunnel-cloudflared.service
  echo "Web UI: http://127.0.0.1:8080  ·  status: systemctl status tunnel-cloudflared.service"
fi
