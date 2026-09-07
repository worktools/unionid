# 升级与格式兼容

unionid 0.1.0 把应用 schema migration 与数据库内部格式升级视为两件不同的事。应用字段、类型和变体的变化使用版本化 migration；内部格式版本只由明确支持它的 unionid 二进制打开。

## v0.1 格式契约

发布包中的 `RELEASE.json` 记录构建版本、Rust target、工具链和以下格式版本：

| 层 | v0.1 值 |
| --- | --- |
| unionid / redb | 0.1.0 / 4.1.0 |
| storage / catalog / ADT value | 1 或 2 / 2 / 1 |
| index key / migration ledger / receipt | 1 / 1 / 1 |
| logical backup / JSON Lines protocol | 1 或 2 / 1 |

当前二进制打开 storage format 1（无 receipt）和 format 2（含 durable idempotency receipt）。第一次成功提交持久幂等写入时会在同一事务选择 format 2；此后不能降级到只认识 format 1 的旧二进制。逻辑备份相应使用无 receipt 的 format 1 或含 receipt 的 format 2。未知 storage、catalog、value、index、migration 或 receipt codec 会在修改文件前失败；不会猜测或静默重写。命名类型、字段、变体、表和索引用稳定 ID 编码，应用侧重命名必须通过 migration，不能直接编辑数据库文件。

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

1. 用旧二进制运行 `check`，并保存可校验逻辑备份。
2. 保留旧二进制、发布压缩包和 `.sha256`，直到新版本验证完成。
3. 在数据库副本上运行新二进制的 `check`、migration plan 和应用查询。
4. 只有目标版本的 release notes 明确声明支持当前内部格式时，才让它打开生产文件。
5. 如果内部格式发生变化，使用该版本提供的显式转换工具；先写到新路径并验证，再切换应用。

逻辑 `restore` 只写入不存在的新 redb 路径，便于并行验证和回退。v0.1 不承诺未来二进制自动原地升级内部格式，也不把正常重开当作备份。

原型 WAL/snapshot 是过渡输入，不是正式 redb 格式。只能使用 `import-legacy` 显式转换到新路径；它们不会与 `--db` 双写，也不能改名后直接作为 redb 打开。

## English compatibility policy

unionid 0.1.0 separates application schema migrations from internal database-format changes. Versioned migration files evolve fields, variants, types, constraints, indexes, and their data. A unionid binary opens only the internal codec versions it explicitly knows and fails before mutation when it encounters an unknown version.

Before changing binaries, run `check` with the old binary, create a verified logical backup, retain the old release archive and checksum, and test the new binary against a copy. Open the production file only when the target release notes declare support for its stored versions. Any future internal-format conversion must be explicit and write a separately verifiable path; v0.1 makes no promise of silent in-place upgrades.
