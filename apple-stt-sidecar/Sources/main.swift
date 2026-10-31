// steno-stt-sidecar — audio in, text (and snore annotations) out.
//
// One job: turn a stream of 16 kHz mono PCM into recognised text and candidate
// snore episodes, using whichever macOS on-device models are current. Nothing in
// this process decides when a phrase is over, when a night ends, or what to do
// about silence — those belong to the caller, and to the frameworks.
//
//   --mode socket --path <p>  framed duplex over a Unix socket (the server's path)
//   --mode stdio              framed duplex over stdin/stdout
//   --mode mic                the default input device
//   --mode file <audio>       transcribe one finished file
//   --stream                  raw PCM on stdin, no framing
//   --check                   what this machine can do, as JSON
//   --install-assets          download the on-device model for --locale
//
// Both framed modes are full duplex: audio and results go out while `config` and
// `control` frames come in, on the same connection.

import AVFoundation
import Darwin
import Foundation

// MARK: - helpers

/// One JSON object on stdout. Used by `--check`, `--install-assets` and
/// `--mode file --format json`, all of which are read by a machine.
func emitJSON(_ object: [String: Any], to path: String? = nil) {
    guard let data = try? JSONSerialization.data(
        withJSONObject: object, options: [.sortedKeys, .withoutEscapingSlashes]
    ) else {
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

/// A `config` frame arriving before the session exists. The wire wins over argv
/// for the fields it carries: the server knows which language this night is in,
/// and a stale default on the command line must not override it. Fields the wire
/// omits keep their argv value — that is how `--snore-model` survives, since a
/// model path is a local fact the server has no business knowing.
func applyWireConfig(_ payload: ConfigPayload, to options: inout Options) {
    if let locale = payload.locale { options.locale = locale }
    if let raw = payload.preset {
        if let preset = TranscriptionPreset.parse(raw) {
            options.preset = preset
        } else {
            log("config: unknown preset \(raw); keeping \(options.preset.rawValue) (expected \(TranscriptionPreset.known))")
        }
    }
    if payload.snore == false {
        options.snore = nil
    } else if payload.snore == true, options.snore == nil {
        options.snore = SnoreConfig()
    }
    if let threshold = payload.snoreThreshold { options.snore?.threshold = threshold }
    if let window = payload.snoreWindow { options.snore?.windowSeconds = window }
    log("config: locale \(options.locale), preset \(options.preset.rawValue), snore \(options.snore?.label ?? "off")")
}

func makePipelineConfig(_ options: Options) -> PipelineConfig {
    PipelineConfig(
        locale: options.locale,
        engine: options.engine,
        preset: options.preset,
        snore: options.snore
    )
}

// MARK: - framed modes (socket, stdio)

func runFramed(options: Options, channel: FrameChannel) -> Never {
    var config = options
    var held: (samples: [Int16], utcMs: Int64)?

    // The first frame decides whether the session starts with wire config. It is
    // read here, before anything is built, because starting a recogniser and
    // then discovering it is listening for the wrong language wastes a model
    // load and leaves a `ready` that is already wrong.
    do {
        guard let first = try channel.next() else {
            log("peer closed before sending a frame")
            exit(0)
        }
        guard first.direction == .rustToSwift else {
            log("protocol: the first frame is not a downlink frame")
            exit(7)
        }
        switch first.type {
        case .config:
            applyWireConfig(try JSONPayload.decode(ConfigPayload.self, from: first.payload), to: &config)
        case .audio:
            held = try AudioPayload.decode(first.payload)
        case .control:
            let control = try JSONPayload.decode(ControlPayload.self, from: first.payload)
            if control.action == "stop" {
                log("control: stop before the session started")
                exit(0)
            }
            log("control: \(control.action) arrived before the session started; ignored")
        default:
            log("unexpected first frame: \(first.type)")
            exit(7)
        }
    } catch {
        log("cannot read the first frame: \(error)")
        exit(7)
    }

    Task.detached {
        let sink = WireSink(channel: channel) {
            // The server is gone. There is nobody left to tell, so stop being
            // alive rather than write into a dead pipe for a whole night.
            log("peer is gone; closing")
            exit(0)
        }
        do {
            let session = try await FramedSession.make(config: config, channel: channel, sink: sink)
            try session.start()
            if let held { session.ingest(samples: held.samples, utcMs: held.utcMs) }

            await withCheckedContinuation { (continuation: CheckedContinuation<Void, Never>) in
                Thread.detachNewThread {
                    session.readLoop()
                    continuation.resume()
                }
            }
            await session.finish(reason: "the peer closed the stream")
            exit(0)
        } catch {
            sink.failed("\(error)")
            log("cannot start: \(error)")
            exit(4)
        }
    }
    dispatchMain()
}

// MARK: - local modes

func runFile(options: Options) -> Never {
    guard case .file(let path) = options.mode else { exit(64) }
    guard FileManager.default.fileExists(atPath: path) else {
        log("no such file: \(path)")
        exit(2)
    }
    Task.detached {
        let sink = TerminalSink(format: options.format, out: options.out)
        do {
            let pipeline = try await Pipeline.make(config: makePipelineConfig(options), sink: sink)
            try pipeline.start()
            try FileSource.run(path: path, hub: pipeline.hub, pace: true)
            await pipeline.finish()
            sink.finish()
            exit(0)
        } catch {
            sink.failed("\(error)")
            log("cannot transcribe \(path): \(error)")
            exit(5)
        }
    }
    dispatchMain()
}

func runRawStdin(options: Options) -> Never {
    Task.detached {
        let sink = TerminalSink(format: options.format, out: options.out)
        do {
            let pipeline = try await Pipeline.make(config: makePipelineConfig(options), sink: sink)
            try pipeline.start()
            try RawStreamSource.run(
                fd: 0, rate: options.sampleRate, channels: options.channels, hub: pipeline.hub
            )
            await pipeline.finish()
            sink.finish()
            exit(0)
        } catch {
            sink.failed("\(error)")
            log("cannot read stdin: \(error)")
            exit(5)
        }
    }
    dispatchMain()
}

func runMicrophone(options: Options) -> Never {
    Task.detached {
        let sink = TerminalSink(format: options.format, out: options.out)
        do {
            let pipeline = try await Pipeline.make(config: makePipelineConfig(options), sink: sink)
            try pipeline.start()
            let microphone = MicrophoneSource(hub: pipeline.hub)
            try microphone.run(seconds: options.seconds)
            await pipeline.finish()
            sink.finish()
            exit(0)
        } catch {
            sink.failed("\(error)")
            log("microphone: \(error)")
            exit(5)
        }
    }
    dispatchMain()
}

// MARK: - device report

func runCheck(options: Options) -> Never {
    Task.detached {
        var report = await Transcriber.probe(localeIdentifier: options.locale)
        report["engine"] = options.engine
        report["preset"] = options.preset.rawValue
        report["hub_format"] = AudioHub.canonicalFormat().description
        if let snore = options.snore {
            report["snore"] = SnoreDetector.probe(config: snore)
        } else {
            report["snore"] = ["enabled": false]
        }
        emitJSON(report)
        exit(0)
    }
    dispatchMain()
}

func runInstall(options: Options) -> Never {
    Task.detached {
        do {
            let result = try await Transcriber.installAssets(localeIdentifier: options.locale)
            emitJSON(["result": result])
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

guard let options = Options.parse(CommandLine.arguments) else {
    log(Options.usage)
    exit(64)
}
verboseEnabled = options.verbose
if options.logText {
    logTranscripts = true
    // A transcript in a world-readable file is worse than no transcript. Create
    // the log 0600 and tighten one that already exists (it is appended to across
    // runs, so it may have been created by a run without the flag).
    FileManager.default.createFile(
        atPath: logPath, contents: nil, attributes: [.posixPermissions: 0o600]
    )
    _ = chmod(logPath, 0o600)
}

switch options.mode {
case .socket(let path):
    guard let fd = connect(socketPath: path) else {
        log("cannot connect to \(path)")
        exit(3)
    }
    log("connected to \(path)")
    runFramed(options: options, channel: FrameChannel(fdIn: fd, fdOut: fd))
case .stdio:
    log("stdio framing on stdin/stdout")
    runFramed(options: options, channel: FrameChannel(fdIn: 0, fdOut: 1))
case .file:
    runFile(options: options)
case .rawStdin:
    runRawStdin(options: options)
case .microphone:
    runMicrophone(options: options)
case .check:
    runCheck(options: options)
case .installAssets:
    runInstall(options: options)
}
