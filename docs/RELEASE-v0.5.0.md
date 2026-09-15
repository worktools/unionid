# unionid v0.5.0 发布说明

v0.5.0 把现有 ADT 数据库主链补成一个可部署、可诊断、可恢复的单机版本。应用仍以 PRQL 风格的无分号语言声明 sum/product/递归类型，并通过 Rust typed API、生成查询绑定、CLI、TCP 或 HTTP 使用同一套类型语义；本版本不增加新的查询或类型表达力。

## 用户可见变化

- `explain analyze` 使用真实查询执行器返回不含业务值的 examined/decoded/returned rows、索引访问、pipeline 耗时、批次与工作内存画像。`ConcurrentEngine::metrics_snapshot()` 提供有界 cardinality 的进程指标；可选 `metrics` 与 `tracing` feature 只提供渲染和事件接点，不自动开放网络 endpoint 或 collector。
- 增量备份使用显式启用的数据库内原子 journal 和外部 portable baseline/segment chain。CLI 与 Rust API 支持 init、export、list、verify、按 manifest sequence restore、checkpoint、preview/confirmed prune 和安全 disable。损坏、缺口、分叉、越界 sequence 与已有目标都 fail closed；logical backup 1–4 继续可恢复。
- 离线 compact 使用经认证的相邻 proof 在数据库未变化时快速返回 no-op。proof 缺失或无效只会退回完整 compact，不会被当成数据或备份。
- 官方 Envoy v1.39.1 参考部署让 Unionid 继续监听 loopback，由边界网关执行 TLS 1.3 mTLS、精确客户端 URI SAN、CRL、5 秒握手超时、连接/缓冲限制和不含业务值的审计。raw TCP 身份对应实例级权限；需要逐请求授权时应使用 HTTP adapter 并安全传递已验证身份。

## 从空目录试用

发布包内的五分钟教程覆盖命名 ADT、嵌套值、穷尽 match、更新、进程重开、`doctor` 和完整 `check`。仓库中的 typed application 验收进一步覆盖 Rust 生成绑定、精确标量、lookup/aggregate/returning、两版 migration 与 redb 重开。备份文档给出从 baseline 到指定 sequence 的恢复与 retention 流程；部署文档给出 loopback 到受控网络的 mTLS 路径。

```bash
cargo install unionid --version 0.5.0 --locked
unionid version --format json
```

## 兼容与边界

v0.5.0 要求 Rust 1.94，固定 redb 4.1.0。新数据库仍创建为 storage format 6，默认 current codec 为 catalog/value/index/migration/receipt/maintenance/journal `4/2/3/1/2/1/0`；二进制读取 storage format 1–7。只有显式 `backup incremental init` 会把目标数据库升级到 format 7 并启用 journal codec 1。format 7 没有原地降级，切回旧二进制前应从启用前的 logical backup 恢复到新路径。

logical backup 当前格式仍为 4、可读 1–4；JSON Lines protocol 仍为 1/2，stream protocol 仍为 1。v0.4.0 format-6 数据库可直接打开。静态查询生成物绑定软件版本和 schema identity，升级后应使用 v0.5.0 对现有 `.uid` 查询重新生成并重新编译；`.unid` 后缀迁移留给 v0.6。

本版本面向单机、单写者和约 10,000 行的舒适工作集。100,000 行只作为候选发布的已测试上限，不是吞吐或延迟承诺。通用扁平 join、window、多写者、复制和分布式执行不在范围内。

## English Description

v0.5.0 completes the existing ADT database journey with deployable, diagnosable, and recoverable single-node operation. Applications still declare sum, product, and recursive types in the semicolon-free PRQL-style language and use the same typed semantics through the Rust API, generated query bindings, CLI, TCP, or HTTP. This release adds no query or type expressiveness.

### User-visible changes

- `explain analyze` uses the real executor to report value-free examined/decoded/returned row counts, index access, pipeline timing, batches, and peak working memory. `ConcurrentEngine::metrics_snapshot()` exposes bounded-cardinality process metrics. The optional `metrics` and `tracing` features provide rendering and event hooks without opening an endpoint or collector.
- Incremental backup combines an explicitly enabled transactional journal with a portable external baseline/segment chain. CLI and Rust APIs cover init, export, list, verify, manifest-sequence restore, checkpoint, preview/confirmed prune, and safe disable. Corruption, gaps, forks, out-of-range sequences, and existing destinations fail closed. Logical backup formats 1–4 remain restorable.
- Offline compaction uses an authenticated adjacent proof for a fast no-op when the database is unchanged. A missing or invalid proof falls back to full compaction and is never treated as data or a backup.
- The maintained Envoy v1.39.1 reference keeps Unionid on loopback while the boundary enforces TLS 1.3 mTLS, an exact client URI SAN, a CRL, a five-second handshake timeout, connection/buffer limits, and value-free audit records. Raw TCP identity grants instance-level authority. Per-request authorization belongs in the HTTP adapter with safely propagated verified identity.

### Start from an empty directory

The packaged five-minute tutorial covers named ADTs, nested values, exhaustive matching, updates, process reopen, `doctor`, and a full `check`. The repository's typed-application acceptance also covers generated Rust bindings, exact scalars, lookup/aggregate/returning, two schema versions, migration, and redb reopen. The backup guide provides baseline-to-sequence recovery and retention operations; the deployment guide provides the loopback-to-controlled-network mTLS path.

```bash
cargo install unionid --version 0.5.0 --locked
unionid version --format json
```

### Compatibility and limits

v0.5.0 requires Rust 1.94 and pins redb 4.1.0. New databases remain storage format 6 with default current catalog/value/index/migration/receipt/maintenance/journal codecs `4/2/3/1/2/1/0`; the binary reads storage formats 1–7. Only explicit `backup incremental init` upgrades that database to format 7 and enables journal codec 1. Format 7 has no in-place downgrade, so returning to an older binary requires restoring the logical backup made before enablement into a new path.

The current logical backup format remains 4 with formats 1–4 readable. JSON Lines protocols remain 1/2 and the stream protocol remains 1. v0.4.0 format-6 databases open directly. Generated static-query artifacts bind the software version and schema identity; regenerate existing `.uid` queries with v0.5.0 and recompile the client after upgrading. The `.unid` suffix migration remains scheduled for v0.6.

This release targets one machine, one writer, and a comfortable working set around 10,000 rows. A 100,000-row workload is a release-candidate tested ceiling, not a throughput or latency promise. General flattened joins, windows, multiple writers, replication, and distributed execution remain outside the product boundary.
