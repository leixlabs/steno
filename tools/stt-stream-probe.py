#!/usr/bin/env python3
"""Drive the streaming STT sidecar over a Unix socket with a WAV file.

This is the server's stand-in: it speaks exactly the framing the Rust side speaks
(`crates/server/src/sidecar_wire.rs`), so a protocol mismatch shows up here
instead of inside the pipeline.

    [u16 BE magic "SR"][u8 direction][u8 msg type][u32 BE length][payload]

Server -> sidecar: 0x01 audio, 0x81 control, 0x82 config
Sidecar -> server: 0x02 snore, 0x03 transcript, 0x04 session

Audio payloads are `[i64 BE utc_ms][s16le mono PCM]`; everything else is JSON.
"""
import json
import os
import socket
import struct
import subprocess
import sys
import threading
import time
import wave

WAV = next((arg for arg in sys.argv[1:] if not arg.startswith("--")), "/tmp/long.wav")
SOCK = "/tmp/steno-stt-test.sock"
SIDECAR = os.environ.get(
    "STENO_STT_SIDECAR", os.path.expanduser("~/.local/bin/steno-stt-sidecar")
)
FRAMES_PER_PACKET = 320  # 20 ms at 16 kHz

MAGIC = 0x5352
DIR_SIDECAR_TO_SERVER = 0x00
DIR_SERVER_TO_SIDECAR = 0x01
TYPE_AUDIO = 0x01
TYPE_SNORE = 0x02
TYPE_TRANSCRIPT = 0x03
TYPE_SESSION = 0x04
TYPE_CONTROL = 0x81
TYPE_CONFIG = 0x82

NAMES = {
    TYPE_AUDIO: "audio",
    TYPE_SNORE: "snore",
    TYPE_TRANSCRIPT: "transcript",
    TYPE_SESSION: "session",
}


def encode(direction, msg_type, payload=b""):
    return struct.pack(">HBBI", MAGIC, direction, msg_type, len(payload)) + payload


def recv_exactly(conn, count):
    data = b""
    while len(data) < count:
        chunk = conn.recv(count - len(data))
        if not chunk:
            return None
        data += chunk
    return data


def reader(conn, events):
    while True:
        header = recv_exactly(conn, 8)
        if header is None:
            print("sidecar closed the stream", flush=True)
            return
        magic, direction, msg_type, length = struct.unpack(">HBBI", header)
        if magic != MAGIC:
            print(f"bad magic 0x{magic:04X}", flush=True)
            return
        if direction != DIR_SIDECAR_TO_SERVER:
            print(f"frame points the wrong way: {direction}", flush=True)
            return
        payload = recv_exactly(conn, length) if length else b""
        if payload is None:
            return
        if msg_type == TYPE_TRANSCRIPT:
            event = json.loads(payload)
            print(
                f"[{event['kind']}] {event.get('text', '')}"
                + (
                    f" ({event.get('start_ms')}..{event.get('end_ms')})"
                    if event.get("start_ms") is not None
                    else ""
                ),
                flush=True,
            )
            events.append(event)
        elif msg_type == TYPE_SNORE:
            event = json.loads(payload)
            print(
                f"[snore] {event['label']} {event['start_ms']}..{event['end_ms']} "
                f"peak {event['peak']:.2f} over {event['windows']} windows",
                flush=True,
            )
            events.append(event)
        elif msg_type == TYPE_SESSION:
            event = json.loads(payload)
            detail = {k: v for k, v in event.items() if k != "state"}
            print(f"[session:{event['state']}] {json.dumps(detail, ensure_ascii=False)}", flush=True)
            events.append(event)
        else:
            print(f"[{NAMES.get(msg_type, msg_type)}] {len(payload)} bytes", flush=True)


if os.path.exists(SOCK):
    os.unlink(SOCK)

server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
server.bind(SOCK)
server.listen(1)

proc = subprocess.Popen([SIDECAR, "--mode", "socket", "--path", SOCK])
server.settimeout(20)
conn, _ = server.accept()
conn.settimeout(30)
print("sidecar connected", flush=True)

events = []
thread = threading.Thread(target=reader, args=(conn, events), daemon=True)
thread.start()

# The session configuration travels before any audio: the sidecar waits for its
# first frame to learn the language, exactly as the server does it.
conn.sendall(
    encode(
        DIR_SERVER_TO_SIDECAR,
        TYPE_CONFIG,
        json.dumps({"locale": os.environ.get("STENO_STT_LOCALE", "zh-CN")}).encode(),
    )
)

with wave.open(WAV, "rb") as wav:
    assert wav.getframerate() == 16000, wav.getframerate()
    assert wav.getnchannels() == 1
    frames = wav.readframes(wav.getnframes())

base_utc_ms = int(time.time() * 1000)
step = FRAMES_PER_PACKET * 2
sent = 0
started = time.time()
for offset in range(0, len(frames) - step + 1, step):
    chunk = frames[offset : offset + step]
    utc_ms = base_utc_ms + (offset // 2) * 1000 // 16000
    conn.sendall(encode(DIR_SERVER_TO_SIDECAR, TYPE_AUDIO, struct.pack(">q", utc_ms) + chunk))
    sent += 1
    # Real time: the analyser is tuned for a live stream, and a burst would only
    # measure how fast the machine can queue audio.
    target = started + (offset // 2) / 16000.0
    delay = target - time.time()
    if delay > 0:
        time.sleep(delay)

print(f"sent {sent} packets ({len(frames) / 32000:.1f} s of audio)", flush=True)

# Ask for a clean finish, then let the shutdown path run. The server does this
# before closing the socket so the last phrase is not lost.
conn.sendall(encode(DIR_SERVER_TO_SIDECAR, TYPE_CONTROL, json.dumps({"action": "stop"}).encode()))
time.sleep(3)
try:
    conn.shutdown(socket.SHUT_WR)
except OSError:
    pass

thread.join(timeout=15)
try:
    proc.wait(timeout=10)
except subprocess.TimeoutExpired:
    proc.kill()

finals = [e for e in events if e.get("kind") == "final" and e.get("text")]
snores = [e for e in events if "windows" in e]
print(f"\n{len(finals)} final utterance(s), {len(snores)} snore episode(s)")
for event in finals:
    print(" -", event["text"])
for event in snores:
    print(f" - snore {event['start_ms']}..{event['end_ms']} peak {event['peak']:.2f}")
if os.path.exists(SOCK):
    os.unlink(SOCK)
