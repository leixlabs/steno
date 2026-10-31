# crates/lib — `steno-lib`

平台中立的核心库：配置契约、DSP、检测状态机、采集环、UDP 线格式、时钟换算和离线分析。
服务端（`crates/server`）和采集端（`crates/node`）都依赖它，所以它**不碰**文件系统、
SQLite、HTTP 和进程生命周期。

> 目录叫 `lib`，Cargo 包名是 `steno-lib`：目录名只是为了短，包名前缀才是身份，
> `use steno_lib::...` 与目录名无关。命名约定见根目录 `AGENTS.md` 的 "Naming"。

## 这个 crate 的边界

- **不链接服务端栈。** 没有 `axum`、`tokio`。`rusqlite` 只在 `db` feature 下出现，
  且只为了 `error::Error::Db` 这一个变体存在。
- **采集回调路径零分配。** `audio_capture` 的回调只做有界拷贝进预分配的 SPSC 环，
  不跑 DSP、不写日志、不阻塞。
- **校验即失败。** `config` 里没有"猜一个合理值"的分支；未知键、越界值、不支持的
  采样格式都以键名报错并终止启动。唯一的回落是采集设备协商（见下）。

## 模块地图

| 文件 | 职责 |
|---|---|
| `config.rs` | `Config` 全套结构与逐字段校验（TECH_SPEC §11）；`SUPPORTED_CAPTURE_RATES = [16k, 32k, 44.1k, 48k, 96k]`、`DETECTION_RATE_HZ = 16000` |
| `audio_capture.rs` | `CaptureRing`（所有音频生产者唯一的写入契约）+ CPAL 设备选择与格式协商 |
| `bounded_queue.rs` | `SpscRing`（采集回调是唯一生产者，溢出只加计数）+ `BoundedQueue`（音频槽位之上预留控制槽位，控制消息永不丢弃） |
| `dsp.rs` | DC 阻断、带通、10 ms 分帧、噪声底估计；构造后无分配，跨 block 带状态 |
| `resampler.rs` | 精确有理数多相重采样（44100 → 16000 就是 441:160，`f64` 比率表达不了，所以自己实现） |
| `detector.rs` | `DetectionPipeline` + 候选事件状态机；`DETECTOR_VERSION = "rule-v1"` |
| `timeline.rs` | 单调时钟锚点、gap 记录、漂移估计、`time_quality` |
| `protocol.rs` | 节点 ↔ 服务端 UDP 线格式：32 B 头、40 B HELLO、PCM 载荷 ≤ 1280 B、magic `0x534E`、version 1 |
| `metrics.rs` | 供 `/status` 读取的原子计数器；音频路径上不分配 |
| `error.rs` | 全库统一错误类型；进入 HTTP 层前降级为短字符串，不带样本或转写内容 |
| `detect_wav.rs`、`eval.rs` | 离线读 WAV 与打分，**仅 `tools` feature** |

## Feature

| feature | 打开什么 | 谁打开 |
|---|---|---|
| （默认，空） | 只有上面不带门控的模块 | 采集端 |
| `db` | `dep:rusqlite` + `Error::Db` | `steno-server` |
| `tools` | `detect_wav`、`eval` 模块，`dep:clap`、`dep:hound`，以及两个二进制 | 离线分析的人 |

两个二进制是 `required-features = ["tools"]`，所以 `cargo build --bin detect-wav`
**不带** `--features tools` 会直接失败——这是设计，不是意外。

## 构建与测试

```sh
cargo build -p steno-lib                      # 只编核心库
cargo build -p steno-lib --features tools     # 带动 detect-wav / steno-eval
cargo test  -p steno-lib --all-features       # 含 tools 门控的测试
```

`cargo test -p steno-lib` 单独跑会跳过 `tests/detect_wav.rs` 和 `tests/steno_eval.rs`，
因为它们带着 `required-features`。要全量就用 `--workspace --all-features`。

| 测试 | 覆盖 |
|---|---|
| `tests/config_example.rs` | 解析并校验 `deployment/config.example.toml`，模板不能和校验器脱节 |
| `tests/resampler_metrics.rs` | 重采样比率/延迟与计数器 |
| `tests/detect_wav.rs` | 离线检测（需 `tools`） |
| `tests/steno_eval.rs` | 打分规则（需 `tools`） |

## 离线工具

```sh
# 用线上同一套 DetectionPipeline 重放一段录音，输出候选事件
cargo run -p steno-lib --features tools --bin detect-wav -- night.wav
cargo run -p steno-lib --features tools --bin detect-wav -- night.wav --format csv -o events.csv
cargo run -p steno-lib --features tools --bin detect-wav -- night.wav --frames   # 逐帧特征打到 stderr

# 拿标注语料问"检测得对不对"
cargo run -p steno-lib --features tools --bin steno-eval -- \
  --manifest testdata/steno-eval/labels.json
```

两者都不开麦克风、不连数据库、不需要跑服务。`detect-wav` 回答"服务会存下什么"，
`steno-eval` 回答"那对不对"——检出的是**候选**，不是诊断。

## 改这里之前

- **事件时间用 segment 内的 frame offset**，不是墙钟。改掉它会让已存 WAV 的边界对不上。
- **检测固定 16 kHz**。换检测率不是改一个常量：滤波器、分帧、阈值全按 16 kHz 标定。
- **先想采集端会不会因此变重**。往 `steno-lib` 加依赖前，确认 `steno-node` 不会顺带
  链接上 SQLite 或 HTTP；需要数据库的东西要放进 `db` feature。
- **别"顺手"补默认值**。`config` 的严格性是产品约束：配错就该起不来。
- `DETECTOR_VERSION` 变了要连带更新文档与标注对比，否则新旧事件不可比。

## 相关文档

- `docs/decisions/2026-10-29-architecture.md` — 为什么拆 crate、为什么前端独立
- `docs/spec/TECH_SPEC.md` — §4 线程、§5 检测、§11 配置（历史规格）
- `../../deployment/config.example.toml` — 配置契约的可执行样板
