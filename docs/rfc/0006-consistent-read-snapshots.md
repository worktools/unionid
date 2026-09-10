# RFC 0006：一致并发读快照

状态：accepted and implemented

## 问题

旧服务把整个请求放在一个 `Mutex<Engine>` 中执行。它保证原子可见性，却让 full scan、sort 或 aggregate 在完整执行期间阻塞所有客户端，包括安全的读取与短写入。HTTP adapter 照搬该锁法还会占用 async runtime worker。

## 决策

`Engine` 把最后一次完整提交的 `Database` 与 idempotency receipt map 保存为 `Arc`。写请求仍在唯一 writer lock 中建立候选状态，持久提交成功后才用新 `Arc` 替换当前状态；确定失败和结果不确定的提交均不发布候选状态。结果不确定时仍关闭 durable handle 并禁止继续写。

服务提供 `ConcurrentEngine`：

- query、explain、introspection、receipt status/preview 在 writer lock 内只克隆 committed-state `Arc` 与小型元数据，随后在锁外解析、绑定和执行；
- mutation、confirmed receipt prune 与 maintenance 继续串行；
- 每个快照固定 schema revision/hash、commit sequence、rows、indexes、migration ledger 与 receipts，不混合两个提交；
- 分类使用与执行相同的 parser AST；解析失败走只读路径返回原诊断，快照自身仍拒绝 mutation；
- `with_exclusive` 为 migration/check 等管理操作提供同一串行边界。HTTP adapter 应在 blocking worker 中调用同步入口，示例见 `examples/todolist.rs`。

首版最多允许 8 个 active read snapshots。其余读取进入有界队列，等待受请求绝对 deadline 控制；shutdown 不再接纳新快照，已执行请求由原 deadline 收敛。`ConcurrencyStats` 暴露 active/queued reads/writes、peak active reads 和上限，并按进程生命周期累计 read/write admission 次数、总 queue wait 与最大 queue wait（微秒）；这些无业务值的聚合量为 adapter 评测提供统一口径，不是逐请求 tracing。没有脱离请求生命周期的公开快照句柄。

## 存储方案比较

| 方案 | 快照成本 | 查询复用 | 结论 |
| --- | --- | --- | --- |
| 每次深复制 memory database | O(database + receipts) | 完全复用 | 原型实测成本过高，未采用 |
| immutable `Arc<Database>` | O(1) 引用复制 | 完全复用 | 当前方案 |
| redb read transaction | 低 | 需重写 typed query/index 为按需解码 | 当前不采用；双实现会扩大 codec/planner 一致性风险 |
| 共享 catalog、按表 COW | O(1)，写复制更细 | 需重构内部 ownership | 工作集超过当前边界后再评估 |

并发查询仍各自拥有 working rows、sort buffer、group state 和 response；`Arc` 只消除 database/catalog/index/receipt 的重复快照。现有 250k working-row、100k result-row、16 MiB response 与 aggregate 限制继续逐请求生效。8 是明确的并发/内存预算，不是容量承诺。

## 基准

Apple Silicon macOS release build，10,000 行 typed 表，8 个读者各执行 5 次 full scan + derive + sort + aggregate：

| 模式 | 总耗时 | queries/s | peak RSS | peak readers |
| --- | ---: | ---: | ---: | ---: |
| 整请求串行 mutex | 814,928 µs | 49.08 | 42,074,112 B | 1 |
| immutable snapshots | 439,621 µs | 90.99 | 111,312,896 B | 8 |

该样本吞吐为 1.85×，peak RSS 为 2.65×。内存增长来自八个查询的独立 working state，而非八份 database snapshot。原始结果在 `docs/benchmarks/data/concurrency-2026-09-08-10k.json`。以下命令可复现；单机结果不是 SLA：

```bash
cargo run --release --locked --manifest-path tools/concurrency-eval/Cargo.toml -- 10000 8 5
```

## 验收与限制

确定性测试覆盖旧快照与后续写隔离、快照存活时写入不受阻塞、读槽/queue 上限、shutdown 拒绝新快照和 writer queue 统计。memory/redb 使用同一 committed-state 边界；version 1/2 TCP 与 HTTP adapter 复用同一协议入口。

本 RFC 不提供跨请求 transaction 或用户长期快照。显式 operation cancellation 与有背压的 streaming response 后续已由 #135 和 [RFC 0007](0007-cancellable-backpressured-streams.md) 实现，但不会把一次 read snapshot 延长为跨请求事务。

## English Summary

The service publishes each complete committed database and receipt state behind immutable `Arc` references. Reads capture those references under the single writer lock and execute outside it; writes and maintenance remain serialized. At most eight request-scoped snapshots run concurrently, deadlines bound queue waits, and shutdown rejects new admissions. Live counters expose active/queued reads and writes plus process-lifetime read/write admission counts, total queue wait, and maximum queue wait in microseconds. These value-free aggregates give adapters one evaluation boundary rather than per-request tracing. A 10k-row, eight-reader release sample improved throughput by 1.85× with 2.65× peak RSS because each query still owns its working state. Cross-request transactions and user-held long-lived snapshots remain out of scope. Cancellation and backpressured streaming were implemented later by #135 and RFC 0007 without extending snapshot lifetime into network emission.
