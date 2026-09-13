# Format 7 原子 backup journal 小型基准 / bounded format-7 atomic backup-journal benchmark

## 中文说明

本记录为 [#307](https://github.com/worktools/unionid/issues/307) 保存 journal mutation profile 与 workload evaluator 的有界证据。测量使用 Apple M1 Pro、macOS ARM64、8 logical CPUs、Rust 1.94.0 和 release 构建；输入只有 100 行，每个写入场景预热 5 次并采样 5 次。它用于验证计量和写集形状，不是容量结论或 SLA。普通 CI 只编译 evaluator；跨平台或更大数据复验留给发布工作流。

原始 JSON：[`data/journal-100-2026-09-13.json`](data/journal-100-2026-09-13.json)。

| 场景 | 总延迟 p50 / p95 | durable commit p50 / p95 | journal p50 / p95 | journal bytes p50 / p95 |
| --- | ---: | ---: | ---: | ---: |
| format 6 单行 indexed update | 8.955 / 9.079 ms | 8.731 / 8.863 ms | 0 / 0 µs | 0 / 0 B |
| format 7 同一 update + active journal | 9.059 / 10.983 ms | 8.815 / 10.795 ms | 22 / 28 µs | 573 / 573 B |

format 7 样本的业务 durable delta 为 422 B，journal 记录为 573 B，包括 begin/end、sequence/ordinal、parent checksum 和 canonical row change；派生 index delta 不进入 journal。5 个样本中 journal prepare/apply 的 p50/p95 为 22/28 µs，主要 durable 时间仍是同步 redb commit。样本量太小，不能把两组总延迟差解释为稳定性能开销；这里可复现的验收点是 format 6 profile 始终报告零 journal bytes，而显式启用后每次成功 update 都报告非零、固定形状的 journal bytes，并通过完整性检查。

复现命令：

```bash
cargo run --release --locked --manifest-path tools/workload-eval/Cargo.toml -- /tmp/unionid-journal-eval.redb 100 5 100
```

## English Description

This record preserves bounded mutation-profile and workload-evaluator evidence for [#307](https://github.com/worktools/unionid/issues/307). It used an Apple M1 Pro, macOS ARM64, 8 logical CPUs, Rust 1.94.0, and a release build. The input has only 100 rows; each write case uses five warm-ups and five measured samples. It validates instrumentation and write-set shape rather than establishing a capacity result or SLA. Regular CI only compiles the evaluator; cross-platform or larger reruns remain release-workflow work.

Raw JSON: [`data/journal-100-2026-09-13.json`](data/journal-100-2026-09-13.json).

| Case | Total p50 / p95 | Durable commit p50 / p95 | Journal p50 / p95 | Journal bytes p50 / p95 |
| --- | ---: | ---: | ---: | ---: |
| Format-6 single-row indexed update | 8.955 / 9.079 ms | 8.731 / 8.863 ms | 0 / 0 µs | 0 / 0 B |
| Same update with an active format-7 journal | 9.059 / 10.983 ms | 8.815 / 10.795 ms | 22 / 28 µs | 573 / 573 B |

The format-7 sample has a 422-byte business durable delta and a 573-byte journal record set, including begin/end records, sequence/ordinal, parent checksum, and the canonical row change. Derived index changes are omitted. Journal preparation/application measured 22/28 µs at p50/p95; synchronous redb commit still dominates durable time. Five samples cannot establish a stable total-latency delta. The reproducible acceptance result is that format-6 profiles report zero journal bytes, while every successful update after explicit enablement reports a nonzero, fixed-shape journal byte count and passes full integrity checking.

Reproduce with:

```bash
cargo run --release --locked --manifest-path tools/workload-eval/Cargo.toml -- /tmp/unionid-journal-eval.redb 100 5 100
```
