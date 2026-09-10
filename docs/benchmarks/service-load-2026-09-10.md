# M9 混合服务负载证据 / M9 mixed service-load evidence

## 中文说明

### 范围与环境

本次运行验证 commit `ef79978` 上的完整 10k/100k 矩阵。主机为 macOS ARM64、8 logical CPUs，工具链为 `rustc 1.94.0 (4a4ef493e 2026-03-02)`。每个规模包含 embedded、TCP、HTTP 三种 adapter，1/4/8 clients，90/10 与 50/50 read/write mix，每个 client 100 个计时请求。数据准备、预热和最终完整性检查不计入请求延迟。

原始结果：

- [`service-load-m9-2026-09-10-10k.json`](data/service-load-m9-2026-09-10-10k.json)
- [`service-load-m9-2026-09-10-100k.json`](data/service-load-m9-2026-09-10-100k.json)

结果只描述这台机器、这个 commit 和固定工作负载，不是 SLA。全部 36 个普通 case 都通过 response、schema、sequence 与 operation shape 校验，结构化错误为空。

### 代表性结果

下表保留 8 clients 时的吞吐与 p99；完整的 1/4/8 clients、p50/p95/p99、排队时间及逐请求样本见原始 JSON。

| 行数 | Adapter | 读比例 | 吞吐（请求/秒） | p99（ms） |
| ---: | --- | ---: | ---: | ---: |
| 10k | embedded | 90% | 1177.92 | 30.962 |
| 10k | embedded | 50% | 202.57 | 149.965 |
| 10k | TCP | 90% | 796.15 | 30.543 |
| 10k | TCP | 50% | 183.03 | 112.252 |
| 10k | HTTP | 90% | 556.59 | 69.131 |
| 10k | HTTP | 50% | 209.96 | 76.054 |
| 100k | embedded | 90% | 1008.27 | 62.430 |
| 100k | embedded | 50% | 194.75 | 138.074 |
| 100k | TCP | 90% | 660.78 | 42.230 |
| 100k | TCP | 50% | 182.53 | 97.175 |
| 100k | HTTP | 90% | 568.51 | 66.097 |
| 100k | HTTP | 50% | 180.82 | 88.454 |

固定工作负载中的 point read 和 stable page 都使用索引路径。90% 读负载从 10k 增至 100k 后仍保持同一量级；当前舒适使用范围内，数据规模没有带来数量级退化。50/50 负载由串行写入主导：更多 client 主要增加写排队和尾延迟，不会带来线性吞吐提升。应用应把并发用于独立读取，并让写批次保持小而有界。

10k case 的数据库文件均为 11,206,656 bytes（约 10.7 MiB），peak RSS 为 27,951,104–33,095,680 bytes（约 26.7–31.6 MiB）。100k case 的数据库文件均为 89,657,344 bytes（约 85.5 MiB），peak RSS 为 128,778,240–133,169,152 bytes（约 122.8–127.0 MiB）。这些数字包含各 release 子进程的整体常驻内存，仅用于容量判断。

### Stream、取消与 writer 隔离

| 行数 | Adapter | 消费者 | terminal | 接受耗时（ms） | 终止耗时（ms） | writer（ms） | 收到行数 |
| ---: | --- | --- | --- | ---: | ---: | ---: | ---: |
| 10k | TCP | normal | complete | 110.658 | 142.059 | — | 128 |
| 10k | TCP | slow | E_CANCELLED | 55.723 | 65.778 | 9.895 | 0 |
| 10k | HTTP | normal | complete | 5.367 | 22.968 | — | 128 |
| 10k | HTTP | slow | E_CANCELLED | 7.217 | 24.819 | 10.533 | 2 |
| 100k | TCP | normal | complete | 110.622 | 138.690 | — | 128 |
| 100k | TCP | slow | E_CANCELLED | 61.545 | 71.232 | 9.574 | 0 |
| 100k | HTTP | normal | complete | 7.922 | 28.880 | — | 128 |
| 100k | HTTP | slow | E_CANCELLED | 7.894 | 20.568 | 12.469 | 0 |

四个 slow case 都在观察到 emitting 后启动 writer，通过真实 control connection 得到 `cancel_status = accepted`，最终只产生一个 `E_CANCELLED` terminal。timeout/rejection 均为 0，writer 在读取 terminal 前提交，结束后 registered operations 为 0。TCP accepted 时间包含 server 的 100 ms connection poll，不能解释为查询执行耗时。

### 使用结论

- 约 10k 行仍是更舒适的日常范围；100k 行在当前索引化负载下可用，但文件和 RSS 都明显增长，应作为经过验证的容量上限来规划，而不是默认目标。
- 读多负载能从多个 client 获得更高 adapter 吞吐；写比例升高后，应用更应关注 p99 与 write queue，而不是继续增加并发。
- 慢 stream 不会阻止独立 writer，显式取消可以稳定清理 operation；客户端仍应主动消费 frame、设置 deadline，并把大结果限制在协议预算内。

## English Description

### Scope and environment

This run covers the complete 10k and 100k matrices at commit `ef79978`. The host was macOS ARM64 with 8 logical CPUs and `rustc 1.94.0 (4a4ef493e 2026-03-02)`. Each size covers embedded, TCP, and HTTP adapters; 1, 4, and 8 clients; 90/10 and 50/50 read/write mixes; and 100 timed requests per client. Preparation, warmup, and final integrity checking are excluded from request latency.

Raw results:

- [`service-load-m9-2026-09-10-10k.json`](data/service-load-m9-2026-09-10-10k.json)
- [`service-load-m9-2026-09-10-100k.json`](data/service-load-m9-2026-09-10-100k.json)

The results characterize this host, commit, and fixed workload and are not an SLA. All 36 ordinary cases passed response, schema, sequence, and operation-shape validation with no classified errors.

### Representative results

The table retains throughput and p99 at 8 clients. The raw JSON contains the full 1/4/8-client matrix, p50/p95/p99, queue observations, and every accepted request sample.

| Rows | Adapter | Reads | Throughput (req/s) | p99 (ms) |
| ---: | --- | ---: | ---: | ---: |
| 10k | embedded | 90% | 1177.92 | 30.962 |
| 10k | embedded | 50% | 202.57 | 149.965 |
| 10k | TCP | 90% | 796.15 | 30.543 |
| 10k | TCP | 50% | 183.03 | 112.252 |
| 10k | HTTP | 90% | 556.59 | 69.131 |
| 10k | HTTP | 50% | 209.96 | 76.054 |
| 100k | embedded | 90% | 1008.27 | 62.430 |
| 100k | embedded | 50% | 194.75 | 138.074 |
| 100k | TCP | 90% | 660.78 | 42.230 |
| 100k | TCP | 50% | 182.53 | 97.175 |
| 100k | HTTP | 90% | 568.51 | 66.097 |
| 100k | HTTP | 50% | 180.82 | 88.454 |

Point reads and stable pages use indexed paths in this fixed workload. The 90%-read cases remain in the same performance range from 10k to 100k rows, with no order-of-magnitude degradation inside the tested range. Serial writes dominate the 50/50 cases: more clients mainly add write queueing and tail latency rather than linear throughput. Applications should use concurrency for independent reads and keep write batches small and bounded.

Database files were 11,206,656 bytes (about 10.7 MiB) for every 10k case, with peak RSS of 27,951,104–33,095,680 bytes (about 26.7–31.6 MiB). Files were 89,657,344 bytes (about 85.5 MiB) for every 100k case, with peak RSS of 128,778,240–133,169,152 bytes (about 122.8–127.0 MiB). RSS covers the complete release child process and is capacity evidence only.

### Streaming, cancellation, and writer isolation

| Rows | Adapter | Consumer | Terminal | Accepted (ms) | Terminal (ms) | Writer (ms) | Rows received |
| ---: | --- | --- | --- | ---: | ---: | ---: | ---: |
| 10k | TCP | normal | complete | 110.658 | 142.059 | — | 128 |
| 10k | TCP | slow | E_CANCELLED | 55.723 | 65.778 | 9.895 | 0 |
| 10k | HTTP | normal | complete | 5.367 | 22.968 | — | 128 |
| 10k | HTTP | slow | E_CANCELLED | 7.217 | 24.819 | 10.533 | 2 |
| 100k | TCP | normal | complete | 110.622 | 138.690 | — | 128 |
| 100k | TCP | slow | E_CANCELLED | 61.545 | 71.232 | 9.574 | 0 |
| 100k | HTTP | normal | complete | 7.922 | 28.880 | — | 128 |
| 100k | HTTP | slow | E_CANCELLED | 7.894 | 20.568 | 12.469 | 0 |

All four slow cases started a writer after observing emission, received `cancel_status = accepted` through the real control connection, and ended with one `E_CANCELLED` terminal. There were no timeout or rejection outcomes, each writer committed before terminal consumption, and registered operations returned to zero. TCP accepted timing includes the server's 100 ms connection poll and should not be interpreted as query execution time.

### User guidance

- Around 10k rows remains the more comfortable everyday range. The current indexed workload remains usable at 100k rows, but file size and RSS grow materially, so 100k should be treated as a tested planning bound rather than the default target.
- Read-heavy workloads benefit from multiple clients. As the write ratio increases, applications should watch p99 and the write queue instead of adding more concurrency.
- A slow stream does not block an independent writer, and explicit cancellation reliably cleans up the operation. Clients should still consume frames promptly, set deadlines, and keep large results within protocol budgets.
