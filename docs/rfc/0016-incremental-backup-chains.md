# RFC 0016：可验证增量备份链与按 sequence 恢复 / verified incremental backup chains and sequence restore

- 状态 / Status: accepted for staged implementation
- 日期 / Date: 2026-09-13
- 跟踪 / Tracking: [#250](https://github.com/worktools/unionid/issues/250), [#303](https://github.com/worktools/unionid/issues/303), [#304](https://github.com/worktools/unionid/issues/304)

## 中文说明

### 1. 问题与不可伪造的边界

现有 logical backup format 1–4 保存一个完整、可移植的最新状态，语义可靠，但 JSON 对嵌套 ADT、字段名和类型标签重复编码，数据库较大时备份体积与恢复成本接近完整数据量。它没有增量链，也不能恢复到备份创建前后的任意 commit。

redb 和 unionid Engine 只保留最新 committed state。MVCC read transaction 是进程内临时 snapshot；关闭后不能用 sequence 重新打开。幂等 receipt 保存请求摘要与响应，不保存可重放的数据变化。migration ledger 只保存 schema migration 身份，也不是行级变更日志。因此，仅凭当前数据库、receipt 或已有 full backup，无法重建没有被记录的历史 sequence。

本 RFC 只把以下位置声明为可恢复点：一个已验证 baseline 的 sequence，以及该 baseline 之后、由连续 journal commit 覆盖并已封存到 archive segment 的每个 sequence。未启用 journal 之前、被 retention 删除之后、或 checksum chain 断裂处之后的 sequence 明确不可恢复，返回稳定错误，不做近似恢复。

### 2. 决策

采用**可选的数据库内原子 change journal + 外部 portable baseline/segment chain**：

1. `incremental init` 在数据库静止且独占时，从一个已完整检查的 committed view 写出 portable binary baseline，并完整验证临时产物。
2. baseline 与 `prepared` manifest 原子发布后，一个同步 redb transaction 启用单个 active backup chain；随后把 manifest 切为 `active`。之后每个成功的 unionid 逻辑提交都在同一个 redb transaction 中追加一条 journal commit；数据效果与 journal 不会分离。
3. `incremental export` 把尚未导出的连续 journal commits 写为一个或多个不可变 segment，校验并原子更新 archive manifest，随后才从源数据库裁剪已封存的 journal entries。
4. restore 从 baseline 开始，按 checksum/sequence 顺序重放 commit delta，停在用户指定且确实存在的 sequence，然后重建派生索引并执行完整检查。

baseline 与 segment 使用 unionid 自己的稳定 ID、RowId 和版本化 value/catalog/receipt codec，不复制 redb 页、allocator、transaction ID 或平台文件布局。这样产物可跨 Linux/macOS 和 redb 文件重排使用，同时保留 ADT nominal identity。

首版每个数据库只允许一个 active chain。复制或上传已经封存的 archive 目录由外部备份系统负责；本 RFC 不实现远端对象存储、复制、HA、CDC 或多写者。

### 3. 为什么不选择其他方案

- **在线复制 redb 文件**：运行中的文件可能跨内部提交与 repair 边界，且会绑定 redb 私有页布局；不作为受支持备份。
- **只比较两个 logical backup**：可以离线生成一次差异，但不能原子获知两次备份之间每个 commit，也不能恢复中间 sequence。
- **外部 WAL/sidecar 双写**：redb commit 与第二个文件无法组成一个原子提交；掉电可产生已提交数据但没有 delta 的静默缺口。
- **用 receipt 或 migration ledger 回放**：两者都不包含完整、规范化的数据效果。
- **默认记录所有数据库**：会让不使用增量备份的用户承担持续写放大与空间成本。journal 只由显式 init 开启。
- **把二级索引完整写入 delta**：索引是由 catalog 与 typed rows 确定的派生状态，重复记录明显放大 segment。restore 重建并逐项验证索引。

### 4. Archive 目录与版本

一个 archive 目录包含：

```text
manifest.json
baselines/
  b-<sequence>-<checksum>.uib
segments/
  s-<first>-<last>-<checksum>.uis
```

`manifest.json` 是很小的 versioned 索引，包含：

- archive format 与 record codec version；
- chain ID、源 database instance ID 的不可逆摘要、创建时间；
- 当前 baseline 的 sequence、schema identity、文件名、payload checksum 与 stored checksum；
- 按顺序排列的 segment：`first_sequence`、`last_sequence`、parent payload checksum、payload checksum、stored checksum、压缩方式和大小；
- journal policy 与当前可恢复范围；
- manifest 自身的 canonical SHA-256。

canonical manifest/header JSON 使用固定 struct 字段顺序、UTF-8、无多余空白、十进制整数和 JSON 标准字符串转义；map-like 项先按原始 UTF-8 bytes 排序。读取时拒绝重复或未知字段，而不是依赖通用 JSON object 的迭代顺序。

baseline/segment 是 length-delimited framed record stream。每个文件都有固定 magic、format version、header 长度、有界 records、trailer record count 和 SHA-256。整数使用 big-endian 固定宽度，字符串与 bytes 使用明确长度；未知 record kind 或 codec fail closed。checksum chain 基于规范的**未压缩 payload**，因此压缩库升级不会改变逻辑身份；stored checksum 另行覆盖实际文件 bytes。

archive file codec 1 的规范 framing 为：4-byte magic（baseline 为 `UIB1`，segment 为 `UIS1`）、big-endian `u16 archive_version`、`u16 record_codec`、`u8 compression`、3 个必须为零的 reserved bytes、`u32 header_len`、canonical UTF-8 JSON header、随后是压缩或未压缩的 frame stream。每个 frame 为 `u8 kind | u32 payload_len | payload`。baseline kind 固定为 meta=1、catalog=2、row=3、migration=4、receipt=5、end=255；segment kind 固定为 commit-begin=16、catalog-delete/write=17/18、row-delete/write=19/20、migration-delete/write=21/22、receipt-delete/write=23/24、commit-end=25、end=255。delete payload 是 `u32 key_len | key`，write payload 是 `u32 key_len | key | u32 value_len | value`；key/value 分别沿用 manifest 声明的 stable-key/content codec。end frame 保存此前 canonical header 与未压缩 frames 的 record count、expanded bytes 和 SHA-256，不把自身 checksum 字段纳入 hash。reserved、顺序、唯一性或终止位置不规范均拒绝。

archive codec 1 支持 `none` 与 `zstd` transport encoding，默认使用固定参数的 zstd level 3。解压器同时限制 stored bytes、expanded bytes、单 record、record count、ADT depth 和总 rows，不能用压缩包绕过现有资源上限。golden vectors 固定 header、规范 payload 和 checksum；不把特定 zstd 实现生成的完整压缩 bytes 当作永久 golden。

文件先写同目录唯一 temp，flush、`sync_all`、重新读取并验证，再 rename 到 checksum 命名的最终路径并 sync 目录。不可变文件若已存在，只在两个 checksum 与长度都一致时视为幂等重试；否则报 chain conflict。

### 5. Baseline 内容

baseline 保存恢复一个逻辑状态所需的非派生 durable 数据：

- `DurableMeta` 中的 sequence、schema revision/hash 与 stable ID 水位，但不复制 cursor secret；
- 按 stable ID 排序的 type/field/variant/table/index catalog definitions 与各表 next RowId；
- 按 `(table_id, RowId)` 排序的 typed row value bytes；
- 按 ledger sequence 排序的 migration entries；
- 按 key bytes 排序的完整 idempotency receipts；
- archive codec 和各内容 codec versions。

baseline 不保存 secondary-index postings、redb generation/allocator 状态、active maintenance generation、operation capability、metrics、trace 或 compact proof。开始 init 前必须没有 unfinished migration maintenance；restore 根据 catalog 与 rows 确定性重建索引。

增量 restore 默认生成新的 database/cursor instance identity，与现有 logical restore 一致。schema、stable IDs、RowIds、水位、migration ledger、receipts、业务 sequence 和 typed values 保持目标 sequence 的状态；旧 cursor 明确失效，恢复库不会与仍存活的源库共享 cursor HMAC secret。恢复后的数据库不自动延续原 active chain；需要继续增量备份时创建新 baseline 与 chain ID。

### 6. 数据库内 journal

实现引入显式 storage format 7 和两个内部表：

| 表 | 键 | 值 | 用途 |
| --- | --- | --- | --- |
| `backup_chain_state` | singleton | versioned chain state | chain ID、baseline/export sequence、parent checksum、limits、状态 |
| `backup_journal` | `(sequence, ordinal)` | versioned delta record | 一个逻辑 commit 的有序 meta/catalog/row/ledger/receipt 变化 |

`incremental init` 是显式格式变更授权：它可以在 baseline 和 `prepared` manifest 验证并发布后，把 format 6 原子升级到 format 7 并启用 journal；CLI 必须报告 previous/current format，旧二进制会按既有规则拒绝 format 7。若在 enable commit 前退出，留下的是未激活 archive，可由下一次 init 核对后继续或显式删除；数据库没有不完整 journal。若 enable 已提交但 manifest 还未切为 `active`，baseline 已经 durable，下一次 init/export 根据两侧相同 chain ID 完成激活。chain ID、baseline sequence 或 checksum 任一不符都报 conflict，不能自动挑选一侧。

外部 manifest 状态为 `prepared | active | sealed`，数据库状态为 `disabled | active`。合法对账只有 prepared+disabled（可重试 enable）、prepared+相同 active（完成 manifest 激活）、active+相同 active（正常）、sealed+disabled（只读 archive）。其他组合均返回 `E_BACKUP_CHAIN`。init/enable 不改变业务 sequence；baseline sequence `S` 的下一次真实逻辑提交必须是 `S+1`。

每个 journal commit 具有一个 header 和若干 records：

- `sequence` 必须等于上一逻辑状态 sequence + 1；
- `parent_commit_checksum` 与 `commit_checksum` 形成逐 commit chain；`commit_checksum = SHA-256(domain || parent || canonical header || canonical delta records)`，只处理本次 delta，不扫描全库；baseline payload checksum 是第一个 parent；
- meta before/after（不包含新的 cursor secret）；
- catalog delete/write，按 stable catalog key 排序；
- row delete/write，按 `(table_id, RowId)` 排序，value 使用当时声明的 codec；
- migration-ledger append/change；
- receipt delete/write；
- record count、expanded bytes 和每类 count。

普通 insert/upsert/update/delete 直接复用已经计算的 stable-key `PreparedDelta`。DDL 与非 generation migration 使用完整前后 diff。format-6/7 shadow migration 的 Building/Validating/Reclaiming 是物理 maintenance，不产生可见 sequence；只有 cutover 发布新 schema/rows/ledger 的那一个逻辑 sequence 形成 journal commit。compact、完整 check、export 后 journal prune 和 redb repair 不产生逻辑 commit，也不进入 archive delta。

一个成功的原子脚本只产生一个 journal commit，即使内部含多个 DDL/DML statements；失败脚本、只读请求、explain、幂等 replay 和没有新 effect 的 retry 都不产生记录。receipt 首次与 effect 一同创建时进入同一 delta，后续 receipt prune 作为其自己的 sequence 记录 delete。

secondary-index postings 不进入 journal。commit 前必须证明 after-state 的 typed rows 与 catalog 可确定性重建相同索引；restore 最终执行完整索引验证。

journal record 与业务 delta 在同一个 `Durability::Immediate`、two-phase redb transaction 中写入。编码、大小或 journal capacity 检查发生在 transaction mutation 前。commit 成功意味着两者都存在；确定失败意味着两者都不存在；不确定错误沿用 `E_STORAGE_REOPEN_REQUIRED`，重开后根据 sequence 与 journal header 只接受完整 old/new state。

### 7. 有界容量与可用性

journal 是严格有界的，不允许静默丢 commit：

- 默认最多 10,000 个未封存 commits 或 64 MiB expanded delta，以先到者为准；
- 用户可在 init 时降低限制，或提高到实现硬上限 100,000 commits / 1 GiB；
- 单 commit delta 也受 1 GiB 和现有行/值/ADT 深度上限约束；
- commit admission 计算加入后的精确 encoded/expanded bytes，不能只按行数估算。

若新 commit 会超过限制，整个 mutation 在发布前以 `E_BACKUP_JOURNAL_FULL` 确定失败。错误报告当前 first/last sequence、entries/bytes 与建议运行 export，不包含业务 key/value。系统不会为了可用性自动丢旧 delta；调用方必须 export，或显式 disable 并接受链在当前 head 结束。

journal 增加的同步写字节与耗时进入 mutation profile 的独立 value-free 字段。#304 必须对小写入、100-row batch、receipt prune 和 migration cutover 保存 p50/p95 与 bytes，不把开启 journal 后的成本隐藏在普通 storage 时间内。

### 8. Export 与跨文件原子性

`incremental export` 在同一个 Engine owner 内按以下顺序执行：

1. 核对 archive manifest、数据库 active chain、baseline、last exported sequence 和 parent checksum。
2. 从一个稳定 journal view 读取完整连续 range；发现 sequence/ordinal 缺口立即失败。
3. 写、sync、重读并验证 immutable temp segment，再按 checksum 名发布并 sync archive 目录。
4. 原子重写并 sync manifest，把 segment 加入 chain。此时外部 archive 已经完整拥有这些 commits。
5. 用一个 redb transaction 更新 exported head 并删除已封存 journal entries；该物理 prune 不改变业务 sequence，但必须使旧 compact proof 失效。

在步骤 3 前退出不会发布 segment。步骤 3 后、4 前可能留下 orphan immutable 文件；list 只读取 manifest 并忽略它，verify 扫描 archive 目录并报告它。步骤 4 后、5 前数据库仍保留重复 journal entries，下一次 export 通过 checksum 幂等完成 prune。绝不允许先删 journal 再发布 manifest。

一个 segment 默认最多 1,000 commits 或 64 MiB expanded payload；大 commit 可以独占一个 segment但不能超过硬上限。export 可用 `--through-sequence` 限定到已提交 head，不能等待未来 commit。

### 9. Restore、verify 与 list

建议的兼容 CLI：

```text
# 现有 logical 命令保持不变
unionid backup --db app.redb --output app.backup.json
unionid restore --backup app.backup.json --db restored.redb

# 新的增量模式
unionid backup incremental init --db app.redb --repo backups/
unionid backup incremental export --db app.redb --repo backups/
unionid backup incremental list --repo backups/ --format json
unionid backup incremental verify --repo backups/
unionid restore incremental --repo backups/ --db restored.redb --at-sequence 42
```

Rust API 放在 `backup::incremental`，提供对应 `init`、`export`、`list`、`verify`、`restore` 和有界 `prune`；所有返回结构带独立 version。CLI 与 API 使用相同核心，不通过 query language 暴露文件运维。

`list` 只读取 bounded manifest，报告 baseline、segments、可恢复的 inclusive sequence range、缺口、总 stored/expanded bytes，不读取业务 records。`verify` 流式读取所有被引用文件，检查 magic/version/limits、stored/payload checksum、parent chain、sequence 连续性和 schema transition；默认不创建数据库。

`restore` 必须先验证目标 sequence 在 manifest 声明范围内，再完整验证到该点所需的 baseline/segments。低于 baseline 返回 `E_BACKUP_BEFORE_BASELINE`；高于 sealed head 返回 `E_BACKUP_AFTER_HEAD`；chain 缺口/分叉返回 `E_BACKUP_CHAIN`。它只接受不存在的目标路径，在同目录临时 redb 中顺序应用 delta，重建 indexes、验证 schema hash/ledger/receipts/RowId、运行 full check，再原子 rename 和 sync 目录。失败删除 temp，不覆盖目标、archive 或源数据库。

### 10. Retention 与 checkpoint

源数据库中的已 export journal 可按第 8 节自动裁剪；archive retention 必须显式执行。首版用 checkpoint 建立新的完整 baseline：

1. 在当前 sealed head 或更后的数据库 head 创建、验证并发布新 baseline；若还有未 export commits，先 export。
2. 原子更新 manifest，使新 baseline 成为 retained floor，并验证从它到 head 的完整链。
3. `prune --before-sequence N` 先 preview 将删除的 baselines/segments、释放 bytes 和新的最早恢复点；只有显式 `--confirm` 才删除不再被 manifest 引用的文件并 sync 目录。

不能删除仍被任一 retained restore point 依赖的 baseline/segment。删除失败保留 manifest 引用或报告 orphan，不制造一份声称完整但缺文件的 archive。首版不按 TTL/LRU 自动降低恢复范围。

`disable` 封存到当前 exported head；如果数据库仍有未 export commits则拒绝，要求先 export 或显式 `--discard-unexported --confirm`。discard 是有数据恢复能力损失的操作，必须报告失去的 sequence range；业务数据本身不回滚。

### 11. 安全、隐私与错误

archive 包含完整业务数据与 receipt response，不做脱敏。新文件按平台能力创建为 owner-only；远端加密、KMS 与访问控制交给部署环境。SHA-256 chain 用于完整性和误操作检测，不是对拥有写权限攻击者的认证；文档不得把 checksum 称为签名。

manifest 只允许规范的相对文件名；拒绝绝对路径、`..`、路径分隔符注入、符号链接和逃出 archive root 的解析结果。init/restore 不跟随目标或 temp symlink，也不覆盖任何已有目标。

错误与 JSON report 不打印 row、ADT value、receipt key/response、cursor secret 或完整 database instance ID。可以报告 hash、sequence、count、bytes、文件相对名和稳定错误码。路径按现有 CLI 规则出现。

所有 decode 都先执行 manifest/file/record/expanded-size 限制，再分配内存。未知 format/codec、non-canonical ordering、重复 key、sequence 回退、checksum mismatch、schema hash mismatch 和非法 typed value 均 fail closed。

### 12. Compatibility 与升级

- logical backup format 1–4 与现有 `backup`/`restore` flags 原样保留；incremental archive 使用独立 magic 与版本，不伪装成 format 5 JSON。
- format 6 数据库仍可读写；只有显式 `backup incremental init` 才升级到 format 7 并承担 journal 成本。
- format 7 的普通写入不能由旧二进制打开。`version`/`doctor`/`.storage` 报告 journal codec、active state、unexported range/count/bytes，但不显示 chain secret 或业务数据。
- active chain 期间的 storage upgrade 或 codec rewrite 返回 `E_BACKUP_CHAIN_ACTIVE`，要求先 export、checkpoint 并 seal；upgrade、restore 不跨 archive chain 隐式继续。若将来的 codec 能逐 record 解码，可通过新 baseline 开新 chain；旧 archive 始终由声明支持其 archive/content codec 的二进制恢复。
- compact/check/backup logical 对 format 7 保持可用；所有非逻辑的 format/journal/prune 写入必须显式使 compact proof 失效。

### 13. 实现顺序与验收

#304 按以下顺序交付，避免先暴露不可恢复的 CLI：

1. 冻结 archive/journal codec、golden vectors、limits 与 format-7 tables。
2. 将 journal delta 原子接入 incremental/full rebuild/migration cutover/receipt prune，并完成 old/new crash matrix 与容量满回滚。
3. 实现 init/export/list/verify 与跨文件中断恢复。
4. 实现 restore-to-sequence、index rebuild、fresh cursor identity、chain errors 与 logical v1–v4 回归。
5. 实现 checkpoint/prune/disable，补齐中英 CLI/BACKUP/UPGRADING 文档。
6. 普通 PR 只运行 bounded Ubuntu 测试；v0.5 release candidate 才运行 Linux/macOS 10k/100k logical-vs-incremental size、create/export/restore、p50/p95、peak RSS 和故障 evaluator。

完成验收需要证明每个 manifest 声明的 sequence 都可恢复为相同 schema、stable IDs、RowIds、水位、typed rows、ledger、receipts 和重建后的 indexes，并通过完整 check；缺失或超出范围的 sequence 必须明确失败。代表性小变更 segment 必须显著小于完整 logical backup，同时量化启用 journal 对正常写入的成本。

## English Description

### 1. Problem and honest recovery boundary

Logical backup formats 1–4 capture one complete portable latest state. They are semantically reliable, but JSON repeats nested ADT tags, field names, and type structure, so size and restore work approach the full dataset. They provide no incremental chain and no way to restore arbitrary commits around the backup.

redb and Engine retain only the latest committed state. An MVCC read transaction is a temporary in-process snapshot and cannot be reopened by sequence after shutdown. Idempotency receipts retain request digests and responses rather than replayable data effects, while the migration ledger records schema migration identity rather than row changes. Historical sequences that were never recorded therefore cannot be reconstructed from the current database, receipts, or a full backup.

This RFC declares only two kinds of restore points: the sequence of a verified baseline, and each sequence after it covered by contiguous journal commits sealed into archive segments. A sequence before journal enablement, after retention removed it, or beyond a checksum-chain gap is explicitly unavailable and returns a stable error rather than an approximate state.

### 2. Decision

Use an **optional transactionally embedded change journal plus an external portable baseline/segment chain**.

`incremental init` exclusively opens a quiescent database, writes a portable binary baseline from a fully checked committed view, and verifies the temporary artifact. After the baseline and a `prepared` manifest are atomically published, one synchronous redb transaction enables a single active chain and the manifest advances to `active`. Every later successful logical unionid commit appends one journal commit in the same redb transaction as its data effect. `incremental export` seals contiguous unexported commits into immutable segments, verifies and publishes the archive manifest, and only then prunes exported source-journal entries. Restore starts from the baseline, replays checksum- and sequence-ordered deltas through an actually retained target, rebuilds derived indexes, and performs a full check.

Artifacts use unionid stable IDs, RowIds, and versioned value/catalog/receipt codecs. They never copy redb pages, allocator state, private transaction IDs, or platform file layout. The chain remains portable across Linux/macOS and physical redb rearrangement while preserving nominal ADT identity.

The first version permits one active chain per database. External backup systems may copy or upload sealed archive directories. Remote object storage, replication, HA, CDC, and multi-writer operation remain outside this RFC.

### 3. Rejected alternatives

- Copying a live redb file can cross internal commit and repair boundaries and binds recovery to private page layout.
- Diffing occasional logical backups cannot atomically observe every commit or recover intermediate sequences.
- A separately written WAL/sidecar cannot atomically commit with redb and may silently miss an already committed effect after power loss.
- Receipts and the migration ledger do not contain canonical complete data effects.
- Always-on history would impose write and space cost on users who do not use incremental backup; journaling starts only through explicit init.
- Recording secondary-index postings duplicates derived state. Restore deterministically rebuilds and verifies them from catalog and typed rows.

### 4. Archive layout and versioning

An archive directory contains a small versioned `manifest.json`, immutable `baselines/b-<sequence>-<checksum>.uib` files, and immutable `segments/s-<first>-<last>-<checksum>.uis` files. The manifest records archive/record codecs, chain ID, a one-way digest of the source database instance, baseline sequence/schema/file/checksums, ordered segment ranges and parent/payload/stored checksums, compression and sizes, journal policy, recoverable range, and its own canonical SHA-256.

Canonical manifest/header JSON uses fixed struct field order, UTF-8, no insignificant whitespace, decimal integers, standard JSON string escaping, and raw-UTF-8 byte ordering for map-like entries. Readers reject duplicate and unknown fields rather than depending on a generic JSON object's iteration order.

Baseline and segment files are length-delimited framed streams with fixed magic, version, bounded records, record count, and trailer checksum. Integers use fixed-width big-endian representation and strings/bytes have explicit lengths. Unknown kinds/codecs fail closed. The checksum chain covers canonical uncompressed payload, so compression-library changes do not change logical identity; a separate stored checksum covers actual bytes.

Archive file codec 1 uses this normative framing: four-byte magic (`UIB1` baseline or `UIS1` segment), big-endian `u16 archive_version`, `u16 record_codec`, `u8 compression`, three zero reserved bytes, `u32 header_len`, a canonical UTF-8 JSON header, and then a compressed or uncompressed frame stream. A frame is `u8 kind | u32 payload_len | payload`. Baseline kinds are meta=1, catalog=2, row=3, migration=4, receipt=5, end=255. Segment kinds are commit-begin=16, catalog-delete/write=17/18, row-delete/write=19/20, migration-delete/write=21/22, receipt-delete/write=23/24, commit-end=25, end=255. Delete payloads are `u32 key_len | key`; writes are `u32 key_len | key | u32 value_len | value`, using the stable-key/content codecs named by the manifest. The end frame records count, expanded bytes, and SHA-256 over the preceding canonical header and uncompressed frames, excluding its own checksum field. Nonzero reserved bytes, noncanonical order, duplicate keys, or misplaced termination fail closed.

Archive codec 1 supports `none` and `zstd`, defaulting to fixed level 3. Decoding limits stored and expanded bytes, records, individual records, rows, and ADT depth. Golden vectors fix headers, canonical payloads, and checksums without treating one zstd implementation's complete compressed output as permanent. Files are written to unique sibling temporaries, flushed, synced, reopened and verified, checksum-named, renamed, and followed by directory sync. An existing immutable name is an idempotent retry only when both checksums and length agree.

### 5. Baseline and restored identity

A baseline stores non-derived durable state: sequence/schema/stable-ID watermarks without the cursor secret; stable-ID-ordered type/field/variant/table/index catalog and table RowId watermarks; `(table_id, RowId)`-ordered typed values; ordered migration entries; ordered complete idempotency receipts; and archive/content codec versions. It excludes index postings, redb generation/allocator state, unfinished maintenance, operation capabilities, metrics/traces, and compaction proofs. Init rejects unfinished migration maintenance.

Incremental restore creates fresh database and cursor identity, matching logical restore. Schema, stable IDs, RowIds and watermarks, ledger, receipts, business sequence, and typed values match the selected point. Old cursors expire and a restored clone never shares the live source's cursor HMAC secret. Restore does not silently continue the source chain; a new baseline and chain ID are required.

### 6. Transactional source journal

Storage format 7 adds singleton `backup_chain_state` and ordered `backup_journal(sequence, ordinal)` tables. Explicit init may upgrade format 6 to 7 only after publishing a verified baseline and must report the old/new format. A crash before enable leaves a harmless inactive archive; after enable, the durable baseline already exists and chain ID reconciliation can resume.

External manifest states are `prepared | active | sealed`; database states are `disabled | active`. Valid reconciliation pairs are prepared+disabled (retry enable), prepared+the same active chain (finish manifest activation), active+the same active chain (normal), and sealed+disabled (read-only archive). Every other pair returns `E_BACKUP_CHAIN`. Init/enable does not change business sequence, and the first logical commit after a baseline at `S` must be `S+1`.

Each journal commit requires exactly previous sequence + 1 and contains parent/commit checksums, meta before/after without a new cursor secret, stable-key-sorted catalog and row deletes/writes, ledger changes, receipt changes, and bounded counts/bytes. The commit checksum is `SHA-256(domain || parent || canonical header || canonical delta records)`, anchored by the baseline payload checksum, so normal commit work is proportional to the delta and never hashes the full database. Ordinary mutations reuse `PreparedDelta`; DDL and non-generation migration can use a complete before/after diff. Shadow migration work is physical and invisible until its one sequence-advancing cutover, which records the complete logical delta. Compaction, checks, export pruning, and repair do not enter the logical archive.

The journal record and business delta share one Immediate, two-phase redb transaction. Encoding and capacity admission precede mutation. Success contains both; a definite failure contains neither; an uncertain result requires reopen and accepts only a complete old/new sequence-plus-journal pair.

One successful atomic script produces one journal commit even when it contains multiple DDL/DML statements. Failed and read-only requests, explain, idempotent replay, and retries with no new effect produce none. A newly created receipt is part of the same effect delta; later receipt pruning records deletes in its own sequence.

### 7. Bounded capacity and availability

The default unsealed journal limit is 10,000 commits or 64 MiB expanded delta. Init may configure lower values or increase them up to 100,000 commits / 1 GiB. A single commit remains bounded by 1 GiB and existing row/value/depth limits. Admission uses exact encoded/expanded bytes.

A commit that would exceed the bound fails completely with `E_BACKUP_JOURNAL_FULL`, reporting value-free range/count/bytes and suggesting export. unionid never silently discards history to preserve write availability. Operators export or explicitly disable and accept a closed chain. Mutation profiles expose journal sync bytes/time separately; acceptance measures small writes, 100-row batches, receipt pruning, and migration cutover.

### 8. Export atomicity

Export verifies database/archive chain identity and reads a complete contiguous stable journal range. It writes, syncs, reopens, and verifies immutable segments; atomically updates and syncs the manifest; and only then updates the source exported head and deletes covered journal entries in one redb transaction. That physical pruning does not change business sequence but invalidates any old compaction proof.

A crash before segment publication leaves nothing; one between segment and manifest may leave an unreferenced immutable file that list ignores and verify reports; one after manifest but before source pruning leaves safe duplicates that a checksum-idempotent retry removes. Journal entries are never deleted before the manifest owns them. A segment defaults to 1,000 commits or 64 MiB expanded payload, while one oversized but valid commit may occupy a segment alone. `--through-sequence` can bound export at an already committed head and never waits for future work.

### 9. User interfaces and restore

Existing logical CLI remains unchanged. Incremental commands use `backup incremental init/export/list/verify` and `restore incremental --at-sequence`; Rust exposes the same core through `backup::incremental`. Every report has its own version. The query language does not expose filesystem maintenance.

`list` reads only the bounded manifest and reports the baseline, segments, inclusive recoverable range, and stored/expanded bytes without business records. A manifest with a gap or fork is rejected while decoding rather than returned as a partially usable list. `verify` streams every referenced artifact and validates formats, limits, both checksums, parent chain, sequence continuity, and schema transitions without creating a database.

Restore validates availability first, then verifies every required artifact, applies the baseline and deltas to a sibling temporary redb, rebuilds indexes, validates schema/ledger/receipts/RowIds, runs the full check, and atomically publishes a nonexistent target followed by directory sync. A target below baseline returns `E_BACKUP_BEFORE_BASELINE`; one above sealed head returns `E_BACKUP_AFTER_HEAD`; a gap or fork returns `E_BACKUP_CHAIN`. Failure removes the temporary target and never changes the archive, source, or existing destination.

### 10. Retention, safety, and compatibility

Source entries may be pruned only after export. Archive retention first creates and verifies a new checkpoint baseline, atomically advances the retained floor, and then lets an explicit preview/`--confirm` prune remove unreferenced older artifacts. It never removes a file needed by a retained restore point and has no automatic TTL/LRU. Disable rejects unexported commits unless an explicit confirmed discard reports the lost range.

Archives contain business values and receipt responses. Files use owner-only permissions where supported; encryption, KMS, and remote access control belong to deployment. SHA-256 detects corruption and chain mistakes but is not a signature against an attacker with write access. Reports omit values, receipt data, cursor secrets, and full database instance IDs. All decoders enforce size/count/depth limits before allocation and reject unknown, non-canonical, duplicated, regressing, checksum-invalid, schema-invalid, or ill-typed input.

Manifest entries must be canonical relative filenames. Absolute paths, `..`, separator injection, symlinks, and any resolution outside the archive root are rejected. Init and restore do not follow destination or temporary symlinks and never overwrite an existing target.

Logical backup formats 1–4 remain unchanged. Format 6 remains readable/writable; only explicit incremental init upgrades to format 7 and enables its cost. Old binaries reject format 7. Version/doctor/introspection expose journal codec and value-free backlog status. Upgrades, restores, and codec rewrites never continue a chain implicitly. Format-7 compact/check/logical backup remain supported, and every non-logical storage/journal mutation explicitly invalidates compaction proof state.

An active chain rejects storage upgrades and codec rewrites with `E_BACKUP_CHAIN_ACTIVE` until the operator exports, checkpoints, and seals it. Restore always starts a new database identity and leaves journaling disabled.

### 11. Delivery and acceptance

#304 delivers frozen codecs/golden vectors and format-7 tables; atomic journaling on incremental/full/migration/receipt paths with crash and capacity rollback tests; init/export/list/verify and cross-file interruption recovery; restore-to-sequence with fresh identity and index rebuild; then checkpoint/prune/disable and bilingual documentation.

Ordinary PRs run bounded Ubuntu tests only. The v0.5 release candidate runs Linux/macOS 10k/100k comparisons for logical versus incremental size, init/export/restore time, p50/p95 write overhead, peak RSS, and fault cases. Acceptance proves every declared sequence restores identical schema, stable IDs, RowIds/watermarks, typed rows, ledger, receipts, and rebuilt indexes through a full check; unavailable points fail explicitly; representative small-change segments are materially smaller than a complete logical backup; and normal-write journal cost is measured rather than hidden inside aggregate storage time.
