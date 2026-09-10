# Schema migration 语言

本页描述当前可执行的 schema migration 语言和版本化 runner。每个 `.uid` 文件包含一个 migration block；文件按名称排序，`parent` 把它们连成不可分叉的单链。runner 计算规范化源码的 SHA-256 checksum，并把 ID、parent、checksum、应用时间和提交后的 schema revision/hash 持久化到 redb ledger。

storage format 6 使用可恢复的 shadow generation 应用单个文件。runner 保持 active generation 可读，以最多 1,024 条／16 MiB 的源批次转换数据，再用不超过 32 MiB 的事务持久化目标 rows、indexes 和 checkpoint。目标完成并通过有界完整性检查后，一个同步 two-phase transaction 原子切换 active generation、schema identity、sequence 和 ledger；随后分批回收旧 generation。多个待执行文件仍逐个切换：后续文件失败时，之前成功的文件保持已应用状态。没有隐式 down migration；回退通过新的前向 migration 或备份还原完成。

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
- text 导入生产标量必须显式使用 `uuid_parse`、`bytes_parse_hex`、`date_parse`、`timestamp_parse`、`duration_parse` 或 `decimal_parse old P S`；decimal 间改变 precision/scale 使用 `decimal_rescale old P S`。没有隐式 cast 或舍入；任一坏行或非精确 rescale 回滚 schema、全部 rows、indexes 与 ledger。
- 被其他类型或表直接／间接引用的 named type 不能删除。新增、删除或改名后的类型与变体仍遵守大写名称规则。

## 版本化 runner

默认目录是 `migrations`，也可以对每条命令传 `--dir`：

```text
unionid migration new add_task_priority
unionid migration diff --db app.redb --schema schema.uid --name add_task_priority
unionid migration plan --db app.redb
unionid migration apply --db app.redb
unionid migration advance --db app.redb --max-steps 1
unionid migration status --db app.redb
unionid migration abort --db app.redb
```

`new` 生成下一个带序号的 `.uid` 文件和正确 parent，并留下需要编辑的注释占位。保存至少一个 schema 操作后，文件才是有效 migration。例如：

```text
migration m0002_add_task_priority
  parent m0001_initial
  add field Task.priority int = 0
```

`plan` 验证 migration 链、schema 操作和目标 schema，并输出每个文件的 checksum、前后 revision/hash、操作列表与破坏性标记；它不修改数据库，数据库文件不存在时只在内存中按空库规划。format 6 的 plan 不扫描 durable rows，因此依赖既有值的 conversion、unique constraint 和 index 键错误会在 `apply` 构建 shadow generation 时报告。

`apply` 创建不存在的 redb 文件并逐文件推进。进程退出或确定的读错误会保留最后一个 durable checkpoint；使用同一文件再次运行 `apply` 会核对数据库身份、source/target schema、migration ID/parent/checksum 和 executor version，再从 checkpoint 后继续。任一身份不一致返回 `E_MAINTENANCE_CONFLICT`，不会覆盖 shadow 数据。转换、类型或约束错误会自动进入 abort cleanup；管理员也可以显式执行 `migration abort` 丢弃未切换的目标 generation。generation ID 单调分配，abort 后不会复用。

需要把长 migration 放入运维循环时，使用 `migration advance --max-steps N`。一个 step 对应一个已经成功提交的 generation start、row batch checkpoint、validation、cutover 或 reclaim batch；命令绝不会提交超过 N 个 step。JSON 输出是 `MigrationProgress`，包含本次 `committed_steps`、`complete`、本次切换的 migration，以及完整 `MigrationStatus`。使用完全相同的目录重复调用，直到 `complete = true`。因此进程调度、重启与故障注入可以依赖提交边界，不需要猜测毫秒耗时。嵌入式调用方使用 `Engine::advance_migrations(files, max_steps)` 获得相同语义；零 step 返回 `E_LIMIT`，非 format-6 redb 返回 `E_CONFIG`。

`Engine::apply_migrations_until` 仍用于 deadline。timeout 或内部 cancellation 在批次边界返回 `E_TIMEOUT`／`E_CANCELLED` 并保留 Building checkpoint，不会被当作确定的数据错误自动清理。普通 `apply` 仍一次推进到完成；若进程已在 cutover 后退出，再次 `apply` 或 `advance` 会先完成 Reclaimable cleanup。

`status` 展示完整已应用记录、待应用 ID，以及可选的 `maintenance`：phase、source/target generation、已读／已写 row 数、index entry 数、逻辑字节、更新时间和允许的下一步。`.storage` 与 version 1 introspection 返回同一维护信息。Building、Ready 或 Aborting 期间，旧 active generation 的查询和只读打开继续工作，普通 DDL/DML、receipt prune、storage upgrade 和另一条 migration 返回 `E_MAINTENANCE_REQUIRED`。Reclaimable 表示 cutover 已完成，只剩旧 generation 清理，不阻止普通写入。维护事务的 commit 若返回不确定结果，当前 Engine 禁止继续读写并要求重开，通过 `migration status` 判断是继续、清理还是已经切换。

`plan`、`apply`、`advance`、`status` 和 `abort` 都支持 `--format json`，可供脚本稳定解析。`abort` 是写操作，只适用于 redb format 6；只读实例返回 `E_READ_ONLY`。Building/Ready/Aborting 时它放弃并清理 target；Reclaimable 时只完成旧 source 的回收，已经切换的 migration 不会回滚。没有 maintenance 时执行 abort 是成功的幂等 no-op。

已应用文件不可修改，也不能从目录删除。CRLF 与 LF 具有相同 checksum，其他注释、格式和内容变化都会被拒绝。文件名排序必须与 parent 链一致，重复 ID、缺少 parent、分叉或 ledger 与当前 schema hash 不一致都会返回 `E_MIGRATION` 或 `E_STORAGE`。ledger 非空后，普通 `run`、本地 CLI 或 TCP 不能直接执行 schema 变更；应用必须经过 runner，数据读写仍可照常使用。

每个 shadow generation 的 catalog 加 rows 加 indexes 设有 1 GiB 逻辑上限；超过时返回 `E_MAINTENANCE_LIMIT` 并自动清理未切换的 target。执行仍需扫描全部受影响数据，耗时随数据量增长，但常驻转换状态受批次边界限制。schema diff 会列出受影响表的当前行数和索引数，并在新增 sum variant 时提示客户端穷尽 match 的兼容风险。

声明式目标 schema 的检查、规范输出、影响报告和 `migration diff` 草稿规则见 [声明式 Schema 与 Diff](SCHEMA-DIFF.md)。diff 不推断 rename 或转换；未决项会阻止草稿被 runner 解析。
