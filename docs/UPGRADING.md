# 升级与格式兼容

unionid 0.1.0 把应用 schema migration 与数据库内部格式升级视为两件不同的事。应用字段、类型和变体的变化使用版本化 migration；内部格式版本只由明确支持它的 unionid 二进制打开。

## v0.1 格式契约

发布包中的 `RELEASE.json` 记录构建版本、Rust target、工具链和以下格式版本：

| 层 | v0.1 值 |
| --- | --- |
| unionid / redb | 0.1.0 / 4.1.0 |
| storage / catalog / ADT value | 可读 1–5，当前 5 / 4 / 2 |
| index key / migration ledger / receipt | 3 / 1 / 2 |
| logical backup / JSON Lines protocol | 可读 1–4，当前 4 / 1–2 |

当前二进制读取 storage format 1–5；新数据库直接创建为 format 5，使用 catalog/value/index-key/receipt codec 4/2/3/2。format 1/2 仍会补齐 cursor 身份并升级到 format 3；format 3 可显式升级到 4 以使用生产标量，format 4 可继续读写已有单列升序索引。创建复合或降序索引前必须执行 `unionid upgrade --db app.redb --target 5`。升级在一个同步 two-phase redb transaction 中重写 catalog 与 secondary-index keys，并保留 rows、RowId、schema identity、ledger、receipts、数据库/cursor 身份和 sequence。预检或提交前失败保留完整旧格式；提交结果不确定时应重开并执行 `check --db`。逻辑 backup 4 保存复合索引 shape、生产标量和 receipt；backup 3 单列索引按一个升序 component 恢复。restore 会生成新数据库身份。未知 storage 或 codec version 在修改文件前失败。

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

The current binary reads storage formats 1–5. New databases start at format 5 with catalog/value/index-key/receipt codecs 4/2/3/2. Formats 1 and 2 still gain cursor identity and move to format 3; format 3 can be explicitly upgraded to 4 for production scalars. Format 4 remains readable and writable for existing ascending single-column indexes, but composite or descending declarations require `unionid upgrade --db app.redb --target 5`. The synchronous two-phase transaction rewrites the catalog and secondary-index keys while preserving rows, RowIds, schema identity, ledger, receipts, database/cursor identity, and sequence. Preflight or pre-commit failure leaves the complete old format; after an uncertain commit, reopen and run `check --db`. Logical backup 4 preserves composite shapes, production scalars, and receipts; backup 3 maps each legacy column to one ascending component. Restore rotates cursor identity.

Before changing binaries, retain the old binary's `version --format json` and `doctor --db app.redb --format json` reports, run `check`, create a verified logical backup, and keep the old release archive and checksum. Compare the new binary's readable/current storage and protocol versions, then run doctor, check, migration planning, and application queries against a quiescent copy. Doctor diagnoses a private temporary copy and never creates, repairs, or upgrades the requested path; it is not a replacement for checking the actual copy. Automation should branch on the documented CLI exit classes rather than matching prose. Open the production file only when the target release notes declare support for its stored versions. Any future internal-format conversion must be explicit and write a separately verifiable path; v0.1 makes no promise of unannounced in-place upgrades.
