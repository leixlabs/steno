# apple-stt-sidecar

Audio in, JSON out. One process, one session, no policy.

This is the macOS speech backend for the recorder: the server pushes 16 kHz mono
PCM at it over a Unix socket and reads one JSON object per line back. Everything
that used to live here and was *not* a conversion — endpoint detection, task
rotation, re-recognition of the audio Apple refused to finalise, a decode
dashboard — is gone, because `SpeechAnalyzer` does not need it.

## Wire

```
server -> sidecar : [u32 LE length][i64 LE utc_ms][s16le mono 16 kHz PCM]
sidecar -> server : {"type":"ready"} | {"type":"partial"} | {"type":"final"} | {"type":"error"}
```

`utc_ms` is the wall clock of the frame's first sample, stamped by the recorder.
The sidecar never invents a timestamp: `Result.range` is on the analyser's audio
timeline, and `Anchors` maps that timeline back to the recorder's clock. If the
wire skips audio (dropped block, node reconnect), the map re-anchors instead of
padding silence that was never recorded.

A final carries `text`, `start_ms`, `end_ms`, `segments` (reserved: empty today),
`engine` and `language`.

## Modes

| command | what it does |
|---|---|
| `--socket <path>` | connect and stream events until the socket closes |
| `--file <audio> [--out <json>]` | transcribe one finished file, print/emit one `Transcript` object |
| `--check` | JSON: availability, supported/installed locales, asset status, audio formats |
| `--install-assets` | download the on-device model for `--locale` |

`--locale` defaults to `zh-CN`, `--engine` to `apple-speechanalyzer` (the label
written into every row, so a mixed history stays readable).

## Why no permissions

`SpeechAnalyzer` / `SpeechTranscriber` are on-device and do not use the Speech
Recognition permission; a bare binary built from `Sources/` recognises audio with
no prompt. The bundle exists only so `open -n -a` has something to launch and
returns immediately — the server starts the sidecar and does not wait for the
night to end.

## Build and install

```sh
./build.sh                      # build/steno-stt-sidecar.app
PREFIX=$HOME/.local ./install.sh  # lib/steno-stt-sidecar.app + bin wrappers
```

Requires macOS 26 and Xcode 26 (`SpeechAnalyzer`, `SpeechTranscriber`,
`AssetInventory`). Run `--check` on a new machine before trusting a night to it.

| Path | What it is |
|---|---|
| `$PREFIX/lib/steno-stt-sidecar.app` | the bundle; what `open -n -a` launches |
| `$PREFIX/bin/steno-stt-sidecar` | stream mode: `<socket>`, returns immediately |
| `$PREFIX/bin/steno-stt-sidecar-file` | file mode: `<wav>`, prints one `Transcript` |

`[stt] stream_command` points at the second, `[stt] command` (segment mode) at the
third. The annotated template is `deployment/config.example.toml`.

## Debugging

Diagnostics go to stderr *and* `/tmp/steno-stt-sidecar.log` (`STENO_STT_LOG`
overrides). Under `open -a` stderr is swallowed, so the file is the only copy.

A run with no text has exactly three causes worth checking, in this order:

1. `--check` says the locale is unsupported or the asset is missing;
2. `/tmp/steno-stt-sidecar.log` shows a `results stream failed` line;
3. the server never sent audio (see `dropped_stt_blocks` in `/status`).
