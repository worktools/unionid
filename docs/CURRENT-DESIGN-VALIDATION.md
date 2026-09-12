# 当前设计应用验收 / Current-design application validation

## 中文说明

本验收只针对当前 format 6、protocol v2 和 Rust public API，从空目录创建数据库；不读取 v0.1 fixture，不运行旧客户端，也不建立新的向后兼容承诺。

`scripts/verify-current-consumer.py` 先运行 `cargo package`，把生成的 `.crate` 解包到临时目录，再创建一个独立 Cargo 项目。该项目通过公开 feature 依赖解包后的 unionid package，不能访问 workspace 私有模块或 dev dependency。它验证 UUID、timestamp、sum、record、option、list 的 typed prepared 写入与返回、持久幂等重放、稳定分页跨重启、当前格式 schema migration、完整检查和 logical backup/restore。

同一个独立项目还从公开 API 启动 TCP 与 HTTP adapter，并对照 Engine、同步 TCP、异步 TCP 和 HTTP 四个入口。它验证嵌套 ADT 与生产标量的 typed DML/query、分页、有限 NDJSON stream 及终止帧、取消、结构化错误、transport deadline，以及真实丢响应后的 keyed retry。`examples/todolist.rs` 继续覆盖独立服务进程的应用旅程。两条路径共同构成 [#242](https://github.com/worktools/unionid/issues/242) 的 SDK 验收，并保留 [#207](https://github.com/worktools/unionid/issues/207) 的当前设计证据。

```bash
python3 scripts/verify-current-consumer.py
cargo run --locked --example todolist -- /tmp/unionid-current-todolist
```

开发中的脏工作树可以显式使用 `--allow-dirty`；CI 和正式证据不使用该选项。所有运行数据位于独立临时目录，成功输出只包含版本、能力和通过阶段，不包含业务值。

## English Description

This acceptance targets current format 6, protocol v2, and the Rust public API only, starting from an empty directory. It does not read v0.1 fixtures, run legacy clients, or create new backward-compatibility commitments.

`scripts/verify-current-consumer.py` runs `cargo package`, extracts the resulting `.crate` into a temporary directory, and creates an independent Cargo project. That project enables public features on the extracted unionid package and cannot access workspace-private modules or development dependencies. It validates typed prepared writes and results for UUID, timestamp, sum, record, option, and list values; durable idempotent replay; stable pagination across restart; a current-format schema migration; integrity checking; and logical backup/restore.

The same independent project starts the public TCP and HTTP adapters and compares Engine, synchronous TCP, asynchronous TCP, and HTTP entry points. It covers typed DML and queries for nested ADTs and production scalars, pagination, bounded NDJSON streams and terminal frames, cancellation, structured errors, transport deadlines, and keyed retry after a real lost response. `examples/todolist.rs` continues to exercise a separate service-process application journey. Together, the paths provide SDK acceptance for [#242](https://github.com/worktools/unionid/issues/242) while retaining the current-design evidence for [#207](https://github.com/worktools/unionid/issues/207).

Run the commands above with a fresh todolist path. Dirty development trees must explicitly pass `--allow-dirty`; CI and formal evidence do not. Runtime data stays in isolated temporary directories, and successful output contains only versions, capabilities, and passed stages rather than application values.
