# unionid v0.9.0 发布说明

v0.9.0 让 Rust 应用可以把短小、crate 私有的 Unionid 查询直接写在 module 中，同时保留声明式 schema、ADT 语义、编译期检查和运行时 schema identity 防护。独立 `.unid` 文件继续用于跨语言、CLI、LLM、大型查询和 live migrated catalog。

## 用户可见变化

- 新增独立的 `unionid-query` proc-macro crate，以及 `queries! { schema "..." query name { ... } }` module-scope 宏。
- 宏正文使用与 `.unid` 完全相同的 Rust/PRQL 风格查询语法，支持 `$parameter`、ADT constructor 简写、穷尽 match、查询、mutation 与 returning。
- 编译期复用同一个 parser、binder、static query contract 和 Rust codegen，生成共享 ADT、每个查询的 Params/Row/Output 和执行函数；未知字段、错误 constructor、参数冲突与非穷尽 match 会阻止 Rust 编译。
- schema 文件相对于调用 crate 的 manifest 解析，并通过 `include_str!` 进入依赖图；schema 或 query 修改都会重新展开。生成函数仍核对 schema revision/hash，漂移时在读取或 mutation 前返回 `E_SCHEMA_CHANGED`。
- `unionid`、`unionid-derive` 和 `unionid-query` 使用同一个精确版本。v0.9 发布 workflow 按 derive → core → query macro 顺序发布。

## 使用

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
        filter state == Pending && priority >= $minimum
        sort {-priority, id}
        select {id, title, state}
        take 20
    }
}
```

## 升级与边界

v0.9.0 不改变 storage format、component codec、logical backup 或网络协议。最低 Rust 仍为 1.94，redb 固定为 4.1.0；v0.8 format-6/7 数据库和 logical backup 可直接使用，无需 storage upgrade、schema migration 或查询源码重写。

宏只接受显式声明式 schema，不在编译期间打开 redb。依赖 migration 建立的 live stable ID 时，继续使用 `unionid query rust --db app.redb --dir queries`。`.unid` 文件仍适合跨语言、CLI/LLM、独立 formatter 和大型查询。rustfmt 不格式化宏中的 Unionid block；canonical digest 仍由 Unionid formatter 计算。

参考 macOS arm64 样本中，相对已经依赖 runtime 的 consumer，宏路径冷检查增加约 5.6 秒和 389 MB target 缓存；no-op 检查约 0.17 秒，query/schema 修改后的 build + run 小于 1 秒。数字只用于说明边界，不是性能承诺；Linux 候选会保存实际 JSON。完整方法与拆包决策见[编译记录](benchmarks/query-macro-2026-09-18.md)。

## English Description

v0.9.0 lets Rust applications keep short, crate-owned Unionid queries directly in a module while retaining a declarative schema, ADT semantics, compile-time checking, and runtime schema-identity protection. Standalone `.unid` remains the boundary for cross-language use, CLI and LLM workflows, large queries, and live migrated catalogs.

### User-visible changes

- A new standalone `unionid-query` proc-macro crate provides the module-scope `queries! { schema "..." query name { ... } }` macro.
- Macro bodies use exactly the same Rust/PRQL-shaped language as `.unid`, including typed parameters, contextual ADT constructors, exhaustive matching, reads, mutations, and returning.
- Expansion reuses the same parser, binder, static query contract, and Rust generator to emit shared ADTs and per-query Params/Row/Output and execution functions. Unknown fields, invalid constructors, parameter conflicts, and non-exhaustive matches fail Rust compilation.
- The schema path is relative to the consuming manifest and enters Cargo's dependency graph through `include_str!`; schema and query edits re-expand the macro. Generated calls still verify schema revision/hash and return `E_SCHEMA_CHANGED` before reads or mutations against a drifted catalog.
- `unionid`, `unionid-derive`, and `unionid-query` use one exact version. The v0.9 workflow publishes derive, core, then query macro.

### Upgrade and limits

v0.9.0 does not change storage formats, component codecs, logical backup, or network protocols. Rust 1.94 remains the minimum and redb remains pinned to 4.1.0. Existing v0.8 format-6/7 databases and logical backups work directly without a storage upgrade, schema migration, or query-source rewrite.

The macro accepts an explicit declarative schema and never opens redb during compilation. Continue using `unionid query rust --db app.redb --dir queries` when bindings depend on migration-established live IDs. Standalone `.unid` remains appropriate for cross-language, CLI/LLM, independent formatting, and large-query workflows. rustfmt does not format inner Unionid blocks; Unionid's formatter still defines the canonical digest.

In the reference macOS arm64 sample, the macro path added about 5.6 seconds of cold checking and 389 MB of target cache over a consumer that already depended on the runtime. A no-op check took about 0.17 seconds, and query/schema edit build-and-run iterations stayed below one second. These values explain the boundary and are not performance promises; the Linux candidate stores its own JSON evidence. See the [compile record](benchmarks/query-macro-2026-09-18.md) for the method and compiler-crate decision.
