#!/bin/sh
# Builds and installs the Apple speech sidecar.
#
#   ./install.sh                  # installs into /usr/local
#   PREFIX=$HOME/.local ./install.sh
#
# Produces:
#   $PREFIX/lib/steno-stt-sidecar.app   the sidecar bundle
#   $PREFIX/bin/steno-stt-sidecar       stream mode, what `[stt] stream_command` calls
#   $PREFIX/bin/steno-stt-sidecar-file  file mode,   what `[stt] command` calls
#
# The previous release installed `snore-stt-apple`, `snore-stt-stream` and
# `snore-stt-gui`. Those are deliberately not replaced: a stale wrapper pointing
# at a bundle that no longer exists is a confusing failure, and the config now
# names the new commands.
set -eu

cd "$(dirname "$0")"
PREFIX="${PREFIX:-/usr/local}"
APP_DIR="$PREFIX/lib/steno-stt-sidecar.app"
BIN_DIR="$PREFIX/bin"

./build.sh build >/dev/null

mkdir -p "$PREFIX/lib" "$BIN_DIR"
rm -rf "$APP_DIR"
cp -R build/steno-stt-sidecar.app "$APP_DIR"
codesign --force --sign - "$APP_DIR" >/dev/null 2>&1 || true

sed "s|@APP_PATH@|$APP_DIR|" wrappers/steno-stt-sidecar.sh > "$BIN_DIR/steno-stt-sidecar"
chmod +x "$BIN_DIR/steno-stt-sidecar"

sed "s|@APP_PATH@|$APP_DIR|" wrappers/steno-stt-sidecar-file.sh > "$BIN_DIR/steno-stt-sidecar-file"
chmod +x "$BIN_DIR/steno-stt-sidecar-file"

echo "installed $APP_DIR"
echo "installed $BIN_DIR/steno-stt-sidecar"
echo "installed $BIN_DIR/steno-stt-sidecar-file"
echo
echo "Check the device, then run one file through it:"
echo "  $APP_DIR/Contents/MacOS/steno-stt-sidecar --check"
echo "  $BIN_DIR/steno-stt-sidecar-file some.wav"
echo "No permission prompt appears: SpeechAnalyzer runs on-device and does not"
echo "use the Speech Recognition permission."
