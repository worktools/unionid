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

数据库关闭后可执行完整性检查：

```bash
cargo run -- check --db ./data/unionid.redb
cargo run -- check --db ./data/unionid.redb --format json
```

`check` 先打开并验证 unionid 逻辑格式，再运行 redb `check_integrity`；该过程可能修复 redb 的 allocator/commit 元数据，随后会重新加载并再次核对 unionid 的 schema、rows 和 indexes。输出中的 `backend_clean = true` 表示 redb 未发现需要修复的内部状态；`false` 表示修复已执行且修复后的逻辑状态通过验证。服务占用文件时，`check` 返回 `E_BUSY`。

## 提交语义

一次 `Engine.execute`、一个 `run` 脚本或一个 TCP 请求构成一个原子批次。unionid 先解析并在候选状态中完成类型检查和执行，再把 catalog、rows、secondary indexes 和 meta 写入同一个 redb write transaction。事务使用 `Durability::Immediate` 与 two-phase commit；只有 `commit` 成功返回后，Engine 才发布候选状态并向客户端返回成功。

语法、类型、约束、编码或 redb transaction commit 之前的写入错误属于明确中止：返回 `E_STORAGE`，候选状态不发布，事务回滚，当前句柄仍可继续使用和重试。真正进入 redb `commit` 后返回的 I/O 错误属于结果不确定：当前 Engine 关闭 redb 句柄并禁用后续写入；读取仍反映进程内最后一次明确成功的状态。客户端不能把未确认写入当作确定回滚，也不应自动按 exactly-once 重试。重新打开数据库后，应通过业务主键查询确认结果。

当前实现为小工作集优先：每个提交重写完整逻辑 catalog、rows 和派生索引，依靠单个 redb 事务保证一致性。RowId 已经稳定；#15 将在 CRUD 语义完成后引入增量写入，这不会改变上述原子提交承诺。

每张表从 0 开始单调分配 `u64` RowId，并单独持久化下一分配值。RowId 与内存 `Vec` 位置分离，索引 posting 和 redb row key 都引用 RowId；未来删除产生的缺口合法，后续插入不会复用已删除身份。打开时要求已有 RowId 严格递增且小于分配游标。第一版 redb 文件没有游标时，可从原有连续 row key 推导并在下一次写入保存；旧 snapshot 缺少显式 RowId 时按当时的 vector 顺序升级。

## 固定内部表

| 表 | 键 | 值 | 当前用途 |
| --- | --- | --- | --- |
| `meta` | UTF-8 名称 | 固定宽度数字或 UTF-8 hash | 存储格式、codec 版本、提交序号、schema revision、ID 水位和 schema hash |
| `catalog` | `(kind, stable_id)` | 版本化 catalog definition | 命名类型、用户表和索引定义；表记录也保存 RowId 分配游标 |
| `rows` | `(table_id, row_id)` | 版本 1 ADT value codec | 完整类型化 record；字段与变体按稳定 ID 编码 |
| `secondary_index` | 版本化 `(index_id, equality_key, row_id)` | unit | 当前等值索引的派生记录 |
| `migration_ledger` | sequence | 版本化 migration record | 已预留；正式 runner 由 #18 接入 |

打开数据库时会拒绝未知的存储、catalog、ADT value 或索引键版本，并验证 schema hash、RowId 唯一性／顺序／分配水位以及索引是否与 catalog/rows 一致。RowId 可以有删除形成的缺口。集成测试会在一个未提交 redb transaction 修改多个内部表后直接退出子进程，确认重开只看到完整旧状态；也会在 Engine commit 成功后不执行析构直接退出，确认重开看到完整新状态。无效 redb 文件会返回 `E_STORAGE` 并保留原文件，跨进程第二个打开者返回 `E_BUSY`。

这些测试覆盖应用进程退出和库级一致性检查，没有模拟机器掉电、文件系统违反同步承诺、真实设备损坏或所有空间不足位置。`Immediate` 与 two-phase commit 的掉电保证来自 redb 的事务契约；设备与文件系统仍必须正确实现持久同步。更完整的真实 I/O 故障和恢复时间矩阵继续由 #13/#14 跟踪。

## 与旧原型格式的关系

`--wal-path` 和 `--snapshot-path` 暂时保留，用于兼容早期原型。它们通过源码回放恢复，不是 redb 格式，也不会与 `--db` 双写；同一个 server 命令不能混用这两套入口。#20 将提供显式、可检查的旧格式导入和正式备份／还原命令。在导入工具完成前，需要保留原 WAL 和 snapshot 文件。
