#!/usr/bin/env bash
# inginx installer.
#
#   curl -fsSL https://raw.githubusercontent.com/overcuriousity/inginx/main/install.sh | sudo bash
#
# Environment:
#   INGINX_CHANNEL=stable|nightly   which release to pull (default: stable, falls back to nightly)
#   INGINX_VERSION=v0.1.0           pin an exact release tag (overrides channel)
#   INGINX_NO_SERVICE=1             install the binary only, no systemd unit
#   INGINX_NO_START=1               install and enable the unit but do not start it
#   INGINX_PREFIX=/usr/local/bin    install directory
set -euo pipefail

REPO="overcuriousity/inginx"
PREFIX="${INGINX_PREFIX:-/usr/local/bin}"
CHANNEL="${INGINX_CHANNEL:-stable}"
STATE_DIR="/var/lib/inginx"
UNIT="/etc/systemd/system/inginx.service"

log()  { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m==>\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

[ "$(id -u)" -eq 0 ] || die "run as root (pipe into 'sudo bash')"

case "$(uname -s)" in
  Linux) ;;
  *) die "unsupported OS: $(uname -s) (Linux only)" ;;
esac
case "$(uname -m)" in
  x86_64|amd64) ASSET="inginx-x86_64-linux" ;;
  *) die "unsupported architecture: $(uname -m) (prebuilt binaries are x86_64 only; build from source with 'cargo build --release')" ;;
esac

for c in curl install; do
  command -v "$c" >/dev/null 2>&1 || die "missing required command: $c"
done

api() { curl -fsSL -H 'Accept: application/vnd.github+json' "https://api.github.com/repos/$REPO/$1"; }

latest_stable_tag() {
  api releases/latest 2>/dev/null | sed -n 's/.*"tag_name":[ ]*"\([^"]*\)".*/\1/p' | head -n1
}

TAG="${INGINX_VERSION:-}"
if [ -z "$TAG" ]; then
  if [ "$CHANNEL" = "nightly" ]; then
    TAG="nightly"
  else
    TAG="$(latest_stable_tag || true)"
    if [ -z "$TAG" ]; then
      warn "no tagged release found, falling back to the nightly build of main"
      TAG="nightly"
    fi
  fi
fi

BASE="https://github.com/$REPO/releases/download/$TAG"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

log "downloading inginx $TAG ($ASSET)"
curl -fSL --progress-bar -o "$TMP/$ASSET" "$BASE/$ASSET" \
  || die "download failed: $BASE/$ASSET"

if curl -fsSL -o "$TMP/$ASSET.sha256" "$BASE/$ASSET.sha256" 2>/dev/null; then
  if command -v sha256sum >/dev/null 2>&1; then
    log "verifying checksum"
    ( cd "$TMP" && sha256sum -c "$ASSET.sha256" >/dev/null ) || die "checksum mismatch"
  else
    warn "sha256sum not available, skipping checksum verification"
  fi
else
  warn "no published checksum for $TAG, skipping verification"
fi

log "installing $PREFIX/inginx"
install -d "$PREFIX"
install -m 0755 "$TMP/$ASSET" "$PREFIX/inginx"

if [ "${INGINX_NO_SERVICE:-0}" = "1" ]; then
  log "done: $PREFIX/inginx (no service installed)"
  exit 0
fi

if ! command -v systemctl >/dev/null 2>&1; then
  warn "systemd not found, skipping service installation"
  exit 0
fi

install -d -m 0755 "$STATE_DIR"

if [ -e "$UNIT" ]; then
  log "keeping existing unit $UNIT"
else
  log "writing $UNIT"
  cat > "$UNIT" <<UNITFILE
[Unit]
Description=inginx OpenAI-compatible inference proxy
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=$PREFIX/inginx
Environment=INGINX_CONFIG=$STATE_DIR/inginx.toml
WorkingDirectory=$STATE_DIR
DynamicUser=yes
StateDirectory=inginx
Restart=on-failure
RestartSec=2

# Hardening
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
ReadWritePaths=$STATE_DIR

[Install]
WantedBy=multi-user.target
UNITFILE
fi

systemctl daemon-reload

if [ "${INGINX_NO_START:-0}" = "1" ]; then
  systemctl enable inginx
  log "unit enabled but not started (INGINX_NO_START=1)"
  exit 0
fi

systemctl enable --now inginx
sleep 1
systemctl is-active --quiet inginx || die "inginx failed to start; see: journalctl -u inginx -n 50"

log "inginx is running. First-run admin setup URL:"
journalctl -u inginx --no-pager -n 200 2>/dev/null | grep -m1 'setup' || \
  warn "no setup URL in the log yet — run: journalctl -u inginx | grep setup"
