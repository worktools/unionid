# v0.2 端到端发布验收

## 核心正确性场景

`tests/release_scenarios.rs` 从全新临时目录启动真实 `unionid` 子进程，验证三类适合 unionid 的应用状态。每条链路都执行初始 migration、类型化写入与查询、进程退出后的重开、第二版 migration、完整性检查、逻辑备份和还原。

| 场景 | 数据与操作 | Schema 演进 | 还原后验证 |
| --- | --- | --- | --- |
| 任务队列 | 嵌套任务状态、按主键条件 claim、upsert | `Running` 改名为 `Claimed`，增加默认 priority 与索引 | 类型化状态、主键行数、priority 索引计划 |
| 嵌套配置 | `Source` sum 包含命名 `Retry` record，更新远端配置 | 先给 `Retry` 增加深层默认字段，再把 `Remote` 变为带 headers 的 `Http` | 嵌套命名身份、默认值、变体 payload 与重建后的 source 索引 |
| Session/cache | insert、同 key upsert、delete 和重启 | `Active` 改名为 `Ready`，增加 generation 与索引 | key 生命周期、替换后的字段、generation 索引计划 |

每次 CLI 调用都在独立进程中打开并关闭 redb 文件。测试在源库和还原库上再次运行 unionid 完整检查，并比较规范 schema、revision/hash、不可变 migration ledger、类型化查询结果以及结构化 explain 的 access kind、index 和 stage 顺序。测试只使用隔离临时数据库，不读取开发者已有数据。

运行核心场景：

```bash
cargo test --locked --test release_scenarios
```

嵌套配置场景同时形成一条 migration 回归：转换绑定只展开需要访问字段的最外层 record，内部 `Retry` 继续保留命名身份。因此 `old.retry` 可以赋给仍要求 `Retry` 的新 payload；匿名结构相同的 record 不会被当作该命名类型。

## 证据边界

这些场景验证应用进程边界上的查询、DML、schema/data migration、索引重建、ledger 和 backup/restore 一致性。提交前／后退出、真实文件增长失败及 open/check 成本由 #13/#14 的存储测试和[恢复成本记录](benchmarks/recovery-2026-09-07.md)提供。

它们不模拟物理断电、文件系统违反同步承诺或设备损坏，也不提供延迟承诺。当前 1 万／10 万行的 query、write、check 与 shadow migration 数据见 [M7 验收记录](benchmarks/m7-acceptance-2026-09-10.md)；v0.2 双平台候选与包内教程由 [#200](https://github.com/worktools/unionid/issues/200) 做最终验收。

## crates.io 与 GitHub Release 发布

正式版本只由 `.github/workflows/release.yml` 发布，不在开发者工作站运行 `cargo publish`。`release/contract.json` 独立冻结软件、最低 Rust、redb、version-report schema、storage/backup/codec、protocol 与 stream 版本；打包脚本要求它与 Cargo 和二进制一致，并将原文件收入压缩包。验证器再从包外可信副本对比包内 contract、`RELEASE.json` 和二进制报告。workflow 的手动非 tag 运行会在双平台原生包验证之外执行：

```bash
cargo publish --dry-run --locked --registry crates-io
```

所有 Release workflow 运行还会只检查仓库 Actions secret `CARGO_REGISTRY_TOKEN` 是否可见且非空，不打印或使用其内容。随后，`v*` tag 运行在 Ubuntu runner 执行 `cargo publish --locked --registry crates-io`；crate 发布成功后，才汇总 macOS/Linux artifacts 并创建 GitHub Release。tag 版本仍由原生包脚本校验为与 `Cargo.toml` 一致。缺少 token、crate dry-run 或实际发布失败都会阻止 GitHub Release job。

参考的 `command-pool` 流程使用 `katyo/publish-crates@v2`。该 action 当前声明 Node 20 runtime，因此 unionid 保留相同的 GitHub Actions + registry token 模型，但在固定 Rust 1.94.0 runner 中直接调用 Cargo，避免重新引入已在 #105 消除的 Node runtime 警告。

## English Description

`tests/release_scenarios.rs` launches real `unionid` child processes from fresh temporary directories for three representative application-state journeys: a conditional task claim and upsert, a deep named-ADT configuration migration, and a session key lifecycle with upsert and delete. Every journey applies an initial migration, performs typed writes and queries, reopens after process exit, applies a second migration, checks integrity, creates a logical backup, and restores it to a new database.

The source and restored databases are compared for canonical schema, revision/hash, immutable migration ledger, typed query results, and structured explain access paths. The nested configuration case also prevents migration bindings from erasing the identity of a named record contained inside an outer sum payload.

Run the suite with `cargo test --locked --test release_scenarios`. It covers application process boundaries and logical recovery, while physical power loss, broken filesystem synchronization, and hardware damage remain outside the test claim. The [M7 acceptance record](benchmarks/m7-acceptance-2026-09-10.md) retains current 10k/100k query, write, check, and shadow-migration evidence; [#200](https://github.com/worktools/unionid/issues/200) owns final dual-platform candidate and packaged-tutorial acceptance.

Official versions are published only by `.github/workflows/release.yml`; developers do not run `cargo publish` from a workstation. The independent `release/contract.json` freezes software, minimum Rust, redb, version-report schema, storage/backup/codec, protocol, and stream versions. Packaging requires it to match Cargo and the binary, includes the source contract in the archive, and verification compares that copy plus `RELEASE.json` and the binary report against the trusted contract outside the package. A manual non-tag run executes `cargo publish --dry-run --locked --registry crates-io` alongside the native artifact checks, then safely checks only that `CARGO_REGISTRY_TOKEN` is visible and non-empty without printing or using its contents. On a `v*` tag, the Ubuntu runner publishes with that secret; only after publication succeeds does the workflow assemble the macOS/Linux assets and create the GitHub Release. Missing credentials or any crate validation/publication failure blocks the GitHub Release job. The GitHub Release body is selected from `docs/RELEASE-${tag}.md`, so a tag cannot silently reuse notes from an older version.

The reference `command-pool` workflow uses `katyo/publish-crates@v2`. Because that action currently declares a Node 20 runtime, unionid preserves its GitHub Actions plus registry-token model but invokes Cargo directly on the pinned Rust 1.94.0 runner, avoiding the runtime warning removed in #105.
