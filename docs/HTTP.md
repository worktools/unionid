# HTTP 数据协议示例

unionid 的内置远程服务继续使用轻量 JSON Lines。需要 HTTP、认证、TLS、租户路由或应用 API 的生产服务，应把 version 1 `Request` / `Response` 放入自己的 HTTP 框架，并将请求交给共享的 `server::ConcurrentEngine::execute_protocol_request_until`，同时传入该 HTTP 请求的绝对 deadline。读取会捕获一致 committed snapshot 并在 writer lock 外并发执行；mutation 与 maintenance 仍串行。async handler 应使用 blocking worker，且不要另加一个覆盖完整查询生命周期的 `Mutex<Engine>`。数据库不会因此出现第二套 query parser 或执行语义；边界见 [RFC 0006](rfc/0006-consistent-read-snapshots.md)。

启用 `http` feature 后，官方 Axum adapter 提供核心数据路由：

```rust
use std::time::Duration;
use unionid::asynchronous::http::{self, Config};
use unionid::{ConcurrentEngine, Engine};

let engine = ConcurrentEngine::new(Engine::open_redb("app.redb")?);
let app = http::router(
    engine,
    Config { request_timeout: Duration::from_secs(10) },
);
```

`router` 可直接交给 `axum::serve`，也可与应用路由合并。它在 Tokio blocking pool 上调用同一个 `ConcurrentEngine`，并提供以下 versioned endpoint：

- `POST /v1/query`
- `POST /v1/stream`
- `POST /v1/stream/cancel`

最小可运行服务见 `cargo run --features http --example http_service -- app.redb`。`examples/todolist.rs` 继续展示包含应用管理路由、migration、restart 与 backup/restore 的完整旅程。

核心查询 endpoint 是：

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

启用 `http-client` feature 可直接使用异步 typed client；它复用连接、限制 response 大小，并检查 response 的 protocol version 与 request ID：

```rust
use unionid::{HttpClient, PageSpec, ProtocolRequest};

let client = HttpClient::connect("https://database.example.com")?;
let first_request = ProtocolRequest::query("todos-1", "from todos\nsort id")
    .with_page(PageSpec::forward(100));
let first = client.page::<Todo>(&first_request).await?;
let second = client
    .next_page::<Todo>(&first_request, &first.page, "todos-2")
    .await?;
```

需要代理、自定义 TLS 或认证 header 时，用 `HttpClient::from_client` 传入配置好的 `reqwest::Client`；该入口要求 HTTPS。只有本机开发服务可显式使用 `from_local_client`，它把明文 HTTP 限制到 localhost/loopback 地址。`request_retrying` 只接受带 `idempotency_key` 的请求；响应可能在服务端提交后丢失，因此重试必须复用完整 query、typed params、schema precondition 和 key。分页 continuation 会清除 key，因为 page 只允许只读请求且不使用 mutation receipt。

HTTP response 直接序列化 `unionid::ProtocolResponse`。Rust 客户端可用 `response.typed_rows::<Todo>()?` 读取 query 或 DML `returning`，同时 wire JSON 保留 i64 精度、Option/null 区别、tuple/list 形态和稳定 named/variant ID。

长只读结果可直接通过 typed client 逐行解码。`stream` 会消费并验证首个 `accepted` frame；后续只有收到 `Complete` 才表示结果完整。`Error` 事件包含错误和已经发出的行数，此前的行必须视为 partial：

```rust
use unionid::{HttpClient, ProtocolRequest, TypedStreamEvent};

let request = ProtocolRequest::query("todos-stream", "from todos\nsort id");
let mut stream = client.stream(&request).await?;
let operation_id = stream.operation_id().to_owned();
while let Some(event) = stream.next_event::<Todo>().await? {
    match event {
        TypedStreamEvent::Row { row, .. } => consume(row),
        TypedStreamEvent::Complete { .. } => break,
        TypedStreamEvent::Error { error, .. } => return Err(error),
        TypedStreamEvent::Schema { .. } => {}
    }
}
let outcome = client.cancel("cancel-check", operation_id).await?;
```

底层 `POST /v1/stream` body 为 `stream::Request::Query`，响应为 `application/x-ndjson`；`x-unionid-operation-id` 与首个 `accepted` frame 携带同一个 bearer capability。`POST /v1/stream/cancel` 接受 `stream::Request::Cancel`。官方 adapter 在 accepted 已交给 body 后才启动读取，并直接消费 `AcceptedStream::start` 的有界 receiver，不重新编码 row。客户端校验每个 frame 的 version、request ID、operation capability、顺序以及单帧和总字节上限；流不提供隐式重试或续传。

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
- 通过 lazy HTTP body 和共享 NDJSON producer 遍历 typed rows，并用独立 cancel route 查询 terminal outcome；
- 服务端关闭/重开 redb 后继续旧 cursor，并拒绝被修改的 cursor；
- 深层 ADT migration、旧 cursor 的 `E_CURSOR_SCHEMA`、schema mismatch 和索引 explain；
- 完整性检查、逻辑备份、还原及 typed rows 比较。

示例中的 `/v1/admin/*` 是应用生命周期 API，用于展示 migration、restart、check、backup 和 restore 全程通过 HTTP；它们不是内置公共管理服务。生产部署必须自行增加鉴权、授权、TLS、审计、限流和安全的备份目标配置，不能允许不受信任的请求选择服务器文件路径。

## 官方客户端与异步适配

Rust 应用可直接使用 `unionid::client::TcpClient` 连接 TCP 服务。Tokio 应用启用 `asynchronous` feature 后可使用 cloneable `AsyncTcpClient`；clone 共享一条串行请求连接，stream 使用独立连接，cancel 再使用独立控制连接，因此取消不会被正在读取的 stream 阻塞：

```rust
use unionid::{AsyncTcpClient, ProtocolRequest, TypedStreamEvent};

let client = AsyncTcpClient::connect("127.0.0.1:9123").await?;
let request = ProtocolRequest::query("todos", "from todos\nsort id");
let rows = client.request(&request).await?.typed_rows::<Todo>()?;

let mut stream = client.stream(&request).await?;
let operation_id = stream.operation_id().to_owned();
while let Some(event) = stream.next_event::<Todo>().await? {
    if let TypedStreamEvent::Row { row, .. } = event {
        consume(row)
    }
}
let terminal = client.cancel("cancel-check", operation_id).await?;
```

`AsyncTcpClient` 的单次 request/stream 使用同一个绝对 deadline，等待共享连接、重连、写入和读取都会消耗该预算。传输失败、半响应或超时会丢弃已污染的请求连接；下一次请求重新连接。只有带 `idempotency_key` 的完整请求可用 `request_retrying` 自动重发，防止“服务端已提交、客户端未收到响应”造成重复 effect。page continuation 会清除 mutation key；production scalar 继续要求 protocol v2。HTTP 与 TCP stream 共用 frame identity/order/count validator 和 typed row decoder。

需要自定义 async handler 时，同一 `asynchronous` feature 还提供 `unionid::asynchronous::execute_protocol_request`，在 blocking worker 上执行 `ConcurrentEngine` 并传递 deadline。标准 Axum 数据面可启用 `http` feature 和上述 router。所有方式都无需应用持有 `Mutex<Engine>` 或自建线程池；`ConcurrentEngine`、`ReadOperation`、`ConcurrencyStats` 等集成类型已在 crate root 导出。

## 生产边界

- `request_id` 只做一次尝试的关联。需要安全重试的 mutation 使用独立 `idempotency_key`，并原样复用 query、typed params 和 schema precondition；response 中的 `replayed` 区分首次提交与回执重放。
- HTTP adapter 不应自行实现 key 表、digest 或 cursor；直接调用 `ConcurrentEngine::execute_protocol_request_until`，才能与 TCP/Rust 使用相同 canonical wire digest、原子 receipt、分页和错误契约。示例从请求进入 Engine 时计算五秒绝对 deadline。
- receipt 不会自动过期。生产运维应先调用 version 1 receipt status/prune preview，再显式确认有界清理；清理后的 key 可能再次产生 effect。
- query source 仍受解析、执行、结果大小和 deadline 限制；HTTP adapter 应设置更严格的 body/header/connection 限额。deadline 返回 `E_TIMEOUT`，不返回半页或 cursor。
- 只读 HTTP 服务应使用 `Engine::open_redb_read_only(path)`，或在已打开的 Engine 上调用 `with_read_only(true)`，再包装为 `ConcurrentEngine`；adapter 不要只在路由层按字符串猜测写语句。`introspection.read_only` 可作为启动检查。
- 完整 JSON 与最多 1000 行的 keyset page 适合短查询和可恢复遍历。stream 受 8 frame/16 MiB channel、16 MiB 单帧、100,000 rows、256 MiB 和总 deadline 约束；完整 materialize 后即释放 snapshot，慢 HTTP body 不延长 snapshot。断开可触发资源清理但不是取消确认；只有 cancel response 或 terminal frame 可确认 outcome。`complete` 前收到的内容必须视为 partial，不能从半帧隐式续传。
- HTTP 层不得把普通 JSON number/null/object 当作无损 wire value；application-shaped serde 数据应由 Rust helper 或 schema-aware decoder 转为 `WireValue`。
