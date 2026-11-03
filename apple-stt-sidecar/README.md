# apple-stt-sidecar

Audio in, text and snore annotations out. One process, two lanes, no policy.

This is the macOS analysis backend for the recorder. The node captures, the
server orchestrates, this program classifies and recognises — it decides nothing
about when a night starts, when a phrase ends, or what to do about silence.

```
  external audio                                   ┌───────────────┐
  ┌──────────────┐   ┌─────────────────┐          │ SoundAnalysis │──► snore episode
  │ microphone   │   │ socket / stdio  │          │ (classifier)  │
  │ (AVAudioEngine)  │ (framed PCM)    │          └───────┬───────┘
  └──────┬───────┘   └────────┬────────┘                  │
         └───────► AudioHub ◄─┘  one 16 kHz mono buffer    │
                      │  (one conversion, one clock)      │
                      │                                   │
                      └───────► recogniser ───────────────┼──► text (partial/final)
                        SpeechAnalyzer │ audiocpp         │
                        (both lanes see the same block)
```

## Why a hub

Every input narrows to the same thing before anything analyses it: 16 kHz, mono,
signed 16-bit. One `AVAudioConverter`, one `Timeline`, and both consumers see the
identical buffer with the identical frame range. Two paths that resampled
separately would disagree about what "here" means, and a snore annotation that
points at the wrong second of a night is worse than no annotation.

`Sources/` is that split:

| File | What it owns |
|---|---|
| `AudioHub.swift` | conversion to the canonical format, fan-out, one `Timeline` |
| `Timeline.swift` | frame ↔ wall clock, sample-exact, re-anchors across gaps |
| `Backend.swift` | the recogniser contract, `--backend` selection, `--check` dispatch |
| `SpeechAnalyzerBackend.swift` | Apple's lane: one `SpeechAnalyzer` session, locale/assets, presets |
| `AudioCPPBackend.swift` | audio.cpp's lane: one `audiocpp` process, Qwen3-ASR windows, stdin PCM |
| `SnoreDetector.swift` | `SNAudioStreamAnalyzer`, hysteresis, episode aggregation |
| `Wire.swift` | the 8-byte framing and both directions' messages |
| `Session.swift` | pipeline assembly, `TranscriptBridge` (frames → wall clock), the framed loop |
| `Capture.swift` | microphone, finished file, raw PCM on stdin |
| `Emitters.swift` | wire frames, and text/json/jsonl/srt for a terminal |
| `Options.swift` | the command line, refused loudly when wrong |

## Backends

Recognition is one implementation of `TranscriptionBackend` (`Backend.swift`).
Two backends ship:

| `--backend` | engine | result granularity |
|---|---|---|
| `apple` (default) | `SpeechTranscriber` / `SpeechAnalyzer`, on-device, macOS 26+ | one phrase, timed |
| `audiocpp` | [audio.cpp](https://github.com/ggml-org/audiocpp) running a Qwen3-ASR GGUF package | one fixed audio window, `Timing.window(N)` |

The seam is deliberately narrow, because that is what makes a second
implementation cheap:

- a backend gets audio in the hub's format and returns **frame ranges**, never
  `utc_ms`. `TranscriptBridge` owns the only `Timeline`, so a backend cannot
  disagree with the snore lane about when a block happened — and a new one needs
  no clock of its own;
- it never sees the socket, the log or the snore lane. `ready` carries its own
  `engine` and resolved `language`; the bridge adds the lane;
- `capabilities` says what it can promise. `partials` is on or off; `timing` is
  `phrase` for Apple and `window(seconds:)` for a chunked engine, where the
  wire's `start_ms` is the edge of a window and not the first syllable. Saying
  which one it is in the `--check` report and the startup log is the difference
  between a coarse annotation and an invented one.

A final carries a range by construction, so "text with no clock" is not
expressible — which is the point. The other way to get it wrong fails out loud: a
flag another engine does not understand (`--preset` for audio.cpp, `--model` for
Apple) is refused at parse time instead of being accepted and ignored.

Selection is local, not on the wire: switching engines means different models on
disk, so `--backend` belongs where the sidecar is launched. On a server
deployment that is the wrapper — `install.sh` puts `steno-stt-sidecar-audiocpp`
next to the Apple one, and `[stt] stream_command` names whichever should run. In
development it is `tools/dev.sh --backend audiocpp` (`--model` once; the path is
remembered, and `STENO_STT_*` still works underneath). The `engine` field the
sidecar reports takes precedence over `[stt] engine` in the config, so a stored
row names the engine that actually produced it.

### audio.cpp / Qwen3-ASR

```sh
# once: a GGUF package (0.6B q8 ≈ 1.15 GB; 1.7B q8 ≈ 2.5 GB)
audiocpp_model_manager install qwen3_asr_0_6b_q8_0 --models-dir ~/models

# build/refresh the sidecar and record which package the audiocpp wrapper loads
PREFIX=$HOME/.local STENO_STT_MODEL=~/models/Qwen3-ASR-0.6B-GGUF ./install.sh
# then, in the server config:
#   [stt] stream_command = "/Users/you/.local/bin/steno-stt-sidecar-audiocpp"

# or straight from the command line, no server involved
steno-stt-sidecar --backend audiocpp --model ~/models/Qwen3-ASR-0.6B-GGUF \
                  --chunk-seconds 5 --mode file some-16k.wav
```

`AudioCPPBackend` keeps **one** `audiocpp` process for the whole session and
writes raw s16le PCM into its stdin; the engine answers with one line per decoded
window. A warm start costs ~2.5 s before the first decode (Metal init plus the
GGUF load), which is why there is one process per session and not one per chunk.

What the engine's streaming mode actually does — measured on audio.cpp 0.9.1,
Apple M4, Qwen3-ASR-0.6B q8, 5 s windows:

| observation | consequence here |
|---|---|
| text arrives per window, ~1.3 s after the boundary | `final` per window, `partials: false` |
| a window of silence prints nothing | empty windows produce no row, and line count ≠ window count |
| `text_output=` at EOF repeats the whole session | drained and dropped; per-window lines are what is stored |
| `--audio-chunk-mode vad` is refused in streaming ("requires offline --task vad") | `fixed` windows, `--chunk-seconds` |
| closing stdin decodes the tail and exits | `finish` drains the writer, then closes stdin |
| ~1.3 s of decode per 5 s window (RTF ≈ 0.26) | keeps up with realtime all night; 34 windows / 170 s overnight soak, RSS flat |

Two things this engine cannot do, said out loud instead of faked:

- **no partials.** Text exists only when a window closes, so `control: flush` has
  nothing to commit and logs that once per session.
- **no sub-window timing.** `start_ms`/`end_ms` are the edges of a window on the
  engine's own grid (`k*N`), widened to the newest audio the engine had been
  handed when the text arrived. That end can overshoot by the decode latency, so
  consecutive windows overlap slightly; a range that contains the speech is
  honest for `Timing.window`, one that excludes it would point at silence.

Language is the locale's primary subtag (`zh-CN` → `zh`, `yue-Hant` → `yue`) and
is checked against the family's list, because the engine accepts any string —
`--language xx-YY` still produces a transcript. `--locale auto` omits the flag and
lets the model detect.

The wrapper keeps its package path in
`$PREFIX/lib/steno-stt-sidecar-audiocpp.conf`, which `install.sh` writes only when
given `STENO_STT_MODEL` and otherwise leaves alone. That is deliberate: a plain
reinstall (`refresh-all.sh` runs one with no environment) must not erase a working
model path — the first version had the path substituted into the wrapper itself,
and one such reinstall turned a working recogniser into `launcher exited with exit
status: 2`. `STENO_STT_MODEL` / `STENO_STT_CHUNK` still override at launch.

## Wire

Both directions on one connection — Unix socket, or stdin/stdout under
`--mode stdio`:

```
[u16 BE "SR"] [u8 direction] [u8 type] [u32 BE length] [payload]
```

| dir | type | payload |
|---|---|---|
| up | `0x01` | `[i64 BE utc_ms][s16le mono 16 kHz PCM]` |
| up | `0x02` | JSON: `start_ms`, `end_ms`, `label`, `peak`, `mean`, `windows`, `threshold`, `end_reason` |
| up | `0x03` | JSON: `kind` (`final`/`partial`), `text`, `start_ms`, `end_ms`, `engine`, `language` |
| up | `0x04` | JSON: `state` (`ready`/`status`/`error`/`stopped`), `engine`, `locale`, `snore` |
| down | `0x81` | JSON: `{"action":"stop"\|"flush"\|"status"}` |
| down | `0x82` | JSON: `locale`, `preset`, `snore`, `snore_threshold`, `snore_window` |

An 8-byte header plus an explicit length means a stream that arrives split
(half a frame) or coalesced (three frames in one read) still decodes exactly:
the reader asks for 8 then exactly `len`. The direction byte is validated, so a
desynchronised stream fails with a named error instead of being read as payload.

`utc_ms` lives in the payload because the header has no room for it and because
it is the only thing that makes a result placeable: both lanes work on their own
zero-based audio timeline, and the recorder is the only party that knows what
time it was. The sidecar never invents a timestamp — if the wire skips audio, the
map re-anchors instead of padding silence that was never recorded.

The `config` frame arrives before the first audio block: the language is the
orchestrator's decision, so a sidecar started without `--locale` listens for what
the server says rather than for a built-in default.

## Modes

| command | what it does |
|---|---|
| `--mode socket --path <p>` | connect, framed duplex, until the peer closes or says `stop` |
| `--mode stdio` | the same protocol on stdin/stdout |
| `--mode mic [--seconds N]` | the default input device (the one mode that prompts for permission) |
| `--mode file <audio> [--format text\|json\|jsonl\|srt]` | one finished file |
| `--stream [--sample-rate N] [--channels N]` | raw PCM on stdin, no framing: `ffmpeg … \| steno-stt-sidecar --stream` |
| `--check` | JSON: locales, assets, formats, classifier labels |

Two flags control what you can see, and neither changes the wire:

| flag | effect |
|---|---|
| `--log-text` | writes each finalised phrase into the log as `text [start_ms..end_ms] …`. Off by default: the log is `/tmp/steno-stt-sidecar.log` and a transcript does not belong in a world-readable file. When it is on the file is created 0600. `tools/dev.sh` passes it, so `tools/dev.sh logs sidecar` and the console panel's FLOW band show what it heard. |
| `--verbose` | per-block detail (window confidences, result ranges, timeline discontinuities) |

Every log line is stamped `HH:mm:ss.SSS` (local time) after the process prefix, so
the file can be read next to the server's UTC log — `tools/dev.sh` merges the two
into one timeline and converts both to local time.

Snore episodes are always logged — an annotation is not the transcript, and "did
the classifier fire at all" is the first question a quiet night raises. To watch a
*live* session without reading logs, use the server's own tap:
`tools/stt-watch.py` (WebSocket + `/api/v1/status`), which is where the parsed,
stored results are.
| `--install-assets` | download the on-device model for `--locale` |

Shorthands `--socket <path>` and `--file <audio>` are kept for the wrappers.

`--preset progressiveTranscription|transcription` picks the reporting style. Both
map onto **time-indexed** presets, because only those carry `.audioTimeRange`:

```
progressiveTranscription            fastResults, volatileResults
timeIndexedProgressiveTranscription fastResults, volatileResults, audioTimeRange
```

A transcript with no clock is not worth storing, so the un-indexed variants are
never selected.

## The snore lane

Apple's built-in classifier (`.version1`) has 303 labels and one of them is
`snoring`; `breathing` and `cough` are there too. No custom model is needed.
`--snore-model` accepts a `.mlmodelc`/`.mlmodel` if one ever is.

Windows overlap by 50 %, so three seconds of snoring arrives as two or three
results about the same breath. Consecutive windows above `--snore-threshold` are
merged into one episode, which is emitted **once, when it ends**; an episode that
runs longer than 30 s is cut into successive rows rather than becoming one
impossible eight-hour event.

The default threshold is 0.5 because of a measurement, not a preference: across
the 40 ESC-50 *snoring* clips in `testdata/steno-eval`, the best window per clip
scored 0.63 … 0.98 (median 0.87). A 5 s clip rarely fills a 3 s window with
snoring, so the per-window score sits well below the clip peak — 0.7 would have
missed 10 of those 40.

That is a **candidate** threshold, not an accuracy claim. Nothing here has been
measured against a negative (non-snore) corpus, so no precision figure exists yet;
`docs/verification/` stays empty on purpose rather than holding a guess. What the
server stores is tagged `source='apple-soundanalysis-v1'` so these rows can never
be confused with the `rule-v1` rows the server's own detector writes for the same
audio.

## Build and install

```sh
./build.sh                        # build/steno-stt-sidecar.app
PREFIX=$HOME/.local ./install.sh  # lib/steno-stt-sidecar.app + bin wrappers
```

Requires macOS 26 and Xcode 26 (`SpeechAnalyzer`, `SpeechTranscriber`,
`SoundAnalysis`, `AssetInventory`). Run `--check` on a new machine before trusting
a night to it.

| Path | What it is |
|---|---|
| `$PREFIX/lib/steno-stt-sidecar.app` | the bundle; what `open -n -a` launches |
| `$PREFIX/bin/steno-stt-sidecar` | stream mode: `<socket>`, returns immediately |
| `$PREFIX/bin/steno-stt-sidecar-file` | file mode: `<wav>`, prints one `Transcript` |

`[stt] stream_command` points at the second, `[stt] command` (segment mode) at the
third. The annotated template is `deployment/config.example.toml`.

The bundle is not here for TCC: `SpeechAnalyzer` and the sound classifier are
on-device and use no permission. It exists because the server launches the sidecar
through LaunchServices (`open -n -a`), which needs something to open and which
returns immediately instead of blocking until the night is over. `--mode mic` is
the exception — a microphone *is* a permission, and the bundle carries
`NSMicrophoneUsageDescription` for it.

## Debugging

Diagnostics go to stderr *and* `/tmp/steno-stt-sidecar.log` (`STENO_STT_LOG`
overrides). Under `open -a` stderr is swallowed, so the file is the only copy.
`--verbose` adds per-block detail (window confidences, results, timeline
discontinuities); off by default, because a night is 180 000 blocks.

`tools/stt-stream-probe.py` plays the server's half of the protocol against a
sidecar and a WAV file, so a framing change can be tested without the pipeline:

```sh
STENO_STT_SIDECAR=apple-stt-sidecar/build/steno-stt-sidecar.app/Contents/MacOS/steno-stt-sidecar \
  python3 tools/stt-stream-probe.py /tmp/speech16k.wav
```

A run with no text has exactly three causes worth checking, in this order:

1. `--check` says the locale is unsupported or the asset is missing;
2. `/tmp/steno-stt-sidecar.log` shows a `results stream failed` line;
3. the server never sent audio (see `dropped_stt_blocks` in `/status`).
