# crates/server — `steno-server`（`steno-monitor` 进程）

整条后端：录音、检测、存储、保留回收、转写、HTTP API 和 Portal 托管。
音频来源可插拔——本机麦克风（`source.kind = "alsa"`）或采集节点经 UDP 送进来
（`source.kind = "net"`），两条路汇进同一个 dispatcher，下游不关心样本从哪来。

```text
UDP / ALSA -> capture ring -> dispatcher -> { recorder worker, detector worker }
                                   |                    |
                                   +--> DB writer  <-----+
                             HTTP server (Axum) 读同一个数据库
```

## 进程结构

| 线程 / 任务 | 干什么 |
|---|---|
| 采集回调（CPAL 或 `net_capture` 接收线程） | 只往 `CaptureRing` 里有界拷贝 |
| dispatcher | 帧计数、**唯一**的分段轮转决策、gap 登记、给每个块打 `segment_id` / `segment_offset_frames` |
| recorder worker | 顺序追加 S16 PCM 到 `.partial`，周期补 RIFF 长度并 fsync |
| detector worker | 跑 `DetectionPipeline`，把候选事件写成 `EventDraft` |
| DB writer | 唯一持有写连接、唯一执行写操作的线程 |
| HTTP（Tokio，`server.http_threads`） | 只读同一个数据库文件和 metrics |

关停顺序（§4.3）：停流 → flush dispatcher → 录完当前段（`end_reason=shutdown`）→
flush 待写事件 → 排空 DB writer。超过 `shutdown_timeout_seconds` 就**故意**留下
`.partial`，交给下次启动的恢复扫描，而不是强行补一个完整结尾。

## 模块地图

| 文件 | 职责 |
|---|---|
| `main.rs` | 运行时装配与关停顺序；这里不实现业务逻辑 |
| `dispatcher.rs` | 分段、偏移、gap、队列分发；全进程唯一决定何时切段的地方 |
| `recorder.rs` | WAV 写入：`.partial` → 补头 → fsync → 原子 rename；录音分支不过滤、不重采样 |
| `recovery.rs` | 启动扫描：截断到整帧、补真实长度、发布 `interrupted`；头都验不过的 `.partial` 原样留着并告警；文件与行对账（孤儿 / `missing`） |
| `retention.rs` | 按字节预算回收：只删 `complete` 段、最旧优先，路径规范化后必须仍在录音根下，事件与人工复核标签保留 |
| `storage.rs` | SQLite schema v3、迁移、类型化查询；每个连接单独开 WAL + 外键 |
| `db_writer.rs` | 单写者线程；先写 `SegmentStarted` 再放行任何引用它的块 |
| `net_capture.rs` | UDP 接收端：抖动缓冲、丢包/乱序记账，用节点单调时间建立时间锚点（不用到达时间） |
| `http_server.rs` | Axum 路由、静态 Portal、Range 请求、SPA 回退 |
| `stt.rs` | 每段转写：依赖 `Transcriber` trait，Apple 后端是外部命令 |
| `stt_stream.rs` | 长驻识别器 + Unix socket，边说话边出字 |
| `control.rs` | 受 `[control]` 门控的 systemctl 生命周期控制：只跑 `systemctl <verb> <unit>`，单元名来自配置、请求只能选闭集里的 target/action；默认关闭，且 `server.bind_address` 必须是 loopback 才允许开启。经 `GET/POST /api/v1/control` 暴露 |

## 运行

```sh
cargo build -p steno-server
cargo run   -p steno-server
```

进程没有命令行参数，配置文件来自：

1. `STENO_MONITOR_CONFIG`
2. `/etc/steno-monitor/config.toml`

本地开发不用碰 `/etc` 和 `/var`：

```sh
tools/run-server.sh              # net 源，回环 HTTP，60 秒一段
tools/run-server.sh --alsa       # 直接开本机麦克风
tools/run-server.sh --lan        # 绑 0.0.0.0，手机可访问
tools/run-server.sh --reset-config
```

配置分节：`[server]` `[source]` `[audio]` `[detector]` `[stt]` `[storage]`（`[control]`
可选，默认关闭）。示例见`deployment/config.example.toml`；未知键和越界值会让启动失败并
指出键名。

## HTTP API

| 方法 | 路径 | 用途 |
|---|---|---|
| GET | `/`、`/*` | Portal 静态资源；无扩展名的未知路径回落 `index.html` |
| GET | `/api/v1/status` | 运行状态快照（未知一律 `null`，不编造健康度） |
| GET | `/api/v1/days?from=&to=` | 有数据的夜晚列表 |
| GET | `/api/v1/segments?date=` | 当天分段 |
| GET | `/api/v1/segments/{id}/gaps` | 分段内的 gap 记录 |
| GET | `/api/v1/segments/{id}/audio` | WAV，支持 Range |
| GET | `/api/v1/segments/{id}/transcript` | 单段转写 |
| GET | `/api/v1/transcripts?date=` | 当天每段转写 |
| GET | `/api/v1/utterances?date=` | 语句流 |
| WS | `/api/v1/transcript` | 实时转写推送（`ready`/`partial`/`final`/`state`/`error`/`lagged`） |
| GET | `/api/v1/events?date=` | 候选事件 |
| GET / PATCH | `/api/v1/events/{id}` | 读取 / 改复核标签（`snore`、`not_snore`、`unreviewed`） |
| GET | `/api/v1/control` | 控制中心视图：两个 unit 的 `systemctl is-active` 状态与可用性 |
| POST | `/api/v1/control` | `{"target":"server"\|"node","action":"start"\|"stop"\|"restart"}`；默认关闭，开启要求 loopback |

**没有鉴权，没有 TLS。** `server.bind_address` 默认 `127.0.0.1`；只在可信局域网里才手动
改成 `0.0.0.0`，永远不要暴露到公网。日志不写样本、转写文本或文件内容。

## 存储与保留

Schema v3 五张表：`recording_segments`、`audio_gaps`、`snore_events`、
`segment_transcripts`、`utterances`。

**实时推送**：`transcript_hub` 是一条 `tokio::sync::broadcast` 通道（容量 256）。STT worker 在
三个点发布——识别器就绪、partial、final——HTTP 层通过 `GET /api/v1/transcript` 订阅并转发。
`final` 在 `save_utterance` **之后**发布，所以客户端看到的每条句子都已经在库里；慢客户端会被
丢最旧的事件并收到 `lagged`，绝不会反向阻塞 worker。转写未运行时该端点返回 503 并说明原因，
而不是把连接挂着不说活。

- 录音硬上限 `storage.recording_max_bytes`，回收目标 `recording_cleanup_target_bytes`
  （必须更小），另有 `recording_reserve_bytes` 留给 SQLite 与系统。
- 回收定期跑（`retention_check_interval_seconds`），不只在启动和段结束时。
- 转写队列**就是数据库**：每次轮询查 `complete` 且没有 transcript 行的段。没有内存队列，
  崩了下次接着跑。

## 测试

```sh
cargo test -p steno-server
```

| 测试 | 覆盖 |
|---|---|
| `tests/http_api.rs` | 路由、日期时区边界、Range、复核 PATCH、状态里的不确定性 |
| `tests/net_capture.rs` | 抖动缓冲、乱序/丢包、时间锚点 |
| `tests/pipeline_e2e.rs` | 从采集到落库的整条链 |

`cpal` 总是参与编译，所以 Linux 上需要 ALSA 头文件：

```sh
sudo apt-get install -y build-essential pkg-config libasound2-dev
```

## 改动注意

- **分段只看时长**：`audio.segment_duration_seconds` 决定切段，检测器只做标注。
  不要因为"这段有事件"延长或提前轮转。
- **转写覆盖所有 `complete` 段**，不是只转有事件的段。
- **录音分支保持原样**：不加滤波、不重采样、不裁剪。检测器想看什么都只能在自己的分支里做。
- **不要把 `bind_address` 的默认值放宽**，也不要加"方便"的反向代理。
- 时间不确定就报 `null` / `unknown`（`time_quality`、`clock_drift_ppm`、`noise_floor_dbfs`）。

## 相关文档

- `../../deployment/config.example.toml` — 全部分节与注释
- `docs/decisions/2026-10-29-architecture.md` — 拆分与前端重写的原因
- `docs/decisions/2026-10-29-split-deployment.md` — 节点/服务端边界
- `docs/decisions/2026-10-09-t0-1.md` — 逐条决策记录
