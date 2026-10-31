# web — 控制中心（`steno-portal`）

两个页面，因为它们是两件不同的事：

| 页面 | 路由 | 回答的问题 |
|---|---|---|
| **控制台** | `/`（默认） | 在跑吗？能启停吗？存储池还剩多少？三个进程各自用什么命令行启动的？ |
| **录音** | `#/recordings` | 这一天录了什么？哪一段有候选片段？能听吗？说了什么？ |

拆开的理由不是"文件太长"，是**读它的时机不同**：控制台是凌晨三点确认管线还在，
录音页是早上回看。合并在一页时，控制台也要等一整天的 segments 加载完才能显示。
拆开后控制台只轮询 `/status` + `/control`，从不碰录音；录音页只在切日期时加载那一天。

Vite + React 19 + TypeScript，构建产物在 `web/dist`，由服务端按 `server.web_dir` 托管。

**不嵌进二进制。** `npm run build` 是部署步骤，不是编译步骤——改一句文案不该触发一次
几分钟的 Rust 全量构建。

## 开发

```sh
tools/dev.sh       # 推荐：Vite HMR + 后端自动重建，一个 Ctrl-C 收工

cd web
npm install
npm run dev        # http://127.0.0.1:5173，/api 代理到 127.0.0.1:8080
npm run build      # tsc -b && vite build → dist/
npm run preview    # 预览构建产物
```

只改前端时 `npm run dev` 就够（服务端用 `tools/run-server.sh` 单独起）。要连后端一起改，
用 `tools/dev.sh`——它同时管 Vite 和 `cargo build`+重启。

`vite.config.ts` 里把 `/api` 代理到 `VITE_API_TARGET`（默认 `http://127.0.0.1:8080`），所以
开发时后台用 `tools/run-server.sh` 起一个服务端即可，不需要配 CORS，也不需要重编 Rust。
开发服务器显式绑 `127.0.0.1`（Vite 默认解析到 `::1`，按 IP 访问会被拒）且 `strictPort`：
5173 被占用时直接报错，而不是悄悄换端口然后代理到错误的后端。
想看启停按钮，用 `tools/run-server.sh --control`（只允许 loopback 绑定）。

## 标注：Agentation

开发时右下角有一个标注工具（[agentation](https://agentation.com)）。点它 → 悬停高亮 →
点元素 → 写一句话 → 复制，拿到的是带 CSS 选择器、React 组件名、源码路径的 markdown；粘给
agent 就能直接 grep 到代码，不必描述“侧边栏那个蓝色按钮”。

```sh
npm run dev                       # 工具栏自动出现
VITE_AGENTATION=1 npm run build   # 想让构建产物里也有（默认没有）
```

两条刻意的边界：

- **生产包里没有它。** `main.tsx` 的判断是 `import.meta.env.DEV || VITE_AGENTATION === '1'`；
  没有这个环境变量时表达式折成 `false`，而包里声明了 `sideEffects: false`，所以 Rollup 连
  模块一起丢掉。检查方式：`grep -r agentation web/dist` 无输出，DOM 里也不该有
  `data-agentation-portal`。
- **它是 `devDependency`**，不进 `dependencies`，服务端托管 `web/dist` 时自然也用不到它。

想让 agent 直接读标注（省掉复制粘贴），再注册 MCP：`npx -y agentation-mcp server`（默认
4747 端口），然后给组件传 `endpoint="http://localhost:4747"`。现在没接——复制粘贴这条路已经
够用，而 MCP 会把页面内容发给另一个进程，这个决定该由你来做。

## 目录

| 文件 | 内容 |
|---|---|
| `index.html` | 入口，`lang="zh-CN"`，`color-scheme: dark` |
| `src/main.tsx` | 挂载 `App`，引入全局样式 |
| `src/App.tsx` | 外壳：品牌头 + 页面切换（`#/` / `#/recordings`）+ 顶栏状态 chip |
| `src/ConsolePage.tsx` | 控制台：三张状态卡（服务端/采集端/转写）+ 存储池 + 启动参数 |
| `src/RecordingsPage.tsx` | 录音：日期条 + 时间轴 + 候选片段 + 录音详情 + 当天语句 |
| `src/ui.tsx` | 两页共用：`CardHead`/`PanelHead`/`Empty` 与状态词表（无数据请求） |
| `src/api.ts` | 接口类型、状态词表（录制/单元/转写/时间质量）与格式化函数 |
| `src/styles.css` | 夜间低眩光配色：琥珀色 = 选中/可操作，青色 = 正常，红色 = 单元或接口出错 |

### 时间轴缩放

一条带子就是一天的覆盖情况，但 3 小时 35 分压进 640 px 时，一个 60 秒的段只有 3 px 宽。
所以时间轴是一个**可缩放的视口**：

| 操作 | 行为 |
|---|---|
| ⌘/Ctrl + 滚轮 | 以光标下的时刻为锚点缩放（1×–240×） |
| 拖动 | 平移；`overflow-x` 交给浏览器，拖拽/惯性/滚动条都不用自己算 |
| 双击 / `复位` | 回到全天 |
| `−` / `+` | 以中点为锚点缩放 |

缩放是**视口**而不是重排：`zoom` 只是把轨道的 `width` 乘以倍数，滚动位置决定看哪一段。
这样几何关系始终成立——10 秒的段在任何倍率下相对邻居都是 10 秒宽，而且每一级缩放都不需要
重算任何东西。

> 修过一个真 bug：`+` 连点三次只放大一次。因为 `zoom` 是每次渲染捕获的值，同一帧里三次
> 点击读到的都是旧值。改成 `setZoom(current => …)` 的函数式更新后，三次累积到 1.95×
> （实测宽度 638 → 1245 px）。

### 实时转写：WebSocket

`GET /api/v1/transcript` 升级为 WebSocket，由服务端把识别器产生的文本推给页面。

原来靠每 4 秒轮询 `/status` 拿 `stt.partial`，问题是**窗口太宽**：一次 partial 可能在两次
轮询之间被覆盖，一段短句甚至可能说完了、final 了、却一次都没被看到。

推送的事件：

| type | 何时 | 前端怎么用 |
|---|---|---|
| `ready` | 连接建立时 | 拿到引擎名 + 连接时的句数，说明"此后的是历史，走 HTTP" |
| `partial` | 识别器每几百毫秒 | 替换上一行"正在识别" |
| `final` | **行写入数据库之后** | 追加到转录，按 `id` 去重 |
| `state` | 识别器就绪/重启 | 停掉"正在连接" |
| `error` | 识别器报错 | 面板显示原因，不猜 |
| `lagged` | 浏览器落后到丢事件 | 显示"落后 N 条"，而不是装作完整 |

三条刻意的设计：

- **`final` 在落库之后才发。** 顺序反过来的话，页面会显示一条数据库拒绝了的句子。
- **慢客户端被丢事件，不阻塞 worker。** `broadcast` 容量 256，落后就丢最旧的并告知；录音
  主链路绝不能因为某个后台标签页卡住而停。
- **转写没开就不建连。** 服务端对未运行的转写返回 503，前端据此根本不打开 socket，否则会
  无限退避重连。

重连是 1s → 2s → 4s → 上限 10s；`visibilitychange` 到 hidden 时主动断开（口袋里的手机不该
一直挂着连接），回到前台再连。`/status.stt.stream_clients` 是当前订阅数，用于确认没有连接泄漏。

### 转录面板

`2026-10-30 转录` 显示**这一天**的完整转录，按段连成一段连续文本（不是带时间戳的日志）。
每句是行内元素，起止时间在悬停时显示；页脚给出句数和覆盖时长。

实时的那半句（`stt.partial`）**只在今天显示**。原来无条件渲染，导致翻到上周某天时，今天
正在说的话被追加在那天的转录后面——面板看起来像是属于那一天的。

路由是 **hash** 而不是路径：服务端本来就会对未知路径回落到 `index.html`，但 hash 不需要
服务端配合，从 `file://` 预览也能用。两个页面不值得引入路由库。

## 数据流

- 控制台的信息顺序是运维顺序：先看能不能用（状态卡 + 启停），再看存储池，最后是启动参数。
  录音页的顺序是时间顺序：先看这一天覆盖得完不完整（时间轴），再看细节。
- 三个轮询：控制台 `/status` 每 4 秒、`/control` 每 10 秒（后者更慢是因为服务端每次回答
  都要跑两遍 `systemctl is-active`）；外壳顶栏的 chip 每 8 秒，只为了那一行"服务正常"。
  录音页的 `/status` 只在挂载时取一次——它只用来把语句空状态的文案说对。
- 换日期时并行拉 `segments` / `events` / `utterances`，没有客户端状态机，也没有缓存层
  需要失效。
- 不做乐观更新，唯一例外是候选片段的保留/排除：失败时回滚并显示错误横幅。
- 启停**不做乐观更新**：动作返回后用新读到的单元状态渲染，页面不猜结果。停止或重启
  `server` 会把自己的后端一起关掉，所以客户端把这一次请求失败当作“已下发”，由下一次
  轮询判定。
- 时间轴默认选中第一个“真的有声”的段——服务端启动时会立刻开一段占位，第 0 段经常
  是空的。
- 所有格式化函数都是全函数：缺值显示 `—`，不会因为一个 `null` 把整棵树卸载掉。
  字段缺失就显示缺失，不填一个看起来合理的天数或时长。

## 状态词表

服务端只给机器词，人类词在这里：

| 来源 | 值 | 显示 |
|---|---|---|
| `stt.state` | `off` / `starting` / `ready` / `segment` / `error` | 未开启 / 等待识别器 / 识别中 / 逐段转写 / 识别器故障 |
| `control.units[].state` | `active` / `inactive` / `failed` / … | 运行中 / 已停止 / 已失败 / … |
| `control.available = false` | — | 不可查（附服务端给的原因） |
| `time.time_quality` | `synced` / `corrected` / `unsynced` | 已同步 / 已校正 / 未同步 |

`connected` 单独不可用：转写没开、还没连上、以及 `mode = "segment"`（没有长连接）
三种情况它都是 `false`，所以判断一律走 `stt.state`。

## 依赖的接口

| 调用 | 接口 |
|---|---|
| 状态（4 s） | `GET /api/v1/status` |
| 单元状态（10 s） | `GET /api/v1/control` |
| 启停 | `POST /api/v1/control`（`{target: server\|node, action: start\|stop\|restart}`） |
| 日期列表 | `GET /api/v1/days?from=&to=` |
| 分段 | `GET /api/v1/segments?date=` |
| 候选片段 | `GET /api/v1/events?date=` |
| 当天语句 | `GET /api/v1/utterances?date=` |
| 单段转写 | `GET /api/v1/segments/{id}/transcript` |
| 复核 | `PATCH /api/v1/events/{id}` |

音频直接由 `<audio src={segment.audio_url}>` 播 `GET /api/v1/segments/{id}/audio`，
Range 由服务端处理，所以用 `preload="metadata"` 拿真实时长。`POST /control` 是唯一会
改变机器状态的调用：它默认关闭，且服务端只在 loopback 绑定时接受它。

## 部署

```sh
cd web && npm install && npm run build
```

然后把服务端的 `server.web_dir` 指到构建目录（默认 `web/dist`，相对进程工作目录）。
设成空字符串会关掉 UI，只留 JSON API。

服务端那边负责：

- SPA 回退：无扩展名且不存在的路径回落到 `index.html`，刷新客户端路由不会 404；
- `/assets/*` 带内容哈希，长缓存；`index.html` 不缓存，否则发布永远到不了浏览器；
- 拼路径前拒绝 `..`，不依赖 `canonicalize` 兜底。

## 注意

- `web/dist/` 和 `node_modules/` 都在 `.gitignore` 里（第 40–41 行），构建产物和依赖树
  不会进仓库；`package-lock.json` 要提交。
- TypeScript 是 `strict` + `noUnusedLocals` + `noUnusedParameters`，`npm run build`
  会先跑 `tsc -b`，类型不过就不出包。
- 视觉上是夜间场景：深色背景、低对比底色、单一强调色。改成浅色会破坏“凌晨三点瞄一眼”
  这个前提。

## 相关文档

- `docs/decisions/2026-10-30-control-centre.md` — 为什么控制中心长这样，以及 `[control]` 为什么
  默认关闭且只允许 loopback
- `docs/decisions/2026-10-29-architecture.md` §2 — 为什么从内联 HTML 换成 Vite SPA
- `crates/server/README.md` — 接口与静态托管行为
