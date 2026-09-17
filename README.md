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

## 快速开始 / Quick start

`init` 与 `project check` 从 v0.6.0 起提供。需要 Rust 1.94 或更高版本；从 crates.io 安装后，可以在任意空目录完成首用流程，不需要 clone 本仓库：

`init` and `project check` are available from v0.6.0. With Rust 1.94 or newer, a crates.io installation can complete the first-use journey in any empty directory without cloning this repository:

```bash
cargo install unionid --locked
unionid init tasks
cd tasks
unionid project check --dir .
unionid migration apply --db data/tasks.redb --dir migrations
unionid run --db data/tasks.redb --file seed.unid
unionid run --db data/tasks.redb --file queries/list_running.unid
unionid doctor --db data/tasks.redb
unionid check --db data/tasks.redb
```

`project check` 应依次报告 schema、migrations 和 queries 通过；seed 写入两行，随后查询从新进程重开数据库并返回 `id = 1`、标题为 `learn ADTs` 的 `Running` task。`doctor` 只读诊断副本，`check` 验证原数据库。原生压缩包用户把 `bin` 加入 `PATH`，源码构建用户把 `target/debug` 加入 `PATH` 后，执行的是完全相同的命令。备份还原、三种安装入口和下一步见[五分钟入门](docs/GETTING_STARTED.md)。

`project check` should pass schema, migrations, and queries in that order. The seed writes two rows, then the query reopens the database in a new process and returns the `Running` task with `id = 1` and title `learn ADTs`. `doctor` diagnoses a private copy, while `check` verifies the original database. Native-archive users add `bin` to `PATH`, and source builds add `target/debug`; every entry then runs the same commands. See the [five-minute guide](docs/GETTING_STARTED.md) for backup/restore, all three installation entries, and next steps.

当前产品面向单机、一个数据库所有者和串行写入，约 10,000 行是舒适工作集；100,000 行只是已测试上限。通用扁平 join、window 和分布式执行不在当前范围内。

The current product targets one machine, one database owner, serialized writes, and a comfortable working set around 10,000 rows; 100,000 rows is a tested upper bound. General flattened joins, windows, and distributed execution remain outside the current scope.

当前还可直接声明 `uuid`、`bytes`、`date`、`timestamp`、`duration` 与 `decimal P S`：UUID 可作为主键，bytes 支持索引与二进制查询，temporal 值提供显式 offset，decimal 提供固定 scale 与 checked 精确算术。可运行示例见 [`content_metadata.unid`](examples/content_metadata.unid)、[`session_events.unid`](examples/session_events.unid) 与 [`invoices.unid`](examples/invoices.unid)。

The executable language also supports native `uuid`, `bytes`, `date`, `timestamp`, `duration`, and `decimal P S`: UUIDs can be primary keys, bytes support indexed binary queries, temporal values use explicit offsets, and decimals provide fixed-scale checked arithmetic. See [`content_metadata.unid`](examples/content_metadata.unid), [`session_events.unid`](examples/session_events.unid), and [`invoices.unid`](examples/invoices.unid).

## ADT 数据模型与查询 / ADT data model and queries

下面的 schema 同时使用积类型 `Task` 和和类型 `State`。`Running`、`Done`、`Failed` 各自拥有不同 payload；不属于该 variant 的字段根本不存在。

The schema below combines the product type `Task` with the sum type `State`. Each of `Running`, `Done`, and `Failed` has a distinct payload; fields that do not belong to a variant do not exist.

```text
enum State {
  Pending
  Running {
    worker: text
    attempt: int
  }
  Done {
    result: text
  }
  Failed {
    message: text
    retryable: bool
  }
}

struct Task {
  id: int
  title: text
  tags: List<text>
  state: State
}

table tasks: Task {
  key id
}

insert tasks {
  id: 1
  title: "sync directory"
  tags: ["sync", "local"]
  state: Running {worker: "worker-1", attempt: 2}
}
```

查询从上到下组合，并直接解构 `State`。match 必须覆盖所有可能形态，因此新增 variant 时不会被旧查询静默忽略。字段或 match 已确定 enum 类型时可省略 `State::`；独立构造或有歧义时仍可使用完整限定名。

Queries compose from top to bottom and destructure `State` directly. A match must cover every possible shape, so a newly added variant cannot be silently ignored by an old query. When a field or match fixes the enum type, `State::` may be omitted; standalone or ambiguous construction can still use the qualified name.

```text
from tasks
filter match state {
  Running {attempt, ..} => attempt >= 2
  Failed {retryable, ..} => retryable
  _ => false
}
derive state_label = match state {
  Pending => "pending"
  Running {worker, ..} => worker
  Done {result} => result
  Failed {message, ..} => message
}
select {id, title, state, state_label}
sort id
take 20
```

同一套 typed expression 可以执行原子状态转换。The same typed expressions drive atomic state transitions:

```text
update tasks
filter match state {
  Pending => true
  _ => false
}
set state = Running {worker: "worker-1", attempt: 1}
returning {id, state}
```

`filter`、`select`、`sort`、`take`、`page`、`derive`、`group`、`aggregate` 和查询局部 `let` 都是可组合 stage。稳定跨请求分页使用 `sort {-priority, id} | page 100`，并通过响应中的 opaque cursor 继续；排序必须以主键收尾。`explain from tasks | filter id == 1` 返回计划且不读取结果行；`explain analyze ...` 在同一读快照上实际执行并返回无业务数据的耗时、读取量与内存统计。完整语法见 [QUERY.md](docs/QUERY.md)。

`filter`, `select`, `sort`, `take`, `page`, `derive`, `group`, `aggregate`, and query-local `let` are composable stages. Stable cross-request traversal uses `sort {-priority, id} | page 100` and resumes with the opaque response cursor; the order must end in the primary key. `explain from tasks | filter id == 1` reports a plan without reading result rows; `explain analyze ...` executes on the same read snapshot and returns value-free timing, work, and memory observations. See [QUERY.md](docs/QUERY.md) for the complete executable surface.

安装后的 CLI 内置按类别组织、与二进制版本匹配的用户文档；不需要源码 checkout 或网络即可查阅入门、语言、应用、生命周期、集成和运维主题。`unionid docs` 显示目录，`docs list --category language` 筛选类别，`docs show query` 输出完整查询参考。LLM 和代码生成工具可继续用专门的 `docs query` 取得紧凑规则与可运行示例。默认 Markdown 可直接放入 prompt，version 1 JSON 便于工具读取。生成查询前再用 `schema print` 提供实际数据库 schema；保存后的查询可用 `query describe` 在不执行的情况下绑定检查：

The installed CLI bundles user documentation organized into learn, language, application, lifecycle, integration, and operations categories, all matched to the binary version and available without a source checkout or network access. `unionid docs` shows the catalog, `docs list --category language` filters it, and `docs show query` prints the complete query reference. LLMs and code generators can continue to use the dedicated `docs query` bundle for compact rules and runnable examples. Markdown is prompt-ready and version-1 JSON is tool-friendly. Pair generated queries with the database's actual schema, then bind a saved query without executing it:

```bash
unionid docs
unionid docs list --category language
unionid docs show query
unionid docs query
unionid docs query --format json
unionid schema print --db app.redb --format json
unionid query describe --db app.redb --file query.unid
```

The bundled reference is also available in [LLM_QUERY.md](docs/LLM_QUERY.md); it documents the Rust-shaped contextual enum shorthand, arrow closures, pipeline ordering, bounded reads, mutations, and a generation checklist.

服务观测可读取有版本、有限 cardinality 的 `ConcurrentEngine::metrics_snapshot()`；可选 `metrics` feature 只渲染 Prometheus 文本，不自动公开网络 endpoint。完整部署边界见 [METRICS.md](docs/METRICS.md)。

Service observability uses the versioned, cardinality-bounded `ConcurrentEngine::metrics_snapshot()`. The optional `metrics` feature only renders Prometheus text and never exposes a network endpoint automatically. See [METRICS.md](docs/METRICS.md) for deployment boundaries.

需要定位单个请求时，可显式配置值无关的 terminal/slow-query observer；request ID 默认省略，也可选择输出 HMAC 摘要。事件、阈值、采样和保留边界见 [OBSERVABILITY.md](docs/OBSERVABILITY.md)。

For individual-request diagnosis, applications can explicitly configure value-free terminal and slow-query observers. Request IDs stay absent by default and may be represented by an opt-in HMAC digest. See [OBSERVABILITY.md](docs/OBSERVABILITY.md) for event, threshold, sampling, and retention rules.

## Rust typed API

Rust 应用可以把自己的 `struct`、`enum`、`Option`、tuple 和 `Vec` 直接绑定到 prepared operation，再把结果解码回应用类型。

短小且只由一个 Rust crate 使用的查询也可以通过 `unionid_query::queries!` 直接内联。宏在编译期读取声明式 schema，复用同一 parser/binder/codegen 生成共享 ADT、typed Params/Row 和执行函数；运行时仍检查 schema identity。完整用法与 migration 边界见 [内联 Rust 查询](docs/RUST_QUERY_MACRO.md)。独立 `.unid` 查询继续使用 `query describe` / `query rust`，适合跨语言、CLI、LLM 和较大查询。

Short queries owned by one Rust crate can also use inline `unionid_query::queries!`. At compile time the macro reads a declarative schema and reuses the same parser, binder, and code generator to produce shared ADTs, typed Params/Rows, and execution functions; runtime schema-identity checks remain in place. See [Inline Rust queries](docs/RUST_QUERY_MACRO.md) for usage and migration boundaries. Standalone `.unid` queries retain `query describe` / `query rust` for cross-language, CLI, LLM, and larger-query workflows.

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
db.execute(r#"enum State {
  Pending
  Running {worker: text, attempt: int}
}
struct Task {
  id: int
  title: text
  state: State
}
table tasks: Task {
  key id
}"#);

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

查询语言也支持有界的一对多展开：`lookup lines from order_lines on order_id == id take 100` 为每个 driver row 增加 typed `List<Line>`，缺失关联为 `[]`。目标 key 必须有索引，逐行上限必须显式给出；分页时把 lookup 放在 `page` 后面，先限制 driver rows 再组装嵌套 ADT。

Version-1 mutation requests can use `with_idempotency_key` for exactly-once effects across TCP/HTTP retries, with first-commit or replay metadata in the response. Receipts never expire automatically; inspect and explicitly prune a bounded preview with `unionid receipts status/prune`.

Batched relational reads use `Engine::fetch_by_key` / `typed_fetch_by_key` against a primary key or an indexed column: results match the input keys in length and order, missing keys are `None`, and the call runs over one consistent snapshot. Each key is validated for uniqueness at runtime; a non-indexed key or a non-unique match returns `E_RELATION_KEY` or `E_RELATION_NOT_UNIQUE`. See [RFC 0013](docs/rfc/0013-minimal-relational-reads.md).

The query language also supports bounded one-to-many expansion: `lookup lines from order_lines on order_id == id take 100` adds a typed `List<Line>` to each driver row, with `[]` for no match. The target key must be indexed and the per-row bound is explicit. In paginated queries, lookup follows `page`, so driver rows are bounded before nested ADTs are assembled.

可运行代码见 [`parameters.rs`](examples/parameters.rs)。完整 HTTP/Axum todolist 通过相同 version 1 数据协议验证 ADT、typed cursor 分页、真实客户端断开、丢响应后的幂等重试、migration、重启、检查和备份还原，见 [HTTP.md](docs/HTTP.md)。

远程调用可用同步 `TcpClient`；启用 `asynchronous` feature 后，cloneable `AsyncTcpClient` 复用 TCP 连接，并提供 typed page、独立 NDJSON stream/cancel、绝对 deadline 和带幂等 key 的丢响应重连。启用 `http-client` feature 后，异步 `HttpClient` 提供对应的连接池、typed page、stream/cancel 和安全重试体验。异步框架也可用同一 `asynchronous` feature 调用 blocking Engine 入口；`http` feature 的 `unionid::asynchronous::http::router` 提供 versioned query、NDJSON stream 和 cancel 路由。`ConcurrentEngine` 等集成类型已在 crate root 导出。

See runnable code in [`parameters.rs`](examples/parameters.rs). The complete HTTP/Axum todolist validates ADTs, typed cursor pages, a real client disconnect, idempotent retry after a lost response, migrations, restart, integrity checking, backup, and restore through the same version 1 data protocol; see [HTTP.md](docs/HTTP.md).

Remote calls can use the synchronous `TcpClient`. With the `asynchronous` feature, cloneable `AsyncTcpClient` reuses its TCP connection and adds typed pages, dedicated NDJSON stream/cancel connections, absolute deadlines, and idempotency-key guarded reconnect after a lost response. The `http-client` feature supplies the corresponding pooled HTTP typed page, stream/cancel, and safe retry experience. Async frameworks can also use `asynchronous` for the shared blocking Engine entry point, while `http` supplies versioned query, NDJSON stream, and cancellation routes. Integration types such as `ConcurrentEngine` are re-exported at the crate root.

## 当前边界 / Current boundaries

当前实现面向单机、单数据库所有者和约一万行的舒适工作集；十万行是已测试上限，不是日常目标。format-5 Legacy0 和 format-6 active generation 的普通 open、indexed/page read、融合 full pipeline、完整 check 与 logical backup 使用有界 row source。显式启用增量备份会进入 format 7，并通过 portable baseline/segment chain 提供按 sequence 恢复。最新 100k M7 复验中 open p95 为 12.02 ms、主键查询 p95 为 24 µs、完整 check 为 1.54 s／96.33 MiB；完整 shadow migration p95 为 16.51 s／427.98 MiB，仍应按维护操作安排。详见 [M7 验收记录](docs/benchmarks/m7-acceptance-2026-09-10.md)。当前不提供通用扁平 join、window 或分布式执行；受控网络通过官方维护的 Envoy mTLS 部署路径接入。

The current implementation targets a single machine, one database owner, and a comfortable working set around 10,000 rows. A 100,000-row workload is a tested upper bound rather than the routine target. Ordinary format-5 Legacy0 and format-6 active-generation open, indexed/page reads, fused full pipelines, explicit checks, and logical backups use bounded row sources. Explicit incremental-backup enablement enters format 7 and provides sequence restore through a portable baseline/segment chain. In the final 100k M7 run, open p95 was 12.02 ms, primary-key query p95 was 24 µs, and full check took 1.54 seconds and 96.33 MiB. Complete shadow-migration p95 was 16.51 seconds with 427.98 MiB peak RSS, so it remains a planned maintenance operation. See the [M7 acceptance record](docs/benchmarks/m7-acceptance-2026-09-10.md). General flattened joins, windows, and distributed execution remain out of scope; controlled networks use the officially maintained Envoy mTLS deployment path.

## 文档 / Documentation

| 主题 / Topic | 文档 / Document |
| --- | --- |
| 五分钟端到端教程 / Five-minute end-to-end guide | [GETTING_STARTED.md](docs/GETTING_STARTED.md) |
| 当前语言与 query stage / Current language and query stages | [LANGUAGE.md](docs/LANGUAGE.md) · [QUERY.md](docs/QUERY.md) |
| Rust、TCP 与 HTTP 数据协议 / Rust, TCP, and HTTP data protocol | [PROTOCOL.md](docs/PROTOCOL.md) · [HTTP.md](docs/HTTP.md) |
| Schema 身份与 migration / Schema identity and migrations | [SCHEMA.md](docs/SCHEMA.md) · [MIGRATIONS.md](docs/MIGRATIONS.md) |
| 持久化、备份与生产边界 / Storage, backup, and production boundaries | [STORAGE.md](docs/STORAGE.md) · [BACKUP.md](docs/BACKUP.md) · [SERVICE.md](docs/SERVICE.md) · [DEPLOYMENT.md](docs/DEPLOYMENT.md) |
| 发布契约、历史说明与升级 / Release contract, historical notes, and upgrades | [contract.json](release/contract.json) · [RELEASE-v0.9.0.md](docs/RELEASE-v0.9.0.md) · [UPGRADING.md](docs/UPGRADING.md) |
| 实际场景与后续计划 / Real scenarios and roadmap | [SCENARIOS.md](docs/SCENARIOS.md) · [ROADMAP.md](docs/ROADMAP.md) |
| 实现与验证记录 / Implementation and validation history | [DEVELOPMENT.md](docs/DEVELOPMENT.md) |
| 贡献者与 agent 约定 / Contributor and agent conventions | [Agents.md](Agents.md) |

原型兼容、存储内部结构、测试矩阵和历史进度不在 README 重复维护，统一由上述专题文档承载。

Prototype compatibility, storage internals, the test matrix, and historical progress are maintained in the focused documents above instead of being duplicated here.

## License

MIT
