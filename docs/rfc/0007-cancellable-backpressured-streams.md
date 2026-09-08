# RFC 0007：可取消、有背压的流式读取

状态：accepted design；implementation tracked by #156 and #157

## 1. 问题与边界

完整 JSON response 和稳定 cursor page 适合短查询与可恢复遍历，但不适合需要逐行消费、可能超过 16 MiB response 上限的长读取。当前客户端断开后，读取只能依靠 25 秒 deadline 最终收敛；关闭 socket 不是可认证、可观察的取消协议。

本 RFC 定义一个独立的 stream protocol version 1、进程内 operation registry、显式 cancel 和 NDJSON frame。它只服务单条只读 query pipeline，不改变现有 protocol version 1/2 `Request`/`Response`、cursor `u1` 或 mutation 语义。

以下能力不在范围内：mutation/returning stream、introspection、receipt maintenance、explain stream、跨节点取消、断线续传、持久 operation、TLS、用户认证与授权。内置 TCP 服务仍只面向受信本机应用；跨信任边界时，代理必须保护 stream 与 cancel 路由，并把 bearer capability 作为凭证处理。

## 2. Operation capability

服务在接纳每个 stream 时生成 128-bit CSPRNG 随机值，编码为无 padding base64url，并加 `o1.` 前缀。`operation_id` 是 bearer capability：

- 不能由客户端选择，不能由 `request_id`、query、时间或自增序号推导；
- 不写数据库、receipt、普通日志或 metrics label；
- 只在同一服务进程内有效，重启后全部变为 `unknown`；
- 持有者可以取消对应 operation，泄漏 token 等同泄漏取消权限；
- token 语法、版本或长度无效时返回 `E_OPERATION_ID`，但不说明 registry 内容。

生成冲突时重新生成；连续 8 次冲突或随机源失败返回 `E_INTERNAL`，不接纳读取。

`request_id` 仍只用于日志与响应关联，不能取消 operation。cancel 自己有独立 `request_id`。

## 3. 生命周期与线性化

Registry 在数据库读取排队之前建立 entry，因此 adapter 可以先把 operation ID 交给客户端：

```text
registered --accepted flush--> queued --read permit--> executing
     |                         |                      |
     +---------- cancel -------+----------------------+--> cancelled
                                                       
executing --snapshot released--> emitting --all frames--> completed
                      |               |
                      +---- error ----+---------------> failed
```

`registered` 尚未进入 read queue。TCP adapter 必须成功写出并 flush `accepted` frame 后才调用 start；HTTP adapter 在 response header 暴露同一 operation ID，并让 lazy body 先 yield accepted frame，下一次 poll 才 start。accepted 发布失败会撤销 entry，不捕获 snapshot。这里的“可见”是 adapter 已取得并发布 capability 的线性化边界；网络调度不保证客户端在 worker 获得 CPU 前完成读取。`queued` 受现有 read-slot、deadline 与 shutdown 控制。`executing` 包含 schema-aware bind、scan、sort、aggregate 和有总字节预算的 materialize。完整 typed result 建立后立即释放 snapshot，再进入 `emitting`；慢客户端不会延长 snapshot 生命周期。

Registry mutex 下的 terminal transition 是完成/取消竞态的唯一线性化点：

| cancel 观察到的状态 | cancel response | stream 结果 |
| --- | --- | --- |
| registered / queued / executing / emitting | `accepted` | operation 转为 `cancelled`；未发送完时以 `E_CANCELLED` terminal frame 结束 |
| completed / cancelled / failed tombstone | `already_terminal` + outcome | 既有结果不改变 |
| entry 不存在或 tombstone 已过期 | `unknown` | 不影响任何 operation |

如果 cancel 先取得 registry mutex，之后产生的完整 query result 被丢弃，不能再发送 `complete`。如果完成先发布 terminal state，cancel 返回 `already_terminal`。重复 cancel 在 terminal retention 窗口内稳定返回 `already_terminal/cancelled`。

断开连接可以作为本地资源优化触发同一个 cancellation signal，但客户端不能把断开当作已确认取消；只有 cancel response 或 stream terminal frame 是可观察确认。deadline、shutdown 与 cancel 同时发生时，最先在线性化点发布的 terminal outcome 胜出。

## 4. 有界 registry

- 最多 64 个非 terminal operation，包括 registered、queued、executing 和 emitting；超限返回 `E_OPERATION_CAPACITY`，不分配 read permit。
- terminal tombstone 最多 256 个，保留 60 秒；按 terminal sequence FIFO 淘汰。它只保存 operation ID、outcome 和完成时间，不保存 query、参数、row、schema 或错误正文。
- registry entry 保存 cancellation `AtomicBool`、状态、创建/完成时间和小型计数；不保存第二份数据库 snapshot。
- `ConcurrencyStats` 增加 registered/queued/executing/emitting、terminal cancellations 和上限的聚合计数，永不以 operation ID 作为 label。

容量限制独立于 64 TCP connections 和 8 concurrent read snapshots。HTTP 与 TCP 共用同一个 `ConcurrentEngine` 时也共用一个 registry，不能各自绕过上限。

## 5. Transport-neutral request

Streaming 使用独立 envelope，避免给现有、带 `deny_unknown_fields` 的完整响应协议偷偷增加模式：

```json
{
  "stream_version": 1,
  "operation": "query",
  "request": {
    "version": 2,
    "request_id": "export-42",
    "query": "from events\nsort id",
    "params": {}
  }
}
```

嵌套 `request.version` 继续决定 `WireValue` 能力。stream query 必须满足：

- 精确一条 query pipeline；不能是 DDL、DML、migration 或 explain；
- `introspect`、`receipts`、`idempotency_key` 和 `page` 均为空；
- schema precondition 与 typed params 继续复用现有 bind/validation；
- 在 registry/admission 前完成 envelope 大小、stream version、request ID、query parse 和只读 operation shape 检查；schema-aware bind 与执行在 operation 已可取消后进行。语法预检受现有 1 MiB/100,000-token/64-depth 和总 deadline 限制，但不分配 operation。

不合格请求在发送 `accepted` 前返回普通单行 stream error response，且不创建 operation。旧服务会把该 envelope 当作未知 JSON request 并返回现有 `E_PROTOCOL`，不会误执行 query。

```json
{"stream_version":1,"request_id":"export-42","ok":false,"error":{"code":"E_STREAM_SHAPE","message":"streaming requires exactly one read query"}}
```

Cancel 是短的单行 request/response。TCP 必须使用另一条连接，因为原 stream connection 正在输出；HTTP 使用独立 control request：

```json
{"stream_version":1,"operation":"cancel","request_id":"cancel-42","operation_id":"o1.ABC..."}
```

```json
{"stream_version":1,"request_id":"cancel-42","operation_id":"o1.ABC...","ok":true,"status":"accepted"}
```

`status` 是 `accepted`、`already_terminal` 或 `unknown`。`already_terminal` 额外返回 `outcome`：`completed`、`cancelled` 或 `failed`。`unknown` 使用成功 control response，避免把安全的重复清理变成 transport failure；无效 token syntax 仍是 `E_OPERATION_ID`。

## 6. NDJSON frame

成功接纳后，每行是一个完整 JSON object，`stream_version`、`request_id` 和 `operation_id` 在每个 frame 重复，方便独立校验与日志关联：

```json
{"stream_version":1,"frame":"accepted","request_id":"export-42","operation_id":"o1.ABC..."}
{"stream_version":1,"frame":"schema","request_id":"export-42","operation_id":"o1.ABC...","columns":[{"name":"id","type":"int"}],"schema":{"revision":4,"hash":"sha256:..."}}
{"stream_version":1,"frame":"row","request_id":"export-42","operation_id":"o1.ABC...","sequence":"0","row":{"id":{"type":"int","value":"1"}}}
{"stream_version":1,"frame":"complete","request_id":"export-42","operation_id":"o1.ABC...","row_count":"1","encoded_bytes":"612","warnings":[]}
```

Frame 顺序固定为：一个 `accepted`；成功执行后一个 `schema`；零到多条 `row`；最后恰好一个 `complete` 或 `error`。`sequence`、`row_count`、`encoded_bytes` 使用 base-10 string，避免跨语言整数宽度问题。`encoded_bytes` 统计 accepted、schema 与全部 row frame（包括换行），不包含 terminal frame，避免自引用计数。row object 与完整 response 使用同一 `WireValue` codec，columns 决定字段顺序。

执行或发送过程中失败时，在连接仍可写的前提下发送 terminal error：

```json
{"stream_version":1,"frame":"error","request_id":"export-42","operation_id":"o1.ABC...","emitted_rows":"17","error":{"code":"E_CANCELLED","message":"read operation cancelled"}}
```

`schema` 前失败时 `emitted_rows` 为 `0`。写入本身失败时无法承诺 terminal frame；服务仍必须在 registry 中发布 `cancelled` 或 `failed` 并释放资源。partial stream 从不包装成 `QueryResponse`，也不产生 cursor。

取消 emitting operation 时，producer 停止产生 frame，consumer 丢弃 channel 中尚未开始写的 schema/row/complete frame。已经开始写的一行不会被强制截断：adapter 尝试在 5 秒 write timeout 内完成该 frame，只有完整 JSON 加换行成功后才增加 `emitted_rows`，随后发送 `E_CANCELLED`。如果当前写失败或超时，连接可以留下 partial line，但 registry 仍收敛为 cancelled；cancel control response 不承诺 stream socket 仍可写。

## 7. 背压与资源预算

核心执行和 adapter 之间使用同时受 item 与 encoded-byte 限制的 channel：最多 8 个待发送 frame、合计最多 16 MiB。producer 在 channel 满时以最多 100 ms 的间隔检查 cancel、deadline 和 shutdown；不能无界累积 row。

| 资源 | version 1 边界 | 超限结果 |
| --- | ---: | --- |
| active registry entries | 64 | 接纳前 `E_OPERATION_CAPACITY` |
| concurrent read snapshots | 8 | 排队，受 cancel/deadline/shutdown 控制 |
| queued frames / bytes | 8 / 16 MiB | producer 背压等待 |
| one encoded frame | 16 MiB | terminal `E_STREAM_LIMIT` |
| emitted rows | 100,000 | 复用 query `E_LIMIT` |
| emitted bytes | 256 MiB | terminal `E_STREAM_LIMIT` |
| total operation deadline | 25 秒（adapter 可缩短） | `E_TIMEOUT` |
| blocked socket write | 5 秒 | 连接失败并取消 operation |
| terminal tombstones | 256 / 60 秒 | FIFO/TTL 淘汰后 cancel 为 `unknown` |

25 秒从完整 request frame 收到时开始，包含 accepted write、queue、execution、channel wait 和 socket writes。总 deadline 与 emitted-byte limit 意味着 stream 仍是有界读取，不是无限 subscription。

执行器把现有 deadline check 扩展为统一 `ExecutionControl`：`checkpoint()` 按 cancel → shutdown → deadline 顺序检查并返回 `E_CANCELLED`、`E_SHUTDOWN` 或 `E_TIMEOUT`。read admission 每 100 ms 检查一次；schema bind、query 的逐 stage、逐 row、sort/aggregate、result byte accounting 和 wire row 编码循环必须调用 checkpoint。不能只在线程开始与结束检查。materialize 在追加每行前累加其有界 wire-size estimate，超过 256 MiB 立即失败，不能先建立无界 `QueryResponse` 再发现 stream 超限；进入 emitting 后逐行 move/encode，不复制完整结果集。

## 8. Rust 实现边界

#156 提供 transport-neutral 核心：

- `ConcurrentEngine::register_read(...) -> OperationHandle`：完成 bounded parse/read-only eligibility、占用 registry entry、返回 capability 与 handle-owned parsed request，不启动读取；registry 本身不保存 query 或参数；
- `OperationHandle::start(...)`：在 accepted 已 flush 后进入 queue，并使用统一 execution control；
- `ConcurrentEngine::cancel(operation_id) -> CancelResult`：唯一 cancel 入口；
- RAII terminal guard：panic 之外的所有 return path 发布 terminal state并释放 permit/snapshot；worker panic 由 adapter catch/join 后发布 failed；
- 测试 hook 只在 crate tests 内控制 admission/checkpoint，不进入公共协议。

#157 提供共享 frame producer/encoder。TCP writer 和 HTTP body adapter 只消费同一 bounded receiver；不得各自把结果解释成另一套 row codec。HTTP async handler 必须把同步 query 放到 blocking worker，body backpressure 通过 receiver 传播，并在 response header 中复制 operation ID。adapter 发送 accepted 后 start handle；最终 rows 从有 256 MiB materialize 预算的 typed result 中逐行 move，而不是复制一份完整 response。

## 9. 故障与验收矩阵

| 场景 | 必须观察到的结果 |
| --- | --- |
| accepted 尚未 flush 就断开 | 不进入 read queue，无 snapshot，entry 删除 |
| operation A/B 同时运行，cancel A | A 为 `E_CANCELLED`；B 不受影响 |
| 8 个 active + queued operation 被 cancel | queued entry 在一次 admission poll 内退出，不占 read slot |
| cancel 与 complete 竞态 | 只出现 cancelled 或 completed 一个 terminal outcome；cancel response 与之一对应 |
| scan/sort/aggregate 中 cancel | 后续 checkpoint 退出，snapshot/permit/entry 收敛 |
| schema 后发送若干 row 再失败 | 一个 error frame 带精确 `emitted_rows`；无 complete |
| 客户端停止读取 | bounded channel 不增长；5 秒 write timeout 后 operation 终止 |
| TCP/HTTP 同一 query/params | schema、typed row frame 与 terminal counts 相同 |
| shutdown | 不接纳新 stream；registered/queued 立即终止，running/emitting 在 checkpoint/deadline 内终止 |
| 服务重启后 cancel 旧 token | `unknown`，不影响新 operation |
| mutation/introspection/receipt/page stream | 接纳前稳定 shape error，数据库不变 |

## 10. 恢复、兼容与发布

完整 JSON protocol version 1/2 与 CLI 输出不变。stream protocol 独立从 version 1 开始；实现完成时 `version --format json` 增加 `stream_protocol_versions: [1]`，属于 version report schema 的可加字段。

stream 没有隐式 resume token。若在 complete 前断开，客户端必须把已收到的 rows 视为 partial：可以从头重跑并自行去重，或改用稳定 cursor page 获得明确的完整页与续传边界。取消 read 没有数据库 effect，也不写 idempotency receipt。

发布顺序固定为 #155 RFC、#156 core、#157 adapters。#156 不开放未完成 wire；#157 必须同时完成 TCP 与真实 HTTP 场景、协议文档和故障测试后，才在 version diagnostics 中声明 stream version 1。

## English Summary

Stream protocol version 1 is a separate transport-neutral envelope around an existing protocol-v1/v2 query request. A server-generated 128-bit `o1` bearer capability is returned in an accepted frame before read admission. A bounded process-local registry linearizes cancellation against completion, retains only small terminal tombstones, and never logs operation IDs or stores query data. Cancellation applies only to one read operation; mutations, introspection, receipt maintenance, explain, pages, persistence, and cross-node control are excluded.

The NDJSON sequence is accepted, schema, zero or more typed row frames, and exactly one complete or error terminal frame. TCP and HTTP consume the same bounded frame channel and `WireValue` encoder. Eight queued frames and 16 MiB queued bytes enforce backpressure; per-frame, total-byte, row, deadline, socket-idle, registry, and snapshot limits bound every stream. A fully materialized typed result releases its immutable snapshot before network backpressure can extend the snapshot lifetime.

Disconnect may trigger local cleanup but is not an acknowledged cancellation protocol. The registry mutex is the sole completion/cancellation race linearization point. Existing full JSON responses and stable cursor pages remain unchanged; partial streams have no implicit resume semantics, so clients requiring retryable traversal should use complete cursor pages. Core registry/execution work is tracked by #156 and shared TCP/HTTP adapters by #157.
