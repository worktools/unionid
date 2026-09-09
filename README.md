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

需要 Rust 1.94 或更高版本。Rust 1.94 or newer is required.

```bash
cargo install unionid --version 0.1.0 --locked
git clone https://github.com/worktools/unionid.git
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

Version-1 mutation requests can use `with_idempotency_key` for exactly-once effects across TCP/HTTP retries, with first-commit or replay metadata in the response. Receipts never expire automatically; inspect and explicitly prune a bounded preview with `unionid receipts status/prune`.

可运行代码见 [`parameters.rs`](examples/parameters.rs)。完整 HTTP/Axum todolist 通过相同 version 1 数据协议验证 ADT、typed cursor 分页、真实客户端断开、丢响应后的幂等重试、migration、重启、检查和备份还原，见 [HTTP.md](docs/HTTP.md)。

See runnable code in [`parameters.rs`](examples/parameters.rs). The complete HTTP/Axum todolist validates ADTs, typed cursor pages, a real client disconnect, idempotent retry after a lost response, migrations, restart, integrity checking, backup, and restore through the same version 1 data protocol; see [HTTP.md](docs/HTTP.md).

## 当前边界 / Current boundaries

v0.1.0 面向单机、单数据库所有者和约一万行的舒适工作集；十万行是已测试上限，不是日常目标。format-5 redb 普通打开和 indexed/page read 已不再加载完整 rows/indexes：最新 100k 结构测量中 open 约 4 ms，随后冷查询只解码 1 行，open 与查询峰值约 8.23 MiB。完整 check 仍约 5.16 s／554 MiB，既有深层 migration 测量仍约 50 s／1.44 GiB，因此维护路径仍需按大操作规划。详见 [Legacy0 有界读取记录](docs/benchmarks/bounded-legacy-read-2026-09-09.md)与[完整工作负载记录](docs/benchmarks/workload-2026-09-09.md)。当前不提供内置认证、TLS、join、window 或分布式执行。

v0.1.0 targets a single machine, one database owner, and a comfortable working set around 10,000 rows. A 100,000-row workload is a tested upper bound rather than the routine target. Ordinary format-5 redb open and indexed/page reads no longer load all rows and indexes: the latest 100k structural run opened in about 4 ms, decoded one row for the cold lookup, and peaked near 8.23 MiB across open and lookup. Full check still takes about 5.16 seconds and 554 MiB, while the existing deep-migration measurement remains near 50 seconds and 1.44 GiB, so maintenance still needs large-operation planning. See the [bounded Legacy0 read record](docs/benchmarks/bounded-legacy-read-2026-09-09.md) and [full workload record](docs/benchmarks/workload-2026-09-09.md). Built-in authentication, TLS, joins, windows, and distributed execution are currently out of scope.

## 文档 / Documentation

| 主题 / Topic | 文档 / Document |
| --- | --- |
| 五分钟端到端教程 / Five-minute end-to-end guide | [GETTING_STARTED.md](docs/GETTING_STARTED.md) |
| 当前语言与 query stage / Current language and query stages | [LANGUAGE.md](docs/LANGUAGE.md) · [QUERY.md](docs/QUERY.md) |
| Rust、TCP 与 HTTP 数据协议 / Rust, TCP, and HTTP data protocol | [PROTOCOL.md](docs/PROTOCOL.md) · [HTTP.md](docs/HTTP.md) |
| Schema 身份与 migration / Schema identity and migrations | [SCHEMA.md](docs/SCHEMA.md) · [MIGRATIONS.md](docs/MIGRATIONS.md) |
| 持久化、备份与生产边界 / Storage, backup, and production boundaries | [STORAGE.md](docs/STORAGE.md) · [BACKUP.md](docs/BACKUP.md) · [SERVICE.md](docs/SERVICE.md) |
| 实际场景与后续计划 / Real scenarios and roadmap | [SCENARIOS.md](docs/SCENARIOS.md) · [ROADMAP.md](docs/ROADMAP.md) |
| 实现与验证记录 / Implementation and validation history | [DEVELOPMENT.md](docs/DEVELOPMENT.md) |
| 贡献者与 agent 约定 / Contributor and agent conventions | [Agents.md](Agents.md) |

原型兼容、存储内部结构、测试矩阵和历史进度不在 README 重复维护，统一由上述专题文档承载。

Prototype compatibility, storage internals, the test matrix, and historical progress are maintained in the focused documents above instead of being duplicated here.

## License

MIT
