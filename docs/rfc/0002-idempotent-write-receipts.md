# RFC 0002：持久幂等写入回执

状态：已接受并由 #125/#126 实现。日期：2026-09-07。父任务 [#113](https://github.com/worktools/unionid/issues/113)，设计任务 [#124](https://github.com/worktools/unionid/issues/124)，实现任务 [#125](https://github.com/worktools/unionid/issues/125) 与 [#126](https://github.com/worktools/unionid/issues/126)。

## 决策摘要

unionid 将为 version 1 mutation request 增加独立的 `idempotency_key`。`request_id` 继续只做 tracing/correlation；重复 request 可以使用新的 `request_id`，但必须复用同一个 idempotency key 和完全相同的规范请求摘要。

首次成功 mutation 会在同一个提交中原子保存数据效果和回执。相同 key 与 digest 的后续请求不重新执行，而是返回原 QueryResponse，并标记 `replayed = true`；相同 key 与不同 digest 返回 `E_IDEMPOTENCY_CONFLICT`。这保证 exactly-once effect，不保证 response 只发送一次，也不保证客户端只收到一次。

普通执行失败不保存回执。commit 结果不确定时，客户端必须关闭连接／重新打开数据库后用同一个 key 重试：重开后只能观察到“效果与回执都不存在”或“效果与回执都存在”，不能只出现其中一半。

## 请求资格与状态机

idempotency key 只允许用于至少包含一条 mutation 的 version 1 query request：

- insert/upsert/update/delete、DDL，以及包含这些语句的混合原子脚本可以使用 key；
- read pipeline、`explain` 和 introspection 携带 key 返回 `E_IDEMPOTENCY_NOT_MUTATION`；
- 空 query、无效源码、无效参数和 schema precondition 失败不会占用 key；
- migration runner 已由 immutable checksum 与 ledger 提供独立幂等语义，不复用 request receipt；
- transitional WAL/snapshot 不接入该能力，带 key 的请求返回 `E_CONFIG`；正式持久入口是 redb。

Engine 对一个 key 执行以下状态机：

1. 校验 key 长度、协议结构并计算 digest。
2. 查找现有 receipt：
   - key 与 digest 都相同：返回保存的响应，不解析、绑定或执行当前数据库状态；
   - key 相同而 digest 不同：返回 `E_IDEMPOTENCY_CONFLICT`，不扫描或修改数据；
   - key 不存在：继续正常 parse、bind、schema check 和 mutation validation。
3. 普通错误直接返回，不保存 key。
4. 成功生成候选状态和完整响应后，先检查 receipt 大小与总容量，再把候选数据库和 receipt 放入同一个 commit。
5. commit 成功后发布内存状态与 receipt；commit 前确定失败两者都不发布；commit 返回不确定时关闭 durable handle，要求重开并用相同 key 重试。

read-only Engine 仍优先执行正常语法／参数验证，随后返回 `E_READ_ONLY`，且不保存 receipt。未来并发读快照不能绕过上述原子查找与提交；同一个 key 的并发首次请求必须由持久事务串行化。

## 规范 request digest

digest 为 `sha256:<lowercase hex>`。它基于一个内部 canonical document，而不是客户端 JSON 的空格或 object field 顺序。canonical document 固定为以下四个按字典序排列的字段：

```json
{"params":{},"query":"insert tasks {id = 1}","schema":null,"version":"1"}
```

规则如下：

- `version` 是十进制 string；
- `query` 是请求中的精确 UTF-8 源码，不运行 formatter，不忽略注释或空白；
- `params` 的参数名以及 record field 按 UTF-8 byte 字典序排列；
- `WireValue` 保持现有显式 tag，object key 使用协议定义顺序，string 使用 JSON UTF-8 最短转义；int/float/type ID/variant ID string 保持请求中的原始文本，因此不同数值拼写保守地视为不同请求；
- `schema` 为 `null`，或 `{"hash":"...","revision":"<decimal>"}`；revision 在 canonical document 中转为 string；
- `request_id`、`idempotency_key`、HTTP header、TCP connection 和 deadline 不进入 digest。

上述示例的 UTF-8 bytes 产生固定向量：

```text
sha256:b96dd4dc743653cc683dde69fb4133249107a2429c6b5f58b0dd9d228747b2e4
```

选择精确 query source 而不是 formatter 输出，是为了避免 formatter 版本升级改变旧请求的重放身份。选择 wire-level parameter 形态而不是绑定后的 Value，是为了让 schema 已变化时仍能在绑定前识别并重放原回执。保守冲突不会重复 effect；过度语义归一化则可能错误地把调用方认为不同的请求合并。

## 回执与响应

持久 receipt value 使用独立 codec version，至少包含：

```text
digest
committed_sequence
completed_at_unix_ms
original QueryResponse
```

key 是 receipt table 的 key，不在 value 中重复保存。保存的是 transport-neutral QueryResponse，而不是包含旧 `request_id` 的 ProtocolResponse。首次和重放响应都使用本次调用的 `request_id`，并附加：

```json
{
  "idempotency": {
    "key": "create-task-42",
    "digest": "sha256:...",
    "replayed": false,
    "committed_sequence": "17"
  }
}
```

重放时只有 `request_id` 和 `idempotency.replayed` 改变；`ok/message/columns/rows/error/warnings/schema/affected_rows/upsert_action(s)` 来自原回执。即使当前 schema 已变化，也返回原响应及其原 schema identity，因为重新绑定或重新执行会破坏 exactly-once effect。应用必须按回执中的 schema 解码旧 typed rows。

只有成功提交的 mutation 保存 receipt。语法错误、参数错误、constraint error、timeout、`E_READ_ONLY`、确定 storage failure 和 response/receipt limit error 都不保存；调用方修复请求后可以复用该 key。不同内容与已有成功 receipt 冲突时，错误只返回既有 digest 和新 digest，不回显原 query 或参数。

## 资源与清理边界

v1 固定以下默认边界：

| 资源 | 边界 | 行为 |
| --- | --- | --- |
| idempotency key | 1–256 UTF-8 bytes | 空 key 或超限返回 `E_IDEMPOTENCY_KEY` |
| 单 receipt encoded value | 1 MiB | 在数据提交前返回 `E_IDEMPOTENCY_LIMIT` |
| receipt 数量 | 10,000 | 新 key 返回 `E_IDEMPOTENCY_CAPACITY`；既有 key replay 仍可用 |
| receipt encoded 总量 | 64 MiB | 新 key 返回 `E_IDEMPOTENCY_CAPACITY`；既有 key replay 仍可用 |

这些限制只约束带 key 的请求；不带 key 的 mutation 继续遵守现有边界。容量检查必须发生在 candidate commit 前，不能出现数据已提交但 receipt 因过大而丢失。

系统不按墙钟自动 TTL 删除 receipt。隐式过期会使一个延迟重试重新执行，与 exactly-once effect 的直觉冲突，也会把正确性依赖机器时钟。#126 将提供可观察的 status/plan/prune 操作：默认只预览，显式确认后按 `completed_at_unix_ms` 和稳定提交 sequence 删除。prune 本身是原子持久操作。

receipt 被显式清理后，同一个 key 会被视为新请求并可能再次产生 effect。操作方必须让保留窗口长于所有客户端、队列和人工重放的最大重试窗口；备份恢复后也必须保留这一约束。容量不足时应扩容/归档或明确 prune，不得静默淘汰最老 key。

## redb、格式与备份

redb 增加独立 receipt table，table key 为原始 UTF-8 key bytes，value 带 magic、codec version 和上述 receipt payload。打开和 `check --db` 必须验证：

- key 长度与 UTF-8；
- codec magic/version；
- digest 形态；
- response 可解码且 `ok = true`；
- receipt sequence 不大于数据库 commit sequence；
- count 和 encoded byte 总量没有超过支持边界。

首次成功持久 receipt 会把 storage format 从 1 原子升级为 2；新二进制可读取 format 1（无 receipt）和 format 2，旧 v0.1.0 必须拒绝 format 2，不能忽略 receipt 后执行重复 mutation。用户通过发送第一个 durable idempotency key 明确选择这次升级。发布说明和 `check` 输出必须提示不可降级边界。

逻辑 backup 必须包含 receipts，并把 backup format 提升到 2；新实现继续读取无 receipt 的 backup v1。旧二进制必须拒绝 backup v2，防止 restore 时静默丢失重试身份。restore 后相同 key/digest 必须 replay，不能重新产生 effect。

memory Engine 使用相同 key、digest、响应、容量和清理语义，但 receipt 只存在于该 Engine 生命周期。memory response 必须明确标记 durability 为 `process_local`；redb 标记为 `durable`。调用方不能把 memory 重启后的 key 当作 exactly-once 保证。

## 错误码

| Code | 含义 |
| --- | --- |
| `E_IDEMPOTENCY_KEY` | key 为空、不是有效 UTF-8（transport decoder 阶段）或超过 256 bytes |
| `E_IDEMPOTENCY_NOT_MUTATION` | read/explain/introspection 使用了 key |
| `E_IDEMPOTENCY_CONFLICT` | 已存在 key 的 digest 与当前请求不同 |
| `E_IDEMPOTENCY_LIMIT` | 单个成功 response 无法放入 1 MiB receipt |
| `E_IDEMPOTENCY_CAPACITY` | 新 receipt 会超过 10,000 条或 64 MiB 总量 |
| `E_STORAGE` | receipt table/codec/integrity 损坏，或提交确定／不确定失败 |

错误响应继续使用本次 `request_id`。conflict 不返回原 payload；capacity error 应返回当前 count/bytes 与限制的结构化运维信息，但不会自动删除数据。

## 备选方案

- **复用 request ID**：会把 tracing 与重试生命周期耦合，代理或客户端为每次网络尝试生成新 ID 时失效，因此拒绝。
- **只保存 key，不保存 digest**：调用方误用 key 时会返回无关成功结果，无法发现业务错误，因此拒绝。
- **只保存 digest 和 affected rows**：`returning`、upsert action、schema identity 无法忠实重放，客户端仍需特殊恢复路径，因此保存完整有界 QueryResponse。
- **提交后再写 receipt**：崩溃窗口会产生效果但没有回执，重试会重复 effect，因此 receipt 必须进入同一 redb transaction。
- **自动 TTL/LRU**：在客户端仍可能重试时静默恢复 key 的可执行性，正确性取决于时钟与流量，因此只允许显式 prune。
- **把 query formatter 输出作为 digest**：formatter 演进会改变旧 key 身份，而且注释/布局是否属于调用方意图并不总是可推断，因此使用精确 source bytes。

## 实施顺序

1. #125 在 Engine/memory/redb 中实现 receipt 状态、format 2、backup v2、原子提交和故障矩阵；协议字段仍不在只有内存语义时提前开放。
2. #126 增加 version 1 request/response 字段、Rust builder、TCP/HTTP 丢响应场景，以及 status/plan/prune 运维入口。
3. #113 汇总检查 exactly-once effect、资源限制、升级兼容和完整文档后关闭。

## English Description

unionid will add a dedicated `idempotency_key` to version-1 mutation requests. `request_id` remains correlation-only. A retry may use a new request ID, but it must reuse the same idempotency key and the same canonical request digest. The first successful mutation atomically commits both its database effect and a transport-neutral QueryResponse receipt. A later matching request replays that response with the current request ID and `replayed = true`; another digest under the same key fails with `E_IDEMPOTENCY_CONFLICT`.

The guarantee is exactly-once effect, not exactly-once delivery. Ordinary failures do not consume a key. After an uncertain commit, the client must reopen and retry the same key; recovery exposes either neither effect nor receipt, or both. Read-only, invalid, timed-out, oversized, and constraint-failing requests do not create receipts. Migration files retain their separate checksum/ledger idempotency and transitional WAL mode does not support request receipts.

The SHA-256 digest covers a canonical document containing protocol version, exact query UTF-8, sorted typed wire parameters, and the optional schema precondition. It excludes request ID, idempotency key, transport metadata, and deadlines. Exact query bytes and wire spellings are intentionally conservative: formatting or numeric-spelling changes conflict instead of risking an accidental merge. The fixed no-parameter vector in this RFC allows independent implementations to verify canonicalization.

Receipts retain the digest, commit sequence, completion time, and complete successful QueryResponse. Replay preserves the original rows, actions, warnings, and schema identity even after the live schema changes; only the current request ID and replay marker differ. Keys are 1–256 UTF-8 bytes, each receipt is at most 1 MiB, and a database holds at most 10,000 receipts or 64 MiB. Existing-key replay remains available at capacity. There is no automatic TTL or LRU eviction: explicit preview-and-confirm pruning is required, and reusing a key after pruning may execute the effect again.

Durable receipts use a versioned redb table and commit in the same transaction as rows, indexes, schema, and ledger changes. The first durable receipt atomically moves storage format 1 to format 2 so older binaries cannot ignore receipts and repeat effects. Logical backups containing receipts use backup format 2; the new implementation continues to read receipt-free v1 backups. Memory mode provides identical process-local semantics but makes no restart guarantee. #125 implements the atomic state and format transition; #126 exposes protocol/Rust helpers, cleanup operations, and the HTTP lost-response journey.
