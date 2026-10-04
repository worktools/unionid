# RFC 0026：类型化引用与 restrict / Typed references and restrict

- 状态 / Status: implemented on main; v0.14 acceptance in progress, unavailable in published v0.13
- Milestone: v0.14.0
- Tracking: [#401](https://github.com/worktools/unionid/issues/401)

## 中文说明

### 目标与声明

在数据库内保证业务关联的存在性，保留 ADT 完整类型和值相等语义。源字段路径引用目标表主键或全表 unique index；不提供 cascade 或 set-none。语法沿用 index 的表与路径列表结构，不增加分号：

```text
create reference lines (order_id) references orders (id)
create reference reservations (tenant, sku) references inventory (tenant, sku)
```

第二个例子要求目标存在恰好对应有序路径列表的全表 unique index。组件顺序影响引用身份，排序方向不影响相等性。若同一路径列表存在多个不同排序方向的唯一索引，绑定优先使用匹配主键，否则选择 stable ID 最小的全表 unique index；绑定结果固定进入 reference metadata，不在查询时重新选择。删除已绑定的目标 key 必须先 drop/re-add reference，即使另有等价 unique index，也不能静默改变依赖。组件数量复用索引上限 16。不接受跨 list/map 的展开路径；终点可为完整 ADT 值，路径本身遵守现有可绑定 record 路径规则。

顶层删除用 `drop reference lines (order_id) references orders (id)`。Migration 对应 `add reference ... references ...` 和 `drop reference ... references ...`；删除声明写全 source/target shape，避免添加另一套用户命名系统。重复声明报错。formatter、schema source、schema diff 共用同一规范语法，Rust 内联宏复用 parser 和 binder。

### 值与可选引用

每个组件在绑定阶段决定唯一的比较模式，保留命名 ADT 身份、decimal 精度和 scale；不隐式数值转换或跨 nominal type 比较。完整 sum/product/tuple/list/map 使用既有 typed key equality。

本 RFC 提议采用下列静态绑定规则：

1. 源 T 与目标 T 完全相同：Exact 模式，比较完整 typed value。T 本身为 Option 时，None 仍是普通值，必须有目标 None。
2. 若不满足规则 1，但源为 Option<T>、目标为 T：Optional 模式，只解开这一层 Option。None 表示无引用，Some(value) 查找目标 value。
3. 其他组合绑定失败；不递归拆开 Option，不自动包装目标，不把 enum 的其他 variant 当空值。

模式必须进入规范的 portable/schema 描述，不能只藏在运行时。复合引用任一 Optional 组件为 None 时，该行没有这条引用；其余 Exact 组件为 None 不会跳过检查。由此不存在未定义的 SQL NULL 三值比较。

具体业务例子：`tasks.assignee: Option<UserId>` 引用 `users.id: UserId` 时，未分配任务合法，Some(id) 必须对应用户；删除仍被已分配任务引用的用户失败，应用可以显式先把 assignee 更新为 None。`sessions.external_id: Option<ExternalId>` 引用另一张表同类型的全表 unique 字段时是 Exact 模式，None 需要目标 None。复合 `(tenant_id, assignee)` 中未分配行不建立引用，已分配行必须命中同一 tenant。业务若要求 tenant 独立存在，应另声明 tenant_id → tenants.id，不能声称复合引用替代这项约束。

这解决常见可选关联而不增加源 predicate 子语言。类型演进若改变 Exact/Optional 模式必须显式 drop/add reference 并扫描现存数据；schema hash 与 prepared drift 能检测改变。验收必须覆盖 Option<Option<T>>：同类型优先 Exact，Optional 只剥离一层，Some(None) 不等于无引用。

目标 partial unique index 不提供全表唯一身份；首版不能把它静默当作无条件唯一目标。需要条件目标的业务应在后续独立设计中定义行离开 predicate 的限制。源 predicate 与目标 partial index 是两个不同问题。

### 检查时点与原子性

建议按每条完整 DML 语句的候选最终状态检查。insert/upsert many 内先构造整批候选，再验证引用，允许批内自引用且与输入顺序无关。单条 update/delete 检查最终剩余关系；删除同一语句内全部自引用行可成功，有剩余行仍引用已删除 key 则失败。

跨语句脚本仍要求先插入 parent 再 child，先删除 child 再 parent。后面的语句不能修复前面的引用错误。任一失败回滚整个请求，包括 RowId、index、schema、receipt 和 journal，不能留下之前语句的成功效果。自引用可支持；相互引用表的 bootstrap 可以先装载有效数据，再在同一 schema migration 中声明引用。不引入 deferred constraint 模式。

并发写入的线性化边界沿用 Engine 的独占可变访问与 ConcurrentEngine 的同一个 writer mutex：从取得最新 committed root、构建候选、验证全部引用，到 durable commit 和发布新 root，必须保持同一次 writer 所有权。不得在读快照上验证后释放锁，再把旧候选直接提交；引用检查不依赖仅覆盖变化行的 expected-state write set 来证明被读取目标仍存在。若未来引入乐观候选，提交前必须核对其 committed generation 和引用依赖，过期候选明确失败或在最新状态上重新完整验证，不能把结果不确定的提交当作可自动重试。

受控并发验收必须用 barrier 覆盖两个顺序：child 先提交，则删除 parent 返回 restrict；parent 先提交，则插入 child 返回 missing target。两者不能同时成功并留下孤儿。持锁期间取消/失败应按现有原子回滚规则释放候选，已经提交的其他请求不受影响；memory 与 redb、直接 API 与共享服务入口均覆盖。

upsert 的完整替换必须同时检查出向引用和被替换 target key 的入向引用。仍存在另一个合法唯一目标时没有孤儿；不能把物理 RowId 变化误当作关系身份变化。重复幂等 key replay 不重放 DML，保持原 receipt 契约。

### Catalog、性能与迁移

ReferenceDefinition 使用统一 stable ID、源/目标 table ID、stable field-ID paths 和目标 key identity。rename 更新显示名称而不改变身份；源/目标字段类型改变、删除表、删除被依赖 unique index 前必须显式移除/重建引用。一次 migration 以完整候选 catalog/rows 做结构和全量引用校验，然后原子发布。

普通 mutation 应从 write set 验证新源 key，并从被移除/改变的目标 key 查反向 posting，避免每次扫描全部数据库。反向引用索引是受约束的派生结构，拥有明确 stable key 编码、增量维护和 check 验证；不是不可验证的进程缓存。Schema 构建、migration、restore 可以全量重建。性能验收必须证实少量写入不退回全库 full rebuild。

持久化实现分配 storage 12/13（journal 关闭/开启）、catalog codec 7 与 logical backup 7；value/index-key/receipt/maintenance codec 保持 3/4/3/1。显式升级分别为 10→12、11→13，不把普通写入当作隐式升级。反向 posting 复用 codec 4 的 typed tuple 编码，以全局唯一的 reference stable ID 区分普通 index ID，尾部保存源行 RowId；catalog 的 Reference entry 决定其语义。旧软件必须因未知 storage/catalog 版本拒绝打开，不能利用 serde default 静默忽略约束。开发阶段默认新库仍为 format 10，待完整验收后统一切换。

logical backup、journal delta、incremental chain replay、open 和完整 check 共用引用校验。损坏或存在孤儿的恢复候选不得发布为可使用数据库；失败保留既有目标。部分恢复不能静默去掉 reference。

### 可观察性与验收

违反引用返回 E_CONSTRAINT，区分 missing target 和 restricted target change 的 value-free constraint kind/hint。CLI、Rust、TCP/HTTP 保持一致；无业务 key/value 日志。schema describe/portable contract、introspection、schema source/diff 显示完整定义；explain 对 mutation 展示需检查的约束，不执行 mutation 或泄漏 key，不把 reference 自动变成 join 优化。

交付分阶段：定稿语法/Option/检查时点；parser/catalog/binder 与基础内存写入；storage/upgrade/check/backup；migration/diff/portable/macros；真实业务与跨版本发布验收。#401 保持打开直到全部完成。

验收覆盖：非存在 parent 插入拒绝；受引用 parent 删除/改 key 拒绝；嵌套字段与复合 unique/ADT key；批量自引用和整脚本回滚；upsert/returning/prepared；添加引用扫描存量孤儿与 migration 原子失败；rename/drop/type conversion；重启/backup/增量恢复；损坏反向 posting；旧格式显式升级与未知版本拒绝；受控并发写入、receipt replay 和 snapshots；fmt 幂等、schema drift、Rust macro、docs/agent 可发现性。普通 CI Ubuntu，双平台发布检查仅发版运行。

## English Description

This contract targets #401 in v0.14 and is implemented on main pending acceptance. It defines explicit, typed source-path references to a target primary key or unconditional unique index, with restrict semantics and no cascade/set-none. Composite references use the same ordered path lists as indexes; stable IDs preserve identity across renames. Target binding prefers a matching primary key, otherwise the lowest stable-ID unconditional unique index over those exact ordered paths, irrespective of sort direction. Persist that choice; dropping the bound key requires explicitly dropping/recreating the reference even if an equivalent unique index remains. This prevents implicit dependency changes. DDL is `create reference lines (order_id) references orders (id)`, with matching migration add/drop and canonical formatting.

Binding resolves each component statically: identical source/target types use Exact mode, including Option equality where None needs a target None. Otherwise Option<T> may reference T in Optional mode: None establishes no relationship; Some unwraps exactly one layer. All other type pairs fail. Preserve nominal identity and decimal shape. For a composite reference, any absent Optional component skips that relationship, while an Exact None does not. Expose the bound mode in schema/portable metadata and require explicit rebuilding when type evolution changes it.

For example, an unassigned task with Option<UserId> needs no user; assigning Some(id) requires a user and subsequently restricts that user's deletion. In a tenant/optional-assignee key, an absent assignee does not independently validate tenant existence: declare a separate tenant reference if required. Matching Option keys on both sides keep ordinary typed None equality. Nested Option unwraps at most one layer; Some(None) is not an absent relationship. This proposal covers optional application relationships without adding conditional-source predicates. Partial unique indexes still cannot act as unconditional target keys.

Validate the final candidate of each complete DML statement, including an entire batch, before publishing it. This supports order-independent batch self-references while requiring parent-before-child insertion and child-before-parent deletion across separate statements. Any failing statement rolls back the whole request. Upsert validates both outgoing relationships and incoming dependencies on replaced keys. Receipt replay preserves the existing exactly-once-effect contract. Keep the same exclusive writer ownership from acquiring the latest committed root through candidate construction, reference validation, durable commit and root publication. A stale read snapshot must not authorize a later write, and changed-row expected-state checks alone do not protect referenced targets. If optimistic candidates are introduced later, stale generation/reference dependencies must fail or be fully revalidated against current committed state; this is not permission to retry uncertain commits. Controlled barriers must cover both orders: child commits first and parent deletion is restricted, or parent deletion commits first and child insertion reports missing target. Both cannot succeed leaving an orphan. Cover memory/redb and direct/shared service APIs.

Ordinary writes should use write-set changes and reverse postings rather than scan or rebuild all data. Full catalog changes and recovery may rebuild and validate. Persistent definitions, reverse indexes, storage/backup codecs and journal replay require an explicit versioned design, upgrade path and corruption checks; old software must never silently omit constraints. Restore must reject orphaned candidates without replacing a valid destination.

Expose value-free missing-target/restricted-change constraint kinds through E_CONSTRAINT, all adapters, introspection, portable schema, source/diff and mutation explain. Do not execute writes during explain or infer join rewrites from references. Deliver syntax and semantics first, then memory enforcement, durable compatibility, migrations/tooling, and end-to-end acceptance. Keep #401 open until all phases pass, including nested/composite/ADT keys, batches, self-references, atomic failures, prepared writes, migrations, restart, logical/incremental restore, corruption, old formats, concurrency, replay, formatter and Rust macro coverage. Release guidance belongs in Discussions only.

## 实现定位 / Implementation map

Inspected against v0.13 release commit f730114cc66bfe0e7a962e961351c2ebab594150:

- `src/query.rs::Statement`: add first-class reference DDL, with formatter/parser/prepare coverage; do not encode it as an index predicate.
- `src/db.rs::IndexDefinition` and `Database`: stable-ID reference definitions, target-key binding and reverse postings. Existing whole-table uniqueness validation is not a substitute for cross-table existence checks.
- `src/db/migration.rs::apply_schema_migration`: validate references against the completed candidate catalog and rows, following transformation and index reconstruction, before publishing the migration.
- `src/engine.rs`: preserve candidate isolation, keyed receipt atomicity and `LogicalWriteSet` incremental commits. Reference checks must be part of ordinary DML execution so local Database and prepared paths cannot bypass them, not just a CLI wrapper.
- `src/error.rs::ConstraintKind`: add precise value-free kinds and actionable hints; update the agent error vocabulary and all serialization tests together.
- `src/portable.rs`: description version 3 carries source-owned references with decimal-string IDs, ordered source/target paths, pinned primary/unique key, Exact/Optional mode and restrict actions. Versions 1/2 remain readable only without reference metadata. Catalog-backed validation rejects stripped or altered reference metadata; evolution reports flag new restrictions and removed existence guarantees.

These are implementation entry points, not claims that the feature exists. Before changing persistent encoding, inventory catalog, redb reverse keys, journal, backup, upgrade and schema-identity consumers and assign compatible versions as one reviewed contract.

### 持久化契约核对表 / Durable contract checklist

The v0.13 baseline is storage 10/11 (journal disabled/enabled), catalog 6, value 3, index-key 4, receipt 3, maintenance 1, journal 0/1, logical backup 6, portable schema 2. The implementation allocates storage 12/13, catalog 7 and logical backup 7, retaining value/index-key/receipt/maintenance codecs 3/4/3/1. Explicit upgrades are 10→12 and 11→13. Reverse postings reuse codec 4 typed tuples under globally unique reference stable IDs, with source RowId suffixes; Reference catalog entries distinguish them from ordinary index IDs. Unknown storage/catalog versions must reject older readers rather than silently omit constraints. The development default remains format 10 pending complete acceptance.

| Surface | Required change and acceptance |
| --- | --- |
| `DurableCatalogEntry` / redb catalog | Persist stable reference definitions and bound component modes; decoding rejects malformed IDs, invalid target uniqueness and incompatible types before writes are enabled. |
| Reverse postings | Define a versioned key namespace distinct from user indexes; include reference ID, typed target key and source RowId. Check missing, extra, stale, wrong-table and wrong-reference entries. |
| `PreparedDelta` / `LogicalWriteSet` | Fold before/after effects for source and target writes into one expected-state transaction; net-zero effects leave no orphan postings. |
| `Database::validate_logical_backup` | Bind every reference, check every source row against final targets and rebuild derived structures. Check checksum success alone is insufficient. |
| `backup::read_database` | Reject reference metadata carried under an older declared backup version, mirroring existing map/partial-index gates. Verify restored schema identity includes references. |
| Incremental archive / journal | Replay catalog and row changes at complete-commit boundaries; never validate a half-applied record frame as if it were a committed database. Preserve receipts and references together across checkpoint and replay. |
| Shadow migration / maintenance | Build reference postings for candidate generation; atomic cutover publishes matching rows/catalog/postings. Resume, abort and reclaim handle the additional namespace. |
| Open / bounded integrity check | Ordinary open validates catalog bindings and opens physical tables without scanning rows/postings, preserving bounded-open behavior. Explicit check verifies every reference using bounded iterators and current cancellation/deadline checks. Migration Ready propagates request control through both the integrity and generation-summary scans, polling between catalog/row/index entries and reference components and before Ready commit. Cancellation/deadline/shutdown leaves Building unpublished and resumable. Restore retains full candidate validation; lazy read cache must not make those checks incomplete. |
| Upgrade / compact | Explicit upgrade validates old logical state and constructs the new namespace atomically; native compact proves identity preservation including reference definitions/postings. |
| Portable schema / macros | Bump description version, retain old reads, preserve reference metadata in macro schema binding and schema hashes; code-generated row ADTs remain ordinary Rust types. |
| Release contract / doctor | Advertise actual readable versions and codec tuple; provide old-format rejection/upgrade evidence with real prior-version binary. |

This inventory does not justify unrelated protocol, value or receipt bumps: change only encodings whose semantics actually change, and test old readers reject new durable format rather than silently drop constraints.

### 实现边界与验收证据 / Implementation boundaries and evidence

当前反向 posting 复用普通索引的物理表、typed key codec、range cursor、增量事务和 generation 生命周期，但没有把引用声明转换成用户可见的普通索引。Optional 模式需要跳过 None，并按目标类型编码 Some 的内层值；Exact 模式则保留完整 Option 值。当前普通索引 API 使用源字段完整类型，复用需要增加投影或查询边界适配。内存只维护 reference source root；目标存在性复用已绑定的主键或 unique index。内存与 bounded redb 写入共用 final-batch 校验：先收集整个批次的新目标与变更 RowId，再查询原索引并排除被替换行，因此支持批内自引用和同时改 key，不需要每条引用复制 target root。两者的语义仍需独立绑定。完整 check 与 migration Ready 现通过 PostingDefinition 投影和同一行→posting、posting→行遍历检查缺失、额外、错值、缺行与总数，引用额外检查目标存在，唯一性只应用于普通 unique/primary 索引。#438 后续继续收敛维护/重建接口并评估已有源索引复用，不改变 stable ID、用户索引目录或引用语义。这是当前实现的取舍，不代表重复校验代码无需改进。

复用源索引的替代方案仍需维护受约束的依赖（不得删除该索引后绕过 restrict），为 Optional 增加投影，以及保存目标 key identity；若已存在匹配的源索引，复用还可以省去额外 posting；若没有，则自动创建的源索引仍需逐行维护。Optional 用完整 Option 索引时，None 也占 posting，查询需忽略它；使用投影索引则可省去 None posting。两种方案都需约束 catalog 元数据。logical backup 只保存声明和逻辑行，两种方案都在恢复时重建 posting。旧 reader 不理解约束时必须拒绝写入，所以仅复用索引表不足以取消 catalog/storage 版本边界。#438 的成本比较应围绕共同接口和重复代码，而不是声称完全没有新的持久约束语义。

显式升级到 storage 12/13 后，logical backup 使用 format 7，即使尚未声明引用或已经删除最后一个引用；这与现有 map/partial-index 备份按存储能力选版本的规则一致。旧软件不能读取 format 7。逻辑恢复保留 schema identity、rows 和 receipts，不承诺保留未使用的物理能力：仅当备份实际包含引用时自动升级目标到 format 12，否则当前新目标仍使用 format 10。`tests/reference_storage.rs` 覆盖未声明引用和删除最后一个引用后的备份/恢复边界。

`tests/reference_concurrency.rs` 在 memory 与 redb 中用 channel barrier 持有真实 writer 所有权，确认第二个 protocol writer 已排队，再允许首个 mutation 提交。分别覆盖 source insert 先于 target delete/change-key，以及反向顺序；后者必须按最新提交拒绝缺失目标。redb 还比较重开后的完整行、schema identity 并运行完整 check。这证明这些受控写入路径的顺序语义，不替代尚未完成的全部恢复、取消、receipt 和 adapter 验收。

Reverse postings currently reuse the ordinary index physical table, typed key codec, range cursor, incremental transaction and generation lifecycle. They are not exposed as ordinary user indexes: Optional mode skips None and projects Some to its target type, while Exact retains the full Option value. The current ordinary-index API encodes the full source field type; reuse needs projection or query-boundary adaptation. Memory keeps only the reference source root and reuses the bound primary/unique index for target existence. Memory and bounded redb writes share final-batch validation: collect new targets and changed RowIds across the entire batch, then query the original index excluding replaced rows. This preserves batch self-references and simultaneous key changes without a per-reference target root. Binding semantics remain distinct. Full check and migration Ready now share PostingDefinition projection and the same row-to-posting/posting-to-row traversals for missing/extra/mismatched postings, missing rows and cardinality. References additionally check target existence; uniqueness applies only to ordinary unique/primary indexes. #438 still covers maintenance/rebuild interface consolidation and existing-source-index reuse without changing stable IDs, user index catalogs or reference semantics. This explains the current tradeoff, not an exemption from reducing duplicated checks.

Reusing a source index still requires a protected dependency (dropping it must not bypass restrict), Optional projection and persisted target-key identity. An existing matching source index can eliminate additional postings as well as duplicated maintenance/check/migration code; otherwise an automatically created index still needs per-row maintenance. A full Option index retains None postings that lookups must ignore; a projected index can omit them. Both require constraint catalog metadata. Logical backups store declarations and rows; both designs rebuild postings during restore. Older readers must reject writes when they cannot enforce constraints, so table reuse alone cannot remove the catalog/storage compatibility boundary. The cost comparison in #438 should focus on shared interfaces and duplication rather than claiming no new durable constraint semantics.

An explicit storage upgrade to 12/13 selects logical backup format 7 even before declaring references or after dropping the final reference, following the existing storage-capability rule for map/partial-index backups. Older software cannot read format 7. Logical restore preserves schema identity, rows and receipts, but not unused physical capabilities: actual reference metadata upgrades the destination to format 12; otherwise the current fresh destination stays at format 10. The storage regression covers both cases.

The concurrency regression holds real writer ownership behind a channel barrier, observes a queued protocol writer, then permits the first mutation to commit. Both memory and redb cover source insert before target delete/key change and the reverse order; the second writer must validate the latest commit. Redb additionally verifies full rows/schema identity and integrity after reopen. This establishes those controlled write paths, not complete recovery, cancellation, receipt or adapter acceptance.

### #438 成本比较与收敛顺序 / Cost comparison and convergence order

下表比较实现结构，不是性能测量。复用目标索引已落实；复用源索引仍是后续评估，不宣称 #438 全部完成。

| 路径 / Path | 独立反向 posting / Dedicated reverse posting | 复用源普通索引 / Reused ordinary source index |
| --- | --- | --- |
| 普通 source 写入 / Source writes | 按 reference 投影一次并维护 source posting；多个相同 source shape 的声明可能重复维护 / Project and maintain per reference; matching source shapes can duplicate work | 已有匹配索引时可省 posting，否则仍须建立并维护索引；Optional 需投影或边界适配 / Save postings when a matching index exists; otherwise maintain an index, with Optional projection or boundary adaptation |
| 目标存在性 / Target existence | 当前复用绑定的目标主键/unique index；无需每条引用的 target root / Reuse the pinned primary/unique target index; no per-reference target root | 同样复用目标索引，不能仅靠 source 索引证明存在 / Same target lookup; a source index cannot prove existence |
| 删除或改目标 key / Restrict | 用 reference ID 范围查 source posting，排除当前批次替换行 / Scan by reference ID, excluding replaced batch rows | 用 source index ID 与适配后的 key 范围查找；仍须排除替换行 / Scan by source index ID and adapted key bounds, still excluding replaced rows |
| DDL 与身份 / DDL and identity | reference 自有 stable ID，不改变用户索引生命周期 / Independent stable reference identity and user-index lifecycle | 必须保存、保护索引依赖；自动创建索引还需所有权及 drop 规则 / Persist and protect dependencies; automatic indexes also need ownership/drop rules |
| check 与迁移 / Check and migration | 已复用 PostingDefinition 的双向完整性遍历与 generation 生命周期，额外校验目标存在 / Shared bidirectional PostingDefinition checks and generation lifecycle, plus target existence | 可复用同一遍历；投影、依赖及 target-existence 校验仍需保留 / Same traversal; projection, dependencies and target-existence checks remain |
| backup 与格式 / Backup and formats | 逻辑备份只存约束声明与行，恢复重建；旧 reader 必须拒绝未知约束 / Store declarations and rows, rebuild on restore; old readers must reject unknown constraints | 仍需保存约束声明和升级边界；省 posting 不等于省约束格式 / Still persist constraints and an upgrade boundary; fewer postings do not eliminate constraint metadata |

当前保留嵌套路径、ADT/复合 key、Optional 的真实业务链路，由 `tests/reference_recovery_journey.rs` 和 `query-macro/tests/references.rs` 验证。优先去掉重复结构与校验，不为了缩短代码改变这些业务语义。下一步源索引复用应先列出匹配条件（路径顺序、方向、完整类型、无 predicate）、Optional 投影、用户 drop 依赖和迁移重绑定，并比较无已有索引/已有索引两种情况。只有测量和复杂度证据支持时再改持久 posting，格式模型的独立收敛由 #439 跟踪。

The table compares implementation structure, not measured performance. Target-index reuse is implemented; source-index reuse remains an evaluation, so #438 is not complete. Preserve the nested-path, composite/ADT-key and Optional application journeys in the recovery and macro tests. Remove duplicated structures and validation before changing business semantics. A source-index proposal must specify matching paths/order/direction/full types and absence of predicates, Optional adaptation, protected drop dependencies and migration rebinding, comparing both existing-index and no-index cases. Change durable postings only when measurement and complexity evidence justify it; #439 separately tracks the format model.


### #438 持久源 provider 候选边界 / Candidate durable source-provider boundaries

本节是待验证的设计，不改变当前 provider、存储版本、能力清单或新库默认值。[内存写原型结果](https://github.com/worktools/unionid/issues/438#issuecomment-5974406383) 在 10k source / 1k target、debug、每组 10 次的范围内，将匹配索引情况下 dedicated posting 从 10k 降到 0；insert/rebuild 的 p50 分别约下降 23%。无匹配索引仍走 dedicated。它验证了共享和原子 fallback，但不证明完整 Engine、redb、journal 或恢复成本。不能据此完成 #438 或发布持久实现。

候选方案只支持两种派生 backing：`Dedicated(reference_id)` 和 `ExistingIndex(index_id)`。不自动创建隐藏用户索引，不引入第三种投影索引或独立 provider maintenance 状态机。最初只有全部 Exact、稳定 field path/完整类型/组件顺序相同、全 ascending、无 predicate 的索引可以复用；Optional、任一降序或部分索引继续使用 dedicated。Exact Option 的 None 仍是普通值。多个引用可借用同一源索引，但约束声明、target binding 和 stable reference ID 各自保留。

候选实现需要同时满足以下边界：

1. **绑定和快照**：在 catalog/generation 建立时解析一次 source backing，固定 stable index ID；不要在每次 restrict 或普通 DML 时遍历索引目录。已有 backing 不因新增另一个等价索引而切换。引用仍以自身 ID 进入 schema/portable/error 身份，backing 是物理派生元数据，不作为用户可编辑声明。恢复可以重新选择物理 backing，但必须在写入任何行前确定整个目标 generation 的方案；重开不能自行重选。若同一声明还可选择多个索引，首次选择必须有确定的 stable-ID 顺序。
2. **写入和查找**：`TypedRowSource::reference_source_exists` 仍以 reference ID 表达约束；row source 使用绑定 backing 解析 posting ID、component count 和范围编码。source delta 对 ExistingIndex 只由普通 index 路径维护一次，对 Dedicated 保持现有投影；两者仍调用现有 final-batch target validator 并排除全部被替换 RowId。不能把缺失 backing 当作空 posting：catalog 或物理索引不一致必须报存储错误。restrict 仍受 deadline/cancel 约束。
3. **drop、共享和迁移**：删除被借用索引必须在同一原子 DDL 候选或现有 shadow generation 中为每个仍存活的引用建立完整 dedicated backing，再发布新的 catalog/rows/postings；失败保留旧 generation 和 backing。普通写入不能穿过半建 fallback。删除一个引用不能删除其他引用借用的普通索引。类型转换、组件变更、索引谓词变更和 drop/re-add 均重新验证整个目标 generation；沿用 Building/Ready/cutover/reclaim，不另添状态机。source backing 切换不改变已固定的 target key 依赖规则。
4. **check、恢复和增量链**：完整 check 先验证每个 backing 的身份与匹配条件，再检查普通索引物理一致性和每条引用的目标存在性。ExistingIndex 不应再要求 reference-ID posting，也不能把多条引用视为重复物理 posting。所有写集、Ready 检查、恢复重建、journal replay 和 native compact 都必须使用同一 backing 规则。逻辑备份保留声明与行；若保存物理 backing 提示，恢复必须验证而非信任它。增量链必须携带可重建且与生成 posting 一致的 backing 元数据；单靠恢复 schema/ledger/sequence 不够，需逐行、restrict 和 posting 完整性验证。
5. **兼容边界**：不得在 legacy 12/13 或现有 native `typed_references` profile 中悄悄省掉 reference-ID posting；这些 reader 将查不到 source，可能放过 restrict。持久候选仅能在显式离线转换后启用，且 native header 必须包含旧 reader 不认识的 required capability，并使用与 provider 元数据一致的 catalog codec。具体 capability 名称和 codec 数值在完整设计评审前不分配。未知 reader 必须在提供读写服务前拒绝，不能靠 serde 默认值或只在写入时拦截。转换、backup、journal replay 和恢复均需同一能力边界；默认 format 10 及现有 profile 保持不变。

隔离 native 原型已有[同二进制成本比较](https://github.com/worktools/unionid/issues/438#issuecomment-5974845564)、[固定 backing 与恢复验证](https://github.com/worktools/unionid/issues/438#issuecomment-5975124435)和[离线转换提交前/后退出证据](https://github.com/worktools/unionid/issues/438#issuecomment-5975184427)。debug、10k source / 1k target、四组各 10 样本中，匹配索引的总 posting 从 31k 降至 21k，100-row insert 从 300 降至 200 index changes；insert/rebuild p50 分别约改善 22.2%/20.5%。这些成本来自 lookup-time 原型，不能当作随后 pinned 版本的测量；全进程 peak RSS 不能证明单组内存收益，文件大小也受 redb 分配和高水位影响。正式旧二进制、完整 update 成本与复杂 ADT/中断组合、生产 metadata 和净维护成本仍待验证；不据此完成 #438 或发布复用。

This is a candidate design, not an implemented provider, capability allocation, version change or default switch. The linked debug-memory experiment removes 10k dedicated entries when an eligible ordinary index exists and improves median insert/rebuild by about 23%; unmatched cases retain dedicated postings. It does not measure complete Engine/redb/journal/recovery costs.

Use only two derived backing choices: Dedicated(reference ID) and ExistingIndex(index ID). Do not create hidden user indexes, a third projected-index form or another maintenance state machine. Initial reuse requires all-Exact components, identical stable paths/full types/ordered components, ascending direction and no predicate. Optional, descending and partial-index cases retain dedicated backing; Exact None remains an ordinary typed value. Multiple references may borrow one ordinary index while retaining independent constraint and target identities.

Resolve and pin backing once per catalog/generation; preserve existing bindings when another equivalent index is added and use deterministic stable-ID selection for initial binding. Reopen must not silently choose a different backing. Keep reference IDs at the row-source API boundary, resolve physical index identity through the bound metadata, maintain borrowed postings only through ordinary index deltas, and retain final-batch validation/excluded RowIds/cancellation. Missing backing is a storage error, not an empty range.

Dropping a borrowed index builds complete dedicated fallbacks atomically in the DDL candidate or existing shadow generation before publication. Failure preserves the old generation; removing a borrower cannot delete the user index. Revalidate backing during schema/type/index changes using existing Building/Ready/cutover/reclaim machinery and preserve target-key dependency semantics. Check backing identity/eligibility before ordinary posting validation and per-reference target existence; do not require duplicate reference-ID postings for borrowed indexes. Delta construction, Ready validation, rebuild, journal replay, restore and compact must agree on the same provider mapping. Logical backups preserve declarations/rows and validate any physical hints; incremental archives require sufficient consistent metadata to rebuild postings and must verify complete rows and restrict behavior, not just schema/ledger/sequence.

Never silently remove reference-ID postings in legacy 12/13 or the existing native typed_references profile. Old readers could otherwise miss sources and bypass restrict. Enable durable reuse only through explicit offline conversion with an unknown-to-old-readers required capability and a matching catalog codec. Allocate neither the capability name nor codec number before full design review. Older readers must reject before serving reads or writes; serde defaults and write-only checks are insufficient. Apply the boundary consistently to conversion, backup, journal replay and restore without changing format 10 defaults or existing profiles.

The isolated native experiment now has [same-binary cost evidence](https://github.com/worktools/unionid/issues/438#issuecomment-5974845564), [pinned backing and restore checks](https://github.com/worktools/unionid/issues/438#issuecomment-5975124435), and [offline conversion with pre/post-commit exit checks](https://github.com/worktools/unionid/issues/438#issuecomment-5975184427). Four debug cases, each with ten samples over 10k sources/1k targets, reduce matching-index postings from 31k to 21k and 100-row insert index changes from 300 to 200; median insert/rebuild improve about 22.2%/20.5%. Those timings measure the lookup-time experiment, not the later pinned variant. Whole-process peak RSS does not establish per-case savings, and file sizes reflect redb allocation/high-water effects. Released-old-binary checks, complete update costs, complex ADT/interruption combinations, production metadata and net maintenance cost remain gates; this does not complete #438 or ship reuse.


### #438 物理 metadata 与离线转换 / Physical metadata and offline conversion

以下是待评审的生产边界，不是新增 CLI/API 或已发布的升级步骤。实验源码与 raw samples 保留本地，正式 capability 名称与 codec 数值仍未分配。

1. **分开两种身份**：reference 声明、stable ID、target binding 和 Exact/Optional 模式继续参与 schema hash、portable contract 和业务 migration。source backing 属于物理 catalog metadata，必须与语义 manifest 分开编码；仅转换 posting 归属不改变 schema revision/hash。不能简单给当前共享 serde representation 加字段，否则会意外改变 schema hash 或把物理选择暴露为逻辑契约。
2. **持久固定与 fail closed**：新的 profile 为每条引用显式保存 Dedicated 或固定 ExistingIndex ID。重开验证 metadata 完整性与索引 eligibility，缺失或无效值返回存储错误，不通过默认值补齐或重新选择。晚加源索引不转换现有 Dedicated；同名 drop/re-add 的新 ID 不能替代旧依赖。逻辑恢复可在完整目标重建前选择 backing；增量恢复必须保留产生其 postings 的映射。
3. **沿用提交协议**：显式转换先验证原库，再生成完整候选、核对 stable table/RowId、allocator、cursor identity、ledger、回执与 schema。固定旧/新 layout、capability、codec 和 header，在同一同步 two-phase transaction 中比较准确旧状态并写入新 metadata/postings。成功仅推进一次 commit sequence；已有回执及其原始 response 不重写。提交前失败回滚，提交结果不确定要求重开/check。实验的 thread-local mode 与 expected-header oracle 不能成为生产模型。
4. **首个有界链策略**：active journal 转换明确拒绝。维护期间停止业务写入，先 export/verify 所有 retained commits，再 disable 旧链，保留 sealed archive；转换成功并 check 后，以新目录 init/verify 新 baseline，确认恢复可用后再开放写入。不得丢弃未导出 tail、改写旧 manifest 或将两个目录宣称为一条连续链。旧链只覆盖停用前的 sequence，新链从转换后的 baseline 开始；新 baseline 完成前存在备份覆盖窗口，源库的同步提交与重开恢复仍独立有效。任一步失败按当前 durable 状态继续，不重新执行已完成转换。
5. **撤回和范围**：未知 reader 在提供服务前拒绝新 profile。撤回需要从已验证的旧 archive/逻辑备份恢复到另一条路径，不承诺原地降级；转换后新增写入不会自动出现在旧链。旧 12/13 先沿既有显式升级进入 native，再讨论 provider 转换。当前不实现 active-chain 内跨 profile 的连续 journal transition；若后续需要，另行验证其旧/新 header、catalog delta、checkpoint 与 replay，不增加新的维护状态机。

[本轮链切换隔离验证](https://github.com/worktools/unionid/issues/438#issuecomment-5975236321)覆盖 export/verify → disable → convert → fresh init 路径：未导出 tail 会阻止停用；旧 sealed manifest 保持不变，旧恢复点仍有 Dedicated；新 baseline/后续 commit 恢复保留 pinned backing、逐行数据、ledger、原始幂等回执和 restrict。它没有证明正式 released reader、全部业务组合或可发布实现。#438 与 #401 的生产和发布验收仍开放。

The following are proposed production boundaries, not new CLI/API functionality or released upgrade instructions. Experimental source/raw samples remain local; official capability names and codec numbers are unallocated.

1. **Separate identities:** reference declarations, stable IDs, target binding and Exact/Optional modes remain part of schema identity, portable contracts and business migrations. Encode source backing as physical catalog metadata separately from the semantic manifest. Posting-ownership conversion must not change schema revision/hash. Adding a field to the currently shared serde representation without separating these uses could change hashes or expose physical choices as logical contracts.
2. **Pin and fail closed:** persist an explicit Dedicated or ExistingIndex ID for every reference in the new profile. Reopen validates complete metadata and eligibility; missing/invalid values are storage errors, never defaults or reselection. Later indexes do not convert Dedicated backing; re-created shapes with new IDs do not satisfy old dependencies. Logical restore may choose backing before rebuilding the complete destination; incremental replay retains the mapping that produced its postings.
3. **Reuse commit semantics:** check the original database, prepare the complete candidate and compare stable table/RowId identities, allocators, cursor identity, ledger, receipts and schema. Represent exact old/new layout/capability/codec/header identities and use the existing synchronous two-phase transaction to compare old state and publish metadata/postings atomically. Advance one commit sequence on success without rewriting saved responses. Definite failures roll back; uncertain outcomes require reopen/check. Experimental thread-local modes/header oracles are not a production model.
4. **Initial bounded archive policy:** refuse conversion with an active journal. Stop application writes during maintenance, export/verify the complete retained tail, disable and retain the sealed archive, convert/check, then initialize and verify a fresh baseline in another directory before resuming writes. Do not discard unexported history, rewrite the old manifest or claim both directories form one continuous chain. The old chain ends before conversion; the new one starts at the converted baseline. Until that baseline completes there is a backup-coverage window, distinct from synchronous source-database durability/reopen recovery. Resume from the actual durable state after failures without repeating a completed conversion.
5. **Rollback and scope:** unaware readers reject the new profile before serving requests. Rollback restores a verified old archive/logical backup to another path, not an in-place downgrade; later writes are absent from the sealed old chain. Legacy 12/13 first use the existing explicit native upgrade. Continuous active-chain profile conversion is out of this initial scope; any later support must verify both headers, catalog deltas, checkpoints and replay while retaining the existing maintenance machinery.

The [isolated archive-switch journey](https://github.com/worktools/unionid/issues/438#issuecomment-5975236321) verifies export/verify → disable → convert → fresh init, including unexported-tail refusal, an unchanged sealed manifest, old Dedicated restore points, and new baseline/subsequent-commit restores with pinned backing, complete rows, ledger, original receipts and restrict. It does not establish released-reader compatibility, every application combination or a shippable implementation. Production/release acceptance in #438 and #401 remains open.
