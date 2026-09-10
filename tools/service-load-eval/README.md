# 混合服务负载评测 / Mixed service-load evaluation

## 中文说明

该独立工具使用相同的 protocol-v2 请求生成器与校验器，比较 embedded `ConcurrentEngine`、真实 TCP server 和最小 HTTP adapter。每个 adapter/client/read-ratio 配置都在独立 release 子进程中从空 format-6 数据库开始；数据准备、预热和最终完整性检查不计入请求延迟。

完整矩阵包含 1、4、8 clients 与 90/10、50/50 read/write mix，覆盖主键读取、稳定 page、条件 update 和两行原子批次：

```bash
cargo run --release --locked --manifest-path tools/service-load-eval/Cargo.toml -- \
  /tmp/unionid-service-load-10k 10000 100
```

CI 只运行三种 adapter 的小型结构 smoke：

```bash
cargo run --release --locked --manifest-path tools/service-load-eval/Cargo.toml -- \
  /tmp/unionid-service-load-smoke 100 12 --smoke
```

JSON 保留排序后的全部请求延迟、nearest-rank p50/p95/p99、吞吐、请求类型计数、观察到的 sequence 范围、结构化错误计数、read/write queue aggregate、peak RSS、数据库字节数与 schema identity。每个响应通过版本、schema、sequence 及对应 row/action shape 校验后才进入样本。结果只描述指定机器、构建和 workload，不是 SLA，也不设置共享 CI runner 的绝对速度门槛。

## English Description

This standalone tool compares the embedded `ConcurrentEngine`, the real TCP server, and a minimal HTTP adapter through one protocol-v2 request generator and validator. Every adapter/client/read-ratio configuration starts from an empty format-6 database in a separate release child process. Preparation, warm-up, and final integrity checking are excluded from request latency.

The full matrix uses 1, 4, and 8 clients with 90/10 and 50/50 read/write mixes across point reads, stable pages, conditional updates, and atomic two-row batches. CI runs only the structural smoke shown above.

JSON retains every sorted request-latency sample, nearest-rank p50/p95/p99, throughput, operation counts, observed sequence range, classified errors, read/write queue aggregates, peak RSS, database bytes, and schema identity. A response is accepted only after validating its protocol version, schema, sequence, and operation-specific row/action shape. Results characterize one machine, build, and workload; they are not an SLA and establish no absolute threshold on shared CI runners.
