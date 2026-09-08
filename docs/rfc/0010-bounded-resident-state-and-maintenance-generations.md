# RFC 0010：有界常驻状态与可恢复维护 generation

- 状态：accepted for staged implementation
- 日期：2026-09-09
- 跟踪：[M7 #177](https://github.com/worktools/unionid/issues/177)、[观测 #178](https://github.com/worktools/unionid/issues/178)、[设计 #179](https://github.com/worktools/unionid/issues/179)
- 前置：[RFC 0006](0006-consistent-read-snapshots.md)、[RFC 0008](0008-incremental-candidate-state.md)、[RFC 0009](0009-ordered-composite-indexes.md)

## 中文说明

### 1. 问题与证据

M6 已让普通 row-only mutation 使用 persistent candidate roots 和显式 write set，也让 equality-prefix range、index order 与 page seek 使用 typed ordered composite index。10k/100k 工作负载确认，小写集写入和有界有序读取不再随完整表复制增长，但 redb Engine 仍在 open 时载入全部 rows/indexes，并在 schema/data migration 时重载、验证和编码完整前后状态。

[#178 的分阶段样本](../benchmarks/storage-phases-2026-09-09.md) 定位了主要成本：

- 100k open p50 约 5.44 s，其中完整派生索引重算与逻辑验证约 4.72 s，typed `Database` 构造约 0.55 s，row/index bytes 读取合计约 0.15 s；
- 100k migration durable commit p50 约 47.18 s，其中完整候选编码约 41.01 s，重载并验证前态约 5.44 s，delta apply 与 redb sync 合计约 0.76 s；
- 100k migration peak RSS 约 1.43 GiB；普通 update/upsert 的 durable p50 仍约 9 ms，且 reload/encode/diff 均为零。

因此下一步不能只微调 redb I/O 或 sync。设计必须让普通读取无需先构造完整 resident `Database`，把完整检查移到显式有预算的维护路径，并让需要重写数据的 migration 在可恢复 shadow generation 中分批完成。

### 2. 目标与非目标

本 RFC 的目标是：

1. redb open 常驻 catalog、schema identity、sequence、table/index directory、receipt 上限状态和一个 active-generation read view，不常驻全部 row values 或 index postings；
2. primary/equality/range/order/page access 按 index span 和实际候选 RowId 读取、解码，`take`/`page` 能在满足结果后停止；
3. full scan、aggregate、sort、stream、returning 和 mutation target 共用一个有界 typed row-source contract；
4. 每个请求快照绑定一个 redb MVCC commit 和一个 generation，不混合两个 schema、row/index keyspace、sequence 或 receipt 状态；
5. schema/data migration 使用可 checkpoint/resume 的 shadow generation，并在一个短事务中原子 cutover；
6. memory 与 redb 只允许物理 row source 不同，继续共享 parser、binder、typed expression/match、ADT codec、total order、planner 和 DML 规则。

本阶段不增加 join、window、分布式执行、多写者、跨请求长事务或用户可长期持有的 snapshot。它也不把 redb bytes、generation ID 或 cache policy 暴露成 query-language 数据模型。

### 3. 决策摘要

采用四个边界：

1. **catalog 与 row storage 分离**：`Database` 的 schema/catalog 视图不再以 resident `Vec<Row>` 和完整 in-memory posting 作为存在前提。
2. **request-scoped physical snapshot**：redb committed root 持有一个只读 transaction/table view；并发请求克隆该 root，普通 commit 后创建并发布新的 view。
3. **统一 `TypedRowSource`**：planner 产生同一个 typed access plan，memory adapter 从 persistent maps 读取，redb adapter 从 row/index key range 读取。后续表达式和 stage 不区分后端。
4. **generation maintenance**：format 6 为 catalog/rows/indexes 增加 generation keyspace。migration 分批写 shadow generation，只有 cutover transaction 改变 active generation、schema head、ledger、sequence 和可选 receipt。

逻辑结构如下；名称是内部契约，不承诺原样成为公共 Rust API：

```text
Engine
  committed Arc<CommittedView>
  durable option DurableBackend
  writer / maintenance state

CommittedView
  catalog Arc<CatalogSnapshot>
  source Arc<dyn TypedRowSource>
  receipts PersistentMap<IdempotencyKey, Arc<Receipt>>
  sequence / schema identity / cursor identity
  generation GenerationRef
  row_cache Arc<SnapshotRowCache>

GenerationRef
  Legacy0 | Generated(u64)

TypedRowSource
  Memory(MemoryReadView)
  Redb(RedbReadView)  # one MVCC transaction and one generation
  Candidate(Base + coalesced LogicalWriteSet)
```

### 4. `TypedRowSource` 契约

row source 至少提供以下内部操作：

```text
snapshot_identity() -> {database_instance, generation, sequence, schema_hash}
table_stats(table_id) -> {rows, next_row_id, index cardinalities}
get_row(table_id, row_id, control) -> option Arc<Row>
scan_rows(table_id, row_id_bounds, direction, control) -> RowBatchCursor
scan_index(index_id, encoded_bounds, direction, read_limit, control) -> IndexHitCursor
```

`RowBatchCursor`／`IndexHitCursor` 每次返回有界 batch，并在 batch 与行之间检查 cancel → shutdown → deadline。index cursor 只返回 `(typed index boundary, RowId)`；执行器再按 RowId 读取完整 row 并运行 residual filter。row 解码继续调用现有 catalog-aware ADT value codec，index bounds 继续调用 RFC 0009 的 typed ordered encoder，不能在 redb adapter 中实现第二套比较或 match。

所有 cursor 保持稳定顺序：row scan 按单调 RowId；index scan 按 encoded tuple + RowId，支持整体 reverse。重复 RowId、错误 table/index ID、未知 codec、错误 nominal payload 或超过 value/index 深度和大小限制均返回 `E_STORAGE`，不能作为“不存在”跳过。

Candidate source 只允许一个请求内的一层 overlay：base source 减去 pending deletes，再按稳定 key 合并 pending inserts/updates/index changes。它继续遵守 RFC 0008 的 before/after coalescing，并在请求结束时整体丢弃或发布；不能形成跨请求 overlay chain。

### 5. Planner 与访问复杂度

binder 和 planner 只读取 catalog、table/index definitions、schema identity 和小型统计，不触发 row decode。访问复杂度按以下方式表达：

| access | index work | row decode | 停止条件 |
| --- | --- | --- | --- |
| primary/完整 unique lookup | `O(log I)` | 0 或 1 | 第一个完整 key |
| equality/composite lookup | `O(log I + span)` | residual filter 实际候选 | span 结束或 `take` 满足 |
| range/order | `O(log I + examined)` | examined candidates | bound 结束或 `take` 满足 |
| page seek | `O(log I + examined)` | 最多满足 `limit + 1` 所需候选 | `limit + 1` 个结果 |
| full row scan | `O(R)` | 扫描到的 rows | table 结束或可安全下推的 `take` |

`span` 是 durable index bound 内 entry 数；`examined` 包含被 residual filter 拒绝的 entry。planner 不为获得精确估算而预扫完整 span。unique lookup 可以给出 0/1 上界；其他计划使用 generation manifest 的 table/index cardinality 作为上界，并标记 estimate 是否 exact。现有 `estimated_rows` 在过渡期保持兼容，但不能被解释为实际解码数量。

每次执行记录无业务值的 `index_entries_examined`、`rows_decoded`、`row_cache_hits/misses`、`batches` 和 `working_peak_bytes`。`explain` 继续不读取业务行；workload 用执行 observation 证明 ordered/page 只解码实际候选，而不以 wall-clock 代替结构验证。

planner 仍不得越过会改变语义的 stage。只有与当前规则一致的 equality/range/sort/page/take 才能进入 source pushdown；其他 filter、derive、match、aggregate 和 field projection 继续按 source stage 顺序执行。

### 6. 执行流水线与内存预算

执行器把当前 `Vec<&Row>` 模型收敛成 typed batch pipeline：

1. source 产生 row batch；
2. filter/filter-match、derive/derive-match、select 和可安全下推的 take 逐行执行；
3. aggregate 使用现有有界 accumulator；group 使用现有 group/cell/byte 上限；
4. sort 或不能流式完成的 stage 把 typed working rows 放入显式有行数和 encoded-size 预算的 buffer；
5. 非 stream 查询仍受 result-row、returning-byte 和 response-byte 上限；stream producer 从 pipeline 拉取 batch，并继续受 frame、queue、总 emitted bytes 和 deadline 约束。

首个实现使用固定默认值，并在 introspection 中报告：

- snapshot row cache：32 MiB；超过 cache 的单行可以在通用 value 上限内 uncached 返回；
- source batch：最多 1,024 rows 且累计 encoded input 不超过 16 MiB，单个合法大 row 单独成 batch；
- sort/general working buffer：最多 250,000 rows 且累计 typed working state 不超过 64 MiB；
- group state：沿用 100,000 groups、aggregate cell 上限和 64 MiB；
- ordinary response：沿用 100,000 rows 与 16 MiB；stream：沿用 8 frames、16 MiB queued、256 MiB emitted。

实现可在验证后调整数值，但不能只保留 row count 而不计算可变长 ADT/list/text/bytes 的内存。超过 working state 返回 `E_LIMIT`；它不会扩大 cache，也不会退化成完整 resident load。

### 7. Snapshot 与 cache 所有权

`RedbDatabase` 改为由 durable backend 与 read views 共享的 `Arc`。`RedbReadView` 在 writer lock 内、最新 commit 已确定且尚无后续 writer 时创建一个 redb read transaction，并打开 active generation 对应的固定 tables。redb 4.1 允许只读 transaction 与 write transaction 并存；read-only table/iterator 自身保持 transaction guard，因此请求执行期间看到同一个 MVCC root。

`ConcurrentEngine` 继续限制 active read snapshots，并用请求 deadline 限制等待和执行。旧 snapshot 持有旧 redb transaction；新 commit 创建新的 transaction 和 `CommittedView`。redb 负责在旧 transaction 存活时保留所需页面，旧 transaction 释放后才可回收物理页面。网络 backpressure 不得绕过现有 stream deadline 无限延长 snapshot。

row cache 归一个 `CommittedView` 所有，由相同 sequence 的并发读共享。key 为 `(table_id, RowId)`，value 是不可变 `Arc<Row>`，并按编码输入 bytes + 估算 typed overhead 计费。commit 发布新的 view 和空 cache；cache 不跨 sequence 复用，因此更新相同 RowId 不会命中旧值。旧 view/cache 随最后一个请求一起释放。

cache 使用严格 byte-cap LRU 或等价有界策略。锁只保护 cache metadata，不包围 redb I/O、decode 或表达式执行；并发 miss 可以重复解码，但不能让 cache 超限。任何 cache 指标都不包含 key/value 内容。

成功 durable commit 后若无法创建新 read view，持久效果已经确定，不能报告普通 rollback。Engine 关闭 durable handle 并禁止后续写，返回稳定的 `E_STORAGE_REOPEN_REQUIRED`，明确说明 commit 已完成、需要 reopen；带 idempotency key 的调用可在 reopen 后 replay 原成功回执。该故障点必须单独注入测试。

### 8. Storage format 6 与 generation keyspace

format 6 新增以下固定表：

| 表 | key | value |
| --- | --- | --- |
| `generation_catalog` | `(generation, existing catalog key)` | 现有 catalog value codec |
| `generation_rows` | `(generation, table_id, row_id)` | 现有 ADT value codec |
| `generation_index` | `(generation, existing index key)` | unit |
| `maintenance_generation` | `generation` | versioned manifest/checkpoint |

`meta` 新增 `active_generation`、`next_generation_id` 和 `maintenance_codec_version`。`migration_ledger` 与 `idempotency_receipts` 继续是全局表；它们与 active generation、schema head 和 sequence 在 cutover transaction 中一起更新。catalog/value/index component/migration/receipt codec 不因本 RFC 改变；generation prefix 是 format-6 physical envelope，由 storage format fail-closed。

format 5 的原 `catalog`、`rows`、`secondary_index` 视为 `Legacy0`。显式 5→6 upgrade 只创建新表并原子写入：

```text
active_generation = Legacy0
next_generation_id = 1
maintenance_codec_version = 1
storage_format_version = 6
```

它不重写 rows/indexes，不改变 schema revision/hash、sequence、RowId、ledger、receipts 或 database/cursor identity。format-6 binary 能在 Legacy0 上使用 bounded row source 和普通增量 DML；第一次需要重写 schema/data 的操作构建 Generated(1)。cutover 后普通 DML 只修改 active generated keyspace。

新建数据库直接以 Generated(1) 作为空 active generation，`next_generation_id = 2`。generation ID 单调分配且永不复用；abort 后留下的 ID 也不能再次分配，避免 stale manifest、日志或诊断把两个物理状态视为同一 generation。

旧 binary 必须因未知 format 6 而拒绝打开。没有原地 downgrade。升级前的 logical backup 仍可由理解 backup format 4 的旧 binary 恢复成 format 5；一旦切换到 generated keyspace，只能通过 logical backup/restore 回到旧版本能理解的格式。

### 9. Maintenance manifest

同一数据库最多有一个未结束的 building/aborting generation。manifest 至少包含：

```text
codec_version
state = Building | Ready | Aborting | Reclaimable
source_generation / target_generation
operation_kind
migration_id / parent / checksum
source schema revision/hash/sequence
target schema revision/hash and canonical catalog digest
last completed (table_id, row_id)
source rows seen / target rows written / index entries written
logical encoded bytes / rolling digest
executor compatibility version
created/updated timestamps
```

manifest 不保存业务 row/key/value。migration 的 canonical source 或稳定 plan digest 可以保存；恢复执行必须由调用方再次提供同一 migration file，并核对 ID、parent、checksum、source identity 和 executor compatibility version。不能在新 binary 中静默用不同 parser/binder 语义续跑旧 checkpoint。

`migration status` 和 introspection 显示 state、ID、source/target generation、完成 rows/bytes、最后更新时间和可执行动作，不显示 cursor secret、row key 或 value。不同 migration 文件尝试占用已有 checkpoint 返回 `E_MAINTENANCE_CONFLICT`。

### 10. Migration 状态机

一次 versioned schema/data migration 按以下阶段执行：

1. **Plan**：在当前 catalog 上 parse/bind 完整 migration，确定 target catalog、转换表达式、index shapes、schema identity 和资源预算；不写 durable state。
2. **Start**：分配 target generation；一个同步事务写 target catalog、`Building` manifest 和 `next_generation_id`。active generation 不变。
3. **Build batches**：按 `(table_id, RowId)` 从固定 source read view 读取；每行使用同一 migration typed IR 转换、完整类型检查并编码 target row/index。一个 batch 的 rows、indexes、rolling digest 和 checkpoint 在同一同步事务提交。
4. **Validate target**：源行遍历完成后核对 row/table/index cardinalities、RowId watermarks、primary/unique key、catalog/schema digest 和 rolling digest；将 manifest 原子改为 `Ready`。
5. **Cutover**：一个短同步事务核对 active/source generation、source schema/sequence 和 Ready manifest，然后同时更新 active generation、schema meta、sequence、migration ledger、manifest state 与可选 idempotency receipt。
6. **Publish**：创建新 active read view 并一次替换 Engine committed root。响应只在该点后返回成功。
7. **Reclaim**：旧 generation 标记 `Reclaimable`，后续按 key range 分批删除。redb 的旧 read transaction 继续看到其原 MVCC root；清理不等待网络请求，也不改变逻辑 sequence/schema。

Start/Build/Validate 都是内部维护进度，不是用户 schema/data commit。它们不能生成可查询的新 schema、使 cursor stale、占用 mutation idempotency receipt 或追加 migration ledger。只有 Cutover 是逻辑线性化点。

build 期间唯一 writer gate 阻止普通 mutation、另一 migration、receipt prune、format upgrade、restore 和 destructive maintenance，返回 `E_MAINTENANCE_REQUIRED`。普通 read、query、stream、backup old active generation 和只读 introspection 可以继续。这样 source generation 不会在多个 batch 之间变化。

调用方用相同 migration files 再次执行 `migration apply` 时自动从 checkpoint 继续。显式 `migration abort` 把 Building/Ready 改为 Aborting，再分批删除 target keys；完成后删除 manifest，active generation 始终不变。确定的 type/constraint/limit 错误自动进入同一 abort cleanup；deadline/cancel 保留 Building checkpoint，以便 resume 或 abort。

### 11. Crash 与错误矩阵

| 位置 | reopen 可见状态 | 恢复动作 |
| --- | --- | --- |
| Plan/Start 前失败 | old active，无 manifest | 修正后重试 |
| Start commit 前失败 | old active，无 target 或只有回滚事务 | 重试 |
| Start commit 结果不确定 | old active；manifest/ID 可能存在 | reopen 读取 manifest；匹配则 resume |
| Build batch commit 前失败 | old active，上一个 checkpoint | 同文件 resume |
| Build batch commit 结果不确定 | old active，checkpoint 为 batch 前或后 | reopen 以 checkpoint/digest 判定，不重复写已完成 batch |
| type/unique/size/budget 失败 | old active，Aborting 或已清理 | 自动/显式完成 abort，再修订未应用文件 |
| deadline/cancel/shutdown | old active，Building checkpoint | resume 或 abort |
| Ready 后、cutover 前退出 | old active，Ready target | 同文件直接复核并 cutover，或 abort |
| cutover commit 前失败 | old active，Ready target | 可重试 cutover |
| cutover commit 结果不确定 | old或new完整 active；ledger/receipt 与 active 原子一致 | reopen 后以 active+ledger/receipt 判定 |
| cutover 成功、read-view 创建失败 | new active 已确定 | `E_STORAGE_REOPEN_REQUIRED`；reopen/replay |
| reclaim 中退出 | new active，old 部分已删除且 Reclaimable | resume bounded cleanup |

任何状态都不能让 catalog 指向 source generation、rows 指向 target generation，或让 ledger/receipt 宣称 migration 成功但 active generation 仍是 old。损坏/未知 manifest codec、active generation 缺失、Ready digest 不匹配或多个 Building target 以 `E_STORAGE` fail closed；只读诊断可以报告原始 generation IDs 和状态，但不能自动猜测一个 active generation。

### 12. Open、完整检查与 corruption 边界

format-6 快速 open 只做：

- redb 自身 open；
- meta/layout/active-generation/manifest codec 检查；
- active catalog decode、有限 ADT/schema identity 和 index definition 校验；
- ledger head、receipt limits 和 maintenance state 校验；
- active row/index table 的存在性及小型 cardinality/watermark metadata 校验；
- 创建 active read view 和空 row cache。

它不遍历或重新派生全部 rows/index entries。由外部修改、介质问题或旧 bug 造成的单行损坏会在读取该 row/index 时返回 `E_STORAGE`，或由显式 `check --db` 发现；快速 open 不再声称证明全库逻辑完整性。

`check --db` 改为有界 full check：

1. 逐 row 解码并验证 table/type/RowId/watermark；
2. 为每个 row 计算应有 index key，并对 active index table 做点查；
3. 逐 stored index entry 解码，检查 generation/index/RowId，读取对应 row 并重算 key；
4. 通过 cardinality 和双向存在性发现缺失与多余 entry；unique index 按相邻 tuple 检查重复；
5. 检查 ledger、receipts、schema/meta 和 maintenance manifests；
6. 每批受 deadline/cancel 和 working-byte 预算约束，输出 rows/indexes/bytes/progress 与 phase profile。

这样检查的 I/O 可以是 `O(rows × indexes × log N)`，但 resident working state 有界，不再建立完整派生 index set。`check` 成功才能把 generation 标记为在当前 binary/codec 下 full-verified；普通 commit 保留该标记，因为 stable-key expected-value 和同事务 row/index 更新维持局部不变量。外部文件修改或 redb repair 会清除/重新要求 full check。

### 13. Backup、restore 与 storage upgrade

logical backup 保持 format 4 的 schema、typed rows、RowId、ledger、receipts、index definitions 和 checksum 语义，不写 generation ID、cache 或 manifest。首个 read-source slice 可以继续在现有 1 GiB 文件上限内 materialize；M7 收口前必须改为从一个 committed read view 流式序列化，并证明输出可由现有 format-4 reader恢复。若无法保持 canonical checksum bytes，则另开 backup-format RFC，不能在本实现中静默改变。

restore 只写不存在的目标路径。它先验证完整 logical backup，再建立一个 Generated(1) active generation；失败不发布目标数据库。后续可让 restore 复用 generation builder，但不能把半个 restore 目标当作可打开数据库。

format 5 可以使用 bounded Legacy0 read source 而不修改文件；需要 generation migration 前返回 `E_STORAGE_UPGRADE_REQUIRED`。5→6 metadata upgrade 原子且不遍历 rows。format 3/4 仍按现有 codec upgrader 到 5，再进入 6；每一步失败都留下一个完整已知格式。没有原地 downgrade，回滚依赖升级前 logical backup 和旧 binary 可理解的 backup format。

### 14. Cursor、receipt、prepared request 与 read-only

- cursor wire 不增加 generation 字段。现有 database identity + schema/query digest + sequence 已足以使任何成功 cutover 后的旧 cursor stale；内部 snapshot 仍核对 generation 与 sequence。
- receipt 是 global durable state，但每个 read snapshot 通过同一 redb transaction/committed root观察与 active generation 同一次 commit 的状态。cutover 的 migration effect、ledger 和 receipt 必须同事务。
- prepared query/DML 继续绑定 schema revision/hash。执行时还核对 committed snapshot identity；cutover 后旧 plan 在任何 row scan 前失败。
- read-only redb 使用相同 bounded read view，允许 query、stream、status、backup 和显式 check；禁止 start/resume/abort/cutover/reclaim 与普通 mutation。
- memory Engine 不创建 generation tables。它使用同一个 `TypedRowSource`、pipeline、candidate overlay 和 migration typed IR，并在一次内存 root 替换中完成 migration；generation/crash resume 是 durable backend 能力。
- transitional WAL/snapshot 不获得 generation maintenance；其现有兼容入口保持限制，新的可恢复 migration 只适用于 redb。

### 15. 资源预算与错误码

maintenance 每个 batch 最多 1,024 rows、16 MiB source bytes 和 32 MiB target row+index bytes；target generation 的逻辑 encoded bytes 默认上限 1 GiB。达到 batch 边界就 checkpoint；单个合法 row 超过 batch byte 上限时允许单独处理，但仍受 value/index key 上限。物理 redb 文件可能因 active+shadow+MVCC pages 大于逻辑计数，introspection 必须显示 active/shadow logical bytes 和数据库文件大小，文档不能把逻辑上限描述成磁盘空间保证。

稳定错误类别：

| code | 含义 |
| --- | --- |
| `E_MAINTENANCE_REQUIRED` | unfinished generation 阻止普通写；先 resume/abort/reclaim |
| `E_MAINTENANCE_CONFLICT` | 提供的 migration/target 与 durable manifest 不匹配 |
| `E_MAINTENANCE_LIMIT` | generation rows/bytes/batch/working budget 超限 |
| `E_STORAGE_UPGRADE_REQUIRED` | 当前 format 能读写，但不能执行 generation maintenance |
| `E_STORAGE_REOPEN_REQUIRED` | durable commit 已成功，创建/发布新 read view 失败 |
| `E_STORAGE` | codec、manifest、row/index、generation 或完整性损坏 |
| `E_TIMEOUT` / `E_CANCELLED` / `E_SHUTDOWN` | 在可中断 build/check batch 边界停止，active generation 不变 |

commit 前确定失败和 commit 结果不确定继续使用 RFC 0008 的分类。maintenance batch 的不确定提交不等于 migration effect 不确定；reopen 可由 checkpoint 判定。只有 cutover transaction 的不确定结果涉及用户可见 migration effect。

### 16. 可观测性

`.storage`、JSON introspection 和 profile 增加：

```text
storage_mode
active_generation / active_generation_kind
active_sequence / schema revision/hash
resident_catalog_bytes / resident_receipt_bytes
row_cache_limit/used/entries/hits/misses
active_read_views and oldest view sequence
maintenance state/source/target/progress/logical_bytes
reclaimable generations/logical_bytes
last_full_check sequence/time
```

query/workload observation 增加 access kind、index entries examined、rows decoded、cache hits/misses、source batches 和 peak working bytes。maintenance profile 增加 plan/start/build/validate/cutover/reclaim 的 duration、rows、index entries、bytes 和 peak RSS。所有字段只含稳定 ID、数量、版本、状态和时间，不含 schema/field 名以外的业务值、row/index key、cursor secret、idempotency key 或 receipt payload。

### 17. 一致性与故障验证

实现必须用确定性测试证明：

1. 同一 query/params 在 memory 与 redb source 上对 primitive、sum/product、tuple、option/list、递归 ADT、production scalars 得到同样 rows/order/errors；
2. primary/equality/range/order/page 只解码实际候选，residual filter 与 `limit + 1` 计数正确；
3. full scan/filter/derive/match/aggregate/group/sort/stream 在 batch 1、边界 batch 和大值下与 resident executor 一致；
4. old read view 存活时 commit/cutover 后仍只看到 old generation/sequence/receipt，新 view 只看到 new；
5. cache 不跨 sequence 返回旧 RowId，严格遵守 bytes 上限，并发 miss 不影响结果；
6. insert/upsert/update/delete、多语句脚本、unique/index/returning/receipt/deadline 使用 Candidate source 后保持 RFC 0008 原子性；
7. migration 在每个 Start/Build/Ready/Cutover/Reclaim 故障点，包含 commit 前失败、commit 返回错误和进程直接退出，只暴露完整 old/new active；
8. wrong file/checksum/executor version 不能 resume，abort 可从任意 Building/Ready checkpoint完成；
9. format-5 Legacy0 bounded open、5→6 crash、Generated cutover、旧 binary fail-closed、backup/restore 和 cursor/receipt replay 保持契约；
10. 损坏 active generation、manifest、row 或 index 在 fast-open/access/full-check 对应边界返回稳定错误，不发布部分 profile；
11. 10k/100k workload 保存 raw samples，indexed/page 的 decoded rows 由结果规模决定，open 不再全量 decode rows/indexes，migration peak RSS 受 batch/working budget 约束。

性能阈值是 supporting evidence，不替代上述结构性断言。单机结果继续标明环境，不写成 SLA。

### 18. 被拒方案

- **只跳过 open 时的 index 一致性检查，仍加载全部 rows/postings**：能减少一部分 CPU，但没有 bounded resident state，也不能降低 query process RSS。
- **只为 redb 重写一套 query executor**：会复制 ADT、match、comparison 和 stage 语义；采用共享 typed row source 与同一 IR。
- **每次 query 临时打开最新 read transaction**：snapshot capture 后到实际 open 之间可能已有新 commit，无法保证 committed root 的 sequence/receipt 与 rows 相同。
- **全局跨 sequence row cache**：相同 `(table, RowId)` 可在普通 update 后改变，正确 invalidation 会重新引入完整 write/cache 协调；首版 cache 归 committed view。
- **一个大 redb transaction 完成 migration**：仍需要长事务、完整候选内存和不可 checkpoint 的失败恢复，正是 #178 观察到的边界。
- **每个 migration generation 使用动态 redb table name**：redb table definition 使用静态 table identity，动态表也扩大 schema/cleanup 面；采用固定表中的 generation prefix。
- **5→6 立即重写所有 keys**：升级本身先支付一次全量 migration 和高峰值；Legacy0 允许 O(1) metadata upgrade。
- **在 build batch 间允许普通写并做 change capture**：需要 delta log、冲突/replay 和 cutover catch-up，显著扩大首版故障矩阵；M7 维护期间串行 writer，读仍并发。
- **自动选择损坏 manifest 中“看起来最新”的 generation**：可能发布不完整 schema/data；未知或矛盾状态必须 fail closed。
- **把 generation/cache 暴露为语言对象**：它们是物理生命周期，不属于 ADT/query 模型；只通过管理 introspection 观察。

### 19. 分阶段实施

1. **Committed view 与 source abstraction**：拆分 catalog/row ownership，引入 memory adapter、candidate overlay 和结果一致性测试，行为与格式不变。
2. **Legacy0 bounded redb read**：redb read transaction、row cache、row/index range cursor、indexed/page execution observation；format 5 直接受益，open 不全量 row decode。
3. **有界完整执行**：full scan、blocking sort/group、stream、mutation target、check 和 logical backup 接入 batch/byte budget；普通 DML 保持 incremental write set。
4. **Format 6 与 generation builder**：metadata upgrade、新固定表、manifest、batch build/validate/cutover/abort/reclaim 和 crash matrix。
5. **Migration/接口闭环**：runner resume/abort/status、read-only/CLI/introspection、prepared/cursor/receipt/backup/restore/upgrade 兼容。
6. **容量复验**：重跑 10k/100k open/query/write/check/migration，保存 phase/decoded rows/cache/working bytes/RSS 原始样本，更新舒适范围与运维指南。

首个实现 PR 只做第 1 步并建立可替换 source seam；不能以仍由 redb 完整载入 rows 的 façade 宣称完成 bounded read。第二步的退出条件是 format-5 100k indexed/page query 在 open+execute 全程不构造完整 resident row/index state，并由 decoded-row 计数直接证明。

## English Summary

M6 removed complete-database copying from ordinary row-only mutations and added typed ordered range/order/page access, but the retained M7 profiles show that the remaining boundary is full-state materialization. At 100k rows, open spends about 4.72 of 5.44 seconds recomputing and validating every derived index, while a durable migration spends about 41.01 of 47.18 seconds encoding the complete next state and another 5.44 seconds reloading and validating the previous state. Redb transaction apply and sync are a small fraction.

This RFC separates the resident catalog from physical rows and introduces one internal `TypedRowSource` used by both memory and redb. A committed redb view owns one MVCC read transaction, one active generation, schema/sequence identity, bounded receipts, and a 32 MiB snapshot-local row cache. The planner emits the same typed access plan for both adapters. Redb index cursors yield ordered RowIds, rows are decoded through the existing catalog-aware ADT codec, and residual filters/stages use the existing expression and match IR. Primary, equality, range, order, and page operations are measured by index entries examined and rows decoded; they stop after their bound or `take`/`limit + 1` result requirement. Full scans use batches, and blocking sort/group state gains an explicit byte budget in addition to existing row/cell limits.

Concurrent requests clone one `CommittedView`. Old views keep their redb read transactions and continue to observe the old MVCC root while a writer commits and publishes a new view. A row cache belongs to exactly one committed sequence and is never reused across commits. The existing active-read limit, deadlines, cancellation, and stream backpressure bound old-snapshot lifetime. If a durable commit succeeds but the new read view cannot be created, the Engine returns `E_STORAGE_REOPEN_REQUIRED`, disables further writes, and states that the effect committed; an idempotent caller can reopen and replay the stored receipt.

Storage format 6 adds fixed generation-prefixed catalog, row, and index tables plus a versioned maintenance manifest. Existing format-5 tables become `Legacy0`; the explicit 5-to-6 upgrade only creates tables and metadata, without rewriting data or changing schema identity, sequence, RowIds, ledger, receipts, or cursor identity. New databases start at `Generated(1)`. Catalog/value/index component/migration/receipt codecs remain unchanged, while old binaries reject unknown format 6.

A schema/data migration plans against one fixed source generation, allocates a shadow target, then transforms and encodes bounded batches. Each batch commits rows, indexes, rolling digest, counters, and its checkpoint atomically. Target validation marks the manifest Ready. One short cutover transaction verifies the source/target identities and atomically changes the active generation, schema meta, sequence, migration ledger, manifest states, and optional receipt. This transaction is the only user-visible migration linearization point. Old generations become Reclaimable and are deleted in bounded batches; live redb read transactions continue to see their old MVCC roots.

While a generation is Building or Ready, ordinary writes and competing maintenance return `E_MAINTENANCE_REQUIRED`, but reads and streams continue against the old active generation. Reapplying the same migration files resumes a matching checkpoint; a different ID/checksum/source/executor version returns `E_MAINTENANCE_CONFLICT`. Deterministic transform failures enter bounded abort cleanup, while timeout/cancellation leaves a resumable checkpoint. Start/build/cutover/reclaim fault injection must cover pre-commit failure, uncertain commit return, and direct process exit. Reopen must always expose one complete old or new active generation, never a mixed catalog/row/index/ledger/receipt state.

Fast open validates meta, the active generation, catalog/schema identity, ledger/receipt bounds, and manifests, then creates a read view without scanning all rows or indexes. Corrupt row/index payloads fail when accessed or during explicit full `check`. Full check verifies rows and indexes in both directions with bounded batches and point lookups instead of constructing one complete derived-index set. Logical backup format 4 remains generation-independent; M7 must stream it from one committed view or introduce a separate reviewed backup-format RFC if canonical compatibility cannot be preserved.

Implementation proceeds through a shared source abstraction, bounded Legacy0 redb reads, bounded full execution/check/backup, format-6 generation maintenance, interface compatibility, and a final 10k/100k rerun. Completion requires deterministic memory/redb result parity, candidate-write atomicity, snapshot isolation, cache limits, every maintenance crash point, format upgrade/old-binary rejection, backup/restore, cursor/receipt behavior, and raw observations proving that bounded indexed/page reads decode only actual candidates.
