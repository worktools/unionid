# RFC 0008：增量候选状态与原子发布

- 状态：proposed
- 日期：2026-09-08
- 跟踪：[M6 #167](https://github.com/worktools/unionid/issues/167)、[设计 #162](https://github.com/worktools/unionid/issues/162)、[实现 #163](https://github.com/worktools/unionid/issues/163)

## 中文说明

### 1. 问题与目标

当前 Engine 在发现一个脚本包含 mutation 后深复制完整 `Database`，所有语句在副本上执行，成功后再发布副本。redb backend 随后把候选数据库的 catalog、全部 row、全部 secondary-index entry、migration ledger 和 receipts 重新编码为 `PreparedState`，与上次完整编码状态做差，再提交变化的稳定键。

这个模型清楚地保证了请求级原子性，但一次只修改一行的请求仍承担两次与完整数据库相关的工作：

1. `Database::clone` 复制所有 table rows、嵌套 ADT values 和 in-memory index postings；
2. `PreparedState::new` 遍历并编码全部持久对象，再通过全量 map/set 比较恢复真正的 delta。

现有 workload 已观察到该成本：10k 行单行 update/upsert 的 p95 约 94 ms；100k 行约 0.88 s，峰值 RSS 约 1.25 GiB。M6 不把 unionid 改造成多写者或海量数据库；本 RFC 的目标是让单行和小批量 row-only DML 的候选构建与持久准备由实际 write set 决定，同时完整保留：

- 一个脚本只发布一个旧状态或一个新状态；
- 读快照只观察一个完整 commit；
- 主键、unique index、ADT 类型、RowId 和 returning 语义；
- 数据效果与 idempotency receipt 同事务；
- commit 前确定失败与 commit 结果不确定的现有区别；
- 当前 redb bytes、backup、schema identity、cursor 与 wire 协议。

### 2. 决策摘要

采用三个相互配合的边界：

1. **单一 committed root**：Engine 以一个 `Arc<CommittedState>` 保存 database 与 receipts。read snapshot 只克隆这个 root；成功写入在 writer lock 内一次替换 root。
2. **path-copying persistent state**：row、index posting 和 receipt 使用按路径复制的持久有序 map/set。候选状态共享全部未修改节点；修改一个 key 只建立从 root 到该 key 的新路径。catalog、schema 和 migration history 在 row-only 路径中保持共享。
3. **显式 write set**：DML 在修改候选状态时同时记录并合并 row、table watermark、index 和 receipt 的逻辑变化。redb 把这个 write set 直接准备为带 expected-old-value 的 stable-key delta，不再从两个完整编码状态反推差异。

普通 row-only DML 使用增量路径。包含 DDL、schema migration、storage upgrade、restore、完整性重建或旧格式转换的操作继续使用明确标记的 full-rebuild 路径；这些操作本来就需要遍历受影响 schema/data，不伪装成小写集。

### 3. 所有权模型

逻辑结构如下；名称用于说明契约，不要求公开为稳定 Rust API：

```text
Engine
  committed Arc<CommittedState>
  durable option DurableBackend
  writer-only failure/checkpoint state

CommittedState
  database Database
  receipts PersistentMap<IdempotencyKey, Arc<Receipt>>

Database
  catalog Arc<CatalogState>
  tables Map<TableName, Arc<TableState>>
  indexes Map<IndexId, Arc<IndexState>>
  migration_history Arc<[MigrationEntry]>
  sequence / schema identity / cursor identity

TableState
  definition Arc<TableDefinition>
  rows PersistentMap<RowId, Arc<Row>>
  next_row_id

IndexState
  definition Arc<IndexDefinition>
  postings PersistentMap<TypedEqualityKey, PersistentSet<RowId>>
```

`PersistentMap`／`PersistentSet` 必须提供确定顺序、不可变 root 的 O(1) clone、O(log n) path-copy update 和旧 root 稳定迭代。实现可以使用内部 path-copying tree abstraction，但不能用 `Arc::make_mut(BTreeMap)` 复制全部 entries 后声称为增量。它也不能用不断增长的 overlay chain 把写入成本转移到每次读取。

顶层 table/index name map 和小型元数据允许按 schema object 数量复制；row-only 路径不得复制未修改 table 的 row tree、未修改 row 的字段／ADT value、未修改 index posting tree 或完整 receipt map。变更 row 可以建立新的完整 row value，因为 update 的 simultaneous-expression 和完整类型校验本来就需要一个 after image。

当前 row iteration 按单调 RowId 排序。持久 row map 必须保持这个顺序，因此 query 无显式 sort 时的单次响应不会因内部迁移而随机改变；它仍不获得跨请求顺序承诺。insert 只分配 `next_row_id`，delete 不回收 ID。insert 后又在同一脚本删除仍推进 watermark，避免旧身份复用。

### 4. 候选状态与 write set

writer 在 parse、参数绑定、schema precondition、read-only 和 storage-format preflight 通过后建立：

```text
MutationCandidate
  base Arc<CommittedState>
  next CommittedState        # persistent roots initially shared with base
  writes LogicalWriteSet
  mode IncrementalRows | FullRebuild(reason)
```

增量 write set 至少包含：

```text
LogicalWriteSet
  row_changes (table_id, row_id) -> {before?, after?}
  table_watermarks table_id -> {before, after}
  index_entries (index_id, typed_key, row_id) -> {present_before, present_after}
  receipt_changes key -> {before?, after?}
  meta_before / meta_after
```

每个 stable key 在一个请求内只保留最初 before 与最终 after：

- insert → update：一个 row insert，after 为最终值；
- insert → delete：没有 row/index delta，但 table watermark 仍前进；
- update → update：before 为请求开始时的 row，after 为最终值；
- update → delete：一个 row delete，并删除原请求状态对应的 index entries；
- delete 旧 row → insert 相同业务主键：旧 RowId delete 与新 RowId insert，不能合并身份；
- 多条语句修改不同表：一个 write set，一次 sequence 增长，一次 commit。

query/explain 可以出现在 mutation 脚本内并读取 `candidate.next`，因此继续观察同一脚本前面语句的效果。只有最终 `QueryResponse` 进入 receipt；中间响应与现在一样不会单独发布。

候选构建期间，DML 先在 base/candidate index 上选出稳定 RowId，再为命中 row 生成 after image。主键和 unique index 验证使用“base posting 减去本 write set 删除，再加本 write set 插入”的 candidate view。它必须在发布前发现：

- 与未修改 row 冲突；
- 同批输入相互冲突；
- 同一脚本前后语句产生的冲突；
- index key/value 大小超限；
- RowId、sequence 或 receipt 容量耗尽。

普通 index 也从 before/after row 的完整 typed value 计算删除与插入。未改变的 index key 不产生 delta。#164/#165 可以把 equality key 扩展成 ordered composite key，但不能改变本 RFC 的 before/after 合并规则。

### 5. redb 提交边界

`DurableBackend::commit(previous, database, receipts)` 将拆为显式计划：

```text
CommitPlan::Incremental(PreparedWriteSet)
CommitPlan::FullRebuild { previous, next }
```

`PreparedWriteSet` 在打开 redb write transaction 前完成所有可能的类型驱动编码和大小检查：

- meta：expected old sequence/schema/layout 与新 meta；
- table catalog entry：只在 `next_row_id` 改变时更新；
- row：insert 要求 key 不存在，update/delete 携带 expected old bytes；
- secondary index：delete 要求旧 key 存在，insert 要求新 key 不存在；
- receipt：insert/update/delete 携带 expected state；
- row-only 路径没有 type、index definition 或 migration-ledger delta。

redb transaction 继续使用 `Durability::Immediate` 和 two-phase commit。应用 delta 时必须读取并核对 expected state；任何不匹配在 `transaction.commit()` 前返回确定的 `E_STORAGE`，不发布内存 root。`commit()` 返回错误仍分类为结果不确定：Engine 丢弃候选 root、关闭 durable handle、设置 write-failed，并要求重开。不能因为内存 write set 更精确就弱化这个边界。

redb backend 不再长期保存全部编码 row/index 的 `PreparedState`。它只保留 layout 与已验证 durable head 所需的小型元数据。完整 `PreparedState` 仍可作为临时结构用于：

- 打开和逻辑完整性检查；
- schema/catalog migration 的 full rebuild；
- storage-format upgrade；
- restore/import 与测试中的全状态对照。

增量 commit 成功后，backend 更新 durable head，Engine 再替换 `Arc<CommittedState>`。这两个动作都发生在唯一 writer lock 内。read snapshot 在替换前捕获旧 root 或替换后捕获新 root，不存在 database 与 receipt 来自不同 commit 的组合。

### 6. memory、WAL 与维护操作

memory Engine 使用完全相同的候选状态、约束和 write-set 合并，只跳过 durable prepare/commit。成功后一次发布新 committed root。测试不得为 memory 维护第二套简化 mutation 语义。

过渡 WAL/snapshot 模式继续把完整 source append 作为 durable 边界。它可以从 persistent candidate 降低内存复制，但不会获得 stable-key delta，也不新增格式；带参数和 idempotency key 的限制保持不变。checkpoint/legacy snapshot 的 serde 形态必须通过兼容 DTO 保持当前字段、数组和 checksum 行为。

以下操作不属于 #163 的首个增量 row-only 实现：

- DDL 与 versioned schema migration；
- confirmed receipt prune；
- storage upgrade、restore/import、check/rebuild；
- legacy checkpoint 的流式重写。

它们可以先进入 `FullRebuild(reason)`，但 reason 必须在测试 hook 中可观察，避免普通 DML 意外退化而不被发现。后续切片只有在保留同样故障矩阵时才能把维护操作改为增量。

### 7. Deadline、取消与线性化

沿用当前请求控制顺序：cancel → shutdown → deadline。parse、bind、target scan、expression evaluation、candidate index validation、returning/receipt size check 和 durable prepare 都定期 checkpoint。任何这些阶段的错误只丢弃 candidate 和 write set。

进入 redb commit 后不尝试异步中断事务；同步 commit 完成或返回错误后再决定结果。若 deadline 在最后一次 pre-commit checkpoint 后到达，已进入 commit 的请求按 commit 结果收敛，不能向调用方报告回滚但实际已提交。stream 仍只允许 read，因此不会引入 mutation cancel 的新 wire 状态。

原子发布的线性化点如下：

- memory：writer lock 内替换 `Arc<CommittedState>`；
- redb：durable transaction 成功是持久效果线性化点，随后在同一 writer critical section 替换内存 root；
- WAL：同步 append 成功后替换内存 root；
- definite failure：没有 durable 或内存线性化点；
- uncertain failure：持久线性化点未知，内存 root 不替换，Engine 禁止继续写。

### 8. 状态矩阵

| 场景 | durable state | committed root | receipt/key | 后续行为 |
| --- | --- | --- | --- | --- |
| parse/bind/schema precondition 失败 | 不打开 transaction | 旧 | 不占用 | 修正后可重试 |
| target scan/expression/type/constraint 失败 | 不打开 transaction | 旧 | 不占用 | 原 key 可重试 |
| deadline/cancel 在 prepare 前胜出 | 不打开 transaction | 旧 | 不占用 | 返回对应错误 |
| durable prepare/expected-value 核对失败 | transaction 未 commit | 旧 | 不占用 | definite `E_STORAGE`，Engine 可继续 |
| redb commit 成功 | 新 rows/index/meta/receipt | 新 root | 同时存在 | 返回成功；丢响应可 replay |
| redb commit 返回错误 | 旧或新，重开判定 | 旧 root | 未知但与 effect 原子一致 | durable handle 关闭，禁止写；原 key 重试 |
| memory 成功 | 不适用 | 新 root | process-local 同时存在 | 返回成功 |
| WAL append 失败 | 结果按旧契约视为不确定 | 旧 root | 不支持 durable receipt | 禁止写并重开 |
| checkpoint 在成功 commit 后失败 | 新 | 新 root | 已存在 | 成功响应带 warning |

### 9. 确定性验证

#163 必须加入只在 crate tests 可见的 `CandidateStats`／backend observation，不暴露业务数据：

```text
mode
touched_tables
row_inserts / row_updates / row_deletes
index_inserts / index_deletes
receipt_inserts / receipt_deletes
full_state_builds
shared_row_roots / shared_index_roots
```

验收不能只比较 wall-clock。测试必须直接证明：

1. 修改表 A 一行时，表 B 的 table/row/index roots 与旧 snapshot `Arc::ptr_eq`；
2. 表 A 未修改 rows 的 value roots 继续共享；
3. 单行 insert/update/delete 的 `full_state_builds == 0`，write counts 与实际 stable keys 一致；
4. 多语句 insert→update、insert→delete、update→delete 正确合并；
5. 主键／unique 冲突、returning 超限、receipt 超限和 deadline 不发布 row/index/watermark/receipt；
6. fake backend 的 definite/uncertain failure 继续产生当前 Engine 状态；
7. 旧 read snapshot 在成功写入后仍返回旧 row/index/schema/receipt，新的 snapshot 返回完整新状态；
8. redb 重开、`check --db`、backup/restore 与 idempotent replay 得到和增量候选一致的状态；
9. 一个测试强制走 full rebuild，证明兼容路径仍可用且被明确计数。

workload 在 #166 才形成新的容量结论。#163 只需扩展工具以分开 candidate build、durable prepare/commit 与总耗时，并保存原始样本；不得用单机阈值代替结构性验收。

### 10. 兼容与版本

本 RFC 不改变：

- redb storage format 4；
- catalog/value/index-key/migration/receipt codecs；
- backup format 与 checksum 输入；
- schema revision/hash；
- RowId、cursor `u1/u2`、protocol version 1/2 和 stream version 1；
- query language 或 canonical formatter。

这是内存所有权与 commit preparation 的实现变化。兼容 DTO 必须让现有 legacy snapshot 和 logical backup 的 JSON 形态保持一致；golden backup、旧 redb 打开、显式 format upgrade 与 cursor reopen tests 是合并门槛。如果实现发现必须改变任一持久 byte，#163 必须暂停并另开格式 RFC，不能在本任务中隐式升级。

### 11. 被拒方案

- **只把 `Database` 包进 `Arc` 并使用 `Arc::make_mut`**：第一次 mutation 仍会深复制全部 rows/indexes，无法改变小写集复杂度。
- **只让 row 使用 `Arc`，保留完整 `BTreeMap` clone**：避免 ADT value 深复制，但仍线性复制全部 map/posting entries，也没有解决 redb 全量重编码。
- **只优化 redb delta**：memory candidate 仍深复制，且并发旧快照会放大峰值 RSS。
- **只优化 memory candidate**：`PreparedState::new` 仍遍历和编码全部持久状态，持久写延迟仍与数据库规模相关。
- **可变 live state + undo journal**：写期间旧快照需要额外版本管理；panic、constraint error 和 uncertain commit 的 undo 边界更难审计。
- **追加 overlay chain**：写入便宜，但每次 read/index lookup 必须合并多层或依赖不可预测 compaction，把成本与故障面移到读取。
- **让 redb 成为 query 的唯一 source of truth**：会要求重写 typed query、match、index 与 snapshot 执行路径，形成 memory/redb 双语义，不属于本阶段。
- **同时增量化 schema migration**：会把 catalog identity、嵌套 ADT rewrite 与普通 DML ownership 一次混在一起；先保留可观察 full-rebuild fallback。

### 12. 实施顺序

1. 引入 `CommittedState` 和兼容序列化 DTO，read snapshot 只捕获一个 root；行为不变。
2. 引入 persistent row/receipt roots 与候选统计，先让 memory insert/upsert/update/delete 走增量状态。
3. 把 equality index postings 改为 persistent roots，并让 constraint/index 更新记录 coalesced logical write set。
4. 增加 redb `PreparedWriteSet` 和 incremental commit；保留 full-rebuild adapter 给 schema/maintenance。
5. 接通 idempotent request、多语句脚本、prepared DML、WAL 内存路径和完整故障矩阵。
6. 扩展 workload 分段测量；#163 以结构性测试收口，容量声明留给 #166。

## English Description

### Problem and decision

The current atomic-write path deep-clones the complete `Database`, executes a script against that candidate, then encodes the complete catalog, row set, secondary-index set, migration ledger, and receipt map into a new redb `PreparedState` before diffing it with the previous full encoding. A one-row update therefore performs database-wide memory cloning and durable-state encoding. Existing measurements put 100k-row single-row updates/upserts near 0.88 seconds p95 and roughly 1.25 GiB peak RSS.

This RFC selects one `Arc<CommittedState>` root, path-copying persistent ordered maps/sets for rows, index postings, and receipts, and an explicit coalesced logical write set. Read snapshots clone exactly one committed root. A row-only candidate shares every unchanged persistent node and publishes by replacing that root once under the writer lock. DML records the original before image and final after image for each stable row/index/receipt key, so repeated changes within one atomic script collapse without losing RowId-watermark effects.

The persistent collection abstraction must provide deterministic ordering, O(1) immutable-root cloning, O(log n) path-copy updates, and stable iteration of old roots. Cloning an entire `BTreeMap` behind `Arc::make_mut` does not satisfy the contract, and an unbounded overlay chain is rejected because it transfers unpredictable work to reads. Changed rows may own a complete new ADT value; unchanged row values and unaffected table/index roots remain shared.

### Commit and failure boundary

The durable interface gains explicit incremental and full-rebuild plans. Before opening a redb write transaction, an incremental logical write set is encoded into a bounded `PreparedWriteSet` containing stable keys, expected old bytes/presence, and new values. Row inserts require absence; updates/deletes compare old bytes; index and receipt entries also verify expected state. The transaction retains `Durability::Immediate` and two-phase commit.

A successful redb commit is the durable linearization point; the Engine then replaces its committed root in the same writer critical section. A definite pre-commit failure publishes neither durable nor memory state and remains retryable. A commit error remains uncertain: the candidate root is discarded, the durable handle is closed, writes are disabled, and the caller must reopen and retry the same idempotency key. Data effects and receipts remain one transaction. Memory mode uses the same candidate and validation rules and publishes one process-local root.

Redb no longer retains a complete encoded `PreparedState` after each ordinary commit. Full-state preparation remains an explicit temporary path for open/check, schema migrations, storage upgrades, restore/import, and integrity rebuilds. Transitional WAL mode may benefit from shared in-memory candidates but retains source-append durability and its existing parameter/receipt limitations.

### Semantics and compatibility

Queries inside a mutating script read the evolving candidate, preserving current script semantics. Primary and unique constraints use a candidate index view composed from the base posting minus pending deletes plus pending inserts, catching conflicts with unchanged rows, within one batch, and across statements. RowId allocation stays monotonic; an inserted-then-deleted row still consumes its ID. Returning and receipt-size checks finish before durable commit.

Cancellation, shutdown, and deadlines continue to win only before the uninterruptible synchronous commit begins. If commit has started, the request converges according to its commit result rather than reporting a rollback that may be false. Concurrent readers see either the old complete `CommittedState` or the new one, never database and receipt roots from different commits.

This design changes memory ownership and commit preparation only. It does not change storage format 4, any durable codec, backup/checksum shape, schema identity, RowIds, cursor versions, protocol versions, stream protocol, query syntax, or formatting. Compatibility DTOs preserve the current legacy-snapshot and logical-backup JSON. Any implementation discovery that requires changing durable bytes must stop #163 and open a separate format RFC.

### Verification and implementation order

Crate-only candidate/backend observations report the selected mode, touched tables, row/index/receipt changes, full-state builds, and shared roots without exposing application values. Tests use pointer identity and exact write counts to prove structural sharing; they also cover change coalescing, constraint/limit/deadline rollback, definite and uncertain backend failures, old-snapshot isolation, redb reopen/check, backup/restore, receipt replay, and one explicit full-rebuild fallback. Timing is supporting evidence only; #166 owns the new 10k/100k capacity claim.

#163 first introduces a single committed root and compatibility DTOs, then persistent row/receipt state, persistent equality-index postings and coalesced writes, direct redb incremental commits, idempotent/multi-statement/prepared/WAL integration, and segmented workload measurements. Ordered composite indexes in #164/#165 reuse this before/after write-set contract after the row/index ownership model is stable.
