# Schema migration 语言

本页描述当前可执行的 schema migration 语言和版本化 runner。每个 `.unid` 文件包含一个 migration block（兼容窗口内也接受 `.uid`）；文件按名称排序，`parent` 把它们连成不可分叉的单链。runner 计算规范化源码的 SHA-256 checksum，并把 ID、parent、checksum、应用时间和提交后的 schema revision/hash 持久化到 redb ledger。

storage format 6–11 使用可恢复的 shadow generation 应用单个文件。runner 保持 active generation 可读，以最多 1,024 条／16 MiB 的源批次转换数据，再用不超过 32 MiB 的事务持久化目标 rows、indexes 和 checkpoint。目标完成并通过有界完整性检查后，一个同步 two-phase transaction 原子切换 active generation、schema identity、sequence 和 ledger；随后分批回收旧 generation。多个待执行文件仍逐个切换：后续文件失败时，之前成功的文件保持已应用状态。没有隐式 down migration；回退通过新的前向 migration 或备份还原完成。

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
unionid migration diff --db app.redb --schema schema.unid --name add_task_priority
unionid migration plan --db app.redb
unionid migration apply --db app.redb
unionid migration advance --db app.redb --max-steps 8 --step-delay-ms 250
unionid migration status --db app.redb
unionid migration abort --db app.redb
```

`new` 生成下一个带序号的 `.unid` 文件和正确 parent，并留下需要编辑的注释占位。保存至少一个 schema 操作后，文件才是有效 migration。例如：

```text
migration m0002_add_task_priority
  parent m0001_initial
  add field Task.priority int = 0
```

`plan` 验证 migration 链、schema 操作和目标 schema，并输出每个文件的 checksum、前后 revision/hash、操作列表与破坏性标记；它不修改数据库，数据库文件不存在时只在内存中按空库规划。每个待执行文件还报告受影响的类型与表：对签名发生变化的类型列出引用它的表及当前行数、索引数（`impacts`），供估算维护范围。format 6–11 的 plan 不扫描 durable rows，因此依赖既有值的 conversion、unique constraint 和 index 键错误会在 `apply` 构建 shadow generation 时报告。

`apply` 创建不存在的 redb 文件并逐文件推进。进程退出或确定的读错误会保留最后一个 durable checkpoint；使用同一文件再次运行 `apply` 会核对数据库身份、source/target schema、migration ID/parent/checksum 和 executor version，再从 checkpoint 后继续。任一身份不一致返回 `E_MAINTENANCE_CONFLICT`，不会覆盖 shadow 数据。转换、类型或约束错误会自动进入 abort cleanup；管理员也可以显式执行 `migration abort` 丢弃未切换的目标 generation。generation ID 单调分配，abort 后不会复用。

需要把长 migration 放入运维循环时，使用 `migration advance --max-steps N`。一个 step 对应一个已经成功提交的 generation start、row batch checkpoint、validation、cutover 或 reclaim batch；命令绝不会提交超过 N 个 step。`--step-delay-ms M` 可在相邻提交之间暂停，控制持续 I/O/CPU 压力；上限为 60 秒，首个 step 不等待，最后一个 step 后也不等待。延迟会延长 maintenance 写阻塞窗口，不改变每个 checkpoint、cutover 或恢复边界，也不承诺吞吐率。

JSON 输出仍是 `MigrationProgress`，包含本次 `committed_steps`、`complete`、本次切换的 migration，以及完整 `MigrationStatus`。进程可在任一已提交 step 后终止，并用完全相同的目录继续调用，直到 `complete = true`。因此调度、人工暂停、重启与故障注入可以依赖提交边界，不需要猜测事务进度。嵌入式调用方使用 `Engine::advance_migrations(files, max_steps)` 获得相同的无延迟语义；应用需要限速时可每次推进一个 step 并自行调度。零 step 返回 `E_LIMIT`，非 format 6–11 redb 返回 `E_CONFIG`。

`Engine::apply_migrations_until` 仍用于 deadline。cutover 前的 timeout 或内部 cancellation 在批次边界返回 `E_TIMEOUT`／`E_CANCELLED`，并保留 Building checkpoint，不会被当作确定的数据错误自动清理。cutover 已提交后，deadline 若在 cleanup 期间到达，调用会保留 Reclaimable manifest；再次 `apply` 或 `advance` 会先完成旧 generation 的 cleanup。普通 `apply` 仍一次推进到完成。

`status` 展示完整已应用记录、待应用 ID，以及可选的 `maintenance`：phase、source/target generation、已读／已写 row 数、index entry 数、逻辑字节、更新时间和允许的下一步。`.storage` 与 version 1 introspection 返回同一维护信息。Building、Ready 或 Aborting 期间，旧 active generation 的查询和只读打开继续工作，普通 DDL/DML、receipt prune、storage upgrade 和另一条 migration 返回 `E_MAINTENANCE_REQUIRED`。Reclaimable 表示 cutover 已完成，只剩旧 generation 清理，不阻止普通写入。维护事务的 commit 若返回不确定结果，当前 Engine 禁止继续读写并要求重开，通过 `migration status` 判断是继续、清理还是已经切换。

`plan`、`apply`、`advance`、`status`、`rehearse` 和 `abort` 都支持 `--format json`，可供脚本稳定解析。`abort` 是写操作，只适用于带 generation 的 redb format 6–11；只读实例返回 `E_READ_ONLY`。Building/Ready/Aborting 时它放弃并清理 target；Reclaimable 时只完成旧 source 的回收，已经切换的 migration 不会回滚。没有 maintenance 时执行 abort 是成功的幂等 no-op。

`migration rehearse --db app.redb --dir migrations [--copy path]` 先把源库复制到临时路径（或 `--copy` 指定的路径），在副本上执行 apply 与完整 check，并输出源／目标 revision、applied/skipped、总耗时、文件字节、最后一个 shadow migration 的分阶段耗时／行数／索引数／逻辑字节，以及完整检查的 bounded working-state 峰值；源库保持不变。JSON 报告 `schema_version = 2`，`migration_profile` 在没有实际执行 shadow migration 时为 `null`。

这些字段是本次副本运行的实测观察，不是未来运行的 SLA。`migration_profile.logical_bytes` 表示目标 generation 的逻辑体积，`check_profile.working_peak_bytes` 只表示完整检查器的有界工作状态；两者都不是整个进程的 peak RSS，也不是 redb 物理写入字节。需要操作系统级峰值时使用 release evaluator。停机前应在具有相同 value 宽度和索引的生产数据副本上演练，确认 conversion、unique、index 键错误、耗时和磁盘增长，再规划生产窗口。

已应用文件不可修改，也不能从目录删除。CRLF 与 LF 具有相同 checksum，其他注释、格式和内容变化都会被拒绝。文件名排序必须与 parent 链一致，重复 ID、缺少 parent、分叉或 ledger 与当前 schema hash 不一致都会返回 `E_MIGRATION` 或 `E_STORAGE`。ledger 非空后，普通 `run`、本地 CLI 或 TCP 不能直接执行 schema 变更；应用必须经过 runner，数据读写仍可照常使用。

每个 shadow generation 的 catalog 加 rows 加 indexes 设有 1 GiB 逻辑上限；超过时返回 `E_MAINTENANCE_LIMIT` 并自动清理未切换的 target。执行仍需扫描全部受影响数据，耗时随数据量增长，但常驻转换状态受批次边界限制。schema diff 会列出受影响表的当前行数和索引数，并在新增 sum variant 时提示客户端穷尽 match 的兼容风险。

声明式目标 schema 的检查、规范输出、影响报告和 `migration diff` 草稿规则见 [声明式 Schema 与 Diff](SCHEMA-DIFF.md)。diff 不推断 rename 或转换；未决项会阻止草稿被 runner 解析。

## Rust 已保存查询预检 / Rust saved-query preflight

开发中的 v0.13 提供 `Engine::plan_migrations_with_queries` 与 `apply_migrations_with_queries`。输入为现有 `MigrationFile` 列表和 `migration::query_validation::MigrationQuery { path, source }` 列表；源码在目标 catalog 上通过同一个静态 binder 检查，不执行查询或扫描业务数据。当前已发布 v0.12 没有这些 API；开发分支 CLI 已通过 `--queries` 接入同一个预检。

plan 返回原有 plan 与 `query_validation`：`valid` 表示全部文件在最终 schema 上可绑定，`files` 按路径排序，`failures` 保留 current/各 migration checkpoint 的原始错误和 span。新查询可以在当前 schema 失败、在最终 schema 成功；中间失效但最终修复的轨迹保留。`parameters_changed`/`result_changed` 包含递归命名 ADT 的可达定义变化；`conditional` 表示新查询无有效 baseline 或契约变化需要复核，不代表旧生成客户端已兼容。

apply 在第一次 migration/maintenance 提交前重新进行预检，失败返回 `MigrationQueryError`（含原始 error 和可选完整报告）。同一个 Engine 持有数据库写入所有权，报告不能作为跨进程的“批准令牌”复用。校验成功后，实际数据转换仍逐 migration 文件提交，后续文件失败可能留下前面已提交的文件。无 pending 时仍会校验查询；Building/Ready/Aborting 使用 source catalog，Reclaimable 使用已经 cutover 的 target catalog，身份或文件 checksum 不一致时拒绝。

每次预检最多 1,024 个文件、每个文件沿用 1 MiB 源码限制、源码合计 16 MiB、65,536 次绑定、4,096 条诊断和 1 MiB 编码报告；超过预算明确返回 E_LIMIT。详见 [RFC 0024](rfc/0024-migration-query-preflight.md)。

Development toward v0.13 adds the two query-aware Engine methods with existing MigrationFile inputs and immutable MigrationQuery path/source pairs. They are unavailable in released v0.12; the development CLI exposes the same preflight through --queries. The shared static binder checks metadata without executing business operations or scanning rows.

The plan includes a versioned query_validation report: final validity, path-sorted files, current/migration checkpoint errors with spans, and parameter/result changes including reachable recursive named definitions. New queries may lack a valid baseline; repaired intermediate failures remain visible. Conditional compatibility requires review and does not relax exact generated-client schema hashes.

Apply repeats preflight under the same Engine's write ownership before any migration or maintenance commit, returning MigrationQueryError with its report on failure. Reports are not reusable approval tokens. Later data conversion retains per-file commits. No-pending calls still validate queries; active maintenance checks the actual source or cutover catalog identity and original migration checksum before proceeding. Source, bind, diagnostic, and encoded-report budgets fail explicitly; see the RFC for limits and the complete acceptance contract.

### CLI 查询目录 / CLI query directory

以下命令在开发中的 v0.13 使用；每个查询文件描述一个静态查询或 mutation（允许尾随 `expect`），不执行查询，也不需要提供运行参数：

```sh
unionid migration plan --db app.redb --dir migrations --queries queries --format json
unionid migration rehearse --db app.redb --dir migrations --queries queries --format json
unionid migration apply --db app.redb --dir migrations --queries queries --format json
```

`queries/` 必须存在且非空，递归加载 `.unid`（兼容 `.uid`，警告写入 stderr），按相对路径排序。拒绝符号链接、非源码条目和非 UTF-8 源码；目录深度最多 32，总条目最多 4,096（含空目录）。所有源码在预检前只加载一次。无待执行 migration 时仍检查。

成功 JSON 为原有报告增加 `query_validation`。失败只输出一个 `ok: false` 对象：最终查询无效时顶层 `error.code` 为 `E_MIGRATION`、退出码 3；具体错误、文件路径和 checkpoint 在 `query_validation.files[].failures`。I/O 错误退出码 5，资源限制退出码 3。预检成功不保证数据转换成功，后续 migration 文件失败仍可能留下先前已提交的 migration；根据 `query_validation.valid` 区分查询预检失败与后续执行失败。

文本默认逐文件显示最终状态，以及首次和最后一次失败的 migration/schema；超过两次失败时显示省略数量。`plan`、`apply` 和 `rehearse` 的 `--verbose` 展开所有 checkpoint 错误，例如 `unionid migration plan --db app.redb --dir migrations --verbose`。这只是展示选项：不减少绑定或检查，JSON 与 Rust version 1 报告始终保留完整轨迹，错误码和拒绝提交的规则不变。

`plan` 使用锁定源文件后创建的临时副本，避免 redb 打开时的恢复元数据写入源库。`rehearse` 同样保留源库字节；自动副本在成功或失败后清理。`--copy path` 只接受不存在的目标，失败或成功报告中的 `retained_copy` 表示本次创建并保留的副本，需检查其 schema/ledger 后再使用；已有文件及源库别名绝不覆盖。源库被占用时返回 `E_BUSY`（退出码 4）。`apply` 在同一个实际 Engine 中预检并提交；新库查询预检失败不会创建数据库文件。

These commands are available in development toward v0.13. Each file contains one statically describable query or mutation, optionally followed by `expect`; preflight does not execute queries or require runtime parameter values. The nonempty directory is recursively loaded once in relative-path order. `.uid` remains accepted with stderr warnings. Reject symlinks, non-source entries, and invalid UTF-8; cap directory depth at 32 and total entries, including empty directories, at 4,096. Check queries even when no migrations are pending.

Success adds `query_validation` to the existing report. Failure emits exactly one `ok: false` JSON envelope. Invalid final queries return `E_MIGRATION` and exit 3, with original file/checkpoint errors in `query_validation.files[].failures`; I/O returns exit 5 and resource limits exit 3. A valid preflight does not guarantee successful data conversion: later migration failures may retain earlier per-file commits. Use `query_validation.valid` to distinguish preflight rejection from later execution failure.

Default text shows each file's final status and its first/last failing migration and schema, with an omitted count when more than two checkpoints fail. Add `--verbose` to plan, apply or rehearse for every checkpoint error, for example `unionid migration plan --db app.redb --dir migrations --verbose`. This changes presentation only: all binding and checks still run, JSON/Rust version-1 reports retain the complete trace, and error codes and precommit rejection are unchanged.

Plan uses a locked temporary copy to avoid touching the source's redb recovery metadata. Rehearsal also preserves source bytes and removes automatic copies after success or failure. Explicit `--copy` destinations must not exist; `retained_copy` identifies a copy created and retained by this attempt, whose schema/ledger should be inspected before reuse. Existing files and source aliases are never overwritten. An active source owner returns `E_BUSY` with exit 4. Apply preflights and commits within the same actual Engine; rejected new-database queries create no database file.


## 默认项目查询预检 / Automatic project preflight (development)

开发版的 plan/apply/rehearse 在未传 --queries 时发现 migrations 实际目录同级的 queries/；存在 .unid 或兼容 .uid 源码时自动预检，源码只读取一次，在打开数据库之前固定。自动发现忽略普通非源码文件；没有源码时保留原执行路径。无法读取的目录和 symlink 仍报错。显式 --queries 指定目录覆盖发现路径，并要求至少一个查询。--no-queries 与 --queries 互斥，显式关闭时 stderr 输出提示，包括 JSON 模式；stdout 仍为单个 JSON 文档。已有项目因此可能在 apply 前收到新的查询契约错误，需修复查询或明确 opt out，不需要数据库格式升级。

Project plan/apply/rehearse automatically discover queries/ beside the resolved migrations directory when --queries is omitted. Sources are loaded once before database access; .unid and legacy .uid queries trigger preflight. Discovery ignores regular non-source files and preserves legacy execution when no sources exist. Unreadable directories and symlinks are rejected. Explicit --queries overrides discovery and requires a nonempty set. Mutually exclusive --no-queries explicitly disables checks with a stderr notice, including JSON mode; stdout stays one JSON document. Existing projects can now fail before apply with query-contract errors: fix queries or explicitly opt out. No storage upgrade is required.

init 的 README 和 scripts/check.sh 提供 project check → migration plan --queries queries 的 CI 顺序，部署 apply 也带显式目录；runner 需先安装项目使用的 CLI 版本。project check 先绑定全部 query，再报告格式差异，避免必要的 ADT/match 错误被排版提示遮住。

The starter README and scripts/check.sh provide project check → migration plan --queries queries for CI, with explicit directories for deployment apply. Install the project's CLI version on the runner first. Project check binds all queries before reporting layout differences, so ADT/match errors remain visible.

## Rust 精简预检报告 v2 / Compact Rust preflight reports v2 (development)

开发版新增 `Engine::plan_migrations_with_queries_v2` 和 `apply_migrations_with_queries_v2`；现有方法与 CLI 默认的 v1 输出保持原契约；plan/apply/rehearse 可加 `--query-report compact` 选择 v2，`--query-report full` 显式选择 v1。v2 把每个检查点的 migration ID、schema revision/hash 放在共享 `checkpoints` 数组中；每个文件的 `failures` 使用 inclusive 的 `first_checkpoint`/`last_checkpoint` 索引区间。只有相邻且完整错误相同的失败才合并；一次成功会断开区间，原始 code/message/span/hint 等仍保留。每个检查点都实际绑定，`valid` 始终表示最终目标的实际结果。

```rust
let planned = engine.plan_migrations_with_queries_v2(&migrations, &queries)?;
for file in &planned.query_validation.files {
    for failure in &file.failures {
        let first = &planned.query_validation.checkpoints[failure.first_checkpoint];
        let last = &planned.query_validation.checkpoints[failure.last_checkpoint];
        println!("{}: {:?} .. {:?}: {}", file.path,
            first.migration_id, last.migration_id, failure.error);
    }
}
```

v2 最多保留 4,096 个错误区间与 1 MiB 编码报告；1,024 文件、16 MiB 总源码、单源及 65,536 次绑定限制不变。重复错误不再按跨度消耗诊断预算，但不同错误、长路径或检查点元数据仍可能返回 `E_LIMIT`，没有隐式截断。无效最终查询以 `MigrationQueryErrorV2` 携带报告，在首次 migration/maintenance 提交前拒绝；后续数据转换仍按文件提交。CLI 默认仍为 full（v1），没有修改发布默认值或数据库格式；compact（v2）直接收集报告，不先构造 v1。`--verbose` 只影响文本展示，不改变 JSON 版本。

Development adds explicit version-2 Rust plan/apply methods; existing methods and default CLI reports retain v1. Plan/apply/rehearse accept --query-report compact for v2 or --query-report full for v1. A shared ordered checkpoint array stores migration IDs and schema identities. Each file records inclusive failure intervals using zero-based checkpoint indices. Only consecutive identical complete errors merge; success splits intervals. Every checkpoint is still bound, and final validity comes from the final actual bind.

V2 limits retained error intervals to 4,096 and encoded reports to 1 MiB, while preserving existing file/source/bind limits. Repeated failures consume one interval; distinct errors or large metadata can still fail with E_LIMIT without truncation. MigrationQueryErrorV2 preserves the compact report after final-query rejection or later conversion failure. Apply rejects invalid final schemas before any maintenance/migration commit; successful preflight retains per-file conversion commits. CLI still defaults to full/v1; compact/v2 collects directly without building v1 first. --verbose affects text only, leaving the selected JSON version unchanged. Release defaults and storage formats are unchanged.


```sh
unionid migration plan --db app.redb --dir migrations --query-report compact --format json
unionid migration rehearse --db app.redb --dir migrations --query-report compact
unionid migration apply --db app.redb --dir migrations --query-report compact
```

如果大量相同失败使 full 模式报告返回 `E_LIMIT`，可用 compact 保留完整的 checkpoint 语义并查看最终错误；不同错误或长元数据仍有预算保护。显式 `--no-queries` 与 `--query-report` 不能一起使用；没有已保存查询时选择报告模式不会创建查询或改变原有迁移行为。

When repetitive failures exhaust full-mode reports, compact retains every failed checkpoint through intervals and can return the final query errors. Distinct failures and large metadata remain bounded. Explicit --no-queries conflicts with --query-report. Selecting a report mode does not create queries or change legacy migration behavior when none are discovered.
