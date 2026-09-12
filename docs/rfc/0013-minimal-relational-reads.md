# RFC 0013：最小关联读 / minimal relational reads

- 状态 / Status: accepted, first slice implemented
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

### 4. 后续：有界 lookup join stage

在批量取回稳定后，增加查询语言层的关联读，任选其一或组合：

- `join` stage：单侧驱动行 + 另一侧主键/唯一索引点查，明确 join 方向、结果基数上限、去重规则与 sort/page 唯一性要求；只允许索引驱动，禁止 hash join 与全表笛卡尔积。
- 引用字段在 `select`/`returning` 中展开为嵌套 record（PostgREST/GraphQL 风格），底层仍是有界点查。

两者都必须：可 explain、受 working/result/内存预算约束、参与稳定 cursor 分页的主键收尾规则、不引入 null（缺失用 `option` 语义）。

### 5. 非目标

- 不做分布式、hash join、多写者或跨请求事务。
- 首个切片不改变查询语言语法；join stage 的语法与类型规则在后续 RFC/issue 中单独冻结。
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

### 4. Follow-up: bounded lookup-join stage

Once batch fetch is stable, add a query-language relational read, one or both of:

- a `join` stage: driving rows on one side plus PK/unique-index point lookups on the other, with explicit direction, cardinality caps, dedup rules, and stable sort/page ordering; index-driven only, no hash join or full cartesian product.
- reference fields expanded into nested records in `select`/`returning` (PostgREST/GraphQL style), still built on bounded point lookups.

Both must be explainable, bound by working/result/memory budgets, compatible with the stable-cursor primary-key ordering rule, and avoid null (missing uses `option` semantics).

### 5. Non-goals

- No distribution, hash joins, multi-writer, or cross-request transactions.
- The first slice does not change query-language syntax; the join stage's syntax and type rules are frozen in a later RFC/issue.
- No arbitrary graph traversal or large-scale joins; workloads dominated by relational or OLAP queries should still choose SQLite/DuckDB/PostgreSQL.
