# RFC 0011：离线 redb 文件压缩 / Offline redb file compaction

- 状态 / Status: accepted for staged implementation
- 日期 / Date: 2026-09-11
- 跟踪 / Tracking: [#230](https://github.com/worktools/unionid/issues/230), [#232](https://github.com/worktools/unionid/issues/232), [#233](https://github.com/worktools/unionid/issues/233), [#234](https://github.com/worktools/unionid/issues/234), [#235](https://github.com/worktools/unionid/issues/235)

## 中文说明

### 1. 问题与证据

format 6 的 generation reclaim 删除旧 generation 的全部 catalog、row 和 index key，但底层 redb 会保留已经分配的页。M9 固定 100k churn 工作负载中，数据库在第一次 shadow migration 前为 96.00 MiB，完成 reclaim 后为 745.21 MiB；后续两代 migration 和三轮普通 churn 没有继续增长，第六轮仍为 744.84 MiB。逻辑数据约为 52.27 MiB。reclaim 的逻辑正确性已经验证，但它不是物理 vacuum。

当前固定依赖 redb 4.1.0 提供 `Database::compact()`。它要求没有活跃 read transaction 或 savepoint，迭代搬移仍被引用的页，并通过带 maximum shrink policy 的提交缩小同一文件。它保留 redb table/key/value，不需要重建 unionid 的 schema、stable ID、RowId、ledger 或 receipt。该函数内部执行多次 two-phase commit；redb 源码明确说明，中断后可能需要在下次打开时执行完整 repair。

### 2. 决策

首版提供显式离线、原地的 `compact` 维护操作，直接使用固定版本 redb 的原生 compaction。它不会自动触发，不在普通 mutation、reclaim、open、check 或 server shutdown 中隐式运行，也不提供同命令内的路径替换。

选择原地 compact 的原因：

- 原生实现按 redb 自身页与 allocator 规则搬移数据，比 unionid 重写全部内部表更少依赖私有布局；
- 所有 durable key 保持不变，因此 database instance ID、cursor HMAC secret、sequence 和 receipt replay identity 可以保留；
- 不需要同时保留 source、logical backup 和 rebuilt destination 三份大文件；
- redb 已定义活跃 transaction/savepoint 拒绝、two-phase commit、repair 和 maximum shrink 行为。

代价是压缩不是一个 unionid 逻辑事务。进程中断或底层 storage error 可能发生在多个内部提交之间。首版通过独占、前后完整检查、保守的不确定错误和重开 repair 合同管理这一边界，不声称 Ctrl-C 是无成本取消。

### 3. 用户入口

```bash
unionid compact --db app.redb
unionid compact --db app.redb --format json
```

命令本身就是执行授权，不再增加交互确认。它只接受已存在的 redb 文件，不创建空数据库。plain 输出说明是否实际移动页面、压缩前后文件大小、回收字节、schema identity 和 storage format。JSON 使用 version 1 结构：

```json
{
  "version": 1,
  "changed": true,
  "before_bytes": 781025280,
  "after_bytes": 112000000,
  "reclaimed_bytes": 669025280,
  "schema": {
    "revision": 4,
    "hash": "sha256:..."
  },
  "sequence": 42,
  "storage": {
    "format": 6,
    "catalog_codec": 4,
    "value_codec": 2,
    "index_key_codec": 3,
    "migration_codec": 1,
    "receipt_codec": 2,
    "maintenance_codec": 1
  },
  "identity_preserved": true
}
```

`changed` 只在 durable 文件实际变小时为 true。`reclaimed_bytes` 使用 `before_bytes - after_bytes` 的饱和值；实现不能假设每次都能缩小，也不能因 `changed = false` 把成功当作错误。report 不包含路径、业务值、receipt key、database instance ID 或 cursor secret。`after_bytes` 是重开后（即用户下次打开时）的 durable 大小，而不是 native compact 返回瞬间的临时长度；实现通过在 compact 后、post-check 前重开数据库达成，详见第 8 节。

### 4. 前置条件与状态机

1. 以读写模式打开现有 redb 文件。redb 独占所有权保证运行中的 server、CLI writer 或另一个 compact 返回现有 `E_BUSY`，不等待和抢占。
2. 拒绝 read-only、memory、WAL/snapshot 和不存在的路径。首版只支持当前可写 redb Engine。
3. 运行 redb 物理检查和 unionid 逻辑完整检查。损坏输入在 compact 前失败。
4. 若 maintenance manifest 仍处于 Building、Validating、Ready、Reclaimable 或 Aborting，返回 `E_MAINTENANCE_REQUIRED`。用户必须先用相同 migration 继续、或在 cutover 前显式 abort；compact 不代替 generation cleanup。
5. 保存 schema revision/hash、commit sequence、storage/codec versions、catalog next ID、各表 next RowId、ledger、receipt 摘要和内部 database/cursor identity，用于完成后的等值检查。
6. 像 `Engine::check_integrity` 一样，把 Engine 当前 committed redb source 暂时替换为同一 metadata/receipt 的内存 view，释放本 Engine 的 read transaction。若调用方仍持有外部 read snapshot，redb 返回 transaction-in-progress，映射为 `E_BUSY`。
7. 调用 `Database::compact()`。该阶段独占 Engine 和 redb owner，不接受 query、mutation、migration、backup 或 receipt prune。
8. compact 成功后，在重新发布 committed read view 前运行完整物理／逻辑检查，并比较第 5 步的全部 unionid 身份与摘要。任一逻辑变化返回 `E_STORAGE`，关闭当前 durable handle，要求重开检查。
9. 重新建立 active generation 的 bounded committed view，读取最终文件大小并返回 report。

Engine API 使用 `compact_storage(&mut self) -> Result<StorageCompaction>`。它只适合没有并发 snapshot 的本地维护入口。`ConcurrentEngine`、TCP、HTTP 和 query language 首版不暴露 compact；服务部署必须停止 owner 后运行 CLI。

### 5. 身份与并发语义

成功 compact 必须保持：

- storage format 与全部 component codec versions；
- schema revision/hash、catalog stable IDs 与 next catalog ID；
- commit sequence、table RowId 和 next RowId；
- migration ledger、active/next generation 和空 maintenance state；
- idempotency receipts 与 replay response；
- database instance ID 和 cursor HMAC secret。

因此压缩前创建且尚未因业务 mutation 过期的 cursor 在成功压缩后仍可使用。compact 不增加 unionid commit sequence，不产生 receipt，不改变 schema，也不改变 logical backup checksum。redb 自身内部 transaction ID 可以变化，不属于公开身份。

只有调用 Engine 当前持有的 committed read transaction 会被主动释放。任何由 `read_snapshot`、stream 或 ConcurrentEngine 保留的外部 transaction 都使 compact 在搬移页面前返回 `E_BUSY`。首版不等待 reader、不取消 operation，也不把 compact 排入 service writer queue。

### 6. 失败与恢复

- 打开占用、参数错误、unfinished maintenance、preflight check、活跃 transaction/savepoint 在 redb 搬移前失败，属于确定失败。
- `Database::compact()` 的 storage error 可能发生在一个或多个内部提交之后。Engine 不继续服务，不重新发布旧内存 view，返回 `E_STORAGE_REOPEN_REQUIRED`，说明 compact 结果不确定并要求重新打开后运行 `check --db`。
- 进程被终止时没有机会返回错误。下一次正常 open 允许 redb 按固定版本规则 repair；随后 unionid 必须重新验证 format、codec、schema、ledger、rows、indexes、receipts、generation 和 maintenance state。只能接受完整、逻辑等价的数据库。
- compact 成功但 post-check 或 committed-view rebuild 失败，同样关闭当前 handle 并返回 `E_STORAGE_REOPEN_REQUIRED`。CLI 不把文件大小下降单独当作成功证据。
- 首版不提供自动 rollback 文件。用户运维指南要求 compact 前保留 verified logical backup；该 backup 用于灾难恢复，不参与正常 compact 数据路径。

CLI 沿用非查询 JSON error envelope 和 storage exit class。错误不打印底层路径以外的业务数据，不泄露 cursor identity。`doctor` 保持只读副本诊断，不执行 compact。

### 7. 实现切片与验收

1. [#233](https://github.com/worktools/unionid/issues/233) **Core storage**：RedbStore 保存规范路径并封装 native compact；DurableBackend 区分 definite busy 与 uncertain storage failure；Engine 释放／重建 committed view，验证身份并返回 versioned report。
2. [#234](https://github.com/worktools/unionid/issues/234) **CLI 与诊断**：增加 `compact --db --format`、plain/JSON 输出、help、错误 envelope 和文档；memory、WAL、read-only 入口不新增伪兼容。
3. [#235](https://github.com/worktools/unionid/issues/235) **故障与容量证据**：测试 no-op、真实 reclaim 后缩小、owner/read snapshot/maintenance 拒绝、cursor/receipt/RowId/ledger 保留、compact 子进程退出和重开 repair。稳定机器保存 10k/100k before/after/time/RSS；普通 PR 只运行 Ubuntu 小型测试，macOS 仍只在 release workflow。

完成 #230 需要证明压缩后完整 check、typed query、indexed query、cursor、idempotent replay、migration status 和 logical backup 均与压缩前一致。文件缩小幅度是观测结果，不是固定比例或 SLA。

### 8. 实现补充：durable 大小与 reopen

redb 的 `Database::compact()` 返回时文件长度小于其持久化 region 布局。此后只要在同一句柄上执行 `check_integrity`、读事务或普通打开/关闭，下一次 `Database::open` 就会运行 repair，并把文件向上取整到一个 region 边界。若直接以 compact 返回瞬间的长度作为 `after_bytes`，用户下次打开看到的文件会更大，且每次全新 compact 都会再次搬页并报告一个不会持久化的缩小，无法得到稳定 no-op。

因此实现按以下顺序执行：native compact → 释放旧句柄并重开同一路径（触发 repair）→ post-check → 发布新的 committed view。这样 repair 发生在同一次维护操作内部，`after_bytes` 等于重开后的 durable 大小；由于 redb 可能把文件向上取整到 region 边界，`changed` 只在文件实际变小时为 true，durable 大小不变即返回 `changed=false`、`reclaimed_bytes=0`。重开仍要求独占，失败按不确定错误处理并要求重开检查。reopen/repair 会完整遍历文件，所以 no-op 也可能有秒级成本，不能当作廉价轮询。

## English Description

### 1. Problem and evidence

Format-6 generation reclamation removes every catalog, row, and index key from the old generation, but redb retains allocated pages. In the fixed M9 100k churn workload, the file was 96.00 MiB before the first shadow migration and 745.21 MiB after reclamation. Two later migrations and three ordinary churn rounds did not grow it further; round six remained at 744.84 MiB while logical rows occupied about 52.27 MiB. Reclamation is logically correct, but it is not physical vacuuming.

The pinned redb 4.1.0 dependency provides `Database::compact()`. With no live read transaction or savepoint, it iteratively relocates referenced pages and uses commits with maximum shrink policy to reduce the same file. It preserves redb tables, keys, and values, so unionid does not need to rebuild schema, stable IDs, RowIds, ledger entries, or receipts. The function performs multiple two-phase commits, and redb's source explicitly notes that interruption may require a full repair on the next open.

### 2. Decision

The first version provides an explicit, offline, in-place `compact` maintenance operation backed by native redb compaction. It never runs automatically during ordinary mutation, reclamation, open, check, or server shutdown, and it does not replace filesystem paths.

Native in-place compaction follows redb's allocator and page rules, preserves every durable key and database/cursor identity, avoids maintaining another unionid internal-file rewriter, and does not require simultaneous source, logical-backup, and rebuilt-destination files. The tradeoff is that compaction is not one unionid logical transaction. Exclusive ownership, pre/post integrity checks, conservative uncertain-error handling, and repair on reopen define the safety boundary; Ctrl-C is not a cost-free cancellation contract.

### 3. Interface and report

`unionid compact --db app.redb [--format json]` operates only on an existing writable redb database. The explicit command is sufficient authorization and has no interactive confirmation. Plain and version-1 JSON output report whether pages or file size changed, before/after/reclaimed bytes, schema identity, sequence, storage/codec versions, and `identity_preserved`. Reports omit paths, business values, receipt keys, database instance IDs, and cursor secrets.

`changed` is true only when the durable file actually got smaller. `reclaimed_bytes` is saturating `before_bytes - after_bytes`. A successful no-op is valid. `after_bytes` is the durable size seen on the next open, not the transient length at which native compact returned; the implementation reopens the database after compact and before the post-check, as described in section 8. File reduction is an observation rather than a guaranteed ratio.

### 4. Lifecycle

The operation acquires normal exclusive redb ownership, runs physical and unionid logical preflight checks, rejects every unfinished maintenance phase, captures all public and internal logical identity, releases the Engine's committed redb read transaction through the same temporary memory-view pattern used by `check_integrity`, and calls native compact. Any independently retained snapshot causes a definite `E_BUSY` before relocation.

After native compact succeeds, the Engine performs another complete physical/logical check before publishing a new bounded committed view. It compares schema, sequence, storage/codec versions, stable-ID watermarks, RowId watermarks, ledger, receipt summary, generation metadata, database instance ID, and cursor secret with the preflight state. A mismatch is a storage error even if the file became smaller.

The Rust API is `compact_storage(&mut self) -> Result<StorageCompaction>`. The first version does not expose compaction through ConcurrentEngine, TCP, HTTP, or the query language. Operators stop the service owner and use the CLI.

### 5. Identity and concurrency

A successful compaction preserves storage and codec versions, schema and catalog IDs, sequence and RowIds, ledger and generation metadata, receipts and replay responses, database instance ID, and cursor HMAC secret. It does not increment the unionid sequence or change the logical-backup checksum. A cursor that was valid before compaction remains valid afterward unless an ordinary mutation had already expired it. Redb's private transaction ID may change and is not public identity.

The Engine releases only its own committed read transaction. External snapshots, streams, or concurrent readers cause a definite busy result; the first version neither waits nor cancels them and does not queue compaction behind service writers.

### 6. Failure and recovery

Ownership, arguments, unfinished maintenance, preflight validation, and live transaction/savepoint errors occur before page relocation and are definite. A storage error from native compact may follow one or more internal commits. The Engine closes the durable handle, does not republish an old memory view, and returns `E_STORAGE_REOPEN_REQUIRED`. After process termination or an uncertain error, the next open may run redb repair and must then pass complete unionid validation. Only a complete, logically equivalent database is accepted.

A post-check or committed-view rebuild failure uses the same reopen-required result. Reduced file size alone never proves success. Operators retain a verified logical backup before compaction for disaster recovery; backup/restore is not part of the normal compaction data path.

### 7. Delivery and acceptance

Implementation is split into the core RedbStore/Engine operation, CLI/report/docs, and fault/capacity evidence. Tests cover no-op compaction, real shrink after reclamation, owner/read-snapshot/maintenance rejection, preservation of cursors, receipts, RowIds, ledger, schema and indexes, process exit during compaction, repair/reopen, and logical backup. Stable-host 10k/100k observations retain before/after bytes, time, and peak RSS without SLA claims. Ordinary PRs run bounded Ubuntu checks; macOS remains release-only.

#230 is complete only when post-compaction checks, typed and indexed queries, cursor continuation, idempotent replay, migration status, and logical backup all match pre-compaction behavior.

### 8. Implementation addendum: durable size and reopen

redb's `Database::compact()` returns while the file is shorter than its persisted region layout. As soon as `check_integrity`, a read transaction, or an ordinary open/close follows on the same handle, the next `Database::open` runs repair and rounds the file up to a region boundary. Reporting the transient length at compact return would make the file look larger on the next open and would report a non-durable reduction on every fresh compact, preventing a stable no-op.

The implementation therefore runs: native compact -> release the old handle and reopen the same path (triggering repair) -> post-check -> publish the new committed view. Repair happens inside the same maintenance operation, and `after_bytes` equals the durable post-reopen size. Because redb may round the file up to a region boundary, `changed` is true only when the file actually got smaller; an unchanged durable size yields `changed=false` and `reclaimed_bytes=0`. Reopen still requires exclusive access and is treated as an uncertain, reopen-required failure if it cannot complete. Reopen/repair traverses the whole file, so even a no-op can cost seconds and is not cheap polling.
