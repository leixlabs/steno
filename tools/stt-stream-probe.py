#!/usr/bin/env python3
"""Drive the streaming STT sidecar over a Unix socket with a WAV file.

Used as a stand-in for the Rust client while developing the Swift side: it
exercises exactly the wire format the server will use, so a protocol mismatch
shows up here instead of inside the pipeline.
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

if os.path.exists(SOCK):
    os.unlink(SOCK)

server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
server.bind(SOCK)
server.listen(1)

proc = subprocess.Popen([SIDECAR, SOCK])
server.settimeout(20)
conn, _ = server.accept()
conn.settimeout(30)
print("sidecar connected", flush=True)

events = []


def reader():
    buf = b""
    while True:
        try:
            chunk = conn.recv(65536)
        except socket.timeout:
            break
        if not chunk:
            break
        buf += chunk
        while b"\n" in buf:
            line, buf = buf.split(b"\n", 1)
            if not line.strip():
                continue
            event = json.loads(line)
            events.append(event)
            print(
                f"[{event['type']}] "
                + (event.get("text") or event.get("message") or "")
                + (f" ({event.get('start_ms')}..{event.get('end_ms')})" if event.get("start_ms") else ""),
                flush=True,
            )

thread = threading.Thread(target=reader, daemon=True)
thread.start()

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
    conn.sendall(struct.pack("<Iq", len(chunk), utc_ms) + chunk)
    sent += 1
    # Pace roughly like the real pipeline so the recogniser sees a live stream.
    target = started + (offset // 2) / 16000.0
    delay = target - time.time()
    if delay > 0:
        time.sleep(delay)

print(f"sent {sent} frames", flush=True)
conn.shutdown(socket.SHUT_WR)
thread.join(timeout=15)
try:
    proc.wait(timeout=10)
except subprocess.TimeoutExpired:
    proc.kill()

finals = [e for e in events if e["type"] == "final" and e.get("text")]
print(f"\n{len(finals)} final utterance(s)")
for event in finals:
    print(" -", event["text"])
if os.path.exists(SOCK):
    os.unlink(SOCK)
