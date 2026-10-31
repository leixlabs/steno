#!/usr/bin/env python3
"""Replay a WAV file into the UDP capture protocol, at real time.

This is the hardware-free way to exercise the server: it produces exactly the
packets `steno-node` would, so segment rotation, gap handling, detection and
transcription all run against a known input.
"""
import socket
import struct
import sys
import time
import wave

MAGIC = 0x534E
VERSION = 1
KIND_HELLO = 1
KIND_PCM = 2

HEADER = "<HBBIIIQQ"          # 32 bytes
HELLO = "<IHBB32s"            # 40 bytes
SAMPLES_PER_PACKET = 320      # 20 ms at 16 kHz


def header(kind, device_id, session_id, seq, first_frame, t_mono_us):
    return struct.pack(HEADER, MAGIC, VERSION, kind, device_id, session_id,
                       seq, first_frame, t_mono_us)


def main():
    path = sys.argv[1]
    host, port = sys.argv[2].split(":")
    device_id = int(sys.argv[3]) if len(sys.argv) > 3 else 1

    with wave.open(path) as wav:
        if wav.getframerate() != 16000 or wav.getnchannels() != 1:
            raise SystemExit("need 16 kHz mono")
        pcm = wav.readframes(wav.getnframes())

    samples = struct.unpack("<%dh" % (len(pcm) // 2), pcm)
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    target = (host, int(port))
    session_id = 0x5A5A
    name = b"replay"

    sock.sendto(header(KIND_HELLO, device_id, session_id, 0, 0, 0)
                + struct.pack(HELLO, 16000, 1, 1, 0, name), target)

    started = time.monotonic()
    seq = 0
    first = 0
    while first < len(samples):
        chunk = samples[first:first + SAMPLES_PER_PACKET]
        t_mono_us = int((first / 16000.0) * 1_000_000)
        sock.sendto(header(KIND_PCM, device_id, session_id, seq, first, t_mono_us)
                    + struct.pack("<%dh" % len(chunk), *chunk), target)
        first += len(chunk)
        seq += 1
        # Pace against the wall clock so the server sees a realtime stream.
        target_time = started + first / 16000.0
        delay = target_time - time.monotonic()
        if delay > 0:
            time.sleep(delay)
    print("sent %d packets, %.2f s of audio" % (seq, len(samples) / 16000.0))


if __name__ == "__main__":
    main()
