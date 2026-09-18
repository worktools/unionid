# RFC 0021：有界 typed map / Bounded typed maps

- 状态 / Status: proposed
- 日期 / Date: 2026-09-19
- 跟踪 / Tracking: [#246](https://github.com/worktools/unionid/issues/246)

## 中文说明

### 1. 问题与决策

Unionid 的 struct、enum、tuple、Option 和 List 适合键集合在 schema 中已知的数据，但外部 metadata、用户自定义属性和 provider tags 的键集合在运行时才知道。把这类值拆成固定字段会频繁修改 schema；把它们伪装成 JSON 又会丢掉值类型、名义 ADT、索引顺序和 migration 检查。

首版增加一个窄而完整的容器：`Map<text, T>`。键固定为 UTF-8 text，所有 value 使用同一个声明类型 `T`。map 是 `ScalarType`、`Value`、源码、Rust/serde、protocol v2、portable contract、value/index/cursor codec、receipt、logical backup 和 migration 的原生成员。任意 JSON scalar、混合 value 类型、非 text key、开放 record 和 keyed secondary index 不属于本 RFC。

规范源码沿用当前 Rust-shaped PRQL 语法：

```text
enum Attribute {
  Text(text)
  Number(int)
  Enabled(bool)
}

struct Account {
  id: uuid
  attributes: Map<text, Attribute> = map {}
}

table accounts: Account {
  key id
}

insert accounts {
  id: uuid "0191d78a-32d8-7c2f-8f31-4c499bf64d9a"
  attributes: map {
    "plan": Text("pro")
    "region": Text("eu")
    "seats": Number(5)
  }
}
```

`Map<text, T>` 明确类型应用和 key/value 边界；不增加 `map text T` 这种第二套 legacy 形式。`map {}` 明确值 literal 与 record literal 的区别。条目使用 quoted text、冒号和值；多行不要求逗号，parser 可接受逗号与 trailing comma，formatter 按 key 排序并输出无分号的规范多行布局。

### 2. 值域、规范化和 total order

内部类型增加 `ScalarType::Map(Box<ScalarType>)`，其中 box 是 value 类型；text key 是该 variant 的固定契约。内部值增加 `Value::Map(BTreeMap<String, Value>)`。BTreeMap 不是公开的磁盘布局，但保证内存表示、源码输出和边界遍历使用 UTF-8 byte lexicographic key order。输入中的 duplicate key 在类型检查前返回 `E_DUPLICATE_KEY`，不能使用 first-wins 或 last-wins。

完整 typed equality 要求 key 集合相同，且每个 key 对应的 typed value 相等。total order 把 map 看成按 key 排序的 `(text, T)` 序列：先比较 key，再比较 value；共同前缀相同时，较短 map 在前。该顺序与源码插入次序无关，并与 whole-map index、sort、group、count_distinct、min/max 和 cursor 使用同一 catalog-aware comparator。

命名 value 类型保留 stable type ID。两个 value body 结构相同但名义类型不同的 map 不可互换。有限递归 ADT 可以出现在 value 中，并继续受整体深度限制；map 自身不是递归声明的特殊终止规则，空 map 与空 List 一样提供终止路径。

### 3. 预算和稳定错误

每个 map 同时受以下硬限制：

| 预算 | 限制 | 检查位置 |
| --- | ---: | --- |
| 条目数 | 4,096 | literal、serde、wire、coerce、codec decode |
| 单个 key | 4,096 UTF-8 bytes | literal、serde、wire、coerce、codec decode |
| 完整 encoded value | 16 MiB | 共用现有 value codec limit |
| 嵌套深度 | 64 | 共用现有 ADT depth limit |
| 查询遍历 | 共用 expression work budget | `keys`、`values`、`entries`、`any`、`all` |

限制在候选状态发布和 durable transaction 前检查。批量 insert/upsert、update、migration、restore 或 prepared request 中任一 map 超限，整个请求失败，不保留部分 row、index、receipt 或 ledger effect。

新增稳定错误：

| Code | 含义 |
| --- | --- |
| `E_MAP_LIMIT` | 条目数或 key bytes 超出 map 专用限制 |
| `E_DUPLICATE_KEY` | source 或 wire entries 包含重复 key |

形状或 value 类型错误继续使用 `E_TYPE`、`E_SERDE`、`E_PROTOCOL_TYPE` 或 `E_CODEC`，并保留字段、参数或 map key path，例如 `value.attributes["region"]`。深度与总 value bytes 继续使用现有稳定边界。

### 4. 查询表达式

首版只提供无副作用、可在扫描前绑定的基础操作：

```text
from accounts
filter contains_key attributes "plan"
derive plan = get attributes "plan"
derive attribute_keys = keys attributes
derive attribute_values = values attributes
derive attribute_entries = entries attributes
```

类型如下：

| 表达式 | 结果 |
| --- | --- |
| `contains_key map key` | `bool` |
| `get map key` | `Option<T>` |
| `keys map` | `List<text>` |
| `values map` | `List<T>` |
| `entries map` | `List<(text, T)>` |
| `length map` | `int` |

key 必须是 text；map 和 key 参数在 bind 时确定类型。`get` 缺 key 返回 `None`，不会产生 null、默认 value 或运行时异常。遍历结果按规范 key order 返回，因此重启、redb、memory、TCP 和 Rust API 一致。`filter`、普通 derive、match condition、match branch、local function 和 migration conversion 复用同一 expression IR；首版不增加 map destructuring pattern、动态字段 path、隐式 key coercion、map merge 或原地 key mutation。update 可同时求值后替换完整 map。

### 5. Rust、serde、wire 与 portable contract

`Value::from_serde` 把 string-keyed Rust map 解码为 `Value::Map`。`BTreeMap<String, T>` 与 `HashMap<String, T>` 都可输入；规范化后顺序只由 key 决定。非 string key 返回 `E_SERDE`，struct 仍解码为 `Value::Record`，二者不因 JSON object 外形相似而混淆。typed row 解码可把 `Value::Map` 还原到应用 map；重复 key 在进入 Rust 容器前已经被拒绝。

protocol v2 使用 entry array 而不是 JSON object，以便 decoder 在丢失信息前拒绝 duplicate key，并冻结 canonical order：

```json
{
  "type": "map",
  "entries": [
    {"key": "plan", "value": {"type": "variant", "name": "Text", "variant_id": "2", "args": [{"type": "text", "value": "pro"}]}},
    {"key": "seats", "value": {"type": "variant", "name": "Number", "variant_id": "5", "args": [{"type": "int", "value": "5"}]}}
  ]
}
```

entries 必须按 UTF-8 key bytes 严格递增；非规范顺序、重复 key、超限或非 canonical nested value 都在 mutation 前失败。protocol v1 遇到 map 参数、结果或 schema boundary 时返回 `E_PROTOCOL_TYPE`。protocol v2 继续回显版本；不增加 protocol v3。

portable `TypeShape` 增加 text key shape、value shape、`max_entries: 4096` 和 `max_key_bytes: 4096`。Rust schema/query codegen 对应 `BTreeMap<String, T>`，避免生成类型把 nondeterministic iteration order 暴露为数据库语义。宏和 generated binding 使用同一 portable shape，不自行解析 map。

### 6. Durable codec 与格式升级

Map 增加 catalog、row value、ordered index/cursor、receipt 和 logical backup vocabulary，因此旧 binary 必须拒绝包含 map 的 durable state，不能只依赖 serde 的 unknown variant error。格式采用成对演进，保留 incremental journal 是否启用这一既有能力：

| 状态 | 无 active journal | 带 journal capability |
| --- | ---: | ---: |
| map 之前 | storage format 6 | storage format 7 |
| map capable | storage format 8 | storage format 9 |

6→8 与 7→9 是显式 `upgrade --target`，保留 database/cursor identity、sequence、schema、rows、RowId、indexes、ledger、receipts、maintenance state、compaction proof 规则和 journal chain。8 上启用 incremental journal 进入 9；停用仍不降级。不存在 7→8 或 9→8，因为这会丢失 journal capability。新库何时默认创建为 8 由 v0.10 release PR 决定；在此之前，对 format 6/7 声明 map 返回 `E_STORAGE_UPGRADE_REQUIRED`，普通写入不隐式升级。

新 codec 版本：

| Codec | 旧 | map capable | 说明 |
| --- | ---: | ---: | --- |
| catalog | 4 | 5 | `Map<text, T>` type tag |
| value | 2 | 3 | count + ordered key bytes + typed value |
| index key | 3 | 4 | whole-map order-preserving component |
| receipt | 2 | 3 | response rows may contain map wire values |
| logical backup | 4 | 5 | catalog、rows 与 receipts may contain map |
| cursor prefix | `u1`/`u2` | `u3` when boundary contains map | typed boundary vocabulary |

value codec 3 编码 `u32 count`，随后对每项编码 `u32 key_bytes`、UTF-8 key 和按声明 `T` 编码的 value。encoder 强制递增 key；decoder 同时验证 UTF-8、顺序、duplicate、entry/key/depth/byte limit。whole-map ordered key 使用与 total order 相同的 pair sequence framing，不能复用 JSON `index_key()`。

logical backup 5 继续是 generation-independent 的完整状态，不改变 checksum 的 canonical 原则。restore 1–4 保持可读；只有 backup 5 可以恢复 map。format 8/9 的 logical backup 即使当前 schema 没有 map，也写 format 5，避免 restore 静默降低兼容边界。incremental archive codec 必须能携带 codec-3 row/receipt bytes；7→9 upgrade 前先 export/verify active chain，升级提交作为连续 journal commit，旧 archive segment 保持不可变。

### 7. Migration、default 与索引边界

`map {}` 和非空 map literal 都可作为字段 default，并在完整 schema 注册后按 value type 检查。insert 在嵌套 record/sum payload 中沿用逐层 default fill。schema hash 包含 map type 与 value type identity，但不包含条目顺序。

Migration 首版支持新增 map 字段与 typed default、rename/drop 含 map 的 schema 对象、在其他 schema 变化中无损保留 map，以及通过完整 typed expression 显式替换 map。不提供隐式 `Map<text, A>`→`Map<text, B>` conversion；在 map construction/traversal expression 完成前，这类 migration 返回 non-runnable todo，不能只转换部分条目。

普通、unique、composite index 可以把完整 map 当一个 component，使用完整 typed equality/total order。首版不实现 `attributes["plan"]` 的 keyed index，也不让 planner 把 `contains_key` 或 `get` 变成 index boundary。细粒度 index 会造成每行 N 个 posting 和不可忽略的写放大，只有真实 workload、key allowlist 和容量结论齐备后才单独设计。partial unique index #248 不依赖 keyed map index。

### 8. 实现顺序与完成条件

实现按以下顺序提交，每一步都保持 feature gate 闭合或完整可用，不暴露只能解析却不能持久化的公开类型：

1. model/source/formatter/coerce/default/typed equality/total order 与稳定限制；
2. protocol v2、serde、portable contract、Rust codegen 和 prepared binding；
3. codec 3/4、storage format 8/9、receipt、cursor、backup 5、check 与显式 upgrader；
4. `contains_key`、`get`、`keys`、`values`、`entries`、`length` 和 migration 边界；
5. metadata 应用场景执行 restart、migration、check、logical backup/restore，以及 format 6/7 upgrade 和中断恢复验收。

#246 只有在上述五层全部完成后关闭。仅完成 parser、memory Engine 或 serde roundtrip 不视为 typed map 已交付。

## English Description

### 1. Problem and decision

Unionid structs, enums, tuples, Option, and List work when the schema knows the field set. External metadata, user-defined attributes, and provider tags have runtime-defined keys. Modeling those keys as fixed fields causes frequent schema changes; disguising them as JSON loses value types, nominal ADTs, index order, and migration checks.

The first release adds one narrow, complete container: `Map<text, T>`. Keys are UTF-8 text and every value has the single declared type `T`. Map becomes native across `ScalarType`, `Value`, source, Rust/serde, protocol v2, the portable contract, durable codecs, receipts, backups, and migrations. Arbitrary JSON, heterogeneous values, non-text keys, open records, and keyed indexes remain outside this RFC.

Canonical source uses `Map<text, T>` and `map { "key": value }`. Multiline entries need no commas; the parser may accept commas and a trailing comma, while the formatter sorts keys and emits one semicolon-free layout. The explicit `map` prefix keeps dynamic keys distinct from record fields.

### 2. Values, order, and bounds

The internal forms are `ScalarType::Map(Box<ScalarType>)` and `Value::Map(BTreeMap<String, Value>)`. Duplicate input keys return `E_DUPLICATE_KEY`. Typed equality requires identical keys and equal values. Total order compares the canonical `(text, T)` sequence by key and then value, with the shorter map first after a common prefix. It is independent of input order and shared by whole-map indexes, sort, grouping, aggregates, and cursors.

Each map is limited to 4,096 entries and 4,096 UTF-8 bytes per key. It also shares the 16 MiB value limit, depth 64, and expression work budget. Every ingress checks the same limits before publication. `E_MAP_LIMIT` reports entry/key overflow; type, serde, protocol, and codec errors retain a key-aware path.

### 3. Expressions and application boundaries

The first expression surface is `contains_key map key -> bool`, `get map key -> Option<T>`, `keys map -> List<text>`, `values map -> List<T>`, `entries map -> List<(text, T)>`, and `length map -> int`. Missing lookup returns `None`; traversal follows canonical key order. There is no map pattern, dynamic field path, implicit key conversion, merge, or in-place key mutation in the first release.

String-keyed Rust maps serialize to `Value::Map`; structs remain records and non-string keys return `E_SERDE`. Generated code uses `BTreeMap<String, T>`. Protocol v2 uses a strictly key-ordered entry array so duplicates can be rejected before information is lost. Protocol v1 rejects map typed boundaries. The portable shape records text keys, value shape, and both limits.

### 4. Durable compatibility

Maps add catalog, row-value, ordered-index/cursor, receipt, and backup vocabulary. Paired formats preserve the existing optional journal capability: format 6 upgrades to map-capable format 8, while format 7 upgrades to map-and-journal format 9. Enabling the journal on 8 enters 9; disabling does not downgrade. There is no 7-to-8 or 9-to-8 path.

Upgrades are explicit and preserve all durable identity and journal state. Until the v0.10 release chooses the fresh-database default, declaring a map on format 6/7 returns `E_STORAGE_UPGRADE_REQUIRED`. Map-capable formats use catalog codec 5, value codec 3, index-key codec 4, receipt codec 3, logical backup 5, and cursor `u3` when a boundary contains a map. Backup 1–4 stay readable; only backup 5 restores maps.

### 5. Migration, indexing, and delivery

Map literals may be defaults. Migrations can add map fields, rename/drop surrounding schema, preserve maps through unrelated changes, and explicitly replace a whole map. There is no implicit conversion between different value types. Whole-map ordinary, unique, and composite indexes are supported; keyed indexes are deferred until a workload and write-amplification budget exist.

Delivery proceeds through model/source semantics; protocol/serde/portable Rust boundaries; durable formats and upgrades; expressions and migration boundaries; and a metadata journey covering restart, migration, check, backup/restore, legacy upgrades, and interrupted recovery. #246 closes only after every layer is complete. Parser-only, memory-only, or serde-only support is not delivered typed map.
