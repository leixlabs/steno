#!/usr/bin/env python3
"""stt-check — measure how well the Apple Speech sidecar transcribes known audio.

"识别不准" is not something you can debug by ear. A TTS corpus gives ground
truth, so the *same* sentence can be scored twice:

  file    the sidecar's own --file path (whole WAV at once)  -> the engine's floor
  stream  the production socket path, 20 ms frames, real time -> what we ship

Comparing the two scores separates three causes that look identical in a
transcript:

  * engine limit       file is bad too            -> nothing to fix in our code
  * pipeline defect    file good, stream bad      -> format / framing / pacing
  * acoustics          both bad, clean good       -> the microphone, not us

That last split is why every sentence is scored under two conditions: `clean`
(what `say` produces, i.e. a close mic) and `farfield` (band-limited, quiet, a
noise floor and a little room — what a Pi on a bedside table actually sends).

Usage
  tools/stt-check.py                                  # corpus, clean + farfield
  tools/stt-check.py --conditions clean               # near-field only
  tools/stt-check.py --mode stream                    # production path only
  tools/stt-check.py --replay var/recordings/*.wav    # score real recordings

Exit code is 0 whenever the run completes; the numbers are the output. Nothing
here touches the server's own sidecar, its socket or its database.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import wave
from dataclasses import dataclass, field
from pathlib import Path

# ---------------------------------------------------------------------------
# corpus
# ---------------------------------------------------------------------------

# Deliberately not textbook sentences: numbers, times and drug instructions are
# where a recogniser fails first, and they are exactly what this device is for.
CORPUS = [
    "我昨天晚上三点多醒了一次，后来就睡不太着了。",
    "明天早上七点半叫我起床，别忘了。",
    "我的血压是一百三十九，低压八十七。",
    "这个药一天吃两次，每次一片，饭后服用。",
    "帮我记一下，周四下午两点去医院复查。",
    "今天外面下雨了，出门记得带伞。",
    "空调温度调到二十六度，有点热。",
    "我喉咙有点干，想喝点温水。",
]


DIGITS = {"零": 0, "〇": 0, "一": 1, "二": 2, "两": 2, "三": 3, "四": 4,
          "五": 5, "六": 6, "七": 7, "八": 8, "九": 9}
UNITS = {"十": 10, "百": 100, "千": 1000}
NUMERAL_RUN = re.compile("[零〇一二三四五六七八九十百千两]+")
CLOCK = re.compile(r"(\d{1,2})[:：](\d{2})")
# Filler that carries no content and that a dictation recogniser adds or drops
# at will. Counting it would score the punctuator, not the recogniser.
FILLER = "多啊呢吧"


def _chinese_run(run: str) -> str:
    """一百三十九 → 139, 二十六 → 26, 三 → 3, 一九九八 → 1998."""
    if any(unit in run for unit in UNITS):
        total = 0
        current = 0
        for ch in run:
            if ch in DIGITS:
                current = DIGITS[ch]
            else:
                total += (current or 1) * UNITS[ch]
                current = 0
        return str(total + current)
    return "".join(str(DIGITS[ch]) for ch in run)


def normalize(text: str) -> str:
    """Put a transcript and its reference into the same shape before scoring.

    SFSpeechPunk writes "三点" as "3:00" and "一百三十九" as "139". Those are
    correct outputs, so comparing raw strings measures the formatter instead of
    the recogniser and reports a healthy pipeline as 17% wrong. Numerals and
    clock forms are folded to one canonical form, fillers and punctuation are
    dropped, and only then are characters counted.
    """
    text = NUMERAL_RUN.sub(lambda match: _chinese_run(match.group(0)), text)
    text = CLOCK.sub(lambda match: f"{match.group(1)}点{match.group(2)}", text)
    text = text.replace("点半", "点30")
    text = re.sub(r"点0+", "点", text)  # "3点00" and "3点" are the same time
    return "".join(
        ch for ch in text
        if ch.isalnum() and ch not in FILLER
    )


def edit_distance(reference: str, hypothesis: str) -> int:
    if not reference:
        return len(hypothesis)
    previous = list(range(len(hypothesis) + 1))
    for i, ref_ch in enumerate(reference, start=1):
        current = [i]
        for j, hyp_ch in enumerate(hypothesis, start=1):
            cost = 0 if ref_ch == hyp_ch else 1
            current.append(min(previous[j] + 1, current[j - 1] + 1, previous[j - 1] + cost))
        previous = current
    return previous[-1]


def cer(reference: str, hypothesis: str) -> float:
    ref = normalize(reference)
    hyp = normalize(hypothesis)
    if not ref:
        return 0.0 if not hyp else 1.0
    return edit_distance(ref, hyp) / len(ref)


# ---------------------------------------------------------------------------
# audio
# ---------------------------------------------------------------------------


def ffmpeg(*args: str) -> None:
    result = subprocess.run(
        ["ffmpeg", "-hide_banner", "-loglevel", "error", "-y", *args],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise RuntimeError(f"ffmpeg failed: {result.stderr.strip()}")


def read_pcm(path: Path) -> tuple[list[int], int]:
    with wave.open(str(path), "rb") as handle:
        channels = handle.getnchannels()
        rate = handle.getframerate()
        width = handle.getsampwidth()
        raw = handle.readframes(handle.getnframes())
    if channels != 1 or width != 2:
        raise RuntimeError(f"{path.name}: expected mono s16, got {channels}ch/{width * 8}bit")
    samples = [int.from_bytes(raw[i : i + 2], "little", signed=True) for i in range(0, len(raw), 2)]
    return samples, rate


def write_pcm(path: Path, samples: list[int], rate: int) -> None:
    with wave.open(str(path), "wb") as handle:
        handle.setnchannels(1)
        handle.setsampwidth(2)
        handle.setframerate(rate)
        handle.writeframes(b"".join(s.to_bytes(2, "little", signed=True) for s in samples))


def synth(text: str, path: Path, voice: str) -> None:
    """`say` → 16 kHz mono s16 WAV.

    `say --data-format=LEI16@16000` cannot write that into an AIFF container
    ("fmt?"), so the sample-rate conversion is ffmpeg's job, not `say`'s.
    """
    aiff = path.with_suffix(".aiff")
    subprocess.run(
        ["say", "-v", voice, "-r", "180", "-o", str(aiff), text],
        check=True,
        capture_output=True,
    )
    ffmpeg("-i", str(aiff), "-ar", "16000", "-ac", "1", "-c:a", "pcm_s16le", str(path))
    aiff.unlink(missing_ok=True)


def degrade(source: Path, path: Path, strength: float) -> None:
    """Make clean TTS sound like a bedside microphone.

    Band limit (a cheap electret rolls off hard above ~5 kHz), a 16 dB loss of
    level (the speaker is 1.5 m away), a pink noise floor around -52 dBFS and a
    short room reflection. Not a room simulator — just enough that a corpus
    which passes here will not collapse in the bedroom.
    """
    with wave.open(str(source), "rb") as handle:
        seconds = handle.getnframes() / float(handle.getframerate())
    noise = f"anoisesrc=d={seconds:.3f}:c=pink:a={0.0025 * strength:.4f}"
    graph = (
        f"[0:a]highpass=f=120,lowpass=f=5200,"
        f"volume=-{16 * strength:.1f}dB,"
        f"aecho=0.8:0.6:35|78:0.18|0.09[a];"
        f"[1:a]highpass=f=80,lowpass=f=6000,volume=-{12 * strength:.1f}dB[n];"
        f"[a][n]amix=inputs=2:duration=first:normalize=0,alimiter=limit=0.85[out]"
    )
    ffmpeg(
        "-i", str(source), "-f", "lavfi", "-i", noise,
        "-filter_complex", graph, "-map", "[out]",
        "-ar", "16000", "-ac", "1", "-c:a", "pcm_s16le", str(path),
    )


# ---------------------------------------------------------------------------
# the two paths through the sidecar
# ---------------------------------------------------------------------------


# The sidecar's framing (`crates/server/src/sidecar_wire.rs`).
#
# The direction byte names the *producer*, not the reader: 0x00 is the sidecar's
# own uplink, 0x01 is anything the server sends. This script plays the server, so
# it writes 0x01 and accepts only 0x00 — and the sidecar refuses a session whose
# first frame arrived with the wrong one.
MAGIC = 0x5352
DIR_FROM_SIDECAR = 0x00
DIR_FROM_SERVER = 0x01
TYPE_AUDIO = 0x01
TYPE_SNORE = 0x02
TYPE_TRANSCRIPT = 0x03
TYPE_SESSION = 0x04
TYPE_CONTROL = 0x81
TYPE_CONFIG_FRAME = 0x82


@dataclass
class Result:
    mode: str
    text: str
    finals: list[str] = field(default_factory=list)
    error: str = ""
    seconds: float = 0.0


def recognize_file(app: Path, wav: Path, timeout: float = 120.0) -> Result:
    """File mode via LaunchServices.

    `open` is the only launcher that makes the bundle responsible for its own
    Speech Recognition grant, and it hands nothing back — hence the output path
    the sidecar supports for exactly this case.
    """
    out = wav.with_suffix(".json")
    out.unlink(missing_ok=True)
    started = time.time()
    subprocess.run(
        ["open", "-n", "-W", "-a", str(app), "--args", "--file", str(wav), "--out", str(out)],
        check=False,
        capture_output=True,
        timeout=timeout,
    )
    elapsed = time.time() - started
    if not out.exists():
        return Result("file", "", error="sidecar produced no output file", seconds=elapsed)
    payload = json.loads(out.read_text())
    return Result("file", payload.get("text", ""), seconds=elapsed)


class StreamClient:
    """Speaks the sidecar's socket protocol exactly as the Rust server does.

    `[u16 BE "SR"][u8 direction][u8 type][u32 BE length][payload]`, with audio
    payloads of `[i64 BE utc_ms][s16le]` and JSON for everything else. The server
    also speaks first: the sidecar reads one frame before it starts a session, so
    a client that only listens deadlocks (both ends waiting for the other).
    """

    def __init__(self, app: Path, rate: int, locale: str = "zh-CN") -> None:
        self.app = app
        self.rate = rate
        self.locale = locale
        self.dir = Path(tempfile.mkdtemp(prefix="stt-check-"))
        self.path = self.dir / "stt.sock"
        self.events: list[dict] = []
        self.ready = threading.Event()
        self.done = threading.Event()
        self._listener: socket.socket | None = None
        self._conn: socket.socket | None = None

    def __enter__(self) -> "StreamClient":
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        listener.bind(str(self.path))
        listener.listen(1)
        listener.settimeout(30)
        self._listener = listener
        # `open -n` returns at once and the sidecar connects back after it has
        # passed authorisation, so the accept has to be bounded by hand.
        subprocess.run(
            ["open", "-n", "-a", str(self.app), "--args", "--socket", str(self.path)],
            check=False,
            capture_output=True,
        )
        try:
            self._conn, _ = listener.accept()
        except socket.timeout:
            raise RuntimeError("the sidecar never connected to the socket")
        self._conn.settimeout(0.2)
        threading.Thread(target=self._read, daemon=True).start()
        # The session configuration, and the first frame on the wire. The snore
        # lane is switched off: this measures transcription, and a classifier
        # competing for the same CPU would only add noise to the timing.
        self._send_json(TYPE_CONFIG_FRAME, {"locale": self.locale, "snore": False})
        if not self.ready.wait(timeout=20):
            raise RuntimeError("the sidecar connected but never announced ready")
        return self

    def _read(self) -> None:
        assert self._conn is not None
        buffer = b""
        while True:
            try:
                chunk = self._conn.recv(65536)
            except socket.timeout:
                if self.done.is_set():
                    break
                continue
            except OSError:
                break
            if not chunk:
                break
            buffer += chunk
            while len(buffer) >= 8:
                magic, direction, kind, length = struct.unpack(">HBBI", buffer[:8])
                if magic != MAGIC:
                    self.events.append({"type": "error", "message": f"bad magic 0x{magic:04X}"})
                    return
                if direction != DIR_FROM_SIDECAR:
                    self.events.append({"type": "error", "message": "frame points the wrong way"})
                    return
                if len(buffer) < 8 + length:
                    break
                payload = buffer[8 : 8 + length]
                buffer = buffer[8 + length :]
                self._handle(kind, payload)
        self.ready.set()

    def _handle(self, kind: int, payload: bytes) -> None:
        """Normalises a frame into the flat event dicts the report expects."""
        if kind == TYPE_SESSION:
            event = json.loads(payload)
            state = event.get("state", "")
            self.events.append({**event, "type": state})
            if state == "ready":
                self.ready.set()
        elif kind == TYPE_TRANSCRIPT:
            event = json.loads(payload)
            self.events.append(
                {
                    "type": event.get("kind", "transcript"),
                    "text": event.get("text", ""),
                    "start_ms": event.get("start_ms"),
                    "end_ms": event.get("end_ms"),
                }
            )
        elif kind == TYPE_SNORE:
            self.events.append({**json.loads(payload), "type": "snore"})

    def _send_json(self, kind: int, payload: dict) -> None:
        assert self._conn is not None
        body = json.dumps(payload).encode()
        self._conn.sendall(struct.pack(">HBBI", MAGIC, DIR_FROM_SERVER, kind, len(body)) + body)

    def send(self, samples: list[int], utc_ms: int) -> None:
        assert self._conn is not None
        pcm = b"".join(s.to_bytes(2, "little", signed=True) for s in samples)
        payload = struct.pack(">q", utc_ms) + pcm
        self._conn.sendall(struct.pack(">HBBI", MAGIC, DIR_FROM_SERVER, TYPE_AUDIO, len(payload)) + payload)

    def stream_file(self, wav: Path, realtime: bool, tail_seconds: float) -> Result:
        samples, rate = read_pcm(wav)
        if rate != self.rate:
            raise RuntimeError(f"{wav.name}: {rate} Hz, but the sidecar is fixed at {self.rate} Hz")
        started = time.time()
        base = int(time.time() * 1000)
        frames = [samples[i : i + 320] for i in range(0, len(samples), 320)]
        quiet = [0] * int(self.rate * tail_seconds)
        frames += [quiet[i : i + 320] for i in range(0, len(quiet), 320)]
        for index, frame in enumerate(frames):
            self.send(frame, base + index * 20)
            if realtime:
                target = started + (index + 1) * 0.020
                delay = target - time.time()
                if delay > 0:
                    time.sleep(delay)
        # Trailing silence is what lets the analyser finalise what it heard, so
        # the text arrives as a `final` rather than being flushed by the
        # end-of-audio finalisation when the socket closes.
        deadline = time.time() + 25
        while time.time() < deadline:
            if any(e.get("type") == "final" for e in self.events):
                break
            time.sleep(0.2)
        self.done.set()
        time.sleep(0.5)
        finals = [e.get("text") or "" for e in self.events if e.get("type") == "final"]
        errors = [e.get("message") or "" for e in self.events if e.get("type") == "error"]
        return Result(
            "stream",
            text="".join(finals),
            finals=finals,
            error="; ".join(errors),
            seconds=time.time() - started,
        )

    def __exit__(self, *exc) -> None:
        self.done.set()
        for sock in (self._conn, self._listener):
            try:
                sock and sock.close()
            except OSError:
                pass


# ---------------------------------------------------------------------------
# report
# ---------------------------------------------------------------------------


def bar(value: float) -> str:
    filled = min(10, int(round(value * 10)))
    return "█" * filled + "·" * (10 - filled)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--app", default=os.path.expanduser("~/.local/lib/steno-stt-sidecar.app"))
    parser.add_argument("--voice", default="Tingting", help="say(1) voice; any zh_CN voice works")
    parser.add_argument("--locale", default="zh-CN", help="language sent to the sidecar in its config frame")
    parser.add_argument("--mode", choices=("both", "file", "stream"), default="both")
    parser.add_argument("--conditions", default="clean,farfield", help="comma separated: clean,farfield")
    parser.add_argument("--corpus", help="file with one sentence per line, overrides the built-in corpus")
    parser.add_argument("--replay", nargs="*", help="score real WAV files instead of synthesising speech")
    parser.add_argument(
        "--reference",
        action="append",
        default=[],
        metavar="TEXT",
        help="ground truth for --replay, one per file in order. Without it a real "
             "recording can only be read, not scored — which is the difference "
             "between 'sounds wrong to me' and a number.",
    )
    parser.add_argument("--limit", type=int, default=0, help="stop after N sentences (quick check)")
    parser.add_argument("--keep", action="store_true", help="keep the generated WAVs for listening")
    parser.add_argument("--out", help="write the raw results as JSON")
    args = parser.parse_args()

    app = Path(args.app)
    if not app.is_dir():
        print(f"stt-check: no sidecar bundle at {app}", file=sys.stderr)
        return 2

    work = Path(tempfile.mkdtemp(prefix="stt-check-audio-"))
    conditions = [c.strip() for c in args.conditions.split(",") if c.strip()]

    print(f"sidecar  {app}")
    print(f"workdir  {work}")
    print()

    rows: list[dict] = []
    results: list[dict] = []

    if args.replay:
        # Real audio has no ground truth, so it is scored by reading it: this is
        # the honest check of what the bedroom actually sounds like.
        conditions = ["as-recorded"]
        for index, path in enumerate(args.replay):
            reference = args.reference[index] if index < len(args.reference) else ""
            # Absolute, always: the sidecar runs from LaunchServices with `/` as
            # its working directory, so a relative path becomes `/var/...` and
            # fails as "no such file" inside a bundle whose stderr is invisible.
            rows.append({"label": Path(path).name, "reference": reference, "condition": "as-recorded",
                         "file": None, "stream": None, "wav": str(Path(path).resolve())})
    else:
        lines = CORPUS
        if args.corpus:
            lines = [line.strip() for line in Path(args.corpus).read_text(encoding="utf-8").splitlines() if line.strip()]
        if args.limit:
            lines = lines[: args.limit]
        for index, text in enumerate(lines):
            clean = work / f"{index:02d}-clean.wav"
            synth(text, clean, args.voice)
            for condition in conditions:
                wav = clean
                if condition == "farfield":
                    wav = work / f"{index:02d}-farfield.wav"
                    degrade(clean, wav, 1.0)
                rows.append({"label": f"{index + 1:02d}", "reference": text, "condition": condition,
                             "file": None, "stream": None, "wav": str(wav)})

    for row in rows:
        wav = Path(row["wav"])
        if args.mode in ("both", "file"):
            row["file"] = recognize_file(app, wav)
        if args.mode in ("both", "stream"):
            _, rate = read_pcm(wav)
            with StreamClient(app, rate, locale=args.locale) as client:
                row["stream"] = client.stream_file(wav, realtime=True, tail_seconds=2.0)

    print(f"{'#':>3} {'cond':<12} {'file':>14} {'stream':>14}  sentence")
    print("-" * 96)
    for row in rows:
        has_ref = bool(row["reference"])

        def cell(result: Result | None) -> str:
            if result is None:
                return "-"
            if result.error:
                return "err"
            if not has_ref:
                return "n/a"
            return f"{cer(row['reference'], result.text) * 100:5.1f}%"

        ref = row["reference"] or "(recorded audio — no ground truth)"
        print(f"{row['label']:>3} {row['condition']:<12} {cell(row['file']):>14} {cell(row['stream']):>14}  {ref}")
        if not has_ref:
            # Nothing to compare against, so the transcript itself is the result.
            for mode in ("file", "stream"):
                result = row[mode]
                if result is None:
                    continue
                if result.error:
                    print(f"{'':>4} {mode} err→ {result.error}")
                else:
                    print(f"{'':>4} {mode}  → {result.text or '(empty)'}")
            continue
        if row["file"] and not row["file"].error and cer(row["reference"], row["file"].text) > 0.001:
            print(f"{'':>31} file→  {row['file'].text}")
        if row["stream"] and row["stream"].error:
            print(f"{'':>31} stream err→ {row['stream'].error}")
        if row["stream"] and not row["stream"].error and cer(row["reference"], row["stream"].text) > 0.001:
            print(f"{'':>31} stream→ {row['stream'].text}")
        results.append(
            {
                "label": row["label"],
                "condition": row["condition"],
                "reference": row["reference"],
                "file": {"text": row["file"].text, "cer": cer(row["reference"], row["file"].text)} if row["file"] else None,
                "stream": {"text": row["stream"].text, "cer": cer(row["reference"], row["stream"].text)} if row["stream"] else None,
                "wav": row["wav"],
            }
        )

    print()
    print("summary")
    print("-" * 96)
    scored = [r for r in results if r["reference"]]
    for mode in ("file", "stream"):
        for condition in conditions:
            subset = [r for r in scored if r["condition"] == condition and r[mode]]
            if not subset:
                continue
            values = [r[mode]["cer"] for r in subset]
            mean = sum(values) / len(values)
            exact = sum(1 for v in values if v == 0)
            label = f"{condition}/{mode}"
            print(f"  {label:<22} {mean * 100:5.1f}%  {bar(mean)}  exact {exact}/{len(values)}")

    # The diagnosis, not just the score: which of the three causes is it?
    def mean_cer(mode: str, condition: str) -> float | None:
        values = [r[mode]["cer"] for r in scored if r["condition"] == condition and r[mode]]
        return sum(values) / len(values) if values else None

    clean_file = mean_cer("file", "clean")
    clean_stream = mean_cer("stream", "clean")
    far_file = mean_cer("file", "farfield")
    if clean_file is not None and clean_stream is not None:
        gap = clean_stream - clean_file
        print()
        if gap <= 0.05:
            print("  pipeline: the stream path matches the file path — framing, rate and pacing are fine.")
        else:
            print(f"  pipeline: the stream path is {gap * 100:.1f} points worse than the file path on identical audio.")
            print("            That is our bug, not the engine's: check rate, frame size and endpointing.")
        if far_file is not None and clean_file is not None and far_file - clean_file > 0.15:
            print(f"  acoustics: far-field costs {100 * (far_file - clean_file):.1f} points of character error.")
            print("            Gain, band limit and the noise floor of the node's microphone matter more than the engine.")

    if args.keep:
        print(f"\n  kept audio in {work}")
    if args.out:
        Path(args.out).write_text(json.dumps(results, ensure_ascii=False, indent=1))
        print(f"  wrote {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
