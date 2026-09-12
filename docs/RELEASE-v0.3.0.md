# unionid v0.3.0 发布说明

v0.3.0 集中改善 Rust 应用从模型定义到长期演进的完整使用路径。应用现在可以从 Rust 类型或 unionid schema 维护一份权威模型，通过官方 typed client 使用本地 Engine、同步／异步 TCP 与 HTTP，并用有界关联读取和可预演 migration 处理真实应用状态。

## 主要变化

- `unionid-derive` 的 `UnionidSchema` 与 `SchemaBuilder` 按依赖顺序产生可执行 schema，支持 struct、enum、option、tuple、list、命名 scalar/tuple newtype、默认值以及单字段普通／唯一索引。serde rename 与 tag 表示会被准确映射或明确拒绝，窄整数等不能无损往返的表示在生成阶段报错。
- schema 也能生成可编译的 Rust 模型；稳定类型、字段和 variant 身份继续由数据库 schema 管理，生成代码不会把 nominal domain ID 降为裸 scalar。
- 官方 `TcpClient`、`AsyncTcpClient` 与 `HttpClient` 共用 versioned typed request/response 边界，覆盖参数、typed rows、分页、deadline、NDJSON stream/cancel，以及带持久幂等 key 的丢响应安全重试。
- `Engine::fetch_by_key` / `typed_fetch_by_key` 提供按主键或索引键的一致批量取回。查询语言的 `lookup ... take N` 提供有界的一对多嵌套结果；缺失的一对一引用为 `None`，缺失的一对多关联为 `[]`。
- `migration diff`、`plan` 与 `rehearse` 在生产副本上报告 schema 动作、受影响行、索引重建和有界工作估算。`migration advance --max-steps N --step-delay-ms M` 在已提交 checkpoint 间暂停，便于控制持续维护压力。
- 打包后的独立 Rust consumer 通过 Engine、同步 TCP、异步 TCP 和 HTTP 验证同一嵌套 ADT、UUID、timestamp、分页、取消、结构化错误、丢响应重试、重启、migration、check 与 backup/restore。

## 升级与兼容

v0.3.0 没有增加内部格式或 wire protocol 版本。它继续读取 storage format 1–6 和 logical backup 1–4，新库写入 storage format 6 / backup format 4，支持数据协议 1/2 与 stream protocol 1。现有 v0.2.0 format-6 数据库可由 v0.3.0 直接打开；仍建议先保留旧二进制与可校验 backup，并在数据库副本上运行 `doctor`、`migration rehearse`、`check` 和应用读写验收。

应用 schema migration 仍会在 format 6 shadow generation 中构建并校验，再原子切换。migration 期间读取旧 generation，普通写入被拒绝。`--step-delay-ms` 只在相邻提交之间暂停，会延长写阻塞窗口；它不是零停机或固定吞吐承诺。

## 安装与发布边界

需要 Rust 1.94 或更高版本：

```bash
cargo install unionid --version 0.3.0 --locked
```

Rust 模型派生同时依赖 `unionid = "0.3.0"` 与 `unionid-derive = "0.3.0"`。正式 workflow 先发布 `unionid-derive`，再发布 `unionid`，随后创建带 Linux/macOS 原生包与 SHA-256 的 GitHub Release。

推荐舒适工作集仍约为 10,000 行，100,000 行只作为已测试上限。服务面向可信单机应用；内置认证、TLS、通用扁平 join、window、多个写者与分布式执行不属于本版本。完整升级步骤和资源边界见 [UPGRADING.md](UPGRADING.md) 与 [M7 验收记录](benchmarks/m7-acceptance-2026-09-10.md)。

## English Description

v0.3.0 focuses on the complete Rust application path from model definition through long-term evolution. An application can keep either Rust types or unionid schema as its authoritative model, use official typed clients across the local Engine, synchronous or asynchronous TCP, and HTTP, and handle practical application state with bounded relational reads and rehearsable migrations.

`unionid-derive` now emits executable schemas in dependency order and covers structs, enums, options, tuples, lists, named scalar/tuple newtypes, defaults, and single-field ordinary or unique indexes. Serde renames and enum representations are either mapped precisely or rejected explicitly; narrow numeric representations that cannot round-trip losslessly fail during generation. Schema-to-Rust generation preserves nominal domain identities as well.

The official `TcpClient`, `AsyncTcpClient`, and `HttpClient` share the versioned typed request/response boundary. They cover parameters, typed rows, pagination, deadlines, NDJSON streaming and cancellation, and safe lost-response retry guarded by durable idempotency keys. `Engine::fetch_by_key` and `typed_fetch_by_key` provide consistent batched fetches by primary or indexed key. The query-language `lookup ... take N` stage produces bounded nested one-to-many results.

Migration diff, planning, and rehearsal report schema actions, affected rows, index rebuilds, and bounded work estimates on a database copy. `migration advance --max-steps N --step-delay-ms M` pauses between committed checkpoints to reduce sustained maintenance pressure. A packaged independent Rust consumer validates the same nested ADTs, production scalars, paging, cancellation, structured errors, lost-response retry, restart, migration, integrity check, and backup/restore across Engine, TCP, and HTTP.

This release does not change internal format or wire protocol versions. It continues to read storage formats 1–6 and logical backups 1–4, writes storage format 6 and backup format 4, and supports data protocols 1/2 plus stream protocol 1. Existing v0.2.0 format-6 databases can be opened directly, but retain the old binary and a verified backup, then run doctor, migration rehearsal, check, and application acceptance against a copy first.

Schema migration still builds and validates a format-6 shadow generation before atomic cutover. Reads continue against the old generation while ordinary writes are blocked. Step delay pauses only between commits and extends the write-blocking interval; it is not a zero-downtime or fixed-throughput guarantee.

Rust 1.94 or newer is required. Applications using schema derives should depend on both `unionid = "0.3.0"` and `unionid-derive = "0.3.0"`. The release workflow publishes `unionid-derive` first, then `unionid`, and finally creates the GitHub Release with Linux/macOS native archives and SHA-256 files.

The recommended comfortable working set remains around 10,000 rows, with 100,000 retained as a tested ceiling. The service targets trusted single-machine applications. Built-in authentication, TLS, general flattened joins, windows, multiple writers, and distributed execution remain outside this release. See [UPGRADING.md](UPGRADING.md) and the [M7 acceptance record](benchmarks/m7-acceptance-2026-09-10.md) for detailed steps and resource evidence.
