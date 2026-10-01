# unionid v0.11.0 发布说明

v0.11.0 聚焦本地外部数据查看与真实试用的正确性问题。

## 中文说明

- **本地 Parquet（#393、#394）**：直接查看 schema、行数与有限预览；以只读表 `data` 执行现有 typed pipeline，包括 filter、select、derive、group/aggregate 和 sort。`explain` 展示 Parquet scan 与顶层列投影。不会创建数据库或向 TCP/HTTP 开放文件访问。查询 JSON 失败与打开文件失败都输出单个 QueryResponse，避免重复错误 envelope。
- **增量迁移的项目检查（#398）**：`project check` 重放 migration 后比较规范声明结构，避免稳定 ID 与 fresh schema 的 hash 差异造成误报；仍检查真实字段、默认值、key、index 和 ADT 成员顺序差异。运行时 hash 与 Rust binding 身份契约保留。
- **可选邮箱唯一性（#405）**：普通 unique 把 `None` 作为一个值；允许多个未填写邮箱时使用 `create unique index users (email) if is_some email`。文本按精确值比较，应用需统一大小写、空白和 Unicode 规范化策略。见 [可运行示例](../examples/account_email.unid)。

```bash
unionid parquet examples/people.parquet --limit 0
unionid parquet examples/people.parquet --query 'from data | filter active | select {id, name} | sort id'
unionid docs show parquet
```

Parquet 仅支持单个本地文件与无损类型映射；不推断命名 enum，不支持远程存储、多文件/glob、稳定 page cursor 或导出。预览最多 1,000 行、64 MiB 保留数据；查询 source batch 最多 1,024 行、16 MiB 序列化 typed 数据，超大单行拒绝。上述预算不保证瞬时 Arrow 解码内存上限。完整说明见 [PARQUET.md](PARQUET.md)。

storage、backup、protocol 与 v0.10.0 相同：默认 storage 10，可读 1–11；backup 6，可读 1–6；protocol 1/2、stream 1。组件 codec 为 `6/3/4/3/1/0`（catalog/value/index-key/receipt/maintenance/journal）。无需内部格式升级或应用 migration。`unionid`、`unionid-derive` 与 `unionid-query` 统一使用 0.11.0；Parquet 为 CLI 增加依赖，现有 ADT 数据契约不变。

发布验收由 #411 跟踪：普通 Ubuntu CI、发布专用 Ubuntu/macOS 原生包验收、v0.10.0 format-10/11 数据库/备份/客户端兼容，以及空目录中的 Parquet、增量 migration project check 和可选邮箱示例。原子业务断言与逐语句结果（#399）仍待 v0.12 实现。

## English Description

- **Local Parquet (#393, #394)**: inspect schemas, row counts, and bounded previews; query the read-only `data` table with the existing typed pipeline. Explain reports the Parquet scan and top-level projection. File access stays local, without creating a database or exposing it to TCP/HTTP. Failed JSON queries and file-open failures emit one QueryResponse instead of duplicate error envelopes.
- **Project checks after incremental migrations (#398)**: replay migrations and compare canonical declaration structure, avoiding false failures caused by stable-ID differences. Actual declaration drift and ADT member order remain checked. Runtime hashes and Rust binding identities remain strict.
- **Optional-email uniqueness (#405)**: ordinary unique indexes treat `None` as one value. Use a partial unique index with `if is_some email` to allow multiple missing emails. Text equality is exact; applications must apply a consistent normalization policy on every write path.

Parquet supports a single local file and lossless mappings. It does not infer nominal enums or provide remote storage, globs, stable page cursors, or export. Preview retention is capped at 1,000 rows and 64 MiB; query source batches at 1,024 rows and 16 MiB of serialized typed values, rejecting an individually oversized row. These bounds do not cap transient Arrow decode memory. See [PARQUET.md](PARQUET.md).

Storage, backup, and protocol contracts match v0.10.0: default storage 10, readable 1–11; backup 6, readable 1–6; protocol 1/2 and stream 1. Component codecs remain `6/3/4/3/1/0`. No internal format upgrade or application migration is required. Keep all three Rust crates on 0.11.0 together.

Issue #411 tracks regular Ubuntu CI, dedicated Ubuntu/macOS archive acceptance, published v0.10.0 format-10/11 database/backup/client compatibility, and clean-directory Parquet, incremental-migration project-check, and optional-email journeys. Atomic business assertions and per-statement results (#399) remain planned for v0.12.
