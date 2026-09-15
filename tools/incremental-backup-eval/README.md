# unionid incremental-backup evaluation

## 中文说明

该工具生成包含命名 record、sum、list、主键和二级索引的 ADT 工作负载，用同一类单行更新分别测量未启用 journal 和启用 format-7 journal 的提交耗时。JSON 保存每个原始样本、nearest-rank p50/p95、durable profile 中的 journal 时间与编码字节，并比较压缩 baseline、增量 segment 和完整 logical backup 的大小。

工具随后在独立子进程恢复到 archive head，记录 restore、完整 check、逐页逐行源库对比的时间与进程 peak RSS。恢复结果必须保留 sequence、schema identity 和全部 typed rows；目标数据库保持 journal disabled。输出不包含业务值。

传入一个不存在的新目录：

```bash
cargo run --release --locked --manifest-path tools/incremental-backup-eval/Cargo.toml -- /tmp/unionid-incremental-10k 10000 30 1000
cargo run --release --locked --manifest-path tools/incremental-backup-eval/Cargo.toml -- /tmp/unionid-incremental-100k 100000 30 3000
```

每次运行会保留 source/restored redb、archive 和前后 logical backup，便于复查。请在空闲机器运行，并保存命令、commit、完整 JSON、OS/CPU/内存信息。结果描述指定机器和 workload，不是 SLA。普通 PR 不运行大规模样本；v0.5 release candidate 通过 release workflow 的 `run_v05_backup_evaluator` 开关在 Linux/macOS 各运行一次。

## English Description

This tool creates an ADT workload with named records, sums, lists, a primary key, and secondary indexes. It measures the same class of single-row update before and after enabling the format-7 journal. JSON retains every raw sample, nearest-rank p50/p95, the durable profile's journal time and encoded bytes, and compressed baseline/segment sizes beside complete logical-backup sizes.

It then restores the archive head in a separate child process and records restore, full-check, page-by-page source comparison time, and process peak RSS. The restored database must preserve its sequence, schema identity, and every typed row while leaving the journal disabled. The report contains no business values.

Pass a new directory that does not already exist:

```bash
cargo run --release --locked --manifest-path tools/incremental-backup-eval/Cargo.toml -- /tmp/unionid-incremental-10k 10000 30 1000
cargo run --release --locked --manifest-path tools/incremental-backup-eval/Cargo.toml -- /tmp/unionid-incremental-100k 100000 30 3000
```

Each run retains the source/restored redb files, archive, and before/after logical backups for inspection. Run on an otherwise idle machine and preserve the command, commit, complete JSON, and OS/CPU/memory details. Results characterize one machine and workload; they are not an SLA. Ordinary PRs do not run large samples. The v0.5 release candidate uses the release workflow's `run_v05_backup_evaluator` switch once on Linux and macOS.
