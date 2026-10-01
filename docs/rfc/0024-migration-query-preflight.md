# RFC 0024：迁移前检查已保存查询 / Saved-query preflight before migrations

- 状态 / Status: proposed; not implemented
- 日期 / Date: 2026-10-01
- 跟踪 / Tracking: [#400](https://github.com/worktools/unionid/issues/400), [v0.13 milestone](https://github.com/worktools/unionid/milestone/23)
- 相关 / Related: [RFC 0014](0014-portable-adt-contract.md), [RFC 0015](0015-static-query-contract.md)

## 中文说明

### 用户问题与入口

给 State 增加一个 variant 不破坏已有数据，却可能使原有 match 不再穷尽。迁移成功后才在业务查询中得到 E_MATCH，时机太晚。修复后的 project check 可以检查项目声明，但还需要从真实数据库 catalog 出发，把已有查询的有效性作为迁移前关卡。

```bash
unionid migration plan --db app.redb --dir migrations --queries queries --format json
unionid migration rehearse --db app.redb --dir migrations --queries queries --format json
unionid migration apply --db app.redb --dir migrations --queries queries --format json
```

不传 --queries 时保持已有接口与行为。传入时目录必须存在、包含至少一个查询源文件；不能把空目录的“零项通过”当作业务已检查。按相对路径稳定排序递归加载 .unid；.uid 使用既有过渡兼容规则和警告。拒绝 symlink 与非源文件，不忽略无法读取的文件。文件显示名保留相对路径，不扁平化成函数名，避免不同目录的同名文件冲突。

目录全部内容在首次校验前载入一次。首版最多 1,024 个源文件、目录深度 32、单文件沿用 MAX_SOURCE_BYTES、累计源码最多 16 MiB。额外限制一次预检最多 65,536 次 query/checkpoint bind，报告编码最多 1 MiB；执行前估算绑定次数，提交前检查报告大小。超过限制返回 E_LIMIT，不能先截断文件列表再声称通过。路径与摘要属于源码元数据，不输出业务 rows 或参数值。

### 校验对象与复用边界

使用真实 catalog lineage 的 metadata candidate，复用 plan_migrations 的 schema 演进顺序与稳定 ID；不把 schema print 后重新 parse 的 fresh IDs 当作当前数据库。静态源文件使用现有 describe_query，复用 prepare 的参数推断、类型绑定、穷尽性、不可达分支与静态操作结构检查。一条 DML 加尾随 expect 与既有静态查询契约一致；多步业务脚本不伪装成静态查询文件。

校验不执行 query、mutation 或参数，不扫描业务行，也不增加独立 binder。字段改名/删除、类型变化、variant 增加，以及仅在空表上也必须失败的 match 都走同一机制。query 可成功绑定只说明源码对目标 schema 有效，不证明运行时值、性能、权限、幂等容量或所有数据转换都成功。

### 最终目标关卡与定位

最终目标 schema 是硬关卡。任意文件在最终目标上失败，则 plan 返回非零，apply 在首次 maintenance action 或 migration commit 前拒绝；不能等 apply 提交完后再检查。rehearse 在副本上执行相同关卡，失败不改变源库。

同时记录当前 schema 和每个待应用 migration 后的校验结果，用于说明“哪个文件在什么 schema 上失效、哪次 migration 首次导致失败”。每项记录相对路径、checkpoint（current 或 migration ID）、schema revision/hash、compatible/incompatible，以及原 binder 错误 code/message/span/statement_index。新增查询允许在当前 schema 上失败、在最终 schema 上成功；中间失败后来恢复时明确展示该轨迹，但不把它冒充成最终失败。这个功能不保证逐 migration checkpoint 的在线服务兼容。

没有 pending migrations 时仍检查当前 schema，不能因为零次迁移就跳过 --queries。格式与 query binder 错误按文件列出，不在遇到第一个错误时隐藏其他文件；报告保存所有文件的最终状态。报告最多保留 4,096 条诊断，达到上限时返回 E_LIMIT，不能把截断报告作为完整成功证据。

版本化报告至少包含：version=1、current_schema、target_schema、checked_files、valid、每个文件的最终检查和按 checkpoint 的失败记录。成功绑定后记录参数、结果形状/cardinality 是否相对当前 schema 变化；变化必须标为需审查，不能把“能 bind”误写成旧客户端无需重建。baseline 本来无法绑定的查询没有可比较契约，报告说明该事实。

RFC 0014 的 schema query axis 是背景风险，不能替代具体文件的检查。比如增加 variant 使 schema 级 query 兼容性 conditional，但带通配分支的查询可以继续有效；穷尽旧 variant 的查询必须报告具体 E_MATCH。旧生成客户端仍检查精确 schema hash（#406 保留独立 backlog），本功能不改变该边界。

### apply 与 rehearse 的失败保证

为嵌入式调用方提供与 CLI 共用的 query-aware plan/apply API；原有 API 不强制要求 query 文件。query-aware apply 用同一个持有写入所有权的 Engine 进行 candidate preflight 与后续提交，不能先关闭/重开数据库或在另一副本上通过检查后跳过本次 catalog 校验。query 源码使用已经载入的不可变列表，不在提交中重新读取文件。

预检失败发生在 maintenance 清理、shadow generation 建立、cutover、ledger/sequence 改变之前。测试比较 rows、schema identity、ledger、receipt/sequence、索引与维护状态，重开后再次核验。正在进行的 maintenance 必须显式识别实际 source/target；不能把 Building candidate 或已 cutover 状态当作原 catalog。无法确定一致目标时拒绝执行并给 status/advance/abort 指引，不自行 abort。

预检通过后，既有“每个 migration 文件独立提交”的数据转换契约仍然存在；后续数据转换失败可能留下先前已经提交的文件。报告应区分 query 预检拒绝和实际 apply 错误，不声称整个 migration 目录原子执行。

rehearse 的 --copy 目标不得覆盖已有文件，也不得与源路径（含 symlink/hardlink 别名）相同。预检失败清理自动临时副本；显式保留副本按现有约定报告其状态，不删除用户已有文件。成功时仍运行真实转换和完整性检查。

### CLI、文档与兼容

JSON 每次命令输出一个对象；失败对象遵循既有 ok=false/error/exit_code envelope，并增加有版本的 query_validation 报告。不能先输出成功 plan 再输出错误 envelope。校验失败使用校验类非零退出码，错误顶层保持 E_MIGRATION，具体 E_MATCH/E_FIELD/E_TYPE 等保留在按文件的报告中；I/O 与预算错误保留各自分类。表格模式展示文件、migration checkpoint 与原因，不输出数据行。

成功的 --queries 命令为原有 plan/apply/rehearse 结果增加可选 query_validation；不使用该选项的 JSON 与旧调用方保持既有形态。无需 storage、backup、protocol codec 升级；本功能只影响显式的 migration tooling 与 Rust API。

实现后同步 migration 专题、CLI help、agent 能力清单和 LLM 可查询文档。版本使用指南只发布 Discussion，不在仓库保存副本。日常 PR 保持 Ubuntu CI；完整 macOS 验收仅在 v0.13 发布前运行。

### 验收与交付顺序

1. 共享 Engine candidate/schema/checkpoint 预检、类型化报告与预算，覆盖 enum 扩展、字段删除/改名、类型变化、未变查询、新增查询、无 pending、多个文件与重名路径。
2. CLI plan/apply/rehearse 的 --queries、单 JSON 对象与分类退出码，真实 redb 验证失败前后的完整状态；成功迁移与副本演练保持现有语义。
3. Rust 入口、参数化 mutation/guard、默认值、递归/嵌套 ADT、客户端结果/参数变化与运行中的 maintenance 回归；文档、agent 及真实项目旅程。

每阶段单独可审查；#400 只有全部验收后才能关闭。#402 与 #407 按同一 milestone 后续执行，不把引用完整性、标量函数或 rolling deployment 纳入此实现。

## English Description

### Problem and entry points

Adding an enum variant can preserve existing data while invalidating an exhaustive saved match. Detect this before deployment through an opt-in --queries directory on migration plan/apply/rehearse. Without the option, preserve existing commands and APIs.

Load a nonempty directory recursively in deterministic relative-path order. Accept .unid and the existing warned .uid transition. Reject symlinks, non-source entries, unreadable files, and empty input. Preserve relative paths rather than flatten names. Load sources once, with limits of 1,024 files, depth 32, the existing per-source byte limit, and 16 MiB aggregate source. Cap one preflight at 65,536 query/checkpoint binds and 1 MiB encoded reports. Check the planned bind count before binding and report size before committing. Never truncate silently.

### Shared validation and diagnostics

Reuse the same-lineage metadata candidate from migration planning and the existing static describe_query/prepare binder. Preserve stable IDs. Validate types, parameters, match coverage, and static operation structure without executing queries/DML or reading business rows. One mutation plus a trailing guard follows the existing static contract; arbitrary multi-write scripts are not static query files.

The final target is the hard gate. Any invalid final query makes plan fail and causes apply to reject before its first maintenance or migration commit. Also record current and migration checkpoints so errors identify relative query paths, schema identities, and the migration introducing a failure. New queries may be invalid currently but valid at the final target; intermediate failures repaired later remain visible without being treated as final failures. This is not a promise of online compatibility at each checkpoint. No-pending runs still validate current queries.

Return a version-1 report with current/target schemas, checked-file count, final validity, every file's final status, and checkpoint diagnostics containing original error code/message/span/statement index. Collect multiple failures rather than stop at the first file; cap diagnostics at 4,096 and fail explicitly on budget exhaustion. Record changes to inferred parameters/results/cardinality as review requirements; successful rebinding does not establish old-client compatibility. A missing valid baseline cannot yield a claimed contract comparison.

RFC 0014's schema query axis supplies background risk, not a substitute for actual binding. An enum addition may be conditional at schema level while a wildcard query stays valid and an exhaustive query fails. Exact client schema hashes remain unchanged; #406 stays separate.

### Failure boundary and integration

Expose shared query-aware Rust plan/apply APIs while retaining old APIs. Validate and commit with the same Engine holding write ownership and immutable loaded sources. Reject invalid queries before maintenance cleanup, generation creation, cutover, or ledger/sequence updates. Reopening must prove unchanged rows, identities, ledger, receipts, indexes, and maintenance state. Handle active maintenance explicitly; reject ambiguous targets with status/advance/abort guidance rather than silently aborting or pretending a generation is the committed catalog.

Once preflight passes, data conversion retains existing per-file commits. A later conversion failure may leave earlier migrations committed; this does not make a migration directory atomic. Rehearsal uses the same gate on its copy and preserves the source. Its copy destination must not overwrite an existing file or alias the source, including symlink/hardlink aliases. Clean automatic temporary copies on failure; preserve/report intentionally retained copies according to the documented behavior.

JSON emits exactly one object. Validation failure uses the existing ok=false/error/exit_code envelope plus a versioned query_validation report, a validation-class nonzero exit, and top-level E_MIGRATION; preserve underlying binder errors in file diagnostics. Preserve I/O/limit classifications. Successful option-enabled outputs add optional query_validation to the existing result; option-free JSON stays unchanged. Text output identifies files/checkpoints/reasons without business rows. No storage/backup/protocol codec change is required.

### Delivery and acceptance

First deliver shared Engine candidates, checkpoint checks, reports, and budgets with enum/field/type-change, unchanged/new queries, no-pending, multiple-file and duplicate-basename tests. Then wire CLI plan/apply/rehearse and prove failure-before-effects with real redb state comparisons and successful migration/copy journeys. Finally cover Rust integration, parameterized mutation guards, defaults, recursive/nested ADTs, parameter/result changes, active maintenance, docs/agent discovery, and a real project journey. Close #400 only after all stages pass. #402/#407 remain subsequent v0.13 tasks; do not absorb foreign keys, scalar functions, or rolling deployments. Publish the eventual user guide only in Discussions and keep macOS CI release-only.
