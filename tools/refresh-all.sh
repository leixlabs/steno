#!/usr/bin/env bash
# Refresh every runtime of this project in one command.
#
#   Portal (web/dist) → server binary → Apple STT sidecar → capture node on the Pi
#   → restart the server → verify through /api/v1/status
#
# It builds first and stops things second, so the outage is only the restart.
# Every step is skippable, and a Pi that cannot be reached is reported without
# aborting the local refresh.
#
#   tools/refresh-all.sh                     # everything, debug build
#   tools/refresh-all.sh --release           # optimised server
#   tools/refresh-all.sh --no-node           # local only (no Pi, no ssh)
#   tools/refresh-all.sh --stt on --control on
#   tools/refresh-all.sh --dry-run           # print the plan, change nothing
#
# Exit status: 0 when the server answers after the restart and every step that
# was asked for succeeded; 1 otherwise (the failing step is named).

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# Project-local by default: this directory holds real recordings and a database,
# so it must not live under $TMPDIR (macOS purges /var/folders entries it thinks
# are unused). `var/` is git-ignored.
_tmp="${TMPDIR:-/tmp}"; _tmp="${_tmp%/}"
RUNTIME_DIR="${STENO_RUNTIME_DIR:-$REPO_ROOT/var}"
RUNTIME_DIR="${RUNTIME_DIR%/}"
# Where the old default kept the data. Adopted once, on the first run after the
# move, because that directory contains nights that exist nowhere else.
LEGACY_DIR="${STENO_LEGACY_RUNTIME_DIR:-$_tmp/steno-server}"
CONFIG="${STENO_MONITOR_CONFIG:-$RUNTIME_DIR/server.toml}"
LOG="$RUNTIME_DIR/server.log"
PIDFILE="$RUNTIME_DIR/server.pid"
STT_PREFIX="${STENO_STT_PREFIX:-}"

PROFILE="debug"
DO_WEB=1
DO_SIDECAR=1
DO_NODE=1
DO_SERVER=1
RESET_CONFIG=0
DRY_RUN=0
FOREGROUND=0
ADOPT=1
STT_MODE=""
CONTROL_MODE=""
PI_ARGS=()
FAILED=()

usage() {
  local last
  last="$(grep -n '^set -euo pipefail' "${BASH_SOURCE[0]}" | head -1 | cut -d: -f1)"
  sed -n "2,$((last - 2))p" "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
  cat <<'EOF'

Options:
  --release             optimised server build (default: debug)
  --no-web              skip the Portal build
  --no-node             skip the Pi deploy (no ssh)
  --no-sidecar          skip rebuilding/installing the Apple STT sidecar
                        (`--no-helper` is still accepted as the old name)
  --no-server           build and deploy, but do not restart the server
  --stt on|off          set [stt] enabled in the runtime config (patched in place)
  --control on|off      set [control] enabled (patched in place; needs loopback)
  --reset-config        regenerate the runtime config from the template
  --pi-host HOST        passed to tools/deploy-node.sh (default: $PI_HOST)
  --pi-user USER        passed to tools/deploy-node.sh (default: $PI_USER)
  --no-adopt            do not move a legacy $TMPDIR/steno-server into place
  --foreground          start the server in this shell instead of detaching
  --dry-run             print every action, change nothing
  -h, --help            this text
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --release) PROFILE="release"; shift ;;
    --no-web) DO_WEB=0; shift ;;
    --no-node) DO_NODE=0; shift ;;
    --no-sidecar|--no-helper) DO_SIDECAR=0; shift ;;
    --no-server) DO_SERVER=0; shift ;;
    --stt) STT_MODE="${2:-}"; shift 2 ;;
    --control) CONTROL_MODE="${2:-}"; shift 2 ;;
    --reset-config) RESET_CONFIG=1; shift ;;
    --pi-host) PI_ARGS+=(--host "$2"); shift 2 ;;
    --pi-user) PI_ARGS+=(--user "$2"); shift 2 ;;
    --foreground) FOREGROUND=1; shift ;;
    --no-adopt) ADOPT=0; shift ;;
    --dry-run) DRY_RUN=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

case "$STT_MODE" in ""|on|off) ;; *) echo "--stt takes on|off, got '$STT_MODE'" >&2; exit 2 ;; esac
case "$CONTROL_MODE" in ""|on|off) ;; *) echo "--control takes on|off, got '$CONTROL_MODE'" >&2; exit 2 ;; esac

say() { printf '\033[1m==>\033[0m %s\n' "$*"; }
step() { printf '\n\033[1;36m[%s]\033[0m %s\n' "$1" "$2"; }
warn() { printf '\033[33mwarning:\033[0m %s\n' "$*" >&2; }
die() { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }

# Runs a command, or prints it under --dry-run.
run() {
  if [ "$DRY_RUN" = 1 ]; then printf '  [dry-run] %s\n' "$*"; else "$@"; fi
}

have() { command -v "$1" >/dev/null 2>&1; }

listener_pids() { lsof -nP -iTCP:"$PORT" -sTCP:LISTEN -t 2>/dev/null || true; }

# Replaces every literal occurrence of `old` with `new`. Literal, not regex: these
# are filesystem paths, and sed would treat a `.` in one as "any character".
rewrite_paths() {
  local file="$1" old="$2" new="$3"
  awk -v o="$old" -v n="$new" '{
    out = ""; rest = $0
    while ((i = index(rest, o)) > 0) {
      out = out substr(rest, 1, i - 1) n
      rest = substr(rest, i + length(o))
    }
    print out rest
  }' "$file" >"$file.rewritten" && mv "$file.rewritten" "$file"
}

# Stops whatever is serving on $PORT. Called from step 4, and early when a legacy
# runtime directory has to be moved (its database must not be open while it
# moves). Idempotent: a second call just reports that nothing is listening.
stop_server() {
  local pids stopped
  if [ -z "$(listener_pids)" ]; then
    say "nothing is listening on $PORT"
    return 0
  fi
  if [ "$DRY_RUN" = 1 ]; then
    printf '  [dry-run] SIGTERM %s\n' "$(listener_pids | tr '\n' ' ' | sed 's/ $//')"
    return 0
  fi
  pids="$(listener_pids)"
  say "SIGTERM $(echo "$pids" | tr '\n' ' ' | sed 's/ $//')"
  kill -TERM $pids 2>/dev/null || true
  stopped=0
  # The graceful path closes the open segment and drains the database writer,
  # which is worth waiting for: SIGKILL leaves a .partial to recover.
  for _ in $(seq 1 40); do
    if [ -z "$(listener_pids)" ]; then stopped=1; break; fi
    sleep 0.5
  done
  if [ "$stopped" = 0 ]; then
    warn "still listening after 20 s; SIGKILL (the open segment stays .partial)"
    kill -9 $(listener_pids) 2>/dev/null || true
    sleep 1
  fi
  # Any sidecar left over from the old worker would keep the recogniser busy and
  # make the new process fail to open its socket. Both names are killed: a
  # process started before the rename is still holding the old bundle.
  pkill -f "steno-stt-sidecar.app/Contents/MacOS/steno-stt-sidecar" 2>/dev/null || true
  pkill -f "snore-stt-apple.app/Contents/MacOS/snore-stt-apple" 2>/dev/null || true
  rm -f "$RUNTIME_DIR"/steno-stt-*.sock 2>/dev/null || true
  say "stopped"
}

# The server's port comes from the config it will actually load, so a run that
# was started on a different port cannot be "restarted" on the wrong one.
port_from_config() {
  [ -f "$CONFIG" ] || { echo "${STENO_HTTP_PORT:-8080}"; return; }
  local port
  port="$(sed -n '/^\[server\]/,/^\[/s/^port[[:space:]]*=[[:space:]]*\([0-9]\+\).*/\1/p' "$CONFIG" | head -1)"
  echo "${port:-${STENO_HTTP_PORT:-8080}}"
}
PORT="$(port_from_config)"

# Rewrites `key = value` inside `[section]`, leaving every other line alone.
# A full regeneration would reset the segment length and the port, which is not
# what "turn transcription on" should mean.
patch_section() {
  local file="$1" section="$2" key="$3" value="$4"
  awk -v sec="$section" -v k="$key" -v v="$value" '
    $0 ~ "^\\[" sec "\\]$" { inside = 1; print; next }
    /^\[/ { inside = 0 }
    inside && $0 ~ "^" k "[[:space:]]*=" { print k " = " v; next }
    { print }
  ' "$file" >"$file.patched" && mv "$file.patched" "$file"
}

section_exists() {
  grep -q "^\[$2\]$" "$1" 2>/dev/null
}

# --- 0. preflight -----------------------------------------------------------
step 0 "preflight"
say "repo:    $REPO_ROOT"
say "config:  $CONFIG"
say "port:    $PORT"
say "profile: $PROFILE"
have cargo || die "cargo is not on PATH"
if [ "$DO_WEB" = 1 ]; then
  have npm || die "npm is not on PATH (or pass --no-web)"
fi
if [ "$DO_NODE" = 1 ]; then
  have ssh || die "ssh is not on PATH (or pass --no-node)"
fi
[ -d "$RUNTIME_DIR" ] || run mkdir -p "$(dirname "$RUNTIME_DIR")"

# --- 1. runtime config ------------------------------------------------------
step 1 "runtime config"
# The runtime directory used to be $TMPDIR/steno-server. Moving it is not a
# cleanup: the recordings and the database there are the only copy, and macOS is
# free to delete that tree. Adopt it once, and only when the new location has no
# config yet (so a populated `var/` is never overwritten).
if [ "$ADOPT" = 1 ] && [ ! -f "$CONFIG" ] && [ -f "$LEGACY_DIR/server.toml" ] \
   && [ "$LEGACY_DIR" != "$RUNTIME_DIR" ]; then
  say "found a legacy runtime at $LEGACY_DIR; moving it to $RUNTIME_DIR"
  # Stop first: the SQLite database is open while the server runs.
  stop_server
  run mkdir -p "$(dirname "$RUNTIME_DIR")"
  # `mv a b` with `b` an existing directory moves `a` *inside* `b`. An empty
  # target is removed so the move is a rename; a populated one is refused,
  # because merging two nights of history is not something a script may decide.
  if [ -d "$RUNTIME_DIR" ]; then
    if [ -z "$(ls -A "$RUNTIME_DIR" 2>/dev/null)" ]; then
      run rmdir "$RUNTIME_DIR"
    else
      die "$RUNTIME_DIR exists and is not empty, but has no server.toml; refusing to merge two runtimes. Move it aside, or pass --no-adopt"
    fi
  fi
  run mv "$LEGACY_DIR" "$RUNTIME_DIR"
fi

# A config written before the move still names the old absolute paths, and the
# server would happily create a *new empty* database there — the pool reads 0
# while every file sits in the new directory. Rewriting is part of the move.
#
# Two forms to match: macOS's TMPDIR ends in a slash, so configs written by the
# old default contain ".../T//steno-server". A single-form check silently misses
# exactly the files this repair exists for.
LEGACY_DIR_ALT="${_tmp}//$(basename "$LEGACY_DIR")"
for old in "$LEGACY_DIR" "$LEGACY_DIR_ALT"; do
  [ -f "$CONFIG" ] || break
  grep -qF "$old" "$CONFIG" 2>/dev/null || continue
  say "config still points at $old; rewriting it to $RUNTIME_DIR"
  run rewrite_paths "$CONFIG" "$old" "$RUNTIME_DIR"
done
if [ -f "$CONFIG" ] && grep -qF "${_tmp}/" "$CONFIG" 2>/dev/null; then
  warn "$CONFIG still mentions ${_tmp}; check recording_dir and database_path by hand"
  FAILED+=("runtime paths")
fi

if [ "$RESET_CONFIG" = 1 ] || [ ! -f "$CONFIG" ]; then
  write_args=(--write-config-only)
  [ "$RESET_CONFIG" = 1 ] && write_args+=(--reset-config)
  [ "$STT_MODE" = on ] && write_args+=(--stt)
  [ "$CONTROL_MODE" = on ] && write_args+=(--control)
  say "regenerating $CONFIG from tools/run-server.sh"
  run "$REPO_ROOT/tools/run-server.sh" "${write_args[@]}"
else
  say "keeping the existing $CONFIG"
  for pair in "stt:$STT_MODE" "control:$CONTROL_MODE"; do
    section="${pair%%:*}"
    mode="${pair#*:}"
    [ -n "$mode" ] || continue
    if section_exists "$CONFIG" "$section"; then
      value=false
      [ "$mode" = on ] && value=true
      say "[$section] enabled = $value"
      if [ "$DRY_RUN" = 1 ]; then
        printf '  [dry-run] patch [%s] enabled = %s in %s\n' "$section" "$value" "$CONFIG"
      else
        patch_section "$CONFIG" "$section" "enabled" "$value"
      fi
    else
      warn "$CONFIG has no [$section] section; pass --reset-config to regenerate it"
      FAILED+=("[$section] config")
    fi
  done
fi

# --- 2. Portal --------------------------------------------------------------
step 2 "Portal (web)"
if [ "$DO_WEB" = 1 ]; then
  if [ ! -d "$REPO_ROOT/web/node_modules" ]; then
    say "installing web dependencies (first run)"
    run npm --prefix "$REPO_ROOT/web" install --silent
  fi
  run npm --prefix "$REPO_ROOT/web" run build
  if [ "$DRY_RUN" != 1 ]; then
    [ -f "$REPO_ROOT/web/dist/index.html" ] || die "web/dist/index.html is missing after the build"
    say "web/dist rebuilt ($(du -sh "$REPO_ROOT/web/dist" | cut -f1))"
  fi
else
  say "skipped (--no-web)"
fi

# --- 3. server binary -------------------------------------------------------
step 3 "server binary"
if [ "$PROFILE" = "release" ]; then
  run cargo build --manifest-path "$REPO_ROOT/Cargo.toml" --release -p steno-server
  BIN="$REPO_ROOT/target/release/steno-monitor"
else
  run cargo build --manifest-path "$REPO_ROOT/Cargo.toml" -p steno-server
  BIN="$REPO_ROOT/target/debug/steno-monitor"
fi
if [ "$DRY_RUN" != 1 ] && [ ! -x "$BIN" ]; then
  die "no server binary at $BIN"
fi

# --- 4. stop the server -----------------------------------------------------
# Stopping before the sidecar and the node are touched means nothing is reading a
# half-replaced binary, and the sidecar is not orphaned holding the socket.
step 4 "stop the server"
if [ "$DO_SERVER" = 0 ]; then
  say "skipped (--no-server)"
else
  stop_server
fi

# --- 5. Apple STT sidecar ---------------------------------------------------
step 5 "Apple STT sidecar (macOS app)"
if [ "$(uname -s)" != "Darwin" ]; then
  say "not macOS; skipped"
elif [ "$DO_SIDECAR" = 1 ]; then
  # Install where it already lives: reinstalling into a different prefix is how
  # a working TCC grant gets silently lost (the grant is per bundle path).
  if [ -z "$STT_PREFIX" ]; then
    if [ -d "/usr/local/lib/steno-stt-sidecar.app" ]; then
      STT_PREFIX="/usr/local"
    else
      STT_PREFIX="$HOME/.local"
    fi
  fi
  say "installing into $STT_PREFIX (build + codesign + wrappers)"
  run env PREFIX="$STT_PREFIX" "$REPO_ROOT/apple-stt-sidecar/install.sh"
  if [ "$DRY_RUN" != 1 ]; then
    # The wrappers live in <prefix>/bin, which is not automatically on the PATH
    # of a process started from a desktop session.
    if ! command -v "$STT_PREFIX/bin/steno-stt-sidecar" >/dev/null 2>&1 \
       && ! command -v steno-stt-sidecar >/dev/null 2>&1; then
      warn "steno-stt-sidecar is not on PATH; add $STT_PREFIX/bin to PATH or the server cannot start the recogniser"
    fi
  fi
else
  say "skipped (--no-sidecar)"
fi

# --- 6. capture node --------------------------------------------------------
step 6 "capture node (Pi)"
if [ "$DO_NODE" = 0 ]; then
  say "skipped (--no-node)"
elif [ "$DRY_RUN" = 1 ]; then
  printf '  [dry-run] %s/deploy-node.sh' "$REPO_ROOT/tools"
  [ "${#PI_ARGS[@]}" -gt 0 ] && printf ' %s' "${PI_ARGS[@]}"
  printf '\n'
else
  # A node that cannot be reached must not stop the local refresh: the server is
  # still worth restarting with the new Portal and sidecar.
  if "$REPO_ROOT/tools/deploy-node.sh" "${PI_ARGS[@]+"${PI_ARGS[@]}"}"; then
    say "node deployed"
  else
    warn "node deploy failed; the local refresh continues"
    FAILED+=("node deploy")
  fi
fi

# --- 7. start the server ----------------------------------------------------
step 7 "start the server"
if [ "$DO_SERVER" = 0 ]; then
  say "skipped (--no-server)"
elif [ "$DRY_RUN" = 1 ]; then
  printf '  [dry-run] STENO_MONITOR_CONFIG=%s %s >>%s\n' "$CONFIG" "$BIN" "$LOG"
else
  [ -f "$CONFIG" ] || die "no config at $CONFIG"
  mkdir -p "$RUNTIME_DIR"
  if [ "$FOREGROUND" = 1 ]; then
    say "running in the foreground (Ctrl-C to stop)"
    exec env STENO_MONITOR_CONFIG="$CONFIG" RUST_LOG="${RUST_LOG:-info}" "$BIN"
  fi
  (
    cd "$REPO_ROOT"
    STENO_MONITOR_CONFIG="$CONFIG" RUST_LOG="${RUST_LOG:-info}" \
      nohup "$BIN" >>"$LOG" 2>&1 &
    echo $! >"$PIDFILE"
  )
  say "pid $(cat "$PIDFILE" 2>/dev/null || echo '?'), log $LOG"
fi

# --- 8. verify --------------------------------------------------------------
step 8 "verify"
if [ "$DO_SERVER" = 0 ] || [ "$DRY_RUN" = 1 ] || [ "$FOREGROUND" = 1 ]; then
  say "skipped"
else
  STATUS_JSON="$RUNTIME_DIR/status.json"
  if have python3; then
    if ! python3 - "$PORT" "$STATUS_JSON" <<'PY'
import json
import sys
import time
import urllib.request

port, path = sys.argv[1], sys.argv[2]
url = f"http://127.0.0.1:{port}/api/v1/status"


def fetch():
    with urllib.request.urlopen(url, timeout=2) as response:
        return json.load(response)


# 1. The server itself: 30 s is enough for a cold debug start.
status = None
for _ in range(60):
    try:
        status = fetch()
        break
    except Exception:
        time.sleep(0.5)
if status is None:
    sys.exit(3)

def settle(key, done, tries):
    """Polls until `done(status)`, so the summary is a verdict, not a snapshot."""
    global status
    if done(status):
        return
    for _ in range(tries):
        time.sleep(0.5)
        try:
            status = fetch()
        except Exception:
            continue
        if done(status):
            return


# 2. A node re-announces itself within a second of the server coming back.
#    Reporting "离线" at t+0 would be a transient dressed up as a finding.
if status["source"]["kind"] == "net":
    settle("node", lambda s: s["source"]["node_connected"], 12)

# 3. The recogniser reconnects on its own timer.
if status["stt"]["enabled"]:
    settle("stt", lambda s: s["stt"]["state"] != "starting", 12)

with open(path, "w") as handle:
    json.dump(status, handle)

service, src, stt, pool = status["service"], status["source"], status["stt"], status["storage"]
pid = (service.get("launch") or {}).get("pid", "-")
print(f"  服务端    {service.get('state')}  v{service.get('version')}  pid {pid}")
print(
    f"  采集端    {'在线' if src.get('node_connected') else '离线'}"
    f"  gated {src.get('gated_frames', 0)} 帧  lost {src.get('packets_lost', 0)}"
)
line = f"  转写      {stt.get('state')}  {stt.get('engine')}  已存 {stt.get('finals_total')} 句"
if stt.get("error"):
    line += f"  错误 {stt['error']}"
print(line)
print(
    f"  存储池    {pool.get('used_bytes')} / {pool.get('max_bytes')} bytes"
    f"  {pool.get('segment_count')} 段"
)
PY
    then
      printf '\n--- last 25 log lines ---\n' >&2
      tail -25 "$LOG" >&2 || true
      die "the server did not answer on port $PORT within 30 s"
    fi
  else
    curl -fsS --max-time 2 "http://127.0.0.1:$PORT/api/v1/status" | head -c 400
    echo
  fi
  portal_code="$(curl -s -o /dev/null -w '%{http_code}' --max-time 3 "http://127.0.0.1:$PORT/" || true)"
  printf '  Portal    HTTP %s\n' "$portal_code"
  [ "$portal_code" = "200" ] || warn "the Portal did not return 200; check server.web_dir"

  if [ "$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["stt"]["state"])' "$STATUS_JSON" 2>/dev/null || echo '')" = "error" ]; then
    warn "the recogniser reported an error; ask the sidecar what it can see:"
    warn "  \"${STT_PREFIX:-$HOME/.local}/lib/steno-stt-sidecar.app/Contents/MacOS/steno-stt-sidecar\" --check"
    FAILED+=("stt error")
  fi
fi

# --- summary ----------------------------------------------------------------
printf '\n'
if [ "${#FAILED[@]}" -eq 0 ]; then
  say "all runtimes refreshed"
  exit 0
fi
printf '\033[33mfinished with problems:\033[0m\n' >&2
for item in "${FAILED[@]}"; do printf '  - %s\n' "$item" >&2; done
exit 1
