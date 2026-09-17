# RFC 0020：内联 Rust 查询宏 / inline Rust query macros

- 状态 / Status: accepted for v0.9 implementation
- 日期 / Date: 2026-09-17
- 跟踪 / Tracking: [#352](https://github.com/worktools/unionid/issues/352), implementation [#353](https://github.com/worktools/unionid/issues/353), hardening [#354](https://github.com/worktools/unionid/issues/354)
- 依赖 / Dependencies: [RFC 0012](0012-schema-rust-bindings.md), [RFC 0015](0015-static-query-contract.md), [RFC 0017](0017-rust-shaped-prql-language.md)

## 中文说明

### 1. 目标

Rust 应用已经可以通过 `unionid query rust` 从 `.unid` 文件生成类型化绑定，但短小、只由一个 Rust crate 使用的查询仍需要额外文件和显式生成步骤。查询语言已经采用 Rust 形状的 enum constructor、record、match、range、bool operator 和 arrow closure，因此可以安全地放入 function-like procedural macro 的 token block。

v0.9 增加以**内联形式为主**的 `unionid-query` crate。一个宏调用声明同一 schema 下的一组查询：

```rust
unionid_query::queries! {
    schema "schema.unid"

    query find_pending {
        from tasks
        filter state == Pending and priority >= $min_priority
        sort {-priority, id}
        select {id, title, state}
        take 20
    }

    query reprioritize_task {
        update tasks
        filter id == $id
        set priority = $priority
        returning {id, priority, state}
    }
}
```

宏在 module scope 展开为现有 static-query generator 的共享 schema ADT、每个查询的 `Params`、typed result row 和执行函数。调用方式与 `query rust --dir` 生成物一致：

```rust
let rows = find_pending::find_pending(
    &engine,
    find_pending::FindPendingParams {min_priority: 3},
)?;
```

独立 `.unid` 查询继续直接使用 `query describe`、`query rust` 和 `project check`。文件形式已经有完整工具链，不另加 `query_file!` 包装。

### 2. 编译期与运行时契约

宏必须复用 `query_contract::describe` 和 `codegen::rust_query_bundle`，不得维护第二套 parser、binder、类型映射或 cardinality 推断。编译阶段按调用 crate 的 `CARGO_MANIFEST_DIR` 解析显式 schema 相对路径，读取有界 UTF-8 声明式 schema，并对所有内联查询完成：

1. Unionid parse 与 canonical format；
2. schema-aware field、constructor、match coverage 和参数类型绑定；
3. 参数与结果 `TypeShape` 推断；
4. query digest、schema revision/hash 固定；
5. Rust model、parameter、row 与调用函数生成。

任一 schema 或查询错误使 Rust 编译失败，不产生部分模块。展开结果通过 `include_str!` 跟踪 schema 文件，修改 schema 必须触发重新展开。查询正文来自宏 token span；实现不得仅依赖 `TokenStream::to_string()` 猜测 `$parameter`、`::`、`@` temporal literal 或负数字面量的原始拼写。无法取得可信 source text 时应给出编译错误，而不是静默改变查询。

运行时仍使用生成函数中的 schema identity 检查和 `Engine::prepare`。宏编译成功不授权 mutation，也不绕过 idempotency、read-only、deadline、protocol 或服务鉴权。

### 3. 语法取舍

宏 envelope 使用 Rust 常见的 `keyword name { ... }` 层级和必要花括号，不引入分号：

- `schema "path"` 只出现一次，位于查询之前；
- `query <rust_identifier> { <unionid source> }` 至少出现一次；
- query 名必须是普通 Rust identifier，并继续通过现有 codegen 冲突检查；
- 查询正文保持与 `.unid` 相同的 pipeline、match、constructor 和 `$parameter` 语义；
- 花括号明确宏项边界，也保护查询内部多行结构；
- v0.9 不接受字符串形式的 query body，不允许把任意 Rust expression 拼进查询 AST。

为了宏而调整核心语言时，只接受同时改善 `.unid` 一致性、减少歧义或使 token 边界更明确的变化。不得产生“宏专用查询方言”。首版保留 `$parameter`；它是显式 typed binding，不是 Rust token interpolation。若以后改为更 Rust-shaped 的参数引用，必须通过独立 breaking-language RFC 同时迁移 CLI、文件、formatter、LLM 文档和 macro。

### 4. crate 边界

首版使用独立 `unionid-query` proc-macro crate，并在编译 host 上依赖相同版本的 `unionid`，从而直接调用公开 contract/codegen API。核心 `unionid` 不依赖或 re-export macro，避免依赖环；应用显式添加两个同版本依赖：

```toml
[dependencies]
unionid = "=0.9.0"
unionid-query = "=0.9.0"
```

宏依赖完整 Unionid compiler 会增加首次编译成本。v0.9 记录 clean/incremental build 时间；只有证据表明成本不可接受时，才把 schema/query compiler 抽成第三个无运行时依赖的 crate。首版不复制内部模块来换取较小依赖。

### 5. 明确边界

- v0.9 只从声明式 schema 文件编译，不在 Rust 编译期间打开 `.redb`。
- 经过长期 migration、必须保留 live catalog stable ID 的应用继续使用 `query rust --db`；后续可增加显式、可提交的 portable catalog artifact，但不能让 macro 隐式依赖本地数据库。
- macro expansion 只在 module scope 产生 items，不做立即执行的 expression macro。
- rustfmt 不理解 Unionid query block；宏按规范示例保持可读布局，digest 使用 Unionid canonical formatter。独立文件仍由 `unionid fmt` 格式化。
- 首版支持 `PreparedQuery` 已支持的 read、explain 和 mutation 范围，不增加服务器命名查询或动态 Rust AST 拼接。

### 6. 验收

v0.9 最小闭环要求：

1. 一个 consumer crate 用内联宏定义至少一个 ADT match query 和一个 mutation；
2. 编译期拒绝未知字段、错误 constructor、无法统一的参数和非穷尽 match；
3. 生成参数和结果可正常执行、serde 往返，并保持 cardinality；
4. 修改 schema 或 query 会改变生成 digest并触发重编译；
5. runtime schema drift 在扫描或 mutation 前返回 `E_SCHEMA_CHANGED`；
6. 同一源码经宏和 `.unid` contract 得到相同 canonical source、参数、结果与 digest；
7. 文档说明编译成本、migration/live-catalog 限制和不用 macro 的文件工作流。

## English Description

### 1. Goal

Rust applications can already generate typed bindings from `.unid` files with `unionid query rust`, but short queries owned by one Rust crate still require an extra file and an explicit generation step. The language now uses Rust-shaped enum constructors, records, matches, ranges, boolean operators, and arrow closures, so it can live safely inside a function-like procedural macro token block.

v0.9 adds an inline-first `unionid-query` crate. One invocation declares a group of queries bound to one schema, as shown above. At module scope it expands to the same shared schema ADTs, per-query `Params`, typed result rows, and execution functions produced by `query rust --dir`. Standalone `.unid` queries continue to use `query describe`, `query rust`, and `project check`; their complete toolchain makes a `query_file!` wrapper unnecessary.

### 2. Compile-time and runtime contract

The macro must reuse `query_contract::describe` and `codegen::rust_query_bundle`; it must not own another parser, binder, type mapping, or cardinality implementation. Relative to the caller's `CARGO_MANIFEST_DIR`, it reads one explicit bounded UTF-8 declarative schema and performs canonical formatting, schema-aware binding, parameter/result shape inference, digest and schema-identity capture, and Rust binding generation for every inline query.

Any error fails Rust compilation without partial modules. Expansion uses `include_str!` to track the schema file. Query source comes from the macro token span; the implementation must not guess parameter, path, temporal-literal, or negative-literal spelling solely from `TokenStream::to_string()`. Missing trustworthy source text is a compile error.

Generated functions retain the runtime schema-identity check and `Engine::prepare`. Compile-time success grants no mutation authority and bypasses no idempotency, read-only, deadline, protocol, or service boundary.

### 3. Syntax

The envelope uses Rust's familiar `keyword name { ... }` hierarchy and necessary braces without semicolons. It contains one `schema "path"` followed by one or more `query <rust_identifier> { <unionid source> }` entries. Query bodies retain the exact `.unid` pipeline, match, constructor, and `$parameter` semantics. v0.9 accepts neither string query bodies nor arbitrary Rust expressions inside the query AST.

Core-language changes made for macros must also improve `.unid` consistency, ambiguity, or token boundaries; there will be no macro-only dialect. `$parameter` remains an explicit typed binding in v0.9. A future Rust-shaped parameter-reference change requires a separate breaking-language RFC covering CLI, files, formatter, LLM documentation, and macros together.

### 4. Crate boundary

The first release uses a separate `unionid-query` proc-macro crate that depends on the same `unionid` version on the compile host and directly calls its public contract/codegen API. Core `unionid` neither depends on nor re-exports the macro, avoiding a dependency cycle. Applications explicitly depend on matching versions of both crates.

Compiling the full Unionid compiler on the host adds clean-build cost. v0.9 records clean and incremental build time. A third compiler-only crate is justified only if measurements show that cost is unacceptable; the first implementation will not duplicate internal modules to reduce dependencies.

### 5. Boundaries and acceptance

v0.9 compiles from a declarative schema file and never opens redb during Rust compilation. Applications that require migration-established live catalog IDs keep using `query rust --db`; a future committed portable-catalog artifact may support macros without introducing an implicit database dependency. Expansion produces module-scope items, rustfmt does not format the inner Unionid block, and the supported operation set remains the existing `PreparedQuery` surface.

Acceptance requires an inline ADT-match query and mutation in a consumer crate, compile-fail coverage for binding errors, executable typed parameters/results and cardinality, rebuild/digest drift on source changes, pre-scan `E_SCHEMA_CHANGED`, parity with the `.unid` contract, and documentation of compile cost and live-catalog limits.
