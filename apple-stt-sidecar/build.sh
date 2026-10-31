#!/bin/sh
# Builds the Apple speech sidecar as a minimal .app bundle.
#
# The bundle is not here for TCC: `SpeechAnalyzer` is fully on-device and does
# not use the Speech Recognition permission (a bare binary built from
# Sources/ recognises audio with no prompt at all). It is here because the
# server starts the sidecar through LaunchServices (`open -n -a`), which needs
# something to open and which returns immediately instead of blocking until the
# night is over.
#
# Usage: ./build.sh [output-dir]     (default: build/)
set -eu

cd "$(dirname "$0")"
OUTPUT_DIR="${1:-build}"
APP="$OUTPUT_DIR/steno-stt-sidecar.app"
EXECUTABLE="$APP/Contents/MacOS/steno-stt-sidecar"

mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp Info.plist "$APP/Contents/Info.plist"
cp AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"
printf 'APPL????' > "$APP/Contents/PkgInfo"

# Speech is macOS 26+; the target is spelled out so a build on an older SDK
# fails here with a clear message instead of at runtime inside a recognition.
#
# Every file under Sources/ is compiled, so splitting a module means adding a
# file and nothing else. Swift has no ordering requirement at file scope.
SOURCES=$(find Sources -name '*.swift' | sort)

swiftc -O -swift-version 5 \
    -target "$(uname -m)-apple-macos26.0" \
    $SOURCES \
    -o "$EXECUTABLE" \
    -framework Speech \
    -framework SoundAnalysis \
    -framework AVFoundation \
    -framework CoreMedia \
    -framework CoreML \
    -framework Foundation

# Ad-hoc signature: keeps the bundle valid after every rebuild.
codesign --force --sign - "$APP" >/dev/null 2>&1 || true

echo "built $EXECUTABLE"
