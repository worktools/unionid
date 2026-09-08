# Schema migration 语言

本页描述当前可执行的 schema migration 语言和版本化 runner。每个 `.uid` 文件包含一个 migration block；文件按名称排序，`parent` 把它们连成不可分叉的单链。runner 计算规范化源码的 SHA-256 checksum，并把 ID、parent、checksum、应用时间和提交后的 schema revision/hash 持久化到 redb ledger。

单个文件中的 catalog、ADT 数据转换、约束、索引和 ledger 在同一个 redb 事务中提交。多个待执行文件逐个提交：后续文件失败时，之前成功的文件保持已应用状态，修正失败文件后再次运行会从 ledger head 继续。没有隐式 down migration；回退通过新的前向 migration 或备份还原完成。

## 语法

Migration 使用与类型和查询相同的无分号、缩进式语言：

```text
migration task_state_v2
  parent task_state_v1

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

add table table_name RowType
add table table_name RowType key field.path
drop table table_name
rename table old_name to new_name

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
add unique index table.field
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
- `using` 为了支持 `old.field` 会暴露 binding 最外层 record，但内部命名类型仍保留身份。例如给 `Retry` 增加默认字段后，外层 payload 转换可以写 `retry = old.retry`；结构相同的匿名 record 仍不能代替 `Retry`。
- `drop field` 明确丢弃该字段的数据。若主键或 secondary index 仍引用它，操作会失败；先显式 `drop key` 和 `drop index`。
- `set key` 在扫描全部行并确认 int/text 类型、字段存在且唯一后设置主键；缺少等值索引时自动创建。替换旧主键时保留旧索引，之后可显式删除。
- `drop key` 只移除唯一约束，保留可继续服务查询的索引。`drop index` 不允许直接删除仍承担主键约束的索引。
- `add unique index` 对 primitive、sum/product、tuple、option 与 list 的完整 typed value 施加唯一约束；`None` 不作例外。应用前扫描已有行，重复值使整个 migration 回滚。普通索引与 unique index 互换时，声明式 diff 生成先 drop、再 add 的显式步骤。
- 修改命名 ADT 会扫描所有表及其 record/tuple/list/option/sum 嵌套路径。所有稳定 RowId 均保留；受影响的 secondary indexes 在提交前重建并验证。
- 直接自递归命名 ADT 使用相同的全引用路径重写。rename、默认回填和 typed conversion 会遍历每个实际存在的有限值，并受 64 层 migration value 深度预算约束。删除最后一个终止变体或把类型改成无法构造有限值的循环会在提交前返回 `E_SCHEMA`，整个 migration 回滚。
- text 导入 UUID/bytes/temporal 必须显式使用纯函数 `uuid_parse old`、`bytes_parse_hex old`、`date_parse old`、`timestamp_parse old` 或 `duration_parse old`；没有隐式 cast。任一行解析失败返回 `E_SCALAR_LITERAL`，并回滚 schema、全部 rows 与 indexes。UUID 输出规范为小写 RFC 9562 文本，bytes parser 保存原 octets，timestamp parser 规范为 UTC 微秒。
- 被其他类型或表直接／间接引用的 named type 不能删除。新增、删除或改名后的类型与变体仍遵守大写名称规则。

## 版本化 runner

默认目录是 `migrations`，也可以对每条命令传 `--dir`：

```text
unionid migration new add_task_priority
unionid migration diff --db app.redb --schema schema.uid --name add_task_priority
unionid migration plan --db app.redb
unionid migration apply --db app.redb
unionid migration status --db app.redb
```

`new` 生成下一个带序号的 `.uid` 文件和正确 parent，并留下需要编辑的注释占位。保存至少一个 schema 操作后，文件才是有效 migration。例如：

```text
migration m0002_add_task_priority
  parent m0001_initial
  add field Task.priority int = 0
```

`plan` 在数据库副本上执行全部待应用转换，因此会提前发现类型、既有行、唯一约束和索引重建错误；它不提交数据，数据库文件不存在时也只在内存中按空库规划。输出包含每个文件的 checksum、前后 revision/hash、操作列表与破坏性标记。`apply` 创建不存在的 redb 文件，并逐文件原子提交。`status` 展示完整已应用记录和待应用 ID。三条命令都支持 `--format json`，可供脚本稳定解析。

已应用文件不可修改，也不能从目录删除。CRLF 与 LF 具有相同 checksum，其他注释、格式和内容变化都会被拒绝。文件名排序必须与 parent 链一致，重复 ID、缺少 parent、分叉或 ledger 与当前 schema hash 不一致都会返回 `E_MIGRATION` 或 `E_STORAGE`。ledger 非空后，普通 `run`、本地 CLI 或 TCP 不能直接执行 schema 变更；应用必须经过 runner，数据读写仍可照常使用。

当前 Engine 面向可装入内存的小工作集：plan 和 apply 都克隆候选数据库，schema step 在候选状态中验证和重写行。峰值内存和扫描时间仍与受影响数据量相关；schema diff 会列出受影响表的当前行数和索引数，并在新增 sum variant 时提示客户端穷尽 match 的兼容风险。

声明式目标 schema 的检查、规范输出、影响报告和 `migration diff` 草稿规则见 [声明式 Schema 与 Diff](SCHEMA-DIFF.md)。diff 不推断 rename 或转换；未决项会阻止草稿被 runner 解析。
