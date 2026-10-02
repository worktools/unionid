# RFC 0025：回执容量与显式保留策略 / Receipt capacity and explicit retention

- 状态 / Status: accepted via #424; capacity signals merged via #425; one-shot retention core/CLI in #426; service scheduling and lifecycle acceptance in development; not available in released v0.12
- Milestone: v0.13.0
- Issue: [#402](https://github.com/worktools/unionid/issues/402)
- 前置契约 / Prior contract: [RFC 0002](0002-idempotent-write-receipts.md)

## 中文说明

### 问题与范围

当前回执上限为 10,000 条、总编码量 64 MiB、单条 1 MiB。新 key 在容量不足时被拒绝；已有 key replay、不带 key 的写入仍可执行。`receipts status` 和 metrics 已有 count/bytes/limits，但没有一致的阈值告警和面向长期运行服务的显式保留策略。

本 RFC 交付预警、可执行的恢复指引和默认关闭的有界保留策略。保留原硬上限，不增加存储/backup/protocol codec，不提供 LRU、不自动缩短重试窗口、不引入业务行 TTL，不承诺任何流量下都不会满。旧的显式 `receipts prune` 保留。

### 容量状态与响应

统一从实际编码量和数量计算容量状态，count 或 bytes 任一达到上限的 80% 即为 warning，任一达到上限即为 full，否则 normal。整数比较避免浮点阈值误差；状态返回 count、encoded_bytes、各自 limit 与 remaining，并明确 byte 余量非零仍可能放不下下一条回执。

`receipts status`、`doctor` 的回执摘要和 metrics 使用同一计算。CLI JSON 增加可选的版本 1 capacity 描述；旧字段保持。Prometheus 使用固定名称的 gauge，不用 key、digest、query、路径或策略值作为 label。doctor 继续只检查私有副本，不启用策略或删除回执；若原本没有回执摘要则增加该摘要。

成功的新幂等提交在预计提交后达到 warning 时追加一条固定、有界、无业务数据的 warning。先确定是否需要告警，再把告警纳入最终 receipt 编码与容量检查；不能先提交数据再发现告警使 receipt 超限。警告说明“本次提交时容量接近上限”，不携带随重放变化的动态计数。原响应连同原 warning 一起持久化；已有 key replay 完整保持原响应，不重新计算或替换 warning。查询当前容量应使用 status/metrics。

`E_IDEMPOTENCY_CAPACITY` 的 hint 指向 `unionid receipts status --db <path> --format json`，以及预览后显式确认的 prune/retain 流程。示例路径保留占位符，不泄漏业务 key 或源查询。`E_IDEMPOTENCY_LIMIT` 单条过大与总容量不足明确区分，prune 不能解决单条过大。

### 最小保留窗口与配置

提出以下公开配置与命令；在实现验收前均不作为已发布接口：

```text
ReceiptRetentionPolicy {
  min_age_seconds: u64
  max_receipts: usize
}

unionid receipts retain --db app.redb --min-age-seconds 3600 --max-receipts 1000
unionid receipts retain --db app.redb --min-age-seconds 3600 --max-receipts 1000 --confirm

unionid server --db app.redb \
  --receipt-retention-seconds 3600 \
  --receipt-retention-interval-seconds 60 \
  --receipt-retention-max-receipts 1000
```

窗口必须大于零，秒转毫秒使用 checked 运算；max_receipts 范围 1–1,000。显式服务策略的默认周期为 60 秒，允许 1–86,400 秒；只传周期/批大小而没有窗口属于配置错误，不能隐式启用。只读或 WAL/snapshot 模式拒绝开启策略；memory 的本地 API 可以按 process-local 语义使用，durable CLI 示例使用 redb。

策略是运行时配置，不写进 catalog、receipt 或 backup；不通过远程业务请求修改它。服务重启需要部署配置再次明确启用，restore 本身绝不开始清理。对嵌入式及 HTTP adapter 提供同一个 Engine/ConcurrentEngine API；调用方负责显式调度。TCP server 的上述参数是内置调度入口。

### 选择与提交

Engine 提供 plan/apply retention API，复用现有 prune 的选择、原子提交、sequence 和 backup/journal 语义。每次在 writer 所有权下读取时间并计算 `cutoff = now_unix_ms.checked_sub(min_age_ms)`，仅选择 `completed_at_unix_ms < cutoff` 的回执。恰好位于边界的回执保留；窗口大于当前时间时没有可选回执。按既有 sequence/time/key 顺序选择，最多删除 max_receipts，不因容量满而提高上限或绕过窗口。

CLI 默认只预览；`--confirm` 用同一个实际 Engine 在执行时重新计算选择，预览报告不是可跨进程复用的授权令牌。报告至少包含 schema_version=1、policy、as_of_unix_ms、cutoff（可为空）、selected_count/bytes、remaining_count、applied；不显示被删除回执的 query/params/rows。沿用既有可选 boundary 标识的本地运维权限，metrics/日志只保留无业务数据的计数和错误码。

没有可删除回执时为明确 no-op，不增加 sequence。实际删除后按照现有 prune 契约增加 sequence，旧 sequence-pinned cursor 因而过期。删除之后同一个 key 可作为新请求再次执行；必须在 CLI help、文档和策略启动提示中说明。

### 服务调度与生命周期

不在业务请求的 key 查找、重放或容量检查路径里隐式清理。显式启用的服务维护任务按 monotonic interval 调度，一个实例同时至多一轮；每轮尝试获得同一个 writer 所有权，繁忙时跳过本轮并记录 busy，不堆积清理任务。accept loop 不执行清理，防止文件同步阻塞连接接收。shutdown 停止新的轮次，唤醒并 join 维护任务；正在进行的 durable commit 遵守既有完成/不确定结果契约，不强制中止它。

一轮最多一次有界 prune commit，剩余旧回执等下一周期。maintenance generation 未完成时暂停清理，记录 `E_MAINTENANCE_REQUIRED`；不自动推进/abort migration。确定失败保留旧状态，可在后续周期重试；不确定提交沿用 Engine 关闭句柄/禁止继续写的保护，不在后台循环重开并猜测成功。保留窗口内回执占满时，新 key 仍被拒绝，已有 key 仍可 replay。

保留最近一次执行结果（disabled/idle/busy/maintenance/clock_error/applied/failed）、删除数量和时间等固定大小信息；不保存回执列表。开启策略和失败须有可观察信号，不为每个无操作周期刷日志。现有 prune 可能走 full rebuild；本 RFC 不把“最多 1,000 回执”误称为固定耗时，文档与验证必须说明写入锁占用及维护成本。

### 时间边界

窗口按持久 receipt 的 UTC completed_at_unix_ms 计算，是墙钟年龄，不是跨重启的 monotonic 寿命保证。服务周期使用 monotonic clock，避免时钟调整造成密集补跑。读取系统时间失败、早于 Unix epoch 或运算溢出时不删除；进程内发现墙钟回退时暂停清理，直到追上已观察时间。未来完成时间的回执不可选。

时钟向前跳或恢复到另一台时间错误的机器可能使回执显得更旧；不声称该策略能在任意错误时钟下证明真实重试窗口。启动提示和文档要求可信 UTC 时间、正确的重试窗口及恢复流程。若业务要求不依赖墙钟的保留证明，继续使用显式 sequence cutoff 的 prune；持久时间证明/外部时钟服务不在此版范围。

### 容量规划与验收

规划使用“新 key 的成功提交速率”，而不是包含 replay 的请求总 QPS。至少为成功速率 × 最大重试窗口，加上调度滞后、写入突发、维护暂停与单条编码量留余量。清理吞吐上限还受 max_receipts / interval 限制；每秒 1 个新 key、窗口 1 小时约需 3,600 条，每条 1 KiB 约 3.52 MiB，但每秒 1 个 key、窗口 24 小时需要 86,400 条，已超当前数量上限，开启策略不能修复这个配置矛盾。

交付分为 RFC、容量信号、策略核心/CLI、服务生命周期、完整业务验收，不因前两步完成就关闭 #402。验收至少覆盖：

- 9,000+ 真实成功回执的 status/doctor/metrics/warning；count 与 byte 阈值、单条超限、满容量的新 key 拒绝和原 key replay。
- 告警随原 receipt 重放且不改变 digest；带 warning 的编码超过限制时，rows 与 receipts 一起回滚。
- 默认关闭、显式启用、非法配置、窗口等号边界、未来时间、回退/无效时间、空选择、1,000 上限及多轮逐步清理。
- 同一个服务中的写入、重放、清理竞争；busy 跳过、maintenance 暂停、shutdown join、确定/不确定提交失败和重开验证。
- 删除后旧 key 再次执行、未删除 key 仍重放；backup/restore 和增量恢复保留效果/回执一致性，恢复不自动启用策略。
- docs/agent/help 可发现完整运维路径。发布用法只发 Discussions；普通 CI 只跑 Ubuntu，macOS 完整验收留到发版前。

## English Description

### Decision and compatibility

Deliver capacity warnings, actionable recovery guidance, and an explicitly enabled, bounded retention policy for #402. Keep the limits of 10,000 receipts, 64 MiB total encoded bytes, and 1 MiB per receipt. Existing-key replay and writes without keys remain available at capacity. Do not add LRU, business-row TTL, automatic window shortening, or codec changes. Existing explicit prune remains supported.

Calculate a shared normal/warning/full status from count and actual encoded bytes, warning at 80% of either limit. Expose version-1 capacity information through status, doctor, and fixed-cardinality metrics. Remaining bytes do not guarantee that the next receipt fits. Doctor remains observational. A successful new keyed mutation near capacity includes one fixed, bounded warning describing that commit's state. Include it in final receipt size validation before commit. Replay preserves the original response and warning exactly; status/metrics provide current capacity. Capacity hints link to status and preview/confirm maintenance, distinguish oversized individual receipts, and expose no business payloads.

### Proposed interfaces and retention semantics

`ReceiptRetentionPolicy` contains positive min_age_seconds and max_receipts in 1–1,000. The proposed `receipts retain --min-age-seconds ... --max-receipts ...` previews by default; `--confirm` applies under the same actual Engine ownership. The proposed server flags above explicitly enable a window, cadence (default 60 seconds, range 1–86,400), and batch cap. Supplying cadence/cap without a window is invalid. Reject service policy in read-only and WAL/snapshot modes; local memory APIs retain process-local semantics.

Runtime configuration is not persisted in catalog/receipts/backups and is not remotely mutable through business requests. Restart requires explicit deployment configuration; restore never starts cleanup. Share plan/apply APIs between Engine, ConcurrentEngine, TCP, and explicitly scheduled embedded/HTTP integrations.

Under writer ownership, sample UTC time and compute a checked cutoff. Select only receipts strictly older than the window, ordered by existing sequence/time/key semantics, up to the configured cap. Equal-boundary and future-dated receipts remain protected. A window extending before the epoch selects nothing. Preview is not a reusable approval token. Reports include version, policy, evaluation time, optional cutoff, selection counts/bytes, remaining count, and applied status; business queries, parameters, and rows are never included. Empty selection changes no sequence; actual deletion follows existing atomic prune and expires sequence-pinned cursors. A pruned key can execute again and this must be explicit in help, docs, and startup messaging.

### Scheduling, clocks, and failure

Never hide cleanup inside request lookup/replay/capacity checks. An opt-in maintenance worker runs at monotonic intervals with at most one active pass, tries the same writer ownership, and skips busy intervals without building a queue. The accept loop does not perform cleanup. Stop admitting passes on shutdown, wake/join the worker, and preserve existing commit outcome rules for an in-flight operation. Each pass performs at most one bounded prune; remaining work waits for a later interval.

Pause during unfinished migration maintenance without advancing or aborting it. Definite failures preserve state and may retry on a later interval; uncertain commits retain Engine's closed-handle/write prohibition, with no automatic reopen/guess loop. Protected receipts can still fill capacity. Expose fixed-size last-pass state/counts/time and failures without receipt lists or per-no-op log spam. Existing prune can use full rebuild; bounded receipt count does not imply constant latency or a negligible writer lock hold.

Retention is wall-clock age from durable UTC completion timestamps, not a cross-restart monotonic lifetime guarantee. Use monotonic scheduling; invalid/pre-epoch/overflowing time deletes nothing, and an observed backward wall-clock movement suspends cleanup until time catches up. Future completion times are ineligible. Forward clock jumps or restoring onto a misconfigured clock can make receipts appear older: require trusted UTC time and document that limitation. Workloads requiring a clock-independent cutoff can retain explicit sequence-based prune; durable time proofs are out of scope.

### Capacity planning and acceptance

Use successful new-key rate × maximum retry window, plus scheduling lag, bursts, maintenance pauses, and encoded-byte headroom. Replay attempts consume no new receipt. Cleanup throughput is also bounded by batch cap / interval. At one new key/second, one hour needs about 3,600 receipts (3.52 MiB at 1 KiB each), while 24 hours needs 86,400 entries and cannot fit the current count limit. Enabling policy does not fix incompatible capacity requirements.

Close #402 only after RFC, warnings, core/CLI policy, service lifecycle, and full acceptance. Cover 9,000+ real receipts; count/byte/individual limits; stable replay warnings and atomic rejection of oversized warned receipts; disabled/enabled and invalid configuration; cutoff boundaries and clock faults; bounded multi-pass cleanup; concurrent writes/replays, busy/maintenance/shutdown behavior; definite/uncertain commits and reopen; post-prune reuse; backup/restore and incremental recovery without auto-enabled policy; and discoverable docs/agent/help. Publish release guides only in Discussions. Ordinary CI remains Ubuntu-only.
