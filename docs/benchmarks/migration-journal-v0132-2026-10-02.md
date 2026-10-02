# v0.13.2 迁移 journal 复验 / Migration journal patch verification

## 中文说明

这是 [#437](https://github.com/worktools/unionid/issues/437) 的隔离补丁复验，区别于同日 main 预发布实现的[首次评估](migration-journal-2026-10-02.md)。候选二进制构建自 `124fa4af6a9002faa517659c55ef2c7112fe1c3b`，软件版本 0.13.2，SHA-256 `85f5364433e58109245613e8b3e220e0e10c321d26ad64ac50533a065a5e7fbb`。基线为正式 v0.13.1 包。机器、编译器、数据形状和工具与首次评估相同；每组仍为三次隔离 migration 子进程。全部[原始样本](data/migration-journal-v0132-2026-10-02.json)保存二进制版本、hash、精确行/schema 核对证据。

| 行数 | 二进制 | journal | RSS 中位数 / 最大值 MiB | 耗时中位数 s |
| --- | --- | --- | ---: | ---: |
| 60k | 正式 v0.13.1 | 关 | 136.86 / 137.06 | 3.588 |
| 60k | 正式 v0.13.1 | 开 | 387.38 / 387.45 | 4.487 |
| 60k | 候选 v0.13.2 | 关 | 136.77 / 136.95 | 3.445 |
| 60k | 候选 v0.13.2 | 开 | 165.44 / 165.50 | 4.443 |
| 100k | 正式 v0.13.1 | 关 | 195.86 / 195.95 | 6.385 |
| 100k | 正式 v0.13.1 | 开 | 626.45 / 626.52 | 7.815 |
| 100k | 候选 v0.13.2 | 关 | 263.52 / 263.77 | 6.436 |
| 100k | 候选 v0.13.2 | 开 | 265.09 / 265.14 | 7.982 |

候选 journal 开/关的中位数比值为 1.21×/1.01×；journal 最大值除以无 journal 最大值为 1.21×/1.01×，但 journal 最大值除以无 journal 最小值为 1.32×/1.35×。后者高于 issue 示例中的 1.3×，所以不能宣称每个样本或任意 workload 均满足该阈值。100k 无 journal 样本为约 196/264/264 MiB，而 journal 样本约 261–265 MiB；保留这种波动，不挑选基线。相对正式版，journal RSS 中位数降低约 57%/58%；这是固定输入的实测收益，不是 SLA 或容量保证。

24 个迁移和 12 个增量恢复样本均逐行核对并完整 check；恢复到 cutover sequence 后 rows、schema revision/hash 与 canonical row digest 完全一致。恢复路径与不覆盖行为由真实 CLI 回归补充。正式 v0.13.1 包的 format 10/11 → 候选兼容性旅程也通过数据、备份、旧回执和 TCP client 核对。

补丁没有引入 v0.14 引用功能或新格式。回移使用 v0.13.1 的有界 row/index 校验器，并为 cutover 的 source 校验添加取消轮询；完整 source 校验及 target Ready 校验仍保留。执行器不构造全量 row/index/frame maps，但 redb cache 和单次原子事务页仍占资源。100k 仍是已测上限；此次只验证一个添加默认字段的工作集，未推导其他 schema、深度、receipts 或平台的内存结论。复验仍使用 `tools/migration-journal-eval.py`，替换 `--binary` 为准确的补丁候选，不加入日常重型 CI。

## English Description

This is the isolated [#437](https://github.com/worktools/unionid/issues/437) patch verification, separate from the same-day [pre-release main evaluation](migration-journal-2026-10-02.md). The executable was built from `124fa4af6a9002faa517659c55ef2c7112fe1c3b`, reports 0.13.2, and has SHA-256 `85f5364433e58109245613e8b3e220e0e10c321d26ad64ac50533a065a5e7fbb`. The baseline is the official v0.13.1 package. Host, compiler, input shape, and evaluator are unchanged, with three isolated migration processes per combination. [Raw samples](data/migration-journal-v0132-2026-10-02.json) retain binary versions/hashes and exact row/schema verification evidence. The table above gives median/max RSS in MiB and median seconds.

Candidate journal-on/off median RSS ratios are 1.21×/1.01×. Max-on/max-off ratios are also 1.21×/1.01×, but max-on/min-off ratios are 1.32×/1.35×, above the issue's example 1.3× threshold. We therefore do not claim that every sample or arbitrary workload meets it. The 100k journal-off samples vary around 196/264/264 MiB, versus journal-on samples around 261–265 MiB; all baseline variation is retained. Journal median RSS falls by approximately 57%/58% against the official artifact. These are observations for a fixed input, not an SLA or capacity guarantee.

All 24 migrations and 12 incremental restores pass exact row comparison and full integrity checks. Restores at cutover preserve rows, schema revision/hash, and canonical row digests. Real CLI regressions additionally cover destination paths and no-overwrite behavior. The official v0.13.1 format-10/11 compatibility journey validates data, backups, legacy receipts, and TCP clients against the candidate.

The patch excludes v0.14 references and new formats. It uses the v0.13.1 bounded row/index validator, adding cancellation polling to source validation before cutover. Full source and target Ready checks remain. The executor builds no complete row/index/frame maps; redb caches and pages in the single atomic transaction still consume resources. 100k remains a tested upper bound. One add-default-field workload cannot establish memory behavior for other schemas, depths, receipt sizes, or platforms. Reproduce with `tools/migration-journal-eval.py` using the exact patch binary; no heavyweight ordinary CI is introduced.
