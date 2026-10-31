# 2026-10-31 — sidecar 重构：音频分发中枢 + 双向二进制协议

状态：已实施。代码是唯一事实来源，本文只记录当时的判断与实测数据。

## 变更

`apple-stt-sidecar` 从「PCM 进、JSON 行出」的单通道转换器，改成「一个音频中枢、
两条分析 lanes、双向二进制协议」：

```
任意输入 ─► AudioHub（一次转成 16 kHz mono Int16、一条 Timeline）
              ├─► SpeechAnalyzer    ─► 文本（partial / final）
              └─► SoundAnalysis     ─► 鼾声片段（结束时上报一次）
```

Rust 侧 `stt_stream` 从 std 线程 + 行协议改为 tokio 单运行时三任务（reader /
writer / 阻塞 store），一个连接同时承载音频上行与 `config`/`control` 下行。

## 为什么

1. **两条 lane 必须看到同一份音频与同一个时钟。** 各自重采样就会各自定义一个
   "here"，而指向错误秒数的鼾声标注比没有标注更糟。中枢负责唯一一次格式转换，
   `Timeline` 负责唯一的帧↔墙钟映射（样本级，`utc_ms + Δframe*1000/16000`）。
2. **8 字节头 + 显式长度**取代 12 字节 LE 头与 JSON 行：半个包、三个包粘在一起
   都能精确切分；`direction` 字节被校验，错位不再被当作负载。
3. **编排权归 server。** 语言（`[stt] locale`）通过 `config` 帧下发，关闭时用
   `control: stop` 让 sidecar 提交仍在 volatile 里的最后一句 —— 以前直接关 socket，
   夜里最后一句会丢。

## 实测（本机 macOS 26.6，arm64）

- 系统声音分类器 `.version1`：303 个标签，含 `snoring` / `breathing` / `cough`；
  窗口默认 3.0 s，允许 0.5–14.5 s。**不需要自带 CoreML 模型。**
- ESC-50 鼾声片段 40 条（`testdata/steno-eval`）：每条的最高窗口置信度
  0.63–0.98，中位 0.87。窗口往往只覆盖一段鼾声的一部分，所以默认阈值取 0.5；
  0.7 会漏掉 40 条里的 10 条。**这是候选阈值，不是准确率**：尚无负样本语料，
  因此没有任何 precision 数字。
- 合成中文语音 8.376 s：`--mode file` 输出一个 final，`start_ms=0`、`end_ms=8376`；
  socket 全双工（418 个 20 ms 包）输出 partial 序列 + 一个 final，
  `control: stop` 后最后一句仍然落盘。
- `SpeechTranscriber.Preset` 的实测构成决定了预设映射：

  | preset | reporting | attributes |
  |---|---|---|
  | `progressiveTranscription` | fastResults, volatileResults | — |
  | `timeIndexedProgressiveTranscription` | fastResults, volatileResults | audioTimeRange |

  只有 time-indexed 变体带 `audioTimeRange`。没有它就没有时间戳，所以 CLI 的两个
  预设值都映射到 time-indexed 变体。

## schema v4（重建表，但保留旧行）

`snore_events` 增加 `source` / `classifier_label` / `classifier_confidence` /
`classifier_threshold`。表是重建的（不是 `ALTER ADD COLUMN`），因为新列要按可读顺序排列
并各自带 CHECK。

一开始写的是 `DROP + CREATE`（"旧行无法回填 source"）。这个判断是错的：v4 之前
`snore_events` 只有一个写入者——rule 检测器——所以每一行**本来就是** `rule-v1`，不是猜测。
改成 `CREATE 新表 → INSERT ... SELECT → DROP → RENAME`，分类器三列对旧行保持 `NULL`
（DSP 没有 label，也没有模型置信度）。

实测（本机 `var/db/app.db` 的备份，4614 行 rule-v1 事件）：迁移后 `user_version=4`，
`source` 分布 `rule-v1=4614`，`rule_score` 4614 行全部保留。

`insert_classified_event` 接收墙钟区间，按覆盖到的 segment 逐段换算出帧偏移
（`offset = round(Δµs × capture_rate)`），跨段时写成多行并置 `continued=1` ——
音频本来就是多个文件，一行指不全。没有任何 segment 覆盖该区间时返回 0，
调用方必须把"没落库"说出来，而不是当作已存。

## 未做

- 分类器事件尚未进入 Portal 的实时推送（`snore_events` 只有在刷新列表时可见）。
- 本机（macOS，只有 HDMI/DisplayPort 输出、没有输入设备）跑不起 `steno-node`，
  所以 `tools/dev.sh --with-node` 的 UDP 链路只能在有麦克风的机器或 Pi 上验证。
- `--mode mic` 需要麦克风权限，只在 bundle 里加了 `NSMicrophoneUsageDescription`，
  尚未在真机上验证 TCC 行为。
- 负样本语料（非鼾声整夜）没有，因此 `docs/verification/` 里不做任何精度声明。
