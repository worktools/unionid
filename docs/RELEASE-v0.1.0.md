# unionid v0.1.0 发布说明

v0.1.0 是第一个面向日常本地应用状态的可用版本。它提供无分号的 PRQL 风格语言、原生命名和类型与积类型、嵌套 option/list、穷尽 pattern matching、类型化 pipeline、原子 CRUD/upsert、redb 持久化、显式 schema migration、备份还原、本地 CLI、Rust Engine 和有界 TCP 服务。

## 获取与验证

GitHub Release 为 macOS 和 Linux 的 CI 原生 Rust target 生成 `unionid-v0.1.0-<target>.tar.gz`。目标三元组同时写入文件名和包内 `RELEASE.json`，因此使用者可以明确选择与机器匹配的产物。每个压缩包旁有独立 `.sha256`：

```bash
sha256sum -c unionid-v0.1.0-<target>.tar.gz.sha256
tar -xzf unionid-v0.1.0-<target>.tar.gz
unionid-v0.1.0-<target>/bin/unionid --version
```

macOS 可用 `shasum -a 256 -c` 校验。压缩包包含五分钟教程和验证器，见 [GETTING_STARTED.md](GETTING_STARTED.md)。

## 容量证据与边界

- 1 万行是当前较舒适的操作范围。实测 write p95 约 72–94 ms，深层 ADT migration 约 344 ms。
- 10 万行是 v0.1 已测试上限，不是建议工作集。实测 write p95 约 0.51–0.88 s，migration 约 4.15 s，写入和迁移 peak RSS 约 1.1–1.29 GiB。
- 10 万行数据库的 open/check 中位数约 653/741 ms；完整检查 peak RSS 约 745.84 MiB。
- 服务面向受信本机应用，没有认证或 TLS。当前 Engine 串行进入查询、写入和 migration；活动连接最多 64，源码 1 MiB，working rows 250,000，结果 100,000 行，响应 16 MiB，服务执行 deadline 25 秒。
- v0.1 不提供 join、window、递归查询、分布式执行或多写者事务。

测量环境、20 个原始样本和复现命令见[工作负载记录](benchmarks/workload-2026-09-07.md)与[恢复记录](benchmarks/recovery-2026-09-07.md)。这些数字描述已测试行为，不构成延迟或容量承诺。

## 格式兼容

v0.1.0 固定使用 redb 4.1.0，并使用 version 1 storage、catalog、ADT value、index key、migration ledger、logical backup 与 JSON Lines protocol 格式。未知版本会在写入前拒绝打开；未来内部格式变化需要 release notes 明确说明和显式转换。应用 schema 通过不可变、有 checksum 的 migration 历史演进。完整操作流程见[升级与格式兼容](UPGRADING.md)。

## English release notes

v0.1.0 is the first daily-usable release for local application state. It combines semicolon-free PRQL-style declarations and pipelines with named sum/product types, nested option/list values, exhaustive matching, typed queries, atomic CRUD/upsert, redb durability, explicit schema migrations, verified backup/restore, a local CLI, an embeddable Rust Engine, and a bounded TCP service.

GitHub Releases contain native macOS and Linux artifacts named with their exact Rust target, an adjacent SHA-256 file, `RELEASE.json`, and an executable tutorial. A 10k-row database is the current comfortable range; 100k rows is a tested ceiling with materially higher write, migration, and memory costs. The service is for trusted local use and has no authentication or TLS. Internal version-1 codecs fail closed on unknown versions, and future format conversion must be explicit.
