# 控制中心：服务启停、存储池、以及“识别器未连接”为什么会一直出现

> **更正（2026-10-31）**：Apple 后端已重写为 `apple-stt-sidecar`，改用 macOS 26 的
> `SpeechAnalyzer`。SFSpeechRecognizer、端点轮换、窗口化重识别、解码窗口、以及语音识别
> 授权都已不存在；本文提到的旧路径与旧行为只作历史记录。


> 2026-10-30。记录 Portal 从「夜间复核」改造成控制中心时做的三个决定，以及
> 一次被误报掩盖的配置问题。代码优先于本文。

## 1. “识别器未连接”不是识别器的问题

Portal 上那句提示来自 `/api/v1/status` 的 `stt.connected`，它由流式 worker
收到 helper 的 `ready` 事件后才置位。排查后是四件事叠在一起：

1. 本机运行时配置 `[stt] enabled = false`（`tools/run-server.sh` 生成的默认
   值），`start_transcription` 第一行就返回 `None`，`connected` 永远是 false。
   这一步是**正确**的：功能确实没开。
2. 但 `stt` 段只有 `connected/engine/finals/partial/error`，**没有 `enabled`、
   没有 `mode`**。客户端只能用一个布尔去猜，“关着”和“坏了”在页面上长得一样，
   空状态文案还会在已开启时提示“开启 [stt] 后…”。
3. `mode = "segment"` 时 `connected` 永远不可能为 true（该模式没有长连接），
   逐段转写正常工作也会被渲染成故障。
4. `deployment/config.example.toml` 的 `[stt]` 段缺少 `mode` / `stream_command` /
   `socket_path`。键的默认值是 `mode = "stream"` + `stream_command =
   "snore-stt-stream"`，于是照示例只写 `command = "snore-stt-apple"` 的人，
   那个 `command` 根本不会被用到，实际去启动的是一个可能不存在的 launcher。

修法：`/status.stt` 增加 `enabled`、`mode`、`state`，`state` 取
`off / starting / ready / segment / error` 之一，由 `stt_state()` 派生——顺序是
“没开 → 有错 → 逐段 → 连上 → 等待”，最可行动的判断优先。示例配置补齐三个键，
`run-server.sh` 增加 `--stt` 并在 launcher 不在 PATH 时告警。

**残余的人工步骤**：macOS 的语音识别授权只能由人点一次。`tools/apple-stt/install.sh`
装好后双击 bundle 或跑 `snore-stt-apple --authorize`，否则 helper 会以
`permission was not granted` 退出，`stt.state` 会是 `error`（这次是被如实报出来的，
而不是藏在一句“未连接”里）。

## 2. `[control]` 默认关闭，且只允许 loopback

控制中心要能启停 `steno-monitor` / `steno-node`，只能通过 `systemctl`。这是
整个 HTTP API 里**唯一会改变机器状态**的端点，而这个 API 没有鉴权也没有 TLS，
录的又是私人音频。所以：

- `[control] enabled = false` 是默认值，不写就打不开按钮。
- `enabled = true` 只有在 `server.bind_address` 是 loopback 时才通过校验，否则
  启动失败并点名 `control.enabled`。`tools/run-server.sh --control --lan` 直接拒绝。
- 请求体只能给 `target`（`server`/`node`）+ `action`（`start`/`stop`/`restart`），
  两者都是封闭枚举；单元名只来自配置文件，不能从线上传。
- 没有 systemd 的机器（容器、开发机）返回 `available = false` 加原因，
  `POST` 返回 403 并带上原因，**不会**假装成功。

`node` 单元在分体部署里在另一台机器上，`systemctl` 只会说查不到，页面就显示
“不可查”——这正是真话，比一个假的“运行中”有用。

## 3. 顺手修掉：SIGTERM 之后进程不会退出

`main` 在优雅关闭的最后一步 `server_thread.join()`，但当时没有任何东西会让
`axum::serve` 返回，于是进程在 SIGTERM 后一直挂着，`systemctl stop` 只能等
`TimeoutStopSec` 再被 SIGKILL —— 分段会残留成 `.partial`。加了
`http_server::serve_with_shutdown`，用 `tokio::sync::watch` 在排空之后关监听。
实测：`kill -TERM` 后约 2 秒打出 `shutdown complete` 并退出。

## 4. 前端：从“夜间复核”到控制中心

信息顺序改成运维顺序：服务端 / 采集端 / 转写三张状态卡（带启停按钮）→ 存储池
→ 当天录音 → 候选片段 → 语句。文案去掉了“鼾声监测”“误报”这类把产品锁死在
一个用途上的说法；候选片段仍是候选，卡片上写明“检测器只做标注，不决定记录什么”。

同时修掉两个数据口径错误：语句面板一直查的是**整天**（`?date=` 忽略选中的
分段），空状态却写“这一段”；`预加载 none` 的播放器显示 `0:00 / 0:00`，看起来
像坏掉，改成 `preload="metadata"`。

`/status.storage` 增加 `cleanup_target_bytes`、`reserve_bytes`、`segment_count`、
`oldest_started_at_utc`、`newest_ended_at_utc`；`used_bytes` / `segment_count`
在数据库读不到时返回 `null` 而不是 0 —— 0 会被读成“池子是空的”。

## 5. 开启转写之后：流式识别其实一句都存不下来

把 `[stt] enabled` 打开、helper 连上、`/status` 报 `state = ready`、partial（正在识
别的那半句）也一直在往下推——但 `finals_total` 始终是 0，语句面板永远空白。链路没
断，是 Apple 那边把结果丢了。

用 `tools/stt-stream-probe.py` 直连 helper（不经过服务端）能看到：partial 正常，而
task 结束时的回调是 `kAFAssistantErrorDomain 1110 "No speech detected"`、`result = nil`。
也就是说 `endAudio()` 把整句连同已经识别出来的文字一起作废了。同一段音频走**文件**
模式（`snore-stt-apple <wav>`）完全正常，逐词时间戳都对——所以问题只在流式的收尾。

三个坑，逐个排掉的：

1. `endAudio()` 之后立刻把 request 置空 → 请求（和它持有的音频）可能在 final 到达前
   就被释放。改成放进 `retired` 队列，等结果回来再释放。**单独不解决问题。**
2. 用同一个 `SFSpeechRecognizer` 再发一个 URL 请求去补救 → 同样 1110。换成独立实例
   也还是 1110。
3. 真正的原因：**只要进程里还有一个活着的流式 task，文本请求就被拒**。所以补救前先
   把实时 task 停掉，这期间到手的音频暂存到 `pendingSamples`，补救完再按 20 ms 一块
   喂回新的 task。

补救路径本身走的是文件识别（已验证可靠），因此最终落的语句带真实起止时间和逐词
segments，不是拿 partial 拼的。没形成过任何 partial 的 task（夜里没人说话）直接跳过，
不会白白跑一次识别。

代价与取舍：每句多花约 1 秒（文件识别是收尾时才跑，不影响录音主链路）；partial 仍然
实时，所以页面上「正在识别」的体验没变。

验证：旁路实例（UDP 8093、独立库）回放一段 8.8 秒合成语音，`/api/v1/utterances`
得到 1 条、6.0 秒、15 个词级片段，页面显示「识别中 · 1 句」。

## 6. 卡片的主状态：`/status` 与心跳，不是 systemd 单元

第一版把三张卡的状态徽章都接到了 `control.units[].state`，于是没有 systemd 的机器上
「服务端」和「采集端」都显示“不可查”——明明 `/api/v1/status` 正在回答，采集端也正在
发包。这是把**启停能力**当成了**存活状态**，错的。

现在的分工：

| 卡片 | 状态来源 | 取值 |
|---|---|---|
| 服务端 | 这个页面是否拿到了 `/status`，以及 `service.state` / `recording.state` | 运行中 / 降级 / 已停止 / 不可达 / 连接中 |
| 采集端（节点） | `source.node_connected`（HELLO + 收包超时）| 在线 / 离线 |
| 采集端（本机） | `microphone.state` | 采集中 / 未连接 / 降级 |
| 语音转写 | `stt.state` | 未开启 / 等待识别器 / 识别中 / 逐段转写 / 故障 |

systemd 单元状态只在启停按钮旁边出现，且只在可查时出现（`control.available`）；不可查
时那句话明确写成“启停不可用 —— …”，不再冒充卡片的状态。

同时补了一个字段：`/status.source.last_packet_at_utc`。`node_connected` 只回答“在不在
线”，回答不了“多久没动了”——节点 20 秒前断和 1 小时前断是两个问题。采集端卡片现在显示
“最后收包 4 秒前”，`null` 表示这个进程还没收到过任何包（不是“1970 年收到过”）。

## 7. 三边的启动参数

控制中心要能回答“这台机器上到底跑的是哪个二进制、配的是哪个文件”，而不需要 SSH 进去
看进程表。

| 来源 | 谁提供 | 内容 |
|---|---|---|
| 服务端 | 进程自报 | `std::env::args()` + `Config::resolve_path()` + pid |
| 采集端 | 节点自报（新增 `Kind::Info` 数据报） | argv、config 路径、版本、server 地址、rate/channels |
| 识别器 | 服务端记录 | 实际要执行的命令 + 参数（`stream_command` + 解析后的 socket 路径） |

服务端和识别器的参数是**进程自己的事实**：`metrics.set_stt_launch()` 在 worker 启动
时写入解析后的 socket 路径，而不是把配置文件里的空值原样显示。节点在另一台机器上，
服务端读不到它的进程表，所以新增了一种 UDP 消息：`Kind::Info`（值 4），载荷是节点自己
生成的 JSON，服务端**原样存储**（只校验是不是合法 JSON），不参与解析成结构体——两端
没有共享 schema，节点加字段不需要服务端跟着改。

代价与边界：

- 老服务端收到 `Kind::Info` 会在解头时报 `UnknownKind`，每个上报计一次协议错误。同版本
  部署不受影响（两端一起构建）；混合版本需要版本协商。
- 老节点不会发这个包，页面上写的是“节点还没有上报”，不是“没有参数”。
- 上报跟着 HELLO 定时器走，服务端重启后能自己重新学到，不需要重启节点。

## 8. 端侧门控：一个两状态机，不是一个阈值

上一节算过一笔账（31.3 KB/s，一夜 900 MB），并由此否掉了“端侧剔除静音”。那个否定是错
的，两处错：

1. **整夜录制是档案要求，不是传输要求。** 只要端侧留存整夜，上行完全可以只发活跃段。
2. **自适应门控看的是“比环境电平低多少”，不是底噪的绝对值。** 我先前把“底噪 -39.5
   dBFS、0% 的帧低于 -55”读成“没有静音可省”，这个结论在方向上错了——那 100% 的帧确实是
   环境噪声。

### 8.1 第一版做错了：把判定单元当成了尾音

第一版是“逐块判定 + 2 秒 hangover”。跑在真实录音上，它把一段话切成碎片：

| hangover | 发送段/小时 | 段中位长度 | 剔除 |
|---|---|---|---|
| **2 s（第一版默认）** | **316** | **2.1 s** | 47% |
| 10 s | 70 | 17.2 s | 13% |
| **20 s（现在默认）** | **10.7** | **79.8 s** | 3.4% |
| 30 s | 5.1 | 345.6 s | 1.5% |

2.1 秒的中位段长意味着一句话还没说完就被切掉两头。那 47% 的“节约”不是省掉了静音，
是**把语音边缘削掉换来的**。

错在语义：`hangover_ms` 被当成“事件尾音补偿”，实际它决定的是**什么算一个单位**——
2 秒 = 一次爆发，20 秒 = 一段对话。

### 8.2 现在的模型

```text
           level >= floor + margin
  CLOSED ─────────────────────────► OPEN
     ▲                                │
     └────────────────────────────────┘
         连续安静 close_quiet_ms
```

- **OPEN 发送每一块**，不逐块再判。所以一段话、一个对话回合，在安静够久之前是连续的。
- **CLOSED 什么都不发**，并记住被扣下的帧区间；重新打开时用 `Kind::Gate` 报出去。
- **两条边刻意不对称**：打开很灵敏（任何高于环境的一块都开，误开只花带宽，开慢了会切掉
  攻击段）；关闭很迟钝（要连续 20 秒安静），而且**等待期间那些安静也照发**，粘在刚结束
  的那一段后面。

最后这一点是关键：**暂停在被证明结束之前是观测不到的**。如果门控扣下安静的头一秒然后话
又接上，服务端收到的是两个文件中间多一个洞——那个洞是门控制造的，不是测出来的。等
`close_quiet_ms` 再关，服务端看到的每个空洞都至少有 20 秒，这样时间轴才能区分“人停了”
和“节点丢包了”。

### 8.3 顺带修掉的真 bug：帧坐标

第一版的 `VadGate` 自己从 0 数帧，而包头里的 `first_frame` 用的是 `meta.capture_seq`——
后者在采集流重建时（设备掉线重连）会归零。于是**一次重连之后，上报的剔除区间和实际发出
的音频永久错开**。现在 `observe()` 直接收调用方的帧号，自己不再维护计数器，这类偏移从结构
上消失了，并有一条 `ranges_use_the_callers_frame_index` 测试钉住。

### 8.4 边界

| 约束 | 落点 |
|---|---|
| 剔除的音频必须有别处存 | `[archive] dir`：端侧连续 WAV，10 分钟一轮转，30 秒刷一次头（断电最多损失 30 秒，不是一夜），20 GiB 上限、最旧优先 |
| 门控不能切掉检测器会标的东西 | `[gate] margin_db` 默认 6 dB，**小于**检测器的 `noise_margin_db`（8 dB）。注意这是两个估计值的比较：检测器用带内能量，门控用宽带 RMS，两个底噪接近但不相等 |
| 跳过的区间必须显式上报 | `Kind::Gate`（值 5），载荷 `[u64 from][u64 to]`；服务端计入 `gated_frames`，**不计入** `packets_lost` |
| 不能在没有副本时开 | `[gate] enabled = true` 而 `[archive] dir` 为空时，节点**拒绝启动**并点名这个矛盾 |

### 8.5 它省不到什么（诚实的那部分）

修掉碎片化之后，在这间 -40 dBFS 底噪的屋子里的实测收益只有 **3.4%**。原因很简单：门控
真正省的是“长时间没人”，而这间屋子一直有一层风扇底噪，同时又断断续续有声音，于是状态机
大部分时间在 OPEN。

**真正要省带宽，杠杆是压缩，不是门控**（ADPCM 4:1 或 Δ+熵编码，档案语义不变，`detect-wav`
还能重放）。门控的价值在于“3am 空房间八小时”那种场景：那时它几乎不发东西，而档案仍然是
完整的。

还有一个必须先定的事：**上行门控 ⇒ 服务端没有那段音频 ⇒ 服务端转写不了那段**。“省带宽”
和“服务端整夜转写”在架构上是对立的，除非把 STT 也搬到端侧、或事后补传档案。所以门控只
应该开在“档案优先、转写次要”的夜晚。

未完成项：服务端把跳过区间计入 `gated_frames` 并单独展示，但时间轴上仍按普通捕获空洞记
录（`GapKind::Gated` 还没接）。
