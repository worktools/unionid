# unionid 与 SQLite + SQLx 配对记录 / unionid and SQLite + SQLx paired record

- 采集时间 / Collected at: `2026-09-13T06:16:24+08:00`（日期由此时间戳确定 / date derived from this timestamp）
- 跟踪 / Tracking: [#290](https://github.com/worktools/unionid/issues/290)
- 可重复入口 / Repeatable entry point: `python3 scripts/verify-adt-interop-eval.py`
- 原始样本 / Raw samples: [`docs/benchmarks/data/adt-interop-2026-09-13-1k.json`](../benchmarks/data/adt-interop-2026-09-13-1k.json)

## 中文说明

### 固定场景与公平边界

两端实现同一任务模型：`State` sum type、带 `option (option text)` 的 `Task` product type、固定精度金额与 UTC timestamp。每个独立 release 进程创建单文件数据库，以一次同步持久事务写入 1,000 行，读取 Running 子集并汇总金额，随后新增 `Archived {at}` variant、带默认值的 `priority` 字段，并验证旧行与新 variant。unionid 使用 redb immediate/two-phase 提交；SQLite 使用 WAL、`synchronous=FULL`、foreign keys 和 SQLx 0.8.6。两者都只测试 embedded 单进程，不包含网络和连接池。

业务结果完全相同：500 行 Running、总金额 5,125.00、1,000 行获得默认 `priority = 0`。每次运行使用新文件，五个样本分别进入新进程并记录 peak RSS。

### 接入与维护成本

| 项目 | unionid | SQLite + SQLx |
| --- | --- | --- |
| schema 与 migration | 19 + 3 行；字段直接写 `State`、nested option、`decimal 18 2` | 20 + 3 行；六个逻辑字段展开为十个 v1 列、variant 表、payload/nullability CHECK、presence bit 和缩放整数 |
| 应用模型 | #289 的生成 bundle 直接产生 Rust enum/struct、参数、结果和调用函数 | 应用另写 enum/struct，并维护 tag/payload 解码、未知 tag、presence bit、decimal/timestamp 表示转换 |
| 查询 | `filter match state` 穷尽匹配并对 decimal 求和 | SQL 按 `state_tag` 过滤、对 `price_cents` 求和；恢复 ADT 时执行显式 decoder |
| 初次命令 | `unionid query rust --schema ... --dir ... --output ...`，应用依赖 `unionid` | 准备 SQLite schema 或 migration 数据库，应用依赖 `sqlx`/runtime，并选择在线 `DATABASE_URL` 或离线 metadata 供 checked macros 使用 |
| 维护文件 | schema、migration、`.uid` 查询；生成 `.rs` 是派生物 | SQL schema、SQL migration、SQL query、Rust model/decoder；使用 checked macros 时还维护离线 metadata 或构建数据库 |

评估器故意使用 SQLx runtime query API，使同一 release binary 可以在临时空数据库上自举。因此 SQL 列名、variant payload 和 projection 漂移在执行时暴露。改用 `query!`/`query_as!` 可把可见 SQL 列型问题移到编译期，但需要可连接的 build-time schema 或 `.sqlx` 离线 metadata；它仍不会自动证明 tag/payload 构成穷尽且合法的 Rust sum type，也不会生成 nested option 或 decimal 的领域映射。

### 演进错误出现位置

| 变化 | unionid | SQLite + SQLx runtime API |
| --- | --- | --- |
| 新增 `Archived {at}` | 旧穷尽 `match` 在 query bundle 生成阶段失败；旧 bundle 对新 catalog 在 prepare 阶段返回 `E_SCHEMA_CHANGED` | migration 增加 variant 行和 payload 列；遗漏 decoder 分支只在读到该 tag 时失败 |
| 新增 `priority int = 0` | migration 类型检查后原子补齐；旧客户端读写在 prepare 阶段拒绝 | `ALTER TABLE ... DEFAULT 0` 补齐；旧 projection 仍可能继续运行并忽略新列，直到应用主动更新 |
| projection 改变 | 生成结果 struct 与 schema digest 一起改变；调用方重新编译 | dynamic row 在执行/取列时报错；checked macro 可在 metadata 更新后的编译阶段发现 |

### 本机原始结果

2026-09-13 在 macOS arm64、Rust 1.94 release build 上运行 1,000 行、每端五个独立进程。下表为中位数，仅描述本次样本：

| backend | startup µs | batch write µs | read µs | migration µs | DB bytes | peak RSS bytes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| unionid | 62,036 | 30,037 | 3,008 | 45,732 | 991,232 | 23,871,488 |
| SQLite + SQLx | 2,553 | 7,283 | 135 | 863 | 81,920 | 8,847,360 |

SQLite 在这个小型关系编码负载中明显更快、更小。unionid 当前代价来自 schema/query 解析、完整 typed state 与 migration 重写；本样本不足以推广到其他机器、规模或 workload。unionid 的实际收益位于语义边界：应用和查询共享名义 ADT、穷尽性、精确标量与 schema digest，少维护一套关系编码。若数据天然扁平、SQL 生态和最低资源成本优先，SQLite + SQLx 更合适；若 sum/product 深度进入持久模型、查询和长期演进，unionid 提供 SQLite/SQLx 本身没有的端到端约束。

## English Description

### Frozen scenario and comparison boundary

Both implementations use one task model containing a `State` sum, a `Task` product with `option (option text)`, fixed-precision money, and a UTC timestamp. Each independent release process creates a single-file database, writes 1,000 rows in one synchronous durable transaction, aggregates the Running subset, then adds an `Archived {at}` variant and a defaulted `priority` field. unionid uses redb immediate/two-phase commit; SQLite uses WAL, `synchronous=FULL`, foreign keys, and SQLx 0.8.6. This is an embedded single-process comparison without networking or pooling.

Both sides return 500 Running rows, a 5,125.00 total, and 1,000 existing rows with `priority = 0`. Five fresh processes per backend retain raw timings and peak RSS.

unionid's 19-line schema and 3-line migration preserve the application ADT directly. The SQLite schema expands six logical fields into ten v1 columns plus a variant table, payload constraints, a nested-option presence bit, and scaled integers. unionid's #289 bundle generates Rust models, arguments, results, and calls. The SQLx side separately owns Rust models, tag/payload decoding, invalid-tag handling, presence-bit reconstruction, and scalar representation conversion.

The evaluator uses SQLx's runtime query API so a release binary can bootstrap an empty temporary database. SQL/projection drift therefore appears during execution. `query!` or `query_as!` can move visible SQL column errors to compilation when supplied a build-time database or offline `.sqlx` metadata, but they do not prove that tag/payload columns form an exhaustive Rust sum or generate the domain mapping.

For evolution, unionid rejects a stale exhaustive match during bundle generation and rejects stale bundles against the new catalog with `E_SCHEMA_CHANGED` during prepare. SQLite adds a variant row and payload column; a missing Rust decoder arm remains latent until that tag is read. A defaulted unionid field updates the generated result and schema digest, while an old dynamic SQL projection can silently ignore an added SQLite column.

The five local macOS arm64 release samples have medians of 62,036/30,037/3,008/45,732 µs for unionid startup/write/read/migration and 2,553/7,283/135/863 µs for SQLite + SQLx. Database sizes were 991,232 versus 81,920 bytes, and peak RSS medians were 23,871,488 versus 8,847,360 bytes. These figures describe only this 1,000-row run. SQLite is clearly faster and smaller here. unionid's benefit is the shared nominal ADT, exhaustiveness, exact-scalar, and schema-digest boundary. Prefer SQLite + SQLx for naturally flat data and mature SQL tooling; unionid becomes relevant when sum/product structure must remain consistent across storage, queries, application types, and evolution.
