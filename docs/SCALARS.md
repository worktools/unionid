# 生产标量基础 / Production scalar foundations

## 当前可用范围

`unionid::scalars` 提供 `Uuid`、`Date`、`Timestamp`、`Duration`、`Decimal` 和 `Bytes` 的 Rust 值校验与 serde 表示。这是 [#137](https://github.com/worktools/unionid/issues/137) 的基础切片；当前查询语言、`ScalarType`、`Value`、网络协议和持久化尚未接入这些类型。`Value::from_serde` 遇到这些 wrapper（包括嵌套在应用 ADT 中）返回 `E_SERDE`，防止静默转换成 text 或 record。

完整目标见 [RFC 0004](rfc/0004-production-scalars.md)。数据库的显式 format-4 升级、protocol v2 和 codec 转换仍需完成后才能存储这些值。构造 Rust wrapper 不会触发数据库升级。

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

## English Description

`unionid::scalars` provides validated Rust domains and canonical serde payloads for six production scalars. This is a foundation slice of #137. Query syntax, native `ScalarType` / `Value` variants, protocol v2, durable codecs, and the explicit format-4 upgrade remain pending. `Value::from_serde` rejects these wrappers with `E_SERDE`, including nested wrappers, so their identities cannot silently become ordinary text or records.

UUID/date/timestamp payloads are canonical strings. Duration uses a signed decimal microsecond string. Decimal uses a string coefficient and numeric scale; declared schema precision is checked separately. Bytes use canonical unpadded base64url and retain a 16 MiB decoded bound. Source parsing may normalize equivalent spellings; serde decoding requires canonical forms and revalidates all domain limits. Decimal rescaling is exact and never rounds.

Reserved `unionid::scalar::v1::<type>` newtype markers carry identity to typed serializers. Their version is independent of protocol and storage versions. JSON erases newtype markers; application structs, enums, options, tuples, and lists recover scalar identity through their declared Rust types. These payloads are not protocol-v2 envelopes. Existing application newtypes retain transparent conversion.

Validation lives in `tests/scalars.rs` and `tests/scalar_serde.rs`: logical vectors, calendar boundaries, precision/overflow, canonical rejection, binary limits, nested application ADTs, and the database conversion guard. End-to-end storage and wire validation remains tracked by #137–#140.
