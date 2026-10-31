#!/bin/sh
# steno-stt-sidecar — attach the sidecar to the server's Unix socket.
#
# Returns immediately: the sidecar stays alive for the whole session and talks
# to the server over the socket it is handed, so there is nothing to wait for
# and no stdio to hand back. That "start it and forget it" shape is why the
# launch goes through LaunchServices.
set -eu

APP="@APP_PATH@"
if [ ! -d "$APP" ]; then
    echo "steno-stt-sidecar: bundle not found at $APP" >&2
    exit 2
fi

if [ "$#" -lt 1 ]; then
    echo "usage: steno-stt-sidecar <socket-path>" >&2
    exit 2
fi

exec open -n -a "$APP" --args --socket "$1"
