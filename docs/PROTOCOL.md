# 版本化接口与参数

unionid 的稳定网络边界是 JSON Lines 协议 version 1：每个请求和响应各占一个物理行。<code>query</code> 是 JSON string，因此源码中的换行、缩进、引号和管道符都作为数据传输，不参与协议分帧。服务仍暂时接受旧的 <code>{"query":"..."}</code> 和纯文本单行请求，新的客户端应使用本页协议。

持久幂等写入的 exactly-once effect、request digest、回执、容量、显式清理和格式升级契约见 [RFC 0002](rfc/0002-idempotent-write-receipts.md)。`request_id` 仍然只做单次尝试的关联；安全重试必须复用独立的 `idempotency_key`。

version 1 的 `Request` / `Response` 是与 transport 无关的数据协议。内置服务使用 JSON Lines；HTTP adapter 应在 `POST /v1/query` 的 JSON body 中直接使用同一结构，并调用 `server::execute_protocol_request`，或用 `execute_protocol_request_until` 设置 adapter 自己的 deadline。这样 TCP、HTTP 和嵌入式 adapter 共享版本检查、参数解码、schema identity、deadline、introspection、错误与返回行语义，而不是各自解释 query。

## 生产标量接入状态

version 2 增加 UUID、date、timestamp、duration、decimal 和 bytes wire value，canonical envelope 见 [RFC 0004](rfc/0004-production-scalars.md)。Rust request builder 默认仍为 version 1，可用 `.with_version(2)?` 选择 version 2；同一个 request_id 不影响幂等 identity，版本仍参与 canonical digest。六类生产标量均已接入源码语言、Rust typed API、wire、codec 与 redb；decimal wire 使用 coefficient string 与 numeric scale，schema precision 由 column type 保留。

当前已实现新参数解码、返回行、版本回显和完整 v1 typed-boundary 预检；v1 在执行 mutation 前检查嵌套参数、最终 query／`returning`／`explain` 结果类型及 introspection schema，并以 `E_PROTOCOL_TYPE` 拒绝无法表达的请求。幂等命中保持不解析源码的重放语义。新 redb 数据库使用 storage format 4；旧 format 1–3 只允许新参数的只读用法，必须显式执行 `unionid upgrade --db <path> --target 4` 后才能持久写入。Rust 与 codec 当前范围见 [SCALARS.md](SCALARS.md)。

## 请求

~~~json
{"version":1,"request_id":"task-42","query":"from tasks\nfilter id == $id\nselect {id, title}","params":{"id":{"type":"int","value":"9007199254740993"}}}
~~~

| 字段 | 规则 |
| --- | --- |
| <code>version</code> | 支持 1 和开发中的 2；响应回显请求版本，其他值返回 <code>E_PROTOCOL_VERSION</code> |
| <code>request_id</code> | 客户端提供的 UTF-8 string，最多 1 KiB，响应原样返回；它只用于关联请求，不提供去重或 exactly-once |
| <code>query</code> | 完整 unionid 源码，最多 1 MiB |
| <code>introspect</code> | 可选的 `schema`／`tables`／`types`／`storage`；使用时 query 必须为空且不能携带 params/schema |
| <code>params</code> | 可省略的命名 typed value；源码以 <code>$name</code> 引用 |
| <code>schema</code> | 可省略的 <code>{revision, hash}</code>；不等于当前 schema 时，在解析或扫描前返回 <code>E_SCHEMA_CHANGED</code> |
| <code>idempotency_key</code> | mutation 可选，1–256 UTF-8 bytes；同 key 与规范 digest 重放原成功响应，不同 digest 返回 `E_IDEMPOTENCY_CONFLICT` |
| <code>receipts</code> | 可选的独立 `status` / `prune` 运维操作；不能和 query、params、schema、introspection 或 idempotency key 混用 |
| <code>page</code> | 可选的 `{limit, direction, cursor}`；与源码末尾 `page` 同构，不能同时出现，limit 为 1..=1000 |

参数名使用与标识符相同的 ASCII 规则，以字母或下划线开头。缺少参数返回 <code>E_PARAM_MISSING</code>，多余参数返回 <code>E_PARAM_EXTRA</code>，wire value 无法解码返回 <code>E_PARAM_TYPE</code>；参数解码后仍由查询上下文做普通类型检查，所以类型不匹配返回 <code>E_TYPE</code>。绑定发生在 AST 上，不通过文本替换，文本参数中的引号、换行、注释符或 pipeline 符号不会改变查询结构。参数也可直接作为 `derive match` 或 `set ... = match ...` 的分支结果及嵌套 constructor 负载，并由结果／目标字段类型检查。

### 幂等 mutation

Rust 客户端使用 `Request::with_idempotency_key`，不发送或手写 digest：统一执行入口会从 version、精确 query UTF-8、排序后的 typed wire params 和可选 schema precondition 计算 `sha256:<hex>`。request ID、key 本身和 deadline 不参与 digest，因此网络重试可使用新的 request ID。相同 key/digest 在参数解码和当前 schema 检查之前返回原回执；read、explain 或 introspection 携带 key 返回 `E_IDEMPOTENCY_NOT_MUTATION`。

~~~json
{"version":1,"request_id":"attempt-1","query":"insert tasks $row\nreturning","params":{"row":{"type":"record","fields":{"id":{"type":"int","value":"42"}}}},"idempotency_key":"create-task-42"}
~~~

成功响应增加以下结构；memory 为 `process_local`，redb 为 `durable`：

~~~json
{"idempotency":{"key":"create-task-42","digest":"sha256:...","replayed":false,"committed_sequence":"17","durability":"durable"}}
~~~

响应丢失后必须原样复用 query、wire params、schema precondition 和 key。只改变 request ID 会得到 `replayed: true`；改变源码空白、参数 wire 拼写或 schema 会产生不同 digest 并安全冲突。普通失败、timeout、read-only、容量错误和确定中止不会占用 key。commit 结果不确定时关闭连接／重开数据库，再用同一请求重试。

### 回执状态与显式清理

`Request::receipt_status` 返回 count、encoded bytes、固定容量以及最老／最新 sequence 和时间边界。`Request::receipt_prune` 必须给出 `completed_before_unix_ms`（严格早于）或 `committed_through_sequence`（包含该 sequence）；同时给出时两者取交集。`max_receipts` 为 1–1000，按 sequence、完成时间和 key 稳定排序后截断。`confirm: false` 只预览，`confirm: true` 才在一个事务中删除同一选择。

清理后同 key 会重新成为可执行的新请求，因此保留窗口必须长于客户端、消息队列和人工重放的最大重试窗口。系统没有自动 TTL/LRU。CLI 对应 `unionid receipts status` 和默认预览的 `unionid receipts prune`；执行删除必须显式传 `--confirm`。

参数可用于 filter、match condition、普通／match derive 的算术或 bool 结果，以及 update <code>set</code>；prepared 绑定会在扫描前从字段或分支结果推导类型。完整 insert/upsert row 使用 <code>insert tasks $row</code> / <code>upsert tasks $row</code>；<code>insert many tasks $rows</code> 与 <code>upsert many tasks $rows</code> 接受 <code>list Task</code>，按输入顺序原子写入并 returning。批量 upsert 要求主键，拒绝输入内重复主键，并返回与输入逐项对齐的 action。一个带参数的多语句请求仍是同一个原子批次。过渡 WAL 不能安全重放绑定后的写 AST，因此参数化写入只支持 memory/redb；redb 是正式持久入口。

### 结构化分页

应用可以让 query 省略源码 `page`，改用结构化字段，避免拼接 cursor：

~~~json
{"version":1,"request_id":"tasks-2","query":"from tasks\nfilter archived == false\nsort {-priority, id}","page":{"limit":100,"direction":"forward","cursor":"u1.payload.mac"}}
~~~

`direction` 为 `forward` 或 `backward`，省略时是 forward；第一页省略 cursor。Rust 可写 `Request::query(...).with_page(PageSpec::forward(100))`。`Response::typed_page::<T>()` 同时返回应用 row 与 `PageInfo`；`PageInfo::next_page()` / `previous_page()` 直接构造下一次 `with_page` 所需的结构，不需要应用读取或拼接 wire cursor tag：

cursor 的 prefix 由实际排序 boundary 决定。全部键都属于 version 1 vocabulary 时，即使请求使用 protocol v2，也继续输出 `u1`；boundary 中出现 UUID、时间、decimal 或 bytes 时输出 `u2`。`u2` 只扩充 typed value vocabulary，HMAC、schema/query/sequence 绑定和 8192-byte token 上限不变。

~~~rust
let mut page = PageSpec::forward(100);
loop {
    let response = send(Request::query("tasks", query).with_page(page))?;
    let typed = response.typed_page::<Task>()?;
    consume(typed.rows);
    let Some(next) = typed.page.next_page() else { break };
    page = next;
}
~~~

服务把结构化字段归一为语言的最终 `page` stage，因此两种入口共享绑定、plan digest、错误和资源限制。page 只接受单条 read pipeline；与源码 page、mutation、introspection、receipt operation 或 idempotency key 混用时返回稳定错误。

Introspection 使用同一版本请求，返回完整的类型化快照，客户端再按请求种类展示。它不执行查询或修改数据：

~~~json
{"version":1,"request_id":"inspect-1","query":"","introspect":"types"}
~~~

响应的 <code>introspection</code> 包含 schema identity、规范 schema 源码、tables、types、fields、storage mode、`read_only`、migration count 和可选 head。`read_only` 表示当前 Engine 是否拒绝 mutation，客户端可用它验证自己连接的服务边界；旧 payload 缺少该字段时反序列化为 `false`。payload 上限为 1 MiB，超过时返回 <code>E_LIMIT</code>；整个响应仍受服务的 16 MiB 上限。未知 introspection 值或与 query/params/schema 混用返回 <code>E_PROTOCOL</code>。不带 <code>introspect</code> 的既有 version 1 请求以及旧 `{query}` 请求保持兼容。

## 无损值编码

所有协议值都有显式 <code>type</code>。i64、稳定 type ID 和 variant ID 使用十进制 string，避免 JavaScript number 的 53-bit 精度限制。有限 f64 也使用 string，使解码不依赖 JSON number 实现。null、option 的 None、空 list 和 unit variant 具有不同结构。

~~~json
{"type":"int","value":"-9223372036854775808"}
{"type":"float","value":"1.25"}
{"type":"text","value":"hello"}
{"type":"bool","value":true}
{"type":"null"}
{"type":"option","value":null}
{"type":"option","value":{"type":"text","value":"present"}}
{"type":"list","items":[]}
{"type":"tuple","items":[{"type":"int","value":"1"},{"type":"text","value":"x"}]}
{"type":"record","fields":{"id":{"type":"int","value":"1"}}}
{"type":"variant","name":"Running","variant_id":"17","args":[]}
{"type":"named","type_id":"9","value":{"type":"variant","name":"Pending","variant_id":"16","args":[]}}
~~~

客户端构造上下文明确的 ADT 参数时可以将 <code>type_id</code> / <code>variant_id</code> 设为 "0"，由目标字段类型解析名称。查询结果总是返回 catalog 中的稳定 ID，因此不同命名类型下的同名 constructor 不会混淆。普通 JSON 的 number/null/array/object 没有足够信息表达这些区别，不作为 version 1 typed value 的替代格式。

### 源码、Rust 与协议的共同数据形态

协议不把 query AST JSON 化。查询结构仍由同一份可格式化源码表达，应用数据则通过 `$param` 进入 typed binder。三层形态按下面的规则对应：

| unionid 源码 | Rust serde | version 1 wire |
| --- | --- | --- |
| `{id = 1, title = "x"}` | `struct { id: i64, title: String }` | `record.fields` |
| `Running {attempt = 2}` | `enum::Running { attempt: i64 }` | `variant {name, variant_id, args}` |
| `Some value` / `None` | `Option<T>` | `option.value` |
| `[a, b]` | `Vec<T>` | `list.items` |
| `(a, b)` | Rust tuple | `tuple.items` |
| 命名 ADT | 应用 struct/enum | `named {type_id, value}` |

Rust 客户端无需手工构造这些标签：`Request::query(...).with_serde_param("row", &row)` 把普通 serde struct/enum 编码为无损参数；服务端根据 prepared query 的目标 schema 补齐 nominal identity 并完成完整类型检查。`Response::typed_rows::<T>()` 将 wire rows 直接解码回应用类型。由此，源码的 constructor/record 形态和 Rust 的 enum/struct 保持接近，而 wire 层仍保存跨语言所需的 ID、数值精度和容器区别。

HTTP 适配、生命周期 endpoint 和完整 todo 场景见 [HTTP 数据协议示例](HTTP.md)。

## 响应

~~~json
{
  "version": 1,
  "request_id": "task-42",
  "ok": true,
  "message": "1 row(s)",
  "columns": [{"name":"id","ty":"int"},{"name":"title","ty":"text"}],
  "rows": [{"id":{"type":"int","value":"9007199254740993"},"title":{"type":"text","value":"hello"}}],
  "schema": {"revision":2,"hash":"..."},
  "page": {"limit":100,"direction":"forward","snapshot_sequence":"42","next_cursor":"u1...","previous_cursor":null,"has_more":true}
}
~~~

<code>columns</code> 决定展示和读取顺序，row object 只承载按名称访问的值。分页响应增加 `page`，cursor 缺失时字段省略；`has_more` 表示当前遍历方向还有数据。`explain` 响应额外包含 <code>plan</code>：源表、`full_scan`／`primary_key_lookup`／`secondary_index_lookup`、可选索引与 lookup 条件、候选行数、源码顺序 stage 和最终结果 schema；分页计划还包含唯一 order、boundary、sequence、`sorted_scan` 和预算。introspection 响应改为包含 <code>introspection</code>，receipt 运维响应包含 `receipts`，这些操作都不执行数据查询。失败响应的 <code>error</code> 包含固定 <code>code</code>、可读 <code>message</code> 和可选源码 <code>span</code>。DML 使用 <code>affected_rows</code>；单行 upsert 使用 <code>upsert_action</code>，批量 upsert 使用按输入顺序排列的 <code>upsert_actions</code> array。`returning` 直接复用相同的 typed columns/rows wire codec，不改变 version。旧 version 1 request 缺少新增字段时按 `None` 处理。warnings 不改变 <code>ok</code>。

## Rust 嵌入接口

<code>Engine::memory()</code> 和 <code>Engine::open_redb(path)</code> 创建数据库；<code>Engine::open_redb_read_only(path)</code> 只打开已存在的 redb，并建立返回 `E_READ_ONLY` 的 mutation 边界。已有 Engine 也可用 <code>with_read_only(true)</code> 配置 adapter。<code>execute</code> 执行无参数原子脚本，<code>execute_with_params</code> 在 AST 上绑定参数；`execute_page` 和 `execute_with_params_page` 接受结构化 `PageSpec`，`QueryResponse::typed_page` 返回 `TypedPage<T>`。<code>prepare</code> 接受只读 pipeline、explain、参数化单行／批量 insert/upsert，以及 update/delete；准备阶段不扫描数据，却会绑定表、target、set/match、returning 和参数类型。完整 row 参数显示命名 RowType，批量参数显示 <code>list RowType</code>。plan 记录当前 schema revision/hash；<code>query</code> 或 <code>execute_prepared</code> 执行时若 schema 已变化会返回 <code>E_SCHEMA_CHANGED</code>，调用方可重新 prepare。<code>execute_prepared_until</code> 为 prepared operation 增加 deadline。prepared 写入只在 memory/redb 执行，过渡 WAL 返回 <code>E_CONFIG</code>。Rust 调用方可直接读取 <code>QueryPlan</code>、<code>QueryAccessPlan</code>、`PageInfo` 和对应 enum。migration 继续通过 <code>plan_migrations</code>、<code>apply_migrations</code> 和 <code>migration_status</code> 进入同一个 Engine 提交边界。

可运行示例：

~~~bash
cargo run --example parameters
~~~

TCP 客户端可直接构造 <code>ProtocolRequest</code> 并调用 <code>cli::send_request</code>。<code>WireValue</code> 与 <code>Value</code> 之间提供无损转换；网络 codec、redb 的版本化 binary value codec 和内部 Rust enum 布局彼此独立。连接、执行与响应限制以及优雅关闭行为见[服务运行边界](SERVICE.md)。

HTTP/TCP Rust adapter 还可使用 `Request::query`、`Request::with_serde_param`、`Request::with_page`、`Response::typed_rows` 和 `Response::typed_page`，避免应用代码手工拆装 `WireValue` 或 cursor。`server::execute_protocol_request` 是使用内置 25 秒预算的统一执行入口；`execute_protocol_request_until` 接受 adapter 计算的绝对 deadline。两者都不启动 listener，也不规定认证、TLS、路由或部署策略。

当前稳定 wire 入口只返回完整 `Response` 或 bounded page。独立的 stream protocol version 1 已在 [RFC 0007](rfc/0007-cancellable-backpressured-streams.md) 定义 accepted/schema/row/complete/error NDJSON frame、server-issued operation capability 和 cancel control。transport-neutral Rust 核心已提供 `ConcurrentEngine::register_read`、`ReadOperation::start` 与 `ConcurrentEngine::cancel`，但 TCP/HTTP envelope 要等 #157 同时完成两种 adapter 后才开放；客户端不得预先发送该 envelope 或把 socket 断开当作取消确认。
