# 离线 compact 空间收益与 no-op 证据 / Offline compaction space-reduction and no-op evidence

## 中文说明

### 范围与环境

本记录为 [#230](https://github.com/worktools/unionid/issues/230) 和 [#235](https://github.com/worktools/unionid/issues/235) 保存离线 compact 的真实空间收益与稳定 no-op 证据。输入直接复用 [#229](https://github.com/worktools/unionid/issues/229) 的长期 churn 数据库（format 6，同一 release 二进制与固定 seed），测量基于 Apple M1 Pro（16 GiB）、macOS ARM64、8 logical CPUs、Rust 1.94.0，运行 release 构建。结果描述这台机器和固定输入，不是 SLA。

原始结果：

- [10k JSON](data/compaction-10k-2026-09-11.json)
- [100k JSON](data/compaction-100k-2026-09-11.json)

运行命令：

```bash
tools/compaction-eval/target/release/unionid-compaction-eval \
  /tmp/unionid-compact-10k.redb 10006
tools/compaction-eval/target/release/unionid-compaction-eval \
  /tmp/unionid-compact-100k.redb 100006
```

输入分别是 churn 10k/100k 结果（`/tmp/unionid-churn-evidence-10k.redb`、`/tmp/unionid-churn-evidence-100k.redb`）的副本，避免破坏原始 churn 证据。每次运行在独立子进程中打开数据库、执行一次 `Engine::compact_storage`、运行完整 check、逐行核对行数，并记录压缩前后与重开后的文件字节、耗时和进程 peak RSS。每个规模运行两次：第一次回收空间，第二次验证稳定 no-op。

### 结果

| 指标 | 10k | 100k |
| --- | ---: | ---: |
| 压缩前行数 | 10,006 | 100,006 |
| compact 前文件 | 45.98 MiB | 744.84 MiB |
| compact 后 durable 文件 | 21.87 MiB | 219.18 MiB |
| 回收字节 | 24.11 MiB | 525.67 MiB |
| 文件缩小 | 52.4% | 70.6% |
| 逻辑行大小（churn） | 5.22 MiB | 52.27 MiB |
| 压缩后 logical / file | 23.9% | 23.9% |
| open / compact / check | 12.5 µs / 1.12 s / 0.29 s | 25.1 µs / 9.77 s / 2.93 s |
| 进程 peak RSS | 38.0 MiB | 281.4 MiB |
| 第二次运行 `changed` | false | false |
| 第二次运行 `reclaimed_bytes` | 0 | 0 |
| 第二次运行 compact 耗时 | 0.76 s | 7.03 s |

第二次运行在全新进程中打开第一次压缩后的数据库，`before_bytes == after_bytes == durable_bytes`，`changed=false`、`reclaimed_bytes=0`，schema identity 与行数保持不变。因此 no-op 在跨进程、跨打开的场景下是稳定的。

### durable 大小与 reopen 语义

redb 的 `Database::compact()` 返回时文件长度小于其持久化 region 布局，紧接着执行 `check_integrity`/读事务后，下一次打开会运行 repair 并把文件向上取整到 region 边界。首版实现据此在 native compact 之后、post-check 之前重开数据库，使 repair 发生在同一次维护操作内部：报告的 `after_bytes` 等于重开后的 durable 大小，`changed` 只在文件实际变小时为 true。这样重复运行稳定返回 no-op（本次实测第二次运行 `changed=false`、`reclaimed_bytes=0`）。重开与 repair 需要完整遍历文件，因此即使 `changed=false`，一次 compact 仍可能有秒级成本，不能当作廉价轮询。

### 使用结论

- 离线 compact 在真实 generation reclaim 后确实显著缩小文件：10k 约 52%，100k 约 70%；100k 从约 745 MiB 回落到约 219 MiB，接近逻辑行的 4 倍。
- 完整 check、typed/indexed query、cursor continuation、幂等重放、migration ledger 和 logical backup 在压缩后保持压缩前语义；没有引入新 sequence 或 receipt。
- 压缩是同步离线维护，会多次完整遍历并执行 redb 内部提交；10k 约 1 秒、100k 约 10 秒，100k peak RSS 约 281 MiB。应按生产数据副本预留维护窗口、内存和磁盘余量，并先保留 verified logical backup。
- 第二次 compact 稳定返回 no-op，证实高水位已被真正回收；但 no-op 仍会完整遍历，调用方不应频繁触发。
- 文件缩小比例和耗时为观测值，不是固定比例或 SLA。

## English Description

### Scope and environment

This record retains real space-reduction and stable no-op evidence for offline compaction for [#230](https://github.com/worktools/unionid/issues/230) and [#235](https://github.com/worktools/unionid/issues/235). Inputs reuse the [#229](https://github.com/worktools/unionid/issues/229) long-term churn databases (format 6, same release binary and fixed seed). Measurements used an Apple M1 Pro (16 GiB RAM), macOS ARM64, 8 logical CPUs, Rust 1.94.0, and a release build. Results characterize this host and fixed input and are not an SLA. Raw results are retained in the linked [10k](data/compaction-10k-2026-09-11.json) and [100k](data/compaction-100k-2026-09-11.json) JSON files.

Each input is a copy of the churn 10k/100k result so the original churn evidence is not destroyed. A child process opens the database, runs one `Engine::compact_storage`, performs a full check, verifies the row count, and records before/after/durable bytes, timings, and process peak RSS. Each size runs twice: the first run reclaims space and the second validates a stable no-op.

### Results

| Metric | 10k | 100k |
| --- | ---: | ---: |
| Rows before compaction | 10,006 | 100,006 |
| File before compact | 45.98 MiB | 744.84 MiB |
| Durable file after compact | 21.87 MiB | 219.18 MiB |
| Reclaimed bytes | 24.11 MiB | 525.67 MiB |
| File reduction | 52.4% | 70.6% |
| Logical row bytes (churn) | 5.22 MiB | 52.27 MiB |
| Logical / file after | 23.9% | 23.9% |
| open / compact / check | 12.5 µs / 1.12 s / 0.29 s | 25.1 µs / 9.77 s / 2.93 s |
| Process peak RSS | 38.0 MiB | 281.4 MiB |
| Second run `changed` | false | false |
| Second run `reclaimed_bytes` | 0 | 0 |
| Second run compact time | 0.76 s | 7.03 s |

The second run opens the compacted database in a fresh process with `before_bytes == after_bytes == durable_bytes`, `changed=false`, and `reclaimed_bytes=0`, while schema identity and row count stay identical. No-op is therefore stable across processes and opens.

### Durable size and reopen semantics

redb's `Database::compact()` returns while the file is shorter than its persisted region layout. After the mandatory `check_integrity`/read transaction, the next open runs repair and rounds the file up to a region boundary. The implementation therefore reopens the database after native compact and before the post-check, so repair happens inside the same maintenance operation: the reported `after_bytes` equals the durable size seen on the next open, and `changed` is true only when the file actually got smaller. This makes repeated runs a stable no-op (the second run here reports `changed=false` and `reclaimed_bytes=0`). Reopen and repair still traverse the whole file, so even a `changed=false` run can cost seconds and must not be treated as cheap polling.

### Operational conclusions

- Offline compaction materially shrinks a file after real generation reclamation: about 52% at 10k and 70% at 100k, bringing 100k from roughly 745 MiB down to roughly 219 MiB, near four times the logical rows.
- Full checks, typed and indexed queries, cursor continuation, idempotent replay, migration ledger, and logical backup retain their pre-compaction semantics; compaction adds no sequence step and no receipt.
- Compaction is synchronous offline maintenance that performs several full traversals and native redb commits: about one second at 10k and ten seconds at 100k, with about 281 MiB peak RSS at 100k. Rehearse on a production-data copy, reserve a maintenance window, memory, and disk headroom, and retain a verified logical backup first.
- A second compaction returns a stable no-op, proving the high-water mark was reclaimed; because a no-op still traverses the whole file, callers should not trigger it frequently.
- Shrink ratio and elapsed time are observations, not a fixed ratio or SLA.
