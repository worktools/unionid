# ADR 0001：采用 SQLite 作为持久化事务后端

- 日期：2026-09-06
- 状态：已接受
- 关联：[GitHub issue #6](https://github.com/worktools/unionid/issues/6)

## 背景

unionid 需要把 catalog、ADT 数据、索引和 migration ledger 作为一个原子单位提交，并在进程退出或机器故障后恢复。当前 JSON WAL 与 snapshot 已能保护语言预览的正常重启和若干已知失败路径，但它仍然回放源码，并由项目自行负责记录格式、校验、恢复、压缩和备份。继续扩展这套实现会把数据库页管理和故障恢复变成项目的主要工作。

原生 ADT 不要求自研存储格式。unionid 自己负责类型 catalog、逻辑值编码、查询语义和 migration；底层后端负责事务页、同步、恢复和一致快照。

## 决策

持久模式采用 SQLite，通过 `rusqlite` 访问。发布构建启用 `bundled`，避免依赖目标机器预装的 SQLite 版本。内存模式继续保留，当前 JSON WAL/snapshot 仅作为原型兼容输入，在 SQLite 接入完成后退出主写入路径。

每个 Engine 写请求对应一个 SQLite 写事务。schema、catalog、row、secondary index 和 migration ledger 必须在同一事务中提交。首版仍由 unionid 的数据库占用锁保证一个服务或嵌入式 Engine 独占数据库；SQLite 自身允许多个连接，不能替代这个产品约束。

持久连接采用以下基线：

- `journal_mode=WAL`，允许拥有数据库的进程内读连接与单写者并存。
- `synchronous=FULL`；Apple 平台同时启用 `fullfsync`。成功响应只能在提交返回后发送。
- 写事务使用明确的事务边界，写入前后不维护另一份先行发布的内存真相。
- 提交返回 I/O 错误时关闭当前存储句柄并重新打开确认状态；API 不承诺把结果不确定的提交自动重试成 exactly-once。
- 在线数据库通过 SQLite backup API 备份。WAL 模式下不能把正在使用的主文件单独复制成备份。

[SQLite 的事务说明](https://www.sqlite.org/transactional.html)给出 ACID 与掉电恢复保证；[WAL 文档](https://www.sqlite.org/wal.html)说明 WAL 文件是持久状态的一部分以及同步等级的差异；[同步与 fullfsync 文档](https://www.sqlite.org/pragma.html#pragma_fullfsync)说明 Apple 平台的额外刷盘选项；[在线备份 API](https://www.sqlite.org/backup.html)提供运行中数据库的一致快照。`rusqlite` 的 [0.40.2 crate 文档](https://docs.rs/rusqlite/0.40.2/rusqlite/)提供 bundled SQLite 与 backup feature。

## 物理边界

SQLite 只承载 unionid 的有序键和版本化二进制值，不成为用户可见的 SQL 接口。初始物理 schema 按职责分表：

| 表 | 关键字段 | 用途 |
| --- | --- | --- |
| `meta` | `key`, `value` | 存储格式版本、当前 schema revision、ID 水位 |
| `catalog` | `kind`, `stable_id`, `payload` | 类型、字段、变体、表和索引定义 |
| `rows` | `table_id`, `row_id`, `payload` | 版本化 ADT 行值 |
| `secondary_index` | `index_id`, `encoded_key`, `row_id` | 可重建的类型化二级索引 |
| `migration_ledger` | `sequence`, `id`, `parent`, `checksum`, `payload` | 已应用 migration 的线性历史 |

逻辑值编码必须带 codec 版本，稳定 ID 不能由名称或 SQLite rowid 临时推导。未知存储格式或 codec 版本拒绝打开；格式升级与应用 schema migration 分开处理。二级索引是派生数据，但更新时仍与行及 catalog 原子提交，恢复工具可以从行重建并核对。

数据库路径在用户体验上仍视为一个受管理的数据库。实现必须一起管理主文件、运行时 WAL/SHM 和占用锁；备份命令产出可独立打开的快照，而不是要求用户理解 sidecar 文件。

## 候选比较

| 方案 | 原子性与恢复 | 备份与工具 | 依赖与维护 | 结论 |
| --- | --- | --- | --- | --- |
| 继续自有 WAL/snapshot | 已覆盖部分故障，但仍回放源码，缺少冻结的逻辑 codec、校验和与完整故障模型 | 需要自行设计备份、校验和格式升级 | 无新增依赖，长期正确性工作量最高 | 不进入长期写入路径 |
| redb 4.1 | 纯 Rust、ACID、MVCC、单写者；`Immediate` durability 在本次进程退出验证中通过 | 可在 read transaction 上做逻辑快照；独占打开由后端提供 | 集成直接，但备份、诊断和格式生命周期仍由本项目包一层 | 保留为未来约束变化时的备选 |
| SQLite + rusqlite 0.40 | 成熟的事务、WAL 与恢复实现；本次 FULL/fullfsync 验证通过 | 在线备份、完整性检查和成熟运维知识可复用 | bundled 会编译 C；unionid 需要维护逻辑编码与所有权锁 | 采用 |

redb 的 [4.1.0 官方文档](https://docs.rs/redb/4.1.0/redb/)说明其 ACID、MVCC、copy-on-write B-tree 和单写者模型；`Immediate` 的提交语义见 [Durability 文档](https://docs.rs/redb/4.1.0/redb/enum.Durability.html)。它与 unionid 的 KV 边界很契合。最终选择 SQLite 的主要原因是恢复、在线备份、检查工具和长期格式已有更成熟的边界，而不是一次本机速度测试。

## 可复现实验

[`tools/storage-eval`](../../tools/storage-eval/README.md)固定使用 redb 4.1.0 与 rusqlite 0.40.2。每个事务同时写 schema、catalog、row、index 和 ledger 五条逻辑记录，然后验证：

1. 未提交子进程直接退出后，五条记录都不可见。
2. 同步提交返回后子进程立即退出，五条记录都可见。
3. 重新打开得到完整批次。
4. 备份可以独立打开并包含一致的已提交视图。
5. 第二个后端实例能否打开同一路径。

2026-09-06 在 macOS、Rust 1.94.0 release 构建上串行运行三次，每次 200 个事务。下表是中位数：

| 后端 | 200 次同步写 | 重新打开并读取 | 一致备份 | 主库大小 | 后端独占打开 |
| --- | ---: | ---: | ---: | ---: | --- |
| redb | 1,212 ms | 51 ms | 103 ms | 323,584 B | 是 |
| SQLite | 1,396 ms | 1 ms | 4 ms | 106,496 B | 否 |

两者的未提交退出、提交后立即退出和备份一致性验证均通过。数字只描述这台机器上的小型同步写工作负载；样本太小，不能作为吞吐承诺，也没有用于决定胜负。CI 在 macOS 与 Linux 上运行较小的相同验证，防止示例只在开发机成立。

实验模拟的是应用进程在事务边界退出，没有切断机器电源，也没有注入文件系统、磁盘缓存或 `fsync` 故障。掉电保证依据后端文档和同步配置，后续 #14 仍需补充故障矩阵、完整性检查、损坏处理与备份还原测试。

运行完整实验：

```bash
cargo run --release --locked --manifest-path tools/storage-eval/Cargo.toml -- redb /tmp/unionid-redb.db 200
cargo run --release --locked --manifest-path tools/storage-eval/Cargo.toml -- sqlite /tmp/unionid-sqlite.db 200
```

## 实施与替换成本

#13 在 `Engine` 与 SQLite 之间建立窄的 storage transaction 边界，并先实现上表的物理 schema 与版本化 codec。接入时直接以 SQLite 作为唯一持久写入路径，不做长期双写。已有原型 WAL/snapshot 由 #20 提供显式导入，保留原文件并验证行数、catalog 和 schema revision 后才完成转换。

如果未来 bundled C 成为无法接受的发布约束，或目标平台不能可靠运行 SQLite，可以保持上层 codec 和事务接口不变，重新评估 redb。替换时需要重做故障恢复、备份和格式升级验收；本 ADR 不把底层文件格式暴露为 unionid 的公共语言契约。
