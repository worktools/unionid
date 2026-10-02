# RFC 0027：受控生成默认值 / Controlled generated defaults

- 状态 / Status: proposed; not implemented or available in published binaries
- Milestone: v0.15.0
- Tracking: [#404](https://github.com/worktools/unionid/issues/404)
- Storage design dependency: [#439](https://github.com/worktools/unionid/issues/439)

## 中文说明

### 范围与声明

让应用省略服务端生成的 ID、UUID 和创建时间。首版只支持表的顶层字段默认值，不引入查询中的可变函数、触发器或任意默认表达式。已有 struct 的常量默认值保持不变；生成策略属于 table，不改变可复用 ADT 本身。

下面是待实现的规范源码，不是当前可执行示例。调用括号区分受控生成器与常量字段默认值，兼容 Rust macro token；不添加分号。

```text
sequence account_ids {start 1}

struct Account {
  id: int
  public_id: uuid
  owner: text
  created_at: timestamp
}

table accounts: Account {
  key id
  default id = next(account_ids)
  default public_id = uuid_v7()
  default created_at = now()
}

insert accounts {owner: "Ada"}
returning {id, public_id, owner, created_at}
```

`sequence` 是命名、具有 stable ID 的独立 schema 对象；start 是 i64 字面量，默认 1，增量固定为 1，不支持 cycle、cache 或显式 reset。`next(seq)` 只接受已声明的 sequence 名，不接受参数或表达式。三个生成器分别产生 int、uuid、timestamp；允许同底层类型的 nominal 字段上下文初始化，不隐式转换为 text、Option 或其他类型。每个字段最多一个生成默认值，字段名在 schema 绑定阶段检查。

只有字段缺失才生成；显式值（包括合法的 None）不触发。表默认值优先于该字段的 struct 常量默认值。嵌套 ADT 的现有常量补齐规则不变；首版禁止在 list/map、enum payload 或嵌套路径安装生成策略。显式写入 ID 不会隐式推进 sequence，也不提供 max+1；给存量表配置 sequence 时应明确选定 start，碰撞仍由普通唯一约束拒绝。

### 求值、回滚与 replay

writer 获得最新 committed root、检查幂等回执并绑定整个请求后，才创建 request-local 生成上下文。prepare、explain、schema/source/diff、project check、read-only 拒绝和 replay 均不读取时钟/随机源、不消耗 sequence。绑定参数时保留缺失字段意图，不能提前物化为完整行；只有执行阶段的 typed row builder 可以生成。

同一原子脚本的全部 `now()` 返回同一个 UTC 微秒 timestamp，取样在执行阶段首次需要时间生成器时；只有 sequence 或所有字段均显式提供时不取样。它是墙钟取样，不是 commit timestamp，不能保证与提交顺序单调一致；业务排序使用现有 commit sequence。`uuid_v7()` 采用同一取样的 Unix 毫秒和每次调用独立的 74-bit 随机部分，遵循 [RFC 9562 §5.7](https://www.rfc-editor.org/rfc/rfc9562.html#section-5.7)；不承诺 UUID 排序反映行或提交顺序。UUID 唯一性仍需要 key/unique index。now 取样必须在 timestamp 支持的 0001–9999 年范围内，UUID v7 另要求非负且能装入 48-bit 的 Unix 毫秒。无效时间或随机源失败返回 E_GENERATION，sequence exhausted 返回 E_ARITH，均终止整请求。

分配顺序为脚本语句顺序、batch 输入顺序、row type 字段声明顺序；不依赖 HashMap 遍历、查询候选扫描或网络调度。sequence counter 在候选根内递增，行、索引、counter、receipt 和 journal 在同一持久事务提交。失败、取消、deadline、expect/unique/reference 拒绝均回滚全部分配。成功提交后的删除不回收号码；返回不确定 commit 错误时沿用关闭句柄、重开/check 的契约，不自动重新分配。

sequence 支持分配 i64::MAX 后持久化 exhausted 标记，下一次分配失败，不 wrap、不复用；“最后已分配值”和 exhausted 不计入 schema hash。没有要求成功事务外观察到的序列完全无缺口；已经提交后删除/prune 不逆转历史。

同 key/digest 的 receipt replay 返回首次响应、不求值。digest 由原始源码和绑定参数确定，不包含自动生成的值、时钟或随机源；这些结果作为 typed rows 和原 response 持久化。使用 returning 获取生成字段；没有 returning 时不额外扩展响应。失败未占用 key，重新执行可以得到新的时间/UUID，但不得留下失败尝试的 counter 变化。

### DML、migration 与恢复

insert / insert many 支持省略生成字段。upsert / upsert many 必须显式给出主键，避免生成 key 碰撞后意外替换已有行；其他缺失字段仍按完整行替换规则补默认值，不能暗中保留旧 created_at。update 不自动调用生成器，delete 不回退 counter。各入口（Engine、prepared、TCP/HTTP、Rust macro）共享同一规则；宏生成的输入契约应允许省略这些字段，输出是完整 typed row。

生成默认值的新增、删除和修改是 schema migration；sequence 支持 add/rename/drop，rename 保留 ID/counter，drop 在仍被默认值引用时拒绝。schema source/diff、portable 描述和 hash 包含 sequence 定义以及 table 字段到 generator/sequence stable ID 的绑定，不包含活动 counter。绑定和代码生成必须把生成策略纳入 schema drift 检查。

首版不在 migration backfill 中运行 now/uuid/next。给存量表增加必需字段时，用已有显式常量或确定性 using conversion 填充历史行，再设置仅供未来写入的默认策略；重复 plan/apply 和 shadow resume 不得重生旧行 ID 或时间。migration 创建 sequence 的 start 固定在源码中，counter 与 maintenance cutover 同步原子发布；abort 不保留目标 counter。shadow generation 按稳定对象 ID 携带已有 counter，不能从 max(row.id) 重建或在 reclaim 时删除活动对象。

逻辑 backup/restore 包含 sequence 定义、counter/exhausted、默认策略和首次成功 receipt。journal 记录物化行与 counter before/after，而不是生成器调用；增量 restore、重开、check 和 compaction 从不读取外部时钟或随机源。校验引用、counter 范围和生成器结果类型；exhausted 状态必须合法。恢复只延续选定恢复点之前的分配，不承诺与原库分叉后的 ID/UUID 全局唯一。

持久化方案先对齐 #439 的基础格式 + required capability + 独立 journal 模型，不再临时新增 generated/journal 成对格式号。持久新能力必须有旧二进制拒绝、显式单向升级、codec/backup/journal 原子兼容方案，不能仅向现有 serde 元数据加 optional 字段让旧软件静默忽略。此 RFC 不新增格式号、不修改默认写入格式、不将 memory-only 实现当成完整交付；格式方案未冻结时只推进语法/类型与候选根，不宣称持久默认值可用。

### 分阶段验收

1. 语法与类型：parser/formatter、schema/source/diff、prepared/explain、只读无副作用；禁止非法字段、generator 类型和悬空 sequence。
2. 执行：两客户端串行分配，batch/脚本确定顺序，显式覆盖，i64 exhaustion，完整 ADT returning；失败、expect/unique/reference、deadline/cancel 回滚；prepare/replay 不消耗。
3. 持久与演进：#439 兼容方案验收后接通 counter 小写集、receipt/journal 原子提交，旧软件拒绝；migration add/rename/drop、shadow abort/resume、重开与 compaction 保留分配状态。
4. 用户旅程：缺字段写入 → returning → 丢响应/replay → 重开/check → migration → logical 与指定 sequence 的 incremental restore → 再分配。Rust macro、真实 TCP/HTTP、CLI 和内置 docs/agent 共用契约；正式使用指南仅发布 Discussions。

测试使用注入的私有 clock/entropy 接口验证精确结果和失败，不访问用户数据库；生产 API 不允许客户端指定服务端时钟/随机种子。#404 在上述完整验收和发布前保持开放。

## English Description

### Scope and syntax

Provide server-generated IDs, UUIDs and creation timestamps through top-level table-field defaults. Do not add volatile query functions, triggers or arbitrary default expressions. Existing struct constant defaults remain unchanged; generation belongs to the table rather than the reusable ADT.

The source example above is proposed syntax, not executable today. Parentheses distinguish controlled generator calls from constant defaults and work with Rust macro tokens; no semicolons are added. A named sequence is a separate stable-ID schema object, with an i64 literal start (default 1) and increment 1. No cycle, caching or reset. `next(seq)` accepts only a declared sequence identifier. The three generators produce int, uuid and timestamp, including contextual initialization of nominal fields with the same underlying type. Reject implicit text/Option conversion, duplicate defaults and invalid fields at schema binding.

Generate only missing fields. Explicit values, including valid None, win; table defaults precede struct constants. Keep existing nested constant completion, but disallow generated defaults inside paths, enum payloads, lists and maps in this first delivery. Explicit IDs never advance a sequence or invoke max+1; choose start deliberately for existing data and retain ordinary uniqueness checks.

### Execution and replay

Acquire the writer's latest committed root, check receipts and bind the complete request before creating its generation context. Prepare, explain, schema tooling, project checks, read-only rejection and replay neither read clock/entropy nor consume counters. Preserve omitted-field intent during parameter binding; materialize generated values only in the execution-time typed row builder.

Every now() in one atomic script returns the same UTC microsecond wall-clock sample taken on the first execution-time need for a temporal generator. Sequence-only requests and fully explicit fields do not sample the clock. It is not commit time and need not increase with commit order; use commit sequence for that ordering. UUID v7 uses that sample's Unix milliseconds and independent 74-bit randomness per call according to [RFC 9562 §5.7](https://www.rfc-editor.org/rfc/rfc9562.html#section-5.7). UUID order does not promise row/commit order; use key/unique constraints for uniqueness. Now requires timestamp years 0001–9999; UUID v7 additionally requires nonnegative Unix milliseconds fitting 48 bits. Invalid time/entropy returns E_GENERATION, while sequence exhaustion returns E_ARITH; both abort the request.

Allocate in statement order, batch input order and row-type field declaration order, independently of map iteration, query scans and networking. Counters live in the candidate root and commit atomically with rows, indexes, receipts and journal. Failures, cancellation, deadlines and expect/unique/reference violations roll back every allocation. Deletion after successful commit never recycles IDs. An uncertain commit follows the existing closed-handle/reopen/check contract without automatic regeneration.

Allow allocating i64::MAX once, then persist exhausted state; the next allocation fails without wrapping. Mutable last-allocated/exhausted state is excluded from schema hash. Do not promise gapless externally observed identifiers after committed deletions or pruning.

Same-key/digest replay returns the original response without evaluation. Canonical digest covers original source and bound parameters, not generated values, clock or entropy. Persist generated typed rows and the original response. Returning exposes generated fields; no extra response fields are added otherwise. Failed attempts do not claim the key or retain counter changes; their retry may use new time/UUID samples.

### DML, evolution and persistence

Insert and insert-many may omit generated fields. Upsert and upsert-many require an explicit primary key to prevent an accidental replacement following a generated-key collision. Other missing fields still use full-replacement defaults, without silently preserving old creation times. Update never calls generators and delete never rewinds counters. Engine, prepared, TCP/HTTP and Rust macros share these rules; generated input contracts permit omission while output remains a complete typed row.

Default changes are schema migrations. Add/rename/drop sequences; preserve identity/counter on rename and reject dropping a referenced sequence. Source/diff, portable schema, hash and schema-drift checks include definitions and stable-ID default bindings, excluding mutable counters.

Do not execute generators during migration backfill. Populate new required historical fields with existing constants or deterministic using conversions, then attach defaults for future writes. Repeated plan/apply and shadow resume must not regenerate historical IDs/times. Sequence start is fixed in migration source; publish target counters atomically at cutover and discard them on abort. Carry existing counters by stable object ID through generations; never reconstruct them from maximum row IDs or reclaim active objects.

Logical backups preserve definitions, counters/exhaustion, defaults and successful receipts. Journal stores materialized rows and counter before/after states, never generator calls. Restore, reopen, check and compaction use no external clock/entropy. Validate references, ranges, result types and legal exhaustion. Restore continues the chosen recovery point's allocation state without promising global uniqueness across later database forks.

Align durability with #439's base-layout/required-capability/independent-journal design instead of adding another pair of feature/journal formats. Require old-binary rejection, explicit one-way upgrade and atomic codec/backup/journal compatibility; optional serde fields that old software ignores are insufficient. This RFC changes no format/default version. Syntax, binding and candidate-root work can proceed before storage design freezes, but memory-only support does not constitute complete delivery.

### Acceptance

1. Parser/formatter, schema/source/diff and prepared/explain enforce typing, reject dangling sequence/default bindings and prove read-only/tooling have no generation effects.
2. Controlled clients, batches and scripts prove allocation order, explicit override, exhaustion, ADT returning, rollback and no consumption during prepare/replay, including constraint/deadline/cancel failures.
3. After #439 compatibility acceptance, connect incremental counter writes and atomic receipt/journal commits, old-binary rejection, sequence migrations, shadow abort/resume, restart and compaction.
4. Exercise omitted-field insert → returning → lost response/replay → reopen/check → migration → logical and sequence-selected incremental restore → next allocation through macros, real TCP/HTTP and CLI. Bundle docs/agent contracts; publish release usage guides only in Discussions.

Private clock/entropy injection verifies exact results/failure without touching user databases. Public requests cannot set the server clock or entropy seed. Keep #404 open until full acceptance and release.
