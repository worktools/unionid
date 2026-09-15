# 增量备份发版候选评估 / Incremental backup release-candidate evaluation

## 中文说明

`tools/incremental-backup-eval` 为 #309 的最终容量验收提供可重复方法。它不把微基准的 journal bytes 当作 archive 大小，而是实际执行 `init → mutation → export → verify → restore-to-sequence`，同时生成同一状态的 format-4 logical backup 作为比较基线。

报告分别保存：

- 无 journal 与有 journal 的单行主键更新原始耗时、p50/p95，以及 durable profile 的 journal 时间和 bytes；
- baseline、全部 segment、完整 archive 和 mutation 前后 logical backup 的实际文件 bytes；
- segment/logical 比例（basis points），避免跨平台浮点表示差异；
- 独立恢复进程的 restore、完整 check、逐页逐行等价检查时间和 peak RSS；
- sequence、schema revision/hash、row count、OS、架构、逻辑 CPU 与 Rust 版本。

工具最多接受 100,000 行、1,000 个测量样本，每批不超过总行数。每次更新必须走增量 durable path；未启用 journal 时 profile bytes 必须为零，启用后必须非零。恢复目标必须不存在，工具不会覆盖旧目录或数据库。

普通 PR 只对独立 crate 执行 fmt/check/clippy。release workflow 在 Linux/macOS 运行 100-row smoke；只有显式启用 `run_v05_backup_evaluator` 的 v0.5 release-candidate workflow 才运行 10k/100k，并上传完整 JSON。正式结论应填写具体 candidate commit、两平台 workflow URL、机器信息与原始 artifact，不能把共享 runner 的绝对耗时当作 SLA。

## English Description

`tools/incremental-backup-eval` provides the reproducible final capacity method for #309. It does not treat microbenchmark journal bytes as archive size. It executes the real `init → mutation → export → verify → restore-to-sequence` path and creates format-4 logical backups of the same workload for comparison.

The report retains:

- raw, p50, and p95 primary-key update timings without and with the journal, plus durable-profile journal time and bytes;
- actual file bytes for the baseline, all segments, the complete archive, and logical backups before and after mutations;
- the segment/logical ratio in basis points, avoiding cross-platform floating-point representation differences;
- restore, full-check, page-by-page equality comparison time, and peak RSS from an isolated restore process;
- sequence, schema revision/hash, row count, OS, architecture, logical CPU count, and Rust version.

The tool accepts at most 100,000 rows and 1,000 measured samples, with each batch no larger than the total row count. Every update must use the incremental durable path. Profile journal bytes must be zero before journal activation and nonzero afterwards. The restore destination must not exist, and the tool never replaces an old directory or database.

Ordinary PRs only run fmt/check/clippy for the standalone crate. The release workflow runs a 100-row smoke on Linux and macOS. Only a v0.5 release-candidate workflow explicitly enabling `run_v05_backup_evaluator` runs 10k/100k and uploads complete JSON. The final conclusion must record the exact candidate commit, both platform workflow URLs, machine information, and raw artifacts; shared-runner absolute timings are not an SLA.
