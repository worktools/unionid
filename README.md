# unionid

[中文](#中文介绍) · [English](#english-introduction) · [快速开始 / Quick start](#快速开始--quick-start) · [文档 / Documentation](#文档--documentation)

## 中文介绍

unionid 是一个用 Rust 编写、直接支持代数数据类型（Algebraic Data Types，ADT）和 pipeline 查询的轻量数据库。它可以作为嵌入式 Rust Engine、本地 redb 数据库或 TCP 服务运行。

它围绕两个核心特点设计：

1. **数据库直接用 ADT 描述数据。** `struct` 对应的积类型、`enum` 对应的和类型，以及 `option`、`list`、tuple 和有限递归类型都是 schema 的一部分，而不是藏在无类型 JSON 中的应用约定。数据库会检查 constructor、payload、默认值、主键、索引和 migration。
2. **查询语言直接理解 ADT。** PRQL 风格的 pipeline 可以对 variant 做穷尽模式匹配、解构嵌套字段并构造新的 typed value；查询、更新、参数绑定和 `returning` 共用相同类型语义。

因此，一个任务状态不必由 `status = "running"` 和若干 nullable 字段模拟。schema 能准确表达每种合法形态，query 也能直接匹配这些形态。

## English introduction

unionid is a lightweight Rust database with algebraic data types (ADTs) and pipeline queries built directly into its data model. It runs as an embedded Rust Engine, a local redb database, or a TCP service.

It is designed around two defining ideas:

1. **Describe database data directly with ADTs.** Product types corresponding to Rust structs, sum types corresponding to Rust enums, plus options, lists, tuples, and finite recursive types are part of the schema—not application conventions hidden in untyped JSON. The database validates constructors, payloads, defaults, keys, indexes, and migrations.
2. **Query ADTs as ADTs.** The PRQL-style pipeline language can exhaustively match variants, destructure nested fields, and construct new typed values. Reads, updates, parameter binding, and `returning` share the same type semantics.

A task state therefore does not need to be simulated with `status = "running"` and nullable payload columns. The schema describes every valid shape precisely, and queries match those shapes directly.

当前还可直接声明 `uuid`、`bytes`、`date`、`timestamp`、`duration` 与 `decimal P S`：UUID 可作为主键，bytes 支持索引与二进制查询，temporal 值提供显式 offset，decimal 提供固定 scale 与 checked 精确算术。可运行示例见 [`content_metadata.uid`](examples/content_metadata.uid)、[`session_events.uid`](examples/session_events.uid) 与 [`invoices.uid`](examples/invoices.uid)。

The executable language also supports native `uuid`, `bytes`, `date`, `timestamp`, `duration`, and `decimal P S`: UUIDs can be primary keys, bytes support indexed binary queries, temporal values use explicit offsets, and decimals provide fixed-scale checked arithmetic. See [`content_metadata.uid`](examples/content_metadata.uid), [`session_events.uid`](examples/session_events.uid), and [`invoices.uid`](examples/invoices.uid).

## ADT 数据模型与查询 / ADT data model and queries

下面的 schema 同时使用积类型 `Task` 和和类型 `State`。`Running`、`Done`、`Failed` 各自拥有不同 payload；不属于该 variant 的字段根本不存在。

The schema below combines the product type `Task` with the sum type `State`. Each of `Running`, `Done`, and `Failed` has a distinct payload; fields that do not belong to a variant do not exist.

```text
type State =
  Pending
  | Running {
    worker text,
    attempt int,
  }
  | Done {
    result text,
  }
  | Failed {
    message text,
    retryable bool,
  }

type Task = {
  id int,
  title text,
  tags list text,
  state State,
}

table tasks Task
  key id

insert tasks {
  id = 1,
  title = "sync directory",
  tags = ["sync", "local"],
  state = Running {worker = "worker-1", attempt = 2},
}
```

查询从上到下组合，并直接解构 `State`。match 必须覆盖所有可能形态，因此新增 variant 时不会被旧查询静默忽略。

Queries compose from top to bottom and destructure `State` directly. A match must cover every possible shape, so a newly added variant cannot be silently ignored by an old query.

```text
from tasks
filter (
  match state {
    Running {attempt, ..} => attempt >= 2,
    Failed {retryable, ..} => retryable,
    _ => false,
  }
)
derive state_label = match state {
  Pending => "pending",
  Running {worker, ..} => worker,
  Done {result} => result,
  Failed {message, ..} => message,
}
select {id, title, state, state_label}
sort id
take 20
```

同一套 typed expression 可以执行原子状态转换。The same typed expressions drive atomic state transitions:

```text
update tasks
filter (
  match state {
    Pending => true,
    _ => false,
  }
)
set state = Running {worker = "worker-1", attempt = 1}
returning {id, state}
```

`filter`、`select`、`sort`、`take`、`page`、`derive`、`group`、`aggregate` 和查询局部 `let` 都是可组合 stage。稳定跨请求分页使用 `sort {-priority, id} | page 100`，并通过响应中的 opaque cursor 继续；排序必须以主键收尾。`explain from tasks | filter id == 1` 返回主键／索引访问方式、候选行数、stage 顺序和结果 schema，但不读取结果行。完整语法见 [QUERY.md](docs/QUERY.md)。

`filter`, `select`, `sort`, `take`, `page`, `derive`, `group`, `aggregate`, and query-local `let` are composable stages. Stable cross-request traversal uses `sort {-priority, id} | page 100` and resumes with the opaque response cursor; the order must end in the primary key. `explain from tasks | filter id == 1` reports primary-key/index access, candidate rows, stage order, and result schema without reading result rows. See [QUERY.md](docs/QUERY.md) for the complete executable surface.

## 快速开始 / Quick start

需要 Rust 1.94 或更高版本。当前公开版本是 `0.2.0`，可直接从 crates.io 安装。

Rust 1.94 or newer is required. The current public release is `0.2.0` and can be installed directly from crates.io.

```bash
cargo install unionid --version 0.2.0 --locked
git clone --depth 1 https://github.com/worktools/unionid.git
cd unionid
unionid run --file examples/tasks.uid
```

`run` 默认使用临时内存库；传入 redb 路径即可持久化。一个源码请求是一个原子批次。

`run` uses a fresh in-memory database by default; pass a redb path for durable use. One source request is one atomic batch.

```bash
unionid run --db ./data/app.redb --file examples/tasks.uid
unionid run --db ./data/app.redb --query 'from tasks | filter id == 1'
unionid cli --db ./data/app.redb
unionid check --db ./data/app.redb
```

启动普通 TCP 服务或明确的只读查询服务。Start a regular TCP service or an explicit read-only query service:

```bash
unionid server --db ./data/app.redb --addr 127.0.0.1:7878
unionid server --db ./data/app.redb --read-only --addr 127.0.0.1:7878
```

只读模式只打开已有 redb，并在进入持久事务前以 `E_READ_ONLY` 拒绝整个 mutation 批次。Read-only mode opens an existing redb database and rejects the complete mutation batch with `E_READ_ONLY` before entering a durable transaction.

## Rust typed API

Rust 应用可以把自己的 `struct`、`enum`、`Option`、tuple 和 `Vec` 直接绑定到 prepared operation，再把结果解码回应用类型。

Rust applications can bind their own structs, enums, options, tuples, and vectors directly to prepared operations, then decode rows back into application types.

```rust
use std::collections::BTreeMap;
use serde::{Deserialize, Serialize};
use unionid::{Engine, Value};

#[derive(Debug, PartialEq, Serialize, Deserialize)]
enum State {
    Pending,
    Running { worker: String, attempt: i64 },
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Task {
    id: i64,
    title: String,
    state: State,
}

let mut db = Engine::memory();
db.execute(r#"type State =
  Pending
  | Running {worker text, attempt int}
type Task = {
  id int,
  title text,
  state State,
}
table tasks Task
  key id"#);

let row = Task {
    id: 1,
    title: "ship it".into(),
    state: State::Running { worker: "local".into(), attempt: 1 },
};
let insert = db.prepare("insert tasks $row\nreturning").unwrap();
let response = db.execute_prepared(
    &insert,
    BTreeMap::from([("row".into(), Value::from_serde(&row).unwrap())]),
);
assert_eq!(response.typed_rows::<Task>().unwrap(), [row]);
```

网络协议使用显式 typed wire values，保留完整 `i64` 精度、命名类型身份和 `None`/`null` 的区别；Rust helper 通常不需要应用手写标签。

The wire protocol uses explicit typed values to preserve full `i64` precision, nominal type identity, and the distinction between `None` and `null`; Rust helpers normally avoid manual tag construction.

Version 1 mutation 请求可通过 `with_idempotency_key` 获得跨 TCP/HTTP 重试的 exactly-once effect；服务端返回首次提交或 replay 元数据。回执不会自动过期，使用 `unionid receipts status/prune` 先预览、再显式有界清理。

关联读可用 `Engine::fetch_by_key` / `typed_fetch_by_key` 按主键或已索引列批量取回，结果与输入键等长同序、缺失键为 `None`，并基于一次一致快照；每个键在运行时校验唯一性，未索引键或非唯一命中分别返回 `E_RELATION_KEY` / `E_RELATION_NOT_UNIQUE`。设计见 [RFC 0013](docs/rfc/0013-minimal-relational-reads.md)。

查询语言也支持有界的一对多展开：`lookup lines from order_lines on order_id == id take 100` 为每个 driver row 增加 typed `list Line`，缺失关联为 `[]`。目标 key 必须有索引，逐行上限必须显式给出；分页时把 lookup 放在 `page` 后面，先限制 driver rows 再组装嵌套 ADT。

Version-1 mutation requests can use `with_idempotency_key` for exactly-once effects across TCP/HTTP retries, with first-commit or replay metadata in the response. Receipts never expire automatically; inspect and explicitly prune a bounded preview with `unionid receipts status/prune`.

Batched relational reads use `Engine::fetch_by_key` / `typed_fetch_by_key` against a primary key or an indexed column: results match the input keys in length and order, missing keys are `None`, and the call runs over one consistent snapshot. Each key is validated for uniqueness at runtime; a non-indexed key or a non-unique match returns `E_RELATION_KEY` or `E_RELATION_NOT_UNIQUE`. See [RFC 0013](docs/rfc/0013-minimal-relational-reads.md).

The query language also supports bounded one-to-many expansion: `lookup lines from order_lines on order_id == id take 100` adds a typed `list Line` to each driver row, with `[]` for no match. The target key must be indexed and the per-row bound is explicit. In paginated queries, lookup follows `page`, so driver rows are bounded before nested ADTs are assembled.

可运行代码见 [`parameters.rs`](examples/parameters.rs)。完整 HTTP/Axum todolist 通过相同 version 1 数据协议验证 ADT、typed cursor 分页、真实客户端断开、丢响应后的幂等重试、migration、重启、检查和备份还原，见 [HTTP.md](docs/HTTP.md)。

远程调用可用 `unionid::client::TcpClient`（`request`/`request_retrying` 加 `typed_rows`）。启用 `http-client` feature 后，异步 `HttpClient` 提供连接池、typed page 续读、逐行解码的 typed NDJSON stream、显式 cancel，以及带幂等 key 的丢响应重试。异步框架可启用 `asynchronous` feature 调用同一 blocking 入口；`http` feature 的 `unionid::asynchronous::http::router` 直接提供 versioned query、NDJSON stream 和 cancel 路由，应用无需复制 worker 或流桥接。`ConcurrentEngine` 等集成类型已在 crate root 导出。

See runnable code in [`parameters.rs`](examples/parameters.rs). The complete HTTP/Axum todolist validates ADTs, typed cursor pages, a real client disconnect, idempotent retry after a lost response, migrations, restart, integrity checking, backup, and restore through the same version 1 data protocol; see [HTTP.md](docs/HTTP.md).

Remote calls can use `unionid::client::TcpClient` (`request`/`request_retrying` plus `typed_rows`). With the `http-client` feature, async `HttpClient` adds connection pooling, typed page continuation, row-by-row typed NDJSON streams, explicit cancellation, and lost-response retries guarded by idempotency keys. Async frameworks can enable `asynchronous` for the shared blocking entry point; the `http` feature supplies versioned query, NDJSON stream, and cancellation routes through `unionid::asynchronous::http::router`. Integration types such as `ConcurrentEngine` are re-exported at the crate root.

## 当前边界 / Current boundaries

v0.2.0 面向单机、单数据库所有者和约一万行的舒适工作集；十万行是已测试上限，不是日常目标。format-5 Legacy0 和 format-6 active generation 的普通 open、indexed/page read、融合 full pipeline、完整 check 与 logical backup 使用有界 row source。最新 100k M7 复验中 open p95 为 12.02 ms、主键查询 p95 为 24 µs、完整 check 为 1.54 s／96.33 MiB；完整 shadow migration p95 为 16.51 s／427.98 MiB，仍应按维护操作安排。详见 [M7 验收记录](docs/benchmarks/m7-acceptance-2026-09-10.md)。当前不提供内置认证、TLS、通用扁平 join、window 或分布式执行。

v0.2.0 targets a single machine, one database owner, and a comfortable working set around 10,000 rows. A 100,000-row workload is a tested upper bound rather than the routine target. Ordinary format-5 Legacy0 and format-6 active-generation open, indexed/page reads, fused full pipelines, explicit checks, and logical backups use bounded row sources. In the final 100k M7 run, open p95 was 12.02 ms, primary-key query p95 was 24 µs, and full check took 1.54 seconds and 96.33 MiB. Complete shadow-migration p95 was 16.51 seconds with 427.98 MiB peak RSS, so it remains a planned maintenance operation. See the [M7 acceptance record](docs/benchmarks/m7-acceptance-2026-09-10.md). Built-in authentication, TLS, general flattened joins, windows, and distributed execution are currently out of scope.

## 文档 / Documentation

| 主题 / Topic | 文档 / Document |
| --- | --- |
| 五分钟端到端教程 / Five-minute end-to-end guide | [GETTING_STARTED.md](docs/GETTING_STARTED.md) |
| 当前语言与 query stage / Current language and query stages | [LANGUAGE.md](docs/LANGUAGE.md) · [QUERY.md](docs/QUERY.md) |
| Rust、TCP 与 HTTP 数据协议 / Rust, TCP, and HTTP data protocol | [PROTOCOL.md](docs/PROTOCOL.md) · [HTTP.md](docs/HTTP.md) |
| Schema 身份与 migration / Schema identity and migrations | [SCHEMA.md](docs/SCHEMA.md) · [MIGRATIONS.md](docs/MIGRATIONS.md) |
| 持久化、备份与生产边界 / Storage, backup, and production boundaries | [STORAGE.md](docs/STORAGE.md) · [BACKUP.md](docs/BACKUP.md) · [SERVICE.md](docs/SERVICE.md) |
| v0.2 版本契约与发布说明 / v0.2 contract and release notes | [contract.json](release/contract.json) · [RELEASE-v0.2.0.md](docs/RELEASE-v0.2.0.md) · [UPGRADING.md](docs/UPGRADING.md) |
| 实际场景与后续计划 / Real scenarios and roadmap | [SCENARIOS.md](docs/SCENARIOS.md) · [ROADMAP.md](docs/ROADMAP.md) |
| 实现与验证记录 / Implementation and validation history | [DEVELOPMENT.md](docs/DEVELOPMENT.md) |
| 贡献者与 agent 约定 / Contributor and agent conventions | [Agents.md](Agents.md) |

原型兼容、存储内部结构、测试矩阵和历史进度不在 README 重复维护，统一由上述专题文档承载。

Prototype compatibility, storage internals, the test matrix, and historical progress are maintained in the focused documents above instead of being duplicated here.

## License

MIT
