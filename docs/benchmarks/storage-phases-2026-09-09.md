# M7 open 与 full-rebuild 存储阶段记录

本记录为 [#178](https://github.com/worktools/unionid/issues/178) 保留同一 release build 在 10k/100k typed ADT 工作集上的结构化 phase 证据。它用于选择后续架构边界，不是其他机器或数据形状的延迟 SLA。

## 方法与边界

环境为 Apple Silicon macOS、Rust 1.94.0。每个规模保留 20 个 open、query、write 和 migration 样本；每次 open/migration 使用独立进程和未修改模板的新副本。命令为：

```bash
cargo run --release --locked --manifest-path tools/workload-eval/Cargo.toml -- /tmp/unionid-profile-10k.redb 10000 20 1500
cargo run --release --locked --manifest-path tools/workload-eval/Cargo.toml -- /tmp/unionid-profile-100k.redb 100000 20 1500
```

open profile 从 `Engine::open_redb` 内部计时，依次覆盖 redb open、必要 bootstrap、meta/catalog/row/index/migration/receipt 读取、typed `Database` 构造和逻辑验证。逻辑验证当前包含 receipt/legacy 边界检查，并从全部 typed rows 重新派生索引后与 durable index keys 比较。各阶段互斥；它们与 total 的小差额是函数调用和计时器开销。outer open 还包含调用方边界。

durable commit 的顶层互斥阶段是 prepare、transaction apply 和 sync。full rebuild 的 reload previous、encode next 和 diff 是 prepare 内的子阶段，不能再次加到 prepare 上。outer migration 与 durable total 的差额包括 parser/binder、完整 migration candidate 构造、结果生成和 Engine 边界。peak RSS 是整个子进程的峰值，不表示单一子阶段的瞬时分配。

profile 只含固定 phase、数量、encoded byte 数与耗时。它不含 schema/field 名、key/value、cursor secret、idempotency key 或 receipt payload，并且只在成功 open/commit 后发布。

## 主要结果

| 规模 | open outer p50/p95 | open validation p50/p95 | Database 构造 p50 | row+index read p50 | open peak RSS |
| --- | ---: | ---: | ---: | ---: | ---: |
| 10k | 156.08 / 160.95 ms | 84.28 / 86.31 ms | 50.19 ms | 13.32 ms | 75.91 MiB |
| 100k | 5.45 / 5.48 s | 4.72 / 4.74 s | 552.21 ms | 150.66 ms | 668.48 MiB |

100k open profile 统计 100,000 row entries、400,000 index entries、约 20.32 MiB encoded row key/value bytes 和 15.08 MiB index key bytes。raw redb iteration 并不是主要耗时；完整派生索引重算与一致性验证占 open p50 的约 87%。validation 从 10k 到 100k 增长约 56 倍，明显比 row/index read 的约 11 倍更快，后续设计不能继续把每次普通 open 的完整索引重算当作固定成本。

| 规模 | migration outer p50/p95 | durable p50/p95 | reload previous p50 | encode next p50 | diff p50 | apply+sync p50 | peak RSS |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 10k | 939.05 / 950.91 ms | 711.38 / 719.58 ms | 153.63 ms | 501.21 ms | 3.60 ms | 53.93 ms | 155.09 MiB |
| 100k | 49.60 / 49.89 s | 47.18 / 47.45 s | 5.44 s | 41.01 s | 42.16 ms | 762.01 ms | 1.43 GiB |

100k migration 的 durable delta 包含 100,000 row changes、166,666 index changes 和约 47.07 MiB encoded delta-plan bytes。该 byte 计数包含用于覆盖核对的旧编码、键和新编码，不是物理写入量。完整 next-state 编码占 durable p50 约 87%，重载并验证前态约 12%，transaction apply 与 redb sync 合计约 2%。优化 durable sync 或差异算法本身只能触及小部分总成本；maintenance generation 需要避免在一个进程峰值内同时持有并重复编码完整前后状态。

普通写仍保持增量路径。100k conditional update/upsert 的 durable p50 约 8.87/8.94 ms，100-row atomic batch 约 11.48 ms；其 reload/encode/diff 均为零，单行变化只编码一个 row，batch 编码 100 rows 与 400 index entries。profile 接入没有让普通写退回 full rebuild。

## 对 #179 的约束

1. redb 模式的普通 indexed/range/page open 不能以完整 row decode 和完整派生索引验证作为前置条件；snapshot 应绑定已验证 generation，并按 RowId/index span 解码实际候选。
2. 快速 open 仍须验证固定 meta、catalog identity、generation 状态和 durable key/version 边界；全量 row/index 一致性检查应成为有预算、可取消的显式 maintenance/check 路径。
3. migration 应写入 shadow generation，并按 checkpoint/resume 状态逐段解码、转换和编码。cutover 必须在一个原子事务中绑定 catalog、row/index generation、schema identity 与 sequence。
4. 旧 generation 在仍有 snapshot 引用时不能删除；进程退出与重开必须能区分 active、building、ready-to-cutover 和 reclaimable 状态。
5. 后续优化继续用相同 fixed phase schema 复测 10k/100k，并保留 raw samples。

完整 JSON：[10k](data/storage-phases-2026-09-09-10k.json) · [100k](data/storage-phases-2026-09-09-100k.json)
