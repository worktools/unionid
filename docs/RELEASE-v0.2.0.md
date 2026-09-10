# unionid v0.2.0 发布说明

v0.2.0 把 v0.1 之后完成的生产边界、增量执行和有界持久化工作收口为一个可升级、可校验的版本。它保持无分号的 PRQL 风格语言和原生代数数据类型模型，并补齐安全重试、稳定分页、生产标量、并发读取、可取消流、有序复合索引，以及 format-6 可恢复 migration。

## 主要变化

- UUID、date、timestamp、duration、decimal 和 bytes 贯通源码、Rust/serde、protocol v2、redb、索引、cursor、backup 和 migration；既有 protocol v1 继续服务原有标量和 ADT。
- mutation 可携带持久幂等键，使数据效果与完整成功回执在同一 redb transaction 提交。稳定 keyset cursor 绑定 schema、query、sequence 与唯一排序，并由 HMAC 验证。
- `ConcurrentEngine` 提供最多 8 个一致并发读快照；TCP 与 HTTP adapter 共用可取消、有背压的、有资源上限的 NDJSON stream protocol v1。
- 普通 DML 只构造目标 row/index 写集；typed ordered composite index 支持 equality prefix、range、正反序 order 和 page seek。
- format 6 使用 generation envelope。schema/data migration 分批构建并验证 shadow generation，再原子 cutover；中断后可继续或显式 abort，旧 generation 分批回收。
- redb 普通 open、按索引读取、融合 full pipeline、完整检查和 logical backup 使用有界 row source。`version`、`doctor`、`.storage` 和 operation profile 暴露不含业务值的诊断。
- crate、CLI、原生包和 `RELEASE.json` 使用同一个 0.2.0 版本。包内 `release/contract.json` 独立冻结 version-report schema、可读/当前 storage 与 backup 格式、codec、protocol 和 stream 版本。

## 从 v0.1.0 升级

保留原始数据库与旧二进制产生的 logical backup，并先在数据库副本上演练。v0.2.0 第一次可写打开 format 1/2 时会补齐 cursor identity 并归一到 format 3；后续内部格式转换必须逐步显式执行：

```bash
cp app.redb app-v0.2-rehearsal.redb
unionid doctor --db app-v0.2-rehearsal.redb --format json
unionid upgrade --db app-v0.2-rehearsal.redb --target 4
unionid upgrade --db app-v0.2-rehearsal.redb --target 5
unionid upgrade --db app-v0.2-rehearsal.redb --target 6
unionid check --db app-v0.2-rehearsal.redb
```

公开 v0.1.0 二进制生成的真实 format-1 数据库已持续测试这条路径，包括 schema identity、migration ledger、嵌套 ADT、索引、mutation、protocol v1/v2 和 backup/restore。logical backup 1 也可直接恢复成新的 format-6 数据库。没有原地 downgrade；需要回到旧二进制时，从升级前备份恢复到新路径。完整步骤见 [UPGRADING.md](UPGRADING.md)。

## 容量与服务边界

10k 行仍是推荐的舒适工作集，100k 行是已测试上限，不是日常目标。在 Apple M1 Pro、Rust 1.94.0 的 M7 最终复验中，100k open p95 为 12.02 ms，主键查询 p95 为 24 µs，单行条件更新 p95 为 9.90 ms，100-row 原子 batch p95 为 641.98 ms；完整 check 为 1.54 s／96.33 MiB，完整 shadow migration p95 为 16.51 s／427.98 MiB。数字只描述该机器与工作负载，不是 SLA；部署前应使用真实 value 宽度、索引和 migration 复测。原始样本见 [M7 验收记录](benchmarks/m7-acceptance-2026-09-10.md)。

服务仍面向受信单机应用，没有内置认证或 TLS。当前不提供 join、window、分布式、多写者或跨请求长事务；完整资源上限见 [SERVICE.md](SERVICE.md)。

## 获取与验证

tag workflow 使用 Rust 1.94.0 在 macOS 与 Linux 构建 locked 原生产物，并在空目录校验外部 SHA-256、版本契约、本地 CLI、TCP 和包内教程。crate 只由 GitHub Actions 发布到 crates.io。每个包中的 `RELEASE.json`、`release/contract.json` 和本说明必须与二进制报告一致。

## English Description

v0.2.0 closes the production-boundary, incremental-execution, and bounded-persistence work completed since v0.1 into one upgradeable and verifiable release. It retains the semicolon-free PRQL-style language and native algebraic data model while adding safe retries, stable pagination, production scalars, concurrent reads, cancellable streams, ordered composite indexes, and recoverable format-6 migrations.

UUID, date, timestamp, duration, decimal, and bytes now cross source, Rust/serde, protocol v2, redb, indexes, cursors, backups, and migrations. Protocol v1 remains supported for the original scalar and ADT vocabulary. Durable idempotency receipts commit with mutation effects, while HMAC-authenticated keyset cursors bind schema, query, sequence, and a unique order. `ConcurrentEngine` serves up to eight consistent read snapshots, and TCP/HTTP adapters share a bounded, backpressured, cancellable NDJSON stream protocol v1.

Ordinary DML builds only its target row/index write set. Typed ordered composite indexes serve equality-prefix, range, forward/reverse order, and page seeks. Storage format 6 adds generation envelopes; schema/data migrations build and validate a shadow generation in bounded batches before atomic cutover, support resume or explicit abort after interruption, and reclaim old generations incrementally. Ordinary open, indexed reads, fused full pipelines, checks, and logical backups use bounded row sources.

For v0.1.0 upgrades, retain the original database and a logical backup, rehearse on a copy, allow the compatible format-1/2 cursor-identity normalization to format 3, then explicitly run upgrades to formats 4, 5, and 6 followed by `check`. A durable fixture created by the public v0.1.0 binary continuously verifies schema identity, ledger, nested ADTs, indexes, mutations, protocol boundaries, and backup/restore. Logical backup 1 can also restore directly into a fresh format-6 database. There is no in-place downgrade; restoring a pre-upgrade backup to a new path is the rollback route.

The recommended comfortable working set remains around 10k rows, with 100k retained as a tested ceiling. On the final M7 Apple M1 Pro / Rust 1.94.0 run, 100k open p95 was 12.02 ms, primary-key query p95 was 24 µs, conditional-update p95 was 9.90 ms, a 100-row atomic batch p95 was 641.98 ms, full check took 1.54 seconds and 96.33 MiB, and shadow-migration p95 was 16.51 seconds with 427.98 MiB peak RSS. These measurements are evidence for that workload and machine, not an SLA.

The service remains intended for trusted single-machine applications and has no built-in authentication or TLS. Joins, windows, distributed execution, multiple writers, and cross-request long transactions remain outside this release.

The tag workflow builds locked native macOS and Linux artifacts with Rust 1.94.0, then verifies an externally supplied SHA-256, the independent release contract, local CLI, TCP, and the packaged tutorial from an empty directory. Only GitHub Actions publishes the crate. `Cargo.toml`, the binary version report, `RELEASE.json`, packaged `release/contract.json`, and these version-specific notes must agree before an artifact passes verification.
