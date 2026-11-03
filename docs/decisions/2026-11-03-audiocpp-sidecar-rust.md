# 2026-11-03 — 把 audio.cpp 后端从 Swift 侧车搬到 Rust 侧车

状态：已完成。Rust 侧车实现并端到端验证；Swift 侧的 `AudioCPPBackend`、
`--backend audiocpp` 分支、audiocpp wrapper 与 `.conf` 逻辑已全部删除；
配置、`dev.sh`、`refresh-all.sh` 已接线。

## 为什么

`AudioCPPBackend.swift` 只 `import AVFoundation` 和 `Foundation`，而 AVFoundation 只用在
两处传输类型上（`AVAudioFormat` 第 295 行、`AVAudioPCMBuffer` 第 456 行）。真正依赖 Apple
能力的是另外两个文件：`SpeechAnalyzerBackend.swift`（`Speech`、`CoreMedia`）和
`SnoreDetector.swift`（`SoundAnalysis`、`CoreML`）。

也就是说，这条后端从来不是「macOS 后端」，它只是一个 `audiocpp_cli` 的子进程驱动器，
碰巧住在一个 macOS 应用里。住在那里的代价是三条：

1. **只能用 macOS 跑。** 服务端本体是能交叉编译到 armv7 Pi 的 `crates/server`，但想用
   Qwen3-ASR 就必须在那台机器上 `open -n -a steno-stt-sidecar.app`。
2. **必须有一张 shell wrapper 和一个 conf 文件。** 见 2026-11-02 记录的「事后修正」：
   `refresh-all.sh` 不带环境变量重跑 `install.sh`，把模型路径替换成空串，`/status` 只剩
   `launcher exited with exit status: 2`。
3. **模型与窗口长度这类本地事实走了两条路**（wrapper 的 conf + 命令行参数），
   没有一处统一校验。

搬到 Rust 之后，这三条都不存在：二进制随服务端一起构建、一起部署，启动契约还是
「一个可执行、一个参数、立刻返回」，线格式一个字节没变，所以 `stt_stream` 不改。

## 变更

| 文件 | 变化 |
|---|---|
| `crates/server/src/audiocpp/mod.rs` | 新增。入口 `main()`、`--check`、参数解析（位置参数 / `--socket` / `--path`，`--mode socket` 接受、`--mode file` 明确拒绝）、日志初始化 |
| `crates/server/src/audiocpp/config.rs` | 新增。`STENO_STT_MODEL` / `STENO_STT_CHUNK` / `STENO_STT_AUDIOCPP_BIN` 的读取与校验；30 项语言码表与 `resolve_language`；`--check` 报告 |
| `crates/server/src/audiocpp/engine.rs` | 新增。`audiocpp_cli` 的启动参数（与 Swift `launchArguments()` 逐项一致）、stdin 写线程、stdout 读线程、窗口落点、`finish()` 顺序 |
| `crates/server/src/audiocpp/queue.rs` | 新增。`AudioPipe`：无界积压 + 已交付帧 + 60 s 告警 |
| `crates/server/src/audiocpp/session.rs` | 新增。连接、首帧决定语言、帧循环、出站 writer、停机顺序 |
| `crates/server/src/bin/steno-stt-sidecar-audiocpp.rs` | 新增。三行入口 |
| `crates/server/src/sidecar_wire.rs` | `write_frame_as(direction, …)`；`write_frame` 委托给它。服务端写向下、侧车写向上，帧头只有一份实现 |
| `crates/server/src/lib.rs`、`crates/server/Cargo.toml` | 加模块与 `[[bin]]` |

## 搬过来的四个坑（原样保留，逐条注释）

1. `partial_text=` 是**一窗的定稿**，不是半截。因此以 `kind: "final"` 上报，
   `capabilities.partials` 为 false，界面几秒没字是预期。
2. `text_output=` 是**整场拼接**，读进来只记一行 debug，绝不入库。
3. **静音窗什么都不打印**，所以行数 ≠ 窗数，窗口边界只能用自己记的 index，
   不能靠数行推。
4. **尾部窗只在 EOF 前吐出来**。`finish()` 的顺序是「停止喂 → 等 writer 排空 →
   关 stdin → 等 EOF」，顺序颠倒会把增量音频丢掉。

## 窗口落点的已知不精确（诚实记录）

`start = index * chunk_frames`，`index` 按**上报的行数**递增；静音窗不打印，所以
经历一段长静音之后 index 会落后于真实窗号，`start` 会偏早。这一点是从 Swift 版
原样搬过来的，**没有顺手改**：改它属于行为变更，会让同一天的库存在两种时间口径。
`end` 取「引擎已交付到的帧」，因此相邻窗会重叠约一个解码延迟（实测 6.0–6.2 s 的区间）,
好处是区间**永远包含**那段语音。

## 验证

- `--check`（有模型）：`available=true`、`binary=/opt/homebrew/bin/audiocpp_cli`、
  `version=audio.cpp dev`、`model_bytes=1151272416`、`chunk_seconds=5.0`、
  `capabilities="5 s windows, partials off"`、exit 0。
- `--check`（无模型 / 目录里两个 GGUF）：exit 2，reason 点名变量并列出两个文件名。
  这一条是端到端跑出来的：引擎自己会拒绝多 GGUF 目录，但那发生在**进程已起之后**，
  服务端只能看到一句引擎输出。现在同样的拒绝发生在 spawn 之前。
- `tools/stt-stream-probe.py /tmp/qwen-long.wav`（17.06 s，852 包，实时喂入）：
  `[session:ready] {"engine":"audiocpp-qwen3-asr","language":"zh","locale":"zh-CN"}` →
  3 个 final，跨度 6000 / 5920 / 6120 ms，文本与 2026-11-02 记录的 Swift 版逐字一致：
  「今天晚上打呼噜的声音很大，大概。」「凌晨三点左右开始的，一直持续到天亮。」
  「第二天早上，他觉得很累，嗓子也有点干。」→ `[session:stopped]`，退出码 0。
- `cargo test -p steno-server --lib audiocpp`：16 个测试通过（时钟落点、队列、
  语言解析、帧解析）。
- `cargo clippy -p steno-server --bins`：新增模块零告警。

## 接线与删除（同一次改动）

| 位置 | 变化 |
|---|---|
| `deployment/config.example.toml` | `[stt]` 段落改写：`stream_command` 指向 `steno-stt-sidecar-audiocpp`，列出三个环境变量，说明目录只在「恰好一个 GGUF」时可用 |
| `tools/dev.sh` | `STENO_STT_BACKEND=audiocpp` 时 wrapper 改成 `export STENO_STT_*` + `nohup <target/debug 侧车> --socket "$1"`；不再传 `--backend/--model/--chunk-seconds`（那是 Swift 侧车的参数），也不再传 `--log-text`（改用 `STENO_LOG=info,steno_server::audiocpp=debug`，只打窗口序号与字符数，不打文本） |
| `tools/refresh-all.sh` | 第 3 步顺带检查 `steno-stt-sidecar-audiocpp` 存在（同一个 `cargo build` 产物），缺失只告警——Apple 路径不需要它 |
| `apple-stt-sidecar/Sources/Backend.swift` | 删 `case audiocpp`、`defaultEngine` 分支、`Timing.window`、三个 `audioCpp:` 参数 |
| `apple-stt-sidecar/Sources/Options.swift` | 删 `AudioCPPConfig`、`--model/--chunk-seconds/--audiocpp-bin/--model-package` 四个参数、两条拒绝规则、usage 里的对应行 |
| `apple-stt-sidecar/Sources/Session.swift`、`main.swift` | 删 `PipelineConfig.audioCpp` 与三处传参 |
| `apple-stt-sidecar/install.sh` | 删 audiocpp wrapper 与 `.conf` 的安装逻辑 |
| 删除文件 | `Sources/AudioCPPBackend.swift`、`wrappers/steno-stt-sidecar-audiocpp.sh` |

删除后 `./build.sh` exit 0；`--backend audiocpp --check` 现在报
`unknown --backend audiocpp; expected apple`；`--check`（apple）逐字段不变
（`capabilities: "phrase-timed, partials on"`）。

> **2026-11-03 补充：** 上表第 2 行的入口改了。dev 里选引擎不再拼环境变量，
> 而是 `tools/dev.sh --backend audiocpp`（`--model` 只第一次给，路径记在
> `var/stt-model.conf`；`--chunk N` 同上）。上面那套 `STENO_STT_*` 注入原样保留，
> 仍是 wrapper 实际执行的东西、也仍是环境变量直用的路径——变的是人怎么开口。
> 同时 `--backend audiocpp` 不再顺带构建 Swift bundle（它的侧车就是服务端 crate
> 的第二个 `[[bin]]`），`apple-stt-sidecar/**` 的监听在那个模式下也不再触发重载。

## 未做

- **1.7B 未测**，只有 0.6B q8 在本机；**WER 未测**，口径与验收报告一致——
  没有人工标注就不声称准确率。
- **Pi 3 跑不动**：模型常驻 1.38 GB，树莓派只有 926 MiB。这条路径让「不必须是 macOS」
  成立，但不是「到处能跑」。
