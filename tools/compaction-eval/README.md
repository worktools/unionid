# 离线 compact 容量评测 / Offline compaction capacity evaluation

## 中文说明

该独立工具对一个已存在的 format-6 redb 数据库执行一次 `Engine::compact_storage`，并在独立子进程中测量压缩前后文件字节、回收字节、open/compact/full-check 耗时和进程 peak RSS。它不生成业务数据，也不自动运行 compact；输入应由 `tools/churn-eval` 等生成器准备，或复制自真实数据库。评测会原地修改数据库，因此对需要保留的证据应先复制副本。

```bash
cp /tmp/unionid-churn-evidence-10k.redb /tmp/compact-10k.redb
cargo run --release --locked --manifest-path tools/compaction-eval/Cargo.toml -- \
  /tmp/compact-10k.redb 10006
```

输出为单个 JSON：`before_bytes` / `after_bytes` / `reclaimed_bytes` / `changed` / `identity_preserved`，压缩后 schema revision/hash 与逐行 count 校验，`open_micros`、`compact_micros`、`check_micros`、`peak_rss_bytes` 和运行环境。压缩成功后再次运行同一文件应稳定返回 no-op，可用于验证没有更多可回收页面。

结果描述固定主机和固定输入，文件缩小比例、耗时和 RSS 都不是 SLA。已提交的 10k/100k 证据见 [compaction-2026-09-11.md](../../docs/benchmarks/compaction-2026-09-11.md)。

## English Description

This standalone tool runs one `Engine::compact_storage` against an existing format-6 redb database and, in a separate child process, measures before/after/uncompacted file bytes, reclaimed bytes, open/compact/full-check timings, and process peak RSS. It neither generates business data nor compacts automatically; prepare the input with `tools/churn-eval` or copy it from a real database. The measurement mutates the file in place, so copy any input whose original form must be retained.

```bash
cp /tmp/unionid-churn-evidence-10k.redb /tmp/compact-10k.redb
cargo run --release --locked --manifest-path tools/compaction-eval/Cargo.toml -- \
  /tmp/compact-10k.redb 10006
```

The output is a single JSON document with `before_bytes` / `after_bytes` / `reclaimed_bytes` / `changed` / `identity_preserved`, post-compaction schema revision/hash and row-count verification, `open_micros`, `compact_micros`, `check_micros`, `peak_rss_bytes`, and the runtime environment. Running the same file again after a successful compaction should return a stable no-op, proving no reclaimable pages remain.

Results characterize a fixed host and fixed input; shrink ratio, elapsed time, and RSS are not an SLA. Committed 10k/100k evidence lives in [compaction-2026-09-11.md](../../docs/benchmarks/compaction-2026-09-11.md).
