#!/usr/bin/env bash
# Harmony server installer for Debian / Ubuntu / Raspberry Pi OS.
#
# Architecture-agnostic: x86-64, arm64, armv7 and armv6 are all detected below
# and get the matching MediaMTX build.
#
# Installs MediaMTX, installs the Harmony control server, and registers both as
# systemd services. Safe to re-run: it upgrades MediaMTX in place and leaves an
# existing /etc/harmony/harmony.env untouched.
#
#   sudo ./install.sh

set -euo pipefail

# Pinned deliberately: mediamtx.yml is validated against this release, and
# MediaMTX removes config keys between versions (a stale key aborts startup).
MEDIAMTX_VERSION="${MEDIAMTX_VERSION:-1.21.0}"
PREFIX=/opt/harmony
CONF_DIR=/etc/harmony
SRC_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if [[ $EUID -ne 0 ]]; then
  echo "This script needs root: sudo ./install.sh" >&2
  exit 1
fi

# --- architecture -----------------------------------------------------------
case "$(uname -m)" in
  aarch64|arm64) MTX_ARCH=linux_arm64 ;;
  armv7l)        MTX_ARCH=linux_armv7 ;;
  armv6l)        MTX_ARCH=linux_armv6 ;;
  x86_64)        MTX_ARCH=linux_amd64 ;;
  *) echo "Unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac
echo "==> Architecture $(uname -m) -> ${MTX_ARCH}"

# --- dependencies -----------------------------------------------------------
# Node 24, not 22: the database is `node:sqlite`, which is only unflagged from
# Node 22.13. Installing the 22.x line risks landing on an earlier 22 where
# `import ... from "node:sqlite"` throws at startup with nothing to install to
# fix it, so take the next LTS line and leave no room for doubt.
if ! command -v node >/dev/null 2>&1; then
  echo "==> Installing Node.js 24"
  curl -fsSL https://deb.nodesource.com/setup_24.x | bash -
  apt-get install -y nodejs
fi

NODE_MAJOR="$(node -p 'process.versions.node.split(".")[0]')"
NODE_MINOR="$(node -p 'process.versions.node.split(".")[1]')"
if (( NODE_MAJOR < 22 )) || { (( NODE_MAJOR == 22 )) && (( NODE_MINOR < 13 )); }; then
  echo "Node 22.13+ required (node:sqlite is flagged before that), found $(node -v)" >&2
  echo "  curl -fsSL https://deb.nodesource.com/setup_24.x | sudo bash - && sudo apt-get install -y nodejs" >&2
  exit 1
fi

# Fail now rather than at first boot if this build has no SQLite.
if ! node -e 'import("node:sqlite").then(()=>0,()=>process.exit(1))'; then
  echo "This Node build has no node:sqlite module. Install Node 24 from nodesource." >&2
  exit 1
fi
echo "==> Node $(node -v)"

# --- MediaMTX ---------------------------------------------------------------
echo "==> Installing MediaMTX ${MEDIAMTX_VERSION}"
TARBALL="mediamtx_v${MEDIAMTX_VERSION}_${MTX_ARCH}.tar.gz"
URL="https://github.com/bluenviron/mediamtx/releases/download/v${MEDIAMTX_VERSION}/${TARBALL}"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

curl -fsSL "$URL" -o "$TMP/$TARBALL"
tar -xzf "$TMP/$TARBALL" -C "$TMP"
install -m 0755 "$TMP/mediamtx" /usr/local/bin/mediamtx
echo "    $(/usr/local/bin/mediamtx --version 2>&1 | head -n1)"

# --- Harmony control server -------------------------------------------------
echo "==> Installing Harmony control server to ${PREFIX}"
mkdir -p "$PREFIX"
cp -r "$SRC_DIR/src" "$SRC_DIR/package.json" "$PREFIX/"

# Prefer the committed lockfile so the host gets the same tree that was tested.
if [[ -f "$SRC_DIR/package-lock.json" ]]; then
  cp "$SRC_DIR/package-lock.json" "$PREFIX/"
  (cd "$PREFIX" && npm ci --omit=dev --no-audit --no-fund)
else
  (cd "$PREFIX" && npm install --omit=dev --no-audit --no-fund)
fi

mkdir -p "$CONF_DIR"
install -m 0644 "$SRC_DIR/mediamtx.yml" "$CONF_DIR/mediamtx.yml"

if [[ ! -f "$CONF_DIR/harmony.env" ]]; then
  install -m 0640 "$SRC_DIR/.env.example" "$CONF_DIR/harmony.env"
  echo "    Created ${CONF_DIR}/harmony.env -- EDIT THIS before starting."
else
  echo "    Keeping existing ${CONF_DIR}/harmony.env"
fi

# --- service user -----------------------------------------------------------
if ! id -u harmony >/dev/null 2>&1; then
  useradd --system --no-create-home --shell /usr/sbin/nologin harmony
fi
chown -R harmony:harmony "$PREFIX"
chown -R root:harmony "$CONF_DIR"
chmod 0750 "$CONF_DIR"

# --- systemd ----------------------------------------------------------------
echo "==> Registering systemd units"
install -m 0644 "$SRC_DIR/systemd/mediamtx.service" /etc/systemd/system/mediamtx.service
install -m 0644 "$SRC_DIR/systemd/harmony-server.service" /etc/systemd/system/harmony-server.service

# Keeps the advertised ICE address correct on a dynamic public IP.
install -m 0755 "$SRC_DIR/ip-watch.sh" /usr/local/bin/harmony-ip-watch.sh
install -m 0644 "$SRC_DIR/systemd/harmony-ip-watch.service" /etc/systemd/system/harmony-ip-watch.service
install -m 0644 "$SRC_DIR/systemd/harmony-ip-watch.timer" /etc/systemd/system/harmony-ip-watch.timer

systemctl daemon-reload
systemctl enable harmony-server mediamtx harmony-ip-watch.timer

cat <<EOF

==> Installed.

Next steps:

  1. Edit the configuration:
       sudo nano ${CONF_DIR}/harmony.env

     At minimum set HARMONY_SIGNALING_URL and MTX_WEBRTCADDITIONALHOSTS to the
     address clients will actually use.

  2. Forward UDP port 8189 on your router to this host. WebRTC media travels over
     UDP and a Cloudflare Tunnel cannot carry it -- only the signaling on 8889
     can go through a tunnel.

  3. Start both services (order matters on first boot -- the control server
     answers MediaMTX's auth hook):
       sudo systemctl start harmony-server mediamtx

  4. Check it:
       curl -s http://127.0.0.1:8080/api/health
       journalctl -u harmony-server -u mediamtx -f

EOF
