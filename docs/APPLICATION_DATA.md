# Application data boundaries / 应用数据边界

## 中文说明

unionid 提供 ADT 存储、查询、索引、原子请求和 typed Rust 数据转换。独立服务和嵌入式 Engine 共用语义；Calcium 是应用集成场景，数据库不依赖该框架。方向跟踪 [#191](https://github.com/worktools/unionid/issues/191)，Calcit binding 另由 [#192](https://github.com/worktools/unionid/issues/192) 跟踪，尚未实现。

### 热摘要与冷正文

Calcium 在运行时维护有界公共/业务分区和隔离的用户热分区，一个分区的 diff 供其订阅者复用。正文、历史和大型嵌套 ADT 按需查询磁盘；持久热摘要仍存数据库。热/冷依据持续同步需要划分，不意味着热数据可以丢失。历史 ID/版本目录也需要有界窗口或分页，不能把全部元数据永久保存在运行时。

摘要与正文分表可以使摘要读取避免解码大型正文；仅对同一大行做字段投影不保证避免其底层 row decode。具体索引应通过实际 query/explain 验证。示例使用 owner 前缀和版本/ID 顺序，版本只用于演示，真实业务应选择自己的列表顺序。

```bash
cargo test --locked --example cold_content
cargo run --locked --example cold_content -- /tmp/unionid-cold-content-new.redb
```

[示例源码](../examples/cold_content.rs) 要求不存在的目标文件并保留生成的数据库。它通过 Rust enum/struct/Option/List 和 prepared 参数执行：

1. 在同一请求内 upsert 摘要和正文，二者共享应用 revision。
2. 只读取 owner 对应的最多 12 条摘要。
3. 按 ID 与认证 owner 获取正文和实际 revision；跨 owner 返回空。
4. 更新后关闭重开，重新读取摘要与正文并执行完整性检查。

示例中的 owner 参数由可信应用层提供；数据库没有内置认证，直接调用 Engine 的程序可以查询所有数据。写入 helper 假设已鉴权、单写者和应用分配 revision，不是 compare-and-swap API，也未实现幂等重试或 WebSocket。

### 提交、版本与恢复

应用先在一个数据库请求内写入内容、持久摘要和 content revision，确认成功后再发布热分区。content revision 是应用字段，不复用 schema revision、数据库维护 sequence 或分页 cursor。多个摘要读取与正文请求不承诺跨请求快照：正文返回其实际版本，应用识别迟到/过期结果；只保存最新值时不承诺返回历史版本。

提交前确定失败不发布热状态。commit 结果不确定与提交成功但 read-view 发布失败必须按 [存储契约](STORAGE.md) 处理，不能当作普通回滚重试。需要网络重试的应用使用[幂等回执](rfc/0002-idempotent-write-receipts.md)。数据库提交后、运行时发布前退出时，从持久数据重建热分区，以新的同步 epoch 重同步客户端。第一版业务写入统一经过 Calcium；外部直接写入需要另行设计失效检测。

### 后续边界与验收

Calcium 的 [#54](https://github.com/Cumulo/calcium-workflow/issues/54)、[#56](https://github.com/Cumulo/calcium-workflow/issues/56) 负责分区、鉴权、callback、客户端缓存；[#58](https://github.com/Cumulo/calcium-workflow/issues/58) 设计可序列化资源引用及可观察加载状态。资源构造/序列化/inspect 不触发数据库或网络 I/O；运行时 resolver 显式执行查询。Respo 生命周期接口在 Calcium 验证后再评估。

本轮无需数据库 CDC、订阅、物化视图、view catalog 或 change journal；有限 NDJSON 是单次查询输出，不是实时更新流。M7 的 [format-6 envelope #184](https://github.com/worktools/unionid/issues/184) 和 [可恢复维护 #185](https://github.com/worktools/unionid/issues/185) 继续独立推进。当前示例不改变 storage format 5，也不证明 Calcit 内存或网络成本。

完整集成由 [Calcium #57](https://github.com/Cumulo/calcium-workflow/issues/57) 验收：固定活跃窗口，增加冷数据量，记录数据库与语言运行时 RSS、候选/解码行数、编码/复制成本、请求次数和 diff/网络字节，并验证失败、慢客户端和重启。数据库单独基准不能替代这些证据。

## English Description

unionid owns ADT storage, queries, indexes, atomic requests, and typed Rust conversion. Standalone and embedded entry points share semantics. Calcium is an integration use case, not a database dependency. [#191](https://github.com/worktools/unionid/issues/191) tracks these boundaries; the Calcit binding in [#192](https://github.com/worktools/unionid/issues/192) is not implemented.

Calcium maintains bounded shared and private hot partitions in its runtime and reuses each partition diff across authorized subscribers. Fetch bodies/history/large ADTs on demand; persist hot summaries when durability is required. Bound historical metadata too. Separate summary and content rows prevent summary reads from decoding large bodies; projection alone does not guarantee this. Select indexes from actual plans, and choose application ordering rather than treating content revision as a universal timestamp.

Run the commands above with a fresh path. The example leaves its database for inspection. It atomically writes a summary and nested Rust ADT content, reads at most 12 owner summaries, fetches content with its actual revision, filters other owners, and verifies update/reopen/integrity. The trusted caller supplies the authenticated owner: Engine itself is not an authorization boundary. The write helper assumes an authorized single writer assigning revisions; it is not CAS, a retry implementation, or a WebSocket service.

Persist content, summary and application revision in one request before publishing hot state. Schema revisions, maintenance sequences and page cursors are separate identities. Separate requests may observe different commits; return actual content versions and handle stale responses without promising historical values. Handle uncertain commits and committed-but-reopen-required errors according to the storage contract; use existing idempotent receipts for retries. Rebuild hot partitions after commit-before-publication crashes and reset clients with a fresh epoch. Initially route business writes through Calcium; external writers need an explicit invalidation design.

Calcium #54/#56 own partitions, authorization, callbacks and caches. #58 proposes serializable Resource references and observable loading, with pure construction/serialization/inspection and explicit resolver effects. Evaluate Respo lifecycle integration after the Calcium prototype.

Database CDC, subscriptions, materialized views, view catalogs and journals are not prerequisites. Finite NDJSON streaming remains a single query. M7 #184/#185 still implement the generation envelope and resumable maintenance separately; this example does not change format 5 or establish Calcit capacity. Calcium #57 must measure full database/runtime RSS, decoded rows, conversion costs, requests and diff/network bytes with fixed active windows and growing cold datasets, including failure and restart behavior.
