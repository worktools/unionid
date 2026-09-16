# unionid v0.6.0 发布说明

v0.6.0 把首次使用收敛为一条从已安装二进制和空目录开始的项目工作流。新用户不需要 clone unionid 仓库或查找内部示例，即可生成规范的 ADT schema、migration、seed 和 typed query，先静态检查项目契约，再创建和验证持久数据库。本版本不增加查询、类型、存储、备份或网络协议能力。

## 用户可见变化

- `unionid init <目录>` 生成最小任务项目：`schema.unid`、`migrations/0001_initial.unid`、`seed.unid`、`queries/list_running.unid`、本地 `data/` 与双语 README。它只接受不存在或空目录，写入前验证全部内置源码，不覆盖用户文件，也不隐式创建数据库。
- `unionid project check --dir <目录>` 按 schema → migrations → queries 的固定顺序检查规范格式、migration 最终 schema 和 query binding。它不执行 seed、不打开数据库，并为自动化提供 version 1 JSON、相对路径、稳定错误码和源码 span。
- README、CLI 与五分钟入门现在共用 `install → init → project check → migration apply → seed/query → reopen → doctor/check → backup/restore` 路径。发布包从空目录执行同一 validator，并比较源库和恢复库的 typed rows 与 schema identity。
- `.unid` 是 schema、query 和 migration 的规范后缀。兼容期内仍接受 `.uid` 并输出弃用提示；migration checksum 不包含路径，因此重命名不会重新应用 migration。兼容窗口计划在 v1.0.0 关闭。

## 从空目录开始

需要 Rust 1.94 或更高版本：

```bash
cargo install unionid --version 0.6.0 --locked
mkdir unionid-first-use
cd unionid-first-use
unionid init tasks
cd tasks
unionid project check --dir .
unionid migration apply --db data/tasks.redb --dir migrations
unionid run --db data/tasks.redb --file seed.unid
unionid run --db data/tasks.redb --file queries/list_running.unid
unionid doctor --db data/tasks.redb
unionid check --db data/tasks.redb
unionid backup --db data/tasks.redb --output data/tasks.backup.json --format json
unionid restore --backup data/tasks.backup.json --db data/restored.redb --format json
unionid run --db data/restored.redb --file queries/list_running.unid
unionid check --db data/restored.redb
```

## 兼容与边界

v0.6.0 只改变软件版本和首次接入体验。最低 Rust 仍为 1.94，redb 仍固定为 4.1.0；新数据库仍创建为 storage format 6，默认 catalog/value/index/migration/receipt/maintenance/journal codec 仍为 `4/2/3/1/2/1/0`。二进制继续读取 storage format 1–7；只有显式启用增量备份会进入 format 7 和 journal codec 1。

logical backup 当前格式仍为 4、可读 1–4；JSON Lines protocol 仍为 1/2，stream protocol 仍为 1。v0.5.0 数据库、logical backup、协议客户端和生成查询的语义保持兼容；静态 Rust 生成物会记录生成器版本，升级 binary 后应重新生成并编译。升级前仍应保留已校验的 logical backup，并先在数据库副本运行 `doctor`、`check` 和应用读写。

产品边界保持为单机、单数据库所有者、串行写入和约 10,000 行舒适工作集。100,000 行只是已测试上限。通用扁平 join、window、多写者、复制与分布式执行不在本版本范围内。

## English Description

v0.6.0 converges first use on a project workflow that starts with an installed binary and an empty directory. A new user can generate a canonical ADT schema, migration, seed, and typed query without cloning the unionid repository or locating internal examples, validate the project contract before creating data, and then exercise persistent reopen, diagnosis, and recovery. This release adds no query, type, storage, backup, or network-protocol capability.

### User-visible changes

- `unionid init <directory>` generates a minimal task project with `schema.unid`, `migrations/0001_initial.unid`, `seed.unid`, `queries/list_running.unid`, local `data/`, and a bilingual README. It accepts only a missing or empty directory, validates every built-in source before writing, never overwrites user files, and creates no database implicitly.
- `unionid project check --dir <directory>` validates canonical formatting, the migration target schema, and query binding in fixed schema → migrations → queries order. It runs no seed and opens no database. Automation receives version-1 JSON with relative paths, stable error codes, and source spans.
- README, CLI, and the five-minute guide now share one `install → init → project check → migration apply → seed/query → reopen → doctor/check → backup/restore` journey. Native-package verification runs the same validator from an empty directory and compares typed rows and schema identity between source and restored databases.
- `.unid` is the canonical schema, query, and migration suffix. `.uid` remains accepted with a deprecation warning during the compatibility window. Migration checksums exclude paths, so renaming does not reapply a migration. The window is planned to close in v1.0.0.

### Start from an empty directory

Rust 1.94 or newer is required:

```bash
cargo install unionid --version 0.6.0 --locked
mkdir unionid-first-use
cd unionid-first-use
unionid init tasks
cd tasks
unionid project check --dir .
unionid migration apply --db data/tasks.redb --dir migrations
unionid run --db data/tasks.redb --file seed.unid
unionid run --db data/tasks.redb --file queries/list_running.unid
unionid doctor --db data/tasks.redb
unionid check --db data/tasks.redb
unionid backup --db data/tasks.redb --output data/tasks.backup.json --format json
unionid restore --backup data/tasks.backup.json --db data/restored.redb --format json
unionid run --db data/restored.redb --file queries/list_running.unid
unionid check --db data/restored.redb
```

### Compatibility and limits

v0.6.0 changes only the software version and first-use experience. The minimum Rust remains 1.94 and redb remains pinned to 4.1.0. Fresh databases still use storage format 6 and default catalog/value/index/migration/receipt/maintenance/journal codecs `4/2/3/1/2/1/0`. The binary still reads storage formats 1–7; only explicit incremental-backup enablement enters format 7 with journal codec 1.

The current logical backup format remains 4 with formats 1–4 readable. JSON Lines protocols remain 1/2 and the stream protocol remains 1. v0.5.0 databases, logical backups, protocol clients, and generated-query semantics remain compatible. Static Rust artifacts record their generator version, so regenerate and rebuild them after upgrading the binary. Retain a verified logical backup before upgrading and rehearse `doctor`, `check`, and application reads/writes against a database copy first.

The product boundary remains one machine, one database owner, serialized writes, and a comfortable working set around 10,000 rows. A 100,000-row workload is only a tested ceiling. General flattened joins, windows, multiple writers, replication, and distributed execution remain outside this release.
