# M7 Legacy0 有界读取记录 / bounded Legacy0 read record

## 中文说明

本记录为 [#182](https://github.com/worktools/unionid/issues/182) 保存 format-5 Legacy0 redb 的 10k/100k 结构化证据。测量使用 `8f5ffc9` 之上的 #182 实现、Apple M1 Pro（16 GiB）、macOS 26.6.2 和 Rust 1.94.0；结果描述这次实现与机器，不是延迟或容量 SLA。

命令：

```bash
cargo run --release --locked --manifest-path tools/recovery-eval/Cargo.toml -- /tmp/unionid-bounded-read-10k.redb 10000
cargo run --release --locked --manifest-path tools/recovery-eval/Cargo.toml -- /tmp/unionid-bounded-read-100k.redb 100000
```

每个 open 在独立进程中执行。工具要求 `bounded_view = true`，并要求 open profile 的 `row_entries`、`index_entries`、`row_bytes` 和 `index_key_bytes` 全为零。随后按唯一 text index 查询中间位置的一行：冷读必须只检查一个 index entry、解码一行并产生一次 cache miss；同一 committed view 的热读必须只检查一个 index entry、解码零行并产生一次 cache hit。峰值 RSS 在这两次查询后、full count 前采集。最后的 full count 和显式 check 只用于核对数据完整性。

| 规模 | 数据库 | open | open + indexed read peak RSS | cold examined / decoded / miss | warm examined / decoded / hit | full check / peak RSS |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 10k | 8.03 MiB | 4 ms | 8.20 MiB | 1 / 1 / 1 | 1 / 0 / 1 | 138 ms / 63.20 MiB |
| 100k | 64.25 MiB | 4 ms | 8.23 MiB | 1 / 1 / 1 | 1 / 0 / 1 | 5.16 s / 554.17 MiB |

结果证明普通 format-5 open 和有界 indexed read 不再构造完整 resident rows/indexes；100k 的 open 与 indexed-read 常驻内存没有随数据规模出现旧实现的完整 typed state 放大。每个 committed view 拥有独立、严格 32 MiB 的 row cache，提交后的新 view 从空 cache 开始；旧 redb MVCC snapshot 可跨新提交继续读取同一个 schema/sequence/row/index root。

本切片没有把 full scan、check、backup、blocking aggregate/sort 或 mutation candidate 改造成完整的 bounded batch pipeline。100k `check` 仍物化并核验完整逻辑状态，约 5.16 秒且峰值约 554 MiB；这些边界由 [#183](https://github.com/worktools/unionid/issues/183) 继续处理。

原始 JSON：[10k](data/bounded-legacy-read-2026-09-09-10k.json) · [100k](data/bounded-legacy-read-2026-09-09-100k.json)

## English Description

This record retains the format-5 Legacy0 evidence for [#182](https://github.com/worktools/unionid/issues/182). It was measured from the #182 implementation above `8f5ffc9` on an Apple M1 Pro with 16 GiB RAM, macOS 26.6.2, and Rust 1.94.0. These are implementation and machine observations, not an SLA.

Both the 10k and 100k runs opened a metadata-only committed view without traversing durable row or index entries. A cold unique-index lookup examined one index entry, decoded one row, and missed once; the repeated lookup examined one entry, decoded no row, and hit the view-local cache. The 100k open plus indexed lookups peaked at 8.23 MiB RSS. Full integrity checking remains a materializing maintenance path at 5.16 seconds and 554.17 MiB for 100k, and is assigned to #183.

Raw JSON: [10k](data/bounded-legacy-read-2026-09-09-10k.json) · [100k](data/bounded-legacy-read-2026-09-09-100k.json)
