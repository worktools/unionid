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

`check` 先打开并验证 unionid catalog，再运行 redb `check_integrity`；该过程可能修复 redb 的 allocator/commit 元数据，随后从新的 committed view 做有界逻辑检查。检查逐 row 验证 type、RowId 和 watermark，为每个 row 点查所有期望 index entry，再逐 stored index entry 反查 row 并重算 exact key；cardinality 与相邻 unique key 检查发现缺失、多余或重复项。它不建立完整 typed `Database` 或派生 index set。输出中的 `backend_clean = true` 表示 redb 未发现需要修复的内部状态；`false` 表示修复已执行且修复后的逻辑状态通过验证。`profile` 报告 backend/logical 耗时、rows/index entries/bytes、point lookups、working peak 和 `bounded`。服务占用文件时，`check` 返回 `E_BUSY`。

## 提交语义

一次 `Engine.execute`、一个 `run` 脚本或一个 TCP 请求构成一个原子批次。unionid 先解析并在候选状态中完成类型检查和执行，再把 catalog、rows、secondary indexes 和 meta 写入同一个 redb write transaction。带幂等 key 的 Engine mutation 还会在该事务中写入完整成功回执。事务使用 `Durability::Immediate` 与 two-phase commit；只有 `commit` 成功返回后，Engine 才同时发布候选状态和回执，并向客户端返回成功。

语法、类型、约束、编码或 redb transaction commit 之前的写入错误属于明确中止：返回 `E_STORAGE`，候选状态不发布，事务回滚，当前句柄仍可继续使用和重试。真正进入 redb `commit` 后返回的 I/O 错误属于结果不确定：当前 Engine 关闭 redb 句柄并禁用后续写入；读取仍反映进程内最后一次明确成功的状态。客户端不能把未确认写入当作确定回滚，也不应自动按 exactly-once 重试。重新打开数据库后，应通过业务主键查询确认结果。

format-5 Legacy0 和 format-6 active generation 上的单条 insert/upsert/update/delete 直接从 committed source 构造有界 change set，不先物化全表。update/delete 的 filter/match/index-order/take target 只保留 RowId，再按需读取目标 row；primary/unique constraint 通过 durable typed index 点查验证。change set 同时受 250,000 rows 与 64 MiB working-state 上限，returning 仍受独立 100,000 rows／8 MiB 上限。它生成合并的逻辑 write set，并只编码其中变化的 catalog、`(table_id, row_id)`、版本化 index key 和 receipt key。format 6 在物理 key 外增加 generation envelope，内部 catalog/value/index component codec 不变。redb transaction 只删除消失的键，只写入新增或编码内容变化的键；普通数据写入不触碰 `migration_ledger`。meta 的固定版本与水位值保持同步更新。删除或覆盖时会核对 redb 中的旧值是否与 Engine 基线一致，不一致则在 commit 前明确中止并回滚事务。多语句原子脚本、旧 storage format 和需要 catalog 规范化的兼容写入仍使用完整候选。

DDL、格式升级、restore 与 receipt prune 仍走 full-rebuild 路径：重新加载 durable 前态，编码完整候选状态，再按稳定键计算差异。该路径保留同一原子提交契约，但 CPU 和峰值内存仍随完整数据规模增长。format-6 schema/data migration 改走下述 shadow-generation 路径。

## 可恢复 migration generation

format 6 的 migration runner 先持久化 Building manifest 和空 target catalog，active generation 保持不变。它从一个固定 source MVCC view 按 table stable ID、RowId 顺序读取最多 1,024 rows／16 MiB，逐批执行相同 typed migration，并在不超过 32 MiB 的同步事务中写 target rows、派生 indexes 和 durable checkpoint。manifest 同时绑定 database instance、source/target schema identity、migration ID/parent/checksum、executor version、row/index 计数、逻辑字节和 rolling digest。target generation 的 catalog、rows 与 indexes 合计最多 1 GiB。

全部 source rows 写完后，runner 有界扫描 target generation，复核 catalog digest、row/index 数量、逻辑字节、rolling digest、typed values、RowId 水位和双向 index 一致性，再把 manifest 标为 Ready。cutover 用一个 `Durability::Immediate`、two-phase transaction 同时切换 active generation、schema revision/hash、sequence，追加 ledger entry，并把旧 source 标为 Reclaimable。此事务前失败保留旧 active；commit 返回错误时结果不确定，Engine 关闭句柄并阻止继续读写，重开后只会看到完整旧状态或完整新状态。

Building、Ready 与 Aborting 阶段允许查询旧 active generation，并阻止普通 DDL/DML、receipt prune、storage upgrade 和不同 migration。完全相同的 migration 可核对 manifest 后从 checkpoint 继续；不匹配身份返回 `E_MAINTENANCE_CONFLICT`。`migration abort` 把未切换 target 标为 Aborting，并以每事务最多 1,024 entries 删除。cutover 后同样分批回收旧 source；活跃的旧 `Engine::read_snapshot` 继续持有原 redb MVCC root，直到该 snapshot 释放。generation ID 由 durable watermark 单调分配，失败或 abort 后不复用。

`Engine::open_profile`、成功 mutation 的 `MutationProfile::durable` 与成功 format-6 migration 的 `last_migration_profile` 提供不含业务值的内部诊断。open 分离 redb open、bootstrap、各内部表读取、typed `Database` 构造和逻辑验证；format-5/6 普通 open 还以 `bounded_view = true` 明确表示它没有遍历 durable rows/indexes，此时相应 entry/byte 计数为零。普通 commit 分离 prepare、transaction apply 与 sync，full rebuild 还记录前态 reload、完整后态 encode 和 diff；shadow migration 分离 prepare、build、validate、cutover 与 reclaim，并记录 generation、row/index 和 logical-byte 计数。profile 只在对应操作成功后发布；memory Engine 没有 durable profile，read-only redb 会显式标记。类型不包含 schema/field 名、key/value、cursor secret、idempotency key 或 receipt payload，也不改变 storage/catalog/value/index/backup/protocol 格式。

每张表从 0 开始单调分配 `u64` RowId，并单独持久化下一分配值。RowId 与内存 `Vec` 位置分离，索引 posting 和 redb row key 都引用 RowId；未来删除产生的缺口合法，后续插入不会复用已删除身份。打开时要求已有 RowId 严格递增且小于分配游标。第一版 redb 文件没有游标时，可从原有连续 row key 推导并在下一次写入保存；旧 snapshot 缺少显式 RowId 时按当时的 vector 顺序升级。

## 固定内部表

| 表 | 键 | 值 | 当前用途 |
| --- | --- | --- | --- |
| `meta` | UTF-8 名称 | 固定宽度数字或 UTF-8 hash | 存储格式、codec 版本、active/next generation、提交序号、schema revision、ID 水位、schema hash、cursor instance ID 与 secret |
| `catalog` | `(kind, stable_id)` | 版本化 catalog definition | format 1–5 和 format-6 Legacy0 的命名类型、用户表和索引定义 |
| `rows` | `(table_id, row_id)` | 版本 2 ADT value codec | format 1–5 和 format-6 Legacy0 的完整类型化 record |
| `secondary_index` | 版本化 `(index_id, component_count, typed_components, row_id)` | unit | format 1–5 和 format-6 Legacy0 的 ordinary/unique 有序派生记录 |
| `generation_catalog` | `(generation envelope, kind, stable_id)` | 不变的 catalog codec | generated keyspace 的 catalog；envelope 使用 `UIDG` magic、版本与非零 generation ID |
| `generation_rows` | `(generation envelope, table_id, row_id)` | 不变的 ADT value codec | generated keyspace 的完整类型化 record |
| `generation_index` | `(generation envelope, versioned index key)` | unit | generated keyspace 的有序派生记录 |
| `maintenance_generation` | generation ID | 版本化 manifest | shadow generation 状态；`UIDN` magic、codec version 与 source/target/state |
| `migration_ledger` | sequence | 版本化 migration record | `UIDM` magic、codec version、JSON entry；与 schema/data/index 同事务提交 |
| `idempotency_receipts` | 原始 UTF-8 key bytes | 版本化成功回执 | `UIDR` magic、codec version、digest、提交序号、完成时间和原 QueryResponse |

format-5/6 普通打开会验证 meta、active catalog、schema hash、ledger 单链及其 head、receipt codec/容量边界，并确认对应 row/index 表存在；它不会遍历全部 row value 或 index key。format 6 还会验证 generation ID 单调关系、manifest codec，以及 active、source、target 和 unfinished state 不矛盾。按需读取仍会拒绝触及的未知／损坏 value、index-key 或 generation-key codec。显式 `check` 才遍历 active generation 的全部 rows/indexes，验证 RowId 唯一性／顺序／分配水位和索引是否与 catalog/rows 一致。RowId 可以有删除形成的缺口。差异计划测试检查 update/delete/insert 只生成预期的 catalog/row/index 键变化；migration 测试确认 schema、数据、索引和 ledger 一起提交，普通数据提交保留 ledger。集成测试还会在一个未提交 redb transaction 修改多个内部表后直接退出子进程，确认重开只看到完整旧状态；也会在带 receipt 的 Engine commit 成功后不执行析构直接退出，确认重开能重放完整新状态。无效 redb 文件会返回 `E_STORAGE` 并保留原文件，跨进程第二个打开者返回 `E_BUSY`。

index-key codec 3 使用 catalog 中绑定的 component 类型编码完整 tuple。primitive、命名 sum/record、tuple、option、list 和有限递归 ADT 都遵循查询比较器的 total order；sum variant 与 record field 按稳定 ID 编码。UUID 使用 16-byte network order，bytes 使用 unsigned lexicographic order，descending component 对完整 prefix-free component 编码取反。任一索引键内的 bytes leaf 最多 8192 octets，完整 durable key 最多 64 KiB；建索引、写入、migration 和 restore 共用 `E_INDEX_KEY_LIMIT` 原子拒绝边界。未索引 bytes 的 value 上限为 16 MiB。

storage format 1 表示没有持久回执，format 2 增加 durable idempotency receipt。format 3 在 meta 中增加 128-bit database instance ID 和 256-bit cursor HMAC secret；format 4 增加生产标量 codec；format 5 增加有序复合索引的 catalog codec 4 与 index-key codec 3；当前 format 6 增加 generation key codec 1、maintenance codec 1 和四张固定 generation 表。打开 format 1/2 时会生成 cursor 身份并通过同步事务升级到 3；format 3→4、4→5 与 5→6 使用显式 `upgrade`。5→6 只把旧表登记为 Legacy0 并创建 envelope 表/meta，不扫描或重写数据。新库从 Generated(1) 开始。format 4 可以继续读写已有单列升序索引，但新增复合或降序 shape 会返回 `E_STORAGE_UPGRADE_REQUIRED`。升级保留逻辑 schema identity、rows、RowId、ledger、receipts、数据库/cursor 身份和 sequence。secret 不进入 introspection、日志、错误或逻辑 backup；restore 生成新身份，所以源数据库 cursor 不能用于副本。旧二进制拒绝未知的更高格式；不支持原地 downgrade，回滚依赖升级前 logical backup/restore。memory Engine 使用进程内随机身份；WAL/snapshot 兼容入口不承诺跨重启 cursor。幂等语义见 [RFC 0002](rfc/0002-idempotent-write-receipts.md)，分页语义见 [RFC 0003](rfc/0003-stable-cursor-pagination.md)，复合索引契约见 [RFC 0009](rfc/0009-ordered-composite-indexes.md)。

macOS/Linux 测试还在隔离子进程中用操作系统 `RLIMIT_FSIZE` 把 redb 文件上限固定在已提交基线大小，再写入 900,000 字节 typed text 强制触发真实文件增长失败。子进程忽略 `SIGXFSZ`，使底层写入以错误返回 Engine：若失败发生在 `commit` 前，响应明确中止且句柄允许再次尝试；若 `commit` 返回错误，响应标记结果不确定并禁用后续写。父进程重开并运行完整性检查，接受完整旧状态或完整新状态，再核对 typed row、主键索引、schema 和 migration ledger，不接受部分内部表。

这些测试覆盖应用进程退出、真实文件增长失败和库级一致性检查，没有模拟机器掉电、文件系统违反同步承诺、物理设备损坏或每一个空间不足位置。`Immediate` 与 two-phase commit 的掉电保证来自 redb 的事务契约；设备与文件系统仍必须正确实现持久同步。更广的发布环境矩阵继续由 #24 跟踪。

可重复的恢复测量工具位于 `tools/recovery-eval`。2026-09-07 的 v0.1 基线中，10,000 行 ADT 工作集 open/check 为 66/78 ms，峰值 RSS 为 52.67/81.48 MiB；100,000 行为 653/741 ms，峰值 RSS 为 459.28/745.84 MiB。当时 100,000 行检查的内存放大来自完整加载和索引验证；环境、命令、数据库大小和完整结果见[恢复成本记录](benchmarks/recovery-2026-09-07.md)。当前 v0.2 的有界结果见下方 M7 记录。

M6 使用更宽的 row 与额外复合索引重新测量完整工作负载：10k/100k 的增量单行 write p95 都约 10 ms，但 100k Engine open p95 约 5.6 s、resident query 接近 1 GiB，完整深层 migration p95 约 49.9 s、peak RSS 约 1.44 GiB。普通 write set 已不再复制整库；open/check 与 schema/data full rebuild 仍是大工作集的主要限制。完整方法、结构化访问计划和原始样本见 [M6 工作负载记录](benchmarks/workload-2026-09-09.md)。

M7 的早期 profile 定位到 full-resident open 与 full-rebuild migration 的主要成本，见 [存储阶段基线](benchmarks/storage-phases-2026-09-09.md)。完成有界 committed view 与可恢复 generation 后，100k open p95 约 12 ms，完整 check 约 1.54 s／96.33 MiB，shadow migration p95 约 16.51 s／427.98 MiB；完整分段、原始样本和接口矩阵见 [M7 验收记录](benchmarks/m7-acceptance-2026-09-10.md)。

[RFC 0010](rfc/0010-bounded-resident-state-and-maintenance-generations.md) 据此冻结物理边界：memory/redb 共用 typed row source 和 query IR；format-5 Legacy0 与 format-6 active generation committed view 绑定一个 MVCC transaction、schema/sequence/receipt root 和严格 32 MiB snapshot-local row cache，并按 RowId/index span 解码实际候选。full scan 的 filter/match/derive/select/take 逐行融合，aggregate/group 保留有界 accumulator，blocking sort 才物化受 250,000 rows／64 MiB 限制的 working buffer；NDJSON、single-statement mutation、full check 和 logical backup 都消费同一 source。format-6 已实现 shadow generation 的分批构建、验证、cutover 和 reclaim；最新 check 与 maintenance 容量见 [M7 验收记录](benchmarks/m7-acceptance-2026-09-10.md)。

## 与旧原型格式的关系

`--wal-path` 和 `--snapshot-path` 暂时保留，用于兼容早期原型。它们通过源码回放恢复，不是 redb 格式，也不会与 `--db` 双写；同一个 server 命令不能混用这两套入口。它们不能保存幂等回执；需要该保证时应先用 `import-legacy` 转换到新的 redb 路径，并继续保留原 WAL 和 snapshot 文件。
