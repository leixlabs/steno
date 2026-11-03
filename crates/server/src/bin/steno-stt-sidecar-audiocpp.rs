//! Sidecar entry point: `steno-stt-sidecar-audiocpp <socket-path>`.
//!
//! A separate binary rather than a subcommand of `steno-monitor`, because the
//! launch contract the server already implements is "one executable, one
//! argument, return immediately" — the same one the Swift wrappers satisfy. The
//! code lives in [`steno_server::audiocpp`] so it is built, installed and
//! deployed with the server; only the entry point is here.

fn main() -> std::process::ExitCode {
    steno_server::audiocpp::main()
}
