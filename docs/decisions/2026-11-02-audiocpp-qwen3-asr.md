# 2026-11-02 — 用 audio.cpp 跑 Qwen3-ASR，补齐第二个 backend

状态：已实施。`--backend audiocpp` 可运行；`--backend apple` 行为不变，wire 与 Rust 侧零改动。

> **2026-11-03 修正：** 本文写的 `AudioCPPBackend.swift` 没有用到任何 Apple 能力，
> 它的职责已迁到 `crates/server/src/audiocpp/`，作为 `steno-stt-sidecar-audiocpp`
> 随服务端一起构建（见 `2026-11-03-audiocpp-sidecar-rust.md`）。下面的四个坑、
> 窗口落点与拒绝规则全部原样保留，仍然有效；Swift 实现待与接线一起删除。

## 变更

| 文件 | 变化 |
|---|---|
| `Sources/AudioCPPBackend.swift` | 新增。一个长驻 `audiocpp` 子进程：stdin 喂 s16le PCM，stdout 按行读回文本；`LineReader`、`AudioPipe`（无界积压 + 帧记账）、`AudioCPPLanguage` 也在这个文件 |
| `Sources/Backend.swift` | `BackendID.defaultEngine` 对 audiocpp 给出 `audiocpp-qwen3-asr`；`make`/`probe`/`install` 增加 `audioCpp: AudioCPPConfig` 参数；删除 `BackendError.notCompiledIn` |
| `Sources/Options.swift` | 新增 `--model`、`--chunk-seconds`、`--audiocpp-bin`、`--model-package`；四条拒绝规则（见下） |
| `Sources/Session.swift` | `PipelineConfig` 增加 `audioCpp`，透传给 `BackendID.make` |
| `Sources/main.swift` | `--check` / `--install-assets` 带上该配置；顶部注释改成「两个引擎」 |
| `tools/dev.sh` | `STENO_STT_BACKEND` / `STENO_STT_MODEL` / `STENO_STT_CHUNK` 注入 dev wrapper；命令行改用 `printf %q` 生成，不再 `sed` 替换路径 |
| `wrappers/steno-stt-sidecar-audiocpp.sh` | 新增。和 `steno-stt-sidecar` 同一契约（一个参数：socket 路径，立刻返回），多带 `--backend audiocpp --model --chunk-seconds`；模型路径读 `$PREFIX/lib/steno-stt-sidecar-audiocpp.conf`，运行时可用 `STENO_STT_MODEL` 覆盖，缺模型直接 exit 2 |
| `install.sh` | 装第三个 wrapper；`STENO_STT_MODEL` / `STENO_STT_CHUNK` 写进上面那份 conf，**不给就不动**已有 conf |

## 为什么是一个长驻进程

一次热启动的 `audiocpp` 要花 5.8 s 墙钟里约 2.5 s 才解码出第一个字（Metal library init + 1.15 GB GGUF 加载）。按窗口起进程的话，启动引擎的 CPU 会超过识别本身。所以整夜一个进程，窗口靠引擎自己的定长分块。

## 引擎在流式模式下的真实行为（实测 audio.cpp 0.9.1 / M4 / Qwen3-ASR-0.6B q8 / 5 s 窗口）

```
audio_input=stdin format=s16le rate=16000 channels=1   ← 模型加载完，这才是 ready
partial_text=<这一窗的完整文本>                          ← 每窗一行，不是「半截」
text_output=<整场文字拼接>                              ← EOF 时一行，本实现丢弃
audiocpp_cli failed: <原因>                            ← 启动/配置失败
```

四个坑，全部在实测里踩到：

1. **`partial_text` 不是 partial。** 它是一窗的定稿，下一行不会修订它。所以这里转成 `final`，`capabilities.partials = false`。界面上几秒没字是预期行为，不是卡住。
2. **`text_output` 是整场拼接。** 存下来等于把整夜再写一遍成一句。
3. **静音窗什么都不打印。** 行数 ≠ 窗数，所以窗口边界不能用「数行」推，只能用自己记的 `windowIndex`。纯数字静音与 -55 dBFS 房间噪声各跑 20–30 s，都没有产生任何文本（没有幻觉），但计数因此不可靠。
4. **尾部窗只在 EOF 前吐出来。** 先关 stdin 再排空队列，会把 `--mode file` 的绝大部分音频丢掉：第一次实现就是这样，17 s 的文件只出来 1 个窗口、1 句话。现在 `finish()` 的顺序是「停止喂 → 等 writer 排空 → 关 stdin → 等 EOF」。

## 时间边界为什么允许溢出

窗口 `k` 是引擎自己网格上的 `[k*N, (k+1)*N)`，`k` 是准的（一行一窗、按序）。不准的是「引擎读到第 `k` 窗末尾的时刻」：文本比边界晚约 1.3 s 到达，此时 writer 已经把更多音频推进了管道。于是 `end` 取「文本到达时引擎已被交付到的帧」，`start` 取 `k*N`。

代价是相邻窗口重叠约一个解码延迟（实测 5 s 窗口给出 6.0–6.2 s 的区间）。收益是区间**永远包含**那段语音；反过来收紧区间，就会把某句话指到一片寂静上。`Timing.window(5)` 已经把精度声明出去了，读者知道这是窗边界而不是音节边界。

## 四条拒绝规则（都在 parse 阶段，不等到第一窗）

- `--backend audiocpp` 缺 `--model` → 拒绝。没有权重的识别器会报 `ready` 然后整夜不出声，这和「没人说话」在库里长得一样。
- `--model` / `--chunk-seconds` / `--audiocpp-bin` / `--model-package` 配 apple → 拒绝（和 `--preset` 配 audiocpp 同一条规矩）。
- `--chunk-seconds` 不在 1…30 → 拒绝。
- `--locale xx-YY` → 拒绝并列出支持的语言码。**必须自己校验**：引擎对 `--language xx-YY` 照单全收，照样出一段转写，只是用的不是那个语言。

## 验证（本机 macOS 26.6.2，arm64；audio.cpp 0.9.1；模型 `/Users/lei/dev/projects/2601-monosub/models/asr/qwen3_asr`）

- `--check --backend audiocpp --model …`：`available=true`、`binary=/opt/homebrew/bin/audiocpp`、`version=audio.cpp dev`、`language=zh`、`capabilities="5.0 s windows, partials off"`、`model_bytes=1151272416`。
- `--check`（apple）逐字段不变。
- `--check --backend audiocpp`（无 `--model`）、`--check --model …`（无 backend）、`--preset` 配 audiocpp、`--chunk-seconds 60` 均在第一行拒绝并打印 usage。
- `--mode file /tmp/qwen-long.wav`（17.05 s，三句 + 2 s 静音）→ 3 个 final：
  `[0, 8192] 今天晚上打呼噜的声音很大，大概。` / `[5000, 12288] 凌晨三点左右开始的，一直持续到天亮。` / `[10000, 17055] 第二天早上，他觉得很累，嗓子也有点干。` 全文无丢失（上一版因为关 stdin 早，只出第一句）。
- `tools/stt-stream-probe.py`（852 个 20 ms 包，17.1 s）：`ready` → 3 个 final（跨度 6220 / 6060 / 6040 ms）→ `stop` 正常收尾，`272640 frames, 0 discontinuities`。
- 170 s 连续压测（10 遍同一段语音，realtime 喂入）：34 个 final（= 170/5），相邻间隔无一超过 8 s；`audiocpp` RSS 稳定在 1.38 GB（模型常驻，不随窗口数增长），sidecar 稳定在 12 MB。
- 真实部署切换：`tools/dev.sh stop` → 写入 audiocpp wrapper → `tools/refresh-all.sh --no-web --no-node`。`/api/v1/status` 报 `state=ready, connected=true, engine=audiocpp-qwen3-asr`；进程树是 `steno-monitor → steno-stt-sidecar(app) → audiocpp_cli --mode streaming --audio -`，引擎参数含 `--audio-chunk-seconds 5 --language zh`。

## 事后修正：wrapper 里的模型路径不能是替换出来的

第一版把 `--model` 的值用 `sed` 替换进 wrapper。`refresh-all.sh` 会不带环境变量重跑 `install.sh`，于是那样一次重装把占位符换成了空串，wrapper 在 `-z "$MODEL"` 处 exit 2，服务端只剩一句 `launcher exited with exit status: 2`（真正的原因行 `no Qwen3-ASR package at ""` 在同一份 `var/server.log` 的上一行，但 `/status` 只有摘要）。

现在路径写在 `$PREFIX/lib/steno-stt-sidecar-audiocpp.conf`，`install.sh` 只有显式给了 `STENO_STT_MODEL` 才覆盖；wrapper 先读 conf 再看环境变量。回归验证：`PREFIX=$HOME/.local ./install.sh`（不带任何环境变量）之后 conf 仍是原来那行，wrapper 照常启动（exit 0 并拉起 sidecar），而不是 exit 2。

## 未做 / 已知边界

- **`flush` 是空操作**，只记一行日志：引擎按窗解码，没有「把手上这半窗跑完」的接口。语义没有丢，只是这次真的做不到。
- **文件模式会积压**：`FileSource` 按 ~16× 实时喂，引擎只有 ~4×，所以 1 h 分段会排到几十秒的积压。队列**不丢弃**（丢块＝时间线上一个没人知道的洞），只在 60 s 时警告一行。放慢喂入还是放大队列，等真的用文件模式跑整段再定。
- **1.7B 未测**（只有 0.6B q8 在本机）。`--model` 指向哪个包就跑哪个，显存/内存相应翻倍。
- **WER 未测**：没有人工标注的夜间语音，`docs/verification/acceptance-report.md` 的口径不变——没有标注就不许声称准确率。窗口化必然在边界切词（实测把「打呼噜」切成「猪的」），这是 `--chunk-seconds` 的取舍，不是 bug。
- **鼾声 lane 仍是 SoundAnalysis**，与 backend 无关：audio.cpp 有 VAD，没有鼾声分类器。
- **按夜选 backend** 仍未做：换引擎＝换模型文件，选择留在启动处（wrapper / unit / `tools/dev.sh` 的环境变量）。
