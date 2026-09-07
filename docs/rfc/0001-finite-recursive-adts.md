# RFC 0001：有限自递归命名 ADT

状态：已实现的第一阶段，2026-09-07。父任务 [#25](https://github.com/worktools/unionid/issues/25)，实现任务 [#81](https://github.com/worktools/unionid/issues/81)。

## 决策

unionid 支持一个命名类型在定义体中直接引用自己，不增加 `rec`、分号或其他声明标记：

```text
type Tree =
  Leaf text
  | Branch
    label text
    children list Tree

type Chain =
  value int
  next option Chain = None
```

递归只描述有限树形值，不引入引用、指针身份、共享子树或循环对象图。直接和间接互递归、用户定义类型参数、递归函数、模式查询简写和高阶函数不属于本阶段。

## 实际价值

现有 v0.1 场景中的 list 和嵌套 record 只能表达预先固定的层数。直接自递归补齐以下小型应用状态，而不需要表 join 或动态 JSON：

- 文件／目录快照：`File metadata | Directory {children list Node}`。
- 评论或任务树：叶节点和带 `children list Item` 的分组节点。
- 规则与表达式 AST：`Literal | All (list Rule) | Not Rule`。
- 有限原因链：record 中的 `cause option Error`。

这些值通常整行读取、按构造器处理，并随所属对象原子更新，符合 unionid 的小工作集和弱关联定位。任意图、跨行边、图遍历和大规模层级分析仍应使用应用代码或更合适的数据库。

## 类型有效性

类型图中的循环必须存在至少一个有限值。Catalog 通过不动点计算验证：

- primitive 总是有限；
- `option T` 的 `None` 和 `list T` 的 `[]` 总是提供有限值；
- tuple/record 只有所有成员都有有限值时才有限；
- sum 只需一个变体的全部负载都有有限值；
- named ref 在目标命名类型已经证明有限后才有限。

有限性检查之前会遍历所有结构分支，确认每个 stable type ID 都存在；`option`/`list` 的空值只负责终止递归，不能掩盖损坏或未解析的引用。因此 `Next Chain | End`、`Leaf text | Branch (Tree, Tree)`、`option Chain` 和 `list Tree` 合法；`type Loop = Loop`、`type Endless = Next Endless` 和 `{next Required}` 被 `E_SCHEMA` 拒绝。失败属于完整请求的候选 catalog，不消耗可见 ID，也不发布部分 schema。

类型定义仍按原 version 1 顺序为结构字段和变体分配 ID，最后分配 type ID。解析定义体前先计算结构需要的 ID 数量，只临时预留最终 type ID 给自身 `Ref`；非递归 schema 的 ID 和 hash 不因此改变。

## 值与查询边界

递归值必须由普通 constructor、record、tuple、option 和 list 字面量完整构造。每次沿 named ref 或容器进入下一层都消耗现有深度预算；parser/type coercion 上限为 64 层，codec 上限为 64 层和 16 MiB，collection item 上限为 1,000,000。超限使用 `E_LIMIT`，原子写入不发布候选行。

`filter match` 和 `derive ... match` 继续使用同一 pattern IR。pattern 只按源码显式深度展开；通配或 binding 覆盖其余有限／递归子树。coverage matrix 在完整通配行处停止，并在构造缺失 witness 时优先选择终止分支，例如把缺少 `Next` 报为 `Next(End)`，避免沿递归变体无限展开。

本阶段不提供递归查询函数。深层遍历可在已知深度的 pattern 中完成；任意深度 fold/map 留在应用层，直到查询执行预算和返回语义有独立 RFC。

## 持久化、索引与演进

version 1 value codec 已按 stable `Ref` ID 和有限值递归编码，无需改变字节格式。schema manifest 保存引用 ID 而不展开定义，因此 hash、backup 和 redb catalog 也不会递归展开。

完整递归值可作为 typed equality 二级索引键；索引只支持精确相等，不增加 subtree 索引或递归路径。现有字段路径仍不能穿过 sum、option 或 list。

Migration 用有限性检查替代“拒绝所有 catalog cycle”：

- type/field/variant rename 保留稳定 ID，并递归更新有限值中的显示名称；
- add field/default、payload change 和 variant mapping 按现有 migration value depth 预算遍历每个实际值；
- 会移除最后终止构造、形成无有限值类型的 change 在扫描和提交前返回 `E_SCHEMA`；
- 任何值超过 migration 深度预算时整个 migration 回滚。

逻辑 backup/restore 和 redb reopen 重建同一 catalog、row、index 与 schema identity。载入持久 catalog 时也重新检查有限性；手工损坏形成的无终止循环以 `E_STORAGE` 拒绝。

## 备选方案

- 显式 `rec type`：能突出风险，但给现有 PRQL 风格声明增加无必要关键字；有限性检查已经能精确区分合法定义。
- 把树拆成邻接表：适合跨节点查询和大图，但把整行拥有的 ADT 状态变成 join，并失去 constructor 约束。
- 允许任意循环类型，只在写入时限制：会接受永远无法构造的 schema，并让 coverage、migration 和诊断更复杂。
- 同时实现互递归和泛型：需要批量预声明、参数化名义身份和实例化 codec 契约，审查面远大于当前直接自递归价值。

## 后续切片

1. 用真实 schema 判断是否需要同一声明批次内的互递归 SCC；若需要，设计两阶段 catalog 注册和失败 ID 规则。
2. 单独定义 `Result ok err`、`Tree item` 的类型参数身份、实例化、migration 和 schema hash，不能把内建 option/list 视为用户泛型。
3. 只有重复的实际查询证明价值后，才为 `filter case`／`filter_map` 选择一种能直接降为当前 match/filter/derive IR 的简写。

## English Description

unionid now permits a named type to reference itself directly with the existing semicolon-free syntax. Values remain finite trees: there are no references, object identity, shared nodes, or cyclic runtime graphs. The catalog accepts a recursive type only when a fixed-point inhabitation check proves at least one finite value. Primitive types are finite, option/list have empty terminating values, every product component must be finite, and a sum needs one finite variant. This accepts `Next Chain | End` and records with `option Chain`, while rejecting aliases or required-product cycles with no terminating constructor.

The implementation preserves the existing version-1 catalog allocation order by predicting body field/variant IDs and reserving the final type ID only during resolution. Existing non-recursive schemas retain their IDs and hashes. Runtime coercion, the value codec, and migration reuse the current depth, byte, and collection budgets. Match coverage stops at wildcard rows and prefers terminating variants when constructing witnesses, so recursive types do not create infinite analysis.

The version-1 codec already stores named `Ref` IDs and finite nested values, so no storage-format change is required. Exact equality indexes, redb reopen, schema diff, migration, and logical backup/restore use the same nominal identity. Migrations may preserve and transform recursive shapes as long as the resulting catalog remains finitely inhabited and every actual value stays within the migration depth budget.

Mutual recursion, user-defined type parameters, recursive query functions, match shorthand, and higher-order composition remain separate follow-ups. Combining them here would require batch catalog predeclaration, parameterized nominal identities, and new execution semantics without evidence that all of that complexity is needed together.
