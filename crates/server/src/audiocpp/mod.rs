//! A sidecar for audio.cpp's Qwen3-ASR, in Rust.
//!
//! ## Why this is a separate process and still "built in"
//!
//! The recogniser is a GGUF model in a 1.4 GB resident process. Putting it in
//! the server's address space would mean a model crash takes the recorder with
//! it, so it stays behind a process boundary — the same boundary the Apple
//! sidecar uses.
//!
//! What it does **not** need is the Apple app. The Swift backend that used to
//! drive `audiocpp_cli` imported only `AVFoundation`, and only for
//! `AVAudioFormat` / `AVAudioPCMBuffer` — transport types, not capability. Every
//! Apple-only thing in `apple-stt-sidecar` lives in `SpeechAnalyzerBackend`
//! (`Speech`, `CoreMedia`) and `SnoreDetector` (`SoundAnalysis`, `CoreML`).
//! Keeping the audio.cpp path in that app meant the engine could only run on a
//! Mac, behind `open -n -a`, a shell wrapper, and a `.conf` file rewritten by
//! `install.sh`.
//!
//! Compiling it here removes all three: this binary ships with the server, takes
//! the socket path as its only argument, and speaks the same framed protocol, so
//! `stt_stream` does not change and neither does the wire.
//!
//! ## What it replaced, and why that mattered
//!
//! The wrapper read the model path from `$PREFIX/lib/steno-stt-sidecar-audiocpp.conf`.
//! One `refresh-all.sh` (which runs `install.sh` with no environment) rewrote that
//! file with an empty path, and the server reported only `launcher exited with
//! exit status: 2`. Here the same facts come from the environment and are
//! validated at startup with the offending variable named — the same rule the
//! rest of this project follows: unknown or unusable configuration fails loudly,
//! it never becomes a `ready` that produces nothing.
//!
//! ## Layout
//!
//! ```text
//! config   what this machine has: model, binary, window length, language table
//! queue    the backlog between the socket and the engine's stdin
//! engine   audiocpp_cli: spawn, stdin writer, stdout reader, window placement
//! session  one socket, one engine, and the order things stop in
//! ```

pub mod config;
pub mod engine;
pub mod queue;
pub mod session;

use std::path::PathBuf;
use std::process::ExitCode;

use crate::sidecar_wire::MsgType;

/// One frame on its way to the server.
///
/// The socket writer owns the encoding so a transcript and a session notice can
/// never interleave inside a frame; `payload` is therefore already encoded.
#[derive(Debug)]
pub struct Outgoing {
    pub kind: MsgType,
    pub payload: Vec<u8>,
}

/// Configuration is wrong and this process refuses to start.
const EXIT_CONFIG: u8 = 2;
/// The socket or the engine failed after a valid start.
const EXIT_RUNTIME: u8 = 3;

pub fn main() -> ExitCode {
    init_logging();

    let mut socket: Option<PathBuf> = None;
    let mut check = false;
    let mut arguments = std::env::args().skip(1);
    let set_socket = |value: &str, socket: &mut Option<PathBuf>| -> bool {
        if socket.is_some() {
            eprintln!("steno-stt-sidecar-audiocpp: exactly one socket path may be given");
            return false;
        }
        *socket = Some(PathBuf::from(value));
        true
    };

    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--check" => check = true,
            // Both spellings are in use: the server passes the path positionally
            // (the launch contract), `tools/stt-stream-probe.py` passes `--path`.
            "--socket" | "--path" => {
                let Some(value) = arguments.next() else {
                    eprintln!("steno-stt-sidecar-audiocpp: {argument} needs a value");
                    return ExitCode::from(EXIT_CONFIG);
                };
                if !set_socket(&value, &mut socket) {
                    return ExitCode::from(EXIT_CONFIG);
                }
            }
            "--mode" => {
                let Some(value) = arguments.next() else {
                    eprintln!("steno-stt-sidecar-audiocpp: --mode needs a value");
                    return ExitCode::from(EXIT_CONFIG);
                };
                if value != "socket" {
                    // File mode is the *server's* per-segment path, and it spawns the
                    // engine directly for one finished WAV. Faking it here would put
                    // a second implementation of that path in the repository.
                    eprintln!(
                        "steno-stt-sidecar-audiocpp: --mode {value} is not supported; this sidecar only runs the socket session"
                    );
                    return ExitCode::from(EXIT_CONFIG);
                }
            }
            "--help" | "-h" => {
                print_usage();
                return ExitCode::SUCCESS;
            }
            "--version" => {
                println!(
                    "steno-stt-sidecar-audiocpp {}",
                    env!("CARGO_PKG_VERSION")
                );
                return ExitCode::SUCCESS;
            }
            other if other.starts_with("--") => {
                eprintln!("steno-stt-sidecar-audiocpp: unknown option {other}");
                print_usage();
                return ExitCode::from(EXIT_CONFIG);
            }
            other => {
                if !set_socket(other, &mut socket) {
                    return ExitCode::from(EXIT_CONFIG);
                }
            }
        }
    }

    if check {
        let (report, available) = config::check_report();
        println!("{report}");
        return if available {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(EXIT_CONFIG)
        };
    }

    let Some(socket) = socket else {
        eprintln!("steno-stt-sidecar-audiocpp: the socket path is required");
        print_usage();
        return ExitCode::from(EXIT_CONFIG);
    };

    let config = match config::from_env(socket) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("steno-stt-sidecar-audiocpp: {error}");
            return ExitCode::from(EXIT_CONFIG);
        }
    };

    match session::run(config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("steno-stt-sidecar-audiocpp: {error}");
            ExitCode::from(EXIT_RUNTIME)
        }
    }
}

fn print_usage() {
    eprintln!("usage: steno-stt-sidecar-audiocpp <socket-path>");
    eprintln!("       steno-stt-sidecar-audiocpp --socket <path> | --path <path>");
    eprintln!("       steno-stt-sidecar-audiocpp --check");
    eprintln!();
    eprintln!("environment:");
    eprintln!("  STENO_STT_MODEL         Qwen3-ASR package directory (required)");
    eprintln!("  STENO_STT_CHUNK         window length in seconds, 1..30 (default 5)");
    eprintln!("  STENO_STT_AUDIOCPP_BIN  audiocpp_cli to run (default: the one on PATH)");
    eprintln!("  STENO_LOG               log filter for this process (default: info)");
}

fn init_logging() {
    // Whatever `stderr` is, is where the lines go: launched by the server, that is
    // the service's journal — next to the session they explain.
    let raw = std::env::var("STENO_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .unwrap_or_else(|_| "info".to_string());
    let filter = tracing_subscriber::EnvFilter::try_new(&raw)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}
