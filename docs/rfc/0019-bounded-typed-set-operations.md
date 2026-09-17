# RFC 0019: Bounded typed set operations

## 中文说明

### 状态与目标

已实现。目标是在一次 typed pipeline 中合并活跃/归档任务、多个事件来源等同形结果，同时保留 unionid 的 nominal ADT 身份、扫描前绑定和显式资源边界。

### 语法

```text
from active_tasks
select {state, tags}
union {
  from archived_tasks
  select {state, tags}
}
```

`union`、`intersect`、`except` 是当前 pipeline 的阻塞 stage。花括号是必要的层级边界：右侧从自己的 `from` 开始，之后可以继续在合并结果上执行 `sort`、`take`、`select` 等 stage，不使用分号。

### 类型与相等性

绑定器要求两侧字段数量、名称、顺序和类型完全一致。`Ref` 使用 stable type ID，因此两个结构相似但名字不同的 ADT 不兼容。执行器以现有 `Value::cmp_eq` 作为最终相等判定；哈希只缩小候选集合，碰撞仍逐值比较。浮点 `-0.0` 与 `0.0` 依照现有 equality 视为相等，enum、named value、record、tuple、Option 和 list 递归比较。

三种运算都返回 distinct 结果。`union` 保留左侧、再保留右侧的首次出现；`intersect` 和 `except` 保留左侧首次出现顺序。集合 stage 后的业务顺序由显式 `sort` 建立。

### 边界

- 首版拒绝嵌套 set operation，保持绑定、计划和内存模型单层可审计。
- 任一侧拒绝 `page`。cursor 绑定单表访问顺序和 commit sequence，不能冒充跨来源 cursor。
- 左右 materialized rows 的数量、encoded bytes 与 membership hash entry 估算合并计入现有 working-state 上限；最终结果继续受 result-row 上限约束。
- deadline/cancel 在右侧查询、membership 构建和结果选择中检查。
- `explain` 绑定两侧并返回右侧访问计划，不读取数据行；`explain analyze` 走普通执行器并合并两侧观测。

### 延后

不提供 `union all`、隐式 coercion、嵌套 set tree、跨来源 cursor、磁盘 spill 或分布式集合执行。真实调用方证明需要后再分别设计。

## English Description

### Status and goal

Implemented. The feature combines schema-identical sources such as active/archive tasks or multiple event feeds in one typed pipeline while preserving nominal ADT identity, bind-before-scan behavior, and explicit resource limits.

### Syntax

```text
from active_tasks
select {state, tags}
union {
  from archived_tasks
  select {state, tags}
}
```

`union`, `intersect`, and `except` are blocking stages. Braces form the necessary right-hand pipeline boundary. Later stages operate on the combined result, and no semicolons are introduced.

### Types and equality

The binder requires identical field count, names, order, and types. Stable type IDs make structurally similar but differently named ADTs incompatible. Runtime membership uses `Value::cmp_eq` as the final equality decision; hashing only narrows candidates and collisions still perform full value comparison. Following that equality contract, `-0.0` and `0.0` deduplicate as the same value. Nested enums, named values, records, tuples, options, and lists compare recursively.

All operators return distinct rows. `union` preserves first occurrence across the left then right side. `intersect` and `except` preserve first occurrence on the left. An explicit later `sort` establishes application ordering.

### Bounds

- The first version rejects nested set operations.
- Cursor `page` is rejected on either side because its single-source sequence contract cannot represent a cross-source traversal.
- Combined materialized row counts, encoded bytes, and estimated membership entries use the existing working-state limits; final rows use the normal result limit.
- Deadline and cancellation checkpoints cover right-side execution, membership construction, and output selection.
- `explain` binds both sides and reports the right-side access plan without reading rows; `explain analyze` uses the normal executor and merges observations.

### Deferred work

`union all`, implicit coercion, nested set trees, cross-source cursors, disk spilling, and distributed execution remain deferred until a concrete caller justifies them.
