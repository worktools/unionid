# ADT value codec

状态：格式版本 1/2，2026-09-08。本文定义 unionid 逻辑值的持久化编码。实现位于 `src/codec.rs`；storage format 4 的 redb `rows` 表使用 value codec 2，旧 storage format 继续读取 codec 1。过渡 WAL/snapshot 只保留为旧原型兼容入口。

## 目标与调用约束

codec 接收 catalog、期望的 `ScalarType` 和一个逻辑 `Value`。编码前会按期望类型严格检查并规范化值，包括递归补齐字段默认值、解析 sum constructor、写入命名类型／字段／变体的稳定 ID，以及把浮点 `-0.0` 规范化为 `0.0`。解码也必须由 catalog 和期望类型驱动；字节本身不是自描述 schema。

公开入口为：

```rust
encode_value(catalog, ty, value) -> Result<Vec<u8>>
decode_value(catalog, ty, bytes) -> Result<Value>
```

持久值不依赖 Rust enum 的内存布局、serde 数据模型或源码名称。命名类型采用名义身份：两个结构相同但 type ID 不同的类型具有不同字节，也不能相互解码。

## 版本 1/2 格式

所有整数和长度采用大端序。每个独立值由固定头和一个类型驱动的 payload 组成：

| 部分 | 编码 |
| --- | --- |
| magic | ASCII `UIDV`，4 bytes |
| version | `u16`；1 为旧标量词汇，2 增加生产标量 |
| payload | 按调用方提供的类型解释 |

payload 规则如下：

| 类型 | 编码 |
| --- | --- |
| `int` | `i64` |
| `float` | 有限 IEEE-754 `f64` bits；所有零编码为正零 |
| `bool` | tag `u8`：0 为 false，1 为 true |
| `uuid`（v2） | RFC 9562/network order 的固定 16 bytes |
| `date`（v2） | Unix epoch day，`i32` |
| `timestamp`（v2） | Unix epoch microseconds，`i64` |
| `duration`（v2） | microseconds，`i64` |
| `decimal p s`（v2） | 已由目标类型固定 scale 的 coefficient，`i128` |
| `bytes`（v2） | byte length `u32`，随后是原始 bytes |
| `text` | UTF-8 byte length `u32`，随后是 bytes |
| 命名类型引用 | type ID `u64`，随后按该类型定义编码 |
| record | field count `u32`；每项为 field ID `u64` 和字段值 |
| tuple | item count `u32`，随后按声明顺序编码各项 |
| `option T` | tag `u8`：0 为 None，1 后跟一个 `T` |
| `list T` | item count `u32`，随后依次编码各项 |
| sum | variant ID `u64`、argument count `u32`，随后按声明顺序编码负载 |

record 编码前按 field ID 排序，因此源码字段重排不改变字节。type、field 和 variant 的名称均不写入 payload；显式 rename 保留 ID 时，旧字节可由新名称正常解码。tuple 与 sum 负载的位置是其类型契约的一部分。直接自递归类型继续写入同一个 type ID，并只对实际存在的有限子值递归编码；schema manifest 不展开这个引用，因此不需要新的 codec version。codec 1 会预检完整类型图，不能用空 list 或 None 绕过对生产标量的拒绝；codec 2 对旧标量保持相同 payload。

编码是 canonical 的：同一 catalog、类型和逻辑值总是得到相同字节。两个版本都不接受非有限浮点、未知 tag、重复／未知 field ID、未知 variant ID、错误的 type ID、数量与 schema 不符、非法 UTF-8、截断或尾随字节。未知 codec version 直接以 `E_CODEC` 拒绝。

## Schema evolution

值是否可直接复用由稳定身份和形状共同决定：

| 变化 | 版本化值处理 |
| --- | --- |
| 类型、字段或变体显式 rename | 原字节直接可读，保留对应 ID |
| record 字段重排 | 原字节直接可读，field ID 决定映射 |
| 新增 record 字段 | migration 以默认值回填并重写值 |
| 删除 record 字段 | migration 明确确认数据丢弃并重写值 |
| sum 新增变体 | 已有值可读；旧穷尽 match 计划失效 |
| sum 变体负载改变 | migration 转换并重写该变体的值 |
| 字段类型、tuple arity 改变 | migration 转换并重写值 |
| drop 后创建同名对象 | 新 ID，不能解释成旧对象 |

解码器不会在读取时静默补字段、丢字段或按位置猜测身份。需要改变值形状的 migration 必须在一个事务中解码旧值、执行显式转换、用目标 schema 重新编码，并与 catalog、索引及 ledger 一起提交。

## 限制与损坏处理

单个编码值最多 16 MiB，单个 list 最多 1,000,000 项，嵌套深度沿用模型的 64 层上限。该上限同时约束直接自递归 named ref、record、sum、tuple、option 和 list；超限值不能部分编码或解码。解码在分配集合前检查长度，并为嵌套类型错误保留 `value.field[index]` 路径。所有格式错误使用 `E_CODEC`；调用方应将其视为存储损坏或版本不兼容，不能返回部分值。有限递归类型的完整契约见 [RFC 0001](rfc/0001-finite-recursive-adts.md)。

codec 不包含压缩、加密或独立 checksum。redb 负责事务页的完整性与提交原子性；备份／导入层再校验数据库级 schema hash、表行数和文件完整性。格式升级使用新的 header version，并通过显式 `unionid upgrade --db <path> --target 4` 流程原子重写，不能让同一 version 产生两种解释。UUID/bytes 的源码、Rust、wire、codec、redb 与 backup 往返由 `tests/uuid_bytes.rs` 覆盖；完整生产标量兼容边界见 [SCALARS.md](SCALARS.md)。
