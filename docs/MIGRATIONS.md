# Schema migration 语言

本页描述当前可执行的 schema migration 语法。一个 `migration` block 是一个 schema 变更语句；它与同一 `Engine.execute` 请求中的其他语句一起原子执行。任何定义、数据转换、约束或持久提交失败都会保留变更前的 catalog、rows、indexes、revision 和 hash。

当前 migration 名称用于诊断，还不是持久历史 ID。版本化文件、checksum、`new/plan/apply/status` 和 redb ledger 由 #18 实现；在此之前不要把重复执行同名 block 当成幂等 apply。

## 语法

Migration 使用与类型和查询相同的无分号、缩进式语言：

```text
migration task_state_v2
  rename type State to JobState
  rename variant JobState.Failed to Rejected
  rename field Task.id to task_id
  add field Task.priority int = 0
  change variant JobState.Rejected to {code int, message text}
    using old -> {
      code = 500
      message = old.message
    }
```

当前支持的操作：

```text
add type Name = type-expression
drop type Name
rename type Old to New

add field Record.field type-expression = literal
drop field Record.field
rename field Record.old to new
change field Record.field to type-expression
  using old -> value-expression
change default Record.field to literal
drop default Record.field

add variant Sum.Variant
add variant Sum.Variant type-expression
add variant Sum.Variant (type-expression, type-expression)
add variant Sum.Variant {field type-expression, ...}
drop variant Sum.Variant
drop variant Sum.Variant
  using old -> Sum.Other
rename variant Sum.Old to New
change variant Sum.Variant to {field type-expression, ...}
  using old -> value-expression

add index table.field
drop index table.field
set key table.field
drop key table
```

`using old -> ...` 中的 binding 名可替换为其他小写标识符。结果使用与 `derive match` 相同的 typed value expression：可访问 binding 及其 record 字段，可使用数值表达式，并可构造 record、tuple、list、option、命名 record 和 sum 值。复杂结果可以放在括号或 record 花括号内换行。

`change field` 的结果类型是字段的新类型。`change variant` 的结果类型是变体的新 payload：一个参数直接返回该参数，多个或零个参数返回对应 tuple。`drop variant ... using` 的结果类型是删除后的完整 sum，因此必须构造仍存在的变体。没有数据使用被删变体时可以省略映射；存在数据时会返回 `E_MIGRATION`，并报告表、稳定 RowId、可用的主键和值路径。

## 身份、数据与约束

- `rename` 保留 type/field/variant 的稳定 ID。值按 ID 对齐后改用新名称，不按字段位置解释。
- `add field` 必须声明 typed literal 默认值。默认值会立即回填所有直接或嵌套引用该 record 的既有值；`option T` 也必须显式写 `None`。
- `change field` 和 `change variant` 保留目标成员 ID；field 原有默认值也通过同一个 `using` expression 转换。新旧 inline record 中同名字段保留 ID；需要改名时先写显式 `rename`，再写 `change`。
- `drop field` 明确丢弃该字段的数据。若主键或 secondary index 仍引用它，操作会失败；先显式 `drop key` 和 `drop index`。
- `set key` 在扫描全部行并确认 int/text 类型、字段存在且唯一后设置主键；缺少等值索引时自动创建。替换旧主键时保留旧索引，之后可显式删除。
- `drop key` 只移除唯一约束，保留可继续服务查询的索引。`drop index` 不允许直接删除仍承担主键约束的索引。
- 修改命名 ADT 会扫描所有表及其 record/tuple/list/option/sum 嵌套路径。所有稳定 RowId 均保留；受影响的 secondary indexes 在提交前重建并验证。
- 被其他类型或表直接／间接引用的 named type 不能删除。新增、删除或改名后的类型与变体仍遵守大写名称规则。

当前 Engine 面向可装入内存的小工作集：原子请求先克隆候选数据库，schema step 在候选状态中验证和重写行。它提供清楚的正确性边界，但峰值内存和扫描时间仍与受影响数据量相关。#18 的 `plan` 会在写入前报告受影响表、行数、索引和破坏性操作。
