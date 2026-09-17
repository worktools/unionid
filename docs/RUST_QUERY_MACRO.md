# Inline Rust queries / 内联 Rust 查询

## 中文说明

`unionid-query` 的 `queries!` 宏让 Rust crate 在 module scope 直接声明一组 Unionid 查询。宏读取显式的声明式 schema，在 Rust 编译阶段复用 Unionid parser、binder、static query contract 和 codegen，生成共享 ADT、每个查询的参数、结果 row 和执行函数。

```toml
[dependencies]
serde = { version = "1", features = ["derive"] }
unionid = "=0.9.0"
unionid-query = "=0.9.0"
```

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

`schema` 路径相对于调用 crate 的 `CARGO_MANIFEST_DIR`，必须是小于等于 1 MiB 的 UTF-8 普通文件。一个 invocation 至少包含一个 `query`；query 名是 Rust identifier。宏正文使用与 `.unid` 完全相同的语法和 `$parameter` typed binding，不接受字符串 query 或任意 Rust expression 拼接。

生成 API 与 `unionid query rust --dir` 一致：

```rust
let rows = find_pending::find_pending(
    &mut engine,
    find_pending::FindPendingParams {min_priority: 3},
)?;
```

宏会在编译期拒绝未知字段、错误 constructor、参数类型冲突、非穷尽 match 和不支持的 operation。schema 文件通过 `include_str!` 进入 Rust dependency graph，修改后会重新展开。生成函数在运行时再次核对 schema revision/hash，并通过 `Engine::prepare` 执行；连接到漂移后的 catalog 会在扫描或写入前返回 `E_SCHEMA_CHANGED`。

首版不在编译期间打开 redb。以 migration 建立 stable ID、需要按 live catalog 生成绑定的应用继续使用：

```bash
unionid query rust --db app.redb --dir queries --output generated/queries.rs
```

独立 `.unid` 文件也继续适合跨语言、CLI、LLM、独立 formatter 和大型查询。内联宏主要服务于短小、只归一个 Rust crate 所有的查询。rustfmt 不理解宏内 Unionid block；保持示例布局，实际 digest 仍根据 Unionid canonical formatter 计算。

完整契约与后续边界见 [RFC 0020](rfc/0020-inline-rust-query-macros.md)。

## English Description

The `queries!` macro from `unionid-query` declares a group of Unionid operations directly at Rust module scope. It reads one explicit declarative schema and reuses the Unionid parser, binder, static query contract, and code generator during Rust compilation, producing shared ADTs plus typed parameters, result rows, and execution functions for every query.

The schema path is relative to the caller's `CARGO_MANIFEST_DIR` and must name a regular UTF-8 file no larger than 1 MiB. An invocation contains at least one Rust-identifier query name. Bodies use exactly the `.unid` language and `$parameter` typed bindings; strings and arbitrary Rust-expression interpolation are not accepted.

The generated API matches `unionid query rust --dir`, as shown above. Invalid fields, constructors, parameter unification, match coverage, or unsupported operations fail Rust compilation. `include_str!` tracks schema changes. Generated calls still check schema revision/hash and prepare through the Engine, returning `E_SCHEMA_CHANGED` before a scan or mutation when the runtime catalog drifts.

The first version never opens redb during compilation. Applications that require migration-established live IDs continue to generate from `query rust --db`. Standalone `.unid` remains the better boundary for cross-language use, CLI and LLM tooling, independent formatting, and large queries. The inline macro targets short queries owned by one Rust crate. rustfmt does not format the inner Unionid block; query digests still use the canonical Unionid formatter.

See [RFC 0020](rfc/0020-inline-rust-query-macros.md) for the complete contract and follow-up boundaries.
