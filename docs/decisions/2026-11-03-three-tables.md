# 2026-11-03 — 三张表：records / events / transcripts

状态：**已完成**。迁移折叠成单个 `0001_init.sql`（三张表，无兼容路径）；
`storage.rs` / `db_writer` / 录制 worker / `recovery` / `retention` / `http_server` /
`web` 全部切换完毕；旧五张表与段级转写已删除。`cargo test -p steno-server` 142 passed，
`cargo check --workspace` 干净，`web` 构建通过。旧库改名为
`var/db/app.db.before-3tables`。

实施中修正了三处命名：事件表最终叫 **`episodes`**（旧的 `episodes` 名字被占，
而且"episode"比"event"更准确：它是一段有起止的鼾声，不是瞬时事件）；`TranscriptEntry`
定为 `TranscriptRow`；`pool_summary_three` 定为 `pool_summary`。

## 为什么

原来的五张表建模的是**录制器**：一段时长槽一段录音、一个空洞一行、一段生命周期一个
状态，文本还有两个家（段级的 `segment_transcripts` 和流式的 `utterances`）。产品比这
小：哪里发生过事情就留一段音频，给它打标注，把它给人看。

三张表之后，几个长期纠缠的问题自己消失了：

- 不需要"没有内容的段"这种状态——`recordings` 的行只在有内容时才产生；
- 不需要 `status` 字段——"有行"和"有文件"是同一句话；
- 不需要两个文本表——两种模式都写 `transcripts`。

## 表

```sql
CREATE TABLE recordings (
    id              TEXT PRIMARY KEY,
    relative_path   TEXT NOT NULL UNIQUE,   -- 唯一能拿到文件的途径
    started_at_utc  TEXT NOT NULL,
    ended_at_utc    TEXT NOT NULL,
    capture_rate_hz INTEGER NOT NULL CHECK(capture_rate_hz > 0),
    channels        INTEGER NOT NULL CHECK(channels BETWEEN 1 AND 2),
    frame_count     INTEGER NOT NULL CHECK(frame_count >= 0),
    size_bytes      INTEGER NOT NULL CHECK(size_bytes >= 0),
    lost_frames     INTEGER NOT NULL DEFAULT 0 CHECK(lost_frames >= 0),
    created_at_utc  TEXT NOT NULL
);

CREATE TABLE episodes (
    id              TEXT PRIMARY KEY,
    recording_id    TEXT REFERENCES recordings(id) ON DELETE SET NULL,
    started_at_utc  TEXT NOT NULL,
    ended_at_utc    TEXT NOT NULL,
    source          TEXT NOT NULL CHECK(source IN ('rule-v1','apple-soundanalysis-v1')),
    label           TEXT,
    confidence      REAL CHECK(confidence IS NULL OR (confidence BETWEEN 0 AND 1)),
    review_status   TEXT NOT NULL DEFAULT 'unreviewed'
                    CHECK(review_status IN ('snore','not_snore','unreviewed')),
    created_at_utc  TEXT NOT NULL
);

CREATE TABLE transcripts (
    id              TEXT PRIMARY KEY,
    recording_id    TEXT REFERENCES recordings(id) ON DELETE SET NULL,
    started_at_utc  TEXT NOT NULL,
    ended_at_utc    TEXT NOT NULL,
    text            TEXT NOT NULL,
    engine          TEXT NOT NULL,
    language        TEXT,
    created_at_utc  TEXT NOT NULL
);
```

索引：`recordings(started_at_utc)`、`episodes(started_at_utc)`、
`episodes(recording_id)`、`transcripts(started_at_utc)`、`transcripts(recording_id)`。

## 写入时机

- **`recordings`**：文件 close（rename 成功）之后插入，同时用
  `UPDATE … WHERE recording_id IS NULL AND started_at_utc >= ?from AND < ?to` 回填
  同一时间窗内的标注。行与文件同生共死，没有中间态。
- **`episodes` / `transcripts`**：产生即写，`recording_id` 为 NULL，等文件关闭时
  回填。跨边界的句子归它开始的那一段——那是读者会点开听的那一个。
- **删除**：retention 删文件后 `DELETE FROM recordings`；`ON DELETE SET NULL` 保证
  标注留下——音频被回收了，那一夜仍然发生过。

## 放弃的东西

| 去掉 | 替代 | 代价 |
|---|---|---|
| `audio_gaps`（每洞一行，含 kind / estimate_source） | `recordings.lost_frames` 一个数 | 事后分不清 xrun 和队列溢出 |
| `time_quality` / `clock_drift_ppm` / `boot_id` / `channel_mapping` | 无 | 时钟可信度不再入库，只在 `/status` 上活着 |
| `status` 五值 | 无（行存在即文件存在） | 崩溃后的 `.partial` 只能靠启动扫描建档，不再有 `interrupted` 标记 |
| `segment_transcripts` 与 `[stt] mode = "segment"` | `transcripts`（流式唯一路径） | 整条段级转写路径删除：`stt.rs`、`[stt] command/args/min_segment_seconds/poll_interval_seconds/timeout_seconds` |
| `episodes` 的 peak/mean/band/noise_floor 四列 | `confidence` 一列 | Portal 不再显示"峰值 −12 dBFS" |

段级转写之所以可以删：在"有内容才录"的模型里，**每个 recording 存在的理由本身就是
它产生了文本或事件**，所以"待转写"这个队列永远是空的。

## 施工顺序（一次改动，中间必然红）

1. `migrations/`：删 0001–0004，写新的 0001（上文）。
2. `storage.rs`：整体替换。类型 → `RecordingRow` / `RecordingDraft` / `EventRow` /
   `ClassifiedEvent` / `TranscriptRow` / `PoolSummary`；`SegmentStatus`、`GapRecord`、
   `UtteranceRow`、`SegmentStarted/Completed` 全部消失。方法约 18 个
   （`insert_recording`、`link_annotations`、`recordings_between`、
   `list_recordings_for_retention`、`delete_recording`、`pool_summary`、
   `insert_event`、`insert_classified_event`、`events_between`、`save_transcript`、
   `transcripts_between`、`days_between`…）。
3. `db_writer.rs`：`DbCommand` 缩成 `Recording / Event / Classified / Flush / Shutdown`。
4. `main.rs` 录制 worker：环形缓冲 + 触发（见下）+ 关闭时 `insert_recording`；
   检测 worker 改 `EventDraft` 构造。
5. `recorder.rs`：只保留 `WavRecorder` 与 `segment_relative_path`，`SegmentFile` 删。
6. `recovery.rs`：从"扫行状态"改成"扫 `.partial` → 读头 → rename → 建 recording 行"。
7. `retention.rs`：按 `started_at_utc` 从旧到新删文件 + 删行。
8. `stt.rs`：整文件删除；`[stt]` 配置删段级键。
9. `http_server.rs`：`SegmentView` → `RecordingView`（去掉 status/time_quality/
   end_reason/gap_count/xrun_count/clock_drift/sample_format），`EventView` 去掉
   offsets/peak/mean/band；`/segments` → `/recordings`；删 `/transcripts`（段级）路由。
10. `web/`：跟着删字段与路由。

## 触发式落盘（与表结构同一次改动）

`recordings` 只在有内容时产生，所以录制 worker 必须等触发：环形缓冲约 90 s，收到
事件或文本后开文件并从环里回填 `[t1 − 5 s, now]`，静音 60 s 或单文件 300 s 收尾。
触发源：检测 worker（`EventDraft`）与 `stt_stream` 的结果行，经一个共享总线投给
录制线程。没有 `keep_mode` 开关——这是唯一行为。

## 旧库

`var/db/app.db` 先改名成 `app.db.before-3tables`，不删。新 schema 的迁移校验和与旧
文件不同，旧库若被误开会在启动时报 `changed after it was applied`，这是安全的一侧。

## 验证清单

- `cargo test -p steno-server --lib`（全部重写后的 storage/migrate 测试）
- 空夜：跑一小时静音，`recordings` 应为 0 行、`/status` 仍有心跳
- 有声音：一句话 → 一个文件一行 + 一条 transcript，`recording_id` 已回填
- 删除文件后标注仍在，`recording_id` 为 NULL
