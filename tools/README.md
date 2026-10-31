# tools — 开发与部署脚本

这里放不方便塞进 Rust 二进制的东西：本机跑服务端、把节点装到 Pi 上、无硬件回放音频、
开发 macOS 转写后端。都不是 `cargo build` 的一部分，也不由 systemd 安装。

| 文件 | 一句话 |
|---|---|
| `dev.sh` | **开发环境**：Vite HMR + Rust 改动自动重建重启，一个 Ctrl-C 收工 |
| `refresh-all.sh` | **一键刷新所有运行时**：Portal → 服务端 → sidecar → 节点 → 重启 → 自检 |
| `run-server.sh` | 在临时运行目录里前台跑 `steno-monitor`，不碰 `/etc`、`/var` |
| `deploy-node.sh` | 交叉编译 `steno-node` 并一次性装到树莓派 |
| `replay-wav.py` | 把 WAV 按 realtime 灌进 UDP 协议，替身节点 |
| `stt-stream-probe.py` | 用 WAV 驱动流式识别 sidecar，替身服务端 |

## dev.sh

改代码时用它，别用 `refresh-all.sh`（后者是"重新部署"，一条命令要跑一分钟）：

```sh
tools/dev.sh                 # Vite :5173（代理 /api）+ 服务端 :8080
tools/dev.sh --stt on        # 顺带开转写
tools/dev.sh --port 8081     # 另起一个实例，不影响已有的
tools/dev.sh --no-web        # 只要后端
```

`--stt on` 会把 `[stt] stream_command` 写回 `steno-stt-sidecar`，顺手纠正被手动改脏的配置。

谁负责热加载：

| 改什么 | 怎么生效 |
|---|---|
| `web/src/**` | Vite HMR，改组件即时重绘，不丢页面状态，不碰 Rust |
| `crates/**/*.rs` | `cargo build` → SIGTERM 旧进程 → 起新的（实测 35928 → 36299） |
| `apple-stt-sidecar/*` | **不监听**，改完跑 `refresh-all.sh --no-node --no-web` |

两条刻意的行为：

- **先构建、后替换。** `cargo build` 是 rename 落盘，正在跑的进程握着自己那份，所以只在
  构建成功后才发 SIGTERM。**编译失败时旧进程继续服务**（实测：塞一行语法错误，pid 不变、
  `/status` 仍 200），不会因为一个打字错误把你正在看的页面弄下线。
- **运行目录就是 `var/`**，所以页面看到的是真实分段和数据库。代价是它会占用 8080——退出时
  用 `refresh-all.sh --no-node` 把常驻实例放回去（脚本自己也会提示这句）。

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
6. 节点：调 `deploy-node.sh`；**失败不中断**，只记一笔
7. 启动（`nohup` 后台，pid 写 `$STENO_RUNTIME_DIR/server.pid`；`--foreground` 则前台）
8. 自检：轮询 `/api/v1/status`，打印服务端/采集端/转写/存储池 + Portal 的 HTTP 码；
   转写还在 `starting` 就等 3 秒再看一次

退出码：全部成功且服务端能应答才是 0，否则把失败项列出来并返回 1。

## run-server.sh

```sh
tools/run-server.sh                 # source.kind = "net"，HTTP 绑 127.0.0.1:8080
tools/run-server.sh --alsa          # 改成开本机麦克风
tools/run-server.sh --lan           # HTTP 绑 0.0.0.0，手机能访问
tools/run-server.sh --release       # 优化构建
tools/run-server.sh --reset-config  # 重新生成 server.toml
tools/run-server.sh --write-config-only  # 只写配置就退出（供 refresh-all.sh 复用模板）
tools/run-server.sh --port 9000
```

- 运行目录：`$STENO_RUNTIME_DIR`，默认 **`<repo>/var`**（已 git 忽略；`tools/dev.sh` 用同一个），里面有
  `server.toml`、`recordings/`、`db/`、`server.log`。生成出来的配置**不是**部署默认值，
  `deployment/config.example.toml` 才是。
- 这里放的是**真实数据**（录音 + 数据库），所以不能放 `$TMPDIR`：macOS 会清理
  `/var/folders/.../T` 里它认为不用的东西。早期版本默认就是 `$TMPDIR/steno-server`，
  `refresh-all.sh` 会把那份数据搬过来并改写配置里的绝对路径（`--no-adopt` 可跳过）。
- 分段时长写 60 秒（真实一夜是 3600），方便几分钟内看到轮转。
- 首次运行发现 `web/dist` 不存在时，如果装了 npm 就顺手 `npm install && npm run build`；
  没装则警告并只提供 JSON API。
- `Ctrl-C` 走文档化的优雅关停：当前段以 `end_reason=shutdown` 结束，DB writer 排空。

常驻安装不要用它，用 `deployment/systemd/steno-monitor.service`。

## deploy-node.sh

```sh
tools/deploy-node.sh --host 192.168.31.58 --server-host 192.168.31.10
tools/deploy-node.sh --dry-run                     # 默认主机 pi.local
tools/deploy-node.sh --host pi --no-build --enable
```

- 目标固定 `armv7-unknown-linux-gnueabihf`，glibc 钉在 2.28（Pi OS buster 的版本）。
  用 `cargo zigbuild` 而不是 `cross`，因为后者带的是 glibc 2.31，在 buster 上跑不起来。
- 前提：`cargo install cargo-zigbuild`、`zig`（`brew install zig`）、sysroot
  （`$ARMHF_SYSROOT`，默认 `~/armhf-sysroot`）、能免密 `sudo -n` 的 SSH 账号。
- 远端动作：建服务用户、装二进制到 `/usr/local/bin`、配置到
  `/etc/steno-monitor/node.toml`、单元到 `/etc/systemd/system/`，然后 `daemon-reload`
  并 `restart`。默认不动开机自启，要加 `--enable`。
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
