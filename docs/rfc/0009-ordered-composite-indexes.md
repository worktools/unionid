# RFC 0009：有序复合索引与范围访问

- 状态：proposed
- 日期：2026-09-08
- 跟踪：[M6 #167](https://github.com/worktools/unionid/issues/167)、[设计 #164](https://github.com/worktools/unionid/issues/164)、[实现 #165](https://github.com/worktools/unionid/issues/165)

## 中文说明

### 1. 问题与决策

当前索引只绑定一个 field path，并以完整 typed equality key 查找 RowId。redb 的 version 2 index codec 已能为嵌套值生成确定字节，但查询排序使用的 `Value::cmp_ord` 只覆盖部分 scalar；二者还不是一个可供语言依赖的 ADT total order。复合条件、范围过滤、排序和 cursor page 因此仍需扫描、内存排序，再从头跳过 page boundary。

本 RFC 冻结四项共同契约：

1. 索引声明是有顺序、有逐项方向的 field-path tuple；
2. 扫描比较、内存索引与 redb key 共用一个由静态类型驱动的 total order；
3. planner 只消费可机械证明安全的源码 stage 前缀，并保留原 stage 做语义校验；
4. 稳定 page 只有在用户可见顺序可证明唯一时才做 seek，不使用隐式 RowId。

这项设计不增加通用 optimizer、统计信息、join 或 SQL 式隐式排序规则。它为轻量任务队列、事件流、配置和 key/value 工作流提供常见的 equality prefix、单 range field 与有界有序读取。

### 2. 声明语法与 canonical formatter

单行声明沿用 table 后的结构列表，并与 query 的 `sort {-priority, id}` 使用同一个方向记号：

```text
create index tasks (status, -priority, id)
create unique index sessions (tenant, token)
```

`-field` 表示 descending；没有前缀表示 ascending。不接受 `+field`、`asc`／`desc` 关键字或分号。括号表示索引 key 的结构边界，逗号只分隔同一行的成员。formatter 在一行过长时输出无逗号的换行形式：

```text
create index audit_events (
  tenant
  -occurred_at
  id
)
```

parser 接受逗号或一个以上换行作为成员分隔符，也接受尾随逗号以方便编辑。canonical formatter 始终把短声明写成单行逗号列表，把长声明写成无逗号的换行列表。空 key、超过 16 项、重复文本 path、无法解析的方向和 path 都在 schema 变更前失败。

索引没有用户命名。其逻辑 shape 是：

```text
(table stable ID, [(field stable-ID path, direction)...], kind)
```

字段改名保留 stable path 和索引身份；字段次序或方向变化是 drop/add，并分配新 index ID。方向属于身份，因此 `(a, -b)` 与 `(a, b)` 可以同时存在。执行器可以反向遍历 `(a, -b)` 来满足 `sort {-a, b}`，但全局反向 shape 仍是另一项声明；formatter 不做隐式归一化。

同一表不得存在完全相同 field tuple 的两个声明，即使 ordinary/unique kind 不同。重复返回 `E_INDEX_DUPLICATE`，诊断包含 canonical shape；需要把 ordinary 改为 unique 时使用 migration 的显式 drop/add，以便完整验证已有数据。

旧 `create index tasks (status)` 等价于一个 ascending component，canonical source 不变。

### 3. 类型化 total order

索引建立时，每个 field path 绑定 stable field-ID path 和最终静态类型。比较只发生在同一绑定类型内，不定义不同类型之间的用户顺序。实现提供一个 catalog-aware `typed_cmp(type, left, right)`，并让 scan sort、filter range、aggregate min/max、cursor boundary、memory index 与 durable key 测试共享它；不能继续分别推断顺序。

规范顺序如下：

| 类型 | 顺序 |
| --- | --- |
| `bool` | `false < true` |
| `int`、`date`、`timestamp`、`duration` | 有符号数值顺序 |
| `float` | IEEE 有限数值顺序；`-0.0 == 0.0`；NaN/Infinity 仍不能进入 typed value |
| `decimal(p, s)` | 同一声明 scale 下按 coefficient 数值顺序 |
| `text` | UTF-8 bytes 的字典序；该顺序与 Unicode scalar-value 顺序一致，不做 locale collation 或 normalization |
| `bytes`、`uuid` | unsigned bytes 字典序 |
| 命名类型 | type ID 必须相同，再按其 body 排序；名义身份不被擦除 |
| sum type | stable variant ID，再按该 variant 的 payload tuple 排序 |
| record/product | 按 stable field ID 升序形成 value tuple，再逐项排序 |
| tuple | 按声明位置逐项排序 |
| `option T` | `None < Some value`，`Some` 内按 `T` 排序 |
| `list T` | 元素字典序；共同前缀相同时短 list 在前 |

有限直接递归 ADT 按实际有限值递归比较，并继续受深度 64 限制。静态类型验证保证比较双方形状一致；缺字段、未知 variant/type ID 或非有限 float 是数据／codec 错误，不能当成 `Equal`。descending 只反转对应 component 的比较结果。

`typed_cmp == Equal` 当且仅当现有 typed equality 为真。实现 #165 必须补齐当前 `Value::cmp_ord` 对 bool、sum、product、tuple、option 和 list 的缺口，并替换当前依赖字段名称或 JSON 文本的 ordered-index 路径。schema rename 不得在没有数据变化时改变同一 stable typed value 的逻辑顺序。

### 4. 复合 key 与索引内容

一行对一个索引产生一个 key：

```text
TypedTuple(component_1, ..., component_n) -> RowId
```

tuple 按声明顺序逐项比较，每项应用自身方向。RowId 只作为 redb B-tree 中保存重复 tuple 的物理后缀，不属于 typed tuple、查询顺序、唯一性证明或 cursor。ordinary index 可以有重复 tuple；unique index 对完整 typed tuple 强制唯一，方向不影响 equality。

memory index 使用与 redb 相同的 canonical typed tuple bytes 作为有序 key。posting 内 RowId 升序只用于确定存储和检查结果；如果 query sort tuple 本身不唯一，语言仍不承诺 ties 的稳定顺序。

每个索引最多 16 个 component。单个 `bytes` leaf 继续受 8 KiB indexed-bytes 限制；完整编码 key（magic、version、index ID、components 与 RowId）最多 64 KiB。嵌套值继续受深度 64 和 value codec collection 限制。DDL 会先验证可索引类型；insert、upsert、update、migration 和 restore 在发布前编码每个受影响 key，超过限制返回 `E_INDEX_KEY_LIMIT` 并原子回滚。

64 KiB 是硬限制而不是目标尺寸。`check` 会重新编码并逐项比较全部 durable keys，拒绝超限、非 canonical、tuple arity/type/direction 不匹配、错误 RowId 或错误 index ID。

### 5. Equality prefix 与一个 range field

对索引 `(k1, k2, ..., kn)`，一个访问约束可包含：

- 从 `k1` 开始连续零个或多个完整 equality；
- equality prefix 后紧邻的至多一个 range field；
- range field 上可有一个 lower bound、一个 upper bound，分别为 inclusive 或 exclusive；
- range 后不能再用后续 component 缩小 B-tree span，但这些条件仍作为普通 residual filter 执行。

例如索引 `(tenant, state, -priority, id)` 可用于：

```text
from tasks
filter tenant == $tenant
filter state == Open
filter priority >= 10
sort {-priority, id}
```

绑定结果是 equality prefix `(tenant, state)`、range field `priority >= 10` 和 forward traversal。`filter id == 4` 不能越过未约束的 `priority` 直接形成更窄 span。

range 操作符为 `>`, `>=`, `<`, `<=`。同一 range field 上两个 lower 或两个 upper boundary 在 bind 时取更严格者；矛盾区间直接产生零候选。equality 与 range value 必须先按字段静态类型绑定。`contains`、`match`、`or`、`not`、函数调用、字段间比较和计算表达式不产生 index boundary。

### 6. Planner 的 stage 安全规则

planner 只查看 source 后的连续安全前缀：

1. 任意数量不读写 row 的顶层 `let`；
2. 随后的若干个简单 `filter field op bound_value` stage；
3. 可选的一个 `sort`；
4. 可选的 `take` 或最终 `page`。

`bound_value` 只能是 literal、prepared parameter 或已绑定的零参数 local constant。简单比较在 bind 后是 total 且无运行时错误，planner 才可重排这些边界来形成 equality prefix。原 filter stage 仍按源码顺序执行，作为结果一致性的最后检查。

遇到第一个非简单 filter、`filter match`、`derive`、`derive match`、`select`、`aggregate`、`group`，或会改变 field identity／row set 的其他 stage 后停止抽取；不得从后续 stage 倒推索引条件。planner 也不从 `and`／`or` 表达式内部抽取条件，避免改变短路和错误可见性。没有完整连续索引前缀时回退现有 scan/sort。

sort 可以省略已被 equality 固定的索引前缀。去除这些常量项后，query sort 必须与索引剩余 component 的一个连续前缀方向完全相同，或所有方向完全相反；后者使用 reverse iteration。不允许只反转部分 component。

多个可用索引时采用稳定、无需统计信息的选择：优先 equality component 更多者，再优先存在 range boundary，再优先能满足 sort/page 者，再优先 key component 更少者，最后按 stable index ID。该规则进入测试，但 index ID 不进入用户 query digest。

### 7. Sort、take 与 page seek

满足上一节时，普通 query 和 mutation target 可以直接按索引顺序产生候选。`take` 只在所有前置 filter 执行后计数。若 residual filter 存在，执行器继续迭代 index span，直到产生足够的通过行或 span 结束，不能只读取 `take` 个原始 entry。

page 继续要求用户声明的可见排序 tuple 可证明唯一，并且不添加 RowId：

- 现有规则“sort 最后一项是源表 primary key”继续有效；
- 新增规则“equality-fixed fields + 完整 visible sort fields”覆盖同一 unique index 的全部 field path，也可证明在当前 filter domain 内唯一；
- unique 证明只使用简单 equality prefix，不能依赖样本数据、residual filter 或 ordinary index。

例如 unique index `(tenant, slug)` 下，`filter tenant == $tenant | sort slug | page 100` 是唯一顺序。cursor boundary 只保存 `slug`；绑定后的 tenant value 已在 canonical query/params digest 中。`sort created_at | page 100` 即使当前值没有重复也返回 `E_PAGE_ORDER`。

恢复 page 时先完成现有 token、database、schema、plan digest 与 sequence 校验，再把完整 typed boundary 编码成 seek boundary。forward 从严格大于 boundary 开始，backward 从严格小于 boundary 反向读取；响应中的行始终保持声明 sort 顺序。最多保留 `limit + 1` 个通过 residual filter 的 row。

### 8. Explain 与 cursor digest

`QueryAccessKind` 增加 `composite_lookup`、`range_scan`、`ordered_scan` 和 `page_seek`。结构化 `plan.access` 增加：

- stable index ID 与 canonical index shape；
- equality prefix field 列表；
- range field、lower/upper 的存在性与 inclusive 标志；
- `forward`／`reverse` traversal；
- 是否满足 sort、是否使用 cursor seek；
- 当前 span 的 estimated entries 与最终实际访问 entries（执行响应 profile 中提供）。

`explain` 不输出 parameter、literal、cursor boundary 或用户数据。回退路径继续明确显示 `full_scan` + page `sorted_scan`，不能因为存在不满足前缀的索引而报告 seek。

cursor plan digest 继续描述 canonical bound query、typed params、page limit/direction，不包含选择的物理 index ID。增加或删除一个等价访问索引本身会产生 schema revision/hash 变化，已有 cursor 按 `E_CURSOR_SCHEMA` 失效；同一 schema 下的 planner 选择变化不应把物理实现写进 query identity。

### 9. Catalog、migration 与格式升级

`IndexDefinition` 从单个 `column/field_path` 演进为 ordered components：

```text
IndexDefinition
  id
  table_id
  components [{column, field_path, descending}]
  kind
```

schema source、hash、diff、introspection 和 migration ledger 都包含 component 顺序与方向。字段 rename 通过 stable path 更新显示名称，不改变 index ID；drop/reorder/direction/kind 变化显式表现为 drop/add。所有受影响 row 在 migration candidate 内验证并重建 key，失败不发布 schema、rows、indexes 或 ledger。

实现 #165 使用以下格式边界：

| 层 | 当前 | 新格式 | 规则 |
| --- | ---: | ---: | --- |
| redb storage format | 4 | 5 | 新库写 5；1–4 仍可打开 |
| catalog codec | 3 | 4 | v3 单 column 映射为一个 ascending component |
| index-key codec | 2 | 3 | v3 使用有方向、可 seek 的 typed tuple framing |
| logical backup | 3 | 4 | v3 backup 恢复时做同一单 component 映射 |
| row/value/migration/receipt codec | 当前版本 | 不变 | 本功能不重写 row value 或 receipt payload |

format 4 数据库可以继续读写已有单列 ascending index。首次创建复合 index 或 descending index 前必须显式执行 `storage upgrade --to 5`；返回 `E_STORAGE_UPGRADE_REQUIRED`，不隐式改盘。升级在一个 redb transaction 中重写 catalog 与全部 secondary-index keys，保留 rows、RowId、schema revision/hash、migration ledger、receipts、database identity、cursor secret 和 commit sequence。仅格式升级不改变逻辑 schema identity，也不推进应用 migration history。

backup 4 保存 components。新二进制可以恢复 backup 1–4；旧二进制按现有未知版本规则拒绝 backup 4。restore 始终写当前 production storage format。未知 storage/catalog/index/backup 版本继续 fail closed。

### 10. Durable key 约束

index-key codec 3 的逻辑 framing 为：

```text
magic | codec version | index ID |
component count | framed component 1 ... framed component N | RowId
```

每个 component 使用与 `typed_cmp` 同构、可前缀分界的 order-preserving encoding。ascending 保存正向编码；descending 保存逐 byte 反序的完整 framed component，使 B-tree byte order 等于声明 tuple order。长度、terminator 与 escape 也属于反序 framing，不能只翻转 payload。prefix lower/upper sentinel 由 codec 构造，调用方不能拼接任意 `0x00/0xff`。

codec API 同时提供 encode tuple、encode equality prefix、encode inclusive/exclusive range bounds、encode page seek 和 decode/validate。所有入口共享 arity、type、depth 和 64 KiB 检查。测试必须证明 `sign(typed_cmp(a,b)) == sign(encoded(a).cmp(encoded(b)))`，而不是只比较少量手写 scalar bytes。

### 11. 错误与验收向量

稳定错误码：

| Code | Meaning |
| --- | --- |
| `E_INDEX_DUPLICATE` | 同一 table 已有相同 field tuple |
| `E_INDEX_SHAPE` | 空、重复、超过 16 项或无效 field/direction |
| `E_INDEX_KEY_LIMIT` | 单 leaf 或完整 durable key 超限 |
| `E_PAGE_ORDER` | page 的 visible order 无法静态证明唯一 |
| `E_STORAGE_UPGRADE_REQUIRED` | 旧格式请求新索引能力 |
| `E_STORAGE` | durable key 非 canonical、版本/身份/shape 不匹配 |

实现测试至少覆盖：

| Vector | Expected result |
| --- | --- |
| inline 与 multiline index declaration | 相同 AST、schema hash 与 canonical formatter |
| `(a, -b)` 与 `(a, b)` | 不同身份；各自可 exact/reverse traversal |
| 重复 path、17 components、`+a`／`a desc` | bind 前 `E_INDEX_SHAPE` 或 syntax error |
| equality on `k1..km` + lower/upper on `k(m+1)` | bounded range；与 forced scan 逐行相同 |
| 跳过 leading key 或在 range 后约束 key | 不缩小 span；residual filter 保留结果 |
| derive/select/match 前后的可索引 filter | planner 不越过边界 |
| scalar、named、sum、record、tuple、option、list、recursive ADT | scan comparator、memory key、redb key 的 equality/order 一致 |
| `-0.0`／`0.0`、`None`／`Some`、variant ID、list prefix | 规范 equality/order |
| 64 KiB 与 64 KiB + 1 encoded key | 前者接受，后者原子 `E_INDEX_KEY_LIMIT` |
| duplicate prefixes + PK or full unique tuple | forward/backward page 无重复或遗漏 |
| non-unique visible sort | `E_PAGE_ORDER`，零 row scan |
| format 4 reopen | 旧单列索引可读写，schema identity 不变 |
| format 4 使用 composite/descending | `E_STORAGE_UPGRADE_REQUIRED`，文件不变 |
| format 4→5 upgrade 中故障 | 完整旧格式或完整新格式，无混合 key |
| backup 3 restore / backup 4 round trip | 单列映射与复合 shape、rows、RowId、ledger、receipts 完整 |
| update/delete/upsert/batch/migration 回滚 | row 与所有 component key 同时保持旧状态 |

## English Description

### Decision and syntax

An ordered index is an ordered tuple of typed field paths with a direction on every component. It uses the same compact direction marker as query sorting:

```text
create index tasks (status, -priority, id)
create unique index sessions (tenant, token)
```

The absence of a marker means ascending and `-` means descending. The language does not add semicolons, `+`, or `asc`/`desc` keywords. Parentheses make the DDL structure explicit; commas separate inline members. The canonical multiline form uses indentation and newlines without commas. A key contains 1 to 16 distinct paths.

Index identity consists of the table stable ID, ordered stable field-ID paths and directions, and kind. Direction is part of identity. A rename preserves identity; reordering or changing direction/kind is an explicit drop/add with a new index ID. A table cannot contain two indexes over the same complete component tuple, including an ordinary/unique duplicate; it fails with `E_INDEX_DUPLICATE`. Existing one-column syntax remains the canonical ascending form.

### Typed ordering

Implementation must introduce one catalog-aware typed comparator shared by scan sorting, range evaluation, aggregates, cursor boundaries, memory indexes, and durable-key conformance tests. Comparisons occur only within one bound static type. Boolean uses `false < true`; signed and temporal scalars use numeric order; finite floats use numeric order with both zeros equal; fixed-scale decimals compare coefficients; text and bytes use unsigned bytewise lexicographic order; UUID uses its bytes.

Named values retain type identity. Sums compare stable variant ID then payload, records compare values in stable field-ID order, tuples compare by position, options place `None` before `Some`, and lists use lexicographic order with a shorter common prefix first. Finite recursive ADTs recurse over the stored finite value under the existing depth limit. Equality from this comparator must exactly match typed equality. Invalid shapes or non-finite values are errors and may never silently compare equal.

This is an intentional correction to the current split implementation: generic `Value::cmp_ord` does not yet cover every ADT, while the durable ordered encoder is not catalog-aware. #165 must centralize these semantics before exposing range access.

### Eligible access paths

For an index `(k1, ..., kn)`, a bounded span consists of a contiguous equality prefix followed by at most one range field. That field may have one lower and one upper inclusive or exclusive bound. Constraints after a gap or after the range remain residual filters and do not narrow the B-tree span.

The planner examines only leading row-independent `let` stages followed by consecutive simple `field op bound_value` filters, then an optional sort and take/final page. A boundary value is a typed literal, prepared parameter, or bound zero-argument local constant. The planner stops at compound boolean expressions, match, derive, select, aggregate/group, or any other semantic boundary. It retains and executes the original filter stages in source order. It never extracts predicates from `and`/`or`, so index selection cannot change short-circuit or error visibility.

After removing sort fields fixed by equality, the requested sort must match a contiguous index suffix exactly or invert every remaining direction for reverse traversal. Partial direction inversion is not eligible. Selection among multiple eligible indexes is deterministic: most equality components, then a range, then sort/page coverage, then fewer key components, then stable index ID.

### Pagination and explain

An ordered index can serve a cursor page only when the visible order is statically unique. A primary-key suffix remains valid. A complete unique-index tuple split between equality-fixed fields and visible sort fields is also valid within that query domain. Runtime samples, residual predicates, ordinary indexes, and RowId never prove uniqueness. Cursor boundaries contain only visible typed sort values; fixed equality parameters are already part of the canonical plan digest.

Forward resume seeks strictly above the typed boundary and backward resume strictly below it. Existing database/schema/query/sequence checks still finish before access. Responses preserve declared order and retain at most `limit + 1` passing rows.

Explain distinguishes composite lookup, range scan, ordered scan, page seek, and full-scan/sorted-scan fallback. It reports the index shape, equality fields, range-bound presence and inclusivity, traversal direction, sort/page coverage, and bounded estimates without exposing literals, parameters, cursor boundaries, or row values. Physical index IDs remain outside the cursor query digest; schema changes already invalidate cursors through schema identity.

### Resources, persistence, and compatibility

Each row contributes one tuple key per index. RowId is only a durable duplicate-key suffix and is never part of language ordering or uniqueness. A component tuple has at most 16 fields, an indexed bytes leaf remains limited to 8 KiB, and the complete durable key is limited to 64 KiB. Depth remains limited to 64. Every write, migration, restore, and integrity check encodes affected keys before publication and fails atomically with `E_INDEX_KEY_LIMIT` or `E_STORAGE`.

Issue #165 introduces storage format 5, catalog codec 4, index-key codec 3, and logical backup format 4. Row/value, migration, and receipt codecs do not change. Legacy catalog and backup definitions map one column to one ascending component. Format-4 databases may keep reading and writing existing ascending single-column indexes, but composite or descending declarations require an explicit `storage upgrade --to 5`; there is no implicit disk upgrade. The atomic upgrade rewrites catalog and secondary-index keys while preserving rows, RowIds, logical schema identity, ledger, receipts, database/cursor identity, and sequence.

Index-key codec 3 frames the index ID, component count, order-preserving typed components, and RowId. Descending reverses the complete framed component, including escaping and termination. Codec-owned sentinels construct equality-prefix, inclusive/exclusive range, and page-seek bounds. Conformance tests must compare comparator signs with encoded byte ordering over every scalar and ADT family, in addition to explicit format-4 read, format-4-to-5 crash, backup-3 restore, backup-4 round-trip, key-limit, planner-boundary, and mutation-rollback vectors listed above.

### Acceptance and verification

The parser and formatter must normalize inline and multiline declarations to one schema identity and reject empty, repeated, malformed, or over-16-component shapes before mutation. Directional identity and exact/global-reverse traversal require explicit tests. For every primitive, named, sum, record, tuple, option, list, and finite recursive value family, forced scans, memory indexes, and redb indexes must produce the same equality and ordering, including both float zeros, option tags, variant IDs, and list prefixes.

Planner tests must cover contiguous equality prefixes, inclusive and exclusive lower/upper bounds on one following field, gaps, post-range residuals, and every stage barrier. Ordered reads, limited mutations, and forward/backward pages must agree with forced scans under duplicate prefixes. Page tests must accept primary-key or complete unique proofs and reject a non-unique visible order before row access.

Boundary tests must accept a 64 KiB durable key, reject the next byte atomically, reopen format 4 without changing schema identity, require an explicit upgrade for new index shapes, and inject failure through the format 4 to 5 transaction. Backup 3 restore and backup 4 round trips must preserve index shapes, rows, RowIds, migrations, and receipts. Failed update, delete, upsert, batch, and migration operations must leave every row and composite-index key at the old committed state.
