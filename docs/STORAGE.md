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

一次 `Engine.execute`、一个 `run` 脚本或一个 TCP 请求构成一个原子批次。unionid 先解析并在候选状态中完成类型检查和执行，再把 catalog、rows、secondary indexes 和 meta 写入同一个 redb write transaction。带幂等 key 的 Engine mutation 还会在该事务中写入完整成功回执。事务使用 `Durability::Immediate` 与 two-phase commit；只有 `commit` 成功返回后，Engine 才同时发布候选状态和回执，并向客户端返回成功。

语法、类型、约束、编码或 redb transaction commit 之前的写入错误属于明确中止：返回 `E_STORAGE`，候选状态不发布，事务回滚，当前句柄仍可继续使用和重试。真正进入 redb `commit` 后返回的 I/O 错误属于结果不确定：当前 Engine 关闭 redb 句柄并禁用后续写入；读取仍反映进程内最后一次明确成功的状态。客户端不能把未确认写入当作确定回滚，也不应自动按 exactly-once 重试。重新打开数据库后，应通过业务主键查询确认结果。

提交前，持久后端分别编码 Engine 的已提交状态与候选状态，再按稳定 catalog ID、`(table_id, row_id)`、版本化 index key 和 ledger sequence 计算确定性差异。redb transaction 只删除消失的键，只写入新增或编码内容变化的键；普通数据写入不触碰 `migration_ledger`。meta 的固定版本与水位值保持同步更新。删除或覆盖时会核对 redb 中的旧值是否与 Engine 基线一致，不一致则在 commit 前明确中止并回滚事务。

当前 Engine 仍会为请求级隔离复制小工作集，并编码前后逻辑状态来计算差异，因此 CPU 与内存成本还不是 O(变更量)；本阶段消除的是 redb 的全表清空和持久写放大。后续若扩大工作集，可让执行器直接产生 mutation set，同时保留同一稳定键和原子提交契约。

每张表从 0 开始单调分配 `u64` RowId，并单独持久化下一分配值。RowId 与内存 `Vec` 位置分离，索引 posting 和 redb row key 都引用 RowId；未来删除产生的缺口合法，后续插入不会复用已删除身份。打开时要求已有 RowId 严格递增且小于分配游标。第一版 redb 文件没有游标时，可从原有连续 row key 推导并在下一次写入保存；旧 snapshot 缺少显式 RowId 时按当时的 vector 顺序升级。

## 固定内部表

| 表 | 键 | 值 | 当前用途 |
| --- | --- | --- | --- |
| `meta` | UTF-8 名称 | 固定宽度数字或 UTF-8 hash | 存储格式、codec 版本、提交序号、schema revision、ID 水位、schema hash、cursor instance ID 与 secret |
| `catalog` | `(kind, stable_id)` | 版本化 catalog definition | 命名类型、用户表和索引定义；表记录也保存 RowId 分配游标 |
| `rows` | `(table_id, row_id)` | 版本 1 ADT value codec | 完整类型化 record；字段与变体按稳定 ID 编码 |
| `secondary_index` | 版本化 `(index_id, equality_key, row_id)` | unit | 当前等值索引的派生记录 |
| `migration_ledger` | sequence | 版本化 migration record | `UIDM` magic、codec version、JSON entry；与 schema/data/index 同事务提交 |
| `idempotency_receipts` | 原始 UTF-8 key bytes | 版本化成功回执 | `UIDR` magic、codec version、digest、提交序号、完成时间和原 QueryResponse |

打开数据库时会拒绝未知的存储、catalog、ADT value、索引键、migration 或 receipt codec，并验证 schema hash、ledger 单链及其 head、RowId 唯一性／顺序／分配水位、索引是否与 catalog/rows 一致，以及 receipt key/digest/成功响应/sequence/容量边界。RowId 可以有删除形成的缺口。差异计划测试检查 update/delete/insert 只生成预期的 catalog/row/index 键变化；migration 测试确认 schema、数据、索引和 ledger 一起提交，普通数据提交保留 ledger。集成测试还会在一个未提交 redb transaction 修改多个内部表后直接退出子进程，确认重开只看到完整旧状态；也会在带 receipt 的 Engine commit 成功后不执行析构直接退出，确认重开能重放完整新状态。无效 redb 文件会返回 `E_STORAGE` 并保留原文件，跨进程第二个打开者返回 `E_BUSY`。

storage format 1 表示没有持久回执，format 2 增加 durable idempotency receipt。format 3 在 meta 中增加 128-bit database instance ID 和 256-bit cursor HMAC secret；打开 format 1/2 时会生成并通过同步事务升级，之后无写入重开仍可恢复 cursor。secret 不进入 introspection、日志、错误或逻辑 backup；restore 生成新身份，所以源数据库 cursor 不能用于副本。旧二进制不能安全打开更高格式。memory Engine 使用进程内随机身份；WAL/snapshot 兼容入口不承诺跨重启 cursor。幂等语义见 [RFC 0002](rfc/0002-idempotent-write-receipts.md)，分页语义见 [RFC 0003](rfc/0003-stable-cursor-pagination.md)。

macOS/Linux 测试还在隔离子进程中用操作系统 `RLIMIT_FSIZE` 把 redb 文件上限固定在已提交基线大小，再写入 900,000 字节 typed text 强制触发真实文件增长失败。子进程忽略 `SIGXFSZ`，使底层写入以错误返回 Engine：若失败发生在 `commit` 前，响应明确中止且句柄允许再次尝试；若 `commit` 返回错误，响应标记结果不确定并禁用后续写。父进程重开并运行完整性检查，接受完整旧状态或完整新状态，再核对 typed row、主键索引、schema 和 migration ledger，不接受部分内部表。

这些测试覆盖应用进程退出、真实文件增长失败和库级一致性检查，没有模拟机器掉电、文件系统违反同步承诺、物理设备损坏或每一个空间不足位置。`Immediate` 与 two-phase commit 的掉电保证来自 redb 的事务契约；设备与文件系统仍必须正确实现持久同步。更广的发布环境矩阵继续由 #24 跟踪。

可重复的恢复测量工具位于 `tools/recovery-eval`。2026-09-07 的三次中位数显示：10,000 行 ADT 工作集 open/check 为 66/78 ms，峰值 RSS 为 52.67/81.48 MiB；100,000 行为 653/741 ms，峰值 RSS 为 459.28/745.84 MiB。100,000 行检查的内存放大来自当前完整加载和索引验证，因此作为 v0.1 已测试上限，不作为日常目标。环境、命令、数据库大小和完整结果见[恢复成本记录](benchmarks/recovery-2026-09-07.md)。

## 与旧原型格式的关系

`--wal-path` 和 `--snapshot-path` 暂时保留，用于兼容早期原型。它们通过源码回放恢复，不是 redb 格式，也不会与 `--db` 双写；同一个 server 命令不能混用这两套入口。它们不能保存幂等回执；需要该保证时应先用 `import-legacy` 转换到新的 redb 路径，并继续保留原 WAL 和 snapshot 文件。
