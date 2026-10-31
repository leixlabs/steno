# 2026-10-31 — schema 交给迁移文件，DDL 不再写在 Rust 里

状态：已实施。代码是唯一事实来源，本文只记录当时的判断与实测数据。

## 变更

`storage.rs` 里四段用 `execute_batch` 拼起来的 DDL 全部搬进 `migrations/`：

```
migrations/0001_init.sql                recording_segments / audio_gaps / snore_events(v1)
migrations/0002_segment_transcripts.sql 逐段转写
migrations/0003_utterances.sql          流式识别的短语
migrations/0004_events_source.sql       重建 snore_events，加 source 与分类器列
```

`crates/server/src/migrate.rs` 是应用器：`include_dir!` 在编译期把目录嵌进二进制，
按版本号升序执行，每个文件在自己的事务里连同记账行一起落地，记账表就是 sqlx 的
`_sqlx_migrations`（version / description / success / checksum / execution_time），
checksum 是文件字节的 SHA-384。`PRAGMA user_version` 仍然在每次成功后写一次，
给还在读它的工具和运维清单用。

`Database::migrate()` 只剩一行委托，`SCHEMA_VERSION` 常量删掉 —— 最新版本由目录里
最大的文件名决定，不需要第二个地方同步。

三种现场状态都必须能启动：

| 库的状态 | 行为 |
|---|---|
| 空文件 | 从 0001 依次执行 |
| 老库（有 `user_version`，没有记账表） | 把 ≤ `user_version` 的文件补记为已应用，**不重跑 DDL**，然后继续 |
| 被 `sqlx migrate run` 建过的库 | 记账表非空，按它判断，顺手把 `user_version` 补到最新 |

四种状态必须拒绝启动：记账表里有本二进制不认识的版本、已发布文件被改动（checksum
不符）、`success = false`、`user_version` 比最新文件还大。全部点名版本号。

## 为什么不是 `sqlx::migrate` 本身

因为它装不进来。`rusqlite` 0.40 要 `libsqlite3-sys ^0.38`，`sqlx` 0.9 的 SQLite 驱动
要 `>=0.30.1, <0.38.0`，两个 crate 都声明 `links = "sqlite3"`，cargo 直接拒绝解析：

```
package `libsqlite3-sys` links to the native library `sqlite3`, but it conflicts
with a previous package which links to `sqlite3` as well:
  libsqlite3-sys v0.38.0   (required by rusqlite v0.40.0)
  versions that meet `>=0.30.1, <0.38.0` ... (required by sqlx-sqlite v0.9.0)
failed to select a version for `libsqlite3-sys` which could resolve this conflict
```

要换 `sqlx::migrate`，就得把整个存储层改成 sqlx 的异步连接池，`DbWriter`（单写入
线程 + 一条 rusqlite 连接）这个模型随之作废，`http_server`、两个 STT worker、启动
retention 各自开连接的分工也要重排。这是为迁移工具付一次存储层重写，代价和收益不成比例。

真正要的东西只有两件：**SQL 有版本、可 review，DDL 不在 Rust 里**；以及**部署机上
能问出"这个库到底跑到哪一版"**。按 sqlx 的文件格式和记账格式写，两件都拿到了 ——
`sqlx` 的 CLI 能把线上库读得很准，而不需要它就是运行时。

## 实测（本机 macOS，arm64，sqlx-cli 0.9.0）

- 应用建库 → `sqlx migrate info --source migrations --database-url sqlite:///tmp/steno-check.db`：

  ```
  1/installed init
  2/installed segment transcripts
  3/installed utterances
  4/installed events source
  ```

  记账表四条，checksum 各 48 字节（SHA-384），`user_version = 4`。

- 反向：`sqlx database create` + `sqlx migrate run` 建库，再让应用启动接管 —— 无重复
  DDL、无报错，`user_version` 由应用补到 4。

- 现场库拷贝（`var/db/app.db` 的副本：`user_version = 3`、4614 行 `snore_events`、
  没有记账表）：启动后基线 1–3、执行 0004，**4614 行一行不少**，`snore_events` 多出
  `source` 列，`user_version = 4`。

- `cargo test -p steno-server` 82 通过，其中 `migrate` 模块 6 个：命名解析、幂等、
  老库基线、未知版本、checksum 被改、`user_version` 超前。

## 操作约定

- 新迁移用 `sqlx migrate add --source migrations <描述>`，CLI 会接着最大版本号编
  `0005_…`；把占位注释替换成 SQL。不要给已发布的文件重新编号。
- 已发布的文件永不修改。改了就是启动失败，这是设计，不是障碍。
- 文件里不写 `PRAGMA user_version`，由应用器在记账行之后写；这样同一份文件交给
  `sqlx migrate run` 也正确。
- 只有前进没有回退（不写 `.down.sql`）。回滚 = 恢复备份；这个库里的音频与标注
  不可再生，能自动回退的迁移反而更容易被误用。
- `sqlx` CLI 不是运行依赖，只是开发与排障工具；部署机上没有它不影响启动。
