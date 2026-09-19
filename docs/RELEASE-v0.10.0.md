# unionid v0.10.0 发布说明

v0.10.0 完成「日常使用闭环」milestone，交付四类真实业务最常缺口的日常能力，并让新数据库直接具备这些能力。

## 主要变化

- **有界 typed map（#246）**：`Map<text, T>` 支持 `map { "key": value }` 字面量、Rust/serde、protocol v2、portable schema、value codec 与 redb 持久化；提供完整 typed equality、稳定 total order，以及 `contains_key`、`get`、`keys`、`values`、`entries` 和有界 `any`/`all`，并有条目数、字节与深度预算。
- **decimal 乘除、avg 与显式舍入（#247）**：`decimal P S` 支持 checked 乘除、`round`/`rescale`（必须显式舍入模式）、decimal `avg`，在 filter/derive/aggregate/migration、Rust/serde 与 wire 上保持一致，溢出、除零、精度损失保持稳定错误分类。
- **部分唯一索引（#248）**：`create unique index t (c) if <predicate>` 只约束求值为 true 的行，覆盖软删除邮箱与 Active 状态外部 ID 等条件唯一约束。insert/upsert/update/delete、批量写入、prepared mutation 与 migration 都在候选最终状态上维护约束并原子回滚；planner 只在查询过滤条件机械蕴含 index predicate 时使用该索引，`explain` 输出 `index_predicate`、`predicate_proven` 与 value-free 的 `predicate_rejections`；logical backup 与 portable schema 均可无损携带 predicate。
- **CLI/诊断体验（#363）**：依据真实试用改进首次建库、schema/query 检查、migration、备份恢复与只读查看 redb 的 CLI、诊断与错误输出；`version --format json`、`doctor`、`check` 与 introspection 的建议与实际能力保持一致。

## 格式与兼容

新 redb 数据库直接创建为 **storage format 10**，对应 catalog/value/index-key/receipt/maintenance/journal codec `6/3/4/3/1/0`，logical backup 为 **format 6**。二进制可读 storage format 1–11 与 backup format 1–6。

- typed map 与 partial unique index 随默认 format 10 可用，不再需要显式升级。
- 旧 format 6/7 仍可读写；显式 `upgrade --target 8/10` 与 `9/11` 支持 6→8→10 与 7→9→11；启用增量备份使 6→7、8→9、10→11。
- 没有原地降级；回到旧二进制前应从 logical backup 恢复。
- 本版本不改变 protocol（仍为 1/2）、stream protocol（1）或 query contract 版本。

```toml
[dependencies]
unionid = "=0.10.0"
unionid-derive = "=0.10.0"
unionid-query = "=0.10.0"
```

`unionid`、`unionid-derive` 与 `unionid-query` 应统一使用 0.10.0。

## 验证

- `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings` 与全部测试通过。
- `tests/release_scenarios.rs` 的 typed metadata、partial email 与 Active external ID 旅程覆盖 query/DML → restart → migration → check → backup/restore。
- 发布 workflow 在 Ubuntu 与 macOS 运行完整检查、v0.9.1 兼容性旅程、示例、打包与归档验收。

# unionid v0.10.0 release notes

v0.10.0 completes the daily-use-closure milestone with four of the most common everyday gaps, and makes them available in fresh databases by default.

## Highlights

- **Bounded typed maps (#246)**: `Map<text, T>` with `map { "key": value }` literals, Rust/serde, protocol v2, portable schema, value codec, and redb persistence; complete typed equality, a stable total order, and `contains_key`, `get`, `keys`, `values`, `entries`, plus bounded `any`/`all`, all under entry-count, byte, and depth budgets.
- **Decimal multiply/divide, avg, and explicit rounding (#247)**: checked multiplication and division, `round`/`rescale` with a named rounding mode, and decimal `avg`, consistent across filter/derive/aggregate/migration, Rust/serde, and the wire, with stable overflow, division-by-zero, and inexact-result classifications.
- **Partial unique indexes (#248)**: `create unique index t (c) if <predicate>` constrains only rows whose predicate is true, covering conditional uniqueness such as non-deleted emails and active external IDs. insert/upsert/update/delete, batches, prepared mutations, and migrations maintain the constraint on the candidate final state with atomic rollback; the planner uses the index only when the query filters mechanically imply its predicate, `explain` reports `index_predicate`, `predicate_proven`, and value-free `predicate_rejections`, and logical backups and portable schemas carry the predicate losslessly.
- **CLI, diagnostics, and error UX (#363)**: trial-driven improvements to first database setup, schema/query checking, migrations, backup/restore, and read-only redb inspection; `version --format json`, `doctor`, `check`, and introspection stay aligned with actual behavior.

## Formats and compatibility

Fresh redb databases are created directly at **storage format 10** with catalog/value/index-key/receipt/maintenance/journal codecs `6/3/4/3/1/0`, and logical backup **format 6**. The binary reads storage formats 1–11 and backup formats 1–6.

- Typed maps and partial unique indexes are available with the default format 10; no explicit upgrade is required.
- Older formats 6/7 remain readable and writable; explicit `upgrade --target 8/10` and `9/11` support 6-to-8-to-10 and 7-to-9-to-11, and enabling the journal moves 6-to-7, 8-to-9, or 10-to-11.
- There is no in-place downgrade; restore from a logical backup before returning to an older binary.
- This release does not change the protocol (still 1/2), the stream protocol (1), or the query contract version.

Keep `unionid`, `unionid-derive`, and `unionid-query` on version 0.10.0 together.

## Validation

- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and the full test suite pass.
- The typed metadata, partial email, and active external-ID journeys in `tests/release_scenarios.rs` cover query/DML → restart → migration → check → backup/restore.
- The release workflow runs the full checks, the v0.9.1 compatibility journey, examples, packaging, and archive acceptance on Ubuntu and macOS.
