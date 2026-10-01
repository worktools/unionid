# RFC 0023：原子业务写入断言 / Atomic business-write guards

- 状态 / Status: implemented in v0.12 development; unavailable in released v0.11
- 日期 / Date: 2026-10-01
- 跟踪 / Tracking: [#399](https://github.com/worktools/unionid/issues/399), [v0.12 milestone](https://github.com/worktools/unionid/milestone/22)
- 相关 / Related: [RFC 0002](0002-idempotent-write-receipts.md), [RFC 0015](0015-static-query-contract.md), [RFC 0020](0020-inline-rust-query-macros.md)

## 中文说明

### 1. 业务问题

请求级原子性只保证脚本里的写入一起提交或一起回滚，不保证每条条件 update 都命中。余额不足的扣款命中零行后，后续入账仍可能成功。客户端在脚本提交后检查最后一条 affected_rows，无法撤销这个错误业务效果；先读余额再写也存在竞态。

v0.12 聚焦两项：在同一候选状态中检查 mutation 的 affected_rows，失败中止整个请求；成功响应提供有界、按源码顺序排列的逐语句摘要。零行 update/delete 本身仍合法，不隐式改变现有 DML 语义。实现与验证见 `tests/atomic_scripts.rs`、HTTP adapter 与宏 consumer 测试；不应将示例提供给已发布的 v0.11 执行。

### 2. 最小语法与作用域

增加独立语句 `expect affected == 1`，可使用 `==`、`!=`、`<`、`<=`、`>`、`>=` 与非负整数常量。首版不接受任意 bool 表达式、参数、业务字段、函数或查询结果断言。

`expect` 必须紧邻一个 insert/insert many/upsert/upsert many/update/delete 语句。空行和注释不改变相邻关系；query、DDL、migration 或另一个 expect 会打断关系。无前置 mutation 的 expect 在执行任何行前返回 `E_EXPECTATION_CONTEXT`，避免误用陈旧 affected 值。affected 使用前一条 mutation 的实际 affected_rows；upsert 的 replace 仍计为命中一行，批量写入按已有 DML 契约计数，不改为“值真的发生变化的行数”。

```text
update accounts
filter id == $sender && balance >= $amount
set balance = balance - $amount
expect affected == 1

update accounts
filter id == $recipient
set balance = balance + $amount
expect affected == 1

from accounts
filter id in [$sender, $recipient]
sort id
select {id, balance}
```

应用需先验证 amount 为正数并使用一致的 decimal 类型，拒绝不符合业务规则的发送人与收款人；expect 只证明更新行数，不证明完整财务规则。以上示例把两个期望都放在最后查询前，任何一个不满足都不会提交另一个写入。

乐观并发可以不先读取来锁定行，而是把已读取的 version 放入更新条件：

```text
update jobs
filter id == $id && version == $version && state == Pending
set {
  state = Running {worker: $worker}
  version = version + 1
}
expect affected == 1
```

花括号与 constructor 仍沿用当前 Rust 形状；不添加分号、命名事务块、交互式 transaction 或新的 pipeline operator。formatter 保持 expect 为清晰的独立行，input_status 和 query macro 的 token 重建必须识别其语句边界。

### 3. 原子失败与结果

执行器在同一 candidate 中执行 mutation，然后检查 expect；false 返回 `E_EXPECTATION`。提交、WAL 追加、回执保存、sequence 增长和 committed root 发布都必须发生在所有 expect 成功之后。失败时丢弃整段 candidate，不执行后续语句，主键/索引/RowId 分配状态、schema identity、cursor sequence 和已存在回执保持原样。新的幂等 key 不被失败占用。

错误增加可选 `statement_index`，从 **1** 开始，按顶层 AST 语句计数，expect 也占一个序号；pipeline stage 不单独计数。保留 span；错误消息不包含余额、参数值或业务 row。执行阶段的其他语句错误也应携带同一序号；未能形成 AST 的 parse 错误只保留 span，不伪造序号。确定性失败返回空 rows，不返回未提交语句的成功摘要。

成功的 `QueryResponse` 与 protocol v1/v2 `Response` 增加 `statements` 摘要数组，字段为 `index`、`kind` 和可选 `affected_rows`。只包含元数据，不复制每条语句的 rows、参数、表名或 returning 负载。expect 自身不带 affected_rows。顶层 rows/columns/affected_rows/upsert metadata 继续代表最后一个非 expect 语句；末尾 expect 对这些结果透明。最终 query 则仍按现有契约返回该 query 的结果，不把所有 mutation 的计数相加到顶层。

```json
{
  "ok": true,
  "rows": [],
  "affected_rows": 1,
  "statements": [
    {"index": 1, "kind": "update", "affected_rows": 1},
    {"index": 2, "kind": "expect"},
    {"index": 3, "kind": "update", "affected_rows": 1},
    {"index": 4, "kind": "expect"}
  ]
}
```

首版每个脚本最多 4,096 个顶层语句，包含 expect，解析/prepare 后、扫描和候选构造前以 `E_LIMIT` 拒绝超限，不截断摘要。此上限也约束本地 API/CLI，不能只依赖 TCP frame 限制；升级文档明确记录这个新增兼容边界，并验证大批量导入用 insert many 的替代路径。新增 Engine 侧摘要编码预算 512 KiB；用有界 writer 在 commit 前检查完整摘要，不在各 adapter 分别估算。摘要同时计入现有幂等 receipt 容量检查；现有 Engine returning 8 MiB 限制保留。上述 Engine/receipt 的提交前预算失败完整回滚。

现有 TCP 的 MAX_RESPONSE_BYTES（16 MiB）在提交后编码阶段检查，本文不把它追溯改为提交前保障。完整响应还可能因传输 envelope、最终 query 结果或断线而无法交付；这种传输失败不能被理解为 mutation 未提交。CLI、TCP 和 HTTP 文档需区分：E_EXPECTATION 与 Engine 提交前预算失败是确定回滚，响应交付失败需使用幂等 key/回执重试来判断效果。首版不承诺所有 transport E_LIMIT 都回滚，也不引入依赖 transport envelope 的 Engine 预检查。

### 4. 接口、持久化与兼容

- 本地 Engine、prepare/参数绑定、CLI、TCP/HTTP 共用同一 guard IR 和执行入口。guard 不是远端 adapter 才做的检查。只读边界继续拒绝含 mutation 的脚本；stream 和 page 的原有单条只读形状限制不放宽。
- 新 JSON 字段带 serde default，空摘要不序列化以保留旧回执的规范字节；旧客户端忽略未知字段；旧响应和旧回执缺少摘要时按空数组读取，不能凭空重建历史逐语句结果。Error/QueryResponse 的公开字段扩展需要在 Rust 升级说明中记录 literal struct 初始化的源码调整。
- 回执重放返回已保存的完整摘要，不重新执行 expect。请求 canonical digest 包含 expect 的比较符和常量；同 key 不同 guard 仍为 digest conflict。backup/restore、journal、restart 必须无损保留摘要。
- 先验证现有 receipt codec 的 versioned JSON payload 能保留可选字段；不能仅因网络 additive 就断言 durable 兼容。若任何 codec 会丢失摘要，先定义明确的升级契约，禁止静默丢字段或普通写入隐式升格式。
- `describe_query`、query rust 和 `queries!` 首版支持“一个 mutation + 一个尾随 expect”作为同一个 guarded operation，生成的 Output 仍使用该 mutation 的 returning 类型和 affected_rows。guard 纳入规范源码/digest。多 mutation 转账通过 Engine/prepare 与协议执行；静态 codegen 继续明确拒绝多 operation，不能只取最后一条生成代码。完整脚本静态绑定留给独立需求。
- CLI 错误分类、agent manifest、LLM query bundle 与 docs 分类同步增加稳定错误码、statement_index、合法位置和明确的 v0.12 能力边界。

### 5. 实施顺序与完成证据

1. parser/formatter/input_status、guard placement 与语句预算；错误/摘要的 serde 和协议兼容向量。
2. Engine candidate、prepared mutation、尾随输出透明性与全脚本回滚；memory/redb 和确定/不确定 commit 错误边界保留。
3. 幂等回执、codec/backup/journal 重开往返；守卫变化的 digest conflict 与失败后同 key 重试。
4. 单 mutation 静态描述/codegen/内联宏、CLI/TCP/HTTP 业务验收及文档。四阶段全部通过才关闭 #399。

验收覆盖：不足余额的第一步扣款、缺失收款人的第二步入账、成功转账、过期 version、零行合法无 guard、批量 guard 计数、尾随 returning、最终 query、非法/stale guard 上下文、超限脚本/摘要、receipt 容量失败，以及 guard 后的其他运行错误。每个提交前失败比较操作前后全部 typed rows、主键/二级索引、schema/sequence、RowId 分配和回执状态；持久库需关闭后重开、check 并核对 backup。Rust/CLI/TCP/HTTP 返回一致的错误 code、span 与 statement_index；并发两个 claim 仅有一个 guarded success。另外注入提交后响应编码/交付失败，验证回执重试返回已提交的完整摘要；不将它当成确定回滚。普通 PR 仍只跑 Ubuntu，发布前再运行完整双平台 gate。

## English Description

### Problem and scope

Atomic scripts commit their writes together, but a conditional update affecting zero rows is currently a successful operation. A later credit can therefore commit without a debit. Inspecting the last affected_rows after commit or reading a balance before writing does not provide a safe business guard.

Propose `expect affected == 1` as a separate statement immediately following an insert, bulk insert, upsert, bulk upsert, update, or delete. Blank lines and comments do not interrupt adjacency; a query, DDL, migration, or another expect does. Initially support the six integer comparison operators and nonnegative integer constants only. Invalid context fails with `E_EXPECTATION_CONTEXT` before any row execution. Use existing affected-row semantics, including replacements and batches; zero-row mutations without a guard remain valid.

The Chinese examples show guarded debit/credit and optimistic version updates. Applications still validate positive amounts, decimal types, and account rules. This feature proves mutation cardinality rather than complete financial correctness. It introduces no semicolons, transaction block, interactive transaction, or new pipeline operator.

### Atomicity and observable results

Evaluate the guard against the same candidate as its preceding mutation. False produces `E_EXPECTATION` and discards the entire candidate without running subsequent statements. No durable commit, WAL append, receipt publication, sequence advance, or committed-root update occurs before all guards pass. Rows, indexes, RowId allocation, schema identity, cursor sequence, and existing receipts remain unchanged; a failed request does not occupy its idempotency key.

Add an optional, one-based `statement_index` to execution errors, counting top-level AST statements including expect, with the existing span. Parse errors lacking a complete AST keep only their span. Failed scripts return no rows or uncommitted success summaries, and diagnostics do not expose row or parameter values.

Successful responses gain bounded `statements` entries with index, kind, and optional affected_rows, without intermediate business rows or returning payloads. Top-level output remains the last non-expect statement's output; a trailing guard is transparent. A final query remains the final query, and top-level counts are not summed across mutations. Limit: 4,096 top-level statements including guards, rejected with `E_LIMIT` before candidate construction or scanning, never silently truncated. The upgrading guide documents this compatibility boundary and batch-import alternatives. Add a transport-neutral Engine budget of 512 KiB for the complete encoded summary, checked with a bounded writer before commit. Include summaries in existing pre-commit receipt-capacity checks and retain the Engine's 8 MiB returning limit. These pre-commit failures roll back the candidate.

The current TCP 16 MiB MAX_RESPONSE_BYTES check occurs during post-commit encoding; this RFC does not redefine it as a pre-commit check. Full responses may still fail delivery because of transport envelopes, final query output, or disconnection. Distinguish deterministic E_EXPECTATION/Engine-budget rollback from post-commit delivery failures, whose effects must be resolved through idempotency-key/receipt retries. Do not promise rollback for every transport E_LIMIT or introduce transport-envelope-dependent Engine preflight.

### Integration and compatibility

Use one typed IR and executor for Engine, prepare, CLI, TCP, and HTTP. Retain read-only, streaming, and paging restrictions. Additive JSON fields use serde defaults and omit empty summaries to preserve old canonical receipt bytes; missing summaries in old responses/receipts mean an empty list, not reconstructed history. Document Rust literal-struct source adjustments.

Receipt replay returns the saved summary without reevaluating guards. Canonical digests include guard operators/constants, so changing a guard under the same key conflicts. Verify receipt codec, backup, journal, and restart preservation explicitly; do not assume network compatibility proves durable compatibility or silently upgrade storage.

Static descriptions, query rust, and inline macros initially accept one mutation plus its trailing guard as one guarded operation, retaining the mutation's typed Output. Multi-mutation business scripts use Engine/prepare and protocol requests; static codegen continues to reject multiple operations explicitly rather than generating only the final one. Update CLI diagnostics, agent capabilities, categorized docs, and LLM bundles with the version boundary.

### Delivery and acceptance

Deliver syntax/budgets and compatibility vectors, then candidate/prepared atomic execution, then durable receipts/backup/journal, then guarded single-operation codegen/macros and complete user documentation. Keep #399 open until every stage passes.

Exercise insufficient funds, missing recipients, successful transfers, stale versions, unguarded zero-row success, batch counts, trailing returning, final queries, invalid contexts, script/summary limits, receipt capacity, and later execution failures. For pre-commit failures compare complete typed rows, indexes, schema/sequence, allocation, and receipts before/after failures; reopen/check and verify backups in redb. Require matching Rust/CLI/TCP/HTTP diagnostics and exactly one successful concurrent guarded claim. Also inject post-commit encoding/delivery failures and require receipt retry to recover the committed summary instead of claiming rollback. Keep ordinary CI Ubuntu-only and run the full native-platform gate before release.

### 旧 WAL 边界 / Legacy WAL boundary

守卫仅用于 memory/redb。旧 `--wal-path` 以及 WAL + snapshot 模式在构造候选状态和追加日志前以 `E_CONFIG` 拒绝包含 expect 的脚本，提示使用 `--db`；避免成功写入无法重放的日志。已有无守卫 WAL 仍可恢复。

Guards require memory or redb. Legacy WAL and WAL + snapshot modes reject guarded scripts with E_CONFIG before candidate construction or logging, with a hint to use --db. This prevents committing an unreplayable log; existing unguarded WAL files remain recoverable.
