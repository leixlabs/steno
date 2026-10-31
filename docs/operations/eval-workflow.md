# 量检测准确率：`steno-eval`

阈值不能靠耳朵调。这个工具把一批带人工标注的音频按服务端**同一条**
`DetectionPipeline` 跑一遍，报 TP/FP/FN、precision/recall/F1 和起始时间误差。
不需要麦克风、数据库或正在跑的服务。

## 语料清单

一个 JSON 文件，`file` 相对清单自身解析（可用 `--base-dir` 改）：

```json
{
  "clips": [
    { "file": "clips/1-20545-A-28.wav", "snore": true,  "note": "ESC-50 snoring" },
    { "file": "clips/2-102359-A-40.wav", "snore": false, "note": "ESC-50 control" },
    { "file": "clips/night-01.wav", "intervals_s": [[12.4, 15.1], [61.0, 64.8]] }
  ]
}
```

两种标注粒度，二选一：

- `snore`: `true`/`false` —— 整段音频是否包含鼾声。只影响 TP/FP/FN 计数。
- `intervals_s` —— 标记的鼾声区间（秒）。有这个才能算起始时间误差。

`testdata/steno-eval/labels.json` 是现成的 40 段 ESC-50 语料（未跟踪，17 MB）。

## 跑

```sh
cargo run --release --features tools --bin steno-eval -- --manifest testdata/steno-eval/labels.json
```

## 短片段必须先改噪声底预热

**默认参数是按整夜调的，对 5 秒的语料直接把一切判成「漏检」。** 噪声底估计器在
`noise_floor_min_seconds`（默认 10 s）之前不可信，5 s 片段永远到不了那一步，
`wide_noise_floor_dbfs` 是 `null`，于是没有任何帧被判定为 active。表现是
`true positives=0  false negatives=40`，看起来像检测器坏了，其实它只是还没热身。

同一批 40 段，只把预热改短结果就完全不同：

```sh
sed -e 's/^noise_floor_window_seconds = 30/noise_floor_window_seconds = 5/' \
    -e 's/^noise_floor_min_seconds = 10/noise_floor_min_seconds = 2/' \
    deployment/config.example.toml > /tmp/eval-short.toml

cargo run --release --features tools --bin steno-eval -- \
    --manifest testdata/steno-eval/labels.json --config /tmp/eval-short.toml
```

```
true positives=35  false positives=0  false negatives=5
precision=1.000  recall=0.875  f1=0.933
```

注意这个 0.933 说明不了整夜的表现：ESC-50 的片段是「纯鼾声/纯非鼾声」的干净样本，
没有真实卧室的底噪、没有被子摩擦、没有远处说话。它只证明**阈值在这批样本上的行为**，
真正能不能用仍然要有标注过的整夜录音——目前还没有。

## 常用开关

| 参数 | 用途 |
|---|---|
| `--config <toml>` | 从服务配置取检测参数，显式参数仍然优先 |
| `--tolerance-ms <ms>` | 判定命中允许的起始时间偏差（默认 500） |
| `--verbose` | 每条片段一行：ref / det / hit / fp / onset_ms |
| `--json` | 输出完整报告，便于和历史结果 diff |
| `--absolute-floor-dbfs` / `--noise-margin-db` / `--band-margin-db` / `--band-ratio-min` | 单点覆盖阈值，用来扫参数 |

扫参数时把 `--json` 的结果存下来对比，比看表格靠谱：改了阈值之后 recall 涨了
多少、FP 涨了多少，只有两次输出的 diff 说得清。
