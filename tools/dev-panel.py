#!/usr/bin/env python3
"""Render the dev console panel.

`tools/dev.sh` runs the processes; this draws what they are doing. Kept apart
because process supervision in bash is already fragile enough without string
formatting inside it, and because the same renderer serves both the supervisor
(once a second) and `tools/dev.sh status` (once, from another shell).

Inputs, all optional so a half-started environment still renders:

  <state-dir>    one file per role, written by dev.sh:  pid=…  since=…  note=… URL=…
  <status.json>  /api/v1/status from the server, or empty when it is not up
  <flow-file>    pre-filtered workflow lines, newest last, "HH:MM:SS<TAB>text"

Exit code is always 0: a dashboard that cannot draw must not look like a crash.
"""

import json
import os
import re
from datetime import datetime, timezone
import sys
import time

RESET = "\033[0m"
BOLD = "\033[1m"
DIM = "\033[2m"
GREEN = "\033[32m"
YELLOW = "\033[33m"
RED = "\033[31m"
CYAN = "\033[36m"
GREY = "\033[90m"

ROLE_ORDER = ["node", "server", "sidecar", "web"]
ROLE_DOC = {
    "node": "UDP capture sender",
    "server": "orchestrator: record, detect, store, HTTP",
    "sidecar": "classification + recognition (spawned by the server)",
    "web": "Vite dev server (HMR)",
}


class Paint:
    def __init__(self, enabled):
        self.enabled = enabled

    def __call__(self, text, colour=""):
        if not self.enabled or not colour:
            return str(text)
        return f"{colour}{text}{RESET}"


def read_state(path):
    roles = {}
    if not os.path.isdir(path):
        return roles
    for name in ROLE_ORDER:
        entry = {}
        try:
            with open(os.path.join(path, name), encoding="utf-8") as handle:
                for line in handle:
                    line = line.strip()
                    if not line or "=" not in line:
                        continue
                    key, value = line.split("=", 1)
                    entry[key] = value
        except OSError:
            continue
        roles[name] = entry
    return roles


def read_status(path):
    try:
        with open(path, encoding="utf-8") as handle:
            return json.load(handle)
    except (OSError, ValueError):
        return None


def read_flow(path, limit=8):
    try:
        with open(path, encoding="utf-8") as handle:
            lines = [line.rstrip("\n") for line in handle if line.strip()]
    except OSError:
        return []
    return lines[-limit:]


def alive(pid):
    try:
        os.kill(int(pid), 0)
        return True
    except (OSError, ValueError, TypeError):
        return False


def role_row(paint, name, entry):
    state = entry.get("state", "off")
    pid = entry.get("pid", "")
    note = entry.get("note", "")
    url = entry.get("url", "")
    if state == "up" and pid and not alive(pid):
        # The file outlived the process: say so rather than showing a stale pid
        # that a `kill -0` would contradict.
        state = "stale"
    colour = {"up": GREEN, "starting": YELLOW, "down": RED, "stale": RED}.get(state, GREY)
    detail = url or note
    if url and note:
        detail = f"{url}  {paint(note, DIM)}"
    return (
        f" {paint(name.ljust(8), BOLD)} {paint(state.ljust(11), colour)} "
        f"{str(pid or '-').ljust(7)} {detail}"
    )


def pipeline_rows(paint, status):
    if not status:
        base = os.environ.get("STENO_HTTP_BASE", "http://127.0.0.1:8080")
        if os.environ.get("STENO_STATUS_FETCHED") == "0":
            # Distinguish "we could not ask" from "it answered nothing": the first
            # is a server that is not running (or on another port), and saying
            # "no status yet" would hide that.
            return [f" {paint(f'no answer from {base}/api/v1/status', RED)}"
                    f" {paint('(server not running, or a different port)', DIM)}"]
        return [f" {paint('no /api/v1/status yet — is the server up?', YELLOW)}"]

    rows = []
    source = status.get("source") or {}
    mic = status.get("microphone") or {}
    capture = status.get("capture") or {}
    recording = status.get("recording") or {}
    storage = status.get("storage") or {}
    detector = status.get("detector") or {}
    stt = status.get("stt") or {}

    if source.get("kind") == "net":
        connected = source.get("node_connected")
        capture_state = (GREEN + "node ok") if connected else (RED + "no node")
        rows.append(
            f" {paint('capture', BOLD)} {(paint('ok' if connected else 'waiting', GREEN if connected else RED)).ljust(11)}"
            f"{mic.get('capture_rate_hz', '?')} Hz x{mic.get('channels', '?')}"
            f"   packets {source.get('packets_received', 0)} lost {source.get('packets_lost', 0)}"
            f"   {paint(capture_state.replace(GREEN, '').replace(RED, ''), GREEN if connected else RED)}"
        )
    else:
        rows.append(
            f" {paint('capture', BOLD)} {str(mic.get('state', '?')).ljust(11)}"
            f"{mic.get('device') or '(local device)'}  {mic.get('capture_rate_hz', '?')} Hz x{mic.get('channels', '?')}"
        )

    used = storage.get("used_bytes")
    used_mb = f"{used / 1e6:.1f} MB" if isinstance(used, (int, float)) else "?"
    max_mb = f"{(storage.get('max_bytes') or 0) / 1e9:.2f} GB"
    rows.append(
        f" {paint('record', BOLD)} {str(recording.get('state', '?')).ljust(11)}"
        f"{used_mb} / {max_mb}   {(storage.get('segment_count') if storage.get('segment_count') is not None else '?')} segment(s)"
        f"   gaps {capture.get('lost_frames_total', 0)} frames   xruns {capture.get('xrun_count', 0)}"
    )

    floor = detector.get("noise_floor_dbfs")
    floor_text = f"{floor:.1f} dBFS" if isinstance(floor, (int, float)) else "unknown"
    rows.append(
        f" {paint('detect', BOLD)} {str(detector.get('state', '?')).ljust(11)}"
        f"noise floor {floor_text}   events {detector.get('events_total', 0)}"
        f"   {paint('rule-v1', DIM)}"
    )

    peak = capture.get("level_peak_dbfs")
    rms = capture.get("level_rms_dbfs")
    if isinstance(peak, (int, float)):
        filled = max(0, min(18, int(round((max(-80.0, min(0.0, peak)) + 80) / 80 * 18))))
        colour = RED if peak >= -1 else GREEN if peak >= -40 else YELLOW if peak >= -70 else GREY
        meter = paint("█" * filled + "░" * (18 - filled), colour)
        rows.append(
            f" {paint('signal', BOLD)} {str(status.get('signal_state', '?'))[:11].ljust(11)}"
            f"{meter} peak {peak:6.1f} dBFS   rms {rms:6.1f}"
        )
    else:
        # No block dispatched yet: say so rather than drawing an empty bar that
        # reads like silence.
        rows.append(f" {paint('signal', BOLD)} {'unknown'.ljust(11)}no block measured yet")

    stt_state = str(stt.get("state", "off"))
    stt_colour = {"ready": GREEN, "starting": YELLOW, "error": RED}.get(stt_state, GREY)
    rows.append(
        f" {paint('stt', BOLD)} {paint(stt_state.ljust(11), stt_colour)}"
        f"{stt.get('mode', '?')} {stt.get('locale') or '?'} {stt.get('engine', '')}"
        f"   finals {stt.get('finals_total', 0)}"
        f"   snores {stt.get('snore_events_total', 0)}"
        f"   dropped {stt.get('dropped_blocks', 0)}"
    )
    if stt.get("error"):
        rows.append(f" {paint('error', BOLD)} {paint(stt['error'], RED)}")
    partial = stt.get("partial")
    if partial:
        rows.append(f" {paint('partial', DIM)} 「{partial}」")
    return rows


TARGET = re.compile(r"^(\S+)\s+([A-Z]+)\s+(\S+?):\s+(.*)$")
# `steno-stt-sidecar[66193]: 18:41:08.123 text [0..8376] …`; the time is optional
# because a log written by an older build has none, and refusing those lines would
# hide exactly the runs that are already on disk.
SIDECAR = re.compile(r"^steno-stt-sidecar\[(\d+)\]:\s+(?:(\d\d:\d\d:\d\d\.\d{3})\s+)?(.*)$")
# `RUST_LOG_STYLE` is not always set, and a log written by an older build (or by
# the sidecar) can still carry colour codes. Strip them rather than fail to parse.
ANSI = re.compile(r"\x1b\[[0-9;]*m")


def _tail(path, offset):
    """Whole lines appended to `path` since `offset`.

    Only whole lines are consumed: the writer is another process, so the tail of
    the file is routinely half a line, and emitting that would put a torn
    timestamp in the panel and lose the rest of the message forever.
    """
    try:
        size = os.path.getsize(path)
    except OSError:
        return [], offset
    if size < offset:
        offset = 0          # the log was replaced under us
    if size == offset:
        return [], offset
    with open(path, "rb") as handle:
        handle.seek(offset)
        data = handle.read(512 * 1024)
    cut = data.rfind(b"\n")
    if cut < 0:
        return [], offset
    chunk = data[: cut + 1]
    return chunk.decode("utf-8", "replace").splitlines(), offset + len(chunk)


def _local_time(iso):
    """`HH:MM:SS` in local time from a UTC ISO timestamp, best effort."""
    try:
        moment = datetime.strptime(iso[:19], "%Y-%m-%dT%H:%M:%S").replace(tzinfo=timezone.utc)
    except ValueError:
        return iso[11:19]
    return moment.astimezone().strftime("%H:%M:%S")


# `text [1793443754555..1793443764431]` is precise and unreadable. The log keeps
# the milliseconds — that is what a database row is matched against — and the band
# shows the clock, which is what a person is looking at.
EPOCH_RANGE = re.compile(r"\[(\d{13})\.\.(\d{13})\]")


def _shorten_epochs(text):
    def repl(match):
        start = datetime.fromtimestamp(int(match.group(1)) / 1000).strftime("%H:%M:%S")
        end = datetime.fromtimestamp(int(match.group(2)) / 1000).strftime("%H:%M:%S")
        return f"[{start}..{end}]"

    return EPOCH_RANGE.sub(repl, text)


def _format(line, label):
    """(sort key, display line) for one log line, or None to drop it."""
    line = ANSI.sub("", line)
    if not line.strip() or "http_server" in line:
        return None
    match = TARGET.match(line)
    if match and label == "server":
        # `2026-10-31T10:05:28.123456Z` — UTC. The sidecar stamps local time, so a
        # merged band has to convert, or the same second shows up eight hours apart
        # (and sorts as if the sidecar ran last night).
        return _local_time(match.group(1)), match.group(4).strip()
    match = SIDECAR.match(line)
    if match:
        stamp = match.group(2)[:8] if match.group(2) else ""
        return stamp, f"sidecar {_shorten_epochs(match.group(3).strip())}"
    return "", line.strip()


def pump(out_path, pattern, sources, limit=200):
    """Merge what is new in each log into one timeline.

    `sources` is a list of `(label, path, offset_path)`. Each log is read from its
    own byte offset and the new lines are sorted by their timestamps before being
    written, so a sidecar line lands between the server lines it happened between
    instead of after all of them.

    Every source is read even when another has nothing new: they advance
    independently, and skipping one would replay its backlog later.
    """
    matcher = re.compile(pattern, re.IGNORECASE)
    fresh = []
    updates = []
    for label, path, offset_path in sources:
        try:
            with open(offset_path, encoding="utf-8") as handle:
                offset = int(handle.read().strip() or "0")
        except (OSError, ValueError):
            offset = 0
        lines, new_offset = _tail(path, offset)
        updates.append((offset_path, new_offset))
        # A line with no timestamp of its own is placed where the previous line of
        # the same file was: the file is written in order, so that is the closest
        # honest answer. "99:99:99" only for a file whose very first lines predate
        # timestamps — sorting those last keeps them from being trimmed away.
        last_stamp = ""
        for line in lines:
            plain = ANSI.sub("", line)
            # A sidecar log can hold lines that are not the sidecar's own: `--help`
            # logs its usage through the same file. Only prefixed lines are events.
            if label == "sidecar" and not SIDECAR.match(plain):
                continue
            if not matcher.search(plain):
                continue
            formatted = _format(line, label)
            if formatted is None:
                continue
            stamp, text = formatted
            if stamp:
                last_stamp = stamp
            else:
                stamp = last_stamp or "99:99:99"
            fresh.append((stamp, len(fresh), text))
    if fresh:
        fresh.sort()
        try:
            with open(out_path, encoding="utf-8") as handle:
                existing = [line.rstrip("\n") for line in handle if line.strip()]
        except OSError:
            existing = []
        existing.extend(f"{stamp}\t{text}" for stamp, _, text in fresh)
        with open(out_path, "w", encoding="utf-8") as handle:
            handle.write("\n".join(existing[-limit:]) + "\n")
    for offset_path, new_offset in updates:
        with open(offset_path, "w", encoding="utf-8") as handle:
            handle.write(str(new_offset))
    return len(fresh)


def main(argv):
    if "--pump" in argv:
        index = argv.index("--pump")
        rest = argv[index + 1 :]
        if len(rest) < 3:
            print(
                "usage: dev-panel.py --pump <out-file> <pattern> <label|log|offset>...",
                file=sys.stderr,
            )
            return 0
        out_path, pattern, specs = rest[0], rest[1], rest[2:]
        sources = []
        for spec in specs:
            parts = spec.split("|")
            if len(parts) != 3:
                print(f"bad --pump source {spec!r}: want label|log|offset", file=sys.stderr)
                return 1
            sources.append(tuple(parts))
        pump(out_path, pattern, sources)
        return 0
    if len(argv) < 4:
        print("usage: dev-panel.py <state-dir> <status.json> <flow-file> [--no-color]", file=sys.stderr)
        return 0
    state_dir, status_path, flow_path = argv[1], argv[2], argv[3]
    paint = Paint("--no-color" not in argv and sys.stdout.isatty())

    roles = read_state(state_dir)
    status = read_status(status_path)
    flow = read_flow(flow_path)

    width = 84
    rule = paint("─" * width, GREY)
    print(paint(" steno dev", BOLD) + paint(f"  {time.strftime('%H:%M:%S')}  ", GREY)
          + paint(f"port {os.environ.get('STENO_HTTP_PORT', '8080')}  Ctrl-C to stop", GREY))
    print(rule)
    print(paint(" SERVICE   STATE     PID     DETAIL", BOLD))
    for name in ROLE_ORDER:
        entry = roles.get(name)
        if entry is None:
            print(f" {paint(name.ljust(8), BOLD)} {paint('not used'.ljust(9), GREY)} "
                  f"{'-'.ljust(7)} {paint(ROLE_DOC[name], DIM)}")
            continue
        print(role_row(paint, name, entry))
    print(rule)
    print(paint(" PIPELINE", BOLD))
    for row in pipeline_rows(paint, status):
        print(row)
    print(rule)
    print(paint(" FLOW", BOLD) + paint("  newest last", DIM))
    if not flow:
        print(paint(" (waiting for the first event: a segment, a phrase, an episode)", DIM))
    for line in flow:
        stamp, _, text = line.partition("\t")
        print(f" {paint(stamp, GREY)} {text}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
