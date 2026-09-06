# redb 持久模式

unionid 的正式持久入口使用 redb 4.1。内存模式适合语言试验和临时数据；需要跨进程保存应用状态时，为本地命令、REPL 或服务指定同一个 `.redb` 文件。

## 使用入口

执行脚本并保存结果：

```bash
cargo run -- run --db ./data/unionid.redb --file examples/tasks.uid
cargo run -- run --db ./data/unionid.redb --query 'from tasks | filter id == 1'
```

直接打开本地持久 REPL：

```bash
cargo run -- cli --db ./data/unionid.redb
```

启动 TCP 服务：

```bash
cargo run -- server --addr 127.0.0.1:7878 --db ./data/unionid.redb
cargo run -- cli --addr 127.0.0.1:7878
```

redb 对数据库文件保持独占所有权。同一时间只能由一个本地 Engine 或服务打开；已有服务持有文件时，其他进程得到 `E_BUSY`，应连接该服务，而不是再次直接打开文件。

## 提交语义

一次 `Engine.execute`、一个 `run` 脚本或一个 TCP 请求构成一个原子批次。unionid 先解析并在候选状态中完成类型检查和执行，再把 catalog、rows、secondary indexes 和 meta 写入同一个 redb write transaction。事务使用 `Durability::Immediate` 与 two-phase commit；只有 `commit` 成功返回后，Engine 才发布候选状态并向客户端返回成功。

语法、类型或约束错误发生在持久提交前，不改变内存或磁盘状态。存储提交错误返回 `E_STORAGE`，当前 Engine 随即关闭 redb 句柄并禁用后续写入；读取仍反映进程内最后一次明确成功的状态。由于 I/O 错误可能发生在提交结果已经落盘但响应尚未确认的边界，客户端不能把未收到成功响应的写入当作确定回滚，也不应自动按 exactly-once 重试。重新打开数据库后，应通过业务主键查询确认结果。

当前实现为小工作集优先：每个提交重写完整逻辑 catalog、rows 和派生索引，依靠单个 redb 事务保证一致性。#15 将在 CRUD/RowId 稳定后引入增量写入；这不会改变上述原子提交承诺。

## 固定内部表

| 表 | 键 | 值 | 当前用途 |
| --- | --- | --- | --- |
| `meta` | UTF-8 名称 | 固定宽度数字或 UTF-8 hash | 存储格式、codec 版本、提交序号、schema revision、ID 水位和 schema hash |
| `catalog` | `(kind, stable_id)` | 版本化 catalog definition | 命名类型、用户表和索引定义 |
| `rows` | `(table_id, row_id)` | 版本 1 ADT value codec | 完整类型化 record；字段与变体按稳定 ID 编码 |
| `secondary_index` | 版本化 `(index_id, equality_key, row_id)` | unit | 当前等值索引的派生记录 |
| `migration_ledger` | sequence | 版本化 migration record | 已预留；正式 runner 由 #18 接入 |

打开数据库时会拒绝未知的存储、catalog、ADT value 或索引键版本，并验证 schema hash、连续 RowId 以及索引是否与 catalog/rows 一致。更完整的损坏、异常退出、空间不足和恢复时间矩阵由 #14 继续实现。

## 与旧原型格式的关系

`--wal-path` 和 `--snapshot-path` 暂时保留，用于兼容早期原型。它们通过源码回放恢复，不是 redb 格式，也不会与 `--db` 双写；同一个 server 命令不能混用这两套入口。#20 将提供显式、可检查的旧格式导入和正式备份／还原命令。在导入工具完成前，需要保留原 WAL 和 snapshot 文件。
