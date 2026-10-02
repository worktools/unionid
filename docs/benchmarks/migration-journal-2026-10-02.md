# 迁移 journal 有界工作集 / Bounded migration journal working set

## 中文说明

为 [#437](https://github.com/worktools/unionid/issues/437) 比较官方 v0.13.1 与流式 cutover 实现。机器为 Apple M1 Pro、16 GiB、8 logical CPUs、macOS ARM64，Rust 1.94.0 release 构建。原始数据与二进制 SHA-256 保存在 [JSON](data/migration-journal-2026-10-02.json)。旧二进制来自官方 v0.13.1，源码 `7bf030a1fd61adaafb05657e6482180ce5836eac`；新二进制来自 `1ff5956`，基于 main `9f1bb55`。后续 rebase 到 #445 不包含在本次容量测量中，两者软件版本字符串均为 0.13.1，必须用源码与 binary hash 区分。

输入为 60k/100k 行，`Item {id: int, label: text, body: text}`，主键 id、普通 label 索引、120 字节 body；迁移添加 `note text = "migrated"`。每个规模从同一关闭的 seed 数据库复制四组（新/旧二进制 × journal 开/关），各运行三次。每个测量 worker 只启动一个 migration 子进程，使用 `RUSAGE_CHILDREN.ru_maxrss`，不混入 init、查询、检查或恢复的内存。

| 行数 | 二进制 | journal | RSS 中位数 / 最大值 MiB | 耗时中位数 s |
| --- | --- | --- | ---: | ---: |
| 60k | 官方 v0.13.1 | 关 | 125.48 / 125.56 | 3.703 |
| 60k | 官方 v0.13.1 | 开 | 383.25 / 387.45 | 4.537 |
| 60k | 流式 | 关 | 136.81 / 136.95 | 3.997 |
| 60k | 流式 | 开 | 164.73 / 164.77 | 4.848 |
| 100k | 官方 v0.13.1 | 关 | 196.00 / 263.48 | 6.672 |
| 100k | 官方 v0.13.1 | 开 | 619.72 / 628.73 | 7.865 |
| 100k | 流式 | 关 | 262.97 / 263.02 | 6.253 |
| 100k | 流式 | 开 | 264.94 / 265.45 | 8.144 |

新实现的开/关 RSS 中位数比值为 1.20×/1.01×；保守地使用 journal 最大值除以无 journal 最小值，也分别为 1.21×/1.01×，本输入满足 issue 建议的 1.3× 观测阈值。相对官方版 journal 峰值中位数降低约 57%/57%。关闭 journal 的版本间基线也有差异，尤其旧版 100k 单次 RSS 波动较大；不能把全部版本间差异归因于这一修改，或将比值推广到任意 schema/硬件。

24 个迁移样本全部逐行核对 id、label、body、note，执行完整 `check`；12 个 journal 样本还 export 并恢复到 cutover sequence，完整检查恢复库，核对全部行、schema revision/hash 与规范行 SHA-256 相同。小型 Rust 回归另行验证 Legacy0/Generated source 的 add/rename/drop，旧 journal codec 字节完全相同，容量不足与取消后的原子回滚，以及 source posting 损坏时 Ready、schema、ledger 和 journal 不变。

实现按稳定键归并两个 generation，执行器只保留当前两条 entry 和一个编码 record，不构建完整行、索引或 journal frame maps。计数、删除、写入需要三遍扫描；journal 与 cutover 保持单个同步 two-phase transaction。源完整性检查没有删除，目标仍经过 Ready 检查。redb cache 和事务页仍占内存，RSS 并非常量；100k 仍是测试上限，非日常容量承诺。这里测量一次加字段迁移，不覆盖长期 churn、宽行、大型 receipts、复杂引用或多级 ADT。

复验工具不加入普通 PR 的重型 CI：

```sh
python3 tools/migration-journal-eval.py \
  --binary target/release/unionid \
  --previous-binary /path/to/official-v0.13.1/bin/unionid \
  --rows 60000,100000 --samples 3 --output /tmp/migration-journal.json
```

## English Description

This [#437](https://github.com/worktools/unionid/issues/437) comparison used an Apple M1 Pro, 16 GiB RAM, eight logical CPUs, macOS ARM64, and Rust 1.94.0 release builds. The [raw JSON](data/migration-journal-2026-10-02.json) contains all samples, version reports, and binary SHA-256 hashes. The previous executable is the official v0.13.1 artifact from `7bf030a1fd61adaafb05657e6482180ce5836eac`. The streaming executable was built from `1ff5956` on main `9f1bb55`, before rebasing onto #445. Both report software version 0.13.1; source commits and binary hashes identify the measured implementations.

Datasets contain 60k/100k rows with integer id primary key, ordinary text label index, and a 120-byte text body. A migration adds `note text = "migrated"`. Each size uses copies of the same closed seed database for previous/current binaries with journaling off/on, with three runs per combination. Each isolated measurement worker spawns only the migration process and reads child peak RSS; setup, validation, archive export, and restoration are excluded. The table above gives median/max RSS in MiB and median seconds for all combinations.

Streaming journal-on/off median RSS ratios are 1.20× at 60k and 1.01× at 100k. Even journal-on maximum divided by journal-off minimum is 1.21×/1.01×, within the issue's suggested 1.3× observed threshold for this input. Journal-on median RSS falls by about 57% at both sizes relative to the official artifact. Version-to-version journal-off baselines differ, and the old 100k baseline has a large outlier: neither every difference nor arbitrary workloads can be attributed to this change.

All 24 samples validate every migrated field and run full integrity checks. All 12 journal samples also export and restore at the cutover sequence, check the restored database, and compare every row, schema revision/hash, and canonical row digest. Separate Rust regressions cover Legacy0/Generated source generations, add/rename/drop migrations, byte-identical existing journal codecs, atomic rollback on capacity exhaustion/cancellation, and source-posting corruption retaining Ready state, schema, ledger, and journal head.

The executor merges ordered generation keys while retaining two current entries and one encoded record. Counting, deletions, and writes require three passes to preserve canonical record order and checksum. Source validation and target Ready validation remain intact. Journal records and cutover metadata still commit in one synchronous two-phase transaction. redb caches and transaction pages consume memory; bounded executor state does not imply constant process RSS. 100k remains a tested upper bound, not a daily-use capacity guarantee. These measurements cover one add-field migration, not long-term churn, wide rows, large receipts, complex references, or deep ADTs. The command above reproduces the comparison; this heavyweight evaluator is not added to ordinary PR CI.
