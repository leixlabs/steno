#!/usr/bin/env bash
# One-shot deployment of the capture node (crates/node) to a Raspberry Pi.
#
# Cross-compiles the node for armv7 with cargo-zigbuild, pins glibc 2.28, then
# installs the binary, config, service user and systemd unit on the device,
# enables the unit at boot and restarts it. Idempotent: re-running it upgrades in
# place.
#
# Enable-at-boot is the default because a bedside capture node that does not
# come back after a power cut is not a whole-night recorder: it is a device that
# records until the first power failure and then quietly stops. `--no-enable`
# exists for the case where the boot state is deliberately someone else's.
#
# Why zigbuild and not `cross`: cross ships a glibc 2.31 image, which refuses to
# run on Raspberry Pi OS buster (2.28). See docs/operations/2026-10-29-pi-deploy.md §3.1.
#
# Usage: tools/deploy-node.sh [--host pi.local] [options]
#        tools/deploy-node.sh --help

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TARGET="armv7-unknown-linux-gnueabihf"
GLIBC="2.28"
SYSROOT="${ARMHF_SYSROOT:-$HOME/armhf-sysroot}"

# mDNS name, as the device publishes itself on this LAN.
HOST="${PI_HOST:-pi.local}"
SSH_USER="${PI_USER:-pi}"
SERVER_HOST=""
SERVER_PORT=8091
DEVICE_ID=1
DEVICE_NAME=""
ALSA_DEVICE=""
DO_BUILD=1
DO_ENABLE=1
DRY_RUN=0

usage() {
  # Header is prose that grows; derive its end instead of hardcoding a range.
  local last
  last="$(grep -n '^set -euo pipefail' "${BASH_SOURCE[0]}" | head -1 | cut -d: -f1)"
  sed -n "2,$((last - 2))p" "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
  cat <<'EOF'

Options:
  --host HOST          Pi address (default: $PI_HOST or pi.local)
  --user USER          SSH user (default: $PI_USER or pi)
  --server-host HOST   server the node streams to (default: this machine's LAN IP)
  --server-port PORT   server UDP port (default 8091)
  --device-id N        node id the server accepts (default 1)
  --device-name NAME   label shown in /status (default: the Pi's hostname)
  --alsa-device NAME   CPAL device description; omit to keep the Pi's current value
  --no-build           reuse target/<target>/release/steno-node as-is
  --enable             enable at boot (the default; kept for muscle memory)
  --no-enable          leave the boot state alone
  --dry-run            print what would happen, change nothing
  -h, --help           this text
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --host) HOST="$2"; shift 2 ;;
    --user) SSH_USER="$2"; shift 2 ;;
    --server-host) SERVER_HOST="$2"; shift 2 ;;
    --server-port) SERVER_PORT="$2"; shift 2 ;;
    --device-id) DEVICE_ID="$2"; shift 2 ;;
    --device-name) DEVICE_NAME="$2"; shift 2 ;;
    --alsa-device) ALSA_DEVICE="$2"; shift 2 ;;
    --no-build) DO_BUILD=0; shift ;;
    --enable) DO_ENABLE=1; shift ;;
    --no-enable) DO_ENABLE=0; shift ;;
    --dry-run) DRY_RUN=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

say() { printf '\033[1m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[33mwarning:\033[0m %s\n' "$*" >&2; }
die() { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }
run() {
  if [ "$DRY_RUN" = 1 ]; then printf '  [dry-run] %s\n' "$*"; else "$@"; fi
}

# A single remote shell is used for everything, so sudo is asked once and the
# install is atomic from systemd's point of view.
remote() { ssh -o BatchMode=yes -o ConnectTimeout=8 "$SSH_USER@$HOST" "$@"; }

# --- 1. preflight -----------------------------------------------------------
say "checking $SSH_USER@$HOST"
# A missing host key and a missing login key look the same from `ssh`'s exit
# status and are fixed differently, so name which one it is.
if ! SSH_ERROR="$(remote 'true' 2>&1)"; then
  case "$SSH_ERROR" in
    *"Host key verification failed"*)
      die "the host key for $HOST is not in known_hosts. Verify it out of band, then:
  ssh-keyscan -H $HOST >> ~/.ssh/known_hosts" ;;
    *)
      die "cannot ssh to $SSH_USER@$HOST: $SSH_ERROR" ;;
  esac
fi
ARCH="$(remote 'uname -m')"
[ "$ARCH" = "armv7l" ] || die "expected armv7l on the device, got $ARCH"
remote 'sudo -n true' 2>/dev/null || die "$SSH_USER needs passwordless sudo on the device"
[ "$(remote 'systemctl is-system-running' 2>/dev/null || true)" = "offline" ] && warn "systemd reports 'offline'; the unit may not start"

if [ -z "$SERVER_HOST" ]; then
  SERVER_HOST="$(ipconfig getifaddr en0 2>/dev/null || ipconfig getifaddr en1 2>/dev/null || true)"
  [ -n "$SERVER_HOST" ] || die "cannot detect this machine's LAN IP; pass --server-host"
  say "server address for the node: $SERVER_HOST (override with --server-host)"
fi
[ -n "$DEVICE_NAME" ] || DEVICE_NAME="$(remote 'hostname -s')"

# Keep the device selection that already works on this Pi: cpal matches the
# device *description* string, and getting it wrong means "running but silent".
#
# The old `/etc/snore-monitor/node.toml` is read too. A device that has not been
# migrated yet only has that file, and reading just the new path silently
# renders `alsa_device = ""` — which the node accepts (a single USB device is
# then picked automatically), so the deploy "succeeds" while quietly dropping
# the one setting a human chose by hand.
if [ -z "$ALSA_DEVICE" ]; then
  for candidate in /etc/steno-monitor/node.toml /etc/snore-monitor/node.toml; do
    ALSA_DEVICE="$(remote "sudo -n sed -n 's/^alsa_device *= *\"\\(.*\\)\"/\\1/p' $candidate 2>/dev/null" || true)"
    if [ -n "$ALSA_DEVICE" ]; then
      say "keeping existing alsa_device from $candidate: $ALSA_DEVICE"
      break
    fi
  done
  if [ -z "$ALSA_DEVICE" ]; then
    warn "no alsa_device found on the device; the node will only start if the Pi exposes exactly one capture device"
  fi
fi

# --- 2. build ---------------------------------------------------------------
BIN="$REPO_ROOT/target/$TARGET/release/steno-node"
if [ "$DO_BUILD" = 1 ]; then
  command -v cargo-zigbuild >/dev/null || die "cargo-zigbuild is missing: cargo install cargo-zigbuild"
  command -v zig >/dev/null || die "zig is missing: brew install zig"
  [ -d "$SYSROOT/usr/lib/arm-linux-gnueabihf" ] || die "armhf sysroot not found at $SYSROOT (see docs/operations/2026-10-29-pi-deploy.md §3.2)"
  say "cross-building steno-node for $TARGET (glibc $GLIBC)"
  (
    cd "$REPO_ROOT"
    export PKG_CONFIG_PATH="$SYSROOT/usr/lib/arm-linux-gnueabihf/pkgconfig"
    export PKG_CONFIG_ALLOW_CROSS=1
    export RUSTFLAGS="-L native=$SYSROOT/usr/lib/arm-linux-gnueabihf"
    cargo zigbuild --release -p steno-node --target "$TARGET.$GLIBC"
  )
fi
[ -f "$BIN" ] || die "no binary at $BIN; run without --no-build"

# Deploy-time sanity: wrong architecture or a too-new glibc both fail only after
# the service starts, and both look like "the node is broken" on the Pi.
file "$BIN" | grep -q 'ELF 32-bit.*ARM' || die "$BIN is not an ARM 32-bit ELF"
NEWEST_GLIBC="$(strings "$BIN" | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1 | sed 's/GLIBC_//')"
if [ -n "$NEWEST_GLIBC" ]; then
  HIGHER="$(printf '%s\n%s\n' "$GLIBC" "$NEWEST_GLIBC" | sort -V | tail -1)"
  [ "$HIGHER" = "$GLIBC" ] || die "binary needs GLIBC_$NEWEST_GLIBC but the Pi has $GLIBC"
fi
say "binary: $BIN ($(du -h "$BIN" | cut -f1), newest glibc ${NEWEST_GLIBC:-none})"

# --- 3. render the node config ---------------------------------------------
# `[archive]` and `[gate]` are this device's own decisions (where the local copy
# of the night lives, and whether the uplink may gate), and they carry comments
# that took a human to write. Re-rendering the file must not silently drop them:
# a deploy that quietly turned the archive off would also silently unsafeguard a
# night's audio. Everything from the first of those two tables to EOF is kept
# verbatim and appended after the rendered core.
PRESERVED="$(remote "sudo -n sed -n '/^\[\(archive\|gate\)\]/,\$p' /etc/steno-monitor/node.toml 2>/dev/null" || true)"
if [ -z "$PRESERVED" ]; then
  # Same reason as `alsa_device`: a not-yet-migrated device keeps these sections
  # in the old file, and losing them would silently turn the archive off.
  PRESERVED="$(remote "sudo -n sed -n '/^\[\(archive\|gate\)\]/,\$p' /etc/snore-monitor/node.toml 2>/dev/null" || true)"
fi
if [ -n "$PRESERVED" ]; then
  say "keeping the device's existing [archive]/[gate] sections"
fi

TMPDIR_LOCAL="$(mktemp -d)"
trap 'rm -rf "$TMPDIR_LOCAL"' EXIT
NODE_TOML="$TMPDIR_LOCAL/node.toml"
cat >"$NODE_TOML" <<EOF
# Rendered by tools/deploy-node.sh on $(date -u '+%Y-%m-%dT%H:%M:%SZ').
# This device only captures: recording, detection, storage and the Portal all
# live on the server. See docs/decisions/2026-10-29-split-deployment.md.

[node]
device_name = "$DEVICE_NAME"
server_host = "$SERVER_HOST"
server_port = $SERVER_PORT
device_id = $DEVICE_ID
hello_interval_ms = 1000

[audio]
alsa_device = "$ALSA_DEVICE"
device_rate_hz = 16000
device_channels = 1
mono_channel_index = 0
period_ms = 20
buffer_ms = 400
capture_ring_ms = 500
shutdown_timeout_seconds = 10
EOF

if [ -n "$PRESERVED" ]; then
  printf '\n%s\n' "$PRESERVED" >>"$NODE_TOML"
fi

UNIT="$REPO_ROOT/deployment/systemd/steno-node.service"

# --- 4. transfer ------------------------------------------------------------
say "copying to $HOST:/tmp/steno-deploy"
REMOTE_BIN="/tmp/steno-deploy/steno-node"
REMOTE_TOML="/tmp/steno-deploy/node.toml"
REMOTE_UNIT="/tmp/steno-deploy/steno-node.service"
if [ "$DRY_RUN" = 1 ]; then
  printf '  [dry-run] scp %s %s %s -> %s:/tmp/steno-deploy/\n' "$BIN" "$NODE_TOML" "$UNIT" "$HOST"
else
  remote 'mkdir -p /tmp/steno-deploy'
  scp -q "$BIN" "$SSH_USER@$HOST:$REMOTE_BIN"
  scp -q "$NODE_TOML" "$SSH_USER@$HOST:$REMOTE_TOML"
  scp -q "$UNIT" "$SSH_USER@$HOST:$REMOTE_UNIT"
fi

# --- 5. install on the device ----------------------------------------------
# The account must be in `audio`: without it ALSA hides every real device and
# the node captures from the `null` plugin — "running but recording nothing".
say "installing on $HOST"
# Quoted heredoc: the comment backticks below are prose, not commands. The two
# paths that need expanding are passed through the environment instead.
INSTALL_SCRIPT=$(cat <<'INSTALL_EOF'
set -e
# useradd creates a same-named group by default, so creating the group first
# makes the useradd below fail with "group steno-node exists" — and `set -e`
# then aborts the install *before* the binary is copied, leaving the old node
# running and the deploy looking like it worked. Create the group only when the
# user is absent, and let useradd own the primary group.
if ! id -u steno-node >/dev/null 2>&1; then
  useradd --system --user-group --home /var/lib/steno-node --shell /usr/sbin/nologin steno-node
fi
# The account must be in audio, otherwise ALSA hides every real device and
# the node captures from the null plugin — "running but recording nothing".
usermod -aG audio steno-node
# Prove it took: a group that was added but never materialised is the same
# failure one step later, and it is invisible from the node's own logs.
id -nG steno-node | tr ' ' '\n' | grep -qx audio \
  || { echo "steno-node is not in the audio group after usermod" >&2; exit 1; }
install -d -m 755 /etc/steno-monitor
install -d -o steno-node -g steno-node -m 750 /var/lib/steno-node
install -Dm755 "$REMOTE_BIN" /usr/local/bin/steno-node
install -m 640 -o root -g steno-node "$REMOTE_TOML" /etc/steno-monitor/node.toml
install -Dm644 "$REMOTE_UNIT" /etc/systemd/system/steno-node.service
rm -rf /tmp/steno-deploy
systemctl daemon-reload
INSTALL_EOF
)
if [ "$DRY_RUN" = 1 ]; then
  printf '  [dry-run] remote install (user, audio group, binary, config, unit)\n'
else
  printf '%s\n' "$INSTALL_SCRIPT" \
    | remote "sudo -n REMOTE_BIN='$REMOTE_BIN' REMOTE_TOML='$REMOTE_TOML' REMOTE_UNIT='$REMOTE_UNIT' bash -s"
fi

# --- 6. (re)start and verify ------------------------------------------------
# `enable --now` is not enough here: `--now` only *starts* a unit, so a node that
# was already running would keep executing the binary this deploy just replaced.
# Enable and restart are two separate promises — persist across a power cut, and
# run the build that was just installed.
say "restarting steno-node"
if [ "$DO_ENABLE" = 1 ]; then
  # Enable first, and fail the deploy if it does not stick: a node that comes
  # back from a power cut unstarted is the failure this default exists to avoid.
  run remote 'sudo -n systemctl enable steno-node' \
    || die "could not enable steno-node at boot"
fi
run remote 'sudo -n systemctl restart steno-node'

if [ "$DRY_RUN" = 1 ]; then
  say "dry run complete; nothing changed"
  exit 0
fi

sleep 2
STATE="$(remote 'systemctl is-active steno-node' || true)"
if [ "$STATE" != "active" ]; then
  printf '\n--- last log lines ---\n' >&2
  remote 'sudo -n journalctl -u steno-node -n 30 --no-pager' >&2 || true
  die "steno-node is $STATE"
fi

# `is-active` says nothing about the next boot, which is exactly the state the
# last Pi deployment was left in: running tonight, absent after the first power
# cut. Read the unit file state back instead of trusting the enable that ran.
if [ "$DO_ENABLE" = 1 ]; then
  BOOT_STATE="$(remote 'systemctl is-enabled steno-node' || true)"
  case "$BOOT_STATE" in
    enabled|enabled-runtime|static) say "enabled at boot: $BOOT_STATE" ;;
    *) die "steno-node is running but not enabled at boot (is-enabled: $BOOT_STATE)" ;;
  esac
fi

ALSA_MSG="$(remote 'sudo -n journalctl -u steno-node -n 200 --no-pager -o cat | grep -iE "capture device negotiated|no usable capture|MultipleDevices|DeviceNotFound|capture running" | tail -2' || true)"
say "steno-node is active on $HOST"
[ -n "$ALSA_MSG" ] && printf '%s\n' "$ALSA_MSG"

# `is-active` only proves the process is running. The failure this check exists
# for is the one that looks identical from every other angle: the unit is active,
# the server receives packets, the WAVs are written, and every sample is zero
# because the capsule is muted, the mic is unplugged at the capsule, or the
# account lost the `audio` group. Measure the audio itself, at the source.
#
# The device is single-subdevice, so the node has to be stopped for the probe:
# `arecord` cannot open a stream another process already holds.
say "measuring the capture level on $HOST"
# The Python is base64-encoded rather than piped as a heredoc: `bash -s` already
# reads the script from stdin, so a nested heredoc would compete for it.
MEASURE_PY=$(cat <<'PY'
import math, wave
with wave.open("/tmp/steno-probe.wav") as w:
    raw = w.readframes(w.getnframes())
s = [int.from_bytes(raw[i:i+2], "little", signed=True) for i in range(0, len(raw), 2)]
peak = max((abs(v) for v in s), default=0)
rms = math.sqrt(sum(v * v for v in s) / len(s)) if s else 0
print(round(20 * math.log10(peak / 32768), 1) if peak else -999,
      round(20 * math.log10(rms / 32768), 1) if rms else -999)
PY
)
MEASURE_B64="$(printf '%s' "$MEASURE_PY" | base64 | tr -d '\n')"
remote 'sudo -n systemctl stop steno-node' >/dev/null 2>&1 || true
LEVEL="$(remote "sudo -n bash -c 'arecord -q -D plughw:1,0 -f S16_LE -r 16000 -c 1 -d 3 /tmp/steno-probe.wav 2>/dev/null; echo $MEASURE_B64 | base64 -d | python3; rm -f /tmp/steno-probe.wav'" 2>/dev/null || true)"
remote 'sudo -n systemctl start steno-node' >/dev/null 2>&1 || true

if [ -z "$LEVEL" ]; then
  warn "could not measure the capture level; verify by hand before trusting this deploy"
else
  PEAK_DBFS="${LEVEL%% *}"
  say "capture level on the device: peak ${PEAK_DBFS} dBFS"
  # -60 dBFS sits below anything a working microphone produces in a quiet room
  # (a muted capsule reads around -78) and above digital silence, so it separates
  # "the audio path is broken" from "the room is simply quiet".
  if awk -v p="$PEAK_DBFS" 'BEGIN { exit !(p + 0 < -60) }'; then
    warn "the microphone produces no usable signal (peak ${PEAK_DBFS} dBFS)"
    warn "check on the device: the capsule's mute switch, the mic seated in the"
    warn "capsule, and \`sudo amixer -c 1 contents | grep -A2 'Mic Capture'\`."
    warn "recording will run and transcription will stay empty until this is fixed"
  fi
fi

# Best effort: if the server is already listening on this machine, show whether
# the node has attached. A server that is down is not an error here.
if curl -fsS --max-time 2 "http://127.0.0.1:8080/api/v1/status" >/dev/null 2>&1; then
  curl -fsS "http://127.0.0.1:8080/api/v1/status" \
    | sed -n 's/.*"source":{\([^}]*\)}.*/  server sees source: \1/p' || true
  # The end-to-end check: what the *server* measures, not what the node sent.
  # A silent capsule and a broken uplink look the same in the packet counters,
  # and only the level tells them apart.
  curl -fsS "http://127.0.0.1:8080/api/v1/status" \
    | sed -n 's/.*"capture":{[^}]*"level_peak_dbfs":\([^,]*\),"level_rms_dbfs":\([^,]*\).*/  server measures: peak \1 dBFS, rms \2 dBFS/p' || true
else
  # True in both cases that matter: a standalone deploy with no server yet, and
  # `refresh-all.sh`, which stops the server before it deploys the node.
  say "no server on 127.0.0.1:8080 yet; the node will attach when one starts"
fi
