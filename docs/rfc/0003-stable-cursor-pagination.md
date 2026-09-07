# RFC 0003: Stable cursor pagination

- Status: Accepted
- Tracking: #114, #130
- Implementation: #131, #132

## 中文说明

### 1. 决策摘要

unionid 首个生产分页能力使用有界 keyset page，不使用 offset，也不在没有 MVCC 的情况下宣称跨请求 snapshot：

- 分页查询必须显式排序，且最后一个排序键必须是源表主键；因此完整复合键可证明唯一。
- 第一页固定当前 schema identity、数据库 instance ID 和 commit sequence。后续页若 sequence 改变，直接返回 `E_CURSOR_STALE`，不扫描、不返回部分结果。
- cursor 是有版本、有大小上限、以数据库持久 secret 做 HMAC-SHA-256 的 opaque token。它保证来源与完整性，不保证内容保密。
- query language 的 `page` stage、Rust API 和 version 1 `page` object 映射到同一个 `PageSpec`，避免语言与结构化协议形成两套语义。
- 首版只返回完整 JSON page。NDJSON/streaming、背压和跨请求显式取消需要并发读取基础，延后到独立任务；deadline 与断开连接仍有明确资源边界。

这是一种保守的一致性模型：并发写不会被阻塞，但会使旧 cursor 明确失效。调用方可以重启遍历，永远不会在数据库已变化时收到伪装成同一 snapshot 的下一页。

### 2. 查询与协议形态

语言形态：

```text
from tasks
filter archived == false
sort {-priority, id}
page 100
```

恢复下一页时，CLI 可以写：

```text
from tasks
filter archived == false
sort {-priority, id}
page 100 after "u1.payload.mac"
```

version 1 使用与 stage 同构的结构化字段，避免应用拼接查询文本：

```json
{
  "version": "1",
  "request_id": "tasks-2",
  "query": "from tasks\nfilter archived == false\nsort {-priority, id}",
  "page": {
    "limit": 100,
    "direction": "forward",
    "cursor": "u1.payload.mac"
  }
}
```

规则：

- 查询内 `page` 和 request `page` 不能同时出现，冲突返回 `E_PAGE_SHAPE`。
- `limit` 范围为 1..=1000，并继续受全局 row/encoded-response byte limit 约束。
- `page N` 等价于 `page N forward`；`after` 只用于 forward，`before` 只用于 backward。
- page 必须是读取 pipeline 的最后一个 row-producing stage；首版不支持 mutation target、aggregate 或 group。
- `select` 可以隐藏排序字段，但 cursor 边界来自 select 前的 typed row，不要求把主键暴露给客户端。
- Rust builder 产生结构化 `PageSpec`；解析后的语言 stage 也归一化为完全相同的内部结构、验证和 plan digest。

成功响应增加：

```json
{
  "page": {
    "limit": 100,
    "direction": "forward",
    "snapshot_sequence": "42",
    "next_cursor": "u1.payload.mac",
    "previous_cursor": null,
    "has_more": true
  }
}
```

返回行始终保持声明的 sort 顺序。backward 只改变选取方向，不倒转客户端看到的行顺序。空页没有新的边界 cursor；最后一个非空页 `has_more = false` 且 `next_cursor = null`。

### 3. 唯一顺序

分页绑定发生在扫描前。首版只接受：

1. 单表读取 pipeline；
2. 一个有效的最终 `sort` stage；
3. sort 的最后一项是该表主键字段，且方向可独立为升序或降序；
4. 前缀字段和主键都能使用现有 typed total ordering；
5. sort 后只能出现不改变行身份的 projection 与 page。

例如 `sort {-priority, created_at, id}` 稳定，`sort priority` 即使当前数据没有重复也必须返回 `E_PAGE_ORDER`。不能根据样本数据推断唯一性。未来可以接受“完整 sort tuple 命中 typed unique index”，但不属于 #131。

边界比较使用完整 typed tuple，并逐项应用声明方向。cursor 不使用 `RowId` 作为隐式 tie-breaker，因为 RowId 不是查询契约，也不应替代用户可见的唯一顺序。

### 4. Cursor envelope

token 文本格式：

```text
u1.<base64url-no-pad(canonical payload)>.<base64url-no-pad(HMAC-SHA-256(payload))>
```

canonical payload 使用固定字段顺序和当前 version 1 无损 typed-value 编码，包含：

- cursor codec version；
- database instance ID；
- schema revision 与 SHA-256 hash；
- snapshot commit sequence；
- canonical bound-plan digest；
- direction；
- page limit；
- 每个 sort key 的稳定字段身份、方向与最后一行 typed value。

约束：

- token UTF-8 长度最大 8192 bytes；payload 解码后最大 6144 bytes；sort key 最多 16 项。
- 解码、版本、base64、长度、typed value 和 MAC 使用有界、fail-closed 路径。
- MAC 比较必须 constant-time。
- redb 创建时生成 256-bit cursor secret 与 128-bit instance ID，并持久化在 meta；memory Engine 每进程生成。逻辑 backup/restore 生成新的 instance ID 和 secret，因此旧 cursor 对恢复副本不能通过完整性验证。
- secret 不通过 introspection、backup JSON、日志、error 或 `explain` 输出。
- token 未加密，客户端不得把排序值视为秘密；需要保密时由 TLS 保护传输，未来可版本化为 AEAD codec。

plan digest 基于绑定后的规范 IR，而不是原始空白或 request ID。它包含 source、filter/derive/select、sort、page limit/direction 和 typed params，但排除 cursor 本身。等价的语言 `page` 与结构化 `PageSpec` 必须得到相同 digest。

### 5. 一致性与并发变化

首版是 sequence-pinned traversal：

- 第一页在执行开始读取 sequence S，并只在同一 Engine 请求内产生 rows 与 cursor。
- 恢复页在解析、cursor 验证和 schema/plan 绑定后、扫描前检查当前 sequence == S。
- 任意已提交 schema 或 data mutation（即使修改其他表）都会推进 sequence，使旧 cursor 返回 `E_CURSOR_STALE`。
- 失败、回滚、只读请求和幂等 replay 不推进 sequence，因此不使 cursor 失效。
- redb 重开保留 sequence、instance ID 和 secret，所以未发生写入的 cursor 可恢复；memory 重启不承诺恢复。

因此在 sequence 不变时，正向或反向遍历不会静默重复或遗漏；sequence 改变时不返回可能误导的页面。删除、插入、排序键更新和 migration 都适用同一规则。#116 引入真正一致读 snapshot 后，可新增 cursor codec version，而不能悄悄改变 `u1` 语义。

### 6. 完整性检查顺序与错误

恢复请求按下列顺序失败，且在任何 row scan 前完成：

1. shape/limit/token size；
2. token syntax/version/base64；
3. 对 raw payload 做 HMAC constant-time 验证；
4. canonical payload 与 typed boundary 解码；
5. database instance；
6. schema identity；
7. plan digest、direction 与 limit；
8. snapshot sequence and bind。

稳定错误码：

| Code | Meaning |
| --- | --- |
| `E_PAGE_SHAPE` | page 位置、双重来源、limit 或方向无效 |
| `E_PAGE_ORDER` | 缺少可证明唯一的最终排序 |
| `E_CURSOR_LIMIT` | token/payload/key 数超限 |
| `E_CURSOR_CODEC` | syntax、版本、base64 或 typed payload 无效 |
| `E_CURSOR_DATABASE` | cursor 属于另一数据库实例 |
| `E_CURSOR_INTEGRITY` | HMAC 不匹配 |
| `E_CURSOR_SCHEMA` | schema identity 改变 |
| `E_CURSOR_QUERY` | plan、params、limit 或方向不匹配 |
| `E_CURSOR_STALE` | commit sequence 已改变 |

外部响应不区分“随机伪造”与“合法 token 被修改”的细节。日志只记录错误码和 request ID，不记录完整 cursor。

### 7. Explain 与预算

`explain` 增加 page plan：limit、direction、唯一 sort tuple、是否有 resume boundary、snapshot sequence 和访问路径。它不输出 cursor、secret、MAC 或完整 boundary values。

执行器最多读取 `limit + 1` 个已排序候选来判断 `has_more`。首版若现有索引不能按完整 tuple 有序迭代，可以在当前 query scan/sort 预算内排序，但 explain 必须显示 `sorted_scan`，不能伪称 index seek。cursor 验证不增加 scan。

### 8. Deadline、断开与后续取消

- 现有 request deadline 在 parse、bind、scan/sort 和编码阶段继续检查；超时返回 `E_TIMEOUT`，不产生 cursor。
- TCP/HTTP 客户端断开后，首版执行可继续到 deadline，但读取无副作用，且 CPU、rows、sort memory 与响应编码仍受现有限额约束。
- 跨请求显式 cancel 需要 request registry、并发 Engine/read snapshot 和认证后的 operation ID；当前串行 Engine 无法可靠取消正在持锁的请求。
- NDJSON/streaming 需要背压、半写响应错误、snapshot 生命周期和 server shutdown 规则。

因此 #131/#132 只实现 bounded page 与 deadline/disconnect 验证。#132 必须为显式 cancel + NDJSON/backpressure 建立 deferred issue，并关联 #116，不能用“关闭 socket 即取消”的非契约行为替代。

### 9. 验收向量

实现测试至少覆盖：

| Vector | Expected result |
| --- | --- |
| 相同语义的换行/inline query + 相同 typed params | 相同 plan digest，cursor 可恢复 |
| request ID 改变 | cursor 可恢复 |
| filter、typed param、limit 或方向改变 | `E_CURSOR_QUERY`，零扫描 |
| schema migration | `E_CURSOR_SCHEMA`，零扫描 |
| 同库任意成功 mutation | `E_CURSOR_STALE`，零扫描 |
| rollback、失败 mutation、read 或 idempotency replay | sequence 不变，cursor 可恢复 |
| payload/MAC 任一 bit 改变 | `E_CURSOR_INTEGRITY` 或 fail-closed codec error |
| 另一数据库或 restore 副本 | 因使用不同 secret 返回 `E_CURSOR_INTEGRITY` |
| 重复 sort prefix + 主键 tie-breaker | 正反向遍历无重复/遗漏 |
| 只有非唯一 sort prefix | `E_PAGE_ORDER`，零扫描 |
| cursor 8192 bytes / 8193 bytes | 前者按内容验证，后者 `E_CURSOR_LIMIT` |
| redb reopen 且无写入 | cursor 仍有效 |

## English Description

### Decision

The first production pagination release uses bounded keyset pages. A page-capable query must end its explicit sort tuple with the source table primary key, making the complete order provably unique. The first page pins the database instance, schema identity, and commit sequence. Any successful write before a resumed page yields `E_CURSOR_STALE` before scanning instead of pretending that the next request observes the original snapshot.

Language `page`, the Rust API, and the version-1 `page` object normalize to one `PageSpec` and one bound-plan digest. The opaque `u1` token is size bounded and authenticated with HMAC-SHA-256 using a per-database persisted secret. It authenticates but does not encrypt typed boundary values. Logical restore creates a new database identity and secret, so source cursors cannot be replayed against a clone.

The response preserves declared sort order in both directions and exposes bounded page metadata. Cursor decoding, database/MAC/schema/query/sequence checks, and typed-boundary binding all finish before row scanning. Explain reports the page limit, direction, unique order, boundary presence, sequence, and whether execution uses an index seek or bounded sorted scan, but never emits secrets or cursor contents.

The `u1` consistency contract is deliberately conservative: unchanged sequence means forward and backward traversal has no silent duplicates or gaps; changed sequence means explicit restart. A future MVCC snapshot from #116 requires a new cursor version rather than changing this behavior.

The first implementation keeps complete JSON page responses. Existing deadlines bound parse, bind, execution, and encoding. A disconnected client may leave a read running only until those limits expire. Explicit cross-request cancellation and NDJSON/backpressure require request registries and concurrent read snapshots, so #132 must split them into a deferred issue associated with #116.

The normative limits, validation order, stable error codes, and acceptance vectors are defined in the Chinese sections above and apply equally to both language and structured protocol forms.
