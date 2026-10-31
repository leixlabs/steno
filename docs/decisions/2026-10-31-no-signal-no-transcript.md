# 「有信号，但没有识别结果」：一次排查和它暴露的三个缺陷

2026-10-31。起因是：在环境里播放了中文播客，Portal 显示"识别中"、`有信号`，
但已存语句始终是 0。本文记录定位过程、三个根因，以及为让下一次不必重复这次
排查而补上的东西。

## 结论先说

识别器没有坏，链路也没有丢包。**麦克风输出的是数字抖动，不是声音**：
Pi 上直接 `arecord` 测到的峰值是 −76 dBFS，录下来的每一段 WAV 都只有
±1..±5 的采样值。而系统把这种输入报成了 `有信号`，所以看起来"一切正常，
只是不识别"。

## 三个独立的缺陷

### 1. 麦克风确实没有拾音（环境问题，不是代码问题）

在 Pi 上绕过整条流水线直接测：

| 条件 | 峰值 |
| --- | --- |
| `arecord -D plughw:1,0` 16 kHz mono | −76.3 dBFS |
| 48 kHz 立体声，左/右声道分别测 | −73.4 / −73.4 dBFS |
| `Mic Capture Volume` 扫 0 / 12 / 23 dB | −76.3 / −76.3 / −74.7 dBFS |

增益从 0 扫到最大只让峰值从 5 变到 6，说明**模拟信号本身是零**。增益、
ALSA、USB 传输、服务端解码全部正常——问题在话筒座这一侧：静音开关被按下，
或者话筒没有插在话筒座上（Razer Seiren 是"话筒座 + 可拆话筒"结构，只插
USB 不插话筒就会是这个表现）。

判定依据：数字静音的采样值恒为 0；这里非零值占 56%，且全部落在 ±5 以内，
是 ADC 的抖动噪声。所以"有没有信号"不能靠"有没有非零采样"来回答。

### 2. `signal_silent` 只认"每个采样都是 0"，把哑麦报成有信号

`dispatcher.rs` 的判定是 `silent_block = 每个采样 == 0`。哑掉的麦克风不会
产生 0，它产生 ±1..±5，于是 `signal_silent` 永远不触发，`/status` 报
`signal_state = ok`，Portal 显示 `有信号`。

这是本次排查最贵的一处：它让"麦克风没在工作"和"麦克风在工作"在界面上完全
无法区分。人看到的每一个指标都是绿的。

修法：新增 `signal_dead`，按**峰值电平**而不是字节值判定。峰值低于
−60 dBFS 连续 1 秒就置位。−60 dBFS 低于任何正常麦克风在安静房间里能产出的
电平（工作的话底噪也在 −50 上下），又远高于数字静音，所以它把"音频通路坏了"
和"房间很安静"分开。`signal_state` 因此有四个值：`ok` / `silent` /
`dead` / `unknown`，`silent` 保留原义（没有数据流），两者不合并——它们的
修法不同。

### 3. 部署脚本创建不出服务用户，而且失败是静默的

`deploy-node.sh` 里是：

```sh
getent group steno-node >/dev/null || groupadd --system steno-node
id -u steno-node >/dev/null 2>&1 || useradd --system ... steno-node
```

`useradd` 默认会创建同名主组，所以先建的组让它以
`useradd: group steno-node exists` 失败，`set -e` 随即中断**整个安装脚本**
——此时二进制还没拷过去。结果：旧的 `snore-node` 继续跑，`steno-node`
从未安装，而脚本以退出码 0 结束、还打印了 `steno-node is active`。

同一段还有两个次生问题：

- `alsa_device` 和 `[archive]/[gate]` 只从新路径 `/etc/steno-monitor/node.toml`
  读。尚未迁移的设备只有旧路径 `/etc/snore-monitor/node.toml`，于是渲染出
  `alsa_device = ""`。节点会接受空值（只有一个 USB 设备时自动选中），
  所以部署"成功"而人手选的那个设置被悄悄丢掉了。
- 安装用的 `<<EOF` heredoc 没有加引号，注释里的反引号被当成命令替换执行，
  报 `useradd: command not found` 之类的噪音。

修法：只在用户不存在时才 `useradd --user-group`；`usermod` 之后用
`id -nG` 断言 `audio` 组真的生效；两个配置路径都读；heredoc 加引号，
需要展开的路径用环境变量传。

## 补上的排查设施

这次的教训是"指标全绿但输入是死的"，所以补的东西都围绕这一点：

- **`signal_state` 多了 `dead`**，Portal 显示"无可用信号"并在卡片上直接给出
  该查什么（静音开关 / 话筒是否插在座上 / 增益是否为 0）。同时新增"电平"
  一行显示峰值 dBFS——一个数字比一个形容词更容易让人发现问题。
- **部署脚本在最后实测采集电平**：停服务 → `arecord` 3 秒 → 算峰值 →
  低于 −60 dBFS 就告警。`systemctl is-active` 只证明进程活着，证明不了它
  听到了什么。
- **部署脚本同时打印服务端测到的电平**，让"设备端有声音、服务端没声音"
  这类链路问题一眼可见。
- 三个单元测试锁住回归：抖动噪声必须报 `dead`、−50 dBFS 的安静房间不能
  报 `dead`（否则真安静的卧室会天天告警）、数字静音仍然报 `silent`。

## 需要人做的一步

Pi 上的话筒座需要人工检查——静音开关和话筒是否插到位。代码这边已经能正确
报告这件事，但只有人能把它修好。

```sh
# 在 Pi 上，先停节点以释放设备
sudo systemctl stop steno-node
sudo arecord -D plughw:1,0 -f S16_LE -r 16000 -c 1 -d 3 /tmp/t.wav
# 播放 /tmp/t.wav，有声音就说明修好了
sudo systemctl start steno-node
```

另外，旧部署的 `snore-node` 与 `steno-node` 会同时存在并抢同一个音频设备
（单 subdevice）。迁移时**必须先 `systemctl disable --now snore-node`**，
否则新节点起不来，而旧节点继续用旧配置跑，这正是本次 `protocol_errors`
累积到 17634 的来源。
