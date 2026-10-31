# steno — whole-night audio recorder with snore annotations

A bedside capture device streams PCM to a server. The server records it losslessly
as WAV, cuts it into segments, marks **candidate** snore events with a rule-based
detector and transcribes segments to text. Recordings are kept under a byte budget
with oldest-first reclamation, and everything is reviewed in a browser Portal.

Two things about the shape of the product matter when reading the code:

1. **It records all night, not only the snoring.** Segmentation is purely
   duration-driven (`audio.segment_duration_seconds`, default 1 h); the detector
   only annotates. Nothing in the record path consults the detector.
2. **Transcription covers every completed segment**, not only segments that
   contain events. `storage::segments_awaiting_transcript` filters on
   `status='complete'` and duration only.

So this is a recorder that produces annotations, not a classifier that produces
clips. Keep that framing when changing behaviour or writing docs.

The end-to-end picture — deployment, the audio path, and every reader and writer
of the database — is in [Runtime architecture](#runtime-architecture), including a
table that maps each box to the file that owns it.

### Naming

Everything an operator types or configures is **steno**: crates
`steno-lib` / `steno-server` / `steno-node`, binaries `steno-monitor` /
`steno-node` / `steno-eval`, the sidecar `steno-stt-sidecar` and
`steno-stt-sidecar-file` (bundle `steno-stt-sidecar.app`), units
`steno-monitor.service` / `steno-node.service`, paths `/etc/steno-monitor`,
`/var/lib/steno-monitor`, `/mnt/steno-monitor`, the `STENO_*` environment
variables, and `/tmp/steno-stt-sidecar.log`. The rename from `snore-*` is done;
do not reintroduce the old prefix.

What is deliberately still spelled `snore` is the *signal*, not the product:
`snore_events`, `ReviewStatus::Snore` / `NotSnore`, the `snore` / `not_snore`
review values, and "snore band" in DSP prose. Keeping the table name avoids a
schema migration that would buy nothing; the detector still marks snore
candidates, the product is just not only about them.

Consequence for an existing deployment: nothing migrates itself. The unit names,
the service user, `/etc/snore-monitor` → `/etc/steno-monitor`, the SQLite path
under `/var/lib/...`, the fstab mount `/mnt/snore-monitor` → `/mnt/steno-monitor`
and the installed `snore-stt-*` wrappers all have to be moved once, by hand.

## Non-negotiables

These are enforced by the code, and a change that breaks one is a bug even if the
tests pass.

- **Code is the source of truth.** `docs/` holds dated records of decisions; when
  a record disagrees with the code, the code is right and the record needs a
  correction note. Do not "restore" behaviour to match an older doc.
- **No silent fallback.** Unknown config keys, unparsable values, unsupported
  sample formats and unsupported rates all fail startup with the offending key
  named. The only documented fallbacks are the capture negotiation (configured
  rate → 48 kHz → 44.1 kHz, configured channels → mono) and the
  `web_dir`-empty → `../web` compatibility guess in `main::web_root`.
- **Never invent a value.** Unknown state is reported as `null` / `unknown`
  (`/status`, event features, `time_quality`). Do not substitute a plausible
  number, a zero, or a default to make a field look complete.
- **Events are candidates, not a diagnosis.** The detector is `rule-v1`: band
  energy plus noise-floor margins, no ML. No accuracy claim is valid without
  labelled nights; `steno-eval` exists so that claim can be measured instead of
  asserted.
- **No fabricated measurements.** `docs/verification/acceptance-report.md` and
  `docs/verification/spike-pi.md` contain "待实测" placeholders on purpose. Fill them with real
  numbers or leave them empty.
- **Audio and transcripts are private, medical-adjacent data.** The HTTP API has
  no authentication and no TLS; `server.bind_address` defaults to `127.0.0.1`.
  The one endpoint that changes machine state (`POST /api/v1/control`, which runs
  `systemctl`) is off by default and is only accepted with a loopback bind.
  Never default it to a wider address, never add a "convenience" proxy, and never
  log samples, transcripts or file contents.

## Repository map

Directory names are short (`lib`, `server`, `node`), so the package names carry the
prefix: `cargo -p steno-server` is the server, and `use steno_lib::…` is the core.

```
crates/lib            platform-neutral core (package `steno-lib`); no storage, HTTP or process concerns
  config              the tuning contract + every validation rule (§11)
  dsp, resampler      DC block, band-pass, framing, noise-floor estimator, polyphase resampler
  detector            candidate state machine, `DetectionPipeline`, `DETECTOR_VERSION`
  timeline            monotonic clock origin, clock map, gap records, time quality
  audio_capture       CaptureRing (the one producer contract) + CPAL device selection
  protocol            node↔server UDP wire format, shared by both binaries
  bounded_queue       SPSC ring + audio/control slot split
  metrics, error      counters shared with the HTTP layer; `Error::Db` behind `feature = "db"`
  detect_wav, eval    offline analysis, behind `feature = "tools"`
crates/server         recording, detection, storage, transcription, HTTP + Portal
  dispatcher          the only place that decides when a segment rotates
  recorder, storage   WAV writing/recovery, SQLite schema v3
  retention, recovery capacity reclamation, startup reconciliation
  db_writer           the single writer thread; one rusqlite connection
  net_capture         UDP receive half of the split deployment
  stt, stt_stream     per-segment and streaming transcription workers
  control             `systemctl start/stop/restart` for the two units; off by default, loopback-only
  http_server         Axum REST API + static Portal
  main.rs             runtime assembly and shutdown order
crates/node           capture-only binary (package `steno-node`): microphone → UDP. No disk, no DB, no listener
web                   Vite + React 19 + TS control centre (build output `web/dist`, not committed)
apple-stt-sidecar  macOS SpeechAnalyzer sidecar: PCM in, JSON out (.app bundle + wrappers)
tools/dev.sh          dev environment: Vite HMR + rebuild-and-restart for the server
tools/refresh-all.sh  rebuild + redeploy every runtime, then restart and verify
tools/run-server.sh   run the server locally with a throwaway runtime dir
tools/deploy-node.sh  cross-build + install the capture node on a Pi in one shot
tools/replay-wav.py   replays a WAV into the UDP protocol at real time
tools/stt-stream-probe.py  drives the streaming sidecar's socket without the server
docs/                 spec/ (original), decisions/ (dated), operations/, verification/
deployment/           everything a machine needs installed: systemd units + the two
                      annotated example configs (config.example.toml, node.example.toml)
testdata/steno-eval   ESC-50 clip corpus + labels.json (untracked, 17 MB)
.agents/skills/land   the landing workflow (commit → rebase → verify → push)
```

The crate split is load-bearing, not cosmetic: the node package must not link
`rusqlite`, `axum` or `tokio`, because it is what runs on the bedside Pi. If a
change to `steno-lib` needs the database, it goes behind `feature = "db"`.

## Build and verify

Requires Rust with edition 2024 support (1.85+), plus ALSA headers because `cpal`
is always compiled:

```sh
sudo apt-get install -y build-essential pkg-config libasound2-dev   # Debian/Pi
cargo build --workspace                 # debug
cargo build --release --workspace
cargo test --workspace --all-features   # 194 passing, 1 ignored doctest
cargo clippy --workspace --all-targets --all-features
```

Feature flags are not optional in practice:

- The offline binaries are `required-features = ["tools"]`. `cargo build --bin
  detect-wav` **fails** without `--features tools` (or `--all-features`), and
  `cargo test -p steno-lib` alone skips `tests/detect_wav.rs` and
  `tests/steno_eval.rs`. Use `--workspace --all-features` for a full run.
- `feature = "db"` only enables `Error::Db`; the server turns it on.

Current state (last verified run): `195 tests, 194 passed, 0 failed, 1 ignored`
(the ignored one is the illustrative doc-test at the top of `http_server.rs`).
**Clippy is not clean**: it currently reports warnings in all three crates
(`derivable_impls` for `SourceKind`, a stale `&` in `http_server`, `while_let_loop`
in `net_capture`, several `manual_is_multiple_of`). Fix-and-keep-clean is the
intent; do not claim "clippy clean" as a gate until it actually is.

`cargo fmt --check` is **not** a gate: the tree is not rustfmt-clean and was never
committed that way. Format only the files you touch.

Portal:

```sh
cd web && npm install && npm run build    # tsc -b && vite build → web/dist
```

The UI is served from `server.web_dir` (default `web/dist`, relative to the
process CWD; empty disables the UI and leaves the JSON API). It is deliberately
not embedded in the binary — a copy change must not trigger a Rust rebuild.

### Repo hygiene before committing

`.gitignore` covers `target/`, `*.db*`, `*.wav.partial`, `.env*` — but **not**
`web/node_modules/` (68 MB, 2450 files) and **not** `testdata/` (17 MB of audio).
Both are untracked today and `git add -A` will happily commit them. Stage paths
explicitly, or add the ignores first.

## Run and deploy

Two scripts cover the loop; both are the documented path, so keep them working.

**While developing** (frontend and backend, one command, one Ctrl-C):

```sh
tools/dev.sh                 # Vite :5173 proxying /api + server :8080, both hot
tools/dev.sh --stt on        # ...with live transcription
tools/dev.sh --port 8081     # a second instance beside the first
```

`web/src/**` reloads through Vite HMR. `crates/**/*.rs` triggers `cargo build`,
then SIGTERM the old process and start the new one — but only after the build
succeeds, so a syntax error leaves the running server alone instead of taking the
page you were looking at off the air. It uses the same `var/` runtime directory as
a normal run, which means it takes the port over from a detached instance; on exit
`tools/refresh-all.sh --no-node` puts that instance back.

**Everything at once** (after changing code, this is the only command needed):

```sh
tools/refresh-all.sh                 # Portal + server + Apple sidecar + Pi node + restart + verify
tools/refresh-all.sh --no-node       # local only, no ssh
tools/refresh-all.sh --dry-run       # print the plan
```

It builds before it stops anything, so the interruption is only the restart. The
Pi step is allowed to fail without aborting the local refresh (a reachable server
with the new Portal is still worth having); the exit code is 1 if any step failed
or the server did not answer `/api/v1/status` within 30 s. `--stt on|off` and
`--control on|off` patch that one `enabled` key in place rather than
regenerating the config, which would also reset the port and the segment length.

**Server, locally** (throwaway runtime dir, nothing under `/etc` or `/var`):

```sh
tools/run-server.sh                 # [source] kind = "net", HTTP on 127.0.0.1:8080
tools/run-server.sh --lan           # reachable from a phone on the LAN
tools/run-server.sh --alsa          # open this machine's microphone instead
tools/run-server.sh --release --reset-config
```

It generates `var/server.toml` (repo-local, git-ignored — **not** `$TMPDIR`,
which macOS purges, and this directory holds real recordings) on first run,
builds `web/dist` if missing (otherwise the Portal silently degrades to
API-only), and `exec`s `cargo run -p steno-server` with `STENO_MONITOR_CONFIG`
set — so Ctrl-C runs the real shutdown sequence. Note `storage.recording_dir` is
the *parent*: files land in `var/recordings/recordings/YYYY/MM/DD/`.

**Capture node, one shot**:

```sh
tools/deploy-node.sh --host 192.168.31.58        # defaults: user pi, server = this Mac's LAN IP
tools/deploy-node.sh --host pi.local --enable    # also start at boot
tools/deploy-node.sh --no-build                  # reinstall the existing cross build
```

It cross-builds with `cargo-zigbuild` against `~/armhf-sysroot`, refuses an
artifact whose newest `GLIBC_` symbol is above 2.28, then creates the
`steno-node` account, **adds it to `audio`** (without that, ALSA exposes only the
`null` device and the node looks healthy while recording nothing), installs the
binary/config/unit, restarts, and prints the journal line that proves capture
starts. `alsa_device` is preserved from the device unless `--alsa-device` is
passed, because cpal matches the device *description* string.

Permanent deployment stays `deployment/` + `docs/operations/2026-10-29-pi-deploy.md`; these
scripts are for the edit → deploy → watch loop.

## Runtime architecture

One drawing. The vertical axis is the job: audio in, text out — capture,
segmentation, recognition. What the system keeps (WAV, events, transcripts) and
what it serves from that store is the consequence at the bottom, not a second
main line. Each label names the module or component that owns that step; the
box-to-file map is at the end of the section.

```
 BEDSIDE (Pi)                        SERVER HOST — steno-monitor (one process)
 ────────────                        ───────────────────────────────────────

 1  CAPTURE
      mic → steno-node ── UDP 8091 (32 B hdr + ≤1280 B PCM, no retx) ─┐
      mic → ALSA                                                      ┴─► CaptureRing

 2  SEGMENT AND FAN OUT
      CaptureRing ──► dispatcher ──┬──► recorder ──► .partial ──► *.wav
                     rotates on    ├──► detector (rule-v1) ──► candidate events
                     stored frames └──► stt queue (lossy, optional)

 3  RECOGNISE                  (the main line leaves the process here)
      stt queue ──► stt_stream ──Unix socket──► steno-stt-sidecar.app
                                                          │ SpeechAnalyzer
                                                          │ on-device models
                                                          ▼
                                        text (partial / final) ──► utterances

 4  WHAT IS KEPT               (a side effect of the main line)
      disk   *.wav under storage.recording_dir
      db     app.db (WAL, v3): segments, gaps, events, transcripts
                                        │
                                        ├─► HTTP :8080 ─► Portal ─► browser
                                        ├─► stt worker (mode="segment") ──► transcripts
                                        └─► retention (startup) ──► oldest *.wav
```

Band 4 is what the system keeps, not what it does: recordings, events and
transcripts exist so a night can be reviewed later. The product is the first
three bands.

The node owns the microphone and nothing else, so a dropped uplink costs audio —
recorded as a gap — instead of stalling capture. `source.kind = "alsa"` runs the
same pipeline with the node removed: the server opens the microphone itself and
the entry point becomes the CPAL callback. (On macOS that path is CoreAudio; the
key name is historical.)

The dispatcher is fed by one queue and fans out to three. None of them may block
it: a full recording queue drops the block and registers a `queue_overflow` gap
(and withholds it from detection so the detector sees the same hole), a full
detection queue drops its copy and restarts the detector, and transcription is
lossy on purpose.

`stt_stream` is the socket **server** — it binds, the sidecar connects, and the
text comes back over the same socket. The sidecar is an external `.app`, launched
with `open -n -a` through `stt.stream_command` and re-launched 5 s after a
session ends. It holds no policy: `SpeechAnalyzer` finalises its own results as
audio flows, so there is no rotation, no endpoint timer and no re-decode of
audio Apple refused to finalise — that whole layer was deleted with
`SFSpeechRecognizer`. Recognition is on-device and needs no permission grant.
Setting `mode = "segment"` swaps the socket for a one-shot `stt.command`.

The one-writer rule is about the audio path, not the file: `DbWriter` owns the
connection used by recorder and detector, while HTTP, both STT workers and the
startup retention pass each open their own — `rusqlite::Connection` is not
`Sync`.

### Where each box lives

| Box | Code |
|---|---|
| node: microphone → UDP | `crates/node/src/main.rs` |
| `net_capture` (UDP in, jitter buffer, re-anchor) | `crates/server/src/net_capture.rs` |
| `CaptureRing` + CPAL device selection | `crates/lib/src/audio_capture.rs` |
| dispatcher | `crates/server/src/dispatcher.rs` |
| recorder (`.partial` → `.wav`) | `crates/server/src/recorder.rs` |
| detector | `crates/lib/src/detector.rs`, with `dsp.rs` and `resampler.rs` |
| `DbWriter` | `crates/server/src/db_writer.rs` |
| SQLite schema and queries | `crates/server/src/storage.rs` |
| `stt_stream` / `stt` (in-process workers) | `crates/server/src/stt_stream.rs` / `crates/server/src/stt.rs` |
| `steno-stt-sidecar.app` (external process; `SpeechAnalyzer`, no TCC grant) | `apple-stt-sidecar/Sources/*.swift`, built by `build.sh`, installed by `install.sh` |
| wrappers on `PATH` (`steno-stt-sidecar`, `steno-stt-sidecar-file`) | `apple-stt-sidecar/wrappers/*.sh` |
| speech models (system assets, not ours) | `AssetInventory`-managed on-device models, reached through `SpeechAnalyzer` |
| HTTP API + static Portal | `crates/server/src/http_server.rs` |
| control endpoints (`systemctl` start/stop/restart) | `crates/server/src/control.rs`, routed from `http_server.rs` |
| Portal UI | `web/src/` (`App.tsx`, `api.ts`, `styles.css`) |
| recovery / retention | `crates/server/src/recovery.rs` / `crates/server/src/retention.rs` |
| startup, shutdown, wiring | `crates/server/src/main.rs` |
| UDP wire format (both ends) | `crates/lib/src/protocol.rs` |

Ownership rules that must not drift:

- **`CaptureRing::callback_push(&[i16], CaptureMeta)` is the one producer
  contract.** ALSA and UDP both use it; everything downstream is source-agnostic.
  A new source belongs at that seam, not inside the dispatcher.
- **The dispatcher owns every segmentation decision.** Rotation is by stored
  frame count (never wall clock), and each forced rotation carries one of
  `duration | gap | time_step | format_change | size_limit | shutdown | error`,
  mirrored into `recording_segments.end_reason`. Recorder and detector only react
  to `DispatcherMessage`. Note the ordering bug already fixed once: check frames
  first, bytes as a fallback, or every normal rotation gets labelled `size_limit`.
- **`segment_offset_frames` is annotated by the dispatcher**, together with
  `gap_before_frames`/`gap_kind`. Nothing downstream recomputes offsets.
- **Queues split audio slots from control slots** (`BoundedQueue`): audio pushes
  fail loudly when full; control messages (`SegmentStart`, `Gap`, `SegmentEnd`)
  ride 16 reserved slots and are never dropped. A full control slot means the
  consumer is stuck → mark degraded, never block the dispatcher.
- **Backpressure policy**: recording queue full → drop the block, register a
  `queue_overflow` gap, and withhold that block from detection too so the
  detector sees the same hole. Detection queue full → drop the detection copy,
  count it, restart the detector. Transcription is lossy on purpose: it must
  never slow recording down.
- **One writer for SQLite on the audio path.** `DbWriter` owns the connection
  used by recorder and detector. `http_server`, both STT workers and the startup
  retention pass open their own to the same file for their own reads and writes.
  rusqlite connections are not `Sync` — do not share one.
- **Startup order**: open DB → migrate → open HTTP DB → start DB writer →
  recovery scan → capacity check → build queues/workers → dispatcher → STT →
  capture → HTTP. Recovery runs before capture so it is the only writer of those
  paths.
- **Shutdown order** (`audio.shutdown_timeout_seconds`, default 10 s): drop the
  capture handle → `dispatcher.request_shutdown()` → close both queues → join
  recorder (finishes the segment with `end_reason=shutdown`) → join detector
  (flushes pending events) → drain the DB writer → join STT → join HTTP. On
  timeout the `.partial` is deliberately left for the recovery scan; do not
  "fix" that by force-completing it.
- **A missing microphone is a state, not a crash.** With no usable device the
  process starts `serve_without_capture`: HTTP and Portal come up, recording is
  `stopped`, microphone is `disconnected`, and `/status` says why.

## Storage and data model

SQLite schema is version 3, migrated idempotently through `PRAGMA user_version`
(`Database::migrate`). A database newer than the binary is refused, not
downgraded. Tables:

| Table | Holds |
|---|---|
| `recording_segments` | one row per WAV: start/end, frames, bytes, `status`, `end_reason`, gap/xrun counters, `time_quality` |
| `audio_gaps` | every registered hole: offset, lost frames, kind, `estimate_source` |
| `snore_events` | candidate events with feature columns, `review_status`, `continued`, `detector_version` |
| `segment_transcripts` | one transcript per segment (primary key `segment_id`); `status` `ok`/`failed` |
| `utterances` | streaming-recogniser phrases with absolute times (schema v3) |

Segment status vocabulary: `recording`, `complete`, `interrupted`, `missing`,
`deleted`. `interrupted` comes from a repaired `.partial`; `missing` means the row
promises audio that is gone (it does not auto-clear — that is intentional).

On disk, below `storage.recording_dir`:

```
<recording_dir>/recordings/YYYY/MM/DD/<UTC-start>_<segment-id>.wav
```

Note the doubled level: `segment_relative_path` always prefixes `recordings/`, so
`recording_dir = "/mnt/steno-monitor/recordings"` (as in `deployment/config.example.toml`)
actually produces `.../recordings/recordings/2026/...`. The DB stores the path
relative to `recording_dir`, and the audio endpoint joins it after
`validate_relative_path` rejects absolute paths and `..`. Do not build playback
paths from request strings.

During recording a segment is `<name>.wav.partial` with a live header (patched
and `fdatasync`ed every `audio.header_flush_interval_seconds`); it is renamed to
`.wav` only after the header is final and synced. `recovery::scan` repairs
`.partial` files whose header/length agree, marks them `interrupted`, reports
orphan `.wav` files (no row) and `missing` rows (no file), and leaves a
`.partial` it cannot verify exactly as it is — never deleted, never published.

Retention deletes the **oldest complete** segments until the total is at or below
`recording_cleanup_target_bytes`, never touching the open segment and never
deleting event rows. Below `recording_reserve_bytes` of free space it stops
recording and raises a warning instead of filling the disk.

**Correction (code wins over this paragraph's earlier claim):** the intent was
three triggers — segment completion, the startup scan, and every
`storage.retention_check_interval_seconds` — and the key is still parsed and
validated, but the only wired call today is the startup pass in `main::run`. Do
not describe periodic reclamation as working; wire the missing triggers rather
than documenting them as present.

## Capture sources and the wire protocol

`source.kind` selects one of two sources:

- `alsa` (default): this process opens the microphone. Device selection matches
  `audio.alsa_device` against the CPAL device **description string**
  (`Device::to_string()`), not a PCM name; empty means "there must be exactly
  one candidate", otherwise startup fails with `MultipleDevices`. Negotiation
  tries the configured rate, then 48 kHz, then 44.1 kHz, S16 only — a device
  that only offers F32 (e.g. a Mac's built-in mic) fails by design.
- `net`: the server never opens a microphone. It declares the layout in `[audio]`
  and refuses a node that announces anything else, rather than reinterpreting it.

UDP wire format (`steno-lib::protocol`, little-endian, 32-byte header): magic
`0x534E`, version 1, kind (`1` HELLO / `2` PCM / `3` HELLO_ACK), `device_id`,
`session_id`, `packet_seq`, `first_frame`, `t_mono_us`. HELLO carries a 40-byte
payload (rate, channels, `S16_LE`, reserved, 32-byte device name). PCM payloads
are capped at 1280 bytes so 32 + 1280 stays inside a 1500-byte MTU — larger
blocks would be IP-fragmented and a lost fragment costs the whole packet.

Three properties to preserve:

- **Timeline comes from the node's monotonic counter, never arrival time**:
  `mono_start_ns = anchor_ns + (t_mono_us - t0_us) * 1000`. Arrival time would let
  WiFi jitter look like gaps (`gap_tolerance_ms` is 20 ms) and would pin
  `clock_drift_ppm` to ~0 by construction.
- **`net_capture` does not write gap records.** A real loss moves `first_frame`
  and the node timer together, and the dispatcher registers the gap with the
  correct duration.
- **Format mismatch is an error in `/status`, not an adaptation.**

Out-of-order datagrams are held by `packet_seq` for `source.jitter_buffer_ms`
(default 150 ms), then the hole is declared lost. A session id change or a large
frame-number regression rebuilds the session and re-anchors. No packet for
`source.idle_timeout_ms` (default 5 s) flips `node_connected` to false — that is
how "the bedside device died overnight" becomes visible instead of looking like a
quiet night.

The node (`crates/node`) reuses `steno_lib::audio_capture` unchanged, has
its own config (`/etc/steno-monitor/node.toml`, `STENO_NODE_CONFIG`, or the first
CLI argument), repeats HELLO every `hello_interval_ms` so a restarted server
re-learns the format, and deliberately does **no** local buffering and **no**
retransmission.

## Configuration

`steno-lib::config` is the single tuning contract. The path comes from
`STENO_MONITOR_CONFIG`, else `/etc/steno-monitor/config.toml`; the file is read
once at startup and any change needs a restart.
`#[serde(deny_unknown_fields)]` everywhere, and
`Config::validate` collects **all** failures before exiting with status 2.

Sections: `[server]` (bind, port, http_threads 1–8, IANA `timezone`, `web_dir`),
`[source]` (kind/bind/port/device_id/jitter_buffer_ms/idle_timeout_ms),
`[audio]` (device, rate, channels 1–2, period/buffer, segment length, ring and
queue depths ≥ 2 periods, gap tolerance, flush interval, shutdown timeout),
`[detector]` (see below), `[stt]`, `[storage]` (both paths absolute, max >
cleanup target, reserve, retention interval).

`deployment/config.example.toml` is the authoritative key list with defaults, and
`crates/lib/tests/config_example.rs` parses and validates that exact file,
so it cannot drift from the validator. Adding a key means: field with a
`default_*()` function, a validator entry, the example file, and — if it is a
detector knob — the offline tools.

Detector pipeline (`detection_rate_hz` is fixed at 16 000; anything else is a
validation error): DC blocker → 80–1500 Hz band-pass → 10 ms frames → per-frame
`level_dbfs` (wideband) and `band_rms_dbfs`/`band_ratio`. A frame is a **hit**
when `level > max(absolute_floor_dbfs, noise_floor + noise_margin_db)` **and**
`band_ratio >= band_ratio_min` **and** `band_rms >= band_floor + band_margin_db`.
Onset needs `attack_min_hits` within `attack_window_frames`; a candidate survives
`hangover_frames` of misses, merges across `merge_gap_ms`, is dropped below
`min_event_ms`, and is force-split at `max_event_seconds` with `continued = 1`.
Noise floors are percentile estimators over `noise_floor_window_seconds`, frozen
while a candidate is open (max `noise_freeze_max_seconds`), cleared on a gap
longer than `noise_floor_reset_gap_seconds`, and not trusted for the first
`noise_floor_min_seconds` or during `filter_warmup_ms` after a reset.

## HTTP API and Portal

Axum, `/api/v1`, plus the Portal at `/`. No auth, no TLS (see above). Error body
is always `{"error":{"code","message"}}`.

| Route | Notes |
|---|---|
| `GET /api/v1/status` | service/recording/microphone/detector/storage/capture/source/queues/dsp/time/stt + `signal_state` |
| `GET /api/v1/days?from=&to=` | both `YYYY-MM-DD`, required, span ≤ 366 days; only days that actually hold data |
| `GET /api/v1/segments?date=` | `date` required, interpreted in `server.timezone` |
| `GET /api/v1/segments/{id}/gaps` | |
| `GET /api/v1/segments/{id}/audio` | supports `Range`; 200/206/416 |
| `GET /api/v1/segments/{id}/transcript` | `null` when not transcribed yet |
| `GET /api/v1/transcripts?date=`, `GET /api/v1/utterances?date=` | per-segment transcripts, streaming phrases |
| `GET /api/v1/events?date=`, `GET /api/v1/events/{id}` | |
| `PATCH /api/v1/events/{id}` | body `{"review_status": "snore"\|"not_snore"\|"unreviewed"}` |
| `GET /api/v1/control` | both units' `systemctl is-active` plus why control cannot act; `spawn_blocking` |
| `POST /api/v1/control` | `{"target":"server"\|"node","action":"start"\|"stop"\|"restart"}`; needs `[control] enabled` **and** a loopback bind; stopping the server unit drops the connection, so the Portal treats an aborted request as "likely applied" |

Portal rules worth keeping: the SPA falls back to `index.html` for unknown
extension-less paths, `/assets/*` is cached for a year while `index.html` is not,
`..` is rejected before path joining, and every formatter is total — a missing
field renders `—`, because a throw inside render unmounts the whole tree
(React 19). `web/src/api.ts` types are hand-written: a backend rename (e.g.
`used_bytes`) is invisible to `tsc` and only shows up when the page renders.

## Speech-to-text

Off by default (`stt.enabled = false`): it is the only stage that hands audio to
another program. Where that program sits, and how it is launched, is in
[Runtime architecture](#runtime-architecture). Two modes:

- `mode = "stream"` (default): one long-lived sidecar over a Unix socket. Server →
  sidecar: `[u32 len][i64 utc_ms][s16le mono 16 kHz PCM]`; sidecar → server:
  one JSON object per line (`ready`/`partial`/`final`/`error`). Each frame carries
  the recorder's wall clock, so a sidecar restart cannot shift text. The socket
  exists because the sidecar is started with `open -n -a` (the only launcher that
  returns immediately and leaves the process running) and `open` hands the child
  no stdio. `SpeechAnalyzer` needs no permission grant; rebuilding the bundle
  needs no re-authorisation.
- `mode = "segment"`: runs `stt.command` per finished WAV and parses one JSON
  object from stdout. Simpler, restart-safe, one process launch per segment.

The queue is the database (`segments_awaiting_transcript`): no in-memory backlog,
crash-resumes, a slow recogniser cannot backpressure recording, and a failed
segment writes `status='failed'` and is not retried automatically. Text and audio
are equally sensitive.

## Offline tools

All offline analysis replays the **same** pipeline the service uses, so tuning
offline predicts what the service stores.

```sh
cargo build --release --all-features --bins     # or add --features tools

detect-wav night.wav                       # JSON events on stdout
detect-wav night.wav --format csv -o events.csv
detect-wav night.wav --frames > events.json 2> frames.csv   # per-frame inputs
detect-wav night.wav --config /etc/steno-monitor/config.toml \
  --band-ratio-min 0.45 --noise-margin-db 10
detect-wav night.wav --mode segments --chunk-seconds 3600   # rotation / gap replay

steno-eval --manifest testdata/steno-eval/labels.json --tolerance-ms 500 --verbose
steno-eval --manifest labels.json --config /etc/steno-monitor/config.toml --json
```

`detect-wav` accepts 16/32/44.1/48/96 kHz, 1–2 channels, `WAVE_FORMAT_PCM` S16_LE,
including a recorder `.partial` (reports the truncated tail and analyses what
exists). It validates overrides through the service's own validator, so a
threshold startup would reject is rejected here too. `steno-eval` scores a corpus
manifest (`file` + `snore` or `intervals_s`) and reports TP/FP/FN,
precision/recall/F1 and onset error.

Calibration loop: run `--frames` around a known false positive, adjust
`--noise-margin-db` / `--band-ratio-min`, re-run until the candidate list matches
what you hear, write the values back into `deployment/config.example.toml`, then bump
`DETECTOR_VERSION` (`detector::DETECTOR_VERSION`) so stored events stay
attributable.

`tools/replay-wav.py` replays a WAV into the UDP protocol in real time (no
microphone needed for server-side testing); `tools/stt-stream-probe.py` drives the
streaming sidecar socket directly.

## Verification status

Verified locally: `cargo test --workspace --all-features` green, HTTP API and
Range behaviour covered by `crates/server/tests/http_api.rs`, the UDP path
and its failure modes by `tests/net_capture.rs`, pipeline wiring by
`tests/pipeline_e2e.rs`, resampler passband/alias targets by
`tests/resampler_metrics.rs`, and the committed example config by
`tests/config_example.rs`. End-to-end runs have been done with a fake node, a
real Pi node, SIGTERM shutdown, and a browser Portal.

Known defects — do not rediscover them as new:

- **`xrun_count` increments every ~10 s on the Pi with a USB mic**, roughly 1.6%
  of audio missing (60 s wall clock, 61.98 s written). `arecord` with the same
  device shows zero overruns, so it is on the CPAL consumption side, not ALSA.
- **A start with no usable device leaves a `complete` segment with 0 frames and
  44 bytes**, one per start.
- **Retention only runs at startup.** `storage.retention_check_interval_seconds`
  is validated but never scheduled, and nothing calls `RetentionManager::enforce`
  on segment completion, so a long-running server keeps growing past
  `recording_max_bytes` until it is restarted. The startup pass is real and
  tested; the periodic ones do not exist yet.
- **`audio.alsa_device` matches the device description string.** cpal 0.18 does
  expose `DeviceTrait::id()` (`default:CARD=…`), so matching on the PCM id is
  possible and would be the better fix; until then a config that works on one
  host may silently fail on another.
- `stt.timeout_seconds` is validated but not enforced by the Rust side, and the
  sidecar no longer imposes its own cutoff either: a sidecar that never returns
  blocks the segment worker until the process is restarted.
- A failed transcript is never retried and never raises a warning.
- `steno-node` only accepts S16; a host whose only capture device is F32 cannot
  run the node.

Not verified anywhere yet (needs the target board, tracked as T8.x):
**ALSA negotiation on the real USB microphone**, an 8-hour run with RSS < 80 MiB
and DSP CPU < 10% of a core, clock behaviour without an RTC, the systemd units
under a missing USB mount, and threshold calibration against labelled real
snoring.

## Doc map

`docs/README.md` is the index; this table is the short version.

| Doc | Status |
|---|---|
| `docs/decisions/2026-10-09-t0-1.md` | T0.1 decisions (timezone, retention triggers, loopback default, crash consistency, detector blind spot) — still authoritative for *why* |
| `docs/decisions/2026-10-29-architecture.md` | why the workspace split, why Vite, why trait + external process for STT |
| `docs/decisions/2026-10-29-split-deployment.md` | node/server split: protocol, timeline decision, failure model, deployment |
| `docs/decisions/2026-10-30-control-centre.md` | control endpoints, storage pool, and why "recogniser not connected" kept appearing |
| `docs/decisions/2026-10-30-stt-accuracy.md` | how accuracy was measured before anyone changed a threshold |
| `docs/operations/2026-10-29-pi-deploy.md` | the only record of a real Pi build: cargo-zigbuild recipe with an armhf sysroot (`cross` fails — its glibc is too new and the ghcr image hangs), install layout, found bugs |
| `docs/operations/eval-workflow.md` | how to score the detector with `steno-eval`, and why short-clip corpora need a warmer floor |
| `docs/verification/acceptance-report.md`, `docs/verification/spike-pi.md` | templates with 待实测 placeholders — do not fill them with estimates |
| `docs/spec/TECH_SPEC.md`, `docs/spec/tasks.md` | the original spec and task list; historical, still useful for section numbers (§4.3, §5.1, §11) referenced in code comments |

The file names of `TECH_SPEC.md` and `tasks.md` are load-bearing: 90-odd code
comments cite `TECH_SPEC §x` and `tasks.md T8.6`, so renaming them would break
every reference at once.

## Common changes

- **New config key**: `default_*()` + field + validator + `deployment/config.example.toml`
  (+ `tests/config_example.rs` still passes). Unknown or invalid keys must keep
  failing startup with the key named.
- **New detector knob**: config field, validator range, example file, then the
  override flag in *both* `detect-wav` and `steno-eval` so calibration stays
  possible, then bump `DETECTOR_VERSION` if stored events change meaning.
- **New endpoint**: add the Axum route under `/api/v1`, keep the
  `{"error":{...}}` shape and strict `YYYY-MM-DD` parsing, extend
  `web/src/api.ts` only if the Portal needs it, and cover it in
  `crates/server/tests/http_api.rs`.
- **New capture source**: implement the `CaptureRing::callback_push` producer
  contract and a handle that stops on drop; do not touch the dispatcher.
- **Anything that touches the recording path**: keep transcription and detection
  off it. If a change can make the recorder wait on another component, it is
  wrong.
