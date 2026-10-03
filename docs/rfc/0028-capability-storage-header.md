# RFC 0028: 能力驱动的存储 header / Capability-driven storage header

Status: proposed; #439. No runtime support, format identifier, default or release change is introduced by this document. RFC 0027 in #460 reserves generated defaults; this contract does not implement them.

Implementation progress: strict header framing/profile validation is available internally. Existing formats reject any `storage_header` before business decoding; header installation and new-format reads/writes remain unavailable. No physical identifier is allocated. Inspection found no general metadata byte limit to reuse, so the decoder bounds bytes by the canonical shape containing all currently known capabilities and maximum-width integer fields, before allocating JSON. This is a structural bound, not a new configurable resource budget.

Manifest format 2 now declares a nonzero `record_codec` on every baseline, segment and checkpoint-retired artifact, with the chain-wide field set to zero. Format 1 retains its original chain-wide codec and exact canonical bytes/checksums. Readers resolve codecs per artifact; export/checkpoint retain version 2 when continuing such a chain and preserve existing artifact bytes. Ordinary init still writes version 1. Business workflows reject unknown record codecs before artifact decoding; record codec 2 and header transitions are not implemented yet. This is manifest evolution, not native storage upgrade support.

实现进展：内部已实现严格 framing/profile 校验。现有格式在业务解码前拒绝任何 `storage_header`；尚不支持安装 header 或读写新格式，未分配物理编号。代码检查未发现可复用的通用 metadata 字节限制，因此解码前使用「全部已知能力＋最大宽度整数」的规范结构长度作为上界，不增加可配置资源预算。

manifest format 2 已支持 baseline、segment 和 checkpoint-retired artifact 各自声明非零 `record_codec`，链级字段为零。format 1 保留原链级 codec、规范字节与 checksum。reader 逐 artifact 解析 codec，export/checkpoint 续写 v2 链时保持版本和既有 artifact 字节；普通 init 仍生成 v1。业务入口在 artifact 解码前拒绝未知 record codec；尚未实现 record codec 2 和 header transition。这是 manifest 演进，不是原生存储升级支持。

## 中文说明

### 问题与边界

#461 集中了 legacy format 1–13 的解释，#462 为 archive 增加 required-capability 入口检查。数据库仍从格式号推导能力和 journal，不能直接删除配对格式。新模型保留 redb、稳定 ID、generation key 与现有事务语义；不加入多 writer、自动降级或透明升级。不在本提案删除旧 reader/转换路径。

当前 `meta_entries/read_meta` 分别写入/读取格式和组件 codec；未知 meta key 不构成旧 reader 的拒绝边界。因此新模型需要一次明确的物理格式 discriminator，编号在实现时分配，此后仅物理 table/key/generation 布局变化才能再变更。旧 1–13 二进制先拒绝该 discriminator；支持新物理格式的 reader 则按必需能力和组件 codec 拒绝未知语义。

### 持久 header

在 `meta` 保存单个 `storage_header`：四字节 magic `UISH`、big-endian u16 header codec（首版 1），后接固定字段顺序的 UTF-8 JSON。严格字段、禁止重复 key、无空白、bounded 解码；字节上界由已知规范结构推导，不另加任意资源预算。新能力加入时需重新验证该结构上界。

| 字段 | 契约 |
|---|---|
| `physical_format` | 与 `storage_format_version` discriminator 一致；只描述物理布局 |
| `required_capabilities` | 排序、唯一的稳定名称，未知项在读取 catalog/row/receipt/journal 前拒绝 |
| `codecs` | catalog/value/index-key/migration/receipt/maintenance/journal 的独立版本 |

首版物理布局以现有 production generation/cursor/scalar 布局为基础；这些是基础要求。可安装能力复用 archive 名称 `typed_map`、`partial_unique_index`、`typed_references`。`generated_defaults` 预留但在其 codec/事务实现前必须拒绝。不能把 legacy `GENERATIONS` 位与生成默认值混为一谈。capability 仅授予使用权限，不代表 catalog 必有相关对象；drop 不移除能力，历史 receipt/journal 可能仍需要它。

移除新模型的冗余组件 meta key，不能维护两套可矛盾的 codec 来源。sequence/schema/ID/cursor/generation 元数据保留原键及身份。legacy reader 仅走原适配；新 reader 不允许 header 缺失时回退为 legacy。所有 writable/read-only/open/check/recovery 入口共用 header 校验。

已知能力映射到所需 codec，而不是新增 format：初始无额外能力为 catalog/value/index/receipt `4/3/3/2`；map 为 `5/3/4/3`；partial 为 `6/3/4/3`；reference 为 `7/3/4/3`，migration/maintenance 均为 1。首版安装 map→partial→reference 的 codec 依赖保持显式，header 同时声明依赖；未来 codec 演进独立评审，不用“大于某版本”推测支持。journal codec 独立为 0（未安装）或 1（已安装），active chain 由既有 journal state 决定。disable 不降级 codec；enable/disable 不变更物理格式或业务能力。

### 原子升级与安装

现有 `upgrade --target` 接入新物理 discriminator；不把普通写入当作升级。10/11/12/13 的 adapter 提供完整原能力、codec、journal 状态，构造等价 header，逐项验证并在同步 two-phase transaction 中转换 meta。保留 durable instance/cursor secret、schema hash/revision、RowId/next ID、receipt、ledger、generation 及所有数据；未完成 maintenance、read-only 与 uncertain handle 继续拒绝。重复升级是 no-op。更早版本保持原升级路径，不能未经验收直接“抬 header”。

能力安装是另一个显式原子操作，最小所需 codec 重写与能力声明一起提交；安装失败无能力或数据变化，不能先声明再转换。API/CLI 具体参数在实现 PR 评审，不提前宣传命令。未来 DDL 使用未安装能力需说明显式安装动作，不隐式升级。普通 DML 继续使用小写集。check 要验证实际 catalog、receipt 与 retained journal 所需能力是声明集合的子集，而非只检查 header 名称；删掉声明仍应被识别为损坏。

### 活跃链与指定 sequence 恢复

活跃链不能靠数据库单边 header 变更获得支持。当前 manifest 的 record codec 是链级固定值，commit meta 只有 sequence/schema，`catalog_codec_continues` 特判 6→7。因此新 header 必须作为受 checksum 保护的完整 commit transition 进入 journal；升级占用一个 sequence，业务 schema/hash 保持不变。记录完整 before/after header 和必要的 codec 转换数据，重放核对 before，再原子应用 after；不能按最终数据库 codec 解码升级前的 commit。

需要一次 record-codec/manifest 演进：新 manifest 为每个 artifact 声明 record codec，保留原 baseline 和 segment 的 path/bytes/checksum/range；新的 transition segment 使用新的 record codec。archive framing 尽量不变，组件版本与能力由 artifact header 声明。支持 legacy manifest 的 reader 将旧链级 codec 适配为每 artifact 值。不得改写旧 baseline/manifest 校验规则使错误数据变合法。

新 manifest 发布继续采用现有 compare/publish 契约。升级前要求当前链已 export/verify；数据库升级提交不能与外部 manifest 发布假装成跨文件事务。若数据库 commit 成功、export 尚未完成，重开/export 必须从 retained transition 确定性补齐新 manifest；未知提交结果沿用 reopen/check，不自动重试。旧 archive reader 必须在 header/record codec 或 manifest 版本处拒绝，不能静默跳过 transition。

还原到升级前 sequence 使用旧 header；升级提交及之后使用新 header。恢复候选完整验证后才发布目标，保留 receipt replay、reference/unique 约束和稳定身份。逻辑 backup 同样携带 header 并核对内容能力；旧 backup 保持读取，不在旧 JSON 里添加可能被忽略的强制语义。

### 验收与推进

1. 接入共享 header reader/writer 和 legacy adapter：未知物理/header/能力/codec、重复或缺失字段，在业务读取前拒绝；不改变新库默认。
2. 原子升级与 journal/manifest transition：覆盖提交前后中断、成功提交后未 export、混合 old/new artifact、升级前/当次/之后的 sequence，还原逐行及全部 durable identity 比对。
3. 独立 journal 开关、能力安装与 check/backup/recovery；接入 version/doctor/storage 的能力报告，再按 #404 实现 counter/default。
4. 固定业务组合而非格式笛卡尔积：map+partial+reference、receipt replay/prune、migration cutover、只读/并发 snapshots；真实旧发布二进制拒绝新库但仍能读旧库。发布默认/contract 与正式 Discussion 指南单独验收。

在以上实现验证前，本 RFC 不冻结新格式编号、公开安装命令或默认版本，不勾选 #439 完整持久阶段。

## English Description

This proposal completes the design boundary missing from #461 legacy-profile centralization and #462 archive capability checks. It retains redb, stable IDs, generations and the existing atomic writer; it adds no distributed/multiple-writer mode, automatic upgrades/downgrades or removal of legacy conversion support.

Persist one `storage_header` in meta: `UISH` magic, big-endian u16 header codec 1, then strict canonical UTF-8 JSON with `physical_format`, sorted unique `required_capabilities`, and independent catalog/value/index-key/migration/receipt/maintenance/journal codecs. Reject duplicate/unknown fields, unknown capabilities and codecs before catalog/row/receipt/journal reads in writable and read-only modes. Bound bytes by the known canonical shape before JSON allocation; revalidate the structural bound when adding a capability. Allocate the physical discriminator during implementation: old binaries reject it; later binaries reject unknown requirements instead of per-feature format numbers.

The base retains production generation/cursor/scalar requirements. Optional names match archives: `typed_map`, `partial_unique_index`, `typed_references`; reserve but reject `generated_defaults` until implemented. Generation envelopes are unrelated to generated defaults. Capabilities remain installed after schema drops because retained receipts/journals can require them. Remove redundant component meta keys for the new model; preserve sequence/schema/ID/cursor/generation identities and reject missing new headers rather than falling back to legacy.

Initial catalog/value/index/receipt codec profiles are base `4/3/3/2`, map `5/3/4/3`, partial `6/3/4/3`, reference `7/3/4/3`, with migration/maintenance 1. Declare codec dependencies explicitly, including map→partial→reference installation dependencies. Future codec revisions require explicit support, not numeric greater-than guesses. Journal codec is independently 0 or 1; active state remains in the existing journal object. Disable retains installed codecs; enable/disable changes no physical format or business capability.

Explicit upgrades adapt 10/11/12/13 and atomically install equivalent headers using synchronous two-phase commits, preserving data and all durable identities, receipts, ledgers, generations and RowIds. Reject unfinished maintenance, read-only and uncertain handles; repeating an upgrade is a no-op. Older formats retain validated existing upgrade paths. Separate explicit capability installation commits codec rewrites and declarations together; failures change neither. Public installation syntax is reviewed during implementation, with no implicit DDL upgrade. Normal DML retains small write sets. Full check compares actual catalog, receipts and retained-journal requirements with declarations and rejects cleared flags.

Active chains need more than database metadata. The current manifest fixes one record codec for the entire chain; commit metadata lacks storage headers and catalog continuation special-cases 6→7. Journal a checksum-protected complete before/after header transition plus required conversion data, consuming one sequence without changing business schema identity. Replay validates before and atomically applies after, decoding older commits with their own state rather than the final codec.

Evolve record codecs and manifests once: per-artifact record-codec declarations preserve all old baseline/segment paths, bytes, checksums and ranges; new transition segments use the new record codec. Adapt old chain-wide codecs when reading old manifests. Prefer unchanged archive framing and carry required component/capability declarations in artifact headers. Keep compare/publish semantics. Require export/verify before an active-chain upgrade; the database transaction and manifest publication are not a cross-file transaction. A committed but not exported transition must recover deterministically through reopen/export. Uncertain commits require reopen/check. Old readers reject new manifest/header/record semantics rather than skip transitions.

Sequence-selected restore uses the old header before upgrade and the new header at/after its commit, validates the complete candidate before publication and preserves receipts, references, uniqueness and identities. Logical backup also carries and validates required metadata while retaining old-backup reads; silently ignored optional JSON fields cannot enforce requirements.

Acceptance proceeds through shared header/legacy adapters, atomic upgrade and mixed old/new journal/manifest recovery, independent journal/capability installation and integrity/backup checks, then introspection and #404 counters/defaults. Prove pre/post-commit interruption, committed-but-unexported recovery, exact upgrade-boundary restores, fixed cross-feature application journeys and actual old-binary rejection. Defaults, release contracts and Discussion guidance have separate release acceptance. No new identifier, public install command or default is frozen, and #439 durable stages remain pending until implementation proves them.
