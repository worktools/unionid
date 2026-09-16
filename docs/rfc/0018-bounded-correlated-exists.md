# RFC 0018: Bounded correlated exists / 有界相关 exists

- Status / 状态: Accepted / 已接受
- Date / 日期: 2026-09-17
- Tracking / 跟踪: [#337](https://github.com/worktools/unionid/issues/337)
- Parent / 上级: [#244](https://github.com/worktools/unionid/issues/244)

## 中文说明

### 决策

v0.7 的第一个相关子查询只提供查询 stage：

```text
from tasks
filter exists {
  from task_items
  filter task_id == outer.id
  filter state != Done
}
```

内层是独立目标表上的只读 pipeline。普通路径属于目标表；`outer.<path>` 显式读取进入该 stage 时的外层行。首版只允许内层 `filter`，并要求至少一个类型一致的 `target.path == outer.path` 等值条件。该目标路径必须是主键或二级索引的首个 component；绑定在读取数据前完成，缺少关联条件、类型不一致、索引缺失或不支持的 stage 都直接失败。

执行器在每个外层行上把 `outer` 引用绑定成 typed value，通过目标索引执行内层 pipeline，并在第一条通过 residual filters 的行处停止。一个 exists stage 最多接受 10,000 个 driver rows；它沿用同一 committed snapshot、deadline、取消、解码和工作内存预算。`explain` 公开目标表、相关键、实际索引和 driver 上限，但不执行内层查询或暴露绑定值。

`exists` 是独立 filter stage。多个普通 filter 与 exists stage 按 pipeline 顺序组成 conjunction；首版不把 exists 放进任意 bool expression，也不支持 `not exists`、嵌套 exists、内层 derive/lookup/aggregate/sort/take/page/select、mutation target 或顶层集合运算。这些能力只有在真实调用方需要且能保持明确预算时再分别设计。

### 理由

任务/子项、订单/异常和文档/待处理修订都需要“是否至少有一行满足条件”，但不需要把关联行全部装入结果。现有 `lookup` 会返回有界 list，并在匹配超过显式上限时报错，不能可靠模拟 existence。索引前置、10,000 driver 上限和首行短路让该能力保持在 unionid 面向小型 typed 工作负载的资源模型内。

## English Description

### Decision

The first v0.7 correlated-subquery slice is a query stage:

```text
from tasks
filter exists {
  from task_items
  filter task_id == outer.id
  filter state != Done
}
```

The inner read-only pipeline targets a separate table. Ordinary paths belong to that target; `outer.<path>` explicitly reads the row entering the stage. The first version permits only inner `filter` stages and requires at least one type-compatible `target.path == outer.path` equality. The target path must lead a primary or secondary index. Binding rejects a missing correlation, incompatible types, a missing index, or unsupported stages before reading rows.

For each outer row, execution replaces `outer` references with typed values, runs the inner pipeline through the selected target index, and stops at the first row that passes residual filters. One exists stage accepts at most 10,000 driver rows and shares the committed snapshot, deadline, cancellation, decoding, and working-memory budgets. `explain` reports the target table, correlations, selected index, and driver limit without executing the inner query or exposing values.

Exists remains a distinct filter stage. Multiple filters compose in pipeline order. This version excludes arbitrary boolean nesting, `not exists`, nested exists, inner derive/lookup/aggregate/sort/take/page/select, mutation targets, and top-level set operations. Those require separate caller-driven slices with explicit budgets.

### Rationale

Task/child, order/exception, and document/pending-revision workflows need to know whether at least one related row qualifies without returning all related rows. Existing `lookup` produces a bounded list and fails when matches exceed its declared limit, so it cannot model existence reliably. An indexed correlation, a 10,000-driver bound, and first-match termination keep this feature inside unionid's resource model for small typed workloads.
