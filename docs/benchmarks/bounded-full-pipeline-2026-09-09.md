# M7 有界 full pipeline 与 check 记录 / bounded full-pipeline and check record

## 中文说明

本记录为 [#183](https://github.com/worktools/unionid/issues/183) 保存 10k/100k typed ADT 工作集上的结构化证据。测量使用 #183 分支、Apple M1 Pro（16 GiB）、macOS 26.6.2 和 Rust 1.94.0；数值只描述本次实现与机器，不是 SLA。

```bash
cargo run --release --locked --manifest-path tools/recovery-eval/Cargo.toml -- /tmp/unionid-bounded-full-10k.redb 10000
cargo run --release --locked --manifest-path tools/recovery-eval/Cargo.toml -- /tmp/unionid-bounded-full-100k.redb 100000
```

工具在独立进程中准备数据、测量 open，再在另一个进程中执行完整检查。工作集包含一个 record、一个带 record payload 的 sum type、list 字段、主键和两个二级索引。`check_profile` 记录 redb backend 检查、逐行 typed decode、每行期望索引点查、逐索引反向 row/key 核对和峰值逻辑 working bytes；计数不包含业务值。

| 规模 | 数据库 | open / peak RSS | full check / peak RSS | rows / index entries | point lookups | 逻辑 working peak |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 10k | 8.03 MiB | 4 ms / 8.36 MiB | 149 ms / 15.59 MiB | 10,000 / 30,000 | 60,000 | 280 B |
| 100k | 64.25 MiB | 4 ms / 8.42 MiB | 1.44 s / 80.62 MiB | 100,000 / 300,000 | 600,000 | 280 B |

100k full check 相比前一切片的 5.16 s／554.17 MiB 降到 1.44 s／80.62 MiB。算法不再构造完整 typed `Database` 或完整派生 index set：它保留 catalog、当前 row、该 row 的索引键和 redb 自身检查状态。RSS 包含 redb 和进程运行时，不等于 `working_peak_bytes`；结构化的 280 B 指 unionid 逻辑验证器同时保留的最大业务相关 working state。

同一切片还让 full scan 在 source batch 上逐行融合 filter、match、derive、select 和 take；aggregate/group 只保留有界 accumulator，blocking sort 才进入 250,000 rows／64 MiB working buffer。NDJSON producer 直接从 pipeline sink 拉取行，不再先建立完整 `QueryResponse.rows`。logical backup format 4 使用同一个 committed view 做两次有界顺序读取，分别计算兼容 checksum 和写出 envelope；现有 reader、restore 与 golden roundtrip 保持兼容。

原始 JSON：[10k](data/bounded-full-pipeline-2026-09-09-10k.json) · [100k](data/bounded-full-pipeline-2026-09-09-100k.json)

## English Description

This record retains structured 10k/100k evidence for [#183](https://github.com/worktools/unionid/issues/183). It was measured from the #183 branch on an Apple M1 Pro with 16 GiB RAM, macOS 26.6.2, and Rust 1.94.0. The numbers describe this implementation and machine, not an SLA.

The evaluator prepares a record/sum/list workload with a primary key and two secondary indexes, then measures open and full check in separate processes. The check decodes each typed row, point-checks every expected index entry, walks every stored index entry back to its row and exact key, and retains no complete `Database` or derived-index set. At 100k rows it checked 100,000 rows and 300,000 index entries with 600,000 point lookups in 1.44 seconds and 80.62 MiB process peak RSS, down from the previous materializing path's 5.16 seconds and 554.17 MiB. Its value-related logical working peak was 280 bytes; process RSS also includes redb and runtime state.

The same slice fuses row-local full-scan stages, retains only bounded aggregate/group or blocking-sort state, pulls NDJSON rows directly from the query pipeline, keeps durable mutation candidates to their target/change set, and streams logical backup format 4 while preserving its checksum and restore semantics.

Raw JSON: [10k](data/bounded-full-pipeline-2026-09-09-10k.json) · [100k](data/bounded-full-pipeline-2026-09-09-100k.json)
