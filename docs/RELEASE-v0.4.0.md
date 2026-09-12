# unionid v0.4.0 发布说明

v0.4.0 让 schema 与静态查询文件共同成为 Rust 应用的数据接口来源。应用可以生成共享 ADT、查询参数、结果结构和调用函数，并在 schema 演进时尽早获得穷尽性、字段和版本错误，而不再手工同步 `Value`、wire codec 或查询 DTO。

## 主要变化

- `unionid query describe` 输出 version 1 可移植查询契约，包含参数、结果、cardinality、schema identity 与 canonical digest；portable ADT descriptor 和 reference vectors 冻结了 product/sum/tuple/option/list、名义 ID 及六类生产标量的跨语言表示。
- `unionid query rust --schema schema.uid --dir queries --output generated/queries.rs` 为一个目录生成一份共享 Rust ADT 模型，并为每个静态查询生成参数、结果和 Engine 调用函数。单文件 `--file` 入口保持可用。
- 生成前使用真实 binder 完成字段、参数、projection、derive、aggregate、returning、lookup 和穷尽 `match` 检查。生成物携带 schema revision/hash 与 query digest；连接漂移后的 catalog 会在扫描或 mutation 前返回 `E_SCHEMA_CHANGED`。
- `Value::to_serde` 使用直接 typed deserializer，保留 `Option<Option<T>>` 的 `None`、`Some(None)` 和 `Some(Some(value))` 三种状态，同时维持名义类型、嵌套 sum/product/tuple/list 与生产标量表示。
- 仓库维护的独立应用在真实 redb 上验证六类查询、精确标量、嵌套 optional、关闭重开、variant/default-field migration、旧客户端读写拒绝和重新生成后的新客户端。
- SQLite + SQLx 配对记录使用相同任务模型、持久性设置和演进序列。1,000 行本机样本中 SQLite 更快、更小；unionid 的收益是 ADT 约束贯穿存储、查询、Rust 类型与 migration。该证据来自仓库参考应用，不代表外部采用。

## 升级与兼容

v0.4.0 不增加 storage、backup、value/index/receipt/maintenance codec 或 wire protocol 版本。它继续读取 storage format 1–6 和 logical backup 1–4，新库写入 storage format 6 / backup format 4，支持数据协议 1/2 与 stream protocol 1。v0.3.0 数据库可直接打开；升级二进制前仍应保留旧二进制与可校验 backup，并在静止副本上运行 `doctor`、`check`、migration rehearsal 和应用查询。

生成的 Rust 查询 bundle 是 schema/query 的派生物，不承诺跨 schema identity 保持二进制兼容。修改 variant、字段、projection 或查询参数后应重新生成并编译应用。旧 bundle 连接新 catalog 会明确失败，不会悄悄按旧 DTO 解码。

## 安装与边界

发布后可使用：

```bash
cargo install unionid --version 0.4.0 --locked
```

Rust schema derive 同时依赖 `unionid = "0.4.0"` 与 `unionid-derive = "0.4.0"`。最低 Rust 版本仍为 1.94，redb 固定为 4.1.0。正式 workflow 先发布 `unionid-derive`，再发布 `unionid`，最后创建带 Linux/macOS 原生包与 SHA-256 的 GitHub Release。

推荐舒适工作集仍约 10,000 行，100,000 行只是已测试上限。当前没有第二语言 SDK 或外部调用方采用证据；portable descriptor 是实现适配器的契约，并非采用声明。内置认证、TLS、通用扁平 join、window、多写者和分布式执行仍在本版本范围之外。

## English Description

v0.4.0 makes the schema and static query files a shared source for Rust application data interfaces. Applications can generate shared ADTs, query arguments, result structures, and call functions, receiving exhaustiveness, field, and version diagnostics during evolution without manually synchronizing `Value`, wire codecs, or query DTOs.

`unionid query describe` emits the version 1 portable query contract with parameters, results, cardinality, schema identity, and a canonical digest. The portable ADT descriptor and reference vectors cover products, sums, tuples, options, lists, nominal IDs, and all six production scalar families. `unionid query rust --schema schema.uid --dir queries --output generated/queries.rs` emits one shared Rust model plus a public module for each query; the single-file form remains available.

Generation reuses the real binder for fields, parameters, projections, derivations, aggregates, returning clauses, bounded lookups, and exhaustive matches. Generated code pins the schema revision/hash and query digest, and a drifted catalog returns `E_SCHEMA_CHANGED` before scanning or mutation. The direct typed serde deserializer preserves all three nested-option states while retaining nominal and production-scalar representations.

A repository-owned application validates six query forms, precise scalars, nested options, redb reopen, variant/default-field migration, stale-client read/write rejection, and regenerated clients. A paired SQLite + SQLx record uses the same task model, durability, and evolution sequence. SQLite is faster and smaller in the retained local 1,000-row samples; unionid's benefit is the ADT constraint spanning storage, queries, Rust types, and migration. This repository evidence is not external adoption.

This release adds no storage, backup, component-codec, data-protocol, or stream-protocol version. It reads storage formats 1–6 and backups 1–4, writes format 6 / backup 4, and supports data protocols 1/2 plus stream protocol 1. v0.3.0 databases open directly. Retain the old binary and verified backup, then run doctor, check, migration rehearsal, and application queries against a quiescent copy before switching binaries.

Generated Rust query bundles are derived from one schema/query identity and are not binary-compatible promises across schema identities. Regenerate and recompile after changing variants, fields, projections, or parameters. A stale bundle fails explicitly against a new catalog.

After publication, install with `cargo install unionid --version 0.4.0 --locked`. Rust schema derives use both `unionid = "0.4.0"` and `unionid-derive = "0.4.0"`. Rust 1.94 and redb 4.1.0 remain fixed. The comfortable working set remains around 10,000 rows, with 100,000 retained only as a tested ceiling. There is no second-language SDK or external-caller adoption evidence yet; the portable descriptor is an adapter contract, not an adoption claim. Authentication, TLS, general flattened joins, windows, multiple writers, and distributed execution remain outside this release.
