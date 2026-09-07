# RFC 0004：生产标量契约

- 状态：提议
- 日期：2026-09-07
- 父任务：[#115](https://github.com/worktools/unionid/issues/115)
- 实现任务：[#137](https://github.com/worktools/unionid/issues/137)、[#138](https://github.com/worktools/unionid/issues/138)、[#139](https://github.com/worktools/unionid/issues/139)、[#140](https://github.com/worktools/unionid/issues/140)

## 中文说明

### 1. 决策摘要

unionid 选择六个原生生产标量：`uuid`、`date`、`timestamp`、`duration`、`decimal P S` 和 `bytes`。它们是 `ScalarType` 与 `Value` 的独立成员，不伪装成 `text`、`int` 或命名 record。这样 schema、参数、查询结果、索引、migration 和持久 codec 都能保留值的实际含义。

类型声明继续采用 PRQL 风格的空格应用，不增加语句末尾分号或 TypeScript 风格的密集标注。花括号不是要消除的符号：它明确 record、projection 或嵌套层级；圆括号用于 precedence、tuple 和嵌套调用，方括号用于 list，逗号分隔 delimiter 内的相邻项并允许 trailing comma。formatter 以清晰且唯一的输出为准，不以符号数最少为目标。

下面使用花括号明确 `Invoice` 的 product type 边界，同时保留无分号字段和空格式类型应用：

```text
type Invoice = {
  id uuid,
  issued_on date,
  created_at timestamp,
  payment_window duration,
  amount decimal 18 2,
  receipt bytes,
}

table invoices Invoice
  key id
```

日期和 timestamp 使用 PRQL 风格 `@` literal，精确 duration 使用 number-unit literal；UUID、decimal 与 bytes 使用“类型名 + 字符串”，因为引号能避免它们改变普通 identifier/number 的词法规则。完整 query 结构约定见 [RFC 0005](0005-structured-prql-query-syntax.md)。

```text
insert invoices {
  id = uuid "0191d78a-32d8-7c2f-8f31-4c499bf64d9a",
  issued_on = @2026-09-07,
  created_at = @2026-09-07T09:30:15.123456+08:00,
  payment_window = 24hours,
  amount = decimal "199.90",
  receipt = bytes "89504e470d0a1a0a",
}
```

formatter 输出规范值，但 parser 可以接受下表明确列出的等价输入。类型不做隐式 text/int/float 转换；迁移必须写出 parse 或精度转换。

### 2. 为什么使用原生标量

真实应用会同时遇到这些值：任务与同步记录用 UUID 主键和时间戳，账单用定点金额与 civil date，执行记录用精确时长，内容寻址和小型 key/value 工作流用 binary digest。把它们存成 text/int 会把验证、单位、排序和迁移责任分散到每个调用方；用命名 wrapper 虽能表达领域名称，却仍无法给底层索引和 wire codec 一个统一的物理语义。

原生标量与命名 ADT 是互补关系。`uuid` 表示通用 128-bit 标识符；`type UserId = uuid` 仍提供名义身份，使 `UserId` 与 `InvoiceId` 不能混用。`decimal 18 2` 表示精度与 scale；货币种类继续由 record 或 sum 表达，例如 `{amount decimal 18 2, currency Currency}`，数据库不引入含糊的 `money` 类型。

### 3. 规范值域与表示

| 类型 | 逻辑值域 | 规范源码/展示 | 持久 payload | 相等与顺序 |
| --- | --- | --- | --- | --- |
| `uuid` | 任意 128 bits；不限制 RFC version | 36 字符、小写、带连字符 | 16 bytes，network byte order | 无符号 byte lexicographic |
| `date` | proleptic Gregorian `0001-01-01` 至 `9999-12-31` | `@YYYY-MM-DD` | Unix epoch 起的 signed `i32` 日数 | chronological |
| `timestamp` | 上述日期范围内的 UTC instant，微秒精度 | `@` + RFC 3339；展示规范为 UTC `Z` | Unix epoch 起的 signed `i64` 微秒 | chronological instant |
| `duration` | signed `i64` 微秒 | integer + exact unit，例如 `30seconds` | signed `i64` 微秒 | elapsed length |
| `decimal P S` | `1 <= P <= 38`、`0 <= S <= P` 的定点十进制 | 无指数的十进制文本，固定输出 S 位 | signed `i128` coefficient；scale 来自类型 | mathematical value；同字段固定 scale |
| `bytes` | 0 至 16 MiB 的 octet sequence | 小写、偶数长度 hexadecimal | `u32` 长度和原 bytes | unsigned byte lexicographic |

UUID 遵循 [RFC 9562](https://www.rfc-editor.org/rfc/rfc9562.html) 的 16-octet 与 network-byte-order 表示。输入必须是带连字符的 36 字符形式，可使用大小写十六进制；输出统一为小写。nil、max、标准和保留 variant 都作为 opaque 128-bit 值保存，数据库不从 UUID 推导业务时间。UUIDv6/v7 的 byte order 自然提供其规范所设计的局部性，但普通 `uuid` 比较仍只是 opaque bytes。

`date` 是不带时区的 civil day。它不代表午夜 instant，也不会受夏令时影响。首版不提供 `time`、local datetime、时区名称或 calendar interval；这些概念不能从 `date`/`timestamp` 猜出。

`timestamp` 的 `@` 输入接受 [RFC 3339](https://www.rfc-editor.org/rfc/rfc3339.html) 的 `Z` 或 numeric offset，立即规范化为 UTC。小数秒最多六位，不足补零，展示时移除无意义的尾随零；闰秒 `:60`、无 offset timestamp、未知本地时区和超过微秒精度的非零数字被拒绝。两个不同 offset 的输入只要表示同一 instant 就相等并产生相同字节。

`duration` 只接受整数与精确单位组成的 token：`microsecond(s)`、`millisecond(s)`、`second(s)`、`minute(s)`、`hour(s)`、`day(s)` 和 `week(s)`。例如 `30seconds`、`1500milliseconds` 与 `7days`。compound value 使用普通算术 `1day + 2hours`；负 compound value 用 `-(1day + 2hours)` 明确 grouping。year/month 和小数单位被拒绝，因为前者需要 calendar context，后者可无损改写为更小的整数单位。零规范为 `0microseconds`。

`decimal P S` 使用 coefficient × 10^-S。源码值不接受指数、前置 `+`、digit separator、NaN 或 infinity；`-0` 规范为零。目标类型可为较少的小数位补零，但不会静默舍入非零数字。precision 计算 coefficient 的十进制位数，零按一位计算；超出 P 返回 `E_DECIMAL_RANGE`。

`decimal "..."` 是 contextually typed literal：字段、参数、算术另一侧或 migration 目标提供 `P S` 时，literal 直接按目标精确归一；没有期望类型的独立 derive 使用能容纳原文本的最小 precision 和文本 scale。响应 column type 始终携带最终 `P S`，wire value 只携带 coefficient 和 scale。

`bytes` 源码使用 hexadecimal，便于人眼检查 magic、digest 和短 key。wire 使用 [RFC 4648](https://www.rfc-editor.org/rfc/rfc4648.html) base64url、无 padding，并要求 canonical pad bits。两种形式都拒绝空白、前缀和非 alphabet 字符；空字符串表示空 bytes。

### 4. 查询与汇总

六种标量均支持 `==`、`!=`、total ordering、`sort`、`min`、`max`、group key、普通/unique equality index 和 cursor boundary。`uuid` 还可作为表主键；首版不扩大其他主键类型，避免在没有实际需求时把大 binary 或低区分度时间值变成表身份。

算术采用窄而明确的矩阵：

- `decimal P S` 支持同类型 `+`、`-`、一元负号与 `sum`；结果仍是 `decimal P S`，每一步检查 precision，溢出返回 `E_ARITH`。乘法、除法、`avg` 和隐式 scale 推导延后，直到能定义结果 precision 与舍入模式。
- `duration` 支持同类型 `+`、`-`、一元负号与 `sum`；溢出返回 `E_ARITH`。
- `timestamp + duration`、`timestamp - duration` 得到 timestamp，`timestamp - timestamp` 得到 duration。
- `date`、`uuid` 与 `bytes` 首版没有算术。calendar 加减必须由以后带单位的函数定义，不能把 day 当普通 int。
- `length bytes` 返回 octet 数；`contains bytes needle` 做连续 byte subsequence 判断。两者沿用现有表达式预算。

跨类型比较只允许同一个 resolved scalar type。不同 `decimal P S` 即使数学值相等，也必须先显式 rescale；timestamp 不与 date/text 比较，duration 不与 int 比较。命名 wrapper 继续要求相同 stable type ID。

### 5. 类型化参数、Rust 与 JSON wire

网络协议为新标量增加 version 2。version 1 继续接受既有 `WireValue`；若请求参数或结果行需要新标量，或 introspection 会暴露新标量定义，服务在执行 mutation 前返回 `E_PROTOCOL_TYPE`。没有新 typed boundary 的既有 version-1 请求继续工作。version-2 response 回显 request version，idempotency digest 继续包含 version，因此 v1 与 v2 请求不会共享 receipt identity。

version 2 的无损值如下：

```json
{"type":"uuid","value":"0191d78a-32d8-7c2f-8f31-4c499bf64d9a"}
{"type":"date","value":"2026-09-07"}
{"type":"timestamp","value":"2026-09-07T01:30:15.123456Z"}
{"type":"duration","microseconds":"86400000000"}
{"type":"decimal","coefficient":"19990","scale":2}
{"type":"bytes","base64url":"iVBORw0KGgo"}
```

整数 coefficient、duration microseconds 和 stable IDs 继续用 JSON string 避免 JavaScript 精度丢失。decoder 接受且只接受上述 canonical representation；重复的文本拼写先在源码层规范化，再进入 wire。cursor 中没有新标量时继续使用 `u1`；boundary 含新标量时使用 `u2`，保持 RFC 0003 的 HMAC、sequence 和 plan 语义，只扩充 typed-value vocabulary。

Rust API 提供 `Uuid`、`Date`、`Timestamp`、`Duration`、`Decimal` 与 `Bytes` wrapper，以及到对应 `Value` 的无损转换。wrapper 的 serde 实现使用保留的 newtype marker，使 `Value::from_serde` 能保留 scalar identity；读取到应用 struct 时使用上述 canonical 文本或显式 coefficient/scale 表示。外部 `uuid`、`time` 或 decimal crate 的转换应通过可选 feature 单独增加，核心公开类型不把磁盘格式绑定到第三方 crate 的内存布局。

### 6. 索引键、限制与错误

index-key codec version 2 为每个 scalar 使用类型化、order-preserving payload：signed integer 在 big-endian 前翻转 sign bit，UUID 使用 16 raw bytes，decimal 使用固定 scale 的 transformed i128，text/bytes 使用零转义与终止符。这样 equality、sort、cursor 和未来 range seek 使用同一 total ordering。存量 index 在内部格式升级时重建，不能混用 v1 JSON key 和 v2 key。

`bytes` 值仍受 16 MiB value limit；被索引的 bytes 最多 8192 bytes。建索引会先检查全部已有值，写入或 migration 产生超限 index key 时原子失败并返回 `E_INDEX_KEY_LIMIT`。UUID、时间和 decimal 都是定长 key。

新增稳定错误码：

| Code | Meaning |
| --- | --- |
| `E_SCALAR_LITERAL` | UUID/date/time/duration/bytes 文本不合法或不 canonicalizable |
| `E_DECIMAL_TYPE` | precision/scale 声明不合法 |
| `E_DECIMAL_RANGE` | coefficient 超出声明 precision，或 rescale 需要丢弃非零数字 |
| `E_PROTOCOL_TYPE` | protocol version 无法表达请求或结果中的 scalar |
| `E_INDEX_KEY_LIMIT` | typed index key 超出生产上限 |
| `E_STORAGE_UPGRADE_REQUIRED` | schema 使用新标量，但数据库尚未升级到 format 4 |

解析、wire decode、codec decode、migration 和 index build 都必须在分配前执行长度/precision 检查。错误保留字段或参数路径，并且 mutation 不产生部分 catalog、rows、indexes、receipt 或 schema revision。

### 7. Migration 与内部格式升级

新标量加入应用 schema 仍通过普通 versioned migration。不同类型之间没有隐式 cast；转换写出纯函数：

```text
migration invoice_ids_v2
  change field Invoice.id to uuid
    using old -> uuid_parse old
  change field Invoice.amount to decimal 18 2
    using old -> decimal_parse old 18 2
```

对应的精确函数为 `uuid_parse`、`date_parse`、`timestamp_parse`、`duration_parse`、`decimal_parse`、`bytes_parse_hex` 和 `decimal_rescale`。反向文本格式化与有舍入的数据转换不在首个切片；需要舍入时必须先新增带明确 rounding mode 的函数，不能改变 `decimal_rescale` 的 exact 语义。转换失败会报告 row/path 并回滚整个 migration。

这是内部格式的显式 major capability，版本冻结为：

- storage format 4；
- catalog codec 3；
- ADT value codec 2；
- index-key codec 2；
- receipt codec 2；
- logical backup codec 3；
- JSON Lines / HTTP typed protocol 2；
- scalar cursor `u2`。

value codec 2 的 payload 分别为本 RFC 第 3 节列出的固定表示；现有 primitive/ADT payload 在 version 2 中保持原字节规则。format 4 文件只写 current codec，避免一个数据库长期混合两种 index ordering。

新建 redb 使用 format 4。现有 format 1–3 继续以只含旧标量的模式打开；使用新标量前执行显式 `unionid upgrade --db <path> --target 4`。upgrade 先完整验证旧状态，在一个同步 redb transaction 中重编码 catalog、rows、indexes、receipts 和 meta；失败保留完整旧格式。逻辑 backup/restore 是另一条安全升级路径。format 4 不能由旧 binary 打开，release notes 和 `check --db` 必须显示所有 codec 版本。

### 8. 备选方案与未入选类型

| 方案或类型 | 结论 | 当前替代方式 |
| --- | --- | --- |
| 全部使用 `text`/`int` | 不选；单位、canonicalization 和 ordering 无法进入 schema/index/wire 契约 | 只用于导入旧数据，再通过显式 migration parse |
| 只使用命名 wrapper | 不作为物理层；它提供领域身份，但底层仍需要一种稳定 scalar representation | 在原生 scalar 外继续声明 `type UserId = uuid` 等 wrapper |
| `time` 或 local datetime | 暂不选；脱离 date/zone 容易在 DST 和跨日场景产生歧义 | 用命名 record 保存明确的 civil fields，或在应用边界转成 timestamp |
| zoned datetime / timezone database | 暂不选；IANA rules 会更新，不能把系统 tzdata 隐式变成持久语义 | 保存 `{instant timestamp, zone text}`，按业务依赖显式解释 zone |
| year/month calendar interval | 暂不选；一个月没有固定微秒数 | 使用 `type CalendarDelta = {months int, days int}`，由具有 calendar context 的应用计算 |
| arbitrary-precision decimal | 暂不选；当前工作集可由 38-digit fixed decimal 覆盖，任意精度会扩大算术和索引预算 | 保存 canonical text/bytes 并在应用处理，或使用更合适的数据库 |
| 大型 blob | 不把 16 MiB `bytes` 扩成对象存储 | 外部文件/object store + `{digest bytes, location text, size int}` metadata |
| URL/IP/JSON/geography | 暂不增加内建类型；尚无足够查询和 migration 需求 | 使用命名 text/record/sum，把可验证结构直接建模成 ADT |

这些替代不会阻止以后增加独立 scalar，但新增类型必须另行冻结源码、ordering、wire、codec、migration 和 format compatibility，不能复用“看起来像 text”的隐式行为。

### 9. Golden vectors 与验收

实现必须把以下向量固定在源码、Rust wrapper、wire、value codec、redb reopen、backup/restore 和 equality index 测试中：

| Type | Source input | Canonical logical value | Durable payload |
| --- | --- | --- | --- |
| uuid | `uuid "F81D4FAE-7DEC-11D0-A765-00A0C91E6BF6"` | `f81d4fae-7dec-11d0-a765-00a0c91e6bf6` | RFC-order 16 bytes |
| date | `@1970-01-01` | `@1970-01-01` | i32 `0` |
| timestamp | `@1970-01-01T08:00:00.000001+08:00` | `@1970-01-01T00:00:00.000001Z` | i64 `1` |
| duration | `-1500milliseconds` | `-1500milliseconds` | i64 `-1500000` |
| decimal 18 2 | `decimal "19.9"` | coefficient `1990`, scale `2` | i128 `1990` |
| bytes | `bytes "deadbeef"` | bytes `de ad be ef` | length `4` + raw bytes |

边界测试还覆盖 date 最小/最大值、timestamp offset 等价、负 epoch、duration i64 两端、decimal 38 digits/负零/非精确 rescale、空 bytes/16 MiB/索引 8192 与 8193、UUID nil/max，以及所有 truncated、尾随和未知 codec version。索引开启与关闭必须给出同样结果；重启、migration、format upgrade 和 restore 必须保持 schema identity 规则与逻辑值。

### 10. 实现切片

1. 兼容基础：protocol v1/v2 dispatch、format-4 upgrader、codec version、Rust wrapper 与全部 golden roundtrip。
2. 标识与 binary：`uuid`/`bytes` 语法、主键、比较、index、`length/contains` 与同步/content-address 场景。
3. 时间：`@` date/timestamp 与 number-unit duration parse、规范化、比较、算术、汇总与 migration。
4. 定点数：`decimal P S` 类型检查、精确算术、汇总、rescale 与账单场景。

每个实现任务都必须更新 LANGUAGE/QUERY/PROTOCOL/CODEC/MIGRATIONS 的“当前可运行”范围；不能因只完成 model enum 就把 #115 标为完成。

## English Description

### Decision

unionid will add six native production scalars: `uuid`, `date`, `timestamp`, `duration`, `decimal P S`, and `bytes`. They remain distinct throughout schema identity, values, query binding, indexes, migrations, Rust adapters, wire values, and durable codecs. Named wrappers still add domain identity, such as `type UserId = uuid`; money remains an ADT containing a decimal amount and an explicit currency.

Declarations keep PRQL-style space application and omit statement-terminating semicolons. This is not a blanket punctuation-minimization rule: braces mark record, projection, and nested structural boundaries; parentheses express precedence, tuples, and nested calls; brackets express lists; commas separate adjacent delimited items and may trail. The formatter optimizes for one clear representation rather than the fewest symbols.

Dates and timestamps use PRQL-style `@` literals. Exact durations use integer-unit literals such as `30seconds`; compound durations use ordinary arithmetic. UUID, decimal, and bytes retain a necessary typed quoted boundary. UUID text follows RFC 9562 and normalizes to lowercase. Dates are proleptic Gregorian civil days. Timestamps accept RFC 3339 offsets, normalize to UTC, reject leap seconds, and retain microsecond precision. Durations exclude calendar years/months. Decimal uses a signed i128 coefficient with precision 1–38 and a schema-fixed scale. Source bytes use lowercase hex, while wire bytes use canonical unpadded base64url.

All six values have total equality and ordering, grouping, min/max, equality indexes, and cursor support. UUID becomes an eligible primary key. Decimal and duration support checked addition, subtraction, negation, and sum. Timestamp supports exact duration addition/subtraction and timestamp difference. Decimal multiplication/division/average, calendar arithmetic, local time, named time zones, and implicit cross-type conversions are deferred until their result and rounding semantics are explicit.

### Compatibility

Typed protocol version 2 adds dedicated wire variants. Version-1 requests remain valid when their typed parameters, returned rows, and introspection do not expose a new scalar; otherwise the server rejects the request with `E_PROTOCOL_TYPE` before a mutation. Responses echo the request version, and version remains part of idempotency identity. Cursors keep `u1` for old scalar boundaries and use `u2` when a new scalar occurs.

The durable boundary advances to storage format 4, catalog codec 3, value codec 2, index-key codec 2, receipt codec 2, and logical backup codec 3. The new value payloads are fixed-width numeric or length-delimited byte representations; index codec 2 uses order-preserving signed transforms and escaped byte strings. New databases use format 4. Existing format 1–3 databases stay readable for old schemas and require an explicit, atomic `unionid upgrade --target 4` before a schema may use production scalars. Old binaries fail closed on format 4.

Rust exposes stable wrapper types with lossless `Value` conversion and serde markers rather than tying disk layout to third-party crate internals. Text/integer migration is never implicit: exact parse and rescale functions must be written in the migration, and any invalid row rolls back the whole migration.

Local time, zoned datetime, calendar month intervals, arbitrary-precision decimals, large blobs, and URL/IP/JSON/geography scalars remain deferred. Applications can model their explicit structure with named records/sums, pair an instant with a zone string, keep calendar deltas as domain records, or store large content externally with typed digest metadata.

The normative limits, error codes, golden vectors, and four implementation slices are defined above. Completion requires source ↔ Rust ↔ protocol ↔ durable redb roundtrips, equality/index consistency, restart, backup/restore, migration, and format-upgrade coverage for every selected scalar.
