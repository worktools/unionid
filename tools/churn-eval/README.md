# 长期 churn 评测 / Long-term churn evaluation

## 中文说明

该独立工具从空 format-6 数据库构造固定 seed 的窄行、宽行与有限递归 ADT 行，随后执行多轮 update、upsert、delete、批量 insert、幂等写入与回执 prune。前三轮还会依次应用带 parent/checksum 的 shadow-generation migration，验证 plan、build、cutover、reclaim、ledger 和重开状态。每轮都会关闭并重开数据库，运行完整 check 和逻辑 backup，并把 typed rows 与独立内存参考模型逐项比较。

```bash
cargo run --release --locked --manifest-path tools/churn-eval/Cargo.toml -- \
  /tmp/unionid-churn 10000 20 20260910 1000
```

参数依次为输出路径前缀、初始行数、轮数、seed 和准备批量大小。CI 只运行小型结构 smoke：

```bash
cargo run --release --locked --manifest-path tools/churn-eval/Cargo.toml -- \
  /tmp/unionid-churn-smoke 30 3 20260910 15
```

JSON 保存每轮操作、migration phase/generation、逻辑行字节、数据库字节与增长量、receipt 状态、open/check/backup 时间、peak RSS、schema identity 和参考模型摘要。完整行校验按主键边界拆成最多 10,000 行的连续范围，逐行对照参考模型并增量计算完整 JSON 数组大小，避免超过单个 QueryResponse 的 100,000 行上限。相同参数会选择相同记录并产生相同最终逻辑摘要；时间、文件分配和 RSS 仍取决于机器。结果是复现与容量规划证据，不是 SLA。

## English Description

This standalone tool creates deterministic narrow, wide, and finite recursive ADT rows in an empty format-6 database, then runs repeated update, upsert, delete, batch insert, idempotent mutation, and receipt-prune rounds. The first three rounds also apply a parent/checksum shadow-generation migration chain and validate plan, build, cutover, reclamation, ledger, and reopen state. Every round closes and reopens the database, performs a full integrity check and logical backup, and compares all typed rows with an independent in-memory reference model.

Arguments are the output-path prefix, initial rows, rounds, seed, and preparation batch size. CI runs only the small structural smoke shown above. JSON retains each round's operations, migration phases/generations, logical row bytes, database bytes and growth, receipt state, open/check/backup timings, peak RSS, schema identity, and reference-model digest. Complete row validation partitions the primary-key domain into contiguous ranges of at most 10,000 rows, compares every row with the reference model, and incrementally computes the complete JSON-array size without exceeding the 100,000-row QueryResponse limit. Identical parameters select identical records and produce the same final logical digest; timing, file allocation, and RSS remain host-specific. Results are reproducibility and capacity-planning evidence, not an SLA.
