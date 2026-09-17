# v0.9 内联查询宏编译记录

## 中文说明

`scripts/evaluate-query-macro.py` 从空临时目录创建两个独立 Rust consumer。baseline 依赖 `unionid` runtime 与 serde；macro consumer 再依赖 `unionid-query`，声明两个查询，其中一个包含 enum record payload match。两个 consumer 使用独立 target，离线生成 lockfile，并以 Rust 1.94 执行 cold `cargo check`。macro consumer 随后执行 no-op check、修改内联 query 并运行、修改声明式 schema 并运行。

2026-09-18 的本地参考样本来自 macOS arm64、rustc 1.94.0。它用于判断结构调整，不是性能承诺：

| 项目 | 结果 |
| --- | ---: |
| runtime baseline cold check | 16,064 ms |
| inline macro consumer cold check | 21,712 ms |
| 宏路径 cold 增量 | 5,648 ms |
| no-op check | 169 ms |
| 修改 query 后 build + run | 860 ms |
| 修改 schema 后 build + run | 774 ms |
| baseline target 文件 | 172,508,618 bytes |
| macro consumer target 文件 | 561,017,578 bytes |
| 宏路径 target 增量 | 388,508,960 bytes |
| proc-macro 动态库 | 9,399,064 bytes |

脚本同时验证 query 修改会改变 canonical source 与 digest，但不改变 schema hash；schema 修改会触发 `include_str!` 依赖重建并改变 schema hash，但不改变等价 query 的 digest；整个编译过程不会创建或打开 redb 文件。release workflow 只在 Linux runner 保存同结构的 version 1 JSON，普通 PR 继续只运行 Ubuntu 快速检查，macOS 只在发布候选运行一次。

冷 target 增量较大，因为 proc-macro host 需要再次编译当前完整 `unionid` crate；但普通应用本来就需要 runtime，实际新增冷时间在该样本约 5.6 秒，no-op 和局部重建低于 1 秒。v0.9 不拆分 compiler-only crate：拆分会立即扩大 parser、schema、binder、formatter 与 codegen 的内部 API 边界，而现有增量开发成本可接受。若后续真实应用或 CI 样本持续显示冷构建不可接受，再以同一脚本比较拆分方案，不能复制 parser/binder 来换取构建速度。

宏只接受仓库中的声明式 schema 文件。migration 建立的 stable ID 属于 live catalog，v0.9 继续要求使用 `unionid query rust --db ...` 生成绑定；宏不会在编译期打开 redb。可提交 portable catalog artifact 暂不加入 v0.9，因为它需要先冻结独立版本、稳定 ID 来源和 migration 更新流程，不能把当前 backup 或内部 catalog codec 当成公开编译接口。

Rust 编译错误标记对应的完整 `query name { ... }` block，并保留 core binder 返回的 query 相对 span。部分 stage 级错误仍只指向语句起点；精确到每个 token 需要 core parser/binder 提供更细 span，v0.9 不通过解析错误字符串或复制 parser 来伪造精度。

## English Description

`scripts/evaluate-query-macro.py` creates two independent Rust consumers in a fresh temporary directory. The baseline depends on the `unionid` runtime and serde. The macro consumer additionally depends on `unionid-query` and declares two queries, including a record-payload enum match. Each uses a separate target and an offline lockfile, then performs a cold `cargo check` with Rust 1.94. The macro consumer also performs a no-op check, a query-edit build and run, and a declarative-schema-edit build and run.

The 2026-09-18 local reference above was collected on macOS arm64 with rustc 1.94.0. It informs the structural decision and is not a performance promise. The evaluator verifies that a query edit changes canonical source and digest without changing schema identity, while a schema edit triggers the `include_str!` dependency, changes schema identity, and preserves the equivalent query digest. Compilation never creates or opens a redb database. The release workflow records the same version-1 JSON on Linux only; ordinary pull requests retain the Ubuntu fast path and macOS runs once for a release candidate.

The cold target delta is substantial because the proc-macro host compiles the current complete `unionid` crate again. The measured incremental cold time over an application that already needs the runtime is about 5.6 seconds, while no-op and local rebuilds remain below one second. v0.9 therefore does not split a compiler-only crate: doing so would immediately widen internal parser, schema, binder, formatter, and codegen APIs, while incremental development remains acceptable. A future split must be justified against this evaluator using real application or CI evidence and must not duplicate parser/binder logic.

The macro accepts a committed declarative schema only. Stable IDs established by migrations belong to a live catalog, so v0.9 continues to use `unionid query rust --db ...` for that case and never opens redb during compilation. A portable catalog artifact is deferred until its own version, stable-ID provenance, and migration update workflow can be frozen; backup and internal catalog codecs are not public compiler inputs.

Rust diagnostics underline the complete failing `query name { ... }` block and retain query-relative spans supplied by the core binder. Some stage-level errors still point only at the statement start. Token-level precision requires finer spans from the core parser/binder; v0.9 does not parse error prose or duplicate the parser to manufacture it.
