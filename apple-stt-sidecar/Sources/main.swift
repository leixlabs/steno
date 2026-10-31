// steno-stt-sidecar — audio in, JSON out.
//
// One job: turn a stream of 16 kHz PCM into recognised text, using whichever
// macOS on-device model is current. Nothing in this process decides when a
// phrase is over, how long a session may last, or what to do about silence —
// those belong to the caller, and to `SpeechAnalyzer` itself.
//
//   --socket <path>   connect to the server's Unix socket and stream events
//   --file <audio>    transcribe one finished file, print a Transcript object
//   --check           print what this machine can do, as JSON
//   --install-assets  download the on-device model for --locale
//
//

import AVFoundation
import Darwin
import Foundation
import Speech

struct Options {
    enum Mode {
        case stream(socket: String)
        case file(path: String)
        case check
        case installAssets
    }

    var mode: Mode
    var locale: String
    var engine: String
    var verbose: Bool
    /// Where `--file` should leave its JSON. LaunchServices gives the bundle no
    /// stdout, so the wrapper always passes one.
    var out: String?
}

let usage = """
usage: steno-stt-sidecar --socket <path> | --file <audio> [--out <json>] | --check | --install-assets
       [--locale zh-CN] [--engine apple-speechanalyzer] [--verbose]
"""

func parseArguments(_ arguments: [String]) -> Options? {
    var mode: Options.Mode?
    var locale = "zh-CN"
    var engine = "apple-speechanalyzer"
    var out: String?
    var verbose = ProcessInfo.processInfo.environment["STENO_STT_VERBOSE"] == "1"
    var index = 1
    while index < arguments.count {
        let argument = arguments[index]
        switch argument {
        case "--socket":
            index += 1
            guard index < arguments.count else { return nil }
            mode = .stream(socket: arguments[index])
        case "--file":
            index += 1
            guard index < arguments.count else { return nil }
            mode = .file(path: arguments[index])
        case "--check":
            mode = .check
        case "--install-assets":
            mode = .installAssets
        case "--locale":
            index += 1
            guard index < arguments.count else { return nil }
            locale = arguments[index]
        case "--engine":
            index += 1
            guard index < arguments.count else { return nil }
            engine = arguments[index]
        case "--out":
            index += 1
            guard index < arguments.count else { return nil }
            out = arguments[index]
        case "--verbose":
            verbose = true
        default:
            // Unknown arguments are a mistake, not a hint: refusing here is the
            // only way a typo does not turn into a silently different run.
            log("unknown argument: \(argument)")
            return nil
        }
        index += 1
    }
    guard let mode else { return nil }
    return Options(mode: mode, locale: locale, engine: engine, verbose: verbose, out: out)
}

/// One JSON object, on stdout or in a file. `--out` exists because the bundle is
/// launched by LaunchServices, which drops stdout on the floor.
func emitJSON(_ object: [String: Any], to path: String?) {
    guard let data = try? JSONSerialization.data(withJSONObject: object, options: [.sortedKeys, .withoutEscapingSlashes])
    else {
        log("cannot encode JSON")
        return
    }
    guard let path else {
        FileHandle.standardOutput.write(data)
        FileHandle.standardOutput.write(Data([0x0a]))
        return
    }
    do {
        try (data + Data([0x0a])).write(to: URL(fileURLWithPath: path))
    } catch {
        log("cannot write \(path): \(error)")
    }
}

func connect(socketPath: String) -> Int32? {
    // The server binds before it launches us, so the first attempt normally
    // succeeds; the retry loop only covers the socket being replaced by a
    // restart that overlapped our launch.
    for attempt in 0..<25 {
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { return nil }
        var address = sockaddr_un()
        address.sun_family = sa_family_t(AF_UNIX)
        let path = socketPath.utf8CString
        guard path.count <= MemoryLayout.size(ofValue: address.sun_path) else {
            close(fd)
            return nil
        }
        withUnsafeMutableBytes(of: &address.sun_path) { raw in
            raw.copyBytes(from: path.map { UInt8(bitPattern: $0) })
        }
        let size = socklen_t(MemoryLayout<sockaddr_un>.size)
        let result = withUnsafePointer(to: &address) { pointer in
            pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.connect(fd, $0, size)
            }
        }
        if result == 0 { return fd }
        close(fd)
        if attempt == 0 { log("waiting for \(socketPath)") }
        Thread.sleep(forTimeInterval: 0.2)
    }
    return nil
}

/// Reads a finished file as 16 kHz mono samples, converting only when the file
/// is not already in the shape the wire uses (the recorder always writes it, so
/// the conversion branch exists for foreign files, not for ours).
func samplesFromFile(at url: URL) throws -> [Int16] {
    let file = try AVAudioFile(forReading: url, commonFormat: .pcmFormatInt16, interleaved: false)
    guard file.length > 0 else { return [] }
    let source = file.processingFormat
    let frames = AVAudioFrameCount(file.length)
    guard let sourceBuffer = AVAudioPCMBuffer(pcmFormat: source, frameCapacity: frames) else {
        throw StudioError.audioFormat("cannot allocate \(frames) frames")
    }
    try file.read(into: sourceBuffer)

    if source.sampleRate == 16_000 && source.channelCount == 1 {
        guard let channel = sourceBuffer.int16ChannelData?[0] else { return [] }
        return Array(UnsafeBufferPointer(start: channel, count: Int(sourceBuffer.frameLength)))
    }

    guard let target = AVAudioFormat(
        commonFormat: .pcmFormatInt16, sampleRate: 16_000, channels: 1, interleaved: false
    ), let converter = AVAudioConverter(from: source, to: target) else {
        throw StudioError.audioFormat("cannot convert \(source) to 16 kHz mono")
    }
    let capacity = AVAudioFrameCount(Double(file.length) * 16_000 / source.sampleRate) + 1_024
    guard let output = AVAudioPCMBuffer(pcmFormat: target, frameCapacity: capacity) else {
        throw StudioError.audioFormat("cannot allocate \(capacity) converted frames")
    }
    var supplied = false
    var conversionError: NSError?
    let input: AVAudioConverterInputBlock = { _, status in
        if supplied {
            status.pointee = .endOfStream
            return nil
        }
        supplied = true
        status.pointee = .haveData
        return sourceBuffer
    }
    converter.convert(to: output, error: &conversionError, withInputFrom: input)
    if let conversionError { throw conversionError }
    guard let channel = output.int16ChannelData?[0] else { return [] }
    return Array(UnsafeBufferPointer(start: channel, count: Int(output.frameLength)))
}

/// Feeds a finished file through the same path the socket uses, so a file
/// transcript and a live one come out of identical code.
func feedFile(at url: URL, into studio: Studio) throws {
    let samples = try samplesFromFile(at: url)
    let chunk = 320  // 20 ms, the same block size the recorder sends
    var offset = 0
    while offset < samples.count {
        let end = min(offset + chunk, samples.count)
        studio.feed(samples: Array(samples[offset..<end]), utcMs: Int64(offset) / 16)
        offset = end
        // Faster than real time: this is a file, and the analyser may run flat out.
        Thread.sleep(forTimeInterval: 0.005)
    }
}

// MARK: - modes

func runStream(socketPath: String, options: Options) -> Never {
    guard let fd = connect(socketPath: socketPath) else {
        log("cannot connect to \(socketPath)")
        exit(3)
    }
    log("connected to \(socketPath)")
    let writer = EventWriter(fd: fd)
    let sink = SocketSink(writer: writer, onBroken: {
        // The server is gone. There is nobody left to tell, so stop being alive
        // rather than write into a dead pipe for a whole night.
        exit(0)
    })

    Task.detached {
        do {
            let studio = try await Studio.make(
                localeIdentifier: options.locale,
                engine: options.engine,
                sink: sink
            )
            try await studio.start()
            log("session started: \(options.engine) \(options.locale)")

            // The read side is blocking on purpose: one thread owns the socket,
            // and nothing the analyser does can stall it.
            await withCheckedContinuation { (continuation: CheckedContinuation<Void, Never>) in
                Thread.detachNewThread {
                    var reader = FrameReader(fd: fd)
                    var frames = 0
                    do {
                        while let frame = try reader.next() {
                            studio.feed(samples: frame.samples, utcMs: frame.utcMs)
                            frames += 1
                        }
                    } catch {
                        log("read error: \(error)")
                    }
                    log("audio ended after \(frames) frames")
                    continuation.resume()
                }
            }
            await studio.finish()
            log("session finished")
            exit(0)
        } catch {
            sink.failed("\(error)")
            log("cannot start: \(error)")
            exit(4)
        }
    }
    dispatchMain()
}

func runFile(path: String, options: Options) -> Never {
    let url = URL(fileURLWithPath: path)
    guard FileManager.default.fileExists(atPath: url.path) else {
        log("no such file: \(url.path)")
        exit(2)
    }
    Task.detached {
        do {
            let sink = CollectingSink()
            let studio = try await Studio.make(
                localeIdentifier: options.locale,
                engine: options.engine,
                sink: sink
            )
            try await studio.start()
            try feedFile(at: url, into: studio)
            await studio.finish()
            var transcript: [String: Any] = [
                "text": sink.text,
                "engine": sink.engine.isEmpty ? options.engine : sink.engine,
                "segments": sink.segments.map { segment in
                    [
                        "start_ms": segment.start_ms,
                        "end_ms": segment.end_ms,
                        "text": segment.text,
                    ] as [String: Any]
                },
            ]
            if let language = sink.language { transcript["language"] = language }
            emitJSON(transcript, to: options.out)
            exit(0)
        } catch {
            log("cannot transcribe \(path): \(error)")
            exit(5)
        }
    }
    dispatchMain()
}

func runCheck(options: Options) -> Never {
    Task.detached {
        let report = await Studio.probe(localeIdentifier: options.locale)
        emitJSON(report, to: nil)
        exit(0)
    }
    dispatchMain()
}

func runInstall(options: Options) -> Never {
    Task.detached {
        do {
            let result = try await Studio.installAssets(localeIdentifier: options.locale)
            emitJSON(["result": result], to: nil)
            exit(0)
        } catch {
            log("cannot install assets: \(error)")
            exit(6)
        }
    }
    dispatchMain()
}

// MARK: - entry

signal(SIGPIPE, SIG_IGN)

guard let options = parseArguments(CommandLine.arguments) else {
    log(usage)
    exit(64)
}

switch options.mode {
case .stream(let socket):
    runStream(socketPath: socket, options: options)
case .file(let path):
    runFile(path: path, options: options)
case .check:
    runCheck(options: options)
case .installAssets:
    runInstall(options: options)
}
