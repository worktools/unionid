# HTTP 数据协议示例

unionid 的内置远程服务继续使用轻量 JSON Lines。需要 HTTP、认证、TLS、租户路由或应用 API 的生产服务，应把 version 1 `Request` / `Response` 放入自己的 HTTP 框架，并将请求交给共享的 `server::ConcurrentEngine::execute_protocol_request_until`，同时传入该 HTTP 请求的绝对 deadline。读取会捕获一致 committed snapshot 并在 writer lock 外并发执行；mutation 与 maintenance 仍串行。async handler 应使用 blocking worker，且不要另加一个覆盖完整查询生命周期的 `Mutex<Engine>`。数据库不会因此出现第二套 query parser 或执行语义；边界见 [RFC 0006](rfc/0006-consistent-read-snapshots.md)。

`examples/todolist.rs` 使用 Axum 展示完整 adapter。核心查询 endpoint 是：

```text
POST /v1/query
Content-Type: application/json
```

请求 body 直接序列化 `unionid::ProtocolRequest`：

```json
{
  "version": 1,
  "request_id": "todo-42",
  "query": "insert todos $row\nreturning",
  "params": {
    "row": {
      "type": "record",
      "fields": {
        "id": {"type": "int", "value": "9007199254740993"},
        "status": {
          "type": "variant",
          "name": "Inbox",
          "variant_id": "0",
          "args": []
        }
      }
    }
  }
}
```

Rust 客户端应从普通应用类型构造请求，而不是手写上述 JSON：

```rust
let request = ProtocolRequest::query(
    "todo-42",
    "insert todos $row\nreturning",
)
.with_serde_param("row", &todo)?
.with_idempotency_key("create-todo-42")?;
```

HTTP response 直接序列化 `unionid::ProtocolResponse`。Rust 客户端可用 `response.typed_rows::<Todo>()?` 读取 query 或 DML `returning`，同时 wire JSON 保留 i64 精度、Option/null 区别、tuple/list 形态和稳定 named/variant ID。

分页同样不拼接查询文本。第一页使用 `PageSpec::forward`，`typed_page` 解码应用类型并保留续页信息：

```rust
let first = ProtocolRequest::query("todos-1", "from todos\nsort id")
    .with_page(PageSpec::forward(100));
let first = post(first).await?.typed_page::<Todo>()?;
if let Some(next) = first.page.next_page() {
    let second = ProtocolRequest::query("todos-2", "from todos\nsort id")
        .with_page(next);
    let second = post(second).await?.typed_page::<Todo>()?;
}
```

## 示例覆盖

```bash
cargo run --example todolist
# 或指定一个不会覆盖既有目标文件的工作目录
cargo run --example todolist -- /path/to/empty-work-directory
```

指定目录中若已经存在 `todos.redb`、`todos-restored.redb` 或 `todos.backup.json`，示例会拒绝启动，不会删除或覆盖这些文件。

示例启动真实本地 HTTP listener，客户端只持有 HTTP 地址，不接触 `Engine` 或数据库路径。它通过 HTTP 完成：

- 初始 migration；
- 包含嵌套 sum/product、Option、list、tuple 和超出 JavaScript safe integer 的 typed bulk insert；
- 条件状态转换和 `returning`；
- typed 参数错误；
- 提交成功后注入 response loss，再用相同 key 和新 request ID 重试，验证 `replayed = true` 且只写入一次；
- 通过 blocking worker 与共享 `ConcurrentEngine` 执行请求，多页 typed todo 遍历、真实客户端断开，以及断开后的健康请求；
- 服务端关闭/重开 redb 后继续旧 cursor，并拒绝被修改的 cursor；
- 深层 ADT migration、旧 cursor 的 `E_CURSOR_SCHEMA`、schema mismatch 和索引 explain；
- 完整性检查、逻辑备份、还原及 typed rows 比较。

示例中的 `/v1/admin/*` 是应用生命周期 API，用于展示 migration、restart、check、backup 和 restore 全程通过 HTTP；它们不是内置公共管理服务。生产部署必须自行增加鉴权、授权、TLS、审计、限流和安全的备份目标配置，不能允许不受信任的请求选择服务器文件路径。

## 生产边界

- `request_id` 只做一次尝试的关联。需要安全重试的 mutation 使用独立 `idempotency_key`，并原样复用 query、typed params 和 schema precondition；response 中的 `replayed` 区分首次提交与回执重放。
- HTTP adapter 不应自行实现 key 表、digest 或 cursor；直接调用 `ConcurrentEngine::execute_protocol_request_until`，才能与 TCP/Rust 使用相同 canonical wire digest、原子 receipt、分页和错误契约。示例从请求进入 Engine 时计算五秒绝对 deadline。
- receipt 不会自动过期。生产运维应先调用 version 1 receipt status/prune preview，再显式确认有界清理；清理后的 key 可能再次产生 effect。
- query source 仍受解析、执行、结果大小和 deadline 限制；HTTP adapter 应设置更严格的 body/header/connection 限额。deadline 返回 `E_TIMEOUT`，不返回半页或 cursor。
- 只读 HTTP 服务应使用 `Engine::open_redb_read_only(path)`，或在已打开的 Engine 上调用 `with_read_only(true)`，再包装为 `ConcurrentEngine`；adapter 不要只在路由层按字符串猜测写语句。`introspection.read_only` 可作为启动检查。
- 当前 response 是有界完整 JSON；最多 1000 行的 keyset page 适合可恢复列表遍历。客户端断开不会成为可靠的取消信号，读取最多继续到 adapter deadline，并继续受工作行、排序内存和响应大小限制。一致性读快照已由 #116 完成；显式取消与有背压的 NDJSON/streaming 由 [#135](https://github.com/worktools/unionid/issues/135) 跟踪。
- HTTP 层不得把普通 JSON number/null/object 当作无损 wire value；application-shaped serde 数据应由 Rust helper 或 schema-aware decoder 转为 `WireValue`。
