# 服务运行边界

unionid server 面向本机受信应用：默认监听 <code>127.0.0.1:7878</code>，使用自己的 JSON Lines 协议，不兼容 Redis wire protocol。当前不提供内置用户认证或 TLS；跨主机时使用仓库维护的 [Envoy mTLS 受控网络部署](DEPLOYMENT.md)，其中包含可执行配置、信任边界、审计和失败旅程。服务与参考网关都默认只绑定 loopback。

生产中的查询副本或受限应用可以从已有 redb 启动只读执行边界：

```bash
unionid server --db ./data/app.redb --read-only
```

`--read-only` 必须与 `--db` 一起使用，且数据库文件必须已经存在。查询、`explain`、`explain analyze`、introspection、migration status/plan 可继续使用；任何包含 schema 或数据修改的原子脚本，以及待应用的 migration，都会返回 `E_READ_ONLY`。拒绝发生在完整解析和参数绑定之后、候选数据库 clone 和 redb transaction 之前，因此参数错误仍会准确报告，混合读写脚本也不会执行其中的读取或部分写入。`.storage` / version 1 introspection 的 `read_only` 字段可用于启动探针确认实际边界。

## 有界资源

进程内请求、并发、连接、错误、延迟和 receipt 容量可通过 `ConcurrentEngine::metrics_snapshot()` 读取；默认关闭的 Prometheus 文本、指标语义与安全挂载方式见 [有界服务指标](METRICS.md)。单次请求关联、脱敏 plan/work 画像和慢查询阈值见 [结构化请求与慢查询事件](OBSERVABILITY.md)。exporter 与 observer 都不自动启动或注册 endpoint。

| 资源 | 当前边界 | 超限行为 |
| --- | --- | --- |
| 活动 TCP 连接 | 64 | 新连接收到 <code>E_BUSY</code> 后关闭；拒绝处理不创建 worker |
| 并发读快照 | 8 | 后续读取排队并受请求 deadline／shutdown 控制；统计暴露 active/queued reads |
| 已注册可取消读取 | 64 | 接纳前返回 `E_OPERATION_CAPACITY`；未启动 handle drop 后立即释放 entry |
| operation terminal tombstone | 256 / 60 秒 | FIFO/TTL 淘汰；淘汰后的合法 capability 返回 `unknown` |
| stream channel / frame | 8 帧、16 MiB queued / 16 MiB 单帧 | producer 背压并每 100 ms 检查控制信号；超限 `E_STREAM_LIMIT` |
| stream 总结果 | 100,000 rows / 256 MiB | terminal `E_LIMIT` / `E_STREAM_LIMIT`；不产生 partial complete |
| 请求 frame | 6 × 1 MiB + 256 bytes | 返回 <code>E_LIMIT</code> 并关闭该连接；该空间容纳 1 MiB 源码最坏 JSON 转义 |
| 查询源码 | 1 MiB / 100,000 tokens / 64 层 | 返回 <code>E_LIMIT</code> 或带位置的语法错误 |
| ADT value | 16 MiB encoded / 64 层 / 1,000,000 collection items | codec、恢复或写入拒绝超限值 |
| 查询 working rows | 250,000 | 返回 <code>E_LIMIT</code>；应增加选择性 indexed filter |
| 查询结果 rows | 100,000 | 返回 <code>E_LIMIT</code>；应增加 filter 或 take |
| 单个稳定 page | 1,000 rows / 16 sort keys / 8 KiB cursor | 返回 `E_PAGE_SHAPE`、`E_PAGE_ORDER` 或 `E_CURSOR_LIMIT`；应用应续页 |
| DML returning rows | 100,000 / 8 MiB typed wire rows | 提交候选状态前返回 <code>E_LIMIT</code>；应增加选择性 filter 或缩小投影 |
| Introspection payload | 1 MiB | version 1 请求返回带 request ID 与 schema 的 <code>E_LIMIT</code> |
| Version 1 request ID | 1 KiB UTF-8 | 在进入 Engine 前返回 <code>E_LIMIT</code>，避免写入提交后才发现响应元数据过大 |
| Idempotency key / receipt | 256 bytes / 1 MiB | 提交前返回 `E_IDEMPOTENCY_KEY` / `E_IDEMPOTENCY_LIMIT` |
| Receipt store | 10,000 / 64 MiB | 新 key 返回 `E_IDEMPOTENCY_CAPACITY`；现有 key 仍可 replay |
| TCP response | 16 MiB | 丢弃超限结果，发送小型结构化 <code>E_LIMIT</code> |
| 服务执行 deadline | 25 秒 | 返回 <code>E_TIMEOUT</code>；候选写批次不提交 |
| 空闲连接 / socket write | 30 秒 | 关闭空闲或不读取响应的客户端 |
| match coverage | 100,000 analysis steps | 返回 <code>E_LIMIT</code>，要求简化嵌套 pattern |

写批次与 migration 使用唯一 Engine writer mutex。读取在锁内捕获完整 committed `Arc<Database>` 后于锁外执行，最多同时运行 8 个；慢 scan/sort/aggregate 不再持有 writer lock，每次读取只观察同一 schema revision、commit sequence、rows 与 indexes。各查询仍独立分配 working rows、sort/group state 和 response，内存会随 active reads 增长。方案、故障边界和 10k 对照基准见 [RFC 0006](rfc/0006-consistent-read-snapshots.md)。查询扫描和 aggregate 输出定期检查 deadline；其他批次至少在每条语句前后检查，若计算期间越过 deadline，候选状态会被丢弃而不发布。排序受 working-row 上限约束；group/aggregate 另有限制 group 数、accumulator cell 和估算状态内存，查询局部函数限制定义数、调用深度和展开步骤，具体数值见 [QUERY.md](QUERY.md)。`explain` 只绑定查询并读取表／索引元数据和目标 posting，不扫描或复制数据行。

TCP response 使用限长 writer 直接编码，不先创建一个无界 JSON byte buffer。带 returning 的写入先在 Engine 候选状态内验证 typed wire rows 预算，version 1 request ID 也在执行前限长，避免已知 DML 结果在提交后才因响应超限被改写为失败。版本化响应超限时仍回显 request ID 与 schema；旧协议得到旧格式的 <code>E_LIMIT</code>。

## 关闭与失败

<code>SIGINT</code> 或 <code>SIGTERM</code> 触发优雅关闭：

1. listener 停止接纳新连接；
2. 正在读取但尚未提交请求的空闲连接被关闭；
3. 已进入 Engine 的请求完成，或因 deadline/error 丢弃候选事务；
4. 所有 worker 退出后释放 Engine 和 redb 独占锁；
5. stderr 输出 accepted/rejected/requests/failed 计数。

优雅关闭完成后可以立即以同一路径重新打开 redb。强制终止、掉电和 commit 结果不确定的恢复边界见 [存储说明](STORAGE.md)；正常 signal 测试不替代那些故障测试。

客户端断开不会回滚一个已经提交或正在提交的请求。request ID 只关联请求与响应，不是幂等键；带 `idempotency_key` 的 version 1 mutation 通过“数据效果与回执同事务”提供 exactly-once effect，但网络仍只是 best-effort delivery。未收到响应时，重开连接并原样重发 query、wire params、schema precondition 和 key；不要改变内容或猜测结果。规范 digest 和 commit uncertain 恢复见 [RFC 0002](rfc/0002-idempotent-write-receipts.md)。

分页读取没有 effect。客户端断开后，完整 response/page 读取可能继续到内置 25 秒 deadline；HTTP adapter 可设置更短绝对 deadline。stream 使用显式 capability 与有界 producer，TCP socket write 最多阻塞 5 秒，断开会触发同一清理信号，但只有 cancel response 或 terminal frame 是确认。完整 typed result 建立后立即释放 snapshot，再进入 emitting，因此慢客户端不会占住数据库快照。状态机和恢复边界见 [RFC 0007](rfc/0007-cancellable-backpressured-streams.md)。

receipt 没有自动 TTL/LRU。容量运维必须先 status/preview，再用明确 cutoff、最多 1000 条的单次边界和 confirm 原子清理。清理意味着旧 key 可以再次执行，保留窗口必须覆盖所有自动与人工重试。receipt 运维端点与数据库写入权限等价，HTTP adapter 必须鉴权并审计。

只读模式限制 unionid 的查询执行入口，并不把 redb 文件改成操作系统级只读格式，也不允许同一路径绕过独占打开锁。部署仍应配合文件权限、独立运行身份和只向受限进程暴露的数据库路径；需要安全重试的写服务由 [#113](https://github.com/worktools/unionid/issues/113) 跟踪 durable idempotency receipt。

## 嵌入式控制

应用可以调用 <code>server::serve_until(listener, engine, shutdown)</code>，通过共享 <code>AtomicBool</code> 发起同样的关闭流程，并在返回时获得 <code>ServerStats</code>。需要读取运行中并发统计或让 TCP/HTTP 共用执行边界时，构造可克隆的 <code>ConcurrentEngine</code>，调用 <code>stats()</code> 并传给 <code>serve_until_concurrent</code>；HTTP handler 应像 todolist 示例一样通过 blocking worker 调用同步数据库入口。命令行 <code>server</code> 已把 SIGINT/SIGTERM 连接到这个入口。

adapter 可先调用 `ConcurrentEngine::register_read(request, deadline)`，把返回 handle 的 server-issued `o1` capability 发送并 flush 给客户端后，再调用 `ReadOperation::start()`。`ConcurrentEngine::cancel` 只接受 canonical capability，并在线性化点返回 `accepted`、`already_terminal + outcome` 或 `unknown`；`register_read_with_shutdown` 额外把进程关闭信号接入同一检查顺序。operation registry 不保存 query/params，统计只暴露 registered/queued/executing/cancelling、累计 cancelled 和上限，capability 不得进入日志或持久化数据。

服务限制是 v0.2 的明确支持边界，而非容量承诺。M7 的 [1 万/10 万行验收记录](benchmarks/m7-acceptance-2026-09-10.md)显示，普通 open、增量单行写入、有序复合访问和完整检查使用有界 resident state；100-row batch 与完整 shadow migration 在 100k 工作集仍有明显成本。部署前应使用真实 value 宽度、索引数量和 migration 复测。

## 回执容量预警 / Receipt capacity warnings

开发中的 v0.13 在 `receipts status --format json` 的 `capacity` 和 `doctor --db ... --format json` 的 `database.receipt_capacity` 提供 version 1 容量描述。count 或 encoded bytes 任一达到 80% 为 `warning`，达到硬上限为 `full`，否则 `normal`；同时给出剩余条数和编码字节。剩余字节非零仍可能装不下下一条回执。doctor 只读取私有副本，容量摘要不包含 key、digest 或业务数据。

```sh
unionid receipts status --db app.redb --format json
# Replace the cutoff with a timestamp older than every supported retry window.
unionid receipts prune --db app.redb --before-unix-ms 1700000000000 --max-receipts 1000 --format json
# Inspect the preview before repeating with --confirm.
```

成功的新 key 提交在达到阈值时返回固定 warning，表示该次提交时的容量。告警计入回执大小检查并与业务数据原子保存。已有 key replay 保持原响应和原告警，即使后来 prune 使当前容量恢复正常；当前状态以 status/metrics 为准。单条超过 1 MiB 是 `E_IDEMPOTENCY_LIMIT`，不能通过清理别的回执解决。10,000 条或 64 MiB 总容量不足是 `E_IDEMPOTENCY_CAPACITY`，hint 指向 status 与显式预览/确认流程。

默认不会清理任何回执。可使用下述显式窗口命令手动执行一轮清理；服务内的周期调度仍由 #402 后续阶段实现。清理之后，同一个 key 可以再次产生效果。容量规划使用“成功的新 key QPS × 最大重试窗口”，同时预留突发、清理延迟和编码字节余量。已有 key 重放不增加条数；不能为腾空间擅自缩短业务重试窗口。

In development toward v0.13, status exposes version-1 `capacity`, while doctor exposes value-free `database.receipt_capacity` from its private copy. Either count or encoded bytes reaching 80% yields `warning`; reaching a limit yields `full`. Remaining bytes do not guarantee that the next receipt fits. Doctor reveals no receipt keys, digests, or business payloads.

Successful new-key commits near capacity include a fixed warning describing that commit. It participates in size checks before atomic data/receipt commit and is preserved unchanged on replay, even after pruning reduces current usage. Use status/metrics for current capacity. `E_IDEMPOTENCY_LIMIT` denotes one oversized receipt; pruning cannot fix it. `E_IDEMPOTENCY_CAPACITY` denotes insufficient total capacity and provides status plus preview/confirm guidance.

No receipts are automatically deleted. The age-window command below supports one explicit manual pass; automatic service scheduling remains later #402 work. Review a cutoff older than every supported retry window before using --confirm; a deleted key may execute again. Plan for successful new-key rate × maximum retry window, bursts, cleanup lag, and byte headroom. Replays consume no new receipt, and pressure is not permission to shorten the retry window.

### 显式保留窗口 / Explicit retention window

开发中的 v0.13 提供按 UTC 完成时间计算的单轮清理。以下命令默认只预览；窗口必须大于零并长于所有客户端、队列与人工重放的最大重试时间。每轮最多 1,000 条，默认 1,000，实际选择按提交 sequence/time/key 排序。

```sh
unionid receipts retain --db app.redb --min-age-seconds 3600 --max-receipts 100 --format json
# After verifying the preview and your retry window:
unionid receipts retain --db app.redb --min-age-seconds 3600 --max-receipts 100 --confirm --format json
```

只删除 `completed_at_unix_ms < now - window` 的回执，等号处、较新与未来时间的回执全部保留。窗口跨越 Unix epoch 时选择为空，报告 `cutoff_unix_ms: null`；不会退化为无时间限制的删除。没有候选时为 no-op，不增加 sequence。实际删除沿用 prune 的事务与 cursor 过期语义；业务 rows 不随回执删除。

预览在锁定后创建的临时副本上运行，源库字节不变；确认命令在实际 Engine 持有写入所有权时重新采样时间和选择，旧预览不是批准令牌。路径必须为已有数据库，不创建新库。JSON 为单个 version 1 对象，包含 policy、as_of_unix_ms、cutoff_unix_ms、selected_count/bytes、remaining_count、applied 和 key_reuse_warning。清理后的 key 可以再次执行，未删除的 key 继续重放。

Rust 调用方使用 `ReceiptRetentionPolicy { min_age_seconds, max_receipts }`、`Engine::plan_idempotency_retention(policy)` 和 `apply_idempotency_retention(policy)`。预览允许只读 Engine；执行拒绝只读、legacy WAL 和未完成的 migration maintenance。默认没有策略和后台任务，配置不写入数据库或备份。时间是 UTC 墙钟年龄，依赖正确的系统时钟；不宣称跨重启的 monotonic 寿命证明。服务周期调度、进程内时钟回退防护和 shutdown 集成仍未交付。

Development toward v0.13 adds one explicit UTC-age retention pass. Preview is the default; choose a positive window longer than every client, queue, and manual retry horizon. At most 1,000 receipts are selected per pass (default 1,000), in existing sequence/time/key order. Only timestamps strictly before `now - window` qualify; equal-boundary, recent, and future timestamps remain protected. A window extending before the epoch yields a null cutoff and an empty selection, never unrestricted deletion. Empty passes preserve sequence; actual pruning keeps existing transaction/cursor semantics and never deletes business rows.

Preview uses a locked temporary copy and preserves source bytes. Confirm samples time and selection again under the actual Engine's writer ownership; a prior report is not an approval token. The path must already exist. One version-1 JSON report carries policy, evaluation/cutoff times, selection/remaining counts, applied status, and a key-reuse warning. Deleted keys can execute again; retained keys still replay.

Rust callers use ReceiptRetentionPolicy and the Engine plan/apply methods above. Read-only preview is allowed; apply rejects read-only, legacy WAL, and unfinished migration maintenance. No background task or persisted policy is enabled. Age is UTC wall-clock age and assumes correct system time, not a cross-restart monotonic lifetime proof. Service scheduling, in-process backward-clock protection, and shutdown integration remain pending.
