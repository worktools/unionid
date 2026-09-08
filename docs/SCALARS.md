# 生产标量基础 / Production scalar foundations

## 当前可用范围

`unionid::scalars` 提供 `Uuid`、`Date`、`Timestamp`、`Duration`、`Decimal` 和 `Bytes`，并已接入源码语言、原生 `ScalarType` / `Value`、嵌套 ADT serde、protocol v2、value codec 2、索引/cursor、migration 与 storage format 4。decimal 使用 schema 固定 precision/scale，并只提供精确 checked 运算。

完整目标见 [RFC 0004](rfc/0004-production-scalars.md)。新 redb 数据库使用 storage format 4 和 catalog/value/index-key/receipt codec 3/2/2/2；逻辑 backup 使用 codec 3。旧 format 1–3 和过渡 snapshot 仍拒绝新 schema/receipt，持久写请求包含新标量参数时返回 `E_STORAGE_UPGRADE_REQUIRED`；显式执行 `unionid upgrade --db <path> --target 4` 会在一个同步事务中校验并重写 catalog、rows、indexes、receipts 和 meta。失败保留旧格式。

## Rust 值与规范表示

| Wrapper | 逻辑值与校验 | serde JSON payload |
| --- | --- | --- |
| `Uuid` | 16 字节，输入标准带连字符 UUID，可规范化大写 | 小写带连字符字符串 |
| `Date` | 公历 `0001-01-01` 至 `9999-12-31`；epoch day 为 `i32` | `YYYY-MM-DD` 字符串 |
| `Timestamp` | UTC instant，微秒精度；输入可带时区偏移 | UTC `T` / `Z` 字符串，小数去掉尾零 |
| `Duration` | `i64` 微秒，整数单位解析；不包含月或年 | 十进制微秒字符串，如 `"-1500000"` |
| `Decimal` | `i128` coefficient，最多 38 位；scale 为 0–38 | `{"coefficient":"1990","scale":2}` |
| `Bytes` | 最大 16 MiB；文本解析和 Display 使用 hex | 无 padding 的 canonical base64url 字符串 |

`FromStr` 负责 UUID、日期、时间戳、时长和 bytes 的输入解析；decimal 用 `Decimal::parse(text, precision, scale)` 指定目标类型，或用 `Decimal::infer(text)` 推断最小 precision。`Decimal::rescale` 只接受精确转换，不舍入。wrapper 保存 coefficient 和 scale；目标 schema 的 precision 需在绑定时单独校验，serde 不保存 precision。

```rust
use unionid::scalars::{Bytes, Decimal, Timestamp};

let at: Timestamp = "1970-01-01T08:00:00.000001+08:00".parse()?;
assert_eq!(at.to_string(), "1970-01-01T00:00:00.000001Z");

let price = Decimal::parse("19.9", 18, 2)?;
assert_eq!(price.to_string(), "19.90");
assert!(price.rescale(18, 0).is_err());

let digest: Bytes = "deadbeef".parse()?;
assert_eq!(digest.to_base64url(), "3q2-7w");
```

输入解析可接受 RFC 允许的等价拼写，再生成规范输出。serde 反序列化则只接受表中的规范形式：例如 timestamp 的时区偏移、UUID 大写、duration 的 `"-0"`、decimal coefficient 的前导零、base64 padding 或非零 pad bits 均被拒绝。所有数值边界都会重新校验，不能经 serde 绕过构造函数。

## serde 类型身份

wrapper 调用 `serialize_newtype_struct`，marker 使用保留前缀 `unionid::scalar::`，当前名称是 `unionid::scalar::v1::<type>`。这里的 `v1` 只表示 wrapper payload 版本，与网络协议或 redb storage format 的版本独立。

普通 JSON serializer 会省略 newtype marker，输出上述 payload；这不是带 `type` 字段的 protocol-v2 wire envelope。Rust struct、enum、option、tuple 和 list 可以嵌套这些 wrapper，并通过指定目标 Rust 类型恢复身份。没有 schema 或目标 Rust 类型的 JSON 本身不能区分 UUID 与普通字符串。

现有普通应用 newtype 继续透明转换。保留前缀供 unionid 使用，应用不要把自己的 newtype 重命名到这个命名空间。

## 二进制与协议接入

`codec::encode_value` 继续写旧版 codec 1，`codec::encode_value_v2` 显式写 codec 2；`decode_value` 分派这两个版本并拒绝未知版本。旧类型的 payload 不变，新类型采用 RFC 的固定宽度字节或 length + bytes 编码。codec 1 对完整 type graph 预检，因此空的 `list uuid` 或 `None : option uuid` 也不能绕过版本限制。整个 encoded value 仍受既有 `MAX_VALUE_BYTES` 限制，包含 framing 开销。

`Request` 保持默认 version 1；用 `.with_version(2)?` 显式选择 version 2。v1 在 mutation 前检查参数、最终 query／`returning`／`explain` 结果类型和 introspection schema，无法表达新标量时返回 `E_PROTOCOL_TYPE`。幂等命中仍保持“不解析源码直接重放”的既有语义，同时验证存量回执能否由 v1 表达。v2 使用 RFC 0004 的 canonical wire envelope，响应回显版本，幂等 digest 仍包含版本。

cursor 根据实际 boundary 选择词汇版本：只含旧标量时继续输出 `u1`，任一排序键含新标量时输出 `u2`。两个版本共享 HMAC、database/schema/query/sequence 绑定和大小限制；prefix、payload codec 与 typed vocabulary 不一致时 fail closed。全部六类生产标量的源码与查询均已接入；decimal 乘除、avg 与舍入仍明确 deferred。

## English Description

`unionid::scalars` provides six native production scalars across source, typed Rust ADTs, protocol v2, value/index codecs, cursors, redb, backups, and exact migrations. Decimal precision and scale belong to the schema; addition, subtraction, negation, and sum are checked at every step. Multiplication, division, average, and rounding remain deliberately deferred.

Requests default to protocol 1; `.with_version(2)?` opts in. Version 1 rejects new scalar parameters before mutation, responses echo the requested version, and idempotency digests distinguish versions. Legacy durable formats reject native schemas/receipts and persistent mutations with native parameters. Read-only protocol-v2 use can pass and return native parameters without upgrading storage.

Protocol-v1 preflight now covers parameters, final query/returning/explain result types, and introspection before publishing a mutation. Idempotent hits preserve parse-free replay while checking that the stored result is expressible. Cursors remain `u1` for legacy-only boundaries and use `u2` when any boundary key is a production scalar; prefix, payload codec, and typed vocabulary must agree.

New redb databases use storage format 4 with catalog/value/index-key/receipt codecs 3/2/2/2, and logical backups use codec 3. Formats 1–3 remain readable with legacy schemas and require `unionid upgrade --db <path> --target 4` before native scalar writes. The upgrader validates and rewrites catalog, rows, indexes, receipts, and meta in one synchronous transaction. UUID and bytes are now available in source; indexed bytes are limited to 8192 octets and fail atomically with `E_INDEX_KEY_LIMIT`.

Canonical serde uses text for UUID/date/timestamp, string microseconds for duration, string coefficient plus numeric scale for decimal, and unpadded base64url for bytes. Decoding validates both canonical forms and domain bounds. Reserved `unionid::scalar::v1::<type>` newtype markers carry identity to typed serializers; this payload version is independent of protocol/storage versions. JSON itself erases markers, so application Rust types restore identity.

Tests in `tests/scalars.rs`, `tests/scalar_serde.rs`, `tests/native_scalars.rs`, and `tests/uuid_bytes.rs` cover logical/source/wire/binary vectors, nested ADTs, canonical rejection, query/index/key behavior, migration rollback, cursor traversal, protocol versions, redb reopen, and backup/restore.
