# M7 接口与容量验收记录 / interface and capacity acceptance record

## 中文说明

本记录为 [#186](https://github.com/worktools/unionid/issues/186) 保存 M7 最终复验。测量使用 Apple M1 Pro（16 GiB）、macOS、Rust 1.94.0 和 storage format 6；数字只描述这次构建与机器，不是 SLA。原始数据见 [10k workload](data/workload-m7-2026-09-10-10k.json)、[100k workload](data/workload-m7-2026-09-10-100k.json)、[10k recovery](data/recovery-m7-2026-09-10-10k.json) 与 [100k recovery](data/recovery-m7-2026-09-10-100k.json)。

`tools/workload-eval` 为每个 open 和 migration 样本创建独立数据库副本。查询、写入和 migration 分别在 release 子进程中运行；计时不包含复制、计划断言和完整性检查。查询保存 initial 与 20 次 timed `ExecutionObservation`，page seek 的 cursor 准备可能预热 cache，实际 hit/miss 会保留在 initial observation；migration 通过 `MigrationFile` 执行真正的 format-6 shadow-generation 路径，并保存 prepare/build/validate/cutover/reclaim、generation、row/index、logical-byte 和 reclaim 状态。`tools/recovery-eval` 单独测量 open、cold/warm indexed read 和完整 check。

复现命令：

```bash
cargo run --release --locked --manifest-path tools/workload-eval/Cargo.toml -- /tmp/unionid-m7-10k.redb 10000 20 1500
cargo run --release --locked --manifest-path tools/workload-eval/Cargo.toml -- /tmp/unionid-m7-100k.redb 100000 20 1500
cargo run --release --locked --manifest-path tools/recovery-eval/Cargo.toml -- /tmp/unionid-recovery-10k.redb 10000 1500
cargo run --release --locked --manifest-path tools/recovery-eval/Cargo.toml -- /tmp/unionid-recovery-100k.redb 100000 1500
```

### 结果

| 路径 | 10k | 100k |
| --- | ---: | ---: |
| open p50 / p95 | 6.59 / 7.16 ms | 10.04 / 12.02 ms |
| primary-key query p95 | 22 µs | 24 µs |
| full-scan + `take 1` p95 | 3.29 ms | 3.27 ms |
| conditional update p95 | 10.97 ms | 9.90 ms |
| 100-row atomic batch p95 | 81.92 ms | 641.98 ms |
| shadow migration p50 / p95 | 1.53 / 1.60 s | 15.53 / 16.51 s |
| shadow migration peak RSS | 49.77 MiB | 427.98 MiB |
| explicit full check | 163 ms / 17.06 MiB | 1.54 s / 96.33 MiB |

100k open profile 仍为 `bounded_view = true`，row/index entries 与 bytes 都为 0。primary/secondary cold lookup 各检查并解码 1 行，warm 样本命中 snapshot-local cache。range/order 只读取 25 个结果候选，forward page 读取 `limit + 1 = 26` 个 index entries。full scan 按一个 1,024-row batch 解码，约 197 KiB working bytes，不随表总行数扩张。100k full check 遍历 100,000 rows 与 300,000 index entries，使用 600,000 次点查，报告 307 bytes 算法 working peak；进程 RSS 包含 redb 映射、运行时与完整检查自身。

100k shadow migration 的 p50 分段为 build 6.69 s、validate 3.30 s、cutover 10.08 ms、reclaim 5.51 s。每个样本读取并写入 100,000 rows、生成 500,000 index entries、处理约 40.21 MiB logical bytes，全部从 generation 1 原子切换到 generation 2 并完成旧 generation 回收。相比 M6 full-rebuild p95 约 49.94 s 和约 1.44 GiB peak RSS，新的 p95 为 16.51 s、peak RSS 为 427.98 MiB；维护仍明显重于普通请求，应保留窗口并在生产副本预演。

### 接口与故障矩阵

| 验收面 | 可执行证据 |
| --- | --- |
| memory/redb ADT、scalar、query、DML 与错误一致 | `tests/language.rs`、`tests/protocol.rs`、各 scalar 集成测试与 `tests/storage.rs` |
| Rust Engine、prepared request、cursor、receipt、read-only | `tests/protocol.rs`、`tests/pagination_interfaces.rs`、`tests/interfaces.rs` |
| CLI → restart → migration → check → backup/restore | `tests/release_scenarios.rs` 与包内教程验证 |
| TCP 与 NDJSON stream/cancel | `tests/interfaces.rs` 的 versioned TCP、stream 与 shutdown 场景 |
| HTTP typed ADT、分页、幂等、stream | `examples/todolist.rs` 的测试与 CI 可执行示例 |
| deadline/cancel、进程退出、磁盘失败、恢复 | `tests/concurrency.rs`、`tests/storage.rs`、`tests/migration.rs` |
| format upgrade、logical backup/restore | `tests/storage.rs`、`tests/backup.rs` 与 release verifier |

10k 继续作为当前推荐的舒适工作集。100k 是已测上限：bounded read/open/check 已满足结构预算，但 100-row write 和完整 migration 仍具有明显延迟与 RSS 成本。实际部署必须按真实 value 宽度、索引数量、写入频率与 migration 复测。

## English Description

This record closes the M7 capacity rerun for [#186](https://github.com/worktools/unionid/issues/186). Measurements use an Apple M1 Pro with 16 GiB RAM, macOS, Rust 1.94.0, and storage format 6. They characterize this build and machine and are not an SLA. Raw evidence is retained in the linked 10k/100k workload and recovery JSON files above.

`tools/workload-eval` uses a separate database copy for every open and migration sample. Release child processes exclude cloning, plan assertions, and integrity checks from timed intervals. Queries retain an initial and 20 timed execution observations; page cursor preparation may prime the cache, and the initial observation preserves those hit/miss counters. Migrations run through `MigrationFile` and the real format-6 shadow-generation path, retaining phase times, generation IDs, row/index counts, logical bytes, and reclamation state. `tools/recovery-eval` independently measures open, cold/warm indexed reads, and full checks.

At 100k rows, open p95 is 12.02 ms with no resident row/index scan. Cold primary and secondary lookups decode one candidate; range/order decode 25 candidates; forward page seek reads the required 26 index entries; a full scan with `take 1` decodes one bounded 1,024-row batch. A full check takes 1.54 s and 96.33 MiB process RSS. Shadow migration p50/p95 is 15.53/16.51 s with 427.98 MiB peak RSS; its p50 phases are 6.69 s build, 3.30 s validation, 10.08 ms cutover, and 5.51 s reclamation. Every sample switches a complete generation and finishes reclamation.

The repository test matrix covers memory/redb semantics, Rust and prepared APIs, CLI restart/migrate/check/backup journeys, versioned TCP, HTTP, NDJSON streaming and cancellation, cursor and receipt behavior, read-only boundaries, deadlines, process exits, disk failures, upgrades, recovery, packaging, and the tutorial on macOS/Linux CI.

10k remains the recommended comfortable working set. 100k remains a tested ceiling: bounded reads, open, and checks meet their structural budgets, while 100-row writes and complete migrations still have material latency and RSS costs. Deployments must rerun the workload with their actual value widths, index counts, write rates, and migrations.
