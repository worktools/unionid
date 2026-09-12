# RFC 0013：最小关联读 / minimal relational reads

- 状态 / Status: accepted, batch fetch and bounded lookup implemented
- 日期 / Date: 2026-09-12
- 跟踪 / Tracking: [#241](https://github.com/worktools/unionid/issues/241)

## 中文说明

### 1. 问题

unionid 没有 join、子查询或跨表关联读。中等业务常见的“主表 + 明细/引用/权限”只能两次查询在应用层组装，或反范式成嵌套 ADT。前者要求应用自行保证一致快照、批量效率与缺失值处理；后者复制数据、更新放大。这是 ADT 用户引入 unionid 时最频繁的硬性缺口。

完整 SQL join 会引入方向、去重、排序与资源预算的大量语义，风险高。本 RFC 先冻结一个**最小、可组合、可解释**的关联读，并把它拆成能独立验收的切片。

### 2. 首个切片：索引键批量取回

新增 Rust API（不是查询语言语法）：

```rust
let users = engine.fetch_by_key("users", "id", &[Value::Int(1), Value::Int(2)])?;
let typed = engine.typed_fetch_by_key::<User>("users", "id", &[Value::Int(1)])?;
```

- 结果与输入 `keys` **等长且同序**；缺失键对应 `None`。
- 全过程基于调用期间同一个 committed 快照（`&mut Engine` 上无并发写者），因此不存在跨键看到不同提交的问题。
- 每个键只读取至多两行：先用 `explain` 确认访问计划命中索引，再执行 `filter key == $key | take 2`。

### 3. 语义与边界

- **索引前置条件**：`key_column` 的等值访问计划必须是 `primary_key_lookup`、`secondary_index_lookup` 或 `composite_lookup`；否则返回 `E_RELATION_KEY`，避免退化为逐键全表扫描。
- **唯一性**：单个键命中多行返回 `E_RELATION_NOT_UNIQUE`。主键天然满足；非 unique 二级索引由运行时 `take 2` 检查，数据不唯一时显式失败而不是静默取第一行。
- **资源上限**：`keys.len()` 上限为 `Engine::MAX_BATCH_KEYS`（10,000），超限返回 `E_LIMIT`；每个键最多读取 2 行，working/result 预算沿用现有查询上限。
- **错误**：绑定类型不匹配、表/列不存在沿用现有 `E_TYPE`/`E_TABLE`/`E_FIELD`；调用方通过 `typed_fetch_by_key` 解码时沿用 `Value::to_serde` 的错误路径。
- **只读**：不改变单请求原子语义，不写任何状态。

### 4. 第二个切片：有界 lookup stage

查询语言用基数保持的嵌套展开表达一对多关联：

```text
from orders
sort id
page 100
lookup lines from order_lines on order_id == id take 100
select {id, customer, lines}
```

`on` 左侧是目标表字段，右侧是当前 driver row 字段。每个输入行恰好产生一个输出行，并新增 `lines: list Line`；目标没有匹配时为 `[]`。这让结果保持 product type，并避免引入 SQL 式扁平重复行和 `null`。

- **索引前置条件**：目标字段必须是主键、二级索引或复合索引的第一项，否则在扫描前返回 `E_RELATION_KEY`。
- **类型**：两侧 key 必须是完全相同的类型；输出字段不能覆盖已有字段。错误分别使用 `E_TYPE` 和 `E_FIELD`。
- **显式上限**：`take` 必填且范围为 1..=1000。执行器读取至多 `take + 1` 个目标行；超限返回 `E_RELATION_LIMIT`，不会静默截断。
- **重复与顺序**：每个目标 RowId 最多出现一次，不做基于值的去重。首版嵌套 list 使用目标索引的稳定 traversal 顺序：单字段索引按 RowId，复合索引按剩余 component 再按 RowId；语法暂不接受目标侧自定义 sort。
- **driver 上限**：单个 lookup 最多处理 10,000 个输入行，并继续服从通用 working bytes、结果行和 deadline 预算。
- **一致性**：driver 与全部目标读取使用同一个 `TypedRowSource` 快照。嵌套 lookup 可逐层组合，但每层独立执行上述预算。
- **分页**：分页查询必须写成 `sort -> page -> lookup -> select`；`lookup` 不得位于 `page` 之前，`page` 之后只允许 `lookup` 与 `select`。cursor 仍由 driver 的唯一 sort tuple 决定，并绑定完整规范查询。
- **执行计划**：`explain` 的 `lookups` 列表公开 stage、两侧字段、目标索引与逐行上限；driver 的 `access` 仍只描述主查询访问路径。

### 5. 非目标

- 不做分布式、hash join、多写者或跨请求事务。
- 不提供会改变 driver 基数的扁平 inner/left/right/full join，也不在 mutation target 或 returning 内执行 lookup。
- 不承诺任意图遍历或大规模关联；主要依赖关联与 OLAP 的场景仍应选择 SQLite/DuckDB/PostgreSQL。

## English Description

### 1. Problem

unionid has no joins, subqueries, or cross-table relational reads. Common "parent + detail/reference/permission" shapes force two application-side queries or denormalized nested ADTs. The former makes the app guarantee a consistent snapshot, batching, and missing-value handling; the latter duplicates data and amplifies updates. This is the most frequent hard gap for ADT adopters.

A full SQL join brings a large semantic surface (direction, dedup, ordering, budgets) and high risk. This RFC freezes a **minimal, composable, explainable** relational read and splits it into independently verifiable slices.

### 2. First slice: indexed batch fetch

New Rust API (not query-language syntax):

```rust
let users = engine.fetch_by_key("users", "id", &[Value::Int(1), Value::Int(2)])?;
let typed = engine.typed_fetch_by_key::<User>("users", "id", &[Value::Int(1)])?;
```

- The result is the **same length and order** as the input `keys`; a missing key is `None`.
- The whole call runs against one committed snapshot (no concurrent writer on `&mut Engine`), so keys never observe different commits.
- Each key reads at most two rows: first `explain` confirms indexed access, then `filter key == $key | take 2` executes.

### 3. Semantics and bounds

- **Index precondition**: the equality plan for `key_column` must be `primary_key_lookup`, `secondary_index_lookup`, or `composite_lookup`; otherwise `E_RELATION_KEY` prevents per-key full scans.
- **Uniqueness**: a key matching more than one row returns `E_RELATION_NOT_UNIQUE`. Primary keys always satisfy this; a non-unique secondary index is checked at runtime with `take 2`, failing explicitly instead of silently taking the first row.
- **Bounds**: `keys.len()` is capped at `Engine::MAX_BATCH_KEYS` (10,000) with `E_LIMIT`; each key reads at most two rows and existing working/result budgets apply.
- **Errors**: binding/type/table/field errors reuse `E_TYPE`/`E_TABLE`/`E_FIELD`; typed decode reuses the `Value::to_serde` error path.
- **Read-only**: request atomicity is unchanged and no state is written.

### 4. Second slice: bounded lookup stage

The query language represents one-to-many reads as cardinality-preserving nested expansion:

```text
from orders
sort id
page 100
lookup lines from order_lines on order_id == id take 100
select {id, customer, lines}
```

The left side of `on` is a target-table field and the right side is a field on the current driver row. Every input row produces exactly one output row with a new `lines: list Line` field; no match produces `[]`. The result remains a product type without SQL-style flattened duplicates or `null`.

- **Index precondition**: the target field must be a primary key, secondary index, or the first component of a composite index; otherwise binding returns `E_RELATION_KEY` before scanning.
- **Types**: both keys must have exactly the same type, and the output field may not replace an existing field. Errors use `E_TYPE` and `E_FIELD` respectively.
- **Explicit bound**: `take` is required and must be in 1..=1000. Execution reads at most `take + 1` target rows; overflow returns `E_RELATION_LIMIT` instead of truncating.
- **Duplicates and order**: each target RowId appears at most once; equal row values are not deduplicated. The first version uses stable target-index traversal order: RowId for a single-component index, or the remaining components followed by RowId for a composite index. Target-side custom sort is not yet part of the syntax.
- **Driver bound**: one lookup processes at most 10,000 input rows and remains subject to the general working-byte, result-row, and deadline budgets.
- **Consistency**: the driver and all target reads use the same `TypedRowSource` snapshot. Nested lookup stages compose, with each layer enforcing the same budgets.
- **Pagination**: paginated queries use `sort -> page -> lookup -> select`. Lookup cannot precede page, and only lookup/select may follow page. The cursor remains defined by the driver's unique sort tuple and binds the complete canonical query.
- **Plan**: `explain.lookups` exposes each stage, both fields, the target index, and the per-row limit. The driver's `access` continues to describe only the main query path.

### 5. Non-goals

- No distribution, hash joins, multi-writer, or cross-request transactions.
- No flattened inner/left/right/full join that changes driver cardinality, and no lookup in mutation targets or returning.
- No arbitrary graph traversal or large-scale joins; workloads dominated by relational or OLAP queries should still choose SQLite/DuckDB/PostgreSQL.
