# 混合服务负载证据 / Mixed service-load evidence

当前 runner、工作负载边界和运行命令见 [`tools/service-load-eval/README.md`](../../tools/service-load-eval/README.md)。PR CI 只证明 JSON 结构、三种 adapter、TCP/HTTP 正常与慢 stream、显式取消及 writer 隔离路径保持可执行；10k/100k 运行应在稳定机器上手工或定时执行，并把 stdout 原样保存到 `docs/benchmarks/data/`，同时记录 commit 与机器环境。

The runner, workload boundary, and commands are documented in [`tools/service-load-eval/README.md`](../../tools/service-load-eval/README.md). PR CI proves only that the JSON structure, all three adapters, normal and slow TCP/HTTP streams, explicit cancellation, and writer-isolation paths remain executable. Run the 10k/100k matrix manually or on a scheduled stable machine, retain stdout unchanged under `docs/benchmarks/data/`, and record the commit and host environment.
