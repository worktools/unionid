# Storage backend evaluation

这个独立工具为 #6 提供可复现的事务存储验证，不进入 unionid 主 crate 的依赖图。

每个事务同时写入 schema、catalog、row、index 和 migration ledger 五个逻辑域。工具验证：

1. 每次事务同步提交后重新打开，五个域全部可见。
2. 子进程写完五个域但未提交就退出，重新打开后全部不可见。
3. 子进程提交返回后立即退出，重新打开后五个域全部可见。
4. 在线或逻辑快照备份包含一个一致的已提交视图。
5. 后端本身是否阻止同一路径被第二个实例打开。

运行：

```bash
cd tools/storage-eval
cargo run --release -- redb /tmp/unionid-redb.db 100
cargo run --release -- sqlite /tmp/unionid-sqlite.db 100
```

输出为 JSON。`write_millis` 只用于观察本机同一工作负载的数量级，不是项目性能承诺；首次构建耗时不计入写入时间。当前 WAL/snapshot 使用主仓库 `tests/storage.rs` 的故障用例验证，不由该工具重新实现。

redb 探针显式使用 `Durability::Immediate` 与 two-phase commit；SQLite 对照探针使用 WAL、`synchronous=FULL` 和 `fullfsync=ON`。探针模拟应用进程在提交前或提交返回后立即退出，不模拟断电、磁盘缓存丢失或文件系统损坏。选型结论与完整限制见 [ADR 0001](../../docs/adr/0001-redb-storage.md)。
