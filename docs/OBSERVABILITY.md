# 结构化请求与慢查询事件 / Structured request and slow-query events

## 中文说明

`ConcurrentEngine::with_observer` 为 embedded、TCP、HTTP 共用的执行入口增加版本化、值无关的 terminal event。默认的 `ConcurrentEngine::new` 不创建 observer，也不记录事件。每个进入 core 的请求恰好产生一个 `request_terminal`；达到阈值并通过采样的 read、write 或 stream query 另外产生一个内容相同的 `slow_query`。

```rust
use std::{sync::Arc, time::Duration};
use unionid::{ConcurrentEngine, Engine, ObserverConfig, RequestObserver};

let engine = ConcurrentEngine::with_observer(
    Engine::open_redb("app.redb")?,
    ObserverConfig {
        slow_query_threshold: Some(Duration::from_millis(100)),
        slow_query_sample_basis_points: 1_000, // 10%
        request_id_hmac_key: None,
    },
    Arc::new(MyObserver),
)?;
```

version 1 event 包含 operation class、schema revision、terminal outcome、内部 error code、固定 resource-limit class、总耗时、query prepare/plan/execute 时间、mutation candidate/commit 时间、stream emission 时间、值无关工作量和脱敏 plan shape。plan 只保留 access kind、最多 256 个 stage kind、截断标志、lookup 数量和 cardinality，不含表、字段或索引名称。失败发生在某个阶段完成前时，对应 phase 为缺省值；总耗时与 terminal outcome 始终存在。

事件绝不包含 query source、参数、业务 row、表／字段／索引名、cursor、idempotency key 或 stream operation capability。request ID 默认也不输出。只有显式提供进程外保管的 `request_id_hmac_key` 时，事件才包含 `hmac1:` 摘要，用于并发请求关联；摘要不可还原，也不应作为认证凭证。轮换 key 会切断轮换前后的关联。

`slow_query_threshold = None` 关闭慢查询事件。阈值比较使用完整 core latency，包含 admission、prepare、plan、execute、同步 commit 和 stream emission；细分字段只报告当前执行路径能够精确测量的阶段。`slow_query_sample_basis_points` 范围是 0–10,000，采样基于进程内 event sequence，既不读取请求内容，也不适合跨进程做稳定抽样。

observer 在请求线程中同步调用，因此实现应把小型事件复制到一个有界 channel 后尽快返回。unionid 会隔离 observer panic，不会让监控故障改变数据库结果；channel 满时应丢弃观测事件并在应用自己的指标中计数。事件由应用决定保留周期，建议只保留排障所需窗口并限制磁盘、队列和 label cardinality。不要在 observer 中重新附加源码、参数或业务 row。

可选 `tracing` feature 提供 `TracingObserver`，把同一个 JSON event 写到 `unionid::request` target，不启动 collector 或网络 endpoint：

```bash
cargo build --features tracing
```

排障时，先用 `METRICS.md` 的 p95 与 queue gauge 判断问题是否持续，再暂时降低 slow threshold 或提高 sampling。`rows_examined` 高而 `returned_rows` 低通常需要索引或更早的 filter；`working_peak_bytes` 高通常需要缩小 page/batch 或避免阻塞 stage；`commit_micros` 高应检查存储延迟。`storage_outcome_uncertain` terminal 表示必须重开并执行 `check --db`，不能按普通失败直接重试。

## English Description

`ConcurrentEngine::with_observer` adds versioned, value-free terminal events to the execution boundary shared by embedded, TCP, and HTTP callers. `ConcurrentEngine::new` remains unobserved by default. Every request admitted to the core emits exactly one `request_terminal` event. A read, write, or stream query that reaches the configured threshold and passes sampling emits one additional `slow_query` event with the same sanitized payload.

The version 1 event contains the operation class, schema revision, terminal outcome, internal error code, fixed resource-limit class, total latency, query prepare/plan/execute timings, mutation candidate/commit timings, stream emission timing, value-free work counters, and a sanitized plan shape. Plans contain the access kind, at most 256 stage kinds, a truncation flag, lookup counts, and cardinalities only. A phase remains absent when failure occurred before it could be measured; total latency and terminal outcome are always present.

Events never contain source text, parameters, business rows, table/field/index names, cursors, idempotency keys, or stream operation capabilities. Request IDs are omitted by default. Supplying an externally managed `request_id_hmac_key` explicitly enables an irreversible `hmac1:` digest for concurrent-request correlation. Rotating the key intentionally breaks correlation across the rotation boundary.

Set `slow_query_threshold` to `None` to disable slow events. The threshold uses complete core latency. Sampling ranges from 0 to 10,000 basis points and depends only on a process-local event sequence, never request content. Observer callbacks run synchronously, so production observers should copy the small event into a bounded channel and return promptly. Observer panics are isolated from database results. The application owns retention and should cap queue, disk, and label cardinality while avoiding any enrichment with query or row data.

The optional `tracing` feature supplies `TracingObserver`, which emits the same JSON event to the `unionid::request` target without installing a collector or exposing a network endpoint. Use aggregate metrics first, then temporarily lower the threshold or increase sampling for diagnosis. High examined-to-returned ratios suggest indexing or earlier filtering; high working memory suggests smaller pages/batches or fewer blocking stages. A `storage_outcome_uncertain` terminal requires reopening and `check --db` before retrying.
