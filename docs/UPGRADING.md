# 升级与格式兼容

unionid 0.1.0 把应用 schema migration 与数据库内部格式升级视为两件不同的事。应用字段、类型和变体的变化使用版本化 migration；内部格式版本只由明确支持它的 unionid 二进制打开。

## v0.1 格式契约

发布包中的 `RELEASE.json` 记录构建版本、Rust target、工具链和以下格式版本：

| 层 | v0.1 值 |
| --- | --- |
| unionid / redb | 0.1.0 / 4.1.0 |
| storage / catalog / ADT value | 可读 1–6，当前 6 / 4 / 2；maintenance codec 1 |
| index key / migration ledger / receipt | 3 / 1 / 2 |
| logical backup / JSON Lines protocol | 可读 1–4，当前 4 / 1–2 |

当前二进制读取 storage format 1–6；新数据库直接创建为 format 6，使用 catalog/value/index-key/receipt/maintenance codec 4/2/3/2/1。format 1/2 仍会补齐 cursor 身份并升级到 format 3；format 3 可显式升级到 4 以使用生产标量，format 4 可继续读写已有单列升序索引。创建复合或降序索引前必须先升级到 format 5。

显式 `unionid upgrade --db app.redb --target 6` 要求源库已经是 format 5。该升级在一个同步 two-phase redb transaction 中创建固定的 generation catalog/row/index/manifest 表，并把原 format-5 表登记为 `Legacy0`；它不遍历或重写 catalog、rows、indexes、ledger 或 receipts。schema、sequence、RowId、ledger、receipt、database/cursor identity 和全部逻辑 codec 均保持不变。新建 format-6 库从 `Generated(1)` 开始，后续 generation ID 只按持久 `next_generation_id` 单调分配且不复用。

预检或提交前失败保留完整旧格式；提交结果不确定时应重开并执行 `check --db`，重开只会看到完整 format 5 或完整 format 6。旧 binary 会因未知 format 6 拒绝打开。没有原地 downgrade；需要回滚 binary 时，应在升级前创建 logical backup 4，再由旧 binary restore 为它支持的新库。切换到 generated keyspace 后也只能通过 logical backup/restore 回到旧格式。restore 会生成新数据库身份，因此源库 cursor 不能用于副本。未知 storage、component codec、generation key 或 manifest version 均在修改文件前失败。

升级前后的 backup/restore 必须保留 receipt count。不要为了降级而删除 receipt：显式 prune 会恢复旧 key 的可执行性，应只在确认所有客户端、队列和人工重试都已越过 cutoff 后执行。旧 version 1 请求继续可用；只有需要 exactly-once effect 的 mutation 才增加 `idempotency_key`。

## 升级应用 schema

把 migration 文件纳入应用源码并按顺序部署：

```bash
unionid migration plan --db app.redb --dir migrations
unionid backup --db app.redb --output before-upgrade.backup.json
unionid migration apply --db app.redb --dir migrations
unionid check --db app.redb
unionid migration status --db app.redb --dir migrations
```

每个文件有不可变 checksum 和 parent，单个 migration 在一个 redb 事务中同时修改 catalog、数据、索引和 ledger。数据库存在 ledger 后，普通 DDL 不能绕过 runner 修改 schema。

接近 100,000 行时，应把 migration 当作有界 maintenance 操作并预留停机窗口。M6 的代表性宽 row/复合索引工作负载中，普通单行持久写入 p95 约 10 ms，但 100k 深层 schema/data migration p95 约 49.9 s、peak RSS 约 1.44 GiB。先在生产数据副本上运行相同 migration、`check` 和业务读路径；不要用普通 DML 的增量成本估算 full-rebuild migration。环境和原始样本见 [M6 工作负载记录](benchmarks/workload-2026-09-09.md)。

## 升级 unionid 二进制

1. 保存旧二进制的 `version --format json` 和 `doctor --db app.redb --format json` 输出，再运行 `check` 并创建可校验逻辑备份。
2. 保留旧二进制、发布压缩包和 `.sha256`，直到新版本验证完成。
3. 先用新二进制运行 `version --format json`，比较 `readable_storage_formats`、`current_storage` 和 `protocol_versions`；再在数据库副本上运行 `doctor`、`check`、migration plan 和应用查询。
4. 只有目标版本的 release notes 明确声明支持当前内部格式时，才让它打开生产文件。
5. 如果内部格式发生变化，使用该版本提供的显式转换工具；先写到新路径并验证，再切换应用。

逻辑 `restore` 只写入不存在的新 redb 路径，便于并行验证和回退。v0.1 不承诺未来二进制自动原地升级内部格式，也不把正常重开当作备份。

原型 WAL/snapshot 是过渡输入，不是正式 redb 格式。只能使用 `import-legacy` 显式转换到新路径；它们不会与 `--db` 双写，也不能改名后直接作为 redb 打开。

`doctor` 有意不对生产路径执行 redb 打开：它读取权限受限的临时副本并删除副本，因此不会触发恢复或格式变化。它适合部署前兼容性探测，但不能替代对静止副本执行 `check`。部署脚本应按 [CLI 退出码](CLI.md#json-错误与退出码)区分参数、输入、连接、存储和完整性失败，而不是匹配错误句子。

## English compatibility policy

unionid 0.1.0 separates application schema migrations from internal database-format changes. Versioned migration files evolve fields, variants, types, constraints, indexes, and their data. A unionid binary opens only the internal codec versions it explicitly knows and fails before mutation when it encounters an unknown version.

The current binary reads storage formats 1–6. New databases start at format 6 with catalog/value/index-key/receipt/maintenance codecs 4/2/3/2/1. Formats 1 and 2 still gain cursor identity and move to format 3; format 3 can be explicitly upgraded to 4 for production scalars. Format 4 remains readable and writable for existing ascending single-column indexes, but composite or descending declarations first require format 5.

`unionid upgrade --db app.redb --target 6` requires a format-5 source. One synchronous two-phase redb transaction creates the fixed generation catalog/row/index/manifest tables and records the existing format-5 tables as `Legacy0`; it does not traverse or rewrite the catalog, rows, indexes, ledger, or receipts. Schema, sequence, RowIds, ledger, receipts, database/cursor identity, and all logical component codecs stay unchanged. Fresh format-6 databases begin at `Generated(1)`, and durable `next_generation_id` allocation is monotonic and never reused.

Preflight or pre-commit failure leaves the complete old format. After an uncertain commit, reopen and run `check --db`; reopen observes only a complete format 5 or complete format 6. Older binaries reject unknown format 6. There is no in-place downgrade: create a logical backup 4 before upgrading if an older binary may be needed, then let that binary restore a new database it supports. After cutover to a generated keyspace, logical backup/restore is also the only path back to an older format. Restore rotates database/cursor identity. Unknown storage, component-codec, generation-key, or manifest versions fail before mutation.

Near 100,000 rows, treat migrations as bounded maintenance operations and reserve a downtime window. In the representative M6 wide-row/composite-index workload, ordinary single-row durable writes have about 10 ms p95, while a 100k deep schema/data migration has about 49.9 s p95 and 1.44 GiB peak RSS. Run the exact migration, integrity check, and application reads against a production-data copy; incremental DML cost does not estimate a full-rebuild migration. See the [M6 workload record](benchmarks/workload-2026-09-09.md) for the environment and raw samples.

Before changing binaries, retain the old binary's `version --format json` and `doctor --db app.redb --format json` reports, run `check`, create a verified logical backup, and keep the old release archive and checksum. Compare the new binary's readable/current storage and protocol versions, then run doctor, check, migration planning, and application queries against a quiescent copy. Doctor diagnoses a private temporary copy and never creates, repairs, or upgrades the requested path; it is not a replacement for checking the actual copy. Automation should branch on the documented CLI exit classes rather than matching prose. Open the production file only when the target release notes declare support for its stored versions. Any future internal-format conversion must be explicit and write a separately verifiable path; v0.1 makes no promise of unannounced in-place upgrades.
