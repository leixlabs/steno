# 2026-10-31 — 线上格式变成常量：16 kHz mono 只在采集端转换一次

状态：已实施。代码是唯一事实来源，本文只记录当时的判断。

## 变更

音频格式从「两端各写一遍、必须一致」变成「一个常量、在采集端转换一次」。

- `steno_lib::config::WIRE_RATE_HZ`（16 000）/ `WIRE_CHANNELS`（1）/ `S16_LE`
  成为 wire、WAV、检测器、STT 队列和 sidecar 共用的唯一格式。
- `[audio] preferred_rate_hz` / `channels` → `device_rate_hz` / `device_channels`：
  它们**只描述麦克风**，不再描述录音、也不参与线上格式。
- 单机路径（`source.kind = "alsa"`）新增 `LocalWireCapture`：CPAL 回调仍然只把
  *设备*块推进 `CaptureRing(device)`，一个转换线程把它变成
  `CaptureRing(16 kHz mono)`，dispatcher 从这里读。回调里不做重采样——那会分配和
  滤波，不允许发生在音频线程上。
- `mono_channel_index` 真正生效：`WireConverter` 按该下标**取一路**，不再是
  "所有声道求平均"。设备实际开出的声道数不够时报错，不静默退回 0。
- `net_capture` 丢掉 `rate_hz` / `channels` 配置项，只校验 HELLO 是否等于 wire 常量。
- `/status.microphone` 同时报告两件事：`capture_rate_hz`/`channels`（录下来的格式，
  恒为 16 kHz mono）和 `device_rate_hz`/`device_channels`（设备自己跑的格式）。

## 为什么

1. **两端手抄一遍的格式是个静默失败。** 原来的设计里，服务端在 `[audio]` 里声明它
   愿意录什么，node 在 `[audio]` 里声明它要发什么，对不上就**拒收**。拒收本身是
   对的（重采样成"看起来能用"更糟），但它的表现是：节点照常发 HELLO、`/status`
   显示在线、整夜没有一帧音频。真机上就是这样丢过一晚。格式变成常量后，没有第二
   份可以写错的东西。
2. **单机路径从来没归一化过。** net 路径的节点会转换，server 自己开麦时不会：
   48 kHz 立体声麦克风直接录成 48 kHz 立体声 WAV，检测器自己去重采样，识别器被
   喂进一个没人告诉过它的采样率。同一个产品两种行为，取决于麦克风插在哪台机器上。
3. **`mono_channel_index` 一直是死键。** 它在配置里、在校验里、写进 `channel_mapping`
   列，但 downmix 用的是"所有声道求平均"，从不读它。平均值对床头麦克风尤其错：
   一只胶囊对着房间、一只对着墙，平均等于把反射混进信号，还损失 3 dB 直接声。
4. **检测器的重采样器因此可以不再工作。** 采集端归一化后，`DetectionPipeline` 的
   `Resampler` 永远是 `Pass`，它携带的 group delay 也不再需要补偿。检测器保留这个
   能力（`detect-wav` 仍要分析任意采样率的 WAV），但线上路径不再走它。

## 代价

- 旧的 `config.toml` / `node.toml` 会在启动时报 `unknown field` 并退出。
  **这是有意的**：项目规则是不许静默回退，未知键必须点名。迁移就是改两个键名，
  并把 `mono_channel_index` 写成真正想要的那一路。
- 单机路径多了一个线程和第二个 `CaptureRing`。`capture_ring_ms` 现在描述的是
  转换**之后**那个环；设备环按同样的毫秒数分配，所以延迟上限没有变化，只是多了一次
  块级别的搬运。
- `channel_mapping` 列在 net 路径下写 `mono` 而不是 `mono:<index>`：服务端确实不
  知道节点选了哪一路，写具体下标就是发明一个没观测到的值。节点在自己的 INFO 里
  报 `device_channels`，需要知道就去那里看。

## 验证

- `cargo test --workspace --all-features`：全绿。新增用例覆盖"选声道真的选"
  （一路 +8000 一路 -8000，取到的必须是纯 +8000 或纯 -8000，不是 0）、"下标越界被拒"、
  "本地转换线程发布 wire 格式并保留设备时间戳"。
- `pipeline_e2e::a_48k_stereo_device_still_records_16k_mono`：48 kHz 立体声设备、
  1 号声道带 320 Hz 音、0 号声道静音 → dispatcher 分发出去的是
  `(16000, 1)`、3200 帧、峰值 > 1000。取错声道会得到静音，求平均会得到半幅。
- 真机（net 路径，本机 macOS）：回放 60 s 的 16 kHz WAV → 3000 包零丢失，
  落盘 5 个 5.98 s 的 WAV 全部 `16000 Hz / 1 ch`；伪造一个宣告 48 kHz 的 HELLO →
  `protocol_errors` +1、`last_error` 明确写出"the wire format is 16000 Hz 1 ch"、
  零帧落盘。
- 单机路径在开发机上没能实跑：`select_device` 只接受 S16，而这台机器的输入设备
  （Insta360 Link 2 Pro / MiRemoteV 2ch）只提供 F32。这是既有约束，与本变更无关；
  该路径由上面的 e2e 用例覆盖。

## 未做

- sidecar 的 `AudioHub` 仍然保留"任意输入 → 16 kHz mono"的转换能力。线上路径
  现在保证输入已经是这个格式，但 `--mode mic` / `--mode file` 仍需要它，所以没有删。
- `[audio]` 里属于设备的那几个键（`period_ms`、`buffer_ms`、`capture_ring_ms`、
  `rumble_highpass_hz`）在 `source.kind = "net"` 的服务端是死键，和以前一样。
  真正想要的是按 source 拆分配置节，那是另一次改动。
