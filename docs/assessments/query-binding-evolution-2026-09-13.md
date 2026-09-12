# 静态查询绑定演进验收 / Static query binding evolution acceptance

- 日期 / Date: 2026-09-13
- 跟踪 / Tracking: [#264](https://github.com/worktools/unionid/issues/264)
- 契约 / Contract: [RFC 0015](../rfc/0015-static-query-contract.md)

## 中文说明

### 场景

v1 声明 `State = Pending | Running {attempt int}` 和包含 `id/title/state` 的 `Task`。`classify_task_v1.uid` exhaustive match 两个 variant，并投影 `{id, status}`。随后 live redb 通过 migration 增加 `State.Complete`，并为 `Task` 增加带默认值的 `priority int`；v2 查询补齐 `Complete` 分支并把 `priority` 加入 projection。

### 可执行结论

| 边界 | 预期与结果 |
| --- | --- |
| v1 schema + v1 生成代码 | 独立 crate 编译，写入 `Running` 后返回原生 typed row，成功 |
| v2 catalog + v1 查询源码重新生成 | binder 在生成阶段返回 non-exhaustive match，不产生代码 |
| v2 catalog + v2 查询源码 | 从 live redb 私有副本保留 revision/hash 和稳定 ID；独立 crate 先读取 migration 补默认值的旧行 `priority = 0`，再读取新 `Complete` 行的 `priority = 3` |
| v1 已生成函数 + v2 Engine | prepare 前返回 `E_SCHEMA_CHANGED` |
| v1 → v2 projection | query digest 与生成文件 SHA-256 同时变化，CI drift gate 失败，要求显式 review |

这验证了 variant、字段和 projection 三种常见演进。首版选择精确 schema identity：即使 additive change 不影响某个查询，旧生成函数仍要求重新生成。代价是升级时必须同步 bindings；收益是编译产物不会悄悄连接未经验收的 catalog。未来若需要放宽，应基于 #265 的 query/client-read/client-write compatibility report 增加显式兼容模式，不能只比较类型名。

自动入口为 `python3 scripts/verify-query-bindings.py`。脚本生成四个绑定、拒绝 stale match、检查固定摘要，并从独立临时 crate 执行 insert returning、bounded read、v1/v2 ADT match 与 schema drift。普通 CI 只在 Ubuntu 运行并复用已有 target，不启动 macOS 或 release evaluator。

## English Description

### Scenario

Version 1 declares `State = Pending | Running {attempt int}` and a `Task` with `id/title/state`. `classify_task_v1.uid` exhaustively matches both variants and projects `{id, status}`. A live redb catalog then migrates by adding `State.Complete` and a defaulted `priority int` field. The v2 query adds the `Complete` branch and includes `priority` in its projection.

### Executable findings

| Boundary | Expected and observed result |
| --- | --- |
| v1 schema + v1 generated code | An independent crate compiles and returns a native typed row after inserting `Running` |
| v2 catalog + regenerated v1 query source | The binder reports a non-exhaustive match during generation and emits no code |
| v2 catalog + v2 query source | Generation from a private live-redb copy retains revision/hash and stable IDs; the independent crate first reads a migrated v1 row with defaulted `priority = 0`, then a new `Complete` row with `priority = 3` |
| previously generated v1 call + v2 Engine | The call returns `E_SCHEMA_CHANGED` before prepare |
| v1 → v2 projection | Both the query digest and generated-file SHA-256 change; the CI drift gate requires explicit review |

This covers variant, field, and projection evolution. Version 1 intentionally pins the exact schema identity. An additive change therefore requires regeneration even when a particular query is unaffected. The cost is coordinated binding updates; the benefit is that compiled code never silently connects to an unaccepted catalog. Any future relaxation should consume #265 query/client-read/client-write compatibility reports through an explicit compatibility mode rather than comparing names alone.

`python3 scripts/verify-query-bindings.py` is the automated entry point. It generates four bindings, rejects the stale match, checks pinned digests, and runs insert-returning, bounded reads, v1/v2 ADT matches, and schema drift from an independent temporary crate. Normal CI runs this only on Ubuntu and reuses the existing target; it does not start macOS or release evaluators.
