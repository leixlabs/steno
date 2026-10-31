#!/bin/sh
# steno-stt-sidecar-file — transcribe one finished WAV and print the result.
#
# Used by `[stt] mode = "segment"`: the server wants "run a command, read one
# JSON object from stdout". `open -W` gives us no stdout, so the sidecar writes
# its JSON to a file and this wrapper echoes it back.
set -eu

APP="@APP_PATH@"
if [ ! -d "$APP" ]; then
    echo "steno-stt-sidecar-file: bundle not found at $APP" >&2
    exit 2
fi

if [ "$#" -lt 1 ]; then
    echo "usage: steno-stt-sidecar-file <audio-file>" >&2
    exit 2
fi

OUT="$(mktemp -t snore-stt).json"
trap 'rm -f "$OUT"' EXIT

if ! open -n -W -a "$APP" --args --file "$1" --out "$OUT" ; then
    echo "steno-stt-sidecar-file: failed to launch the sidecar bundle" >&2
    exit 3
fi

if [ ! -s "$OUT" ]; then
    echo "steno-stt-sidecar-file: no transcript produced (see /tmp/steno-stt-sidecar.log)" >&2
    exit 4
fi

cat "$OUT"
