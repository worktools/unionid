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

- record → `pub struct`，字段用 `pub name: T`；`option T` → `Option<T>`，`list T` → `Vec<T>`，tuple → Rust tuple，命名 scalar/tuple 生成 serde-transparent tuple newtype。
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

- 从 catalog 生成类型时保留稳定 ID 与 migration 兼容提示。
- 多语言 typed client 生成（与 [#192](https://github.com/worktools/unionid/issues/192) 协同）。
- 反向 derive 的属性扩展：字段默认值、table/index 名称、是否唯一等更细粒度声明。

### 6. 反向：Rust → DDL（已实现）

`unionid-derive` 提供 `#[derive(UnionidSchema)]`，把 Rust struct/enum 映射为 unionid `type` 声明，并用 `#[unionid(table = "...", key = "...")]` 生成 `table` 声明。生成代码实现 `unionid::UnionidSchema`，通过 `unionid::SchemaBuilder` 汇总为可直接执行的 schema 脚本。`SchemaBuilder::build()` 以依赖顺序输出类型（被引用类型先声明，注册顺序无关），缺失依赖或不受支持的互递归返回 `E_SCHEMA`（`build` 返回 `Result`）。

- 类型映射：`i64→int`、`f64→float`、`bool→bool`、`String/&'static str→text`、`Uuid/Date/Timestamp/Duration/Bytes→uuid/date/timestamp/duration/bytes`、`Decimal` 需 `#[unionid(decimal = "P S")]`；`Option<T>→option (T)`、`Vec<T>→list (T)`、`Box<T>→T`、tuple 与嵌套命名类型按名引用。`i32/f32` 等无法覆盖数据库完整值域的数值类型在 derive 阶段拒绝。
- record struct → `type Name = { field type, ... }`；enum 的 unit / 单字段 / 多字段 / record variant 分别生成 `Variant`、`Variant ty`、`Variant (a, b)`、`Variant { field type }`。
- 泛型类型与 union 明确拒绝；匿名 tuple struct 不是 record，报错。
- 该 crate 与核心 `unionid` 解耦（`unionid` 不依赖 `unionid-derive`），应用同时声明两个依赖；避免影响 `unionid` 的 crates.io 发布契约。发布工作流会先校验并发布 `unionid-derive`。

方向取舍：schema → Rust 与 Rust → schema 并存，但**每个项目只选一个单一来源**，另一端用生成器保持同步；不做双向自动合并。

### 7. serde 表示保真（#263 首个切片）

`UnionidSchema` 生成的 schema 必须与 `Value::from_serde` 和 `typed_rows` 实际看到的名称、形状一致。derive 读取会改变数据名称或形状的 serde 属性，并采用以下边界：

| serde 属性 | 当前规则 |
| --- | --- |
| field/variant `rename` | 支持；serialize/deserialize 分别指定时必须相同 |
| struct/enum `rename_all` | 识别 serde 的八种规则，但转换结果必须是合法 unionid identifier，variant 还必须以大写字母开头；不满足时在编译期拒绝 |
| enum `rename_all_fields`、struct variant `rename_all` | 支持；variant 规则优先于 enum 规则 |
| container `rename`、field `alias/default` 等不改变写出形状的属性 | 不改变 unionid type identity 或 schema 字段；由 serde 自身处理 |
| `tag/content/untagged/transparent` | 拒绝，因为它们把默认 externally-tagged sum 或 record 改成另一种值形状 |
| `flatten/skip/skip_serializing_if/serialize_with/deserialize_with/with` 等 | 拒绝，因为 schema 无法从字段类型推导实际写出形状或字段是否存在 |

`#[unionid(key = "...")]` 仍写 Rust 字段名；生成表声明时自动换成该字段的 serde 名称。应用不需要在同一 Rust 类型里重复 schema 字段名。

这一切片只保证已列出的 serde 表示一致性。数值值域、decimal 精度/scale 和命名 scalar/tuple 的契约见下一节。

### 8. 数值与名义类型保真（#263 第二个切片）

生成边界采用“Rust 类型必须覆盖数据库类型完整值域”的规则：

| 方向 | 契约 |
| --- | --- |
| Rust → schema derive | `i64` 覆盖 `int` 完整值域；`f64` 覆盖数据库只允许有限值的 `float` 完整值域，NaN/Infinity 在写入时拒绝。`i32/f32` 与其他窄数值在 derive 阶段拒绝。应用若只在局部使用窄数值，应在数据库边界显式转换并处理范围错误。 |
| schema → Rust codegen | `int→i64`、`float→f64`；不生成一个只能读取部分合法数据库值的窄类型。 |
| `decimal P S` | 生成/derive 使用 `unionid::scalars::Decimal`。wrapper 保存 coefficient 与 scale，schema 的 P/S 在 prepared bind/write、查询与 migration 边界检查；Rust 类型本身不在编译期携带 P/S。 |
| 命名 scalar/tuple | 生成 `#[serde(transparent)] pub struct Name(pub T)`，保留 `UserId`/`OrderId` 等 Rust 名义区别，同时维持底层 unionid value 表示。 |

命名 newtype 的公开字段允许应用显式构造和取出底层值。它们不暴露 unionid 的内部稳定 type ID；持久名义身份仍由 catalog 处理。已有按旧版本生成的 `pub type` 源码需要重新生成，属于生成产物升级，而不是存储格式变更。

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

- record -> `pub struct` with `pub name: T`; `option T` -> `Option<T>`, `list T` -> `Vec<T>`, tuples map to Rust tuples, and named scalars/tuples become serde-transparent tuple newtypes.
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

- Preserve stable IDs and migration-compatibility hints when generating from a catalog.
- Multi-language typed client generation (coordinate with [#192](https://github.com/worktools/unionid/issues/192)).
- Extend the reverse derive attributes: field defaults, custom table/index names, and uniqueness.

### 6. Reverse: Rust -> DDL (implemented)

`unionid-derive` provides `#[derive(UnionidSchema)]`, mapping a Rust struct/enum to a unionid `type` declaration and, with `#[unionid(table = "...", key = "...")]`, a `table` declaration. The generated code implements `unionid::UnionidSchema`, and `unionid::SchemaBuilder` collects declarations into an executable schema script. `SchemaBuilder::build()` emits types in dependency order (referenced types first, independent of registration order) and returns `E_SCHEMA` for a missing dependency or an unsupported cycle (`build` returns `Result`).

- Type mapping: `i64->int`, `f64->float`, `bool->bool`, `String/&'static str->text`, `Uuid/Date/Timestamp/Duration/Bytes->uuid/date/timestamp/duration/bytes`, `Decimal` requires `#[unionid(decimal = "P S")]`; `Option<T>->option (T)`, `Vec<T>->list (T)`, `Box<T>->T`, tuples and nested named types by name. The derive rejects `i32/f32` and other numeric types that cannot represent the complete database domain.
- Record structs become `type Name = { field type, ... }`; enum unit / single-field / multi-field / record variants become `Variant`, `Variant ty`, `Variant (a, b)`, and `Variant { field type }`.
- Generic types and unions are rejected; anonymous tuple structs are not records and error.
- The crate stays decoupled from core `unionid` (no dependency from `unionid` to `unionid-derive`); applications depend on both, keeping `unionid`'s crates.io release contract unchanged. The release workflow verifies and publishes `unionid-derive`.

Direction tradeoff: schema -> Rust and Rust -> schema coexist, but each project picks exactly one single source and keeps the other side generated; no bidirectional auto-merge.

### 7. serde representation fidelity (first #263 slice)

Schemas emitted by `UnionidSchema` must match the names and shapes observed by `Value::from_serde` and `typed_rows`. The derive inspects serde attributes that change data names or shapes:

| serde attribute | Current rule |
| --- | --- |
| field/variant `rename` | Supported; separate serialize/deserialize names must be equal |
| struct/enum `rename_all` | All eight serde rules are recognized, but the result must be a valid unionid identifier and variants must start uppercase; violations are rejected at compile time |
| enum `rename_all_fields`, struct-variant `rename_all` | Supported; the variant rule overrides the enum rule |
| container `rename`, field `alias/default`, and other attributes that do not change the serialized shape | Do not change unionid type identity or schema fields and remain handled by serde |
| `tag/content/untagged/transparent` | Rejected because they replace the default externally-tagged sum or record representation |
| `flatten/skip/skip_serializing_if/serialize_with/deserialize_with/with`, etc. | Rejected because the field type no longer determines the emitted shape or presence |

`#[unionid(key = "...")]` continues to name the Rust field. The table declaration automatically uses that field's serde name, avoiding a duplicate schema name in the Rust type.

This slice only guarantees the listed serde representations. The numeric-domain, decimal, and named scalar/tuple contracts follow below.

### 8. Numeric and nominal fidelity (second #263 slice)

The generation boundary requires the Rust type to represent the database type's complete value domain:

| Direction | Contract |
| --- | --- |
| Rust -> schema derive | `i64` covers the complete `int` domain. `f64` covers the complete finite-only database `float` domain; writes reject NaN and infinity. The derive rejects `i32/f32` and other narrow numeric types. Applications using narrow local values convert explicitly at the database boundary and handle range errors. |
| schema -> Rust codegen | `int->i64` and `float->f64`; generated fields do not accept only a subset of legal database values. |
| `decimal P S` | Generation/derive uses `unionid::scalars::Decimal`. The wrapper retains coefficient and scale; prepared bind/write, query, and migration boundaries enforce schema P/S. The Rust type itself does not carry P/S at compile time. |
| Named scalar/tuple | Emit `#[serde(transparent)] pub struct Name(pub T)`, preserving Rust distinctions such as `UserId` versus `OrderId` while keeping the underlying unionid value representation. |

The public newtype field allows explicit construction and extraction. It does not expose unionid's internal stable type ID; the catalog continues to own durable nominal identity. Source generated by older versions with `pub type` aliases must be regenerated. This changes generated source, not the storage format.
