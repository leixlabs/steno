# 识别准确率：先量，再改

> **更正（2026-10-31）**：Apple 后端已重写为 `apple-stt-sidecar`，改用 macOS 26 的
> `SpeechAnalyzer`。SFSpeechRecognizer、端点轮换、窗口化重识别、解码窗口、以及语音识别
> 授权都已不存在；本文提到的旧路径与旧行为只作历史记录。


2026-10-30。起因是"语音识别结果非常不准"。结论是：引擎和流水线都没问题，
问题在**采集端的电平与频响**，以及**兜底路径在大段音频上直接放弃**。

## 怎么量

```
tools/stt-check.py                       # TTS 语料，近场 + 远场模拟，file 与 stream 双通道
tools/stt-check.py --replay a.wav --reference "你说的话"   # 真实录音，带已知答案
tools/stt-check.py --conditions farfield --mode stream --limit 2   # 快速回归
```

语料由 `say` 合成、`ffmpeg` 做增益/带限/噪声/混响降质，因此**有标准答案**，
可以算字符错误率（CER）。数字归一化在 `normalize()`：苹果会把"三点"写成
`3:00`、"一百三十九"写成 `139`，不归一化会把正确的输出算成 4 个字全错——
第一版就踩了这个坑，17% 的"错误"里只有一处是真的。

## 量出来的三件事

| 条件 | 通路 | CER |
| --- | --- | --- |
| 近场 | file | 0.8% |
| 近场 | stream（生产通路） | 0.8% |
| 远场模拟 | file | 0.8% |
| 远场模拟 | stream | 0.8% |

1. **stream 与 file 逐句完全一致** → 分帧、采样率、节奏都对，流水线无罪。
2. 唯一的真错是"一片"听成"一瓶"，这是引擎的上限，不是我们的 bug。
3. 真实录音里识别不出来，不是因为引擎差——见下。

## 真实录音里的两个真问题

### 1. 麦克风增益拉满，底噪 −33 dBFS

Razer Seiren 的 `Capture 23 [100%] [22.00 dB]`，房间底噪 −33.5 dBFS，
且能量几乎全在 250 Hz 以下（lag-1 自相关 0.97，一阶差分方差比 0.01）。
说话只比它高几 dB，识别器的动态范围全花在"没有字的声音"上。

- 修法：节点侧加 120 Hz 二阶高通（`audio.rumble_highpass_hz = 120`），
  底噪 −33.5 → −41.7 dBFS；
- 建议：`amixer -c 1 sset Mic capture 12` 把模拟增益降到 52%，
  再 `alsactl store`。

### 2. 文件接口在长音频上直接返回空

同一段 60 秒录音（含明显语音）：

- `--mode file`（`SFSpeechURLRecognitionRequest`，整段一次）→ **空**
- `--mode stream`（生产通路，20 ms 一帧）→ `对于在home或者播客办理中需要切换对话要`
- 同段切成 10 秒 → file 接口 → `Famous或者播客办理中需要切换对话`

也就是说 URL 识别器对"长音频里夹杂少量语音"的场景会报 `1110 No speech detected`
并**什么都不返回**。而 `recover()`（流式任务 endAudio 拿不到 final 时的兜底）
和 `[stt] mode = "segment"` 都走这条路——兜底一直在静默丢句子。
这就是"识别很不准"看起来像引擎问题、实际是兜底失效的原因。

修法（`tools/apple-stt/main.swift`）：`FileRecognizer.recognizeSamples()`
把长音频切成 10 秒窗、重叠 1 秒、跳过低能量窗、拼接时去掉重叠的重复字符；
短音频仍走一次请求以保留精确时间戳。

## 顺带修掉的四件事

- **格式协商**：ALSA 会把 16 kHz 请求悄悄改成 48 kHz 立体声，而节点把结果
  标成 16 kHz 单声道。现在先取设备默认格式，再由 `WireConverter` 降混+重采样
  到 16 kHz 单声道，归档、上行、识别三处看到的都是同一个格式。
- **速率自检**：`ArrivalGuard` 用"每秒实际到达的帧数"验证广告出去的采样率，
  偏差 >5% 就报错并写进 `node_report.rate_mismatch`（控制中心可见）。
  开流前 3 秒不测——环形缓冲正在注水，会误报 13500 Hz。
- **阻塞发送**：UDP `send_to` 阻塞会撑爆 500 ms 采集环，整句被撕成两半。
  改成非阻塞，并把这些计数上报（`ring_overflow_frames`、`send_errors`）。
- **取消不是错误**：`endAudio()` 触发的 `1101 Recognition request was canceled`
  被当成错误上报，界面上整个识别器显示为"出错"。现在只是轮转。

## 还需要人工确认的一步

新 helper 换了代码签名，macOS 需要重新授权一次：

```
open -n -a ~/.local/lib/snore-stt-apple.app --args --authorize
```

弹窗点"允许"后重启 server 才会用上分窗识别。在此之前，正在运行的旧 helper
（进程内）仍然照常工作，但**重启 server 会失去授权**，状态会如实显示
`speech recognition permission was not granted`。
