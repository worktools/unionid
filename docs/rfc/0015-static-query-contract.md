# RFC 0015：静态查询描述契约 / static query description contract

- 状态 / Status: accepted, description slice implemented
- 日期 / Date: 2026-09-13
- 跟踪 / Tracking: [#264](https://github.com/worktools/unionid/issues/264)
- 依赖 / Dependency: [RFC 0014](0014-portable-adt-contract.md)

## 中文说明

### 1. 问题与目标

schema codegen 只能生成存储模型。`select`、`derive`、`aggregate`、`lookup` 和 mutation `returning` 都能产生不同于表 row 的类型；应用若手工同步参数 map 和结果 DTO，会把查询错误推迟到运行时。

version 1 query description 使用现有 `syntax` parser、`Database::prepare_pipeline`、expression/match binder 和参数统一过程。它不重新解释查询，也不读取数据。输入是一个声明式 schema 和一个静态查询文件；一个文件必须恰好包含一个 `PreparedQuery` 支持的 operation，使文件名可以稳定映射到一个生成函数。

### 2. 描述结构

`QueryDescription` 包含：

- description version 1；
- schema revision/hash，与 RFC 0014 的 catalog 描述配对；
- canonical source 及其 `sha256:` digest；
- `read`、`explain`、`insert`、`insert_many`、`upsert`、`upsert_many`、`update` 或 `delete` operation；
- 按参数名稳定排序的参数及 RFC 0014 `TypeShape`；
- 有序结果字段、字段 `TypeShape`、row cardinality 和 affected-row metadata 标记。

参数和结果直接来自 binder 保存的 `ScalarType`，不会先格式化成字符串再解析。命名 ADT 使用 catalog-local稳定 ID 的 `ref`，因此生成器必须同时持有同一 schema identity 的 RFC 0014 描述，不能把 query JSON 单独移到另一个数据库 lineage。

Rust 中原有 `PreparedQuery::parameter_types` 继续提供便于观察的文本类型，并新增 `result_columns`；portable contract 另行保留内部 `ScalarType` 以生成无损 `TypeShape`，不把内部 IR 暴露为公共 API。

### 3. Cardinality

cardinality 是成功执行时的保守保证：

| 值 | 含义 |
| --- | --- |
| `none` | 不返回 row；例如无 `returning` 的 mutation 或 explain |
| `exactly_one` | 必有一行；例如未分组 aggregate、单行 insert/upsert returning |
| `at_most_one` | 零或一行；例如 `take 1`，或 aggregate 后继续 filter |
| `many` | 零到多行；包括普通查询、分组 aggregate 和批量 returning |

stage 顺序会改变保证：未分组 aggregate 即使输入为空也产生一行；其后的 filter 可把它降为 `at_most_one`；aggregate 前的 filter 不改变这一保证。`take` / `page` 只收紧上界。描述不根据当前表行数或索引唯一性猜测业务 cardinality。

### 4. Digest、诊断与运行时边界

digest 对 formatter 的 canonical source 计算，因此等价空格和换行不产生漂移，字段、projection、表达式或 stage 变化会产生新 digest。离线绑定检查字段路径、constructor/payload、参数类型统一、aggregate 输入、returning 和 match coverage；错误保留查询文件中的 statement span。

生成代码仍须在目标 `Engine` 上调用 `prepare`。`PreparedQuery` 绑定精确 schema revision/hash，执行前不匹配就返回 `E_SCHEMA_CHANGED`；编译成功不代表可以连接任意同名 schema。查询描述不授权 mutation、不修改 schema，也不替代 protocol、幂等 key 或服务鉴权。

### 5. 后续切片

下一切片从 schema + query 文件生成 Rust 参数类型、结果类型和调用函数，按 cardinality 解码，并在独立 consumer crate 编译执行。之后加入生成漂移检查和两版 schema/query/client 演进场景。服务器命名查询、用户泛型和第二执行器不作为前置。

## English Description

### 1. Problem and goal

Schema code generation only covers stored models. `select`, `derive`, `aggregate`, `lookup`, and mutation `returning` can all produce different shapes, while hand-maintained parameter maps and result DTOs defer drift to runtime.

The version-1 query description uses the existing syntax parser, pipeline preparation, expression/match binders, and parameter unification. It neither reinterprets the language nor reads data. Inputs are one declaration-only schema and one static query file. Each file contains exactly one operation supported by `PreparedQuery`, allowing its file name to map to one generated function.

### 2. Contract

`QueryDescription` carries description version 1, schema revision/hash, canonical source and SHA-256 digest, operation kind, name-sorted parameters, ordered result fields, conservative row cardinality, and an affected-row metadata flag. Parameter and result shapes reuse RFC 0014 `TypeShape` directly from bound `ScalarType`; no type string is reparsed. Named ADT refs use database-lineage-local catalog IDs, so a generator pairs the query contract with the RFC 0014 schema description of the same identity.

The existing Rust `PreparedQuery::parameter_types` remains a human-readable view and `result_columns` now exposes the ordered textual result schema. The portable path separately retains private `ScalarType` values long enough to emit lossless `TypeShape` without making the internal IR public API.

Cardinality describes successful execution: `none`, `exactly_one`, `at_most_one`, or `many`. An ungrouped aggregate returns exactly one row even for empty input; a later filter lowers that to at most one. `take` and `page` only tighten upper bounds. Current row counts and inferred business uniqueness never strengthen the contract.

### 3. Drift, diagnostics, and runtime checks

The digest covers formatter-canonical source, so equivalent layout is stable while field, projection, expression, and stage changes drift. Offline binding validates field paths, constructors and payloads, parameter unification, aggregate inputs, returning projections, and match coverage, retaining the query statement span on errors.

Generated code must still call `prepare` on the target `Engine`. `PreparedQuery` binds the exact schema revision/hash and returns `E_SCHEMA_CHANGED` before execution when they differ. Successful compilation is not permission to connect to an arbitrary same-named schema. The description neither authorizes mutations nor replaces protocol, idempotency, or service authorization.

The next slice generates Rust parameter/result types and call functions from schema plus query files, decodes according to cardinality, and compiles/runs them in an independent consumer crate. Generated-drift CI and two-version schema/query/client evolution follow. Server-side named queries, user generics, and a second executor are not prerequisites.
