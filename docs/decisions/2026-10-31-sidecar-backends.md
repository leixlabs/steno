# 2026-10-31 — sidecar 抽出 backend 抽象，为 audio.cpp 做准备

状态：已实施。apple 后端行为不变；`audiocpp` 只有接口位置，没有实现。代码是唯一事实来源。

## 变更

`apple-stt-sidecar` 里「转写」这一件事从具体实现变成一份契约：

```
AudioHub ──► TranscriptionBackend（--backend apple|audiocpp）
                │  音频进，段落出；返回值是「帧区间」，不是 utc_ms
                ▼
           TranscriptTarget ──► TranscriptBridge ──► EventSink（socket / 终端）
                                  （唯一的 Timeline）
```

| 文件 | 变化 |
|---|---|
| `Sources/Backend.swift` | 新增：`BackendID`、`TranscriptionBackend`、`TranscriptTarget`、`BackendCapabilities`、`BackendError`，以及 `make`/`probe`/`install` 三个分派点 |
| `Sources/SpeechAnalyzerBackend.swift` | `Transcriber.swift` 改名而来；类名 `SpeechAnalyzerBackend`，错误枚举 `SpeechAnalyzerError` |
| `Sources/Session.swift` | `Pipeline` 持有 `backend` 而不是 `transcriber`；新增 `TranscriptBridge`：帧→墙钟、空 final 过滤、partial 限流 |
| `Sources/Options.swift` | 新增 `--backend`；`--engine` 缺省值改为由 backend 给出；`--preset` 只对 apple 生效 |
| `Sources/main.swift` | `--check` / `--install-assets` 交给 backend 回答；`config` 帧里的 `preset` 对非 apple 后端明确报「忽略」 |

## 为什么

1. **差的是时钟，不是文本。** 原来 `Transcriber` 同时知道 `Timeline`、`EventSink`、鼾声 lane 名字
   （`describeSnoreLane`）和 UTC 换算。audio.cpp 的结果是「按 chunk 出的整段文本」，它要么自己
   学一遍时钟，要么就得被伪造时间戳。现在 backend 只会说「这段是 48000..<64000 帧」，
   帧→墙钟只发生在 `TranscriptBridge` 一处 —— 和鼾声 lane 用的是同一个 `Timeline` 实例，
   两条 lane 不可能对「这里」有分歧。
2. **不能说的东西要能表达。** audio.cpp 给不出词级时间，`start_ms` 只能是 chunk 边界。
   于是 `BackendCapabilities.timing` 有 `phrase` / `window(seconds:)` 两种，
   `--check` 和启动日志都写明是哪种。宁可标注「粗」，不能让读者以为那是词级时间。
3. **选错 backend 必须响。** `--backend audiocpp` 在本次提交里 `throw BackendError.notCompiledIn`，
   不返回空 stub —— 空转写和「这一夜没人说话」在数据库里长得一模一样，那是这个程序最不能有的失败。
   同理 `--preset` 配非 apple 后端在解析阶段就拒绝，而不是收下再当没听见。
4. **wire 不动。** 协议、`ready`/`status` 帧字段、日志行的形状都没变，
   Rust 侧零改动；`[stt] engine` 仍然是兜底值，sidecar 自报的 `engine` 优先。

## 边界（backend 不允许知道什么）

- 墙钟、socket、日志、鼾声 lane 是否存在。
- `submit` 跑在 hub 的串行队列上，必须立刻返回；带子进程的实现在自己的队列里排队，
  hub 不等它，丢掉的块就是丢掉了。
- `flush` = 提交仍在 volatile 里的结果，不是结束会话。对 chunk 引擎就是「把手上这段跑完」。
- `finish` 必须在最后一个结果送出去之后才返回（进程紧接着就退出）。

## 验证（本机 macOS 26.6.2，arm64，Swift 6.3.3）

- `./build.sh`：干净编译。
- `--check`：`backend=apple`、`engine=apple-speechanalyzer`、`locale_resolved=zh_CN`、
  `assets=supported`、`snore.available=true`（303 标签）。
- `--check --backend audiocpp`：`available=false` + `reason` 指向 `BackendError`，snore 段照常。
- `--backend audiocpp --preset transcription --check` → 解析阶段拒绝并打印 usage；
  `--backend whisper` → `unknown --backend whisper; expected apple, audiocpp`。
- `--mode file /tmp/speech16k.wav`（`say` 合成中文 6.576 s）：
  `{"engine":"apple-speechanalyzer","language":"zh_CN","segments":[{"start_ms":0,"end_ms":6576,...}]}`
  —— 与重构前同一句、同一区间。
- `tools/stt-stream-probe.py`（328 个 20 ms 包）：`ready` → 6 个 partial → `stop` 后一个 final
  `1793453945516..1793453952076`（跨度 6576 ms），`104960 frames, 0 discontinuities`。
- `control: flush`（自写探针，只喂前 3 s 音频）：flush 之后、stop 之前落一个 final
  `…967754..…970754`（3000 ms），说明 `Pipeline.flush → backend.flush` 这一步没丢语义。

## 未做

- **audio.cpp 后端本身。** 缺口不在接口而在三件事：GGUF 模型路径与 SHA 校验、chunk 缓冲
  与 `flush` 的对应关系、以及它给不出词级时间时 `start_ms` 的语义（现在由
  `Timing.window` 声明，但没有任何一行 audio.cpp 代码）。
- **按夜选择 backend。** 选择发生在命令行（wrapper / systemd unit），因为换引擎等于换模型文件。
  真要让 server 按夜指派，得在 `config` 帧加字段 —— 那是 Rust 与 wire 的改动，不在本次范围。
- **鼾声 lane 仍是 SoundAnalysis。** audio.cpp 有 VAD，没有 snore 分类器；
  两条 lane 目前只有转写这一侧是可替换的。
