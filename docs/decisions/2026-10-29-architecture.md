# 架构：lib / server / client 三层 + 语音转写

> **更正（2026-10-31）**：Apple 后端已重写为 `apple-stt-sidecar`，改用 macOS 26 的
> `SpeechAnalyzer`。SFSpeechRecognizer、端点轮换、窗口化重识别、解码窗口、以及语音识别
> 授权都已不存在；本文提到的旧路径与旧行为只作历史记录。


> 2026-10-29。本文记录把单一 crate 拆成 workspace、把前端重写成 Vite SPA、
> 以及加入语音转文字的原因和边界。

## 1. 为什么拆 crate，而不是拆目录

最初只有一个 `steno-monitor` crate，采集端、服务端、离线工具都在里面。拆成三个 crate
不是为了好看，而是为了让**依赖图表达边界**：

| crate | 依赖 | 谁用 |
|---|---|---|
| `steno-lib` | `biquad`、`cpal`、`chrono`、`serde`、`toml`（可选 `rusqlite`、`clap`、`hound`） | 采集端、服务端、离线工具 |
| `steno-server` | `steno-lib` + `axum`、`tokio`、`rusqlite`、`hound` | 只在有存储和 HTTP 的机器上 |
| `steno-node` | `steno-lib` + `clap`、`libc` | 只在床头设备上 |

关键收益是**采集端不再链接 SQLite、Axum、Tokio**。这在今天就兑现了：

- 之前交叉编译 `steno-node` 时必须给链接器补 `-lsqlite3`，否则失败；
- 拆分后 `steno-node` 的依赖只剩 cpal + serde + toml，Pi 上少了一份 1MB 的
  SQLite，和整个 HTTP 栈。

`error.rs` 里的 `Db(rusqlite::Error)` 变体被 `#[cfg(feature = "db")]` 门控，就是这个
思路的代价：错误类型是共享的，但共享不等于"所有人都得为 SQLite 付费"。

离线工具（`detect-wav`、`steno-eval`）留在 `steno-lib` 里，由 `tools` feature 门控。
它们是纯算法，和核心库同源；但采集端不该为了一个 CLI 解析器多编 3 秒。

## 2. 前端：从内联 HTML 到 Vite SPA

旧的 Portal 是三个手写静态文件（`app.js`/`app.css`/`index.html`），由 Rust 直接读目录
发出去。它的问题是状态管理靠手写 DOM 操作，加一个视图就是一笔债。

新前端在 `web/`，Vite + React + TypeScript：

- 构建产物在 `web/dist`，由服务端按 `server.web_dir` 托管（默认 `web/dist`）。
- **不嵌进二进制**。`npm run build` 是部署步骤而非编译步骤 —— 改一个文案不应该触发
  4 分钟的 `cargo build`。
- SPA 回退：无扩展名且不存在的路径回落到 `index.html`，客户端路由刷新才不会 404；
  `/assets/*` 带内容哈希，一年缓存；`index.html` 不缓存，否则发布永远到不了客户端。
- `..` 在拼路径前就被拒绝，不依赖 `canonicalize` 兜底。

接口没变，仍然只有 `GET /api/v1/{status,days,segments,events}` 加音频 Range；
新增两个转写接口。前端不做乐观更新，除了复核按钮（失败会回滚）。

## 3. 语音转文字

### 为什么是 trait + 外部进程

`SFSpeechRecognizer` 只存在于 Apple 平台。一旦服务端搬到 NAS 或软路由，Apple 后端就
失效；`whisper.cpp` 要几 GB 模型；云端 ASR 要按分钟付费且音频出网。三种都合理，
取决于机器放在哪。

所以管线只依赖 `stt::Transcriber`：加 whisper 是再写一个 impl，不碰存储、API 和录制。

### 为什么是独立 .app，而不是 FFI

TCC 只认"负有责任的进程"。裸 CLI 拿不到 Speech Recognition 授权 —— 实测会直接
`SIGABRT`，崩溃日志里写着 `This app has crashed because it attempted to access
privacy-sensitive data without a usage description`。

试过两条路，都失败：

1. `-sectcreate __TEXT __info_plist` 把 plist 塞进二进制（老技巧）—— 段确实存在
   （`otool` 能看到），TCC 依然不认；
2. 打包成 `.app` 后直接执行 `Contents/MacOS/...` —— 仍然归因到父进程，还是崩。

**可行的是经 LaunchServices 启动**：`open -n -W -a <bundle> --args <wav> <out.json>`。
这样 bundle 成为自己的责任进程，授权提示才会出现并被记住。代价是 `open -W` 没有
stdout，所以 helper 把 JSON 写到文件，外层的 `snore-stt-apple` 再 `cat` 出来 ——
对 Rust 侧，契约仍然是"跑命令、读 stdout 的 JSON"。

### 队列就是数据库

转写 worker 没有内存队列。它每 `stt.poll_interval_seconds` 查一次：

```sql
SELECT ... FROM recording_segments s
LEFT JOIN segment_transcripts t ON t.segment_id = s.id
WHERE s.status='complete' AND t.segment_id IS NULL AND duration >= ?
```

好处：崩溃重启自动续跑；慢识别器不会给录音回路施压；按开始时间排序，积压从旧到新
消化；同一个段不可能转两遍（主键就是 `segment_id`）。失败的段会写一行 `status='failed'`，
不会被无限重试 —— 重新跑要先删掉那行，这是有意的。

### 隐私

默认 `requiresOnDeviceRecognition = true`，音频不出机器。`STENO_STT_ON_DEVICE=0`
才允许走服务器识别。转写文本和音频同样敏感：数据库文件权限、Portal 的暴露面都要
按录音对待。

## 4. 没做但应该知道的

- `timeout_seconds` 目前只在文档里，helper 内部有 120 秒硬超时，Rust 侧没有强制 kill。
- 采集端只接受 S16。Mac 内置麦只给 F32/48k，所以本机调试采集端会失败（真机 Pi 的
  Razer Seiren 给 S16/16k，正常）。要支持 F32 需要在 node 侧加转换，或让 cpal 协商。
- 转写失败不会重试，也不会告警，只在 UI 上显示 `转写失败` + 原因。
- `web/dist` 需要手动 `npm run build`；没有 CI，也没有打包脚本把 dist 和二进制放一起。
