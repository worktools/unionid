# unionid v0.12.0 发布说明

## 中文说明

v0.12 聚焦原子业务写入的正确性（#399、#415），发布验收由 #416 跟踪。

- DML 后可追加独立 `expect affected == 1`，也支持 `!= < <= > >=` 和非负整数常量。余额不足、收款人缺失或版本过期时返回 `E_EXPECTATION`，整个脚本确定回滚；错误带从 1 开始的 `statement_index` 和源码 span。无守卫的零行写入仍合法。
- 成功响应提供有界、按源码顺序排列的 `statements` 元数据。尾随守卫保留 DML returning 和 upsert metadata；最终查询仍提供顶层结果。回执、重开、逻辑备份和增量 journal 保存并重放完整摘要，失败不占幂等 key。
- 静态绑定和内联 `queries!` 接受一条 DML 加尾随守卫；多写入脚本使用 Engine/prepare、CLI、TCP 或 HTTP。`unionid docs query` 与 agent 清单包含规则和可运行版本领取示例。

新脚本最多 4,096 条顶层语句、512 KiB 编码摘要；摘要与回执预算在提交前检查。大量写入使用 insert many。Rust struct literal 需补 `Error.statement_index` 或 response 的 `statements`；构造器及旧 serde payload 的默认值保持可用。守卫仅支持 memory/redb；旧 WAL/snapshot 在提交前以 `E_CONFIG` 拒绝，历史大 WAL 记录仍可恢复。

storage/backup/protocol 与 v0.11 相同：默认 storage 10，可读 1–11；backup 6，可读 1–6；protocol 1/2、stream 1；组件 codec `6/3/4/3/1/0`。无需格式升级或应用 migration，三个 crate 同步为 0.12.0。维护带新摘要的回执时使用匹配版本，旧程序重写可能丢弃未知字段。

守卫只检查行数，不验证完整财务或领域规则。提交后的传输失败不保证回滚；用幂等 key 重试确认效果。详见 [RFC 0023](rfc/0023-atomic-business-write-guards.md) 和 [升级说明](UPGRADING.md)。原生发布包新增转账失败回滚、错误定位、摘要、CLI 文档发现和 restart/backup/journal 回执旅程；发布前仅运行一次完整 Ubuntu/macOS gate，并验证已发布 v0.11 包的直接兼容路径。

## English Description

v0.12 focuses on atomic business-write correctness (#399, #415); #416 tracks release acceptance.

- Append an independent affected-row guard after DML. Six comparisons with nonnegative integer literals are supported. Insufficient funds, missing recipients, or stale versions return E_EXPECTATION and roll back the entire script, with a one-based statement_index and source span. Unguarded zero-row mutations remain valid.
- Successful responses add bounded ordered statement metadata. Trailing guards retain mutation returning and upsert metadata; a final query owns the top-level result. Receipts preserve exact summaries across restart, logical backup, and journal recovery; failures do not occupy a key.
- Static bindings and inline queries! accept one DML plus its guard. Multi-write scripts use Engine/prepare, CLI, TCP, or HTTP. Bundled docs query and agent capabilities include the rules and a runnable version-claim example.

New scripts are capped at 4,096 statements and 512 KiB of encoded summaries, checked with receipt budgets before commit. Use batch inserts for bulk writes. Rust struct literals must initialize the added public fields; constructors and legacy serde defaults remain available. Guards require memory/redb; legacy WAL/snapshot reject before commit, while historical large WAL records remain recoverable.

Storage, backup, and protocol versions match v0.11: default storage 10, readable 1–11; backup 6, readable 1–6; protocol 1/2, stream 1; codecs 6/3/4/3/1/0. No format upgrade or application migration is required. Keep all three crates on 0.12.0, and use matching binaries to maintain new receipt summaries; older programs may discard unknown metadata when rewriting.

Guards prove cardinality rather than complete domain correctness. Post-commit transport failures do not establish rollback; resolve effects through idempotent retries. Native-package acceptance adds transfer rollback, diagnostics, summaries, CLI documentation discovery, and receipt restart/backup/journal journeys. Run the full Ubuntu/macOS gate once before publication and verify direct compatibility with the published v0.11 package.
