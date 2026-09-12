# ADT 数据库与语言互通评估 / ADT database and language interoperability assessment

日期：2026-09-12。代码基线：[main `b4c6a37`](https://github.com/worktools/unionid/tree/b4c6a379e27355e1e92cc24ced4d8b1ce4c7840d)。已 fetch 并确认本地与 origin/main 一致；此时没有开放 PR。本文是评估快照和发展建议，实时范围、优先级和完成状态仍由 GitHub issues 维护。

## 中文说明

计划更新：用户确认后，#262/#263 纳入 v0.3.0，#264/#265 纳入 v0.4.0，联合验收分别由 [#268](https://github.com/worktools/unionid/issues/268) 与 [#269](https://github.com/worktools/unionid/issues/269) 跟踪，最新版本分工见[路线图](../ROADMAP.md)。评估完成后 #267/#277/#278 已交付同步 TCP、HTTP 与异步 TCP typed client，打包后的独立消费者完成 #242 的四入口验收；#270 修复依赖排序，#271 修复 serde 表示漂移，#263 的剩余范围仍开放。下文代码探针保留原基线证据，不继续当作当前行为描述。

### 1. 判断：核心技术命题已经得到验证，应用采用的命题仍待验证

unionid 已经证明，有限 ADT 可以成为数据库的共同数据模型，并贯穿声明、参数绑定、查询构造与解构、索引、事务、持久化和 schema 演进。这比“把 enum 序列化后存起来”前进了很大一步。

目前还不能证明的是：采用 unionid 后，一个真实应用维护领域模型、查询结果和长期数据的总成本，会持续低于成熟数据库加类型化工具的方案。仓库的独立消费项目和进程测试是工程证据；它们不是独立用户的采用证据。[现有路线图](../ROADMAP.md)明确记录目前没有外部使用者，本次检查没有获得新的外部采用证据。因而“失败”应理解为当前没有达到目标的环节，不应推断为市场失败。

建议定位为：**面向 Rust 及其他支持 ADT 的语言、可嵌入也可独立运行的应用状态数据库，让有类型的业务数据能够直接查询并安全演进。** 优先检验任务状态、配置、工作流和有界内容树等场景。redb 继续作为事务后端；后续投入应更多用于应用边界和真实使用验证。

### 2. 最新进度及其实际含义

已发布版本为 v0.2.0；以下包含 main 上的后续开发，不能全部视为已发布能力。

| 方面 | 已验证的进展 | 仍需补齐的边界 |
| --- | --- | --- |
| ADT 模型与语言 | 命名 sum/record、option/list/tuple、有限直接自递归；typed match、构造、derive、聚合和 mutation | 用户泛型与互递归仍在 #118；任意对象图、开放记录和动态数据不属于现有闭合模型 |
| Rust 单一来源 | #256–#258 已实现 schema → Rust 和 Rust derive → schema；#240 已关闭 | 两个方向各有受支持子集；不能据此推导任意 serde 模型或所有数据库约束都可无损互换 |
| 关联读取 | #259 已实现同一快照下、与输入等长同序的 indexed `fetch_by_key`，缺失用 `None` | #241 仍开放：这是 Rust 批量取回 API；查询语言 lookup join 尚未实现 |
| 迁移体验 | #260 增加影响报告，#261 增加离线副本预演；已有 checkpoint、原子 cutover 和恢复 | #243 仍开放；限速回填等剩余范围不能因预演落地就标为完成 |
| 数据可靠性 | 稳定 schema/field/variant ID、RowId、索引、幂等回执、backup、check、恢复和 compact 有成套验证 | 这些是当前数据库状态的保证，不能代替旧应用二进制与新 schema 的兼容检查 |
| 服务与 SDK | 同一 Engine 语义接入嵌入式、同步/异步 TCP 和 HTTP；官方 typed client 覆盖分页、重试、取消和 stream，并由打包后的独立消费者验收 | 暂无跨语言 typed client；应用仍需明确处理 schema/cursor 失效 |

依据：[RFC 0012](../rfc/0012-schema-rust-bindings.md)、[RFC 0013](../rfc/0013-minimal-relational-reads.md)、[#240](https://github.com/worktools/unionid/issues/240)、[#241](https://github.com/worktools/unionid/issues/241)、[#242](https://github.com/worktools/unionid/issues/242)、[#243](https://github.com/worktools/unionid/issues/243)。

容量也应使用最新证据。[M7 验收](../benchmarks/m7-acceptance-2026-09-10.md)在 Apple M1 Pro / 16 GiB / format 6 上，100k 行的 open p95 为 12.02 ms，主键查询 p95 为 24 µs；100 行原子批写 p95 为 641.98 ms，shadow migration p95 为 16.51 s、峰值 RSS 为 427.98 MiB。这些是各自工作负载的测量，不能混成通用吞吐、冷读延迟或 SLA。10k 仍是推荐舒适工作集，100k 是该工作负载的已测上限。本文没有重跑容量评测，也没有据此宣称优于其他数据库。

### 3. 哪些选择成功了

**业务状态可以保留原有形状。** `Pending | Running { worker, attempt } | Failed reason` 将“属于哪个状态”和“这个状态有哪些数据”一起约束。用户不必手工维护一组状态字符串与可空字段的组合规则。数据库还能查询和转换这些结构，因此收益发生在数据处理过程中，而不只发生在编码时。

**类型检查进入了持久数据的生命周期。** match coverage、绑定阶段检查、typed migration、稳定身份和原子回滚共同构成值得继续加强的差异。字段改名、sum payload 转换和嵌套引用处的演进，是数据库比一次性序列化需要多解决的问题。

**轻量数据库的边界比较清楚。** redb、单写者、有界快照、显式维护窗口和资源预算，适合逐步验证应用状态场景。故障分类、退出恢复和维护证据是项目资产。后端选择使 unionid 能自主控制 typed value、catalog 与 migration 的语义，但不证明 SQL 后端无法承载 ADT，也不自动带来性能优势。

**PRQL 风格适合承载数据流。** pipeline 的输入、输出和阶段顺序比较容易解释。下一步应加强每个阶段的输入/输出类型提示、错误定位和 formatter；现有花括号与必要括号应继续用于明确结构和优先级，避免反复改写已可用语法。[PRQL 本身](https://prql-lang.org/)以编译到 SQL 为主要路径；unionid 借鉴其表达方式，仍需自行定义 ADT 查询语义。

### 4. 当前没有达到目标的地方

#### 4.1 “Rust 类型生成成功”还不等于“领域类型可靠互通”

本次用独立临时 Cargo 消费项目，通过公开 API 对上述 main 代码执行了小规模探针；只使用内存 Engine 和合成值，没有访问已有数据库。以下结果均已复现：

| 场景 | 当前结果 | 对应用的影响 |
| --- | --- | --- |
| `TaggedJob` 引用 `TaggedState`，先 add state 再 add job | `SchemaBuilder` 仍按名称排序输出 job 在前，执行得到 `E_SCHEMA: unknown type 'TaggedState'` | 正常嵌套模型能否使用，意外依赖类型名称的字母顺序 |
| struct 使用 `#[serde(rename_all = "camelCase")]`，字段为 `display_name` | derive 生成 `display_name`，序列化提供 `displayName`；prepared insert 返回 `E_FIELD` | 已编译的同一 Rust 模型仍可能在首次写入时失败 |
| enum 使用 `#[serde(tag = "kind", content = "payload")]` | derive 仍生成 sum，但 serde 值表现为 record；写入返回 `E_TYPE` | 支持 Rust enum 不代表支持其所有 serde 表示方式 |
| Rust `i32` 字段 derive 为 `int`，数据库写入 `2147483648` | 数据库接受，`typed_rows` 返回 `E_SERDE`，不能解码为 i32 | 写入 Rust 类型的值可用，不代表数据库允许的全部值都能读回该 Rust 类型 |
| schema 的 `UserId = text` 和 `OrderId = text` | codegen 输出两个 `String` 类型别名 | Rust 编译器不能利用这些别名区分两个业务 ID |

直接依据：[SchemaBuilder](../../src/schema.rs)、[derive 实现](../../derive/src/lib.rs)、[Rust codegen](../../src/codegen.rs)。名义类型的数据库内部身份与 serde 边界有意分离；这里的判断是宿主侧没有完整保留领域区别，不是数据库丢失了持久 ID。字段检查和解码错误也说明当前路径是显式失败，而不是这些探针已造成静默坏数据。

后续实现已经按这一证据推进：#270 修复依赖排序，#271 对齐或拒绝 serde 表示；#263 第二个切片拒绝无法覆盖数据库完整值域的窄数值 derive，把命名 scalar/tuple 生成为 Rust newtype，并明确 decimal P/S 由 schema 边界检查。#263 最后一个切片增加 Rust-first 默认值与单字段索引元数据，让 `SchemaBuilder::build()` 校验完整生成结果，并用嵌套 job queue 模型覆盖默认补齐、typed 写读、唯一约束和索引计划。复合/嵌套索引继续由 schema-first 工作流表达；模块命名不属于表模型映射的前置。

#### 4.2 查询结果类型仍是另一份模型

`from tasks | select { id, title }` 的结果不是完整 `Task`；聚合、derive 和未来关联读取也会产生新形状。`typed_rows::<T>()` 提供运行时解码，不会仅凭 Rust 编译就证明字符串查询的结果一定是 `T`。

这意味着，消除表模型的重复声明还没有消除查询接口的重复声明。最值得补的是从 **schema + 查询源码** 生成参数类型、结果类型和可调用函数，并在生成阶段复用现有 binder 检查字段、variant 和表达式。默认值场景也要区分完整行类型、允许省略字段的 insert input 与 update input；一个 struct 不必承担三种不同契约。

#### 4.3 ADT 数据合法性不能包办业务不变量

合法的状态形状不自动保证合法的状态转移。例如“同一任务只能由一个 worker 从 Pending 领取”，仍需要原子条件更新、受影响行数检查及重试规则。现有 filter/update/returning 可作为基础，应先给出可执行的条件写入范式，不急于增加专门的状态机语言。

有限递归 ADT 适合有界树。共享实体、循环关系和独立生命周期的数据通常需要 ID 引用与关联读取。嵌套 list 不应成为无限历史容器；[应用数据边界](../APPLICATION_DATA.md)已经说明摘要/正文分层和按需读取，也明确投影不保证避免整行解码。应继续解释什么时候内嵌、什么时候拆表，以及引用类型与外键完整性是两个不同保证。

#### 4.4 穷尽检查的优势在演进时也会形成压力

为 sum 添加 variant，可能让旧查询不再穷尽，或者让旧客户端无法解码新值。数据库 migration 成功不代表应用升级完成。显式 `_` 分支本来就允许兜底，因此不能宣传为“新增 variant 一定会在所有使用点报错”。

现有 [schema 契约](../SCHEMA.md)已区分兼容性，当前消费项目验收也明确不验证旧客户端。后续需要把这种区分落实到查询和生成客户端的兼容报告，而不是重新承诺所有历史版本兼容。

迁移限速同样要说明边界：在普通写入被维护阶段阻止的现有契约下，限速可降低瞬时资源压力，但可能延长维护时间，不能据此宣称缩短写阻塞或实现零停机。写入持续可用需要独立设计和证据。

### 5. 业界探索说明了什么

“更多语言提供 ADT 构件”是合理观察，“所有语言正在统一到同一个 ADT 类型系统”则过强。例如 [Kotlin sealed + when](https://kotlinlang.org/docs/sealed-classes.html)支持封闭分支检查，[TypeScript discriminated unions + never](https://www.typescriptlang.org/docs/handbook/2/narrowing.html#exhaustiveness-checking)支持类型收窄和穷尽性模式，但它们与 Rust/OCaml 的名义 sum 在运行时表示、结构相容和序列化上并不相同。机会在这些边界，不只是类型声明语法。

以下比较只依据官方能力文档，没有进行竞品性能或采用率测试。“启发”是本次评估的判断。

| 探索 | 官方资料展示的能力 | 对 unionid 的启发 |
| --- | --- | --- |
| SpacetimeDB | SATS 数据模型支持 sum/product；有生成式多语言 SDK。其 SQL 文档明确说 SQL 本身尚不能构造这些类型或对其使用 scalar operators。[SQL](https://spacetimedb.com/docs/reference/sql/#data-types)、[客户端](https://spacetimedb.com/docs/clients/) | 这是直接相关的方案，不能宣称 ADT 存储独有。unionid 可以突出查询语言直接解构、构造 ADT 的深度，并学习完整绑定流程；无需照搬实时订阅与服务端应用运行时 |
| DuckDB | `UNION` 是带 tag 的和类型，可用 `union_value` 构造、`union_extract` 解构，并参与嵌套数据处理。[UNION 文档](https://duckdb.org/docs/current/sql/data_types/union) | “SQL 只能处理扁平表”不成立。应比较完整闭合 ADT 检查、宿主映射和演进体验；不要只比较是否有 enum |
| Gel | 提供从 `.edgeql` 文件生成带参数/返回类型函数的工具，也提供推断结果形状与基数的 TypeScript query builder。[查询生成](https://docs.geldata.com/reference/using/js/queries)、[query builder](https://docs.geldata.com/reference/using/js/querybuilder) | 最直接可借鉴的是“查询也生成类型”，不必先实现复杂宿主 DSL；其对象/关系模型与 unionid 的闭合 sum 仍有区别 |
| Convex | 同一个 validator builder 描述 schema、union 和参数校验，并生成应用侧 TypeScript 类型。[Schema](https://docs.convex.dev/database/schemas) | 用户可通过熟悉的语言获得 ADT 风格收益。unionid 应减少编码和重复定义的负担，同时保留独立数据库及轻量嵌入的定位 |
| SQLx | 宏检查 SQL 参数与结果列类型，支持从离线元数据构建。[query!](https://docs.rs/sqlx/latest/sqlx/macro.query.html) | 成熟数据库加类型工具是实际比较基线。unionid 需要证明 sum payload 查询和演进带来的额外收益，而不只证明有 typed Rust API |
| Irmin | OCaml 库提供自定义类型序列化，以及可分支、可合并的数据存储。[项目文档](https://irmin.org/) | ADT 与持久化结合有既有探索；应选一个具体应用价值来验证，而不同时追求分支、同步、分析等所有方向 |
| WIT / Protobuf | WIT 用 record、variant、option、result 等描述语言间契约；Protobuf 文档明确列出 oneof 演进与未知字段的限制。[WIT](https://component-model.bytecodealliance.org/design/wit.html)、[oneof 演进](https://protobuf.dev/programming-guides/proto3/#backwards-compatibility-issues) | 先定义可移植的数据契约和版本规则，再实现语言适配。借鉴契约设计，不等于采用其 wire format 或引入 Wasm 运行时 |

### 6. 让 ADT 与编程语言更顺畅地互通

#### 6.1 以一个权威来源生成，不做隐式双向同步

Rust 主导的嵌入式项目可选择 Rust derive 为权威来源；多语言项目可选择 `.uid` schema 为权威来源。另一侧为生成物，CI 检查漂移。把 migration 视为需要评审的数据变换，而不是每次启动把数据库自动改成当前 Rust struct。

为现有 catalog/type IR 增加适合工具消费的、版本化的公共描述接口。复用现有绑定和类型规则，包含：类型结构、命名作用域、默认值、数值约束、schema 身份，以及查询参数和结果描述。稳定 ID 在 catalog 的作用域内有意义，不能把 Rust `TypeId`、内存布局或生成顺序作为跨数据库的全局身份。

#### 6.2 首先生成查询函数

推荐工作流是：权威 schema → 校验查询文件 → 生成 `LoadTaskArgs` / `LoadTaskRow` / 调用函数 → 在同步 Engine 或 #242 SDK 上执行。这里的名字仅说明生成物，不是新增查询语法。

首版可以只支持静态查询文件和有明确类型的参数，不必以完整 ORM、查询 builder 或服务器持久化命名查询为前提。未支持的推断返回明确诊断。生成的函数仍提交原有参数化查询，不引入第二套执行器；schema 不匹配时也仍需运行时检查，因为编译过的应用可能连接到另一版数据库。

结果条数应独立表达：集合、可选单行、恰好单行的语义要有检查。仅有 `take 1` 不能证明基础关系唯一；更不能把空集、缺失关联、`None` 和一个字段允许省略混成一种情况。

#### 6.3 无损映射优先于表面上直接使用普通对象

| 数据语义 | Rust | TypeScript 适配建议 | 契约要求 |
| --- | --- | --- | --- |
| 命名 product / sum | struct / enum | object / 显式 discriminated union | 保留分支与 payload 对应关系，生成运行时校验器 |
| 领域 ID | newtype | branded type + 运行时校验 | `UserId` 与 `OrderId` 的区别不应只剩注释 |
| 嵌套 option | `Option<Option<T>>` | 必要时显式 tag wrapper | `None`、`Some(None)`、`Some(Some(v))` 不合并 |
| int、decimal、时间与 bytes | i64、既有 scalar wrappers | bigint / 精确 wrapper / 明确时间与字节表示 | 不经过会损失精度的普通 JSON number；明确 decimal P/S 与 timestamp 单位 |
| 有限递归 | Box / Vec | 有界递归值 | 限深、限大小，并拒绝循环对象；不保留指针共享身份 |
| 缺失字段 / 默认值 / 部分更新 | 单独 input 类型 | 单独 input 类型 | omission 不等于 None；输入省略规则不污染完整行类型 |

这是适配建议，不是当前 TypeScript SDK 能力承诺。对任意 TS union、Rust trait object、借用对象、闭包、GADT 或带行为的对象，不承诺自动持久化。一个有文档且可检验的可移植子集，比“支持所有语言类型”更实用。

先把 Rust 作为参考实现。第二种语言由真实调用方选择：既有 #192 的 Calcit 调用需求优先调查；若目标是扩大一般应用采用，TypeScript 是值得验证的候选。OCaml/F# 可作为后续功能语言适配候选。不要同时承诺一整套语言 SDK；每接入一种语言都运行相同的语义测试向量。

#### 6.4 把演进分成四类兼容性

| 变更 | 应检查什么 |
| --- | --- |
| 添加 variant | 旧数据通常仍合法，但旧客户端解码、无兜底 match 和写入方支持范围需分别检查 |
| 字段/variant 改名 | 稳定持久 ID 可保留身份，源码名称和 serde/wire 名称仍可能使旧应用失效 |
| 添加有默认值的字段 | 数据补齐与输入省略行为之外，还要检查旧客户端读写的字段形状 |
| 收窄数值范围或更改 decimal | 现有数据、查询算术及客户端表示能力都可能需要验证 |
| 修改返回投影 | 表 schema 可以不变，生成查询函数的接口仍已改变 |

依次报告存量数据兼容、查询兼容、客户端读兼容、客户端写兼容。对当前基线构造两版应用的测试即可，不重新打开历史 v0.1 兼容承诺。未知 variant 默认明确失败；若真实旧客户端需要透传，另设显式 `Unknown` 边界包装与策略，不能把错误值自动改成 None 或某个业务默认状态。

### 7. 后续推进顺序

| 顺序 | 工作 | 与现有计划的关系 | 可验证的完成条件 |
| --- | --- | --- | --- |
| A | 修正 SchemaBuilder 依赖输出，明确 derive/serde 支持范围 | #240 的独立质量跟进，不否定已经交付的生成能力 | 更换类型命名与 add 顺序仍可用；不支持的 serde 表示在生成阶段有诊断；使用生成物完成真实写读 |
| B | 完成 async/typed SDK，并增加静态查询文件绑定 | 继续 #242；查询绑定与 #120 协同，但不要求先实现服务端命名查询 | 应用不用手工构造 Value、结果 DTO 或管理 blocking worker；错字段/分支在生成阶段失败 |
| C | 用有界关联读与 migration 完成一个中等复杂场景 | 继续 #241、#243 | 引用缺失以 option 表达、索引计划明确；一次新增 variant + 字段变换 + 索引变更完成预演与恢复 |
| D | 冻结跨语言契约，接入一个真实第二语言调用方 | #192 继续按需求进入；先完成规范与测试向量 | 核心 ADT、极值标量、嵌套 option、错误和两版应用演进逐项一致 |

上述顺序已进入版本计划：先完成 v0.3 接入质量，再完成 v0.4 查询绑定与互通契约。修复现有正确性问题优先于扩张承诺。

本次已建立独立跟踪：[#262 依赖顺序](https://github.com/worktools/unionid/issues/262)、[#263 Rust 映射保真](https://github.com/worktools/unionid/issues/263)、[#264 查询文件绑定](https://github.com/worktools/unionid/issues/264)、[#265 可移植契约与客户端演进](https://github.com/worktools/unionid/issues/265)。#262/#263 跟进 v0.3 的现有 Rust 接入质量；#264/#265 已纳入 v0.4.0，由 #269 联合验收。它们不关闭或替代 #241–#243、#118、#120、#192。

泛型/互递归 #118、开放 map/JSON #246、表达式/部分索引 #248 应由具体模型推动。泛型可先评估有限实例化的 `Result<T,E>` 等常见数据模板；typed map 可以先于任意开放对象。只有实际 `filter match` 计划显示出必要性时，再据 #248 增加 variant/payload 的索引策略。子查询、窗口、分布式和 >100k 架构继续保留各自需求门槛。

### 8. 用什么验证这一方向值得继续

选择一个真实调用方，完成“嵌套任务状态 + 用户引用 + 内容摘要/正文”的小型应用。它应经历首次建模、参数写入、结果投影、领取任务、分页、进程重启、一次增加 variant 和一次字段转换；第二语言参与时再加入旧/新客户端组合。SDK 不应承诺在成功写入后透明恢复已过期 cursor；应将现有失效语义交给应用清楚处理。

与 SQLite + SQLx 的同等应用实现做一次配对记录，使用同一业务约束、持久性要求和数据集，记录：手写模型/转换代码、首次接入步骤、一次 schema 变更需修改的文件、错误首次暴露阶段，以及常用请求延迟和内存。关系方案允许使用合理的约束或 tagged JSON；不要用刻意糟糕的扁平设计作为对照。

建议的采用验收不是“又支持几个类型”，而是：模型只改一次；查询结果不手写同步；错误能在生成阶段定位；正常接入不手写 wire 编解码；一次真实升级有完整文档和可执行验证。随后观察独立使用者能否在没有维护者带领的情况下完成教程和变更。如果收益始终只停留在类型定义更漂亮，就应收缩表达力扩张，把精力投向绑定、工具和明确的应用场景。

## English Description

Planning update: after user confirmation, #262/#263 target v0.3.0 and #264/#265 target v0.4.0, with joint acceptance in #268 and #269. ROADMAP records current assignments. #267/#277/#278 delivered the synchronous TCP, HTTP, and asynchronous TCP typed clients, and a packaged independent consumer completes the four-entry-point acceptance for #242. #270 fixed dependency ordering, #271 addressed serde representation drift, and #263 retains its remaining scope. The code probes below preserve their original baseline and no longer describe current behavior.

### Assessment

As of main `b4c6a37` on September 12, 2026, unionid has validated its core technical proposition: finite ADTs can share semantics across schema declarations, typed queries and mutations, indexes, persistence, and migrations. The released version is v0.2.0; recent main additions include both Rust/schema generation directions, indexed batch fetch, migration impact reporting, and offline rehearsal. Batch fetch is not a query-language join, and #241–#243 still have remaining scope.

Product adoption remains unproven. Repository-owned consumer and recovery tests are valuable engineering evidence, but they do not establish that independent applications are cheaper to build and evolve than with an established database and typed tooling. Existing documentation reports no external users; this assessment found no new evidence to change that conclusion.

### Observed gaps

An independent temporary Rust consumer reproduced five boundaries through public APIs: SchemaBuilder sorted declarations by name rather than dependency; serde camelCase renaming disagreed with derived field names; adjacently tagged enums serialized differently from the generated sum schema; an i32-derived database field accepted values that could not decode back into i32; and named scalar types generated Rust aliases that did not preserve domain-ID distinctions. #270 fixed dependency ordering and #271 aligned or rejected serde representations. The second #263 slice rejects narrow numeric derives, emits named scalar/tuple newtypes, and documents schema-bound decimal P/S enforcement. The final #263 slice adds derive metadata for defaults and single-field indexes, validates the assembled schema during `SchemaBuilder::build`, and exercises a nested job-queue model through default filling, typed writes/reads, uniqueness, and indexed planning. These probes use synthetic data, not existing databases or a performance benchmark.

Generated table models also do not establish query-specific parameter and result types. Successful data migration does not prove compatibility with previously built clients or exhaustive queries. ADT shape validity does not enforce business transitions or foreign-key integrity. Throttling maintenance may lower resource pressure while extending the write-blocking interval.

### Direction and validation

Keep the bounded embedded/standalone application-state focus and the redb backend. Prioritize existing mapping correctness, the official SDK in #242, and generated functions from schema plus static query files. Reuse the existing binder and execution semantics. Continue bounded relational reads in #241 and the remaining migration workflow in #243.

Official SpacetimeDB, DuckDB, Gel, Convex, SQLx, Irmin, WIT, and Protobuf documentation is linked beside the comparisons above. These projects demonstrate related capabilities, not a measured competitor ranking. The most actionable lessons are generated query contracts, a precise portable value model, and explicit evolution rules. An ADT-supporting database is not unique merely because it can store a sum.

Use one authoritative model source per application, generate the other representations, and expose a versioned description of existing type/query metadata. Preserve options, scalar precision, and domain identities; diagnose unsupported mappings. Keep data, query, client-read, and client-write compatibility separate. Start with Rust and one demand-driven second-language integration under #192 rather than promising many SDKs. This does not reopen historical compatibility commitments.

Follow-ups are tracked in #262 (dependency order), #263 (Rust mapping fidelity), #264 (query-file bindings), and #265 (portable contracts and client evolution). The first two follow existing v0.3 integration quality; the latter two now target v0.4.0 with joint acceptance in #269. Existing issues remain independent.

Validate adoption with a real application and a fair SQLite + SQLx comparison: initial integration steps, manual conversion code, files changed during evolution, when mistakes are detected, and representative latency/memory. New proposals do not automatically become v0.3 release gates. Current implementation status remains in GitHub issues.
