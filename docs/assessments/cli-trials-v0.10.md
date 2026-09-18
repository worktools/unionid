# v0.10 CLI 试用记录 / CLI trial record

日期 / Date: 2026-09-19
版本 / Version: Unionid 0.9.1, current `main` after PR #368

## 中文说明

本记录是 [#363](https://github.com/worktools/unionid/issues/363) 的可复现验收证据。每项记录任务、输入与最小数据库状态、期望、实际结果和是否阻塞。所有命令只使用临时目录与新建数据库，不读取开发者已有数据。

### 场景一：从空目录完成任务项目生命周期

- **任务：** 新用户创建任务项目，验证源码，持久化数据，查询，诊断并恢复备份。
- **输入与最小状态：** 空目录；运行 `scripts/validate-first-use.py --binary target/debug/unionid --work-dir <empty-dir>`。脚本依次执行 `init`、project check、初始 migration、seed、typed query、重新打开、doctor、完整性检查、backup、restore 和恢复库查询。
- **期望：** 生成源码不被运行过程改写；两行任务持久化；原库和恢复库返回相同的 schema identity、typed columns 与 `Running` ADT row。
- **实际：** 通过。生成 6 个项目文件，保存 2 行，原库与恢复库各查询到 1 行，schema revision 为 1；doctor 与完整性检查均成功。
- **阻塞：** 否。没有接受新的修复，首次使用主链继续作为发布验收入口。

### 场景二：嵌套配置完成深层 ADT 演进和恢复

- **任务：** 配置服务保存嵌套 sum/product/list 数据，在重启后演进内部 record 与 enum payload，再备份恢复。
- **输入与最小状态：** 新数据库；`Config` 包含 `Source = Local | Remote {url, retry}`、嵌套 `Retry` 和 `list text`。运行 `cargo test --locked --test release_scenarios nested_config_survives_deep_adt_migration_and_restore`。
- **期望：** 初始 migration、insert/update、递归 match query 和重开均保留 typed value；升级可先 plan/rehearse，再将 `Remote` 改名为 `Http`、为 `Retry` 和 payload 增加字段；升级后索引计划、完整性检查、backup/restore、ledger、RowId 和 schema identity 保持一致。
- **实际：** 通过。深层默认值和 payload conversion 正确，`configs.source` 继续用于 secondary-index lookup，原库与恢复库逐行一致。
- **阻塞：** 否。没有接受新的修复，这条链路为 #363 提供第二个完整业务场景。

### 场景三：像 DuckDB 一样直接查看数据库文件

- **任务：** 不进入交互界面，查看已有 redb 的表、类型、schema 和存储状态。
- **输入与最小状态：** 一个包含 `Task` schema 和两行数据的 redb，以及一个带相同 schema 的内存 TCP 服务；分别验证本地 redb `--query .tables`、本地 redb `--file .types`、本地 redb stdin `.schema` 和 TCP `--query .storage`。
- **期望：** introspection 使用与 REPL 相同的人类输出；`--format json` 返回由 `kind` 和 typed `introspection` 构成的稳定、脱敏 envelope。
- **修复前：** typed row query 可用，但 `.tables` 被当作查询语言并返回 `E_SYNTAX`。这会阻塞 shell 和 LLM 工具的非交互 catalog 查看。
- **修复后：** PR #368 让四个命令可作为 `cli` 的唯一 `--query`、`--file` 或 stdin 输入；memory、redb 和 version 1 TCP introspection 一致。`.help`、`.quit` 仍只属于交互会话，`run` 仍只执行 Unionid 源码。
- **回归验收：** `cli_accepts_introspection_as_one_shot_query_file_stdin_and_tcp_input` 覆盖上述四个代表性组合，同时检查本地 JSON envelope、redb read-only 状态、本地人类输出和 TCP JSON 输出；CLI help、CLI 文档和 LLM 文档同步说明能力边界。
- **阻塞：** 修复前会阻塞自动化查看；修复后解除。

三条场景没有引入无试用依据的命令。人类输出保持可行动，JSON 复用现有 version 1 introspection 协议且不包含业务行；错误继续通过既有稳定 code 和退出码返回。

## English Description

This record is the reproducible acceptance evidence for [#363](https://github.com/worktools/unionid/issues/363). Every entry records the task, input and minimal database state, expected behavior, actual result, and whether the problem blocks use. All commands use temporary directories and newly created databases.

### Scenario one: complete a task-project lifecycle from an empty directory

- **Task:** Create a task project as a new user, validate its source, persist and query data, diagnose it, and restore a backup.
- **Input and minimal state:** An empty directory; run `scripts/validate-first-use.py --binary target/debug/unionid --work-dir <empty-dir>`. The script executes init, project check, initial migration, seed, typed query, reopen, doctor, integrity check, backup, restore, and a restored-database query.
- **Expected:** Generated source remains unchanged; two task rows persist; source and restored databases return identical schema identity, typed columns, and the `Running` ADT row.
- **Actual:** Passed. Six project files were generated, two rows persisted, source and restored queries each returned one row, and schema revision was 1. Doctor and integrity checks succeeded.
- **Blocking:** No. No fix was accepted; the first-use journey remains a release acceptance entry point.

### Scenario two: evolve and restore a deeply nested configuration ADT

- **Task:** Store nested sum/product/list configuration data, reopen it, evolve an internal record and enum payload, then back it up and restore it.
- **Input and minimal state:** A new database where `Config` contains `Source = Local | Remote {url, retry}`, nested `Retry`, and `list text`; run `cargo test --locked --test release_scenarios nested_config_survives_deep_adt_migration_and_restore`.
- **Expected:** Initial migration, insert/update, recursive match query, and reopen preserve typed values. The upgrade can be planned and rehearsed before renaming `Remote` to `Http` and adding fields to `Retry` and the payload. Index planning, integrity, backup/restore, ledger, RowId, and schema identity remain consistent after migration.
- **Actual:** Passed. Deep defaults and payload conversion were correct, `configs.source` remained a secondary-index lookup, and source and restored rows matched exactly.
- **Blocking:** No. No fix was accepted; this is the second complete application journey required by #363.

### Scenario three: inspect a database file directly, like DuckDB

- **Task:** Inspect tables, types, schema, and storage state without entering an interactive session.
- **Input and minimal state:** A redb containing a `Task` schema and two rows, plus an in-memory TCP server with the same schema. Exercise local redb `--query .tables`, local redb `--file .types`, local redb stdin `.schema`, and TCP `--query .storage`.
- **Expected:** Introspection uses the same human output as the REPL; `--format json` returns a stable, redacted envelope containing `kind` and typed `introspection`.
- **Before:** Typed row queries worked, but `.tables` was parsed as query language and returned `E_SYNTAX`. This blocked noninteractive catalog inspection from shell and LLM tooling.
- **After:** PR #368 accepts each of the four commands as the sole `cli` input through `--query`, `--file`, or stdin; memory, redb, and version 1 TCP introspection agree. `.help` and `.quit` remain interactive-only, while `run` continues to execute Unionid source exclusively.
- **Regression acceptance:** `cli_accepts_introspection_as_one_shot_query_file_stdin_and_tcp_input` covers those four representative combinations and checks the local JSON envelope, redb read-only state, local human output, and TCP JSON output. CLI help, CLI documentation, and LLM guidance describe the capability boundary.
- **Blocking:** It blocked automated inspection before the fix and no longer does.

These scenarios add no command without trial evidence. Human output remains actionable; JSON reuses the existing version 1 introspection protocol and contains no business rows. Errors continue to use the established stable codes and exit classes.
