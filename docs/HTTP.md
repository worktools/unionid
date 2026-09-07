# HTTP 数据协议示例

unionid 的内置远程服务继续使用轻量 JSON Lines。需要 HTTP、认证、TLS、租户路由或应用 API 的生产服务，应把 version 1 `Request` / `Response` 放入自己的 HTTP 框架，并将请求交给 `server::execute_protocol_request`。数据库不会因此出现第二套 query parser 或执行语义。

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
.with_serde_param("row", &todo)?;
```

HTTP response 直接序列化 `unionid::ProtocolResponse`。Rust 客户端可用 `response.typed_rows::<Todo>()?` 读取 query 或 DML `returning`，同时 wire JSON 保留 i64 精度、Option/null 区别、tuple/list 形态和稳定 named/variant ID。

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
- 服务端关闭/重开 redb；
- 深层 ADT migration、schema mismatch 和索引 explain；
- 完整性检查、逻辑备份、还原及 typed rows 比较。

示例中的 `/v1/admin/*` 是应用生命周期 API，用于展示 migration、restart、check、backup 和 restore 全程通过 HTTP；它们不是内置公共管理服务。生产部署必须自行增加鉴权、授权、TLS、审计、限流和安全的备份目标配置，不能允许不受信任的请求选择服务器文件路径。

## 生产边界

- `request_id` 只做关联，不是幂等键。写请求在 response 前断线时，提交结果可能未知。
- query source 仍受解析、执行、结果大小和 deadline 限制；HTTP adapter 应设置更严格的 body/header/connection 限额。
- 当前 response 是有界完整 JSON。大结果集的 cursor/NDJSON、背压、取消与 prepared handle 由 #101 后续阶段定义。
- HTTP 层不得把普通 JSON number/null/object 当作无损 wire value；application-shaped serde 数据应由 Rust helper 或 schema-aware decoder 转为 `WireValue`。
