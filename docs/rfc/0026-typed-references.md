# RFC 0026：类型化引用与 restrict / Typed references and restrict

- 状态 / Status: proposed; not implemented or available in v0.13
- Milestone: v0.14.0
- Tracking: [#401](https://github.com/worktools/unionid/issues/401)

## 中文说明

### 目标与声明

在数据库内保证业务关联的存在性，保留 ADT 完整类型和值相等语义。源字段路径引用目标表主键或全表 unique index；不提供 cascade 或 set-none。草案语法沿用 index 的表与路径列表结构，不增加分号：

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

This working draft targets #401 in v0.14. It proposes explicit, typed source-path references to a target primary key or unconditional unique index, with restrict semantics and no cascade/set-none. Composite references use the same ordered path lists as indexes; stable IDs preserve identity across renames. Target binding prefers a matching primary key, otherwise the lowest stable-ID unconditional unique index over those exact ordered paths, irrespective of sort direction. Persist that choice; dropping the bound key requires explicitly dropping/recreating the reference even if an equivalent unique index remains. This prevents implicit dependency changes. Proposed DDL is `create reference lines (order_id) references orders (id)`, with matching migration add/drop and canonical formatting.

Proposed binding resolves each component statically: identical source/target types use Exact mode, including Option equality where None needs a target None. Otherwise Option<T> may reference T in Optional mode: None establishes no relationship; Some unwraps exactly one layer. All other type pairs fail. Preserve nominal identity and decimal shape. For a composite reference, any absent Optional component skips that relationship, while an Exact None does not. Expose the bound mode in schema/portable metadata and require explicit rebuilding when type evolution changes it.

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
