# unionid v0.8.0 发布说明

v0.8.0 为需要保留代数数据类型语义的汇总和“每组前 N 项”查询补齐一个小而完整的切片。查询可以直接对完整 enum、record、tuple、option 和 list 去重或分组，也可以在不压平行的情况下追加分区排名。

## 用户可见变化

- `count_distinct expression` 使用完整 typed equality。constructor、payload、record 字段、`None`/`Some` 和 list 内容都参与去重，未分组空输入返回 `0`。
- `avg` 支持 int、float 和 duration。`avg int` 返回 `Option<float>`；float 与 duration 保留命名输入类型；空输入返回 `None`。decimal avg 在精度扩展与舍入规则明确前返回 `E_TYPE`。
- `window { ... }` 要求显式 `sort`，可选 `partition`，并追加一个或多个命名的 `row_number`、`rank`、`dense_rank` `int` 字段。stage 保留输入基数和原输出顺序，后续可继续 filter/select/sort/take。
- partition、排序、并列与去重沿用数据库现有的完整 ADT equality 和 typed total order；窗口与汇总继续受既有行数、工作内存、deadline 和取消预算约束。
- CLI 内置的 `unionid docs query`、formatter、binder、explain、Rust/CLI/TCP/HTTP 路径以及中英文文档使用同一语义。

## 升级与边界

v0.8.0 不改变 storage format、component codec、logical backup 或网络协议。最低 Rust 仍为 1.94，redb 固定为 4.1.0；新数据库仍创建为 storage format 6，二进制继续读取 format 1–7。v0.7 format-6/7 数据库和 logical backup 可直接使用，无需 storage upgrade、schema migration 或源码重写。

使用静态查询生成物的应用应以 v0.8 `query rust` 重新生成包含新聚合或窗口的查询，并重新编译客户端。升级前仍应保留 v0.7 二进制、数据库副本和已验证 backup，在副本上执行 doctor、check 与应用验收。

首版窗口不支持 frame、lag/lead、窗口 aggregate、用户定义窗口函数、磁盘 spill 或与 cursor page 组合。decimal avg 仍延后。产品边界保持单机、单数据库所有者、串行写入和约 10,000 行舒适工作集；100,000 行只是已测试上限。

## English Description

v0.8.0 completes a small, usable slice for aggregation and per-group top-N queries without discarding algebraic data type semantics. Queries can deduplicate and group complete enums, records, tuples, options, and lists, or append partitioned rankings while preserving rows.

### User-visible changes

- `count_distinct expression` uses complete typed equality. Constructors, payloads, record fields, option state, and list contents all participate; ungrouped empty input returns `0`.
- `avg` accepts integers, floats, and durations. Integer averages return `Option<float>`; float and duration averages preserve named input types; empty input returns `None`. Decimal average returns `E_TYPE` until precision extension and rounding are specified.
- `window { ... }` requires an explicit `sort`, accepts an optional `partition`, and appends one or more named `int` fields using `row_number`, `rank`, or `dense_rank`. It preserves input cardinality and output order, and later filter/select/sort/take stages remain available.
- Partitioning, ordering, ties, and distinctness reuse complete ADT equality and the typed total order. Aggregates and windows remain subject to existing row, working-memory, deadline, and cancellation budgets.
- The bundled `unionid docs query` reference, formatter, binder, explain output, Rust/CLI/TCP/HTTP paths, and bilingual documentation share these semantics.

### Upgrade and limits

v0.8.0 does not change storage formats, component codecs, logical backup, or network protocols. Rust 1.94 remains the minimum and redb remains pinned to 4.1.0. Fresh databases still use storage format 6 and the binary reads formats 1–7. Existing v0.7 format-6/7 databases and logical backups work directly without a storage upgrade, schema migration, or source rewrite.

Applications using static query output should regenerate queries that use the new aggregate or window surface with the v0.8 `query rust` command and rebuild their clients. Retain the v0.7 binary, a database copy, and a verified backup, then run doctor, check, and application acceptance on the copy before switching.

The first window release excludes frames, lag/lead, window aggregates, user-defined window functions, disk spilling, and cursor-page composition. Decimal average remains deferred. The product boundary stays one machine, one database owner, serialized writes, and a comfortable working set around 10,000 rows; 100,000 rows is only a tested upper bound.
