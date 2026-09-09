# Application data boundaries / 应用数据边界

## 中文说明

unionid 提供 ADT 存储、查询、索引、原子请求和 typed Rust 数据转换。独立服务和嵌入式 Engine 共用语义。应用框架的缓存、网络同步和界面生命周期不属于数据库核心。该边界已由 [#191](https://github.com/worktools/unionid/issues/191) 确认；可选宿主语言适配由 [#192](https://github.com/worktools/unionid/issues/192) 按真实需求评估。

### 热摘要与冷正文

应用可以在运行时维护有界摘要，将正文、历史和大型嵌套 ADT 保存在数据库并按需查询。需要恢复的摘要仍应持久化。历史 ID/版本目录也需要有界窗口或分页，不能把全部元数据永久保存在运行时。

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

应用先在一个数据库请求内写入内容、持久摘要和 content revision，确认成功后再更新运行时状态。content revision 是应用字段，不复用 schema revision、数据库维护 sequence 或分页 cursor。多个摘要读取与正文请求不承诺跨请求快照：正文返回其实际版本，应用识别迟到/过期结果；只保存最新值时不承诺返回历史版本。

提交前确定失败不更新运行时状态。commit 结果不确定与提交成功但 read-view 发布失败必须按 [存储契约](STORAGE.md) 处理，不能当作普通回滚重试。需要重试的应用使用[幂等回执](rfc/0002-idempotent-write-receipts.md)。数据库提交后、运行时更新前退出时，从持久数据重建应用状态。多个写入入口之间的缓存失效由应用负责。

### 核心边界

数据库不负责应用缓存、WebSocket、客户端 diff、订阅或组件 Resource 生命周期。有限 NDJSON 是单次查询输出，不是实时更新流。若未来真实数据库场景需要 CDC 或物化视图，应以独立证据和 RFC 重新进入计划。

M7 当前专注 [format-6 envelope #184](https://github.com/worktools/unionid/issues/184)、[可恢复维护 #185](https://github.com/worktools/unionid/issues/185) 和 [容量验收 #186](https://github.com/worktools/unionid/issues/186)。当前示例使用二进制默认创建的 storage format，不构成宿主语言或网络层容量证明。

## English Description

unionid owns ADT storage, queries, indexes, atomic requests, and typed Rust conversion. Standalone and embedded entry points share semantics. Application caches, network synchronization, and UI lifecycles are outside the database core. [#191](https://github.com/worktools/unionid/issues/191) records this boundary; [#192](https://github.com/worktools/unionid/issues/192) evaluates optional host-language adapters only when demanded by a real caller.

Applications may maintain bounded summaries in memory and fetch bodies, history, or large ADTs from the database on demand. Persist summaries when recovery requires them, and bound historical metadata. Separate summary and content rows prevent summary reads from decoding large bodies; projection alone does not guarantee this. Select indexes from actual plans, and choose application ordering rather than treating content revision as a universal timestamp.

Run the commands above with a fresh path. The example leaves its database for inspection. It atomically writes a summary and nested Rust ADT content, reads at most 12 owner summaries, fetches content with its actual revision, filters other owners, and verifies update/reopen/integrity. The trusted caller supplies the authenticated owner: Engine itself is not an authorization boundary. The write helper assumes an authorized single writer assigning revisions; it is not CAS, a retry implementation, or a WebSocket service.

Persist content, summary and application revision in one request before updating runtime state. Schema revisions, maintenance sequences and page cursors are separate identities. Separate requests may observe different commits; return actual content versions and handle stale responses without promising historical values. Handle uncertain commits and committed-but-reopen-required errors according to the storage contract; use existing idempotent receipts for retries. Rebuild application state after a commit-before-runtime-update crash. Applications own cache invalidation across multiple writers.

Application caches, WebSockets, client diffs, subscriptions, and component Resource lifecycles remain outside this repository's core plan. Finite NDJSON streaming is a single query. A future CDC or materialized-view proposal requires independent database evidence and an RFC.

M7 #184/#185/#186 implements the generation envelope, resumable maintenance, and database capacity acceptance. This example uses the binary's default storage format and does not establish host-runtime or network capacity.
