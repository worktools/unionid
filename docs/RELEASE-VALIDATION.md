# v0.7 端到端发布验收

## 核心正确性场景

`tests/release_scenarios.rs` 从全新临时目录启动真实 `unionid` 子进程，验证三类适合 unionid 的应用状态。每条链路都执行初始 migration、类型化写入与查询、进程退出后的重开、第二版 migration、完整性检查、逻辑备份和还原。

| 场景 | 数据与操作 | Schema 演进 | 还原后验证 |
| --- | --- | --- | --- |
| 任务队列 | 嵌套任务状态、按主键条件 claim、upsert | `Running` 改名为 `Claimed`，增加 `Cancelled`，把 title 转为 option，增加默认 priority 与索引 | 类型化状态与 title、主键行数、priority 索引计划 |
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

它们不模拟物理断电、文件系统违反同步承诺或设备损坏，也不提供延迟承诺。当前 1 万／10 万行的 query、write、check 与 shadow migration 数据见 [M7 验收记录](benchmarks/m7-acceptance-2026-09-10.md)；v0.7 双平台候选、v0.6 数据／backup／客户端兼容、原生包和空目录 starter 由 [#343](https://github.com/worktools/unionid/issues/343) 做最终验收。Rust 形状源码、range 迁移、membership、相关 exists 与集合运算另由语言和对应 feature tests 覆盖。

## crates.io 与 GitHub Release 发布

正式版本只由 `.github/workflows/release.yml` 发布，不在开发者工作站运行 `cargo publish`。`release/contract.json` 独立冻结软件、最低 Rust、redb、version-report schema、storage/backup/codec、protocol 与 stream 版本；打包脚本要求它与 Cargo 和二进制一致，并将原文件收入压缩包。`RELEASE.json` 同时记录完整 source commit 和 clean-worktree 状态；脏树只有显式 `--allow-dirty` 才能生成开发包，正式验证器始终拒绝。验证器再从包外可信副本对比包内 contract、`RELEASE.json` 和二进制报告。workflow 的手动非 tag 运行会在双平台原生包验证之外执行：

```bash
cargo publish --dry-run --locked --registry crates-io
```

手动非 tag 候选运行不读取或要求发布凭据。只有 `v*` tag 运行才检查仓库 Actions secret `CARGO_REGISTRY_TOKEN` 是否可见且非空，然后在 Ubuntu runner 依次发布 `unionid-derive` 与 `unionid`；crate 发布成功后，才汇总 macOS/Linux artifacts 并创建 GitHub Release。tag 版本仍由原生包脚本校验为与 `Cargo.toml` 一致。缺少 token、crate dry-run 或实际发布失败都会阻止 GitHub Release job。

参考的 `command-pool` 流程使用 `katyo/publish-crates@v2`。该 action 当前声明 Node 20 runtime，因此 unionid 保留相同的 GitHub Actions + registry token 模型，但在固定 Rust 1.94.0 runner 中直接调用 Cargo，避免重新引入已在 #105 消除的 Node runtime 警告。

## v0.7 候选机器证据

CI 矩阵固定核对 Linux `x86_64-unknown-linux-gnu` 与 macOS `aarch64-apple-darwin`。每个 job 只有在 formatter、locked check、严格 Clippy、完整测试、Engine/CLI/TCP 教程、HTTP/stream 示例、评测 smoke、release build 和包外 SHA-256 验证全部成功后，才运行 `scripts/record-release-acceptance.py`。

候选 job 还会按当前平台下载 GitHub 已发布的 v0.6.0 原生包并核对其公开 `.sha256`。`scripts/verify-previous-release-compatibility.py` 用 v0.6 binary 分别创建普通 format-6 数据库和显式启用 incremental journal 的 format-7 数据库，再要求 v0.7 对两者都保持 capability contract、typed rows 与 schema identity，恢复对应 logical backup，并接受 v0.6 CLI 通过 version 1 TCP 查询 v0.7 read-only server。若旧 format-7 文件首次实际打开需要 redb repair，第一次完整 check 必须成功，紧接的第二次 check 必须报告 `backend_clean=true`。这条测试只覆盖已冻结的直接兼容路径，不把跨版本 mutation 或 format 降级加入承诺。

`scripts/record-release-acceptance.py` 重新读取已验证候选压缩包中的 `RELEASE.json` 与 `release/contract.json`，要求 manifest 的 clean source commit 与实际 GitHub head SHA、预期 target 和源码 contract 一致，然后输出 version 1 JSON。报告保存 runner OS/arch/target、workflow URL、candidate commit、archive/manifest/contract SHA-256、完整 release manifest、通过状态和对应测试／文档证据。两个平台都从空目录运行生成式 starter；只有 Linux 报告把 Envoy mTLS 真实旅程记为通过。报告不保存数据库内容、secret 或不稳定的墙钟时间。macOS/Linux 报告分别以 GitHub Actions artifact 保留 90 天；可选的大型增量备份 evaluator 默认关闭，不是 v0.7 候选门槛。workflow URL 与摘要写入 #343。

## English Description

`tests/release_scenarios.rs` launches real `unionid` child processes from fresh temporary directories for three representative application-state journeys: a conditional task claim and upsert, a deep named-ADT configuration migration, and a session key lifecycle with upsert and delete. Every journey applies an initial migration, performs typed writes and queries, reopens after process exit, applies a second migration, checks integrity, creates a logical backup, and restores it to a new database.

The source and restored databases are compared for canonical schema, revision/hash, immutable migration ledger, typed query results, and structured explain access paths. The nested configuration case also prevents migration bindings from erasing the identity of a named record contained inside an outer sum payload.

Run the suite with `cargo test --locked --test release_scenarios`. It covers application process boundaries and logical recovery, while physical power loss, broken filesystem synchronization, and hardware damage remain outside the test claim. The [M7 acceptance record](benchmarks/m7-acceptance-2026-09-10.md) retains current 10k/100k query, write, check, and shadow-migration evidence; [#343](https://github.com/worktools/unionid/issues/343) owns the final v0.7 dual-platform candidate, v0.6 data/backup/client compatibility, native archives, and empty-directory starter acceptance. Language and feature tests separately cover Rust-shaped source and ranges, membership, correlated exists, and set operations.

Official versions are published only by `.github/workflows/release.yml`; developers do not run `cargo publish` from a workstation. The independent `release/contract.json` freezes software, minimum Rust, redb, version-report schema, storage/backup/codec, protocol, and stream versions. Packaging requires it to match Cargo and the binary, includes the source contract in the archive, and records the full source commit plus clean-worktree status in `RELEASE.json`. Dirty trees require an explicit `--allow-dirty` development override and are always rejected by the formal verifier. Verification compares the packaged contract, manifest, and binary report against the trusted contract outside the package. A manual non-tag run executes `cargo publish --dry-run --locked --registry crates-io` alongside the native artifact checks without requiring publication credentials. On a `v*` tag, the workflow verifies `CARGO_REGISTRY_TOKEN`, publishes `unionid-derive` before `unionid`, then assembles the macOS/Linux assets and creates the GitHub Release. Missing credentials or any crate validation/publication failure blocks the GitHub Release job. The GitHub Release body is selected from `docs/RELEASE-${tag}.md`, so a tag cannot silently reuse notes from an older version.

The reference `command-pool` workflow uses `katyo/publish-crates@v2`. Because that action currently declares a Node 20 runtime, unionid preserves its GitHub Actions plus registry-token model but invokes Cargo directly on the pinned Rust 1.94.0 runner, avoiding the runtime warning removed in #105.

For v0.7 candidate evidence, the CI matrix fixes the expected targets to Linux `x86_64-unknown-linux-gnu` and macOS `aarch64-apple-darwin`. A job runs `scripts/record-release-acceptance.py` only after formatting, locked checks, strict Clippy, the full suite, the packaged independent consumer, Engine/CLI/TCP tutorial, HTTP/stream example, evaluator smoke runs, release build, the generated empty-directory starter, and out-of-package SHA-256 verification all succeed. The optional 10k/100k incremental-backup evaluator stays disabled by default; the Linux job still runs the real Envoy container journey.

Each candidate job also downloads the published v0.6.0 native archive for its platform and verifies the public checksum. `scripts/verify-previous-release-compatibility.py` uses the v0.6 binary to create both an ordinary format-6 database and a format-7 database with the incremental journal explicitly enabled. It then requires v0.7 to preserve the capability contract, typed rows, and schema identity for both, restore each logical backup, and accept version-1 TCP queries from the v0.6 CLI against each v0.7 read-only server. If the first real open of the old format-7 file requires redb repair, the first full check must succeed and the immediately repeated check must report `backend_clean=true`. This proves the frozen direct paths without promising cross-version mutations or storage-format downgrade.

The recorder rereads `RELEASE.json` and `release/contract.json` from the verified archive. It requires the manifest's clean source commit to equal the GitHub head SHA, expected target, and trusted source contract, then writes a version-1 JSON report. The report retains runner OS/arch/target, workflow URL, candidate commit, archive/manifest/contract SHA-256 values, the full release manifest, explicit pass results, and test/document evidence. Both platforms record the empty-directory generated starter; the real Envoy mTLS journey appears as passed only in the Linux report. Reports contain no database values, secrets, or unstable wall-clock timestamp. GitHub retains separate macOS/Linux reports for 90 days, and #343 retains workflow links and summaries.
