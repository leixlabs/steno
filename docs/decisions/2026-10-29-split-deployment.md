# 采集端 / 服务端拆分（2026-10-29）

把「床头设备」和「服务器」拆成两个进程，用同一份代码的两个二进制实现：

| 二进制 | 跑在哪 | 职责 |
|---|---|---|
| `steno-node` | 床头设备（树莓派，后续可换 ESP32-S3） | 打开麦克风、组包、UDP 发送。不落盘、不检测、不监听端口 |
| `steno-monitor` | 家里常开的机器 | 接收、录音、检测、SQLite、Portal |

**动机**：把「掉电丢录音 / SD 卡损坏 / 数据库锁」这些风险从床边移走，床边只留最不容易坏的东西。

## 1. 为什么是拆分，而不是换硬件

在此之前评估过两条路：整机搬到 ESP32-S3，或换 Pi Zero 2 W 继续单机跑。

- 整机搬 ESP32 需要重写 70% 的代码（SQLite、文件系统语义、Axum/Portal 全部作废），
  而且 **microSD + FAT 的掉电风险比 ext4 更高** —— 换平台反而让最担心的问题更严重。
- 换 Zero 2 W 只是同一架构换更小更静的机器，噪音解决了，故障面没变。

拆分之后，`dsp.rs`、`detector.rs`、`dispatcher.rs`、`recorder.rs`、`storage.rs`、
`http_server.rs` 一行不用改，而且**采集端将来换成 ESP32 时，服务器侧完全不用动** ——
它只是另一个说同样协议的采集节点。

## 2. 改动范围

| 文件 | 变化 |
|---|---|
| `src/protocol.rs` | 新增。线格式编解码，两端共用，改布局两端一起编译报错 |
| `src/net_capture.rs` | 新增。UDP 接收、重排、时间轴重建 |
| `src/bin/steno-node.rs` | 新增。采集端二进制 |
| `src/config.rs` | 新增 `[source]` 段，默认 `kind = "alsa"` |
| `src/main.rs` | 按来源分支；`CaptureHandle` 统一 ALSA / net 两种句柄 |
| `src/timeline.rs` | `monotonic_now_ns()` 上移到这里，成为唯一的单调时钟原点 |
| `src/http_server.rs` | `/status` 增加 `source` 段；net 模式下 microphone 状态由节点连接态推导 |
| `tests/net_capture.rs` | 新增。无需硬件即可跑通协议与时间轴 |

**下游一行未动**：`CaptureRing::callback_push(&[i16], CaptureMeta)` 是采集层唯一的
生产者契约，网络来源复用同一个函数，所以 dispatcher 以下全部无感。

## 3. 线格式

小端，头固定 32 字节：

| 偏移 | 长度 | 字段 |
|---|---|---|
| 0 | 2 | magic `0x534E`（"SN"） |
| 2 | 1 | version（当前 1） |
| 3 | 1 | kind：1=HELLO，2=PCM，3=HELLO_ACK |
| 4 | 4 | device_id |
| 8 | 4 | session_id（节点每次启动变化） |
| 12 | 4 | packet_seq（每会话从 0 连续） |
| 16 | 8 | first_frame（首样本的绝对帧号） |
| 24 | 8 | t_mono_us（节点单调微秒） |

HELLO 载荷 40 字节：rate、channels、format、reserved、device_name[32]。

**载荷上限 1280 字节**（16 kHz mono 40 ms）。1280 + 32 = 1312 < 1472，压在
1500 MTU 的 IPv4 载荷之内 —— 超过就会 IP 分片，而 WiFi 上丢一个分片等于丢整包。

采集端默认 `period_ms = 20`，因此**一个 ALSA period 正好一个数据报**：
640 B 载荷 + 32 B 头，50 包/秒，261 kbps。

## 4. 三个关键设计决策

### 4.1 用 UDP，不用 TCP

TCP 在 WiFi 抖动时会把数据堆在重传队列里，最终表现为「一坨迟到的音频」，
但时间轴已经错位；更糟的是它把**丢包藏起来了**，而规格明确要求「不得虚构连续录音」。

UDP + 序号让丢包变成可见的缺口，正好接上现有的 `audio_gaps` 机制。
副作用是采集端永远不会被网络阻塞 —— `send_to` 不阻塞，回调线程不会被拖住。

### 4.2 时间轴由节点单调时钟重建，不用到达时间

```text
mono_start_ns = anchor_ns + (node_t_mono_us - node_t0_us) * 1000
```

`anchor_ns` 是会话首包到达时的 `timeline::monotonic_now_ns()`，所以数值落在
dispatcher 同一个时钟域里（`clock_drift_ppm` 和 wall-clock 锚点都依赖这一点）。

这个选择同时买到两件事，丢掉任何一件都是 bug：

- **抖动不会伪造 gap**：网络晚到 40 ms 不会被当成少了 40 ms 音频，
  否则 `gap_tolerance_ms = 20` 会被 WiFi 抖动刷爆。
- **漂移不会被抹平**：若改用帧数反推时间轴，`clock_drift_ppm` 会按定义恒等于 0，
  因为录音自身的误差成了标尺。用节点时钟做标尺，才能同时测到「它声称了多少帧」
  和「实际过了多久」。

真实丢包会同时抬高 `first_frame` 和节点时钟，于是 dispatcher 用**正确的时长**
登记 gap —— `net_capture` 自己**不写 gap 记录**。

顺带一个好处：**墙钟精度反而比单机更好**。Pi 没有 RTC，开机初期时间不可信；
现在权威时间来自服务器的 NTP，节点只需要一个单调计数器。

### 4.3 采集端不落盘

服务器或 WiFi 断了就丢音频，服务器如实记 gap。

在床头挂 microSD 做回填，会把这个拆分好不容易甩掉的 FAT 掉电损坏问题重新引回来。
真要回填，应该是服务器的责任，不是床边设备的。

## 5. 失败模型

| 情况 | 表现 |
|---|---|
| 单个数据报丢失 | 序号缺口 → 重排窗口（默认 150 ms）到期后跳过 → dispatcher 记 gap，时长正确 |
| 到达乱序 | 按 `packet_seq` 重排后按序交付，计 `packets_reordered` |
| 重复包 | 丢弃，计 `packets_duplicate` |
| 节点重启 | `session_id` 变化 → 重建会话、重新锚定时间轴、`stream_generation` 自增 |
| 节点重启但 `session_id` 复用 | 帧号大幅回退是判据 → 同样重建会话 |
| 服务器重启 | 节点每秒重发 HELLO，服务器重新学会格式后继续收流 |
| 格式与 `[audio]` 不符 | **拒绝**并把错误写进 `/status`，不做静默重解释 |
| 节点长时间无包 | 超过 `source.idle_timeout_ms` → `microphone = disconnected`，附 `last_error` |

最后一条是有意为之：让「床头设备半夜挂了」变成可见状态，而不是看起来像
「昨晚没打鼾」。

## 6. 部署

服务器（`/etc/steno-monitor/config.toml`）：

```toml
[source]
kind = "net"
bind_address = "0.0.0.0"
port = 8091
device_id = 1
```

采集端（`/etc/steno-monitor/node.toml`，模板见 `deployment/node.example.toml`）：

```toml
[node]
server_host = "192.168.31.10"
server_port = 8091
device_id = 1
```

```bash
# 服务器
sudo systemctl enable --now steno-monitor
# 采集端（账号必须在 audio 组，否则 ALSA 只暴露 null 设备）
sudo usermod -aG audio steno-node
sudo systemctl enable --now steno-node
```

验收：`curl -s http://<server>:8080/api/v1/status | jq .source`
应看到 `node_connected: true`、`packets_lost` 不增长、
`/tmp/…/recordings/<日期>/*.wav.partial` 的字节数 ≈ 32000 × 秒数 + 44。

## 7. 已验证 / 未验证

**已验证（2026-10-29，本机）**

- `cargo test`：新增 4 个协议/时间轴集成测试 + 111 个库测试全通过
- 端到端：模拟节点发送 15 s 音频 → 服务器写出 480,044 B 的 `.partial`
  （= 44 + 15.000 s × 32000 B），零丢包，`packets_lost = 0`
- net 模式无节点时：服务正常启动，`/status` 报 `microphone = disconnected`，
  UDP 端口在监听

**未验证（必须在树莓派上做）**

- 真实 USB 麦克风 + 真实 WiFi 下连续 8 小时的丢包率、`clock_drift_ppm`、gap 数量
- `steno-node` 在 Pi 3B 上的 CPU / 内存占用
- 采集端断电重启、服务器换 IP、AP 重启后的恢复行为
- `[audio] period_ms` 与 ALSA 实际 period 不一致时的分包行为

## 8. 下一步

1. **先跑基线**：用现有 Pi 录到的 WAV 跑 `detect-wav`，把结果存成基线。
   换任何硬件之前必须有这个，否则无法判断检测是「没跑偏」还是「阈值全废」。
2. 在 Pi 上把 `steno-node` 跑满 8 小时，统计丢包并和基线对比事件数。
3. 之后再考虑 ESP32-S3：它只是换一个说同样协议的采集端，
   服务器侧（包括本文件第 3～5 节）完全不用动。
