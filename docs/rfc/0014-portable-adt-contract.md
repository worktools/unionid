# RFC 0014：可移植 ADT 描述与演进报告 / portable ADT descriptions and evolution reports

- 状态 / Status: accepted, core contract implemented
- 日期 / Date: 2026-09-13
- 跟踪 / Tracking: [#265](https://github.com/worktools/unionid/issues/265)

## 中文说明

### 1. 问题

unionid 已经能在 Rust、protocol v2 和持久 codec 中无损表示 ADT，但生成工具仍缺少一份稳定的机器可读输入。只暴露格式化后的 schema 源码会迫使每个 SDK 重写 parser；只暴露 Rust 类型会把 `TypeId`、serde 细节和 Rust 数值表示误当成跨语言契约。仅比较 migration 是否成功也无法回答旧查询、旧读取客户端和旧写入客户端是否仍可工作。

### 2. 决策

version 1 `SchemaDescription` 直接从当前 catalog/type IR 产生，包含 schema revision/hash、命名类型、表、主键、索引、默认值和完整类型形状。`unionid schema describe --file <schema.unid>` 描述声明文件，`--db <db.redb>` 以只读方式描述 live catalog；两者输出同一 JSON 结构。Rust API 为 `portable::describe`、`PortableContract::from_source` 和 `Engine::portable_contract`。

schema revision 以及类型、字段、变体、表和索引 ID 使用十进制字符串，避免普通 JSON number 的精度损失。主键和索引同时给出名称与稳定 field-ID path。根对象明确声明 `database_local_catalog` 和 `globally_stable: false`：ID 只在同一数据库的 migration lineage 内稳定，不是 Rust `TypeId`、应用领域 ID 或跨数据库全局 ID。命名 scalar/newtype 的领域身份由命名 type ref 表达；内部 catalog ID 不进入 serde 领域对象。

反序列化后，生成器应先调用 `SchemaDescription::validate`。它检查版本和 ID 作用域、规范 revision/hash、全局唯一的 catalog ID、ref 的 ID/名称一致性、标量约束、容器上限，以及主键/索引名称与其表内 field-ID path 的对应关系。

类型描述覆盖：

- `int` 的完整 i64 范围与有限 f64；
- UTF-8 text、128-bit UUID、date 的 day 范围；
- UTC Unix microsecond timestamp 与 microsecond duration；
- decimal precision/scale 和字符串 coefficient；
- base64url bytes 及大小上限；
- sum、record、tuple、option、list 和命名 ref。

递归类型只通过 ref 回边表达，描述本身保持有限。record 字段同时记录 default 和 `input_omittable`；`option T` 若没有显式默认值仍是必填字段，不能把缺失键静默解释为 `None`。sum payload 保留 unit、位置参数和 record payload 的区别。

### 3. 运行时校验

`PortableContract::validate_type` 接收 protocol v2 `WireValue`，随后调用现有 `Catalog::coerce`。它与 prepared binding 共用类型、默认值、decimal rescale、稳定 type/variant ID、未知字段、未知 variant 和深度限制；不实现第二个类型检查器。成功结果是补齐默认值与稳定 ID 后的规范 `WireValue`。

未知 variant 默认返回 `E_TYPE`。version 1 没有隐式 `Unknown`、降级为 `None` 或保留任意 JSON 的行为；未来只有真实透传调用方出现时才另设显式 wrapper 和演进规则。

`tests/fixtures/portable/v1.json` 是语言无关的 version 1 向量。Rust 参考 runner 覆盖 `None` / `Some(None)` / `Some(Some(v))`、空集合与缺失键、i64 两端、decimal、date/timestamp/duration 单位、bytes、三种 sum payload、未知 variant 和有界递归。未来第二语言 adapter 必须运行同一文件，不能用 TypeScript 静态声明或普通 JSON number 代替 codec 验证。

### 4. 四方向演进报告

`SchemaDescription::compare_same_catalog` 分别输出：

| 方向 | 问题 |
| --- | --- |
| `existing_data` | 旧持久值是否仍能按新 catalog 解释或由默认值补齐 |
| `query` | 旧查询源码、穷尽 match、完整 row shape、key/page 假设是否需重绑或审查 |
| `client_read` | 旧客户端能否解码新服务可能返回的值 |
| `client_write` | 旧客户端产生的值能否由新 schema 接受 |

每个方向返回 `compatible`、`conditional` 或 `incompatible`，并带稳定 code、ID 路径和说明。新增有默认字段对存量数据和旧写入方兼容，但完整 row 读取与查询 shape 需要审查；新增 variant 对旧值和旧写入兼容，但穷尽 match 与旧读取方是 conditional；保留 ID 的 rename 不改持久值，却会破坏使用旧名称的查询和 host 表示；删除或改变 value shape 为 incompatible。

已有字段移除默认值后，可能省略该字段的旧写入客户端变为 incompatible；更改默认值仍可接受旧 payload，但省略值会得到不同的规范结果，因此标记为 conditional 语义变化。

新增 unique index 或将保留 ID 的普通 index 收紧为 unique 不改变已经通过 migration 校验的存量数据，但旧客户端可能继续写入重复值，因此 `client_write` 为 incompatible。普通 index 的增删、方向和 component-only 变化不改变值的接受契约。

该方法名要求调用方保证两个快照来自同一 catalog lineage。独立数据库即使源码相同也不能用偶然相同的数字 ID 推断持久身份。#264 会在此描述上增加查询参数、结果、cardinality 和 query digest，再把 schema 级 conditional 结论收窄为具体静态查询结论。

### 5. 边界

本 RFC 不新增 wire/storage/backup codec，不改变查询执行语义，也不实现第二语言 SDK、服务器命名查询、缓存、订阅、WebSocket、UI 生命周期或业务鉴权。具体第二语言仍由 #192 在真实调用方出现后选择，优先核实 Calcit 的调用与资源生命周期，再评估 TypeScript 的采用面。

## English Description

### 1. Problem and decision

unionid already preserves ADTs through Rust, protocol v2, and durable codecs, but generators need a stable machine-readable input. Requiring every SDK to parse formatted schema source duplicates language logic, while treating Rust types, `TypeId`, serde details, or ordinary JSON numbers as the portable contract loses identity or precision. A successful migration alone also says nothing about old queries, readers, or writers.

Version 1 `SchemaDescription` is emitted directly from the current catalog/type IR. It contains schema revision/hash, named types, tables, primary keys, indexes, defaults, and complete type shapes. `unionid schema describe --file <schema.unid>` describes a declaration file; `--db <db.redb>` opens an existing live catalog read-only. The Rust APIs are `portable::describe`, `PortableContract::from_source`, and `Engine::portable_contract`.

Schema revision and all catalog IDs are unsigned decimal strings to avoid JSON-number precision loss. Primary keys and indexes carry both names and stable field-ID paths. The root explicitly says `database_local_catalog` and `globally_stable: false`: IDs remain meaningful only within one database migration lineage. They are not Rust `TypeId`, application domain IDs, or globally comparable IDs. Named scalar/newtype identity remains a named type reference without leaking catalog IDs into serde domain objects.

After deserialization, generators call `SchemaDescription::validate` before relying on a description. It checks the version and ID scope, canonical revision/hash, globally unique catalog IDs, ref ID/name consistency, scalar constraints, container limits, and the correspondence between key/index names and table-local field-ID paths.

The shape model covers full-range i64, finite f64, UTF-8 text, UUID, date, UTC Unix-microsecond timestamp, microsecond duration, decimal P/S with string coefficients, bounded base64url bytes, sums, records, tuples, options, lists, and named references. Recursive edges remain finite references. Fields record defaults and omission separately, so `option T` without a default is still required. Sum payloads preserve unit, positional, and record shapes.

### 2. Runtime validation and vectors

`PortableContract::validate_type` accepts a protocol-v2 `WireValue` and delegates to the existing `Catalog::coerce` path. Prepared binding and portable validation therefore share type rules, defaults, decimal rescaling, stable type/variant IDs, unknown-field/variant errors, and depth limits. Successful validation returns normalized wire data with defaults and stable IDs populated.

Unknown variants fail with `E_TYPE`. Version 1 has no implicit `Unknown`, conversion to `None`, or arbitrary-JSON preservation. A future caller requiring pass-through must justify an explicit wrapper and evolution contract.

`tests/fixtures/portable/v1.json` is the language-neutral version-1 suite. The Rust reference runner covers nested options, empty collections and missing keys, i64 extrema, decimal, temporal units, bytes, all sum payload shapes, an unknown variant, and bounded recursion. Any future second-language adapter must run this same file; static TypeScript declarations or ordinary JSON numbers are insufficient codec evidence.

### 3. Four-direction evolution report

`SchemaDescription::compare_same_catalog` independently reports `existing_data`, `query`, `client_read`, and `client_write` as `compatible`, `conditional`, or `incompatible`, with stable codes, ID paths, and explanations. A defaulted field preserves existing values and old writes but may change complete-row query/read shapes. A new variant preserves old values and writes while exhaustive matches and old readers become conditional. ID-preserving renames keep durable meaning but break name-based queries and host representations. Removal or value-shape changes are incompatible.

Removing a retained field default makes old writers that omit it incompatible. Changing the default keeps their payload acceptable but reports a conditional semantic change because omission now normalizes to a different value.

Adding a unique index or tightening an ID-retained ordinary index to unique leaves migration-validated existing data intact, but makes `client_write` incompatible because an old writer may still submit duplicates. Ordinary index additions/removals, directions, and component-only changes do not change value acceptance.

The method requires an explicit same-lineage assertion through its API name. Independently created catalogs cannot infer shared durable identity from coincidentally equal numeric IDs. #264 will add parameter, result, cardinality, and query-digest descriptions, narrowing schema-level conditional findings for concrete static queries.

### 4. Boundaries

This RFC changes no wire, storage, or backup codec and introduces no query execution behavior. It does not implement a second-language SDK, server-side named queries, caches, subscriptions, WebSockets, UI lifecycles, or business authorization. #192 remains caller-driven: investigate the concrete Calcit call/resource lifecycle first, then evaluate TypeScript adoption.
