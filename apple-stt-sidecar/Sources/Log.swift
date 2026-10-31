// Diagnostics: one line per event, on stderr and in a file.
//
// LaunchServices swallows stderr, so the file is the only copy for a headless
// run. Nothing here ever writes to stdout: under `--stdio` stdout *is* the wire,
// and a stray log line would be read as a corrupted frame.

import Foundation

let logPath: String =
    ProcessInfo.processInfo.environment["STENO_STT_LOG"] ?? "/tmp/steno-stt-sidecar.log"

private let logLock = NSLock()

var verboseEnabled = ProcessInfo.processInfo.environment["STENO_STT_VERBOSE"] == "1"

/// Whether recognised text may be written to the log.
///
/// Off by default, and deliberately: the log is `/tmp/steno-stt-sidecar.log`,
/// which is world-readable, and a transcript is the one thing this project keeps
/// out of logs. `--log-text` turns it on for diagnosis; `tools/dev.sh` passes it
/// on a dev machine, production does not. When it is on the log file is chmod
/// 0600 at startup (see `main`), because a transcript in a world-readable file is
/// worse than no transcript at all.
var logTranscripts = false

/// Wall clock for a log line.
///
/// Without it the file is a list of facts with no order: the server's log has
/// timestamps, this one did not, so merging the two (which `tools/dev.sh` does to
/// show one timeline) could only ever guess. Local time on purpose — this is read
/// by a person next to a clock, and the utc_ms on the wire stays UTC.
private let logClock: DateFormatter = {
    let formatter = DateFormatter()
    formatter.dateFormat = "HH:mm:ss.SSS"
    formatter.locale = Locale(identifier: "en_US_POSIX")
    return formatter
}()

func log(_ message: String) {
    let stamp = logClock.string(from: Date())
    let line = "steno-stt-sidecar[\(getpid())]: \(stamp) \(message)\n"
    guard let data = line.data(using: .utf8) else { return }
    logLock.lock()
    defer { logLock.unlock() }
    FileHandle.standardError.write(data)
    if let handle = FileHandle(forWritingAtPath: logPath) {
        handle.seekToEndOfFile()
        handle.write(data)
        try? handle.close()
    } else {
        try? data.write(to: URL(fileURLWithPath: logPath))
    }
}

/// Per-block chatter. Off unless `--verbose`, because a night is 180 000 blocks
/// and the log file is not rotated.
func logDebug(_ message: @autoclosure () -> String) {
    guard verboseEnabled else { return }
    log(message())
}
