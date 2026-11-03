#!/usr/bin/env bash
# Development supervisor: every process in the pipeline, one console, one Ctrl-C.
#
#   node ──UDP──► server ──socket──► sidecar (classification + recognition)
#                    │
#                    └──► web (Vite HMR, reads /api/v1/status)
#
#   tools/dev.sh                 # web + server + sidecar, watching all three
#   tools/dev.sh --with-node     # also run a local capture node (real UDP path)
#   tools/dev.sh --only server   # one role; --only server,node for two
#   tools/dev.sh --stt off       # run with transcription disabled
#   tools/dev.sh status          # one-shot panel, from any shell
#
#   tools/dev.sh --backend audiocpp
#                                # recognise with audio.cpp's Qwen3-ASR instead of
#                                # Apple. The first run has to be told where the
#                                # weights are; every run after it is the line
#                                # above, because the path is remembered:
#                                #   tools/dev.sh --backend audiocpp --model ~/models/qwen3-asr-0.6b-q8_0.gguf
#                                # --chunk N overrides the 5 s window.
#   tools/dev.sh reload sidecar  # rebuild it; the server re-launches it
#   tools/dev.sh stop web|start web|restart server|logs server|schema|quit
#
# What reloads on its own, and what needs a command:
#
#   crates/server/**      watched: cargo build, then SIGTERM the old process and
#                         start the new one. A failed build leaves the running
#                         server alone — a syntax error must not take the page you
#                         were looking at off the air.
#   apple-stt-sidecar/**  watched: rebuild the bundle and kill the running
#                         sidecar; the server's worker reconnects within ~6 s and
#                         starts the new build. No install step involved.
#   crates/node/**        watched when the node role is on.
#   web/src/**            Vite HMR. No reload needed; no Rust involved.
#
# With `--backend audiocpp` the recogniser is a second [[bin]] of the server crate,
# so its source is crates/server/**: a server reload rebuilds and restarts it, and
# the Swift bundle is never built.
#
# The console panel (tools/dev-panel.py) polls /api/v1/status once a second and
# tails the server log, so the pipeline state — capture, recording, detector, stt,
# the phrase being formed — is visible without opening the browser. When stdout
# is not a terminal it only prints lifecycle lines, so `dev.sh > log` stays
# readable.
#
# The runtime directory is the same `var/` a normal run uses, so the Portal shows
# your real segments and database. That also means this script takes the HTTP port
# over from a running instance: on exit, `tools/refresh-all.sh --no-node` puts it
# back.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUNTIME_DIR="${STENO_RUNTIME_DIR:-$REPO_ROOT/var}"
RUNTIME_DIR="${RUNTIME_DIR%/}"
CONFIG="$RUNTIME_DIR/server.toml"
DEV_DIR="$RUNTIME_DIR/dev"
STATE_DIR="$DEV_DIR/state"
CMD_FILE="$DEV_DIR/cmd"
FLOW_FILE="$DEV_DIR/flow"
FLOW_OFFSET="$DEV_DIR/flow.offset"
FLOW_SIDECAR_OFFSET="$DEV_DIR/flow.sidecar.offset"
STATUS_JSON="$DEV_DIR/status.json"
PANEL="$REPO_ROOT/tools/dev-panel.py"
PORT="${STENO_HTTP_PORT:-8080}"
HTTP_BASE="http://127.0.0.1:$PORT"
CAPTURE_PORT="${STENO_CAPTURE_PORT:-8091}"

SIDECAR_APP="$REPO_ROOT/apple-stt-sidecar/build/steno-stt-sidecar.app"
SIDECAR_BIN="$SIDECAR_APP/Contents/MacOS/steno-stt-sidecar"
SIDECAR_WRAPPER="$DEV_DIR/steno-stt-sidecar-dev"
SIDECAR_LOG="$DEV_DIR/sidecar.log"

# Which recogniser the dev sidecar runs. Empty means the sidecar's own default
# (Apple, `SpeechAnalyzer`); `--backend audiocpp` swaps in audio.cpp's Qwen3-ASR,
# which needs a GGUF package. The path is asked for once (`--model`) and then
# remembered, because a recogniser without weights starts, reports `ready` and
# hears nothing all night — a failure that looks exactly like a quiet bedroom.
#
# The flags are the interface; the `STENO_STT_*` variables still work as the
# fallback, so an exported environment or an existing script keeps behaving.
MODEL_FILE="$RUNTIME_DIR/stt-model.conf"
STT_BACKEND="${STENO_STT_BACKEND:-}"
STT_MODEL="${STENO_STT_MODEL:-}"
STT_CHUNK="${STENO_STT_CHUNK:-}"
MODEL_GIVEN=0

# The audio.cpp recogniser, built by the same `cargo build` as the server (it is a
# second [[bin]] of the same crate), so watching crates/server rebuilds it too.
AUDIOCPP_SIDECAR_BIN="$REPO_ROOT/target/debug/steno-stt-sidecar-audiocpp"

# Pids matching a command-line pattern, one per line, and **always succeeding**.
#
# `pgrep | head -1` is the trap here: with `set -o pipefail` a `pgrep` that matches
# nothing returns 1, `head` propagates it, and the assignment — inside `$(...)` at
# the top level of a `set -e` script — aborts the whole supervisor. That is how
# `dev.sh stop sidecar` used to shut the *server* down too: it looked for a sidecar
# that was already gone, the lookup failed, and the EXIT trap ran cleanup.
pids_matching() {
  pgrep -f "$1" 2>/dev/null || true
}

# Size of a log, 0 when it does not exist yet. `wc -c <` on a missing file both
# fails and prints nothing, which under `set -e` aborts the caller mid-startup.
log_bytes() {
  if [ -f "$1" ]; then
    wc -c <"$1" 2>/dev/null | tr -d ' ' || echo 0
  else
    echo 0
  fi
}

# Newest of the given files, or empty. `stat -f` because this is macOS.
newest_file() {
  local newest="" best=0 candidate mtime
  for candidate in "$@"; do
    [ -f "$candidate" ] || continue
    mtime="$(stat -f %m "$candidate" 2>/dev/null || echo 0)"
    if [ "$mtime" -gt "$best" ]; then
      best="$mtime"
      newest="$candidate"
    fi
  done
  printf '%s' "$newest"
}

# --- stopping ---------------------------------------------------------------
#
# `dev.sh stop` has to mean "nothing of this pipeline is running any more", which
# is more than the supervisor can see: a server started by `refresh-all.sh` holds
# the same port and the same database but no supervisor's variable points at it.
# So the pids are found from evidence — the state files, the listening socket, the
# sidecar's socket name — and anything that cannot be attributed is left alone.

# Server pids that belong to this runtime directory or this checkout.
owned_server_pids() {
  local pid candidate command_line
  pid="$(state_pid server)"
  if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
    printf '%s\n' "$pid"
  fi
  for candidate in $(listener_pids); do
    [ "$candidate" = "$pid" ] && continue
    command_line="$(ps -o command= -p "$candidate" 2>/dev/null || true)"
    case "$command_line" in
      *"$REPO_ROOT/target/"*|*"$RUNTIME_DIR"*) printf '%s\n' "$candidate" ;;
    esac
  done
}

# A sidecar whose server died keeps its socket path in argv and blocks in accept().
# It is harmless until it holds a stale socket, but it is also invisible to the
# supervisor, so it is swept up here: this checkout's binary, and a socket file
# that no longer exists.
STOPPED=0

stop_orphan_sidecars() {
  local candidate command_line socket
  for candidate in $(pids_matching "steno-stt-sidecar"); do
    command_line="$(ps -o command= -p "$candidate" 2>/dev/null || true)"
    case "$command_line" in
      "$REPO_ROOT"/*) ;;
      *) continue ;;
    esac
    socket="$(printf '%s' "$command_line" | sed -n 's/.*--socket \([^ ]*\).*/\1/p')"
    [ -n "$socket" ] || continue
    [ -S "$socket" ] && continue
    dev "stopping orphan sidecar $candidate (its socket $socket is gone)"
    stop_pid "$candidate" 3
    STOPPED=$((STOPPED + 1))
  done
}

# `prepare_config` points `[stt] stream_command` at the generated wrapper in
# `$DEV_DIR`. Leaving that in place after the dev run ends is a trap: the wrapper
# lives in the directory `stop` deletes, so the next `refresh-all.sh` (or any plain
# `run-server-mac.sh`) starts a server that cannot launch a recogniser at all —
# "cannot launch var/dev/steno-stt-sidecar-dev: No such file or directory".
restore_stt_command() {
  [ -f "$CONFIG" ] || return 0
  local current
  current="$(sed -n 's/^stream_command *= *"\(.*\)"$/\1/p' "$CONFIG" 2>/dev/null | head -1)"
  # The wrapper path may have been written relative to the runtime directory
  # (`var/dev/steno-stt-sidecar-dev`), so match on the name as well as the exact
  # absolute path.
  case "$current" in
    "$SIDECAR_WRAPPER"|*"steno-stt-sidecar-dev"*|"$REPO_ROOT"/*)
      if [ -x "$HOME/.local/bin/steno-stt-sidecar" ] \
         || command -v steno-stt-sidecar >/dev/null 2>&1; then
        patch_stt_command "$CONFIG" "steno-stt-sidecar"
        dev "restored [stt] stream_command to steno-stt-sidecar"
      else
        dev "note: [stt] stream_command still points at the dev wrapper, and no installed sidecar is on PATH"
      fi
      ;;
  esac
}

# Stops what this checkout is running. `role` narrows it; the default is all.
# Returns the number of processes it stopped, so the caller can say "nothing was
# running" instead of printing a cheerful empty summary.
stop_everything() { # [all|server|sidecar|node|web]
  local want="${1:-all}" role pid pids servers supervisors
  supervisors="$(cat "$DEV_DIR/supervisor.pid" 2>/dev/null || true)"

  # The supervisor first, when it is the one holding the children: its cleanup is
  # the documented shutdown order (graceful segment close, then the sidecar) and it
  # prints what it does. What follows is the sweep for what it cannot see.
  if [ "$want" = all ] && [ -n "$supervisors" ] && kill -0 "$supervisors" 2>/dev/null; then
    dev "stopping supervisor $supervisors"
    stop_pid "$supervisors" 15
    STOPPED=$((STOPPED + 1))
  fi

  servers="$(owned_server_pids | sort -u)"
  if [ "$want" = all ] || [ "$want" = sidecar ]; then
    for pid in $servers; do
      pids="$(pids_matching "steno-stt-$pid.sock" | tr '\n' ' ')"
      if [ -n "${pids// /}" ]; then
        dev "stopping sidecar $(echo "$pids" | sed 's/ *$//') (for server $pid)"
        # shellcheck disable=SC2086
        kill -TERM $pids 2>/dev/null || true
        STOPPED=$((STOPPED + 1))
      fi
    done
    stop_orphan_sidecars
    # Whatever was left is gone now; the state file must say so, or the panel keeps
    # showing a sidecar whose pid died with its server.
    state_set sidecar down - "stopped"
    # The sidecar closes its own session; give it a moment before the server goes,
    # or the server logs a truncated stream as a failure.
    if [ "$STOPPED" -gt 0 ]; then sleep 1; fi
  fi

  if [ "$want" = all ] || [ "$want" = server ]; then
    for pid in $servers; do
      dev "stopping server $pid"
      stop_pid "$pid" 10
      STOPPED=$((STOPPED + 1))
    done
    # A server that is gone but left its pid file behind would make the panel
    # claim it is up until the next supervisor starts.
    rm -f "$RUNTIME_DIR/server.pid" 2>/dev/null || true
    state_set server down - "stopped"
  fi

  if [ "$want" = all ] || [ "$want" = node ] || [ "$want" = web ]; then
    for role in node web; do
      [ "$want" = all ] || [ "$want" = "$role" ] || continue
      pid="$(state_pid "$role")"
      if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
        dev "stopping $role $pid"
        stop_pid "$pid" 3
        STOPPED=$((STOPPED + 1))
      fi
      state_set "$role" down - "stopped"
    done
  fi

  if [ "$want" = all ]; then
    restore_stt_command
    rm -rf "$DEV_DIR" 2>/dev/null || true
  fi
  return 0
}

# Where each role's log is. The server's is not in dev/ like the others: it is the
# runtime directory's own log, the one a non-dev run also writes.
log_path_for() { # role
  case "$1" in
    server) printf '%s' "$RUNTIME_DIR/server.log" ;;
    sidecar) sidecar_log_path ;;
    web|node) printf '%s' "$DEV_DIR/$1.log" ;;
    *) printf '' ;;
  esac
}

supervisor_alive() {
  local pid
  pid="$(cat "$DEV_DIR/supervisor.pid" 2>/dev/null)"
  [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null
}

# Which process is serving this panel: the supervisor knows, a client has to look.
server_pid_for_log() {
  if [ -n "${SERVER_PID:-}" ]; then
    printf '%s' "$SERVER_PID"
    return 0
  fi
  lsof -nP -iTCP:"$PORT" -sTCP:LISTEN -t 2>/dev/null | head -1
}

# Where the sidecar's log actually is.
#
# This is not obvious: `log()` writes to $STENO_STT_LOG (default
# /tmp/steno-stt-sidecar.log) *and* to stderr, and the dev wrapper redirects that
# stderr into $DEV_DIR/sidecar.log — so two files hold the same lines. When the
# sidecar was started by an installed wrapper, only the /tmp one exists.
#
# "Newest file wins" is wrong: any stray `steno-stt-sidecar --help` (which logs its
# usage through the same `log()`) makes the /tmp file momentarily newer, and the
# panel would jump to another process's log mid-session. So the live sidecar's pid
# decides — the file that carries lines for *this* server's sidecar is the one to
# read — and only when no sidecar is running does it fall back to the newest.
sidecar_log_path() {
  local candidates=("$SIDECAR_LOG" "${STENO_STT_LOG:-/tmp/steno-stt-sidecar.log}")
  local server pid candidate
  # This script wrote the wrapper it launches, so its stderr redirect is the file
  # before the sidecar has logged a single line — no guessing, no brief switch to
  # /tmp while the first tick waits for a pid to appear in the file.
  # Even before the file exists: the pump handles a missing log, and answering
  # "nothing" here would send the first tick to /tmp and then back, skipping
  # whatever the sidecar wrote in between. Only while this script's supervisor is
  # actually the one running the pipeline — from another shell looking at a
  # detached instance, the wrapper is not ours and /tmp is the real file.
  if [ "$SIDECAR_MODE" = build ] && supervisor_alive; then
    printf '%s' "$SIDECAR_LOG"
    return 0
  fi
  server="$(server_pid_for_log)"
  pid=""
  if [ -n "$server" ]; then
    pid="$(pids_matching "steno-stt-$server.sock" | head -1)"
  fi
  if [ -n "$pid" ]; then
    for candidate in "${candidates[@]}"; do
      [ -f "$candidate" ] || continue
      if tail -n 60 "$candidate" 2>/dev/null | grep -q "steno-stt-sidecar\[$pid\]"; then
        printf '%s' "$candidate"
        return 0
      fi
    done
  fi
  newest_file "${candidates[@]}"
}

# Roles: 1 = this run manages it.
WANT_WEB=1
WANT_SERVER=1
WANT_SIDECAR=1
WANT_NODE=0
WATCH=1
STT_MODE=""
CONTROL_MODE=""
SIDECAR_MODE="build"   # build | installed | off
RESET_CONFIG=0
ONLY=""

say() { printf '\033[1m==>\033[0m %s\n' "$*"; }
dev() { printf '\033[1;36m[dev]\033[0m %s\n' "$*"; }
warn() { printf '\033[33mwarning:\033[0m %s\n' "$*" >&2; }
die() { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

# `--model` may be typed as `~/models/…`, and the shell never expands a `~` that
# arrives inside a variable. Doing it here keeps the remembered path absolute,
# which matters because the wrapper is written and read from another directory.
expand_home() {
  case "$1" in
    "~") printf '%s\n' "$HOME" ;;
    "~/"*) printf '%s/%s\n' "$HOME" "${1#\~/}" ;;
    *) printf '%s\n' "$1" ;;
  esac
}

# The one `.gguf` a directory stands for, or a refusal. Mirrors
# `audiocpp::config::resolve_model` so a bad path fails here — where a human is
# reading — instead of after the sidecar has reported `ready`.
resolve_model_path() { # path -> prints the file, or dies
  local path="$1" found count
  if [ -f "$path" ]; then
    case "$path" in
      *.gguf) printf '%s\n' "$path" ;;
      *) die "--model $path is a file but not a .gguf" ;;
    esac
    return
  fi
  [ -d "$path" ] || die "--model $path is neither a .gguf file nor a directory"
  found="$(find "$path" -maxdepth 1 -name '*.gguf' -type f | sort)"
  count="$(printf '%s\n' "$found" | grep -c . || true)"
  case "$count" in
    1) printf '%s\n' "$found" ;;
    0) die "--model $path holds no .gguf file" ;;
    *) die "--model $path holds $count .gguf files — pass one of them:
$(printf '%s\n' "$found" | sed 's/^/  /')" ;;
  esac
}

# Remembers where the weights are, so `--backend audiocpp` alone works next time.
# Not sourced: this file is state, not a script, and reading one line cannot run
# anything. `%q` quoting is stripped for the same reason it is used.
remembered_model() {
  [ -f "$MODEL_FILE" ] || return 0
  sed -n 's/^STT_MODEL=//p' "$MODEL_FILE" | tail -1 | tr -d "'"
}

remember_model() { # path
  mkdir -p "$RUNTIME_DIR"
  {
    printf '# Written by tools/dev.sh --model. Read when --backend audiocpp runs\n'
    printf '# without a model of its own; delete it to make dev.sh ask again.\n'
    printf 'STT_MODEL=%q\n' "$1"
  } >"$MODEL_FILE"
}

# The package `install.sh` recorded for the installed audiocpp wrapper, if any.
# That file exists precisely because "which GGUF is on this machine" outlives a
# shell, so a deployment that already answered the question does not have to
# answer it again here. Read, never rewritten.
installed_audiocpp_model() {
  local conf
  for conf in "${STENO_STT_PREFIX:-$HOME/.local}/lib/steno-stt-sidecar-audiocpp.conf" \
              "/usr/local/lib/steno-stt-sidecar-audiocpp.conf"; do
    [ -f "$conf" ] || continue
    # `install.sh` quotes the value; strip the quotes rather than match them, so a
    # hand-edited line (quoted or not) reads the same as a generated one.
    sed -n 's/^AUDIOCPP_MODEL=//p' "$conf" | tail -1 | tr -d "'"
    return 0
  done
}

usage() {
  local last
  last="$(grep -n '^set -euo pipefail' "${BASH_SOURCE[0]}" | head -1 | cut -d: -f1)"
  sed -n "2,$((last - 2))p" "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
  cat <<'EOF'

Options:
  --port N            server HTTP port (default 8080; Vite proxies to it)
  --only a,b          run only these roles (web,server,sidecar,node)
  --with-node         also run a local capture node over UDP
  --backend NAME      which recogniser the dev sidecar runs: apple (default) or
                      audiocpp. audiocpp needs --model once, then remembers it
  --model PATH        the .gguf for audiocpp (implies --backend audiocpp)
  --chunk N           audiocpp window length in seconds (default 5, 1..30)
  --stt on|off        set [stt] enabled in the runtime config
  --control on|off    set [control] enabled
  --sidecar build|installed|off
                      where stream_command points (default: the build in this repo)
  --no-web            backend only
  --no-sidecar        do not manage the sidecar (e.g. testing an installed one)
  --no-watch          no rebuild-on-change; reload by command instead
  --reset-config      regenerate the runtime config before starting
  -h, --help          this text

Commands:
  status | start <role> | stop <role> | restart <role> | reload <role>
  stop                stop everything this checkout is running, supervisor included
  logs <role> [n] [-f] | schema | quit
                      (role: server, sidecar, web, node; -f follows)
EOF
}

# --- argument parsing -------------------------------------------------------

COMMANDS=" status start stop restart reload logs schema quit "
is_command() { [ "${COMMANDS#* $1 }" != "$COMMANDS" ]; }

if [ $# -gt 0 ] && is_command "$1"; then
  COMMAND="$1"
  shift
  ARG1="${1:-}"
  ARG2="${2:-}"
else
  COMMAND=""
  ARG1=""
  ARG2=""
  while [ $# -gt 0 ]; do
    case "$1" in
      --port) PORT="$2"; HTTP_BASE="http://127.0.0.1:$PORT"; shift 2 ;;
      --only) ONLY="$2"; shift 2 ;;
      --with-node) WANT_NODE=1; shift ;;
      --backend) STT_BACKEND="${2:-}"; shift 2 ;;
      --model) STT_MODEL="${2:-}"; MODEL_GIVEN=1; shift 2 ;;
      --chunk) STT_CHUNK="${2:-}"; shift 2 ;;
      --stt) STT_MODE="${2:-}"; shift 2 ;;
      --control) CONTROL_MODE="${2:-}"; shift 2 ;;
      --sidecar) SIDECAR_MODE="${2:-}"; shift 2 ;;
      --no-web) WANT_WEB=0; shift ;;
      --no-sidecar) WANT_SIDECAR=0; SIDECAR_MODE="off"; shift ;;
      --no-watch) WATCH=0; shift ;;
      --reset-config) RESET_CONFIG=1; shift ;;
      -h|--help) usage; exit 0 ;;
      *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
  done
fi

case "$SIDECAR_MODE" in build|installed|off) ;; *) die "--sidecar takes build|installed|off" ;; esac
case "$STT_MODE" in ""|on|off) ;; *) die "--stt takes on|off" ;; esac
case "$CONTROL_MODE" in ""|on|off) ;; *) die "--control takes on|off" ;; esac
if [ "$SIDECAR_MODE" = "off" ]; then WANT_SIDECAR=0; fi

# --- recogniser selection ---------------------------------------------------
#
# One line of intent, resolved into what the wrapper exports:
#
#   --backend audiocpp            audio.cpp's Qwen3-ASR, weights from the cache
#   --backend audiocpp --model X  …weights at X, remembered for next time
#   --model X                     the same thing; naming weights implies the engine
#   --backend apple | (nothing)   Apple's SpeechAnalyzer, the sidecar's default
#
# A model path is remembered in `$MODEL_FILE` rather than required every run: the
# weights do not move, and re-typing a 60-character path is how a flag stops being
# used. `--backend audiocpp` with nothing remembered is a hard error — the engine
# starts without weights, prints `ready`, and then hears nothing all night, which
# in the database is indistinguishable from a quiet room.
if [ "$MODEL_GIVEN" = 1 ] && [ -z "$STT_BACKEND" ]; then
  STT_BACKEND=audiocpp
fi
case "$STT_BACKEND" in
  ""|"apple") STT_BACKEND="" ;;
  audiocpp) ;;
  *) die "--backend takes apple or audiocpp (got $STT_BACKEND)" ;;
esac
# Saying both is a contradiction, and silently dropping one of them is how a dev
# run ends up in the wrong engine with nothing in the log to say so.
if [ "$STT_BACKEND" != audiocpp ]; then
  if [ "$MODEL_GIVEN" = 1 ]; then
    die "--model belongs to --backend audiocpp; the Apple recogniser loads no weights"
  fi
  if [ -n "$STT_CHUNK" ]; then
    die "--chunk belongs to --backend audiocpp; Apple decodes whole phrases, not windows"
  fi
else
  if [ -n "$STT_CHUNK" ]; then
    case "$STT_CHUNK" in
      *[!0-9.]*|*.|"") die "--chunk takes seconds (got '$STT_CHUNK')" ;;
    esac
    awk -v c="$STT_CHUNK" 'BEGIN { exit !(c + 0 >= 1 && c + 0 <= 30) }' \
      || die "--chunk is outside 1..30 (got $STT_CHUNK)"
  fi
fi

# Everything above is argument checking; finding the weights waits for the boot
# section, because it reads and writes state and must not run for `dev.sh stop`
# (a client command never starts a recogniser, and a missing model must not stop
# someone from shutting the pipeline down).

if [ -n "$ONLY" ]; then
  WANT_WEB=0; WANT_SERVER=0; WANT_SIDECAR=0
  IFS=',' read -r -a roles <<<"$ONLY"
  for role in "${roles[@]}"; do
    case "$role" in
      web) WANT_WEB=1 ;;
      server) WANT_SERVER=1 ;;
      sidecar) WANT_SIDECAR=1; WANT_SERVER=1 ;;  # the server spawns it
      node) WANT_NODE=1; WANT_SERVER=1 ;;
      *) die "--only takes web,server,sidecar,node (got $role)" ;;
    esac
  done
fi

# `--backend` names a recogniser, so it needs the lane that runs one. Saying both
# is a contradiction, and silently dropping the backend is how a dev run ends up
# in the wrong engine with nothing in the log to say so.
if [ "$STT_BACKEND" = audiocpp ]; then
  if [ "$STT_MODE" = off ]; then
    die "--backend audiocpp with --stt off: the recogniser would never run"
  fi
  if [ "$WANT_SIDECAR" = 0 ]; then
    die "--backend audiocpp needs the sidecar lane (drop --no-sidecar/--sidecar off)"
  fi
  if [ "$SIDECAR_MODE" = installed ]; then
    # The installed wrapper is Apple's. audio.cpp's sidecar has its own
    # (`steno-stt-sidecar-audiocpp`), which takes no dev flags — a deployment
    # switch, not something this script can flip.
    die "--backend audiocpp runs the build in this repo; --sidecar installed names the Apple wrapper"
  fi
fi

# --- state files ------------------------------------------------------------
#
# One file per role, so the panel and a `dev.sh status` in another shell read the
# same truth without talking to this process.

state_set() { # role state pid note [url]
  mkdir -p "$STATE_DIR"
  {
    printf 'state=%s\n' "$2"
    printf 'pid=%s\n' "$3"
    printf 'note=%s\n' "$4"
    printf 'url=%s\n' "${5:-}"
  } >"$STATE_DIR/$1.tmp"
  mv "$STATE_DIR/$1.tmp" "$STATE_DIR/$1"
}

state_clear() { rm -f "$STATE_DIR/$1"; }

# Always succeeds: a role that has never run has no state file, and `set -e` would
# treat that as a fatal error in `pid="$(state_pid node)"`.
state_pid() {
  sed -n 's/^pid=//p' "$STATE_DIR/$1" 2>/dev/null | head -1 || true
}

# The free-text note of a role, so a client refresh keeps "builds 3" instead of
# blanking it.
state_pid_note() { sed -n 's/^note=//p' "$STATE_DIR/$1" 2>/dev/null | head -1; }

# --- read-only commands -----------------------------------------------------

# True when /api/v1/status was actually fetched. A stale copy from a previous run
# is worse than nothing: it would show a healthy pipeline for a server that is
# not running.
fetched=0
fetch_status() {
  fetched=0
  mkdir -p "$DEV_DIR" 2>/dev/null || true
  if have curl; then
    if curl -fsS --max-time 1 "$HTTP_BASE/api/v1/status" >"$STATUS_JSON.tmp" 2>/dev/null; then
      mv "$STATUS_JSON.tmp" "$STATUS_JSON"
      fetched=1
      return 0
    fi
  fi
  rm -f "$STATUS_JSON.tmp" "$STATUS_JSON"
  return 0
}

show_panel() {
  STENO_HTTP_PORT="$PORT" STENO_HTTP_BASE="$HTTP_BASE" STENO_STATUS_FETCHED="$fetched" \
    python3 "$PANEL" "$STATE_DIR" "$STATUS_JSON" "$FLOW_FILE" || true
}

show_logs() {
  local role="${1:-server}" lines="${2:-40}" follow=0
  case "$lines" in
    -f|--follow) follow=1; lines=40 ;;
  esac
  case "${3:-}" in
    -f|--follow) follow=1 ;;
  esac
  local file
  file="$(log_path_for "$role")"
  [ -n "$file" ] || die "unknown role: $role (server, sidecar, web, node)"
  if [ -z "$file" ] || [ ! -f "$file" ]; then
    die "no log for $role yet${file:+ (looked in $file)}"
  fi
  printf '\033[1m==> %s: %s\033[0m\n' "$role" "$file"
  if [ "$follow" = 1 ]; then
    tail -n "$lines" -f "$file"
  else
    tail -n "$lines" "$file"
  fi
}

show_schema() {
  local db
  db="$(sed -n 's/^database_path *= *"\(.*\)"/\1/p' "$CONFIG" 2>/dev/null | head -1)"
  db="${db:-$RUNTIME_DIR/db/app.db}"
  [ -f "$db" ] || die "no database at $db (start the server once)"
  if ! have sqlite3; then
    echo "sqlite3 is not on PATH; the schema is in crates/server/src/storage.rs"
    return 0
  fi
  local version
  version="$(sqlite3 "$db" 'PRAGMA user_version;')"
  printf '\033[1mschema v%s\033[0m  %s\n\n' "$version" "$db"
  printf '%-24s %8s  %s\n' TABLE ROWS DETAIL
  local table rows detail
  for table in recording_segments audio_gaps snore_events segment_transcripts utterances; do
    rows="$(sqlite3 "$db" "SELECT COUNT(*) FROM $table;" 2>/dev/null || echo '?')"
    detail=""
    case "$table" in
      snore_events)
        detail="$(sqlite3 "$db" "SELECT source||'='||COUNT(*) FROM snore_events GROUP BY source;" 2>/dev/null | paste -sd' ' -)"
        ;;
      recording_segments)
        detail="$(sqlite3 "$db" "SELECT status||'='||COUNT(*) FROM recording_segments GROUP BY status;" 2>/dev/null | paste -sd' ' -)"
        ;;
      segment_transcripts)
        detail="$(sqlite3 "$db" "SELECT status||'='||COUNT(*) FROM segment_transcripts GROUP BY status;" 2>/dev/null | paste -sd' ' -)"
        ;;
    esac
    printf '%-24s %8s  %s\n' "$table" "$rows" "$detail"
  done
  printf '\nfull DDL: sqlite3 %s ".schema --indent"\n' "$db"
}

queue_command() {
  if [ ! -d "$DEV_DIR" ]; then
    die "no dev environment in $DEV_DIR (start tools/dev.sh first)"
  fi
  printf '%s\n' "$*" >>"$CMD_FILE"
  dev "queued: $*"
}

# A command line either answers from files (read-only) or is queued for the
# supervisor. Nothing here needs to talk to a socket, which is why `status` works
# from any shell even while the supervisor is busy building.
# The client command runs further down, once the functions it calls exist.

# --- supervisor -------------------------------------------------------------

SERVER_PID=""
VITE_PID=""
NODE_PID=""
SERVER_BUILDS=0
SERVER_RESTARTS=0
SIDECAR_BUILDS=0
LAST_ERROR=""
TTY=0
[ -t 1 ] && TTY=1

# The supervisor's own state. A client command must not touch any of it: it used
# to truncate the flow buffer, re-anchor the offsets and — worst — write its own
# pid into supervisor.pid, so `dev.sh stop` read that file, "stopped" the pid it
# had just written (itself), and left the server it was meant to stop running.
if [ -z "$COMMAND" ]; then
  mkdir -p "$DEV_DIR" "$STATE_DIR"
  # Only the supervisor owns the queue and the flow buffer: a `dev.sh status` in
  # another shell must not wipe the commands waiting to be read.
  rm -f "$CMD_FILE"
  : >"$FLOW_FILE"
  # Start the flow at the end of the existing logs: an old night's segments are not
  # this session's events. Both sources are anchored, or the sidecar's backlog from a
  # previous run would be replayed into the band as if it had just happened.
  if [ -f "$RUNTIME_DIR/server.log" ]; then
    log_bytes "$RUNTIME_DIR/server.log" >"$FLOW_OFFSET"
  else
    echo 0 >"$FLOW_OFFSET"
    : >"$RUNTIME_DIR/server.log"
  fi
  SIDECAR_EXISTING="$(sidecar_log_path)"
  # A missing log is normal on a fresh dev directory: start at 0, so the sidecar's
  # first lines belong to this session instead of being skipped as "history".
  log_bytes "$SIDECAR_EXISTING" >"$FLOW_SIDECAR_OFFSET"
  # Remember which file that offset belongs to. Empty is a legitimate value: the
  # sidecar has not written anything yet, and the offset is then 0 — so its first
  # lines must not be skipped when the wrapper creates the file a second later.
  printf '%s' "$SIDECAR_EXISTING" >"$DEV_DIR/flow.sidecar.path"
  echo $$ >"$DEV_DIR/supervisor.pid"
fi

listener_pids() { lsof -nP -iTCP:"$PORT" -sTCP:LISTEN -t 2>/dev/null || true; }
udp_pids() { lsof -nP -iUDP:"$(capture_port)" -t 2>/dev/null || true; }

stop_pid() { # pid signal-wait-seconds
  local pid="$1" waited=0
  [ -n "$pid" ] || return 0
  kill -0 "$pid" 2>/dev/null || return 0
  kill -TERM "$pid" 2>/dev/null || true
  while kill -0 "$pid" 2>/dev/null; do
    [ "$waited" -ge "$((${2:-10} * 2))" ] && break
    sleep 0.5
    waited=$((waited + 1))
  done
  kill -0 "$pid" 2>/dev/null && kill -9 "$pid" 2>/dev/null || true
}

# Only this server's sidecar: the socket path carries the server's pid, so a
# second instance running from the same checkout is never touched by mistake.
sidecar_pids() {
  [ -n "$SERVER_PID" ] || return 0
  pids_matching "steno-stt-$SERVER_PID.sock"
}

kill_sidecar() {
  local pids
  pids="$(sidecar_pids)"
  [ -n "$pids" ] || return 0
  dev "stopping sidecar pid(s) $(echo "$pids" | paste -sd' ' -)"
  # shellcheck disable=SC2086
  kill -TERM $pids 2>/dev/null || true
  sleep 0.5
  pids="$(sidecar_pids)"
  # shellcheck disable=SC2086
  [ -n "$pids" ] && kill -9 $pids 2>/dev/null || true
  return 0
}

stop_server() {
  [ -n "$SERVER_PID" ] || return 0
  dev "stopping server pid $SERVER_PID"
  stop_pid "$SERVER_PID" 10
  SERVER_PID=""
  state_clear server
}

stop_web() {
  [ -n "$VITE_PID" ] || return 0
  stop_pid "$VITE_PID" 3
  VITE_PID=""
  state_clear web
}

stop_node() {
  [ -n "$NODE_PID" ] || return 0
  stop_pid "$NODE_PID" 3
  NODE_PID=""
  state_clear node
}

cleanup() {
  local status=$?
  trap - INT TERM EXIT
  printf '\n'
  dev "shutting down"
  stop_node
  stop_web
  # The sidecar first, while SERVER_PID still identifies it: `sidecar_pids` matches
  # on `steno-stt-<server pid>.sock`, and `stop_server` clears SERVER_PID. In the
  # other order the sidecar was left to notice the closed socket and exit by
  # itself, which usually works but leaves a window where the panel still says it
  # is up.
  kill_sidecar
  state_set sidecar down - "shut down with the supervisor"
  stop_server
  rm -f "$CMD_FILE" "$DEV_DIR/supervisor.pid"
  dev "done. To put the detached instance back: tools/refresh-all.sh --no-node"
  exit "$status"
}
trap cleanup INT TERM EXIT

# --- config -----------------------------------------------------------------

patch_section() { # file section value
  local file="$1" section="$2" value="$3"
  awk -v sec="$section" -v v="$value" '
    $0 ~ "^\\[" sec "\\]$" { inside = 1; print; next }
    /^\[/ { inside = 0 }
    inside && $0 ~ "^enabled[[:space:]]*=" { print "enabled = " v; next }
    { print }
  ' "$file" >"$file.dev" && mv "$file.dev" "$file"
}

patch_stt_command() { # file value
  local file="$1" value="$2"
  awk -v v="$value" '
    /^\[stt\]$/ { inside = 1; print; print "stream_command = \"" v "\""; next }
    /^\[/ { inside = 0 }
    inside && $0 ~ "^stream_command[[:space:]]*=" { next }
    { print }
  ' "$file" >"$file.dev" && mv "$file.dev" "$file"
}

# The sidecar wrapper this repo installs goes through LaunchServices (`open -a`),
# which returns immediately and hands the child no stdio. A dev build can skip
# both: run the binary detached with its log in the dev directory, so the console
# can show it and `kill` can reach it.
write_sidecar_wrapper() {
  mkdir -p "$DEV_DIR"
  # A missing model cannot reach this point — `--backend audiocpp` resolves one
  # before anything starts — so this is a backstop, not a code path.
  if [ "$STT_BACKEND" = audiocpp ] && [ -z "$STT_MODEL" ]; then
    say "--backend audiocpp without a model; pass --model <path to the .gguf>" >&2
    exit 2
  fi
  # Quoted delimiter on purpose: an unquoted heredoc expands backticks and `$(…)`
  # inside these comments, and a comment that mentions `--log-text` would be run as
  # a command (it was: "line 441: --log-text: command not found").
  cat >"$SIDECAR_WRAPPER" <<'WRAPPER'
#!/bin/sh
# Generated by tools/dev.sh — launches the sidecar built in this repo, detached,
# so the server's Command::status() returns as soon as it is started.
#
# --log-text puts recognised phrases in the log too, which is a dev-only
# convenience: production keeps transcripts out of logs (see apple-stt-sidecar
# Sources/Log.swift). `tools/dev.sh logs sidecar` then answers "what did it hear"
# without a browser.
WRAPPER
  # The command line is printed with `%q` rather than substituted into the heredoc:
  # a model path may contain spaces, and `sed s|…|…|` would either break on them
  # or quietly mangle the arguments.
  {
    printf 'exec </dev/null\n'
    if [ "$STT_BACKEND" = audiocpp ]; then
      # The audio.cpp engine is a Rust sidecar built from crates/server, so it takes
      # its model from the environment instead of flags. `exec` before `nohup` is
      # deliberate: the wrapper has to exit immediately (the server's launcher
      # waits for it) while the sidecar keeps running.
      printf 'export STENO_STT_MODEL=%q\n' "$STT_MODEL"
      if [ -n "$STT_CHUNK" ]; then printf 'export STENO_STT_CHUNK=%q\n' "$STT_CHUNK"; fi
      # Window-level lines without any text in them (chars, not content), which is
      # what `--log-text` gave the Swift wrapper without putting a transcript in
      # a log file.
      printf 'export STENO_LOG=%q\n' "info,steno_server::audiocpp=debug"
      printf 'nohup %q --socket "$1" >>%q 2>&1 &\n' "$AUDIOCPP_SIDECAR_BIN" "$SIDECAR_LOG"
    else
      printf 'nohup %q --socket "$1" --log-text' "$SIDECAR_BIN"
      printf ' >>%q 2>&1 &\n' "$SIDECAR_LOG"
    fi
  } >>"$SIDECAR_WRAPPER"
  chmod +x "$SIDECAR_WRAPPER"
}

# Finds the GGUF `--backend audiocpp` will load, and remembers it. Three places,
# in the order a human would look: this run's `--model`, this checkout's last run
# (so `--backend audiocpp` alone works tomorrow), then whatever `install.sh`
# recorded for the installed sidecar.
resolve_audiocpp_model() {
  [ "$STT_BACKEND" = audiocpp ] || return 0
  if [ -z "$STT_MODEL" ]; then STT_MODEL="$(remembered_model)"; fi
  if [ -z "$STT_MODEL" ]; then
    STT_MODEL="$(installed_audiocpp_model)"
    [ -z "$STT_MODEL" ] || say "using the model recorded for the installed sidecar"
  fi
  [ -n "$STT_MODEL" ] || die "--backend audiocpp needs the weights once:
  tools/dev.sh --backend audiocpp --model <file.gguf | dir>
It is remembered in $MODEL_FILE afterwards, and the short form is then enough."
  STT_MODEL="$(expand_home "$STT_MODEL")"
  STT_MODEL="$(resolve_model_path "$STT_MODEL")"
  remember_model "$STT_MODEL"
  dev "recogniser audiocpp · model $STT_MODEL${STT_CHUNK:+ · ${STT_CHUNK}s windows}"
}

prepare_config() {
  mkdir -p "$RUNTIME_DIR"
  resolve_audiocpp_model
  # The point of this script is the whole pipeline, so the sidecar lane is on
  # unless it was explicitly asked to stay off. Asking for `--only server` is a
  # request for a backend-only run, so it turns the lane off rather than leaving
  # the server trying to launch something this script does not manage.
  if [ -z "$STT_MODE" ]; then
    if [ "$WANT_SIDECAR" = 1 ]; then STT_MODE=on; else STT_MODE=off; fi
  fi
  if [ ! -f "$CONFIG" ] || [ "$RESET_CONFIG" = 1 ]; then
    local args=(--write-config-only)
    [ "$RESET_CONFIG" = 1 ] && args+=(--reset-config)
    [ "$STT_MODE" = on ] && args+=(--stt)
    [ "$CONTROL_MODE" = on ] && args+=(--control)
    say "generating $CONFIG"
    "$REPO_ROOT/tools/run-server-mac.sh" "${args[@]}"
  fi
  if [ "$STT_MODE" = on ]; then patch_section "$CONFIG" stt true; fi
  if [ "$STT_MODE" = off ]; then patch_section "$CONFIG" stt false; fi
  if [ "$CONTROL_MODE" = on ]; then patch_section "$CONFIG" control true; fi
  if [ "$CONTROL_MODE" = off ]; then patch_section "$CONFIG" control false; fi
  if [ "$STT_MODE" = on ] || [ "$WANT_SIDECAR" = 1 ]; then
    case "$SIDECAR_MODE" in
      build)
        write_sidecar_wrapper
        patch_stt_command "$CONFIG" "$SIDECAR_WRAPPER"
        ;;
      installed)
        patch_stt_command "$CONFIG" "steno-stt-sidecar"
        ;;
      esac
  fi
  # The dev server must serve the API on the port Vite proxies to.
  awk -v p="$PORT" '
    /^\[server\]$/ { inside = 1; print; next }
    /^\[/ { inside = 0 }
    inside && /^port[[:space:]]*=/ { print "port = " p; next }
    { print }
  ' "$CONFIG" >"$CONFIG.dev" && mv "$CONFIG.dev" "$CONFIG"
}

# The UDP port the server actually listens on, so the node is pointed at this
# instance rather than at whatever 8091 happens to be.
capture_port() {
  local port
  port="$(awk '
    /^\[source\]$/ { inside = 1; next }
    /^\[/ { inside = 0 }
    inside && /^port[[:space:]]*=/ { gsub(/[^0-9]/, "", $0); print; exit }
  ' "$CONFIG" 2>/dev/null)"
  printf '%s' "${port:-$CAPTURE_PORT}"
}

prepare_node_config() {
  local node_config="$RUNTIME_DIR/node.toml" port
  port="$(capture_port)"
  if [ ! -f "$node_config" ]; then
    say "writing $node_config from deployment/node.example.toml"
    awk -v host="127.0.0.1" '
      /^server_host[[:space:]]*=/ { print "server_host = \"" host "\""; next }
      { print }
    ' "$REPO_ROOT/deployment/node.example.toml" >"$node_config"
  fi
  # Keep it pointed at this instance even when the port changed since the file was
  # written: a node sending to the wrong port looks exactly like a dead node.
  awk -v p="$port" '
    /^server_port[[:space:]]*=/ { print "server_port = " p; next }
    { print }
  ' "$node_config" >"$node_config.dev" && mv "$node_config.dev" "$node_config"
  printf '%s' "$node_config"
}

# --- children ---------------------------------------------------------------

start_web() {
  [ "$WANT_WEB" = 1 ] || return 0
  [ -n "$VITE_PID" ] && return 0
  if ! have npm; then warn "npm is not on PATH; skipping web"; WANT_WEB=0; state_set web down - "npm missing"; return 0; fi
  [ -d "$REPO_ROOT/web/node_modules" ] || {
    say "installing web dependencies (first run)"
    npm --prefix "$REPO_ROOT/web" install --silent
  }
  state_set web starting - "starting"
  VITE_API_TARGET="$HTTP_BASE" npm --prefix "$REPO_ROOT/web" run dev >"$DEV_DIR/web.log" 2>&1 &
  VITE_PID=$!
  local url="" i
  for i in $(seq 1 40); do
    url="$(grep -m1 "Local:" "$DEV_DIR/web.log" 2>/dev/null | sed 's/^ *//;s/.*Local: *//' || true)"
    [ -n "$url" ] && break
    kill -0 "$VITE_PID" 2>/dev/null || { warn "Vite exited; see $DEV_DIR/web.log"; break; }
    sleep 0.5
  done
  # Vite picks the next free port when 5173 is busy, so the banner is the only
  # authority on the URL — never assume the default.
  state_set web up "$VITE_PID" "HMR" "${url:-see web.log}"
  dev "web pid $VITE_PID ${url:-}"
}

build_sidecar() {
  [ "$WANT_SIDECAR" = 1 ] || return 0
  state_set sidecar starting - "building"
  # Two different builds, one role. Apple's recogniser is the Swift bundle;
  # audio.cpp's is a second [[bin]] of the server crate, so the same `cargo build`
  # that makes the server makes it — and building the bundle as well would be a
  # minute spent on a binary the server is not going to launch.
  if [ "$STT_BACKEND" = audiocpp ]; then
    if ! cargo build --manifest-path "$REPO_ROOT/Cargo.toml" -p steno-server \
        >"$DEV_DIR/sidecar-build.log" 2>&1; then
      warn "audiocpp sidecar build failed; see $DEV_DIR/sidecar-build.log"
      tail -12 "$DEV_DIR/sidecar-build.log" >&2 || true
      state_set sidecar down - "build failed"
      return 1
    fi
    write_sidecar_wrapper
    SIDECAR_BUILDS=$((SIDECAR_BUILDS + 1))
    state_set sidecar down - "built, waiting for the server"
    return 0
  fi
  if ! "$REPO_ROOT/apple-stt-sidecar/build.sh" >"$DEV_DIR/sidecar-build.log" 2>&1; then
    warn "sidecar build failed; see $DEV_DIR/sidecar-build.log"
    tail -12 "$DEV_DIR/sidecar-build.log" >&2 || true
    state_set sidecar down - "build failed"
    return 1
  fi
  SIDECAR_BUILDS=$((SIDECAR_BUILDS + 1))
  write_sidecar_wrapper
  state_set sidecar down - "built, waiting for the server"
  return 0
}

start_server() {
  [ "$WANT_SERVER" = 1 ] || return 0
  [ -n "$SERVER_PID" ] && return 0
  if ! cargo build --manifest-path "$REPO_ROOT/Cargo.toml" -p steno-server; then
    warn "build failed — the server that is running (if any) was left alone"
    state_set server down - "build failed"
    return 1
  fi
  SERVER_BUILDS=$((SERVER_BUILDS + 1))
  # Follow the log from where it is: replaying a previous run's segments into the
  # FLOW band would look like they just happened.
  log_bytes "$RUNTIME_DIR/server.log" >"$FLOW_OFFSET"
  log_bytes "$(sidecar_log_path)" >"$FLOW_SIDECAR_OFFSET"
  STENO_MONITOR_CONFIG="$CONFIG" RUST_LOG="${RUST_LOG:-info,steno_server=debug}" \
    "$REPO_ROOT/target/debug/steno-monitor" >>"$RUNTIME_DIR/server.log" 2>&1 &
  SERVER_PID=$!
  SERVER_RESTARTS=$((SERVER_RESTARTS + 1))
  state_set server up "$SERVER_PID" "builds $SERVER_BUILDS" "$HTTP_BASE"
  dev "server pid $SERVER_PID · log $RUNTIME_DIR/server.log"
  return 0
}

start_node() {
  [ "$WANT_NODE" = 1 ] || return 0
  [ -n "$NODE_PID" ] && return 0
  local node_config
  node_config="$(prepare_node_config)"
  if ! cargo build --manifest-path "$REPO_ROOT/Cargo.toml" -p steno-node; then
    warn "node build failed"
    state_set node down - "build failed"
    return 1
  fi
  STENO_NODE_CONFIG="$node_config" RUST_LOG="${RUST_LOG:-info}" \
    "$REPO_ROOT/target/debug/steno-node" "$node_config" >>"$DEV_DIR/node.log" 2>&1 &
  NODE_PID=$!
  state_set node up "$NODE_PID" "udp :$(capture_port) → 127.0.0.1"
  dev "node pid $NODE_PID · log $DEV_DIR/node.log"
  return 0
}

# --- change detection -------------------------------------------------------

changed() { # role
  local stamp="$DEV_DIR/stamp.$1" paths=()
  shift
  [ -n "$stamp" ] || return 1
  find "$@" -newer "$stamp" -type f \( -name '*.rs' -o -name '*.swift' -o -name 'Cargo.toml' -o -name 'Info.plist' \) 2>/dev/null | head -1 | grep -q .
}

stamp() { # role
  touch "$DEV_DIR/stamp.$1"
}

watch_server() { changed server "$REPO_ROOT/crates" "$REPO_ROOT/Cargo.toml" "$REPO_ROOT/Cargo.lock"; }
watch_sidecar() { changed sidecar "$REPO_ROOT/apple-stt-sidecar/Sources" "$REPO_ROOT/apple-stt-sidecar/Info.plist"; }
watch_node() { changed node "$REPO_ROOT/crates/node"; }

reload_server() {
  say "rebuilding the server"
  stop_server
  start_server || true
  stamp server
}

reload_sidecar() {
  say "rebuilding the sidecar"
  build_sidecar || { state_set sidecar down - "build failed"; return 1; }
  stamp sidecar
  # The server owns the sidecar's lifetime: killing it makes the stt worker
  # reconnect and start the new build within its 5 s restart delay.
  kill_sidecar
  state_set sidecar down - "restarting after rebuild"
}

reload_node() {
  say "rebuilding the node"
  stop_node
  start_node || true
  stamp node
}

# --- flow feed --------------------------------------------------------------
#
# The panel's bottom band. Lines are read from the *end* of the log the previous
# tick stopped at, so a restart does not replay the whole night, and only lines
# that say something about the pipeline survive the filter.

# The sidecar writes its own half of the story, so its lines are part of the same
# band: `text` is a recognised phrase (only with --log-text, which the wrapper
# passes), `session started/finished` is a reconnection, and `snore:` is the
# classifier firing. `analysis completed` and `connected to` are the two lines that
# tell a dead lane apart from a quiet room.
FLOW_PATTERN='transcript|utterance|snore|snore_events|sidecar|stt stream|stream session|segment (started|complete|interrupted)|recognis|classifier|error|warn|text \[|session (started|finished)|analysis completed|connected to'

pump_flow() {
  [ -f "$RUNTIME_DIR/server.log" ] || return 0
  # The pump lives in dev-panel.py because it has to cut each log on a line
  # boundary and then merge the two by timestamp: the writers are other processes,
  # a torn line both looks wrong and loses the rest of the message, and appending
  # the sidecar's lines after the server's would put them in the wrong order.
  local sidecar_log marker previous
  marker="$DEV_DIR/flow.sidecar.path"
  sidecar_log="$(sidecar_log_path)"
  if [ -n "$sidecar_log" ]; then
    previous="$(cat "$marker" 2>/dev/null)"
    if [ -n "$previous" ] && [ "$previous" != "$sidecar_log" ]; then
      # A different file means the old offset belongs to another file: applying it
      # here would silently skip the first N bytes of this one. Start at its end,
      # and say so rather than switch quietly.
      log_bytes "$sidecar_log" >"$FLOW_SIDECAR_OFFSET"
      printf '%s' "$sidecar_log" >"$marker"
      dev "sidecar log is now $sidecar_log"
    fi
    python3 "$PANEL" --pump "$FLOW_FILE" "$FLOW_PATTERN" \
      "server|$RUNTIME_DIR/server.log|$FLOW_OFFSET" \
      "sidecar|$sidecar_log|$FLOW_SIDECAR_OFFSET" >/dev/null 2>&1 || true
  else
    python3 "$PANEL" --pump "$FLOW_FILE" "$FLOW_PATTERN" \
      "server|$RUNTIME_DIR/server.log|$FLOW_OFFSET" >/dev/null 2>&1 || true
  fi
}

# --- rendering --------------------------------------------------------------

# The last line a service wrote, as the dashboard note. When a child dies, the
# *reason* is the only thing worth showing, and it is already in its log.
last_line() {
  local file
  file="$(log_path_for "$1")"
  [ -n "$file" ] || return 0
  tail -n 1 "$file" 2>/dev/null | sed 's/^[^ ]*Z *//; s/^ *//; s/^steno-stt-sidecar\[[0-9]*\]: //' \
    | cut -c1-96 || true
}

# Writes one state file per role from what is actually running. Shared by the
# supervisor's redraw and by `dev.sh status` in another shell: the panel only ever
# reads these files, so both paths must produce the same picture.
refresh_state() {
  # Outside the supervisor there is no `$SERVER_PID` in memory, so take it from
  # the listening socket: the sidecar's socket path carries it, and without this
  # `dev.sh status` from another shell would report the sidecar as "waiting" while
  # it is plainly running.
  local server_pid sidecar_pid
  server_pid="$SERVER_PID"
  [ -n "$server_pid" ] || server_pid="$(listener_pids | head -1)"
  sidecar_pid=""
  if [ -n "$server_pid" ]; then
    sidecar_pid="$(pids_matching "steno-stt-$server_pid.sock" | head -1)"
  fi
  if [ -z "$SERVER_PID" ] && [ -n "$server_pid" ]; then
    state_set server up "$server_pid" "" "$HTTP_BASE"
  fi
  if [ -n "$sidecar_pid" ]; then
    local stt_locale
    # The config may predate `[stt] locale`; the server's own answer is the truth
    # either way, so fall back to the status JSON rather than showing "unknown".
    stt_locale="$(sed -n 's/^locale *= *"\(.*\)"/\1/p' "$CONFIG" 2>/dev/null | head -1)"
    if [ -z "$stt_locale" ] && [ -f "$STATUS_JSON" ]; then
      stt_locale="$(grep -o '"locale":"[^"]*"' "$STATUS_JSON" 2>/dev/null | head -1 | cut -d'"' -f4)"
    fi
    state_set sidecar up "$sidecar_pid" "locale ${stt_locale:-unknown}"
  elif [ "$WANT_SIDECAR" = 1 ] && [ -n "$server_pid" ]; then
    state_set sidecar down - "waiting for the server to launch it"
  fi

  if [ -n "$SERVER_PID" ] && ! kill -0 "$SERVER_PID" 2>/dev/null; then
    LAST_ERROR="the server exited by itself; last lines:"
    warn "$LAST_ERROR"
    tail -15 "$RUNTIME_DIR/server.log" >&2 || true
    SERVER_PID=""
    state_set server down - "exited: $(last_line server)"
  fi
  if [ -z "$SERVER_PID" ] && [ -z "$server_pid" ] && [ "$WANT_SERVER" = 1 ]; then
    state_set server down - "not running"
  fi

  # A child that died on its own is reported once, with the line that explains it.
  # It is not restarted here: a crash loop would bury the reason under repetition.
  if [ -n "$NODE_PID" ] && ! kill -0 "$NODE_PID" 2>/dev/null; then
    warn "the node exited: $(last_line node)"
    NODE_PID=""
    state_set node down - "exited: $(last_line node)"
  fi
  if [ -n "$VITE_PID" ] && ! kill -0 "$VITE_PID" 2>/dev/null; then
    warn "Vite exited: $(last_line web)"
    VITE_PID=""
    state_set web down - "exited: $(last_line web)"
  fi

}

render() {
  fetch_status
  refresh_state
  if [ "$TTY" = 1 ]; then
    printf '\033[H\033[2J'
    show_panel
  fi
}

note_event() { # printed even when stdout is not a terminal
  dev "$*"
}

# --- command handling -------------------------------------------------------

handle_command() { # verb [role]
  local verb="$1" role="${2:-all}"
  case "$verb" in
    quit) exit 0 ;;
    start)
      case "$role" in
        web) WANT_WEB=1; start_web ;;
        server) WANT_SERVER=1; start_server || true ;;
        node) WANT_NODE=1; start_node || true ;;
        sidecar) WANT_SIDECAR=1; build_sidecar || true ;;
        all) start_web; start_server || true; start_node || true ;;
        *) warn "start takes web|server|sidecar|node|all" ;;
      esac
      ;;
    stop)
      case "$role" in
        web) stop_web; WANT_WEB=0 ;;
        server) stop_server; stop_node ;;
        node) stop_node ;;
        sidecar) kill_sidecar; state_set sidecar down - "stopped by command" ;;
        # `stop all` inside the supervisor means the whole run is over, so it ends
        # here: a supervisor with no roles left would keep redrawing an empty
        # panel and keep holding the runtime directory.
        all) dev "stop all — shutting down"; exit 0 ;;
        *) warn "stop takes web|server|sidecar|node|all" ;;
      esac
      note_event "$verb $role"
      ;;
    restart|reload)
      case "$role" in
        web) stop_web; start_web ;;
        server) reload_server ;;
        node) reload_node ;;
        sidecar) reload_sidecar || true ;;
        all) reload_sidecar || true; reload_server; reload_node ;;
        *) warn "$verb takes web|server|sidecar|node|all" ;;
      esac
      note_event "$verb $role"
      ;;
    *) warn "unknown command: $verb $role" ;;
  esac
}

drain_commands() {
  [ -s "$CMD_FILE" ] || return 0
  local lines
  lines="$(cat "$CMD_FILE")"
  : >"$CMD_FILE"
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    # shellcheck disable=SC2086
    handle_command $line
  done <<<"$lines"
}

nap() {
  sleep "$1" &
  wait $! 2>/dev/null || true
}

# --- client commands --------------------------------------------------------

if [ -n "$COMMAND" ]; then
  # A client command must not run the supervisor's cleanup: the trap below stops
  # the node, the web server, the server and the sidecar, and this process owns
  # none of them. `dev.sh logs sidecar` was killing its own pipeline by exiting.
  trap - INT TERM EXIT
  case "$COMMAND" in
    status) fetch_status; refresh_state; show_panel ;;
    schema) show_schema ;;
    logs) show_logs "$ARG1" "${ARG2:-40}" ;;
    # `stop` is answered here, not queued: it has to work when the supervisor is
    # wedged, and the supervisor is one of the things it stops.
    stop) stop_everything "${ARG1:-all}" ;;
    quit) queue_command quit ;;
    *) queue_command "$COMMAND" ${ARG1:+"$ARG1"} ;;
  esac
  exit 0
fi

# --- boot -------------------------------------------------------------------

have cargo || die "cargo is not on PATH"
have python3 || warn "python3 is not on PATH; the console panel will not render"

# Taking over the port is deliberate — the same runtime directory as a detached
# run is the point — but only from a process this repo started. Killing whatever
# happens to listen on 8080 is how a dev run takes down an unrelated service (it
# took the detached var/ instance down once, for no good reason).
if [ -n "$(listener_pids)" ]; then
  HOLDER="$(listener_pids | head -1)"
  HOLDER_CMD="$(ps -o command= -p "$HOLDER" 2>/dev/null || true)"
  case "$HOLDER_CMD" in
    *"$REPO_ROOT/target/"*|*steno-monitor*)
      say "port $PORT is taken by pid $HOLDER; stopping it"
      stop_pid "$HOLDER" 10
      ;;
    *)
      die "port $PORT is held by pid $HOLDER ($HOLDER_CMD) — not mine to stop; pass --port"
      ;;
  esac
fi
prepare_config
# A node needs the UDP port this server is about to bind, and whatever held it is
# almost certainly an older instance of this same pipeline.
if [ "$WANT_NODE" = 1 ] && [ -n "$(udp_pids)" ]; then
  say "UDP $(capture_port) is taken; stopping the listener (a node needs it)"
  stop_pid "$(udp_pids | head -1)" 3
fi
if [ "$WANT_SIDECAR" = 1 ] && [ "$SIDECAR_MODE" = "build" ]; then
  build_sidecar || warn "continuing without a fresh sidecar build"
else
  stamp sidecar
fi
stamp server
stamp node

start_web
start_server || die "the server did not start; see $RUNTIME_DIR/server.log"
start_node || true
sleep 1

say "supervisor pid $$ — panel below, commands via tools/dev.sh <cmd>"
[ "$TTY" = 1 ] || dev "stdout is not a terminal; lifecycle lines only (use 'tools/dev.sh status' for the panel)"

while :; do
  drain_commands
  pump_flow

  if [ "$WATCH" = 1 ]; then
    if [ "$WANT_SERVER" = 1 ] && watch_server; then
      note_event "crates/ changed"
      reload_server
    fi
    if [ "$WANT_SIDECAR" = 1 ] && [ "$STT_BACKEND" != audiocpp ] && watch_sidecar; then
      note_event "apple-stt-sidecar/ changed"
      reload_sidecar || true
    fi
    if [ "$WANT_NODE" = 1 ] && watch_node; then
      note_event "crates/node/ changed"
      reload_node
    fi
  fi

  render
  nap 1
done
