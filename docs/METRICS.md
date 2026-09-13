# 有界服务指标 / Bounded service metrics

## 中文说明

`ConcurrentEngine::metrics_snapshot()` 返回进程内、版本为 1 的 `MetricsSnapshot`。它覆盖所有通过该 shared engine 执行的 embedded、TCP 与 HTTP 完整请求，以及 registered stream/cancel operation；内置 TCP listener 还记录连接数。快照只包含固定 operation class、内部错误码、累计计数、固定延迟 bucket、并发 gauge 和 receipt 容量，不保存源码、参数、row、表／字段名、request ID、cursor、idempotency key 或 operation capability。

```rust
let snapshot = engine.metrics_snapshot();
let p50_upper = snapshot
    .operations
    .read
    .latency
    .percentile_upper_bound_micros(5_000);
let p95_upper = snapshot
    .operations
    .read
    .latency
    .percentile_upper_bound_micros(9_500);
```

operation class 固定为 `read`、`write`、`introspection`、`receipt`、`stream` 和 `maintenance`。stream query 完成、cancel control 和注册失败各记录一次；因此 stream 计数表示操作请求，不等于结果流数量。需要让管理操作进入 maintenance 统计时，使用 `ConcurrentEngine::with_maintenance`；通用 `with_exclusive` 无法判断任意返回类型是否成功，不计入请求指标。

完整请求只有进入 `ConcurrentEngine` 后才计数。HTTP JSON extractor、认证代理或 TCP envelope decode 在调用 engine 之前拒绝的输入属于 adapter 指标；内置 server 退出时的 `ServerStats` 仍包含其 raw request/failure 总数。这样 core metrics 不需要保存或重新解释无效 wire payload。

延迟从进入 `ConcurrentEngine` 到得到 terminal result，包含 queue wait、snapshot admission、执行和同步提交。bucket 是累计 upper-bound 微秒数；`percentile_upper_bound_micros` 返回 bucket 上界，并非精确分位。超过最大 bucket 的请求进入 histogram 的总 count，在 Prometheus 输出中进入 `+Inf`。计数使用饱和加法，不会回绕。

错误 label 只来自 unionid 返回的内部 `Error.code`；`with_maintenance` 的任意调用方错误统一为固定 `E_MAINTENANCE`。registry 最多保存 64 个不同 code，之后的新 code 只增加 `error_code_overflow`。这防止 label cardinality 无界增长；已有 code 仍继续计数。

receipt count/bytes 在 engine 创建时读取一次，并在 exclusive write/maintenance 改变 receipt 数量后刷新；普通写入不遍历 receipt 集合。并发字段是 snapshot 时的瞬时值。内置 TCP connection 只统计 `serve_until_concurrent` 接受的 socket；Axum 等外部 server 应使用自己的 transport metrics 统计连接，因为 unionid adapter 不拥有 listener。

这些指标属于一个 `ConcurrentEngine` 实例的进程内生命周期；进程重启或重新创建实例时 counter 会从零开始。快照逐项读取 lock-free counter，因此并发请求发生时是弱一致的观察：每一项都来自有效状态，但不同项可能相差一个正在完成的请求。监控系统应把 counter 当作单调累计值并用 rate 观察，不要要求一次 scrape 中所有字段构成事务快照。

### 可选 Prometheus 文本

默认 feature 不包含 exporter。显式启用 `metrics` 后，可把同一个快照渲染成 Prometheus text exposition：

```bash
cargo run --features metrics --example metrics
```

```rust
let body = engine.metrics_snapshot().prometheus_text();
```

该方法只生成字符串，不启动 listener，也不注册路由。应用应把 metrics route 放在受控管理网络并增加认证；不要直接暴露到公网。scrape 会复制一个小型固定快照和最多 64 个 error-code entry，不读取数据库 row。Prometheus 可用 `histogram_quantile(0.95, sum by (le, operation) (rate(unionid_request_duration_seconds_bucket[5m])))` 估算近期 p95。

容量判断时同时观察 active/queued read/write、operation registry、错误 overflow、receipt count/bytes 与各 operation 的延迟。持续 queue 增长或 p95 接近 25 秒请求 deadline 表示应先降低并发工作量、增加索引或缩小 page，而不是提高无界资源上限。

## English Description

`ConcurrentEngine::metrics_snapshot()` returns an in-process version 1 `MetricsSnapshot`. It covers complete embedded, TCP, and HTTP requests executed through that shared engine plus registered stream/cancel operations. The built-in TCP listener also reports connection counts. Snapshots contain only fixed operation classes, internal error codes, cumulative counters, fixed latency buckets, concurrency gauges, and receipt capacity. They never retain source text, parameters, rows, table or field names, request IDs, cursors, idempotency keys, or operation capabilities.

Operation classes are fixed to `read`, `write`, `introspection`, `receipt`, `stream`, and `maintenance`. A completed stream query, cancel control, or registration failure each records one stream request. Use `ConcurrentEngine::with_maintenance` when an admin operation should be observed. Generic `with_exclusive` cannot infer success from an arbitrary return type and is not counted.

A complete request is counted only after it enters `ConcurrentEngine`. Inputs rejected earlier by an HTTP JSON extractor, authentication proxy, or TCP envelope decoder belong to adapter metrics. The built-in server's terminal `ServerStats` still includes its raw request/failure totals. This keeps invalid wire payloads out of the core registry.

Latency starts when work enters `ConcurrentEngine` and ends at the terminal result, including queue wait, snapshot admission, execution, and synchronous commit. Buckets are cumulative microsecond upper bounds. `percentile_upper_bound_micros` returns a bucket boundary rather than an exact percentile. Counters saturate instead of wrapping.

Error labels come only from unionid `Error.code` values; arbitrary caller errors returned by `with_maintenance` map to the fixed `E_MAINTENANCE` label. The registry stores at most 64 distinct codes and directs later unseen codes to `error_code_overflow`, while existing codes continue counting. Receipt gauges refresh at construction and after an exclusive write or maintenance operation changes the receipt count; ordinary writes do not traverse the receipt set. TCP connection gauges apply only to the built-in listener; an external Axum server should provide its own transport connection metrics.

Metrics have the process-local lifetime of one `ConcurrentEngine`; counters restart when the process or engine instance is recreated. A snapshot reads lock-free counters independently and is therefore weakly consistent during concurrent work: every field is valid, but fields may straddle one completing request. Treat counters as monotonic series and observe rates rather than requiring one scrape to be a transactional snapshot.

The default build has no exporter. Enabling `metrics` adds `MetricsSnapshot::prometheus_text()` without adding a listener or route. Mount the returned text only behind an authenticated management endpoint or controlled network. Scraping copies bounded counters and at most 64 error entries and never reads database rows. Use queue gauges, operation latency, error overflow, and receipt capacity together for capacity decisions.
