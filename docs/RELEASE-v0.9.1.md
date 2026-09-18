# unionid v0.9.1 发布说明

v0.9.1 是 v0.9 inline Rust query macro 的兼容性补丁。它修复了调用模块已经导入 `serde::Serialize` 或 `serde::Deserialize` 时，`queries!` 展开代码再次导入同名 trait 并触发 Rust `E0252` 的问题。

宏生成的 schema ADT 现在直接使用 `serde::Serialize` 和 `serde::Deserialize` 限定 derive 路径，不再向调用模块注入 serde imports。已有查询、生成类型及运行时行为保持不变；应用可以继续在同一模块定义自己的 serde 类型。

```toml
[dependencies]
serde = { version = "1", features = ["derive"] }
unionid = "=0.9.1"
unionid-query = "=0.9.1"
```

`unionid`、`unionid-derive` 与 `unionid-query` 应统一使用 0.9.1。此版本不改变 storage format、component codec、logical backup、网络协议、schema identity 或 query language；v0.9.0 数据库和查询源码无需 migration。

## 验证

- 宏测试覆盖调用模块已有 serde derive imports 的场景。
- codegen 测试确认生成代码使用完全限定的 serde derive 路径且不生成 `use serde`。
- 发布 workflow 在 Ubuntu 与 macOS 上运行完整检查、兼容性验证、示例、打包和归档验收。

# unionid v0.9.1 release notes

v0.9.1 is a compatibility patch for the v0.9 inline Rust query macro. It fixes Rust `E0252` errors when the caller module already imports `serde::Serialize` or `serde::Deserialize` and `queries!` previously expanded another import with the same name.

Generated schema ADTs now use fully qualified `serde::Serialize` and `serde::Deserialize` derive paths instead of injecting serde imports into the caller module. Existing queries, generated types, and runtime behavior remain unchanged, and applications can define their own serde types in the same module.

Keep `unionid`, `unionid-derive`, and `unionid-query` on version 0.9.1 together. This release does not change storage formats, component codecs, logical backups, network protocols, schema identity, or the query language. v0.9.0 databases and query sources require no migration.

## Validation

- Macro tests cover caller modules that already import serde derives.
- Codegen tests verify fully qualified serde derive paths and the absence of generated `use serde` imports.
- The release workflow runs the full checks, compatibility journey, examples, packaging, and archive acceptance on Ubuntu and macOS.
