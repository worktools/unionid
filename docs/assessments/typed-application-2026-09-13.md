# 类型化独立应用验收 / Typed independent application acceptance

- 日期 / Date: 2026-09-13
- 跟踪 / Tracking: [#289](https://github.com/worktools/unionid/issues/289)
- 入口 / Entry point: `python3 scripts/verify-typed-application.py`

## 中文说明

仓库维护的独立临时 crate 从空生成目录读取一个 schema 和六个查询文件。生成 bundle 只声明一份 `Task`、`State` 和 `Event`，应用不手写 `Value`、wire codec 或查询结果 DTO。真实 redb 旅程覆盖 typed insert returning、projection、普通 derive、穷尽 match derive、decimal/timestamp aggregate、索引驱动且每行上限为 5 的 lookup，以及关闭句柄后的重开。

`Task` 同时携带 sum、`option (option text)`、decimal、timestamp、date、duration 和 bytes。验收发现并修复了原先 `Value::to_serde` 经 JSON 中转而把 `Some(None)` 合并为 `None` 的问题；现在直接 typed deserializer 保留三种状态，并继续验证 native scalar marker、名义类型、sum/product/tuple/list 与深度限制。

| 方向 | 可执行结论 |
| --- | --- |
| 存量数据 | v1 两行和关联事件在重开及 migration 后保留；新增 `priority` 对旧行补为 `0`，精确标量和嵌套 option 不漂移 |
| 查询 | 新增 `State.Archived` 后，v1 穷尽 match 在生成阶段以带查询名的 non-exhaustive 诊断失败，且不产生 stale bundle；v2 补齐分支并改变 projection/digest |
| 客户端读取 | 已编译 v1 读取连接 v2 catalog 时在 prepare 前返回 `E_SCHEMA_CHANGED`；重新生成的 v2 客户端读取默认字段和新 variant |
| 客户端写入 | 已编译 v1 mutation 同样被 schema identity 拒绝且没有插入；v2 生成 `Task` 可写入 `Archived` 与显式 `priority = 3` |

生成物的 v1/v2 SHA-256 固定在 fixture，Ubuntu CI 每次从空临时目录重新生成、编译、执行并运行 `check --db`。这是仓库维护的参考应用证据；当前没有独立外部调用方参与，因此不表示外部采用成功。

## English Description

A repository-owned independent temporary crate starts with an empty generated directory and consumes one schema plus six query files. Its bundle declares `Task`, `State`, and `Event` once, with no handwritten `Value`, wire codec, or query-result DTO. The real-redb journey covers typed insert returning, projection, regular derivation, exhaustive match derivation, decimal/timestamp aggregation, an indexed lookup bounded to five rows per driver, handle close/reopen, and migration.

`Task` combines a sum, `option (option text)`, decimal, timestamp, date, duration, and bytes. Acceptance exposed and fixed an existing `Value::to_serde` JSON bridge that collapsed `Some(None)` into `None`. The direct typed deserializer now preserves all three nested-option states while retaining native scalar markers, nominal types, sums, products, tuples, lists, and depth checks.

| Direction | Executable finding |
| --- | --- |
| Existing data | Two v1 rows and related events survive reopen and migration; old rows receive `priority = 0`, while exact scalars and nested options remain unchanged |
| Query | Adding `State.Archived` makes the v1 exhaustive match fail generation with a query-named non-exhaustive diagnostic and no stale bundle; v2 covers the branch and changes projection/digest |
| Client read | A compiled v1 read returns `E_SCHEMA_CHANGED` before prepare against the v2 catalog; regenerated v2 code reads the defaulted field and new variant |
| Client write | A compiled v1 mutation is rejected by the same identity check without inserting; a generated v2 `Task` writes `Archived` with explicit `priority = 3` |

Pinned v1/v2 SHA-256 fixtures make Ubuntu CI regenerate, compile, execute, and run `check --db` from an empty temporary directory. This is repository-maintained reference evidence. No independent external caller participated, so it does not establish external adoption.
