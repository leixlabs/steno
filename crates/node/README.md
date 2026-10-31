# crates/node — `steno-node`（采集端）

床头设备上唯一跑的东西：打开麦克风，把 PCM 切成数据报发给服务端。**没有**磁盘写入、
没有数据库、没有 HTTP 监听、不跑检测、不转写。

```text
ALSA callback -> CaptureRing -> [steno-node] -> UDP -> steno-monitor
```

## 为什么这么切

床边设备只需要做好一件事，坏了换一个就行。把它做成纯采集端换来三个性质：

- **依赖极小**：`steno-lib` + `cpal` + `serde`/`toml`，不链接 `rusqlite`、`axum`、`tokio`。
  交叉编译到 Pi 时不用再给链接器补 `-lsqlite3`。
- **断电不毁录音**：录音、校验、保留策略全在服务端，SD 卡上没有半个 WAV。
- **坏链路是可见的**：丢包变成服务端记录的一段 gap，而不是本地悄悄补齐。

## 运行

```sh
cargo build -p steno-node                 # 调试
cargo build -p steno-node --release       # 部署用
./target/release/steno-node [config.toml]
```

配置查找顺序（先命中先用）：

1. 第一个命令行参数
2. `STENO_NODE_CONFIG`
3. `/etc/steno-monitor/node.toml`

没有别的子命令或开关；`deployment/node.example.toml` 是唯一模板。

## 配置

```toml
[node]
device_name = "bedroom"        # 只用于服务端状态页展示，不参与路由
server_host = "192.168.31.10"
server_port = 8091
device_id = 1                   # 0 = 服务端接受第一个自报的节点
hello_interval_ms = 1000        # 100..=10000

[audio]
alsa_device = "Razer Seiren, USB Audio"   # 空 = 用唯一的 USB 采集设备；多个则启动失败
device_rate_hz = 16000          # 给设备的提示，不是录音格式
device_channels = 1             # 向设备申请的声道数
mono_channel_index = 0          # 哪个设备声道变成单声道流（必须 < device_channels）
period_ms = 20
buffer_ms = 400
capture_ring_ms = 500
```

**线上格式是常量，不是配置**：16 kHz、单声道、S16_LE，定义在
`steno_lib::config::WIRE_RATE_HZ` / `WIRE_CHANNELS`。节点在发送前完成
"设备格式 → 线上格式"的唯一一次转换（`WireConverter`），所以服务端、检测器、
识别器和 sidecar 收到的永远是同一种东西。`[audio]` 里的一切都只描述麦克风。

由此：

- 没有"两端 `[audio]` 必须写一致"这条约束了——服务端根本没有对应的键可写。节点宣告
  非线上格式的 HELLO 仍然会被**拒绝**（那是"这台机器的二进制比服务端旧"的信号），
  但不会重采样成"看起来能用"的格式。
- `mono_channel_index` 是**选一路**，不是求平均：床头麦克风常常一只胶囊对着房间、
  一只对着墙，平均等于把反射混进来。设备实际开出的声道数少于该下标时启动报错，
  不会悄悄退回 0 号。
- `period_ms = 20` 时一包是 640 B PCM + 32 B 头，安全落在 1500 B MTU 内；调大就有
  WiFi 分片风险。

## 运行时行为

1. 协商设备（与单机版同一套 `audio_capture`）。设备自己的默认配置优先——那是 ALSA
   唯一保证的；再依次尝试配置的采样率 → 48 kHz → 44.1 kHz，声道 → 单声道。
   再不行就报错退出。**开出来的格式只影响转换器，不影响录音。**
2. 发 HELLO 宣告线上格式，之后按 `hello_interval_ms` 重发。服务端重启会忘掉已宣告的
   格式，在收到新的 HELLO 之前拒收 PCM，所以这个间隔必须短。
3. 每个采集周期发一个 PCM 数据报，带 session id、包序号和首样本的绝对帧号。
4. `capture_ring_ms` 是**延迟缓冲**，不是录音缓冲：网络卡住时放不下的音频直接丢弃，
   服务端记为 gap。
5. 音频时间不来自墙钟：设备可能没有 RTC，所以数据报里只有节点自己的单调计数和帧号，
   墙钟锚点由服务端建立。
6. `SIGINT` / `SIGTERM` → 停止采集、把环里剩下的发完、退出。

## 部署

```sh
tools/deploy-node.sh --host 192.168.31.58 --server-host 192.168.31.152
tools/deploy-node.sh --dry-run                             # 默认主机 pi.local
```

脚本用 `cargo-zigbuild` 交叉编译 `armv7` 并钉住 glibc 2.28（Pi OS buster），然后装二进制、
配置、服务用户和 systemd 单元。可重复执行，等于原地升级。

常驻安装用 `deployment/systemd/steno-node.service`，服务用户必须在 `audio` 组，否则 ALSA 只暴露
`null` 设备——表现是"进程活着但录不到任何东西"。

## 本机自测（不需要硬件）

```sh
tools/run-server-mac.sh                 # 服务端以 source.kind = "net" 启动，等 UDP 8091
tools/replay-wav.py night.wav 127.0.0.1:8091
```

`replay-wav.py` 发出的包与 `steno-node` 完全同构，所以分段轮转、gap、检测、转写都会
按真实路径跑一遍。要验证真机麦克风再上 `deploy-node.sh`。

## 相关文档

- `../../deployment/node.example.toml` — 带注释的完整模板
- `docs/decisions/2026-10-29-split-deployment.md` — 拆分部署的取舍
- `docs/operations/2026-10-29-pi-deploy.md` — Pi 上的安装、权限与故障排查
