#!/usr/bin/env bash
# Development environment: Vite HMR for the Portal, rebuild-and-restart for the
# server, both driven by one command and one Ctrl-C.
#
#   tools/dev.sh                 # Vite on :5173 (proxying /api), server on :8080
#   tools/dev.sh --stt on        # ...with live transcription
#   tools/dev.sh --port 8081     # a second instance, separate from the first
#
# What reloads, and what therefore does not need this script:
#
#   web/src/**            Vite HMR. Editing a component repaints without losing
#                         the page state, and no Rust is involved.
#   crates/**/*.rs        `cargo build`, then SIGTERM the old process and start
#                         the new one. A build failure leaves the *running* server
#                         alone — a syntax error should not take the thing you
#                         were looking at off the air.
#   apple-stt-sidecar/*
#                         not watched. Run `tools/refresh-all.sh --no-node --no-web`
#                         after changing the sidecar.
#
# The runtime directory is the same `var/` a normal run uses, so the Portal shows
# your real segments and database. That also means this script takes the port over
# from a running instance: on exit, `tools/refresh-all.sh --no-node` puts it back.
#
# Build first, swap second. `cargo build` replaces the binary by rename, so the
# process that is already running keeps its own copy and is only signalled once a
# good build exists.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUNTIME_DIR="${STENO_RUNTIME_DIR:-$REPO_ROOT/var}"
RUNTIME_DIR="${RUNTIME_DIR%/}"
CONFIG="$RUNTIME_DIR/server.toml"
PORT="${STENO_HTTP_PORT:-8080}"
STT_MODE=""
CONTROL_MODE=""
NO_WEB=0

usage() {
  local last
  last="$(grep -n '^set -euo pipefail' "${BASH_SOURCE[0]}" | head -1 | cut -d: -f1)"
  sed -n "2,$((last - 2))p" "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
  cat <<'EOF'

Options:
  --port N          server HTTP port (default 8080; Vite proxies to it)
  --stt on|off      set [stt] enabled in the runtime config
  --control on|off  set [control] enabled
  --no-web          backend only
  --reset-config    regenerate the runtime config before starting
  -h, --help        this text
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --port) PORT="$2"; shift 2 ;;
    --stt) STT_MODE="${2:-}"; shift 2 ;;
    --control) CONTROL_MODE="${2:-}"; shift 2 ;;
    --no-web) NO_WEB=1; shift ;;
    --reset-config) RESET_CONFIG=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done
RESET_CONFIG="${RESET_CONFIG:-0}"
case "$STT_MODE" in ""|on|off) ;; *) echo "--stt takes on|off" >&2; exit 2 ;; esac
case "$CONTROL_MODE" in ""|on|off) ;; *) echo "--control takes on|off" >&2; exit 2 ;; esac
say() { printf '\033[1m==>\033[0m %s\n' "$*"; }
dev() { printf '\033[1;36m[dev]\033[0m %s\n' "$*"; }
warn() { printf '\033[33mwarning:\033[0m %s\n' "$*" >&2; }
die() { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }

have() { command -v "$1" >/dev/null 2>&1; }

# Rewrites `enabled` inside `[section]`, leaving the rest of the file alone.
patch_section() {
  local file="$1" section="$2" value="$3"
  awk -v sec="$section" -v v="$value" '
    $0 ~ "^\\[" sec "\\]$" { inside = 1; print; next }
    /^\[/ { inside = 0 }
    inside && $0 ~ "^enabled[[:space:]]*=" { print "enabled = " v; next }
    { print }
  ' "$file" >"$file.dev" && mv "$file.dev" "$file"
}

# Points `[stt] stream_command` at the sidecar.
#
# The key is printed right after the section header and any old copy inside that
# section is dropped, so the file always ends up with exactly one — position
# within a TOML table carries no meaning here.
patch_stt_command() {
  local file="$1" value="$2"
  awk -v v="$value" '
    /^\[stt\]$/ { inside = 1; print; print "stream_command = \"" v "\""; next }
    /^\[/ { inside = 0 }
    inside && $0 ~ "^stream_command[[:space:]]*=" { next }
    { print }
  ' "$file" >"$file.dev" && mv "$file.dev" "$file"
}

listener_pids() { lsof -nP -iTCP:"$PORT" -sTCP:LISTEN -t 2>/dev/null || true; }

SERVER_PID=""
VITE_PID=""
STAMP="$RUNTIME_DIR/.dev-build-stamp"

# --- cleanup ----------------------------------------------------------------
cleanup() {
  local status=$?
  # Idempotent: the EXIT trap fires after an INT/TERM trap has already run.
  trap - INT TERM EXIT
  printf '\n'
  dev "shutting down"
  if [ -n "$VITE_PID" ] && kill -0 "$VITE_PID" 2>/dev/null; then
    kill "$VITE_PID" 2>/dev/null || true
  fi
  stop_server
  dev "done. To put the detached instance back: tools/refresh-all.sh --no-node"
  # Without this the trap returns into the rebuild loop, which then starts
  # waiting for the next file change: the process would survive Ctrl-C while
  # looking shut down.
  exit "$status"
}
trap cleanup INT TERM EXIT

stop_server() {
  [ -n "$SERVER_PID" ] || return 0
  kill -0 "$SERVER_PID" 2>/dev/null || { SERVER_PID=""; return 0; }
  dev "stopping server pid $SERVER_PID"
  kill -TERM "$SERVER_PID" 2>/dev/null || true
  # The graceful path closes the open segment; give it the same 10 s a restart
  # in tools/refresh-all.sh does, then stop waiting.
  for _ in $(seq 1 20); do
    kill -0 "$SERVER_PID" 2>/dev/null || { SERVER_PID=""; return 0; }
    sleep 0.5
  done
  warn "server did not exit in 10 s; SIGKILL"
  kill -9 "$SERVER_PID" 2>/dev/null || true
  SERVER_PID=""
}

# True when a watched file is newer than the last successful build.
changed() {
  [ -n "$(find "$REPO_ROOT/crates" "$REPO_ROOT/Cargo.toml" "$REPO_ROOT/Cargo.lock" \
        -newer "$STAMP" -type f \( -name '*.rs' -o -name 'Cargo.toml' \) 2>/dev/null | head -1)" ]
}

# Sleeps in the *background* and `wait`s for it. A trap cannot run while bash is
# waiting on a foreground child, so `sleep 1` in a loop would delay Ctrl-C by up
# to a second and, worse, swallow it entirely on some shells.
nap() {
  sleep "$1" &
  wait $! 2>/dev/null || true
}

wait_for_change() {
  dev "waiting for a change under crates/"
  while ! changed; do nap 1; done
  dev "change detected"
}

# --- 0. preflight -----------------------------------------------------------
have cargo || die "cargo is not on PATH"
[ "$NO_WEB" = 1 ] || have npm || die "npm is not on PATH (or pass --no-web)"
if ! have steno-stt-sidecar; then
  warn "steno-stt-sidecar is not on PATH; [stt] will fail to start the sidecar"
  warn "install it with: PREFIX=\$HOME/.local ./apple-stt-sidecar/install.sh"
fi
if [ -z "$(listener_pids)" ]; then
  :
else
  say "port $PORT is taken; stopping the instance that holds it"
  kill -TERM $(listener_pids) 2>/dev/null || true
  for _ in $(seq 1 20); do
    [ -z "$(listener_pids)" ] && break
    sleep 0.5
  done
  [ -z "$(listener_pids)" ] || { kill -9 $(listener_pids) 2>/dev/null || true; sleep 1; }
fi

# --- 1. config --------------------------------------------------------------
mkdir -p "$RUNTIME_DIR"
if [ ! -f "$CONFIG" ] || [ "$RESET_CONFIG" = 1 ]; then
  args=(--write-config-only)
  [ "$RESET_CONFIG" = 1 ] && args+=(--reset-config)
  [ "$STT_MODE" = on ] && args+=(--stt)
  [ "$CONTROL_MODE" = on ] && args+=(--control)
  say "generating $CONFIG"
  "$REPO_ROOT/tools/run-server.sh" "${args[@]}"
fi
if [ -n "$STT_MODE" ] && [ "$STT_MODE" = on ]; then
  patch_section "$CONFIG" stt true
elif [ "$STT_MODE" = off ]; then
  patch_section "$CONFIG" stt false
fi
if [ "$STT_MODE" = on ]; then
  patch_stt_command "$CONFIG" "steno-stt-sidecar"
fi
if [ "$CONTROL_MODE" = on ]; then
  patch_section "$CONFIG" control true
elif [ "$CONTROL_MODE" = off ]; then
  patch_section "$CONFIG" control false
fi
# The dev server must serve the API on the port Vite proxies to.
awk -v p="$PORT" '
  /^\[server\]$/ { inside = 1; print; next }
  /^\[/ { inside = 0 }
  inside && /^port[[:space:]]*=/ { print "port = " p; next }
  { print }
' "$CONFIG" >"$CONFIG.dev" && mv "$CONFIG.dev" "$CONFIG"

# --- 2. Portal --------------------------------------------------------------
if [ "$NO_WEB" = 0 ]; then
  [ -d "$REPO_ROOT/web/node_modules" ] || {
    say "installing web dependencies (first run)"
    npm --prefix "$REPO_ROOT/web" install --silent
  }
  say "starting Vite (HMR for web/src)"
  VITE_API_TARGET="http://127.0.0.1:$PORT" \
    npm --prefix "$REPO_ROOT/web" run dev >"$RUNTIME_DIR/vite.log" 2>&1 &
  VITE_PID=$!
  # Vite picks the next free port when 5173 is busy, which would silently give
  # you a second server; wait for the banner instead of assuming.
  for _ in $(seq 1 40); do
    grep -q "Local:" "$RUNTIME_DIR/vite.log" 2>/dev/null && break
    kill -0 "$VITE_PID" 2>/dev/null || die "Vite exited; see $RUNTIME_DIR/vite.log"
    sleep 0.5
  done
  grep -m1 "Local:" "$RUNTIME_DIR/vite.log" | sed 's/^ *//' || true
fi

# --- 3. backend loop --------------------------------------------------------
say "starting the server loop (edit crates/**/*.rs to reload)"
while :; do
  if ! cargo build --manifest-path "$REPO_ROOT/Cargo.toml" -p steno-server; then
    warn "build failed — the server that is running (if any) was left alone"
    wait_for_change
    continue
  fi
  touch "$STAMP"
  stop_server
  STENO_MONITOR_CONFIG="$CONFIG" RUST_LOG="${RUST_LOG:-info,steno_server=debug}" \
    "$REPO_ROOT/target/debug/steno-monitor" >>"$RUNTIME_DIR/server.log" 2>&1 &
  SERVER_PID=$!
  dev "server pid $SERVER_PID · log $RUNTIME_DIR/server.log"
  # Serve until a source file changes or the process exits on its own.
  while kill -0 "$SERVER_PID" 2>/dev/null; do
    if changed; then
      dev "change detected, rebuilding"
      break
    fi
    nap 1
  done
  kill -0 "$SERVER_PID" 2>/dev/null || {
    warn "the server exited by itself; last lines:"
    tail -15 "$RUNTIME_DIR/server.log" >&2 || true
    SERVER_PID=""
    wait_for_change
  }
done
