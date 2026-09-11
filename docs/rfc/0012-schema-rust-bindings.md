# RFC 0012：从 schema 生成 Rust 绑定 / generate Rust bindings from the schema

- 状态 / Status: accepted, first slice implemented
- 日期 / Date: 2026-09-12
- 跟踪 / Tracking: [#240](https://github.com/worktools/unionid/issues/240)

## 中文说明

### 1. 问题

应用必须在两处声明同一份领域模型：unionid 的 `type`/`table` DDL，以及 Rust 侧的 `struct`/`enum` + serde。没有 derive 宏或 codegen 时，字段改名、新增 variant、调整 option/list 只能人工同步，漂移只能在运行时由 `prepare`/`Value::from_serde` 暴露。这是把 ADT 当核心价值的应用最先遇到的问题。

### 2. 决策

首个切片做**单向 schema → Rust codegen**，而不是 proc-macro：

- 新公开 API `unionid::codegen::rust(source) -> Result<String>`；新 CLI `unionid schema rust [--file <schema.uid>] [--db <db.redb>] [--output <path>]`。
- 生成的 Rust 只使用 `serde` 与 `unionid::scalars`，直接接入现有 typed API（`Value::from_serde`、`typed_rows`、prepared params、returning）。
- 保持核心 Engine 精简，不新增 proc-macro 依赖或 workspace 结构；Rust 侧 `#[derive]` 宏与反向 Rust → DDL 留作后续切片，由真实需求触发。
- 先覆盖命名 record/sum、tuple、option/list、有限自递归 ADT、六类生产标量与 `field type` 默认值之外的形状。

选择 codegen 优先的理由：无需改变 crate 结构、可在不依赖编译器宏展开的情况下测试、并且对“已有 schema、想生成应用类型”的用户立即有用。proc-macro 的优势（Rust 类型为单一来源）在反向方向，另行设计。

### 3. 语义

- record → `pub struct`，字段用 `pub name: T`；`option T` → `Option<T>`，`list T` → `Vec<T>`，tuple → Rust tuple，命名 tuple 生成 `pub type`。
- sum → `pub enum`：unit variant、单位置负载 `Variant(T)`、record 负载 `Variant { .. }`、多位置负载 `Variant(A, B)`。
- 直接自递归（如 `Neg Expr`）在 Rust 需要间接层，生成 `Box<T>`；经 `list` 的递归由 `Vec` 已经间接，不再 box。
- 标量映射：`int→i64`、`float→f64`、`bool→bool`、`text→String`、`uuid/date/timestamp/duration/decimal/bytes → unionid::scalars::*`。
- Rust 关键字字段名加 `_` 后缀，同时保留 `#[serde(rename = "原名")]`，使 typed 序列化仍使用 schema 名称；转义后与另一个标识符冲突（如 `match` 与 `match_`）时返回 `E_SCHEMA`；未声明的匿名 record/enum 类型报 `E_SCHEMA`，要求先命名。
- schema 文件仍只允许 type/table/index 声明；完整脚本不作为 codegen 输入。

### 4. 验证

- `codegen::rust` 对 `examples/schema.uid`、递归 ADT、六类标量、关键字字段名生成正确的 Rust。
- CLI `--file`、`--db`、`--output` 三条路径经真实子进程验证。
- 非 schema 脚本返回 `E_SCHEMA`。

### 5. 后续

- 反向 Rust → DDL（proc-macro 或 codegen），让 Rust 类型成为单一来源。
- 从 catalog 生成类型时保留稳定 ID 与 migration 兼容提示。
- 多语言 typed client 生成（与 [#192](https://github.com/worktools/unionid/issues/192) 协同）。

## English Description

### 1. Problem

Applications declare the same domain model twice: unionid `type`/`table` DDL and Rust `struct`/`enum` with serde. Without a derive macro or codegen, renames, new variants, and option/list changes must be synced by hand, and drift surfaces only at runtime via `prepare`/`Value::from_serde`.

### 2. Decision

The first slice implements one-way **schema -> Rust codegen** rather than a proc-macro:

- New public API `unionid::codegen::rust(source) -> Result<String>` and CLI `unionid schema rust [--file <schema.uid>] [--db <db.redb>] [--output <path>]`.
- Generated Rust uses only `serde` and `unionid::scalars` and plugs into the existing typed API (`Value::from_serde`, `typed_rows`, prepared params, returning).
- Keep the core Engine lean with no proc-macro dependency or workspace change; the Rust `#[derive]` macro and the reverse Rust -> DDL direction are follow-up slices driven by real demand.
- Cover named records/sums, tuples, option/list, finite self-recursive ADTs, and the six production scalars.

Codegen first because it needs no crate-structure change, is testable without macro expansion, and is immediately useful to users who already have a schema and want application types. The proc-macro advantage (Rust types as the single source) lives in the reverse direction and is designed separately.

### 3. Semantics

- record -> `pub struct` with `pub name: T`; `option T` -> `Option<T>`, `list T` -> `Vec<T>`, tuples map to Rust tuples, and named tuples become `pub type`.
- sum -> `pub enum`: unit, single positional `Variant(T)`, record `Variant { .. }`, and multi-positional `Variant(A, B)`.
- Direct self-recursion (e.g. `Neg Expr`) needs indirection in Rust and becomes `Box<T>`; recursion through `list` is already indirected by `Vec` and is not boxed.
- Scalars: `int->i64`, `float->f64`, `bool->bool`, `text->String`, `uuid/date/timestamp/duration/decimal/bytes -> unionid::scalars::*`.
- Rust keyword field names get a `_` suffix plus `#[serde(rename = "original")]` so typed serialization keeps the schema name; a collision after escaping (for example `match` and `match_`) returns `E_SCHEMA`; undeclared anonymous record/enum types return `E_SCHEMA` and must be named.
- Schema files still accept only type/table/index declarations; full scripts are not codegen input.

### 4. Validation

- `codegen::rust` emits correct Rust for `examples/schema.uid`, recursive ADTs, the six scalars, and keyword field names.
- The CLI `--file`, `--db`, and `--output` paths are verified through real subprocesses.
- Non-schema scripts return `E_SCHEMA`.

### 5. Follow-up

- Reverse Rust -> DDL (proc-macro or codegen) so Rust types are the single source.
- Preserve stable IDs and migration-compatibility hints when generating from a catalog.
- Multi-language typed client generation (coordinate with [#192](https://github.com/worktools/unionid/issues/192)).
