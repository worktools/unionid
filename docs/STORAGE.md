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

普通 row-only mutation 会生成合并的逻辑 write set，并只编码其中变化的 catalog、`(table_id, row_id)`、版本化 index key 和 receipt key。redb transaction 只删除消失的键，只写入新增或编码内容变化的键；普通数据写入不触碰 `migration_ledger`。meta 的固定版本与水位值保持同步更新。删除或覆盖时会核对 redb 中的旧值是否与 Engine 基线一致，不一致则在 commit 前明确中止并回滚事务。

DDL、schema/data migration、格式升级、restore 与 receipt prune 仍走 full-rebuild 路径：重新加载 durable 前态，编码完整候选状态，再按稳定键计算差异。该路径保留同一原子提交契约，但 CPU 和峰值内存仍随完整数据规模增长。

`Engine::open_profile` 和成功 mutation 的 `MutationProfile::durable` 提供不含业务值的内部诊断。open 分离 redb open、bootstrap、各内部表读取、typed `Database` 构造和逻辑验证；format-5 普通 open 还以 `bounded_view = true` 明确表示它没有遍历 durable rows/indexes，此时相应 entry/byte 计数为零。commit 分离 prepare、transaction apply 与 sync，full rebuild 还记录前态 reload、完整后态 encode 和 diff。`prepare` 包含这三个 full-rebuild 子阶段，不能与它们重复相加。profile 只在成功 open/commit 后发布；memory Engine 没有 durable profile，read-only redb 会显式标记。类型不包含 schema/field 名、key/value、cursor secret、idempotency key 或 receipt payload，也不改变 storage/catalog/value/index/backup/protocol 格式。

每张表从 0 开始单调分配 `u64` RowId，并单独持久化下一分配值。RowId 与内存 `Vec` 位置分离，索引 posting 和 redb row key 都引用 RowId；未来删除产生的缺口合法，后续插入不会复用已删除身份。打开时要求已有 RowId 严格递增且小于分配游标。第一版 redb 文件没有游标时，可从原有连续 row key 推导并在下一次写入保存；旧 snapshot 缺少显式 RowId 时按当时的 vector 顺序升级。

## 固定内部表

| 表 | 键 | 值 | 当前用途 |
| --- | --- | --- | --- |
| `meta` | UTF-8 名称 | 固定宽度数字或 UTF-8 hash | 存储格式、codec 版本、提交序号、schema revision、ID 水位、schema hash、cursor instance ID 与 secret |
| `catalog` | `(kind, stable_id)` | 版本化 catalog definition | 命名类型、用户表和索引定义；表记录也保存 RowId 分配游标 |
| `rows` | `(table_id, row_id)` | 版本 2 ADT value codec | 完整类型化 record；字段与变体按稳定 ID 编码 |
| `secondary_index` | 版本化 `(index_id, component_count, typed_components, row_id)` | unit | ordinary/unique 复合索引的有序派生记录；每个 component 带方向 |
| `migration_ledger` | sequence | 版本化 migration record | `UIDM` magic、codec version、JSON entry；与 schema/data/index 同事务提交 |
| `idempotency_receipts` | 原始 UTF-8 key bytes | 版本化成功回执 | `UIDR` magic、codec version、digest、提交序号、完成时间和原 QueryResponse |

format-5 普通打开会验证 meta、catalog、schema hash、ledger 单链及其 head、receipt codec/容量边界，并确认固定 row/index 表存在；它不会遍历全部 row value 或 index key。按需读取仍会拒绝触及的未知／损坏 value 与 index-key codec。显式 `check` 才遍历全部 rows/indexes，验证 RowId 唯一性／顺序／分配水位和索引是否与 catalog/rows 一致。RowId 可以有删除形成的缺口。差异计划测试检查 update/delete/insert 只生成预期的 catalog/row/index 键变化；migration 测试确认 schema、数据、索引和 ledger 一起提交，普通数据提交保留 ledger。集成测试还会在一个未提交 redb transaction 修改多个内部表后直接退出子进程，确认重开只看到完整旧状态；也会在带 receipt 的 Engine commit 成功后不执行析构直接退出，确认重开能重放完整新状态。无效 redb 文件会返回 `E_STORAGE` 并保留原文件，跨进程第二个打开者返回 `E_BUSY`。

index-key codec 3 使用 catalog 中绑定的 component 类型编码完整 tuple。primitive、命名 sum/record、tuple、option、list 和有限递归 ADT 都遵循查询比较器的 total order；sum variant 与 record field 按稳定 ID 编码。UUID 使用 16-byte network order，bytes 使用 unsigned lexicographic order，descending component 对完整 prefix-free component 编码取反。任一索引键内的 bytes leaf 最多 8192 octets，完整 durable key 最多 64 KiB；建索引、写入、migration 和 restore 共用 `E_INDEX_KEY_LIMIT` 原子拒绝边界。未索引 bytes 的 value 上限为 16 MiB。

storage format 1 表示没有持久回执，format 2 增加 durable idempotency receipt。format 3 在 meta 中增加 128-bit database instance ID 和 256-bit cursor HMAC secret；format 4 增加生产标量 codec；当前 format 5 增加有序复合索引的 catalog codec 4 与 index-key codec 3。打开 format 1/2 时会生成 cursor 身份并通过同步事务升级到 3；format 3→4 与 4→5 使用显式 `upgrade`。format 4 可以继续读写已有单列升序索引，但新增复合或降序 shape 会返回 `E_STORAGE_UPGRADE_REQUIRED`。升级保留逻辑 schema identity、rows、RowId、ledger、receipts、数据库/cursor 身份和 sequence。secret 不进入 introspection、日志、错误或逻辑 backup；restore 生成新身份，所以源数据库 cursor 不能用于副本。旧二进制不能安全打开更高格式。memory Engine 使用进程内随机身份；WAL/snapshot 兼容入口不承诺跨重启 cursor。幂等语义见 [RFC 0002](rfc/0002-idempotent-write-receipts.md)，分页语义见 [RFC 0003](rfc/0003-stable-cursor-pagination.md)，复合索引契约见 [RFC 0009](rfc/0009-ordered-composite-indexes.md)。

macOS/Linux 测试还在隔离子进程中用操作系统 `RLIMIT_FSIZE` 把 redb 文件上限固定在已提交基线大小，再写入 900,000 字节 typed text 强制触发真实文件增长失败。子进程忽略 `SIGXFSZ`，使底层写入以错误返回 Engine：若失败发生在 `commit` 前，响应明确中止且句柄允许再次尝试；若 `commit` 返回错误，响应标记结果不确定并禁用后续写。父进程重开并运行完整性检查，接受完整旧状态或完整新状态，再核对 typed row、主键索引、schema 和 migration ledger，不接受部分内部表。

这些测试覆盖应用进程退出、真实文件增长失败和库级一致性检查，没有模拟机器掉电、文件系统违反同步承诺、物理设备损坏或每一个空间不足位置。`Immediate` 与 two-phase commit 的掉电保证来自 redb 的事务契约；设备与文件系统仍必须正确实现持久同步。更广的发布环境矩阵继续由 #24 跟踪。

可重复的恢复测量工具位于 `tools/recovery-eval`。2026-09-07 的三次中位数显示：10,000 行 ADT 工作集 open/check 为 66/78 ms，峰值 RSS 为 52.67/81.48 MiB；100,000 行为 653/741 ms，峰值 RSS 为 459.28/745.84 MiB。100,000 行检查的内存放大来自当前完整加载和索引验证，因此作为 v0.1 已测试上限，不作为日常目标。环境、命令、数据库大小和完整结果见[恢复成本记录](benchmarks/recovery-2026-09-07.md)。

M6 使用更宽的 row 与额外复合索引重新测量完整工作负载：10k/100k 的增量单行 write p95 都约 10 ms，但 100k Engine open p95 约 5.6 s、resident query 接近 1 GiB，完整深层 migration p95 约 49.9 s、peak RSS 约 1.44 GiB。普通 write set 已不再复制整库；open/check 与 schema/data full rebuild 仍是大工作集的主要限制。完整方法、结构化访问计划和原始样本见 [M6 工作负载记录](benchmarks/workload-2026-09-09.md)。

M7 的分阶段复测显示：100k open p50 约 5.44 s，其中完整派生索引重算与逻辑验证约 4.72 s，typed `Database` 构造约 0.55 s；row/index 读取合计约 0.15 s。100k migration durable commit p50 约 47.18 s，其中完整候选编码约 41.01 s、重载并验证前态约 5.44 s，而 transaction apply 与 sync 合计约 0.76 s。下一阶段应消除重复全量验证/编码并引入可恢复 generation，而不是只优化 redb I/O。边界、完整样本和解释见 [M7 存储阶段记录](benchmarks/storage-phases-2026-09-09.md)。

[RFC 0010](rfc/0010-bounded-resident-state-and-maintenance-generations.md) 据此冻结物理边界：memory/redb 共用 typed row source 和 query IR；format-5 redb committed view 已绑定一个 MVCC transaction、schema/sequence/receipt root 和严格 32 MiB snapshot-local row cache，并按 RowId/index span 解码实际候选。100k 结构化测量中，普通 open 未遍历 rows/indexes，冷／热唯一索引查询分别只解码 1／0 行，详见 [Legacy0 有界读取记录](benchmarks/bounded-legacy-read-2026-09-09.md)。full scan、check、backup 和 mutation candidate 仍在 #183 继续接入完整有界 pipeline；format 6 将通过 Legacy0 兼容 format 5，再以 shadow generation 分批构建、验证和原子 cutover schema/data migration。

format-5 bounded view 上的 mutation 当前会先把 source 物化为私有 candidate，再复用既有类型、约束和原子 DML 实现；提交到 redb 时仍只编码并核对 write set 中变化的稳定键。这个过渡边界保持结果与文件格式兼容，但 candidate working set 尚未有界，由 #183 继续处理。

## 与旧原型格式的关系

`--wal-path` 和 `--snapshot-path` 暂时保留，用于兼容早期原型。它们通过源码回放恢复，不是 redb 格式，也不会与 `--db` 双写；同一个 server 命令不能混用这两套入口。它们不能保存幂等回执；需要该保证时应先用 `import-legacy` 转换到新的 redb 路径，并继续保留原 WAL 和 snapshot 文件。
