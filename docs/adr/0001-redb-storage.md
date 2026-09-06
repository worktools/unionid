# ADR 0001：采用 redb 作为持久化事务后端

- 日期：2026-09-06
- 状态：已接受
- 关联：[GitHub issue #6](https://github.com/worktools/unionid/issues/6)

实现状态：主 Engine 已提供 `Engine::open_redb`，CLI 的 `run/cli/server --db` 已使用下述固定内部表、版本化 codec、`Durability::Immediate` 与 two-phase commit。提交按稳定 catalog ID、RowId 和 index key 计算差异，只修改变化的键，并在删除或覆盖前核对旧值。`check --db`、提交前／后进程退出矩阵、未知版本与独占打开诊断已经接入；真实设备故障、恢复时间、格式升级和备份仍由 #13/#14/#20 跟踪。

## 背景

unionid 的用户模型是运行时声明的代数数据类型。catalog、ADT 行、类型化索引和 migration ledger 必须作为一个原子单位提交，并在进程退出或机器故障后恢复。当前 JSON WAL 与 snapshot 能保护语言预览的正常重启和若干已知失败路径，但仍然回放源码，并由项目自行负责事务记录、校验、恢复、压缩和备份。

候选后端都不会直接理解 unionid 动态声明的 sum/record/tuple。unionid 必须拥有类型身份、逻辑值 codec、相等与排序、查询和 migration 语义；后端只提供有序存储、事务和恢复。选型因此优先考虑底层模型是否让 ADT 保持唯一的数据抽象，再考虑现成运维能力。

## 决策

持久模式采用 redb 4.1。内存模式继续保留；当前 JSON WAL/snapshot 仅作为原型兼容输入，在 redb 接入完成后退出主写入路径。

redb 的表以 `TableDefinition<K, V>` 定义，`Value` 控制值的字节表示，`Key` 控制键的全序。这与 unionid 自有的版本化 ADT codec 和类型化索引键直接对应。[TableDefinition](https://docs.rs/redb/4.1.0/redb/struct.TableDefinition.html)、[Value](https://docs.rs/redb/4.1.0/redb/trait.Value.html)、[Key](https://docs.rs/redb/4.1.0/redb/trait.Key.html)

redb 对 Rust tuple 和 Option 的内建支持是编译期类型能力，不等同于支持 unionid 的运行时类型声明。首版不为每个用户类型生成 Rust 类型，也不为每张用户表创建不同泛型的 redb 表；所有用户值仍经过 unionid codec。

每个 Engine 写请求对应一个 redb `WriteTransaction`。catalog、row、secondary index 和 migration ledger 必须在同一事务中提交。持久写事务显式使用：

- `Durability::Immediate`，只有 `commit` 成功返回后才向客户端返回成功。
- `set_two_phase_commit(true)`，采用更保守的两阶段提交写盘顺序。
- 提交 I/O 错误后关闭当前数据库句柄并重新打开确认状态；未收到成功响应的客户端不能假定写入已回滚，也不能自动重试为 exactly-once。

redb 文档说明 `Immediate` 在提交返回时保证持久，并说明默认提交槽、校验和、two-phase commit 以及崩溃恢复过程。[Durability](https://docs.rs/redb/4.1.0/redb/enum.Durability.html)、[WriteTransaction](https://docs.rs/redb/4.1.0/redb/struct.WriteTransaction.html)

## 物理边界

redb 只承载 unionid 的稳定键和版本化字节值。初始内部表固定为：

| redb 表 | 键 | 值 | 用途 |
| --- | --- | --- | --- |
| `meta` | UTF-8 key | versioned bytes | 存储格式版本、当前 schema revision、ID 水位 |
| `catalog` | `(kind, stable_id)` | encoded definition | 类型、字段、变体、用户表和索引定义 |
| `rows` | `(table_id, row_id)` | encoded ADT value | 完整的类型化行值 |
| `secondary_index` | encoded `(index_id, ADT key, row_id)` | unit | 可重建的类型化二级索引 |
| `migration_ledger` | sequence | encoded migration record | `id/parent/checksum` 与已应用历史 |

逻辑值编码必须带 codec 版本；稳定 ID 不能从名称或物理位置临时推导。索引键编码必须使字节全序与 unionid 对该类型声明的相等和排序一致，不能继承 Rust 派生顺序或 JSON 文本顺序。未知存储格式、codec 或键编码版本拒绝打开；后端文件格式升级与应用 schema migration 分开处理。

内部表在同一写事务中打开和修改。二级索引是派生数据，但正常写入仍与行及 catalog 原子提交；恢复工具可以扫描行重建并核对索引。当前持久提交比较 Engine 的前后状态，以稳定键生成 delete/write 集合；它不清空内部表，也不改写内容未变化的 row/index 或普通写入尚未使用的 migration ledger。

redb 自身阻止同一个数据库文件被第二个实例打开，这与 unionid 的单所有者模型一致。产品层把 `DatabaseAlreadyOpen` 转成稳定的忙错误；服务占用数据库时，本地 CLI 应连接服务或明确失败。[Database API](https://docs.rs/redb/4.1.0/redb/struct.Database.html)

## 备份与恢复

在线备份在一个 redb read transaction 的一致视图中遍历全部内部表，写入新的 redb 数据库；目标使用 `Immediate` 与 two-phase commit，完成后重新打开，校验格式版本、schema revision、表计数和必要 checksum，再原子发布为备份。不能把运行中的数据库文件直接复制当作受支持的备份流程。

还原在数据库关闭且取得目标路径所有权后进行。原备份文件先保持不变，校验通过后再发布到目标路径。#20 实现正式的 `backup/restore` 命令、逻辑导出和旧 WAL/snapshot 导入。

redb 在打开时自动检测并恢复非正常关闭，也提供 `check_integrity` 和 `compact`；完整损坏诊断、恢复时间和工具行为仍需由 #14/#20 做项目级验证。[Database API](https://docs.rs/redb/4.1.0/redb/struct.Database.html)

## 候选比较

| 方案 | 与 ADT 的模型匹配 | 事务与恢复 | 查询与索引边界 | 结论 |
| --- | --- | --- | --- | --- |
| 继续自有 WAL/snapshot | 可完全自定义，但现状回放源码，尚无冻结 codec、checksum 和完整故障协议 | 所有正确性都由本项目承担 | 可完全控制 | 不进入长期写入路径 |
| redb 4.1 | 有序 KV 与版本化字节值直接承载运行时 ADT；只有 unionid 一套 schema | ACID、MVCC、单写者、校验和与崩溃恢复 | unionid 定义键的相等、全序和查询执行 | 采用 |
| SQLite + rusqlite 0.40 | ADT 必须拆为关系列、JSON 或 BLOB；若用 BLOB，SQLite 主要退化为事务 KV | ACID、WAL、在线备份及成熟工具 | 只有把语言编译到 SQL 才能充分利用 planner；否则还需维护 unionid 索引 | 保留为偏 SQL/运维路线的备选 |

SQLite 即使使用 STRICT table，也只提供 `INT/INTEGER/REAL/TEXT/BLOB/ANY`，没有 sum、record 或 tuple；JSON/JSONB 仍存为 TEXT/BLOB。[SQLite STRICT tables](https://www.sqlite.org/stricttables.html)、[SQLite JSON](https://www.sqlite.org/json1.html)。如果未来产品转向把查询编译成 SQL、需要复杂 join 或 SQL 生态，SQLite 的匹配度会提高。当前目标是让上层乃至内部查询与索引都以 ADT 为唯一抽象，因此选择 redb。

## 可复现实验

[`tools/storage-eval`](../../tools/storage-eval/README.md)固定使用 redb 4.1.0 与 rusqlite 0.40.2。每个事务同时写 schema、catalog、row、index 和 ledger 五条逻辑记录，然后验证：

1. 未提交子进程直接退出后，五条记录都不可见。
2. 同步提交返回后子进程立即退出，五条记录都可见。
3. 重新打开得到完整批次。
4. 备份可以独立打开并包含一致的已提交视图。
5. 第二个后端实例能否打开同一路径。

2026-09-06 在 macOS、Rust 1.94.0 release 构建上串行运行三次，每次 200 个事务。redb 使用 `Immediate` 与 two-phase commit；SQLite 对照使用 WAL、`synchronous=FULL` 与 `fullfsync=ON`。下表是中位数：

| 后端 | 200 次同步写 | 重新打开并读取 | 一致备份 | 主库大小 | 后端独占打开 |
| --- | ---: | ---: | ---: | ---: | --- |
| redb | 1,912 ms | 47 ms | 70 ms | 323,584 B | 是 |
| SQLite | 1,023 ms | 1 ms | 1 ms | 106,496 B | 否 |

两者的未提交退出、提交后立即退出和备份一致性验证均通过。数字只描述一台机器上的小型同步写工作负载，不能作为吞吐承诺；redb 的模型匹配优先级高于本次耗时。CI 在 macOS 与 Linux 上运行较小的相同验证。

实验模拟的是应用进程在事务边界退出，没有切断机器电源，也没有注入文件系统、磁盘缓存或 `fsync` 故障。掉电保证依据后端文档和同步配置，后续 #14 仍需补充提交内部故障点、恢复耗时、完整性检查和损坏处理。

运行完整实验：

```bash
cargo run --release --locked --manifest-path tools/storage-eval/Cargo.toml -- redb /tmp/unionid-redb.db 200
cargo run --release --locked --manifest-path tools/storage-eval/Cargo.toml -- sqlite /tmp/unionid-sqlite.db 200
```

## 实施与替换成本

#13 在 `Engine` 与 redb 之间建立窄的 storage transaction 边界，先实现固定内部表、版本化 ADT codec 与有序索引键。接入时直接以 redb 作为唯一持久写入路径，不做长期双写。已有原型 WAL/snapshot 由 #20 显式导入，保留原文件并验证行数、catalog 和 schema revision 后才完成转换。

如果未来需要把查询下推到成熟 SQL planner、开放 SQL 互操作或大量关系型能力，可以保持上层 ADT、codec 与 storage transaction 契约，重新评估 SQLite。替换时必须重做故障恢复、备份和格式升级验收；redb 文件格式和内部表布局都不成为 unionid 查询语言的公共契约。
