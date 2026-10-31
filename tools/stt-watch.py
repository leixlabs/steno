#!/usr/bin/env python3
"""stt-watch — watch what the server is recognising, live, without the browser.

This is the "skip the web" path. It talks to the running server, not to the
sidecar: text comes from the same WebSocket the Portal uses
(`/api/v1/transcript`), the level and the counters from `/api/v1/status`. That
matters because those are the *decisions the server made* — a final here is a
phrase that exists in the database, and a snore event here is a row.

```sh
tools/stt-watch.py                            # http://127.0.0.1:8080
tools/stt-watch.py --server http://127.0.0.1:8081
tools/stt-watch.py --json | jq -c '.type'     # raw NDJSON for another program
```

What it shows:

  * a level bar, updated from `/status` — the answer to "is the microphone
    still hearing anything", which no transcript can give you;
  * partials replaced in place, finals appended with their wall clock;
  * snore episodes as they are stored, with their confidence;
  * failures and `lagged` notices, instead of a transcript with a silent hole.

Exit code is 0 unless the server never answered at all.
"""

from __future__ import annotations

import argparse
import base64
import json
from datetime import datetime
import os
import socket
import ssl
import sys
import threading
import time
import urllib.error
import urllib.request

RESET = "\033[0m"
BOLD = "\033[1m"
DIM = "\033[2m"
RED = "\033[31m"
GREEN = "\033[32m"
YELLOW = "\033[33m"
CYAN = "\033[36m"
GREY = "\033[90m"


# ---------------------------------------------------------------------------
# the server, as two small clients
# ---------------------------------------------------------------------------


class Status:
    """`/api/v1/status`, polled. The only thing here that is not a WebSocket."""

    def __init__(self, base: str, interval: float) -> None:
        self.url = base.rstrip("/") + "/api/v1/status"
        self.interval = interval
        self.data: dict | None = None
        self.error = ""
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._run, daemon=True)

    def start(self) -> None:
        self._thread.start()

    def stop(self) -> None:
        self._stop.set()

    def _run(self) -> None:
        while not self._stop.is_set():
            try:
                with urllib.request.urlopen(self.url, timeout=1) as response:
                    self.data = json.load(response)
                self.error = ""
            except (urllib.error.URLError, TimeoutError, ValueError) as error:
                self.error = f"{error}"
            self._stop.wait(self.interval)

    # -- readings ----------------------------------------------------------

    def level(self) -> tuple[float, float] | None:
        capture = (self.data or {}).get("capture") or {}
        peak = capture.get("level_peak_dbfs")
        rms = capture.get("level_rms_dbfs")
        if peak is None or rms is None:
            return None
        return float(peak), float(rms)

    def stt(self) -> dict:
        return (self.data or {}).get("stt") or {}

    def counter(self, key: str, default: int = 0) -> int:
        value = self.stt().get(key)
        return int(value) if isinstance(value, (int, float)) else default


class TranscriptSocket:
    """A minimal WebSocket client for `/api/v1/transcript`.

    Hand-rolled because this tool has to run on a machine where nothing is
    installed: no `websockets`, no `websocket-client`, no pip. It only ever
    reads text frames (plus the ping/pong and close handshake), which is all that
    endpoint sends.
    """

    def __init__(self, base: str) -> None:
        scheme, rest = base.split("://", 1)
        host_port, _, _ = rest.partition("/")
        host, _, port = host_port.partition(":")
        self.host = host or "127.0.0.1"
        self.port = int(port or (443 if scheme == "https" else 80))
        self.tls = scheme == "https"
        self.sock: socket.socket | None = None

    def connect(self, timeout: float = 5.0) -> None:
        raw = socket.create_connection((self.host, self.port), timeout=timeout)
        if self.tls:
            raw = ssl.create_default_context().wrap_socket(raw, server_hostname=self.host)
        key = base64.b64encode(os.urandom(16)).decode()
        request = (
            "GET /api/v1/transcript HTTP/1.1\r\n"
            f"Host: {self.host}:{self.port}\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\n"
            "Sec-WebSocket-Version: 13\r\n"
            "\r\n"
        )
        raw.sendall(request.encode())
        raw.settimeout(timeout)
        header = b""
        while b"\r\n\r\n" not in header:
            chunk = raw.recv(4096)
            if not chunk:
                raise ConnectionError("the server closed during the handshake")
            header += chunk
        status_line = header.split(b"\r\n", 1)[0]
        if b"101" not in status_line:
            raise ConnectionError(f"no websocket upgrade: {status_line.decode(errors='replace')}")
        raw.settimeout(0.1)
        self.sock = raw

    def close(self) -> None:
        if self.sock is not None:
            try:
                self.sock.close()
            except OSError:
                pass
            self.sock = None

    def _send_frame(self, opcode: int, payload: bytes) -> None:
        assert self.sock is not None
        mask = os.urandom(4)
        masked = bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))
        header = bytes([0x80 | opcode])
        length = len(payload)
        if length < 126:
            header += bytes([0x80 | length])
        elif length < 65536:
            header += bytes([0x80 | 126]) + length.to_bytes(2, "big")
        else:
            header += bytes([0x80 | 127]) + length.to_bytes(8, "big")
        self.sock.sendall(header + mask + masked)

    def _read_exactly(self, count: int) -> bytes | None:
        assert self.sock is not None
        data = b""
        while len(data) < count:
            try:
                chunk = self.sock.recv(count - len(data))
            except socket.timeout:
                return None  # nothing yet; the caller loops
            if not chunk:
                return b""
            data += chunk
        return data

    def next_event(self) -> dict | None:
        """One decoded text frame, or None when the socket went away."""
        while True:
            head = self._read_exactly(2)
            if head is None:
                return {}
            if head == b"":
                return None
            opcode = head[0] & 0x0F
            masked = head[1] & 0x80
            length = head[1] & 0x7F
            if length == 126:
                extended = self._read_exactly(2)
                if not extended:
                    return None
                length = int.from_bytes(extended, "big")
            elif length == 127:
                extended = self._read_exactly(8)
                if not extended:
                    return None
                length = int.from_bytes(extended, "big")
            mask = self._read_exactly(4) if masked else b""
            if masked and not mask:
                return None
            body = self._read_exactly(length) if length else b""
            if body is None:
                continue
            if body == b"" and length:
                return None
            if masked:
                body = bytes(byte ^ mask[index % 4] for index, byte in enumerate(body))

            if opcode == 0x8:  # close
                return None
            if opcode == 0x9:  # ping
                self._send_frame(0xA, body)
                continue
            if opcode == 0xA:  # pong
                continue
            if opcode in (0x1, 0x2):
                try:
                    return json.loads(body.decode("utf-8", "replace"))
                except ValueError:
                    return {}


# ---------------------------------------------------------------------------
# rendering
# ---------------------------------------------------------------------------


def bar(value: float, low: float, high: float, width: int = 18) -> str:
    if high <= low:
        return ""
    fraction = (value - low) / (high - low)
    filled = max(0, min(width, int(round(fraction * width))))
    return "█" * filled + "░" * (width - filled)


class Screen:
    """Events scroll; one live line at the bottom is redrawn in place.

    Deliberately not a full-screen TUI: with one live line there is no cursor
    arithmetic to get wrong, it degrades to plain lines when stdout is not a
    terminal, and it survives the terminal being resized mid-session.
    """

    def __init__(self, color: bool, tty: bool, width: int) -> None:
        self.color = color and tty
        self.tty = tty
        self.width = width
        self.live_len = 0

    def paint(self, text: str, colour: str = "") -> str:
        if not self.color or not colour:
            return text
        return f"{colour}{text}{RESET}"

    def announce(self, text: str) -> None:
        print(self.paint(text, DIM))

    def event(self, stamp: str, kind: str, text: str, colour: str = "") -> None:
        line = f" {self.paint(stamp, GREY)} {self.paint(kind.ljust(6), colour)} {text}"
        if self.tty:
            sys.stdout.write("\r\033[K" + line + "\n")
        else:
            print(line)
        sys.stdout.flush()

    def live(self, line: str) -> None:
        if not self.tty:
            return
        padded = line[: self.width].ljust(self.live_len)
        self.live_len = len(padded)
        sys.stdout.write("\r\033[K" + padded)
        sys.stdout.flush()


def clock(ms: int | None) -> str:
    if not isinstance(ms, (int, float)):
        return "--:--:--"
    return time.strftime("%H:%M:%S", time.localtime(ms / 1000.0))


# ---------------------------------------------------------------------------
# main loop
# ---------------------------------------------------------------------------


def render_live(screen: Screen, status: Status, partial: str, note: str) -> None:
    """One line at the bottom: level, counters, and the phrase being formed.

    Composed right-to-left in importance: the partial gets whatever room the level
    leaves, and when there is none the counters go instead of being truncated
    mid-number. RMS is deliberately not here — the peak is the headline, and both
    numbers are in `--once`.
    """
    level = status.level()
    if level is None:
        meter = screen.paint("level — no block yet", DIM)
    else:
        peak, _rms = level
        meter = f"level {screen.paint(bar(peak, -80, 0, 16), cyan_for(peak))} {peak:6.1f} dB"

    stt = status.stt()
    counters = (
        f"{stt.get('state', '?'):<7}"
        f"f{status.counter('finals_total')} s{status.counter('snore_events_total')}"
        f" d{status.counter('dropped_blocks')}"
    )

    tail = partial or note
    room = screen.width - len(meter) - len(counters) - 6
    if tail and room > 10:
        line = f" {meter}  {counters}  {screen.paint('▸ ' + tail[:room], BOLD)}"
    elif tail:
        # No room for the counters: the live phrase is worth more than the counts.
        room = screen.width - len(meter) - 4
        line = f" {meter}  {screen.paint('▸ ' + tail[:max(0, room)], BOLD)}"
    else:
        line = f" {meter}  {screen.paint(counters, DIM)}"
    screen.live(line)


def cyan_for(peak: float) -> str:
    if peak >= -1:
        return RED      # clipping
    if peak >= -40:
        return GREEN
    if peak >= -70:
        return YELLOW
    return GREY


def main() -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("--server", default=os.environ.get("STENO_SERVER", "http://127.0.0.1:8080"))
    parser.add_argument("--interval", type=float, default=0.2, help="status poll interval, seconds")
    parser.add_argument("--json", action="store_true", help="print raw NDJSON instead of a screen")
    parser.add_argument(
        "--once",
        action="store_true",
        help="print one snapshot of level + counters and exit (no WebSocket)",
    )
    parser.add_argument("--no-color", action="store_true")
    parser.add_argument("--width", type=int, default=0, help="override the terminal width")
    args = parser.parse_args()

    tty = sys.stdout.isatty() and not args.json
    # `os.get_terminal_size()` answers 0 columns on a pty that never had a window
    # size set (a pipe under `script`, a detached tmux pane). A width of 0 would
    # silently truncate every live line to nothing, so 0 means "unknown".
    try:
        columns = os.get_terminal_size().columns
    except OSError:
        columns = 0
    width = args.width or columns or 100
    screen = Screen(color=not args.no_color, tty=tty, width=width)

    status = Status(args.server, args.interval)
    status.start()

    if args.once:
        # A snapshot, for scripts and for "is it even up": one line, no stream.
        for _ in range(15):
            if status.data or status.error:
                break
            time.sleep(0.2)
        if not status.data:
            print(f"stt-watch: {args.server} did not answer: {status.error}", file=sys.stderr)
            return 3
        level = status.level()
        stt = status.stt()
        if level is None:
            meter = "level unknown (no block dispatched yet)"
        else:
            meter = f"level {bar(level[0], -80, 0)} peak {level[0]:6.1f} dBFS rms {level[1]:6.1f}"
        print(f"{args.server}  {meter}")
        print(
            f"stt {stt.get('state', '?')}  {stt.get('locale', '?')}  {stt.get('engine', '?')}"
            f"  finals {status.counter('finals_total')}"
            f"  snores {status.counter('snore_events_total')}"
            f"  dropped {status.counter('dropped_blocks')}"
        )
        return 0

    if tty:
        print(
            screen.paint(" steno-stt-watch", BOLD)
            + screen.paint(f"  {args.server}  ", GREY)
            + screen.paint("Ctrl-C to stop", GREY)
        )
        print(screen.paint("─" * min(width, 100), GREY))

    partial = ""
    note = ""
    sock = TranscriptSocket(args.server)
    failures = 0
    deadline = time.time() + 10
    connected_once = False

    while True:
        try:
            if sock.sock is None:
                sock.connect()
                failures = 0
                if not connected_once:
                    connected_once = True
                else:
                    screen.event(time.strftime("%H:%M:%S"), "ws", "reconnected", GREEN)
                note = "connected, waiting for events"
            event = sock.next_event()
            if event is None:
                raise ConnectionError("server closed the transcript stream")
            if event == {}:
                # Nothing arrived in this second: refresh the live line and try
                # again, so the level keeps moving while nobody is talking.
                render_live(screen, status, partial, note)
                continue

            kind = event.get("type")
            if args.json:
                print(json.dumps(event, ensure_ascii=False), flush=True)
            elif kind == "ready":
                screen.event(
                    time.strftime("%H:%M:%S"),
                    "ready",
                    f"engine {event.get('engine')}  {event.get('finals_total')} phrase(s) so far",
                    GREEN,
                )
                note = ""
            elif kind == "partial":
                partial = event.get("text") or ""
            elif kind == "final":
                partial = ""
                started = event.get("started_at_utc")
                stamp = clock(_iso_ms(started))
                screen.event(stamp, "final", event.get("text") or "", BOLD)
            elif kind == "snore":
                started = clock(event.get("start_ms"))
                seconds = (event.get("end_ms", 0) - event.get("start_ms", 0)) / 1000.0
                screen.event(
                    started,
                    "snore",
                    f"{event.get('label')}  {seconds:.1f}s  peak {event.get('peak', 0):.2f}"
                    f"  mean {event.get('mean', 0):.2f}  {event.get('windows')} window(s)",
                    CYAN,
                )
            elif kind == "error":
                screen.event(time.strftime("%H:%M:%S"), "error", event.get("message") or "", RED)
            elif kind == "state":
                screen.event(
                    time.strftime("%H:%M:%S"),
                    "state",
                    event.get("state") or "",
                    YELLOW if event.get("state") != "ready" else GREEN,
                )
            elif kind == "lagged":
                screen.event(
                    time.strftime("%H:%M:%S"),
                    "lagged",
                    f"this client fell behind by {event.get('missed')} events; the middle is missing",
                    RED,
                )
            render_live(screen, status, partial, note)

        except KeyboardInterrupt:
            break
        except (ConnectionError, OSError) as error:
            failures += 1
            sock.close()
            if not connected_once and time.time() > deadline:
                print(f"stt-watch: {args.server} did not answer: {error}", file=sys.stderr)
                return 3
            note = f"reconnecting ({error})"
            render_live(screen, status, partial, note)
            time.sleep(min(0.5 * failures, 5.0))

    status.stop()
    sock.close()
    if tty:
        sys.stdout.write("\n")
    return 0


def _iso_ms(text: str | None) -> int | None:
    """RFC3339 → epoch milliseconds, for the event clock."""
    if not text:
        return None
    value = text.replace("Z", "+00:00")
    try:
        return int(datetime.fromisoformat(value).timestamp() * 1000)
    except ValueError:
        return None


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        sys.exit(0)
