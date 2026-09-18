# v0.10 CLI 试用记录 / CLI trial record

日期 / Date: 2026-09-18

## 中文说明

本记录用当前 release 构建执行两条不同的日常使用链路，作为 [#363](https://github.com/worktools/unionid/issues/363) 的可复现依据。试用只使用临时目录和新建数据库。

### 场景一：从空目录完成任务项目生命周期

运行 `scripts/validate-first-use.py`，覆盖 `init`、project check、migration、seed、typed query、doctor、完整性检查、backup 和 restore。链路成功完成，2 行任务数据在恢复后保持一致。该场景没有发现需要接受的修复，说明已有入门主链可以继续作为发布验收入口。

### 场景二：像 DuckDB 一样直接查看数据库文件

使用 `unionid cli --db tasks.redb --read-only --query 'from tasks | sort id'` 能直接读取 typed rows；但相同的一次性入口执行 `.tables` 时返回 `E_SYNTAX`。用户必须进入交互 REPL 才能查看 catalog，脚本和 LLM 工具也无法复用这些命令。

接受的改进是让 `.schema`、`.tables`、`.types` 和 `.storage` 在 `cli` 的唯一 `--query`、`--file` 或标准输入中生效，并覆盖本地 memory、redb、TCP、人类输出和稳定 JSON envelope。`.help`、`.quit` 仍只属于交互会话，`run` 仍只执行 Unionid 源码。CLI help、用户文档和 LLM 文档同步说明这一边界。

## English Description

This record exercises two distinct daily-use journeys with the current release build as reproducible evidence for [#363](https://github.com/worktools/unionid/issues/363). Both trials use temporary directories and newly created databases.

### Scenario one: complete task-project lifecycle from an empty directory

`scripts/validate-first-use.py` covers init, project check, migration, seed, typed query, doctor, integrity check, backup, and restore. The journey completed successfully, and both task rows survived restore unchanged. No fix was accepted from this scenario; the existing onboarding path remains a useful release acceptance entry point.

### Scenario two: inspect a database file directly, like DuckDB

`unionid cli --db tasks.redb --read-only --query 'from tasks | sort id'` reads typed rows directly, but the same one-shot entry returned `E_SYNTAX` for `.tables`. Users had to enter the interactive REPL to inspect the catalog, and scripts or LLM tooling could not reuse those commands.

The accepted improvement makes `.schema`, `.tables`, `.types`, and `.storage` work as the sole `--query`, `--file`, or standard-input command for `cli`. Coverage includes local memory, redb, TCP, human-readable output, and the stable JSON envelope. `.help` and `.quit` remain interactive-only, while `run` continues to execute Unionid source exclusively. CLI help, user documentation, and LLM guidance describe the same boundary.
