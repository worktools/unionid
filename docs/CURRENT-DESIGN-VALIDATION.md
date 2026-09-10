# 当前设计应用验收 / Current-design application validation

## 中文说明

本验收只针对当前 format 6、protocol v2 和 Rust public API，从空目录创建数据库；不读取 v0.1 fixture，不运行旧客户端，也不建立新的向后兼容承诺。

`scripts/verify-current-consumer.py` 先运行 `cargo package`，把生成的 `.crate` 解包到临时目录，再创建一个独立 Cargo 项目。该项目只依赖解包后的 unionid package 和 serde，不能访问 workspace 私有模块或 dev dependency。它验证 UUID、timestamp、sum、record、option、list 的 typed prepared 写入与返回、持久幂等重放、稳定分页跨重启、当前格式 schema migration、完整检查和 logical backup/restore。

HTTP/TCP 进程边界继续由 `examples/todolist.rs` 验收。它从空库启动 HTTP adapter，通过网络执行 typed DML/query、真实丢响应重试、分页、有限 NDJSON stream、TCP/HTTP row 对照、重启、migration、check 和 backup/restore。两条路径共同构成 [#207](https://github.com/worktools/unionid/issues/207) 的当前设计旅程。

```bash
python3 scripts/verify-current-consumer.py
cargo run --locked --example todolist -- /tmp/unionid-current-todolist
```

开发中的脏工作树可以显式使用 `--allow-dirty`；CI 和正式证据不使用该选项。所有运行数据位于独立临时目录，成功输出只包含版本、能力和通过阶段，不包含业务值。

## English Description

This acceptance targets current format 6, protocol v2, and the Rust public API only, starting from an empty directory. It does not read v0.1 fixtures, run legacy clients, or create new backward-compatibility commitments.

`scripts/verify-current-consumer.py` runs `cargo package`, extracts the resulting `.crate` into a temporary directory, and creates an independent Cargo project. That project depends only on the extracted unionid package and serde, so it cannot access workspace-private modules or development dependencies. It validates typed prepared writes and results for UUID, timestamp, sum, record, option, and list values; durable idempotent replay; stable pagination across restart; a current-format schema migration; integrity checking; and logical backup/restore.

The real HTTP/TCP process boundary remains covered by `examples/todolist.rs`. Starting from an empty database, it performs typed DML and queries, a real lost-response retry, pagination, bounded NDJSON streaming, TCP/HTTP row comparison, restart, migration, check, and backup/restore over the network. Together these two paths form the current-design journey for [#207](https://github.com/worktools/unionid/issues/207).

Run the commands above with a fresh todolist path. Dirty development trees must explicitly pass `--allow-dirty`; CI and formal evidence do not. Runtime data stays in isolated temporary directories, and successful output contains only versions, capabilities, and passed stages rather than application values.
