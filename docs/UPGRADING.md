# 升级与格式兼容

unionid 把应用 schema migration 与数据库内部格式升级视为两件不同的事。应用字段、类型和变体的变化使用版本化 migration；内部格式版本只由明确支持它的 unionid 二进制打开。

## v0.9 格式与源码契约

v0.9 发布包同时携带 `RELEASE.json` 和独立的 `release/contract.json`。打包和验证脚本要求 Cargo、二进制 version report、manifest 与契约一致：

| 层 | v0.9 值 |
| --- | --- |
| unionid / 最低 Rust / redb | 0.9.1 / 1.94 / 4.1.0 |
| storage / catalog / ADT value | 可读 1–7，默认当前 6 / 4 / 2；maintenance codec 1 |
| index key / migration ledger / receipt | 3 / 1 / 2 |
| incremental journal | 默认 0；显式启用的 format 7 使用 1 |
| logical backup / JSON Lines protocol / stream | 可读 1–4，当前 4 / 1–2 / 1 |

公开 v0.1.0 使用 storage/catalog/value/index/migration/backup/protocol version 1，并不包含 receipt、cursor identity、生产标量、复合索引或 generation envelope。v0.2.0 引入 format 6；v0.3.0 增加 Rust 接入、关联读取与 migration 使用体验；v0.4.0 增加可移植 ADT 契约与静态查询绑定；v0.5.0 增加显式 format-7 增量备份 journal；v0.6.0 增加安全项目骨架和项目级静态检查；v0.7.0 采用 Rust 形状的规范源码，并增加 typed membership、相关 exists 与集合运算；v0.8.0 增加 count_distinct、typed avg 和基础排名窗口；v0.9.0 增加 schema-aware 内联 Rust 查询宏；v0.9.1 修复宏生成代码与调用模块 serde imports 的名称冲突。v0.9 不改变持久化、backup 或协议数值。上表描述 v0.9.1 二进制的默认写入格式和全部可读范围，不追溯改写旧版本契约。

当前二进制读取 storage format 1–11；新数据库直接创建为 format 10，使用 catalog/value/index-key/receipt/maintenance/journal codec 6/3/4/3/1/0，并写入 logical backup 6。format 1/2 仍会补齐 cursor 身份并升级到 format 3；format 3 可显式升级到 4 以使用生产标量，format 4 可继续读写已有单列升序索引。创建复合或降序索引前必须先升级到 format 5，typed map 与 partial unique index 分别要求升级到 format 8 与 10。

只有 `backup incremental init` / `Engine::enable_backup_journal` 会进入 journal 格式：format 6 进入 7，format 8 进入 9，format 10 进入 11；创建数据库和普通写入仍保持 format 10。journal 格式增加 journal codec 1，没有原地降级；需要回到不理解 journal 格式的二进制时，应从启用前的 logical backup 恢复到新路径。

升级或切换维护版本前，活跃增量链应先执行 `backup incremental export` 和 `backup incremental verify`。需要限制 archive 历史时，先 checkpoint，再 preview/confirm prune；不要直接删除 manifest 引用的文件。停止增量记录时使用 `backup incremental disable`。存在未导出提交时应先 export；只有明确接受这些 sequence 不可恢复时才使用 `--discard-unexported --confirm`。停用不会把 format 7 原地降回 format 6。

Before upgrading or switching maintenance binaries, export and verify any active incremental chain. To bound archive history, checkpoint first and then preview/confirm prune; never delete manifest-referenced files directly. Use incremental disable to stop journaling. Export an unsealed tail unless its restore points are deliberately abandoned with `--discard-unexported --confirm`. Disabling does not downgrade storage format 7 to format 6.

## 源文件后缀：`.uid` → `.unid`

`.unid` 是 Unionid 源文件（schema、query、migration）的规范后缀，`.uid` 进入兼容窗口：

- `run`/`cli --file`、`schema check`、`fmt`、`query rust` 以及 migration/query 目录扫描同时接受 `.unid` 和 `.uid`。
- 使用 `.uid` 时 stderr 输出稳定的弃用提示 `'<path>' uses the deprecated .uid extension; rename it to .unid`；文件内容与执行语义不变。
- `migration new` 生成 `.unid`。
- migration checksum 只基于文件内容、不包含路径，因此把已有 migration 文件从 `.uid` 重命名为 `.unid` 不会改变已应用的 ledger，也不会触发重复应用。
- 自动化迁移：`python3 scripts/migrate-uid-to-unid.py <目录> --dry-run` 预览、去掉 `--dry-run` 执行批量重命名（用 `--keep` 跳过需要保留 `.uid` 的兼容 fixture）。随后把应用、脚本和生成命令中的路径改为 `.unid`，再运行 `unionid migration status` 确认 applied/pending 不变。
- 回滚：窗口内把 `.unid` 改回 `.uid` 即可，内容与 ledger 不受影响。
- 兼容窗口计划在 **v1.0.0** 关闭；届时二进制只接受 `.unid`，未迁移的项目需要先在此窗口内完成重命名。

### 从 v0.8.0 升级

v0.9.0 可直接打开 v0.8.0 的 format-6/7 数据库，不要求 storage upgrade、应用 schema migration 或查询源码重写。使用内联 Rust 查询时，把 `unionid` 与新的 `unionid-query` 依赖固定为同一个精确版本；宏只接受声明式 schema，migration 建立的 live stable ID 继续通过 `query rust --db` 生成。已有 `.unid` 查询和 v0.8 生成物仍可按原路径使用，但重新生成的 binding 会记录 v0.9 software version，应随应用一起重新编译。

完整 checklist、编译成本和边界见 [v0.9.0 发布说明](RELEASE-v0.9.0.md)。

### 从 v0.7.0 升级

v0.9.0 可直接打开 v0.7.0 的 format-6/7 数据库，不要求 storage upgrade、应用 schema migration 或源码重写。v0.8 新增的查询能力保持向后兼容：`count_distinct` 使用完整 typed equality；`avg int` 返回 `Option<float>`，float/duration avg 保留命名类型；`window` 要求显式 sort，并只追加 `int` 排名字段。静态查询生成物绑定软件版本，应以 v0.9 `query rust` 重新生成并重新编译。

完整 checklist 与能力边界见 [v0.8.0 发布说明](RELEASE-v0.8.0.md)。

### 从 v0.6.0 升级

v0.9.0 可直接打开 v0.6.0 的 format-6/7 数据库，不要求 storage upgrade 或应用 schema migration。升级的 breaking boundary 在源码：规范类型、值、pattern、bool operator 和 range 改为 Rust 形状。parser 暂时接受 v0.6 拼写，`unionid fmt` 可生成新规范形式；但旧 `take 2..4` 是闭区间，v0.7 的同一文本是半开区间。先逐条决定 `..` 或 `..=`，再运行 formatter、`project check` 和业务验收。静态查询生成物绑定软件版本，必须用 v0.9 `query rust` 重新生成并重新编译。

Rust 形状源码迁移的完整 checklist 见 [v0.7.0 发布说明](RELEASE-v0.7.0.md)；当前聚合与窗口能力及边界见 [v0.8.0 发布说明](RELEASE-v0.8.0.md)。

### 从 v0.3.0–v0.5.0 升级

v0.9.0 可直接打开 v0.3.0–v0.5.0 的 format-6 数据库，也继续读取由 v0.5.0 显式启用增量备份后产生的 format-7 数据库。先在副本上运行 `doctor`、`check` 和应用读写。先按上节处理 Rust 形状源码与 range 语义；若应用使用静态查询生成物，再使用 v0.9.0 binary 对当前 schema 与全部 `.unid`（或兼容 `.uid`）查询重新运行 `query rust`，提交新的 digest，并重新编译客户端。v0.9.0 不要求 storage upgrade 或 schema migration；`init` 和 `project check` 只影响新项目与源码检查。

### 从公开 v0.1.0 升级

仓库使用公开 v0.1.0 ARM64 产物生成的真实 format-1 数据库与 logical-backup 夹具持续验证升级链，来源、SHA-256 和生成源码见 [`tests/fixtures/v0.1.0/README.md`](../tests/fixtures/v0.1.0/README.md)。该库包含 migration ledger、命名 sum/record、嵌套 option/list、主键和嵌套字段索引。

先用旧二进制运行 `check` 和 `backup`，保留原文件，再复制数据库进行演练。新二进制的 `doctor --db` 只检查私有副本，不修改请求路径。format 1/2 第一次由新二进制可写打开时只补齐随机 cursor identity，并兼容归一到 format 3；因此第一条 `upgrade --target 4` 会报告 `previous_format: 3`。之后每个格式转换都必须显式执行：

```bash
cp app.redb app-v0.9-rehearsal.redb
unionid doctor --db app-v0.9-rehearsal.redb --format json
unionid upgrade --db app-v0.9-rehearsal.redb --target 4
unionid upgrade --db app-v0.9-rehearsal.redb --target 5
unionid upgrade --db app-v0.9-rehearsal.redb --target 6
unionid check --db app-v0.9-rehearsal.redb
```

只有副本上的 schema identity、migration ledger、typed rows、索引查询、应用 mutation、重开和 backup/restore 全部通过后才切换生产路径。version 1 protocol 对 v0.1 标量和 ADT 查询保持兼容；UUID、时间、decimal 与 bytes 参数需要 version 2。logical backup 1 可由当前二进制直接还原为新的 format-6 数据库，这是保留原库的替代迁移路径。

显式 `unionid upgrade --db app.redb --target 6` 要求源库已经是 format 5。该升级在一个同步 two-phase redb transaction 中创建固定的 generation catalog/row/index/manifest 表，并把原 format-5 表登记为 `Legacy0`；它不遍历或重写 catalog、rows、indexes、ledger 或 receipts。schema、sequence、RowId、ledger、receipt、database/cursor identity 和全部逻辑 codec 均保持不变。新建 format-6 库从 `Generated(1)` 开始，后续 generation ID 只按持久 `next_generation_id` 单调分配且不复用。

预检或提交前失败保留完整旧格式；提交结果不确定时应重开并执行 `check --db`，重开只会看到完整 format 5 或完整 format 6。旧 binary 会因未知 format 6 拒绝打开。没有原地 downgrade；需要回滚 binary 时，应在升级前创建 logical backup 4，再由旧 binary restore 为它支持的新库。切换到 generated keyspace 后也只能通过 logical backup/restore 回到旧格式。restore 会生成新数据库身份，因此源库 cursor 不能用于副本。未知 storage、component codec、generation key 或 manifest version 均在修改文件前失败。

升级前后的 backup/restore 必须保留 receipt count。不要为了降级而删除 receipt：显式 prune 会恢复旧 key 的可执行性，应只在确认所有客户端、队列和人工重试都已越过 cutoff 后执行。旧 version 1 请求继续可用；只有需要 exactly-once effect 的 mutation 才增加 `idempotency_key`。

## 升级应用 schema

把 migration 文件纳入应用源码并按顺序部署：

```bash
unionid migration plan --db app.redb --dir migrations
unionid backup --db app.redb --output before-upgrade.backup.json
unionid migration apply --db app.redb --dir migrations
# 或在运维循环中重复执行，直到 JSON 的 complete 为 true
unionid migration advance --db app.redb --dir migrations --max-steps 4 --step-delay-ms 250 --format json
unionid check --db app.redb
unionid migration status --db app.redb --dir migrations
```

每个文件有不可变 checksum 和 parent。storage format 6 先在可恢复 shadow generation 中分批构建 catalog、数据和索引，完整验证后再用一个同步事务原子切换 schema、active generation 和 ledger。数据库存在 ledger 后，普通 DDL 不能绕过 runner 修改 schema。

迁移期间查询继续读取旧 generation，普通写入被拒绝，以免 checkpoint 的 source 发生变化。中断后先运行 `migration status`；使用完全相同的文件再次 `apply` 会恢复，或用 `migration advance --max-steps N` 把每次工作限制为 N 个已提交 maintenance step。需要降低持续 I/O/CPU 压力时增加 `--step-delay-ms M`，它只在相邻提交之间暂停，因此会延长写阻塞窗口，不代表零停机或确定吞吐率。确认放弃时运行 `migration abort --db app.redb`。接近 100,000 行时仍应预留 maintenance 窗口，并先在生产数据副本上运行相同 migration、`check` 和业务读路径。新的 [M9 长期 churn 记录](benchmarks/churn-2026-09-10.md)验证三代 migration 与 build/cutover/reclaim 中断恢复，并观察到 redb 在逻辑 reclaim 后保留第一次 shadow migration 的文件高水位；容量规划不能把 reclaim 当作物理压缩。shadow-generation 分段基线见 [M7 验收记录](benchmarks/m7-acceptance-2026-09-10.md)，此前的 full-rebuild 基线见 [M6 工作负载记录](benchmarks/workload-2026-09-09.md)。

## 升级 unionid 二进制

1. 保存旧二进制的 `version --format json` 和 `doctor --db app.redb --format json` 输出，再运行 `check` 并创建可校验逻辑备份。
2. 保留旧二进制、发布压缩包和 `.sha256`，直到新版本验证完成。
3. 先用新二进制运行 `version --format json`，比较 `readable_storage_formats`、`current_storage` 和 `protocol_versions`；再在数据库副本上运行 `doctor`、`check`、migration plan 和应用查询。
4. 只有目标版本的 release notes 明确声明支持当前内部格式时，才让它打开生产文件。
5. 如果内部格式发生变化，使用该版本提供的显式转换工具；先写到新路径并验证，再切换应用。

逻辑 `restore` 只写入不存在的新 redb 路径，便于并行验证和回退。v0.9 不承诺未来二进制自动原地升级内部格式，也不把正常重开当作备份。

升级或多代 schema migration 后，generation reclaim 完成并不表示 redb 文件已经物理缩小。若需要回收文件高水位，应在升级验证结束后另设停机窗口：停止 server，创建并验证 logical backup，确认 `migration status` 没有 maintenance，再运行 `unionid compact --db app.redb`。compact 不改变 storage format、schema、sequence、ledger、RowId、receipt 或 cursor identity，也不替代升级前备份。它在 native compact 后重开数据库以固化 redb 的 region 布局，报告的 `after_bytes` 是重开后的 durable 大小，`changed` 只在文件实际变小时为 true；按 [#229 churn 数据](benchmarks/compaction-2026-09-11.md)，100k 可从约 744.84 MiB 回落到约 219.18 MiB。成功的完整压缩会尝试写入经过认证的相邻 proof；数据库实例、sequence、schema、codec 和文件身份均未变化时，下次调用跨进程快速返回 no-op。若 native compact 已成功但 proof 发布失败，命令仍返回压缩结果并报告 `proof_persisted=false`，下一次调用会保守执行完整路径。写入、文件替换、proof 缺失或无效也都会执行完整路径；proof 可随时删除，不是备份。完整路径会多次扫描并执行 redb 内部提交；Ctrl-C 或不确定错误后必须重开并运行 `check`。

原型 WAL/snapshot 是过渡输入，不是正式 redb 格式。只能使用 `import-legacy` 显式转换到新路径；它们不会与 `--db` 双写，也不能改名后直接作为 redb 打开。

`doctor` 有意不对生产路径执行 redb 打开：它读取权限受限的临时副本并删除副本，因此不会触发恢复或格式变化。它适合部署前兼容性探测，但不能替代对静止副本执行 `check`。部署脚本应按 [CLI 退出码](CLI.md#json-错误与退出码)区分参数、输入、连接、存储和完整性失败，而不是匹配错误句子。

## English compatibility policy

unionid separates application schema migrations from internal database-format changes. Versioned migration files evolve fields, variants, types, constraints, indexes, and their data. A unionid binary opens only the internal codec versions it explicitly knows and fails before mutation when it encounters an unknown version.

The v0.9 archive contains both `RELEASE.json` and an independent `release/contract.json`. Packaging and verification require Cargo metadata, the binary version report, the manifest, and this contract to agree. v0.9.1 requires Rust 1.94, uses redb 4.1.0, reads storage formats 1–7 and logical backup formats 1–4, creates storage format 6 by default, writes logical backup format 4, supports data protocols 1/2, and supports stream protocol 1. The detailed component codecs are frozen in the contract.

The public v0.1.0 release used version 1 for storage, catalog, values, index keys, migration records, backup, and the JSON Lines protocol. It did not contain receipts, cursor identity, production scalars, composite indexes, or generation envelopes. v0.2.0 introduced format 6; v0.3.0 added Rust integration, relational reads, and migration UX; v0.4.0 added portable ADT contracts and static query bindings; v0.5.0 added the explicitly enabled format-7 incremental-backup journal; v0.6.0 added safe project scaffolding and project-level static checking; v0.7.0 adopted canonical Rust-shaped source and added typed membership, correlated exists, and set operations; v0.8.0 added count_distinct, typed averages, and basic ranking windows; v0.9.0 added schema-aware inline Rust query macros; v0.9.1 fixes generated-name conflicts with caller serde imports. v0.9 does not change persistence, backup, or protocol values. The v0.9 contract describes default current writes and the complete readable range without changing older release contracts.

The current binary reads storage formats 1–11. New databases start at format 10 with catalog/value/index-key/receipt/maintenance/journal codecs 6/3/4/3/1/0 and write logical backup 6. Formats 1 and 2 still gain cursor identity and move to format 3; format 3 can be explicitly upgraded to 4 for production scalars. Format 4 remains readable and writable for existing ascending single-column indexes, but composite or descending declarations first require format 5, typed maps require format 8, and partial unique indexes require format 10.

Only `backup incremental init` / `Engine::enable_backup_journal` enters a journal format: 6 enters 7, 8 enters 9, and 10 enters 11; database creation and ordinary writes remain on format 10. A journal format adds journal codec 1 and has no in-place downgrade. To return to a binary that does not understand it, restore the logical backup made before enablement into a new path.

### Source file extension: `.uid` to `.unid`

`.unid` is the canonical extension for Unionid source files (schema, query, migration); `.uid` is in a compatibility window. `run`/`cli --file`, `schema check`, `fmt`, `query rust`, and migration/query directory scans accept both. A `.uid` path prints a stable deprecation on stderr (`'<path>' uses the deprecated .uid extension; rename it to .unid`) without changing content or semantics, and `migration new` writes `.unid`. Migration checksums are content-only and do not include the path, so renaming an existing migration from `.uid` to `.unid` preserves the applied ledger and never re-applies it. Automate the rename with `python3 scripts/migrate-uid-to-unid.py <dir> --dry-run` (drop `--dry-run` to run; use `--keep` for compatibility fixtures that must stay `.uid`), then update references and confirm `migration status` is unchanged. Renaming back is also safe during the window. The compatibility window is planned to close in **v1.0.0**, after which only `.unid` is accepted and unmigrated projects must rename first.

### Upgrading from v0.8.0

v0.9.0 directly opens v0.8.0 format-6/7 databases without a storage upgrade, application schema migration, or query-source rewrite. For inline Rust queries, pin `unionid` and the new `unionid-query` dependency to the same exact version. The macro accepts declarative schemas only; use `query rust --db` when bindings depend on live stable IDs established by migrations. Existing `.unid` queries and v0.8 generated output remain usable through their existing path, while regenerated bindings record the v0.9 software version and should be rebuilt with the application. See the [v0.9.0 release notes](RELEASE-v0.9.0.md) for the checklist, compile cost, and boundaries.

### Upgrading from v0.7.0

v0.9.0 directly opens v0.7.0 format-6/7 databases without a storage upgrade, application schema migration, or source rewrite. The v0.8 query surface remains additive: count_distinct uses complete typed equality; integer avg returns `Option<float>` while float/duration averages preserve named types; and window requires an explicit sort and only appends `int` rankings. Static query output is software-version-bound, so regenerate with the v0.9 `query rust` command and rebuild clients. See the [v0.8.0 release notes](RELEASE-v0.8.0.md) for the aggregate/window checklist and the [v0.9.0 release notes](RELEASE-v0.9.0.md) for the current boundary.

### Upgrading from v0.6.0

v0.9.0 directly opens v0.6.0 format-6/7 databases without a storage upgrade or application schema migration. The breaking boundary is source code: canonical types, values, patterns, boolean operators, and ranges adopt the Rust-shaped form. The parser temporarily accepts v0.6 spellings and `unionid fmt` emits the new form, but legacy `take 2..4` was inclusive while v0.7 interprets the same text as half-open. Decide explicitly between `..` and `..=` before formatting, running `project check`, and repeating application acceptance. Regenerate version-bound static queries with the v0.9 `query rust` command and rebuild clients. See the [v0.7.0 release notes](RELEASE-v0.7.0.md) for the Rust-shaped source migration checklist, [v0.8.0](RELEASE-v0.8.0.md) for aggregate/window limits, and [v0.9.0](RELEASE-v0.9.0.md) for the current boundary.

### Upgrading from v0.3.0 through v0.5.0

v0.9.0 directly opens v0.3.0–v0.5.0 format-6 databases and continues to read format-7 databases created when v0.5.0 explicitly enabled incremental backup. Run doctor, check, and application reads/writes on a copy first. Apply the Rust-shaped source and range review above, then rerun `query rust` with v0.9.0 over the current schema and every `.unid` (or compatible `.uid`) query, commit the new digests, and recompile the client. v0.9.0 requires no storage upgrade or schema migration; `init` and `project check` affect only new projects and source validation.

### Upgrading from the public v0.1.0

The repository continuously validates this path with real format-1 database and logical-backup fixtures generated by the public v0.1.0 ARM64 artifact. Provenance, SHA-256 values, and generation sources are in [`tests/fixtures/v0.1.0/README.md`](../tests/fixtures/v0.1.0/README.md). The fixture includes a migration ledger, named sum/record values, nested option/list values, a primary key, and a nested-field index.

Run `check` and `backup` with the old binary, retain the original file, and rehearse on a copy. The new binary's `doctor --db` examines a private copy and does not modify the requested path. The first writable open by the new binary adds only a random cursor identity to formats 1/2 and compatibly normalizes them to format 3, so the first `upgrade --target 4` reports `previous_format: 3`. Every later conversion is explicit:

```bash
cp app.redb app-v0.9-rehearsal.redb
unionid doctor --db app-v0.9-rehearsal.redb --format json
unionid upgrade --db app-v0.9-rehearsal.redb --target 4
unionid upgrade --db app-v0.9-rehearsal.redb --target 5
unionid upgrade --db app-v0.9-rehearsal.redb --target 6
unionid check --db app-v0.9-rehearsal.redb
```

Switch the production path only after schema identity, migration ledger, typed rows, indexed queries, application mutations, reopen, and backup/restore all pass on the copy. Protocol version 1 remains compatible with v0.1 scalar and ADT queries; UUID, temporal, decimal, and bytes parameters require version 2. The current binary can also restore logical backup 1 directly into a new format-6 database, providing an alternative migration path that retains the original database.

`unionid upgrade --db app.redb --target 6` requires a format-5 source. One synchronous two-phase redb transaction creates the fixed generation catalog/row/index/manifest tables and records the existing format-5 tables as `Legacy0`; it does not traverse or rewrite the catalog, rows, indexes, ledger, or receipts. Schema, sequence, RowIds, ledger, receipts, database/cursor identity, and all logical component codecs stay unchanged. Fresh format-6 databases begin at `Generated(1)`, and durable `next_generation_id` allocation is monotonic and never reused.

Preflight or pre-commit failure leaves the complete old format. After an uncertain commit, reopen and run `check --db`; reopen observes only a complete format 5 or complete format 6. Older binaries reject unknown format 6. There is no in-place downgrade: create a logical backup 4 before upgrading if an older binary may be needed, then let that binary restore a new database it supports. After cutover to a generated keyspace, logical backup/restore is also the only path back to an older format. Restore rotates database/cursor identity. Unknown storage, component-codec, generation-key, or manifest versions fail before mutation.

Storage format 6 builds each migration in a checkpointed shadow generation and atomically switches the active generation, schema identity, sequence, and ledger after bounded validation. Reads continue against the old generation while ordinary mutations are blocked. After interruption, inspect `migration status`; rerun `apply` with the exact same file to resume, or repeat `migration advance --max-steps N --format json` to cap each call at N committed maintenance steps until `complete` is true. Add `--step-delay-ms M` to pause between adjacent commits when sustained I/O or CPU pressure matters. The delay extends the write-blocking window and does not claim zero downtime or a fixed throughput rate. Use `migration abort --db app.redb` to discard an uncommitted target. Near 100,000 rows, still reserve a maintenance window and rehearse the exact migration, check, and application reads on a production-data copy. The [M9 long-term churn record](benchmarks/churn-2026-09-10.md) verifies three migration generations and interruption recovery at build, cutover, and reclamation, and observes that redb retains the first shadow migration's file high-water mark after logical reclamation. Capacity planning must not treat reclamation as physical compaction. The [M7 acceptance record](benchmarks/m7-acceptance-2026-09-10.md) retains the shadow-generation phase baseline; the previous full-rebuild baseline remains in the [M6 workload record](benchmarks/workload-2026-09-09.md).

Before changing binaries, retain the old binary's `version --format json` and `doctor --db app.redb --format json` reports, run `check`, create a verified logical backup, and keep the old release archive and checksum. Compare the new binary's readable/current storage and protocol versions, then run doctor, check, migration planning, and application queries against a quiescent copy. Doctor diagnoses a private temporary copy and never creates, repairs, or upgrades the requested path; it is not a replacement for checking the actual copy. Automation should branch on the documented CLI exit classes rather than matching prose. Open the production file only when the target release notes declare support for its stored versions. Any future internal-format conversion must be explicit and write a separately verifiable path; v0.9 makes no promise of unannounced in-place upgrades.

Generation reclamation after upgrades or repeated schema migrations does not guarantee physical file shrinkage. If the file high-water mark must be reclaimed, schedule a separate offline window after upgrade validation: stop the server, create and verify a logical backup, resolve migration maintenance, then run `unionid compact --db app.redb`. Compaction preserves the storage format, schema, sequence, ledger, RowIds, receipts, and cursor identity and does not replace the pre-upgrade backup. It reopens the database after native compact to persist redb's region layout, so the reported `after_bytes` is the durable post-reopen size and `changed` is true only when the file actually got smaller. On the [#229 churn data](benchmarks/compaction-2026-09-11.md), 100k drops from about 744.84 to 219.18 MiB. A successful complete run attempts to publish an authenticated adjacent proof, allowing an unchanged database to return a cross-process fast no-op on the next invocation. If native compaction succeeds but proof publication fails, the command still returns the compaction result with `proof_persisted=false`; the next invocation conservatively runs the complete path. A write, replacement, missing, or invalid proof also runs the complete path. The proof is disposable and is not a backup. A complete run performs multiple full traversals and native redb commits; after Ctrl-C or an uncertain result, reopen and run `check`.
