# tools — 开发与部署脚本

这里放不方便塞进 Rust 二进制的东西：本机跑服务端、把节点装到 Pi 上、无硬件回放音频、
开发 macOS 转写后端。都不是 `cargo build` 的一部分，也不由 systemd 安装。

| 文件 | 一句话 |
|---|---|
| `dev.sh` | **开发监督器**：web/server/sidecar/node 四个角色，可按角色启动/重载，一个 Ctrl-C 收工 |
| `dev-panel.py` | 控制台面板：服务表 + 流水线状态 + 最近工作流事件（也负责从日志里切事件） |
| `refresh-all.sh` | **一键刷新所有运行时**：Portal → 服务端 → sidecar → 节点 → 重启 → 自检 |
| `run-server-mac.sh` | **macOS** 上前台跑 `steno-monitor`：项目内 `var/` 运行目录，不碰 `/etc`、`/var` |
| `deploy-node.sh` | 交叉编译 `steno-node` 并一次性装到树莓派 |
| `replay-wav.py` | 把 WAV 按 realtime 灌进 UDP 协议，替身节点 |
| `stt-stream-probe.py` | 用 WAV 驱动流式识别 sidecar，替身服务端（打印每一帧） |
| `stt-check.py` | 用 TTS 语料给识别效果打分：`--file` 整段 vs `--stream` 生产链路，CER 对比 |
| `stt-watch.py` | **实时看服务端识别结果**：WS 订阅 `/api/v1/transcript` + 轮询 `/api/v1/status`，不需要浏览器 |

## dev.sh

改代码时用它，别用 `refresh-all.sh`（后者是"重新部署"，一条命令要跑一分钟）：

```sh
tools/dev.sh                          # web + server + sidecar，全都在监听
tools/dev.sh --with-node              # 再起一个本机采集节点，走真实 UDP 链路
tools/dev.sh --only server,sidecar    # 只跑某几个角色
tools/dev.sh --stt off                # 关掉转写（默认是开的）
tools/dev.sh --port 8081              # 另起一个实例，不影响已有的
tools/dev.sh --backend audiocpp       # 用 audio.cpp 的 Qwen3-ASR 识别，不装 Apple 的
```

`--backend audiocpp` 需要一份 GGUF。第一次把路径给它：

```sh
tools/dev.sh --backend audiocpp --model ~/models/qwen3-asr-0.6b-q8_0.gguf
# 也可以给目录，但目录里必须只有一个 .gguf —— 引擎拒绝在多个包里挑
tools/dev.sh --backend audiocpp --model ~/models/qwen3-asr-0.6b-q8_0.gguf --chunk 8
```

路径会记在 `var/stt-model.conf`（下次 `--backend audiocpp` 就够，删掉它才会重新问），
`--chunk N` 覆盖 5 s 窗口。没给路径时还会去读 `install.sh` 给已安装的 audiocpp
wrapper 写的那份 conf，所以部署过同一台机器的模型不必再答一次。这个后端的识别器
是服务端 crate 的第二个 `[[bin]]`：`crates/server/**` 一变就跟着重建（不会去建
Swift bundle），`--backend audiocpp --sidecar installed` 会被拒绝——安装的 wrapper
是 Apple 那个。

跑起来之后，在**任意终端**用子命令控制这个监督器（命令写进 `$RUNTIME_DIR/dev/cmd`，
监督器每秒取一次）：

```sh
tools/dev.sh status                  # 一次性打印面板
tools/dev.sh reload server|sidecar|node|web|all
tools/dev.sh start web | stop sidecar
tools/dev.sh logs server 80          # 看某个服务的日志（server / sidecar / web / node）
tools/dev.sh logs sidecar -f         # 跟着 sidecar 的日志看
tools/dev.sh stop                    # 关掉这个仓库正在跑的一切
tools/dev.sh schema                  # 当前库的表 + 行数 + 来源分布
tools/dev.sh quit
```

谁负责热加载：

| 改什么 | 怎么生效 |
|---|---|
| `web/src/**` | Vite HMR，改组件即时重绘，不丢页面状态，不碰 Rust |
| `crates/**/*.rs` | `cargo build` → SIGTERM 旧进程 → 起新的 |
| `apple-stt-sidecar/Sources/**` | 重建 bundle → 杀掉 sidecar 进程 → 服务端 5 秒内重连并启动新构建 |
| `crates/node/**` | 重建并重启节点（node 角色开着时） |

关于 sidecar 的两件事：

- **它仍然由服务端拉起**，dev.sh 只是把 `[stt] stream_command` 指向生成的
  `var/dev/steno-stt-sidecar-dev`。那个 wrapper 用 `nohup` 把**本仓库构建出来的**二进制
  放后台跑（Apple 时是 `apple-stt-sidecar/build/` 里的 bundle，`--backend audiocpp`
  时是 `target/debug/steno-stt-sidecar-audiocpp`），所以不需要 `install.sh`，也不会被
  LaunchServices 藏起来。
- `reload sidecar` 只会杀**本实例的** sidecar（按 `steno-stt-<server-pid>.sock` 匹配），
  不会误伤同机另一个实例。

## dev.sh stop

```sh
tools/dev.sh stop                  # 关掉这个仓库正在跑的一切
tools/dev.sh stop server           # 只关服务端（连同它拉起的 node）
tools/dev.sh stop sidecar          # 只关 sidecar
tools/dev.sh stop node | web
```

和 `quit` 的区别：`quit` 是让**监督器**自己走文档化的退出流程（优雅收尾当前分段、排空 DB writer），
`stop` 不依赖监督器——它从证据里找进程：

1. 监督器活着就先停它（它的 cleanup 是正确顺序：先 sidecar，再 server）；
2. 然后按 `var/dev/state/*` 里记录的 pid、以及 8080 上的监听者（**只认命令行里带本仓库
   `target/` 的**）找 server；
3. 按 `steno-stt-<server pid>.sock` 找 sidecar，再扫一遍 socket 已经消失的孤儿 sidecar；
4. 最后 `node` / `web`，并把 `var/dev/` 删掉。

两件它顺手做的事，不做就会留坑：

- **还原 `[stt] stream_command`**。dev 把它改成 `var/dev/steno-stt-sidecar-dev`，而 `stop` 要删
  `var/dev/`——不还原的话下一次 `refresh-all.sh` 起出来的服务端会报
  `cannot launch var/dev/steno-stt-sidecar-dev: No such file or directory`，转写直接不可用。
- **写状态文件**。面板读 `var/dev/state/*`，不写就会一直显示"up"，pid 其实早死了。

`stop` 是本地执行、不进队列的：监督器卡住的时候它也得能用。

## 控制台面板

面板每秒刷新：上半是四个服务的状态（pid、URL、构建次数、失败原因），中间是流水线
（capture → record → detect → stt，含正在成形的 partial 文本），下半是最近的工作流事件
（segment 完成、utterance 落库、鼾声片段、识别器就绪/错误）。

```
 SERVICE   STATE       PID     DETAIL
 server    up          901     http://127.0.0.1:8123  builds 2
 sidecar   up          921     locale zh-CN
 web       up          3146    http://127.0.0.1:5173/  HMR
 PIPELINE
 capture   waiting     16000 Hz x1   packets 0 lost 0   no node
 stt       ready      stream zh-CN apple-speechanalyzer   finals 0   snores 0   dropped 0
 FLOW
 18:54:08 stt stream sidecar connected socket=/var/folders/…/steno-stt-98647.sock
 18:54:08 sidecar connected to /var/folders/…/steno-stt-98647.sock
 18:54:08 sidecar config: locale zh-CN, preset progressiveTranscription, snore snoring
 18:54:08 sidecar snore: snore(snoring@0.50) window 3.00 s, overlap 0.5
 18:54:08 sidecar session started: apple-speechanalyzer zh_CN
 18:54:16 sidecar text [18:54:08..18:54:15] 今天天气很好，我们来测试一下语音识别的效果
```

- 数据来源是 `/api/v1/status` 加 **两路日志**。`FLOW` 由 `dev-panel.py --pump` 同时读服务端
  和 sidecar 的日志、各记一个字节偏移，**只在整行边界切分**（写日志的是另一个进程，读到半个
  行既难看又会丢掉后半句），按关键词过滤后**按时间戳合并**——sidecar 的行会插在它真正发生的
  那两条服务端日志之间，而不是全堆在后面。
- 两边时钟不同源：服务端日志是 UTC（`…Z`），sidecar 打的是本地时间，所以泵把 UTC 转成本地
  时间再排序。sidecar 的 `text [<epoch>..<epoch>]` 也在这里换成 `[18:54:08..18:54:15]`，原始
  毫秒留在日志文件里（那是对数据库用的）。
- **sidecar 的日志行来自哪里**：它同时写 `$STENO_STT_LOG`（默认 `/tmp/steno-stt-sidecar.log`）
  和 stderr，dev 的 wrapper 把 stderr 重定向到 `var/dev/sidecar.log`，两个文件内容一样。取哪个不
  靠修改时间（随便一次 `--help` 都会让 `/tmp` 那份变新，面板就会跳到别人的日志），而是按
  **正在跑的那只 sidecar 的 pid** 认，认不出来才退回"取最新"。
- `logs sidecar` 用的是同一个解析，所以它读的是**活着的那个进程**的日志，`-f` 就是跟着看；
  每行自带 `HH:mm:ss.SSS`（以前没有，两边没法对时）。
- 子进程自己退出时，面板直接显示它最后一行日志，例如
  `node  down  exited: steno-node stopped error=No such file or directory`——本机没有输入
  设备时就是这样，因为没有麦克风可以采集。
- 非 TTY（`dev.sh > log`）只打生命周期行，不刷整屏；要看面板用 `tools/dev.sh status`。

两条刻意的行为：

- **先构建、后替换。** `cargo build` 是 rename 落盘，正在跑的进程握着自己那份，所以只在
  构建成功后才发 SIGTERM。**编译失败时旧进程继续服务**，不会因为一个打字错误把你正在看的
  页面弄下线。
- **运行目录就是 `var/`**，所以页面看到的是真实分段和数据库。代价是它会占用 8080——退出时
  用 `refresh-all.sh --no-node` 把常驻实例放回去（脚本自己也会提示这句）。
- **只停自己启动的进程。** 启动时如果端口被占，只有命令行里带本仓库 `target/` 的那个进程会被
  停掉；别的东西占着端口就报错退出（换 `--port`），不会把不相干的服务打死。`dev.sh stop`
  用的是同一条规则。

## refresh-all.sh

改完代码后一步到位，不要再手动敲四条命令：

```sh
tools/refresh-all.sh                 # 全量：Portal + server + sidecar + Pi + 重启 + 自检
tools/refresh-all.sh --no-node       # 只刷本机（不碰 Pi）
tools/refresh-all.sh --release
tools/refresh-all.sh --stt on --control on   # 就地改配置里的 enabled，不重写整个文件
tools/refresh-all.sh --dry-run       # 只打印计划
```

顺序是**先构建、后停机**，所以中断只有重启那一下：

1. 运行配置（存在就保留；`--stt/--control` 只改那一个 `enabled` 键，`--reset-config` 才整体重生成）
2. `web/dist`（缺 `node_modules` 时先装）
3. `steno-monitor` 二进制
4. 停服务：`SIGTERM`，最多等 20 秒，超时才 `SIGKILL`（会留 `.partial`）；顺手清掉遗留的
   `steno-stt-sidecar` 进程和旧 socket
5. Apple sidecar：装到**它原本所在的**前缀（换前缀等于换一条启动路径）
6. 节点：调 `deploy-node.sh`（默认带 `--enable`，把 Pi 上的 `steno-node` 设成开机自启；
   `--no-enable-node` 可以关掉）；**失败不中断**，只记一笔
7. 启动（`nohup` 后台，pid 写 `$STENO_RUNTIME_DIR/server.pid`；`--foreground` 则前台）
8. 自检：轮询 `/api/v1/status`，打印服务端/采集端/转写/存储池 + Portal 的 HTTP 码；
   转写还在 `starting` 就等 3 秒再看一次

退出码：全部成功且服务端能应答才是 0，否则把失败项列出来并返回 1。

## run-server-mac.sh

**这是 macOS 的服务端入口**，名字里带 `mac` 是有意的：服务端开发、跑夜、看 Portal 都在 Mac 上
（音频从节点走 UDP 进来，或者本机 CoreAudio 设备；识别用 Apple sidecar；Portal 从 `web/dist` 出）。
Pi 上不用这个脚本，用 `deployment/systemd/steno-monitor.service`。脚本开头会检查 `uname -s`，
在 Linux 上直接拒绝并指向 systemd，不会假装能跑。

```sh
tools/run-server-mac.sh                 # source.kind = "net"，HTTP 绑 127.0.0.1:8080
tools/run-server-mac.sh --mic           # 改成开本机采集设备（CoreAudio）
tools/run-server-mac.sh --lan           # HTTP 绑 0.0.0.0，手机能访问
tools/run-server-mac.sh --stt           # 打开流式转写
tools/run-server-mac.sh --control       # 允许 Portal 里启停（需 loopback）
tools/run-server-mac.sh --release       # 优化构建
tools/run-server-mac.sh --reset-config  # 重新生成 server.toml
tools/run-server-mac.sh --write-config-only  # 只写配置就退出（dev.sh / refresh-all.sh 复用模板）
tools/run-server-mac.sh --port 9000
```

- 运行目录：`$STENO_RUNTIME_DIR`，默认 **`<repo>/var`**（已 git 忽略；`tools/dev.sh` 用同一个），里面有
  `server.toml`、`recordings/`、`db/app.db`、`server.log`。生成出来的配置**不是**部署默认值，
  `deployment/config.example.toml` 才是。
- 这里放的是**真实数据**（录音 + 数据库），所以不能放 `$TMPDIR`：macOS 会清理
  `/var/folders/.../T` 里它认为不用的东西。早期版本默认就是 `$TMPDIR/steno-server`，
  `refresh-all.sh` 会把那份数据搬过来并改写配置里的绝对路径（`--no-adopt` 可跳过）。
- 分段时长写 60 秒（真实一夜是 3600），方便几分钟内看到轮转。
- 首次运行发现 `web/dist` 不存在时，如果装了 npm 就顺手 `npm install && npm run build`；
  没装则警告并只提供 JSON API。
- `Ctrl-C` 走文档化的优雅关停：当前段以 `end_reason=shutdown` 结束，DB writer 排空。

两个和平台有关的细节：

- `--mic` 写的是 `[source] kind = "alsa"`。这个值是历史遗留的名字，代码里它表示"开本机设备"，
  在 macOS 上就是 CoreAudio。`--alsa` 作为旧名字仍然接受，会先警告一句再按 `--mic` 处理。
- 麦克风在 macOS 上是**权限**，而端侧识别不是。弹窗记在启动这个二进制的进程名下（通常是终端），
  所以从终端跑一般不会额外弹。

`--stt` 需要 `steno-stt-sidecar` 在 PATH 上（`apple-stt-sidecar/install.sh` 装）。不想装就用
`tools/dev.sh`：它直接跑本仓库构建出来的 bundle。

## stt-check.py

"识别不准"不能靠耳朵调。用 `say` 合成已知句子，同一句走两条路径打分：

```sh
tools/stt-check.py                                    # 内置语料，clean + farfield
tools/stt-check.py --mode stream --limit 3             # 只测生产链路，先看三句
tools/stt-check.py --conditions clean                  # 只看近讲
tools/stt-check.py --replay var/recordings/*.wav --reference "..."   # 真实录音
```

- `--file` 走 sidecar 的 `--file`（整段 WAV 一次给），那是引擎的下限；`--stream` 走生产
  socket，20 ms 一帧、按 realtime 灌。两者对照才能分开"引擎本身不行"和"我们的管线有问题"。
- `--stream` 用的是**和 Rust 服务端同一套分帧**（8 字节头 + `[i64 BE utc_ms]` + PCM），
  并且**客户端先说话**：sidecar 要读到第一帧（`config`）才开始会话，只监听会互相等死。
  方向字节认的是"谁发的"（0x00 = sidecar，0x01 = 服务端），发错方向 sidecar 会直接拒绝。
- 它默认关掉 snore lane：这里量的是转写，分类器抢 CPU 只会给计时添噪声。

## stt-watch.py

跳过 Portal 直接看服务端在识别什么。数据全部来自 server，不碰 sidecar：

```sh
tools/stt-watch.py                     # 默认 http://127.0.0.1:8080
tools/stt-watch.py --server http://127.0.0.1:8081
tools/stt-watch.py --once              # 一行快照（电平 + 计数），给脚本用
tools/stt-watch.py --json | jq -c .type   # 原始 NDJSON，给别人吃
```

- **文字**来自 `/api/v1/transcript`（和 Portal 同一个 WebSocket）：`partial` 原地刷新，
  `final` 带墙钟追加——而且这条 `final` 是**落库之后**才推的，所以看到的句子一定在库里。
- **鼾声**也在这个流里（`snore` 事件）：stt_stream 写库成功后推给 hub。
- **电平**来自 `/api/v1/status` 的 `capture.level_peak_dbfs` / `level_rms_dbfs`（dispatcher
  对每个块算一次，`null` 表示还没收到过块——不拿 0 冒充满刻度）。
- WS 客户端是手写的（握手 + 帧解析），因为这台机器上不一定有 `websockets`；只读、不装依赖。
- `lagged` 会明确打出来：说明中间丢了事件，而不是让画面看起来完整。

## deploy-node.sh

```sh
tools/deploy-node.sh --host 192.168.31.58 --server-host 192.168.31.10
tools/deploy-node.sh --dry-run                     # 默认主机 pi.local
tools/deploy-node.sh --host pi --no-build
tools/deploy-node.sh --no-enable                   # 只部署，不动开机自启
```

- 目标固定 `armv7-unknown-linux-gnueabihf`，glibc 钉在 2.28（Pi OS buster 的版本）。
  用 `cargo zigbuild` 而不是 `cross`，因为后者带的是 glibc 2.31，在 buster 上跑不起来。
- 前提：`cargo install cargo-zigbuild`、`zig`（`brew install zig`）、sysroot
  （`$ARMHF_SYSROOT`，默认 `~/armhf-sysroot`）、能免密 `sudo -n` 的 SSH 账号。
- 远端动作：建服务用户、装二进制到 `/usr/local/bin`、配置到
  `/etc/steno-monitor/node.toml`、单元到 `/etc/systemd/system/`，然后 `daemon-reload`、
  `enable`（默认，`--no-enable` 关掉）并 `restart`。
- `enable` 和 `restart` 是两件事，不能合并成 `enable --now`：`--now` 只会**启动**一个
  没在跑的单元，已经跑着的节点会继续执行刚被替换掉的旧二进制。部署结尾会回读
  `systemctl is-enabled`，不是 enabled 就直接报错——"今晚在跑、下次断电就没声音"正是
  这条检查要挡住的失败。
- 可重复执行，等于原地升级；`--dry-run` 只打印计划。
- Pi 上的 `[archive]` / `[gate]` 两段会被**原样保留**（连同注释）并追加到新配置后面：
  重新渲染这两段等于替设备决定"今晚的音频存哪里、能不能不发"，那不是部署该做的事。
- 主机密钥没记录时给的是 `ssh-keyscan` 提示，不再假装是密码问题。

## replay-wav.py

```sh
tools/replay-wav.py night.wav 127.0.0.1:8091
tools/replay-wav.py night.wav 127.0.0.1:8091 2    # device_id = 2
```

只接受 **16 kHz 单声道** WAV，每 20 ms 发 320 个样本，并严格按墙钟配速。它发出的
HELLO 和 PCM 包与 `steno-node` 同构，所以分段、gap、检测、转写都会走真实路径。
`testdata/steno-eval/clips/` 里的语料是 44.1 kHz，回放前要先转换：

```sh
ffmpeg -i testdata/steno-eval/clips/5-233312-A-28.wav -ar 16000 -ac 1 /tmp/clip16k.wav
```

## stt-stream-probe.py

```sh
PREFIX=$HOME/.local apple-stt-sidecar/install.sh
tools/stt-stream-probe.py /tmp/long.wav
```

在 `/tmp/steno-stt-test.sock` 起一个 Unix socket，把 `~/.local/bin/steno-stt-sidecar`
拉起来，然后按 realtime 推 16 kHz 单声道音频，并把返回的 JSON 行打出来。
它是 Rust 客户端开发期的替身：协议对不上会在这里暴露，而不是在管线里。

## apple-stt-sidecar（已移出 tools/）

macOS 的转写后端（`SpeechAnalyzer`，PCM 进、JSON 出）现在在仓库根目录的
`apple-stt-sidecar/`：构建、安装、协议、排障都在那一份 README 里。

## 相关文档

- `docs/operations/2026-10-29-pi-deploy.md` — Pi 安装、权限、排障
- `docs/decisions/2026-10-29-split-deployment.md` — 为什么节点只做采集
- `docs/decisions/2026-10-29-architecture.md` §3 — 为什么转写是外部进程
