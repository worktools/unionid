# Agents

## 目标

- 构建一个独立运行的 Rust 数据库服务。
- 存储模型基于 `enum`（sum type）与 `struct`（product type）。
- 提供 PRQL 风格（pipeline）的查询语言。
- 支持 TCP 访问与命令行访问。

## MVP 范围

1. 内存数据库（先不做持久化）。
2. 基础 DDL/DML：
   - `create table <name> (<col> <type>, ...)`
   - `insert <table> {key:value,...}`
3. PRQL 风格查询：
   - `from <table> | filter <col> <op> <value> | select <col,...> | limit <n>`
4. TCP 协议：一行请求一行响应（JSON line）。
5. CLI：
   - `server` 启动服务
   - `cli` 连接服务执行单条或交互命令

## 非目标（当前阶段）

- 复杂优化器
- 多写者并发事务
- 分布式与复杂关联查询

## 当前进展（2026-03-05）

- 已完成内存引擎 + TCP + CLI 的 MVP。
- 已增加可选 WAL 持久化：`server --wal-path <path>`。
- 服务启动时会自动回放 WAL，恢复 `create table` 与 `insert` 语句。
- 已增加可选 snapshot 压缩：`server --snapshot-path <path> --snapshot-every <n>`。
- 启动恢复流程为：先加载 snapshot，再回放 WAL 增量。
- 已支持单列索引：`create index <table> (<col>)`，用于等值过滤加速。
- 已支持 Rust 风格参数化 enum 列类型：`enum(A, B(int), C(text,float))`。

## 当前开发进展（2026-09-07）

- 已开始新的无分号语言预览；当前可执行子集见 `docs/LANGUAGE.md`，查询细则见 `docs/QUERY.md`，完整目标见 `docs/DESIGN.md`。
- date/timestamp/duration 已接入 PRQL 风格源码字面量、checked temporal 算术、duration sum、索引/cursor、version 2 Rust/wire/redb/backup 往返与精确 migration parse；不引入隐式本地时间或 calendar 算术。
- REPL 已支持可关闭／可配置路径的安全持久历史、关键字与当前 catalog 补全；`.schema`、`.tables`、`.types`、`.storage` 通过共享 introspection 在 memory、redb 与 version 1 TCP 模式保持一致。
- 类型声明和查询采用 PRQL 风格，优先空格、换行与缩进，不引入分号或 TypeScript 风格的密集注解。
- 复杂 filter 和 match condition 可用缩进块或跨行括号组织；混用 `and` 与 `or` 时规范写法使用括号显式表达分组。括号属于帮助理解的必要符号，应谨慎使用而非一味移除。
- `Engine` 是本地 Rust API、CLI 和 TCP 的共享入口；每次请求是一个原子脚本。schema-aware `prepare` 支持读取、explain 和核心 insert/upsert/update/delete，在扫描前绑定参数类型与完整 DML 结构。
- Rust 嵌入式 API 可通过 `Value::from_serde` 把原生 struct/enum/option/tuple/list 直接绑定到 prepared 参数，并用 `QueryResponse::typed_rows` 从 query 或 DML returning 解码回应用类型；内部 nominal ID 不泄漏到 serde representation。
- 已实现命名 sum/record、tuple、option/list、有限直接自递归 ADT、严格单行／批量插入、主键、filter/select、单键/多键 sort、前 N 行/范围 take、普通 scalar/bool derive，以及支持递归 pattern 与 option/sum/product/list 新值构造的 `filter match` 和 ADT derive。自递归类型经有限可构造性检查，只保存有界树形值；match coverage 会优先选择终止 constructor 生成有限 witness，完整设计见 `docs/rfc/0001-finite-recursive-adts.md`。同一顶层 constructor 可由多个互补嵌套分支覆盖；有预算的 pattern matrix 在扫描前检查穷尽性、不可达分支和积类型相关性。普通 filter、match condition、普通／match derive、typed set 和 migration conversion 共享 int/float 算术与完整 bool 表达式，包括括号、`not/and/or`、字段／binding 间比较、`contains/length`、Option helper，以及有类型、可嵌套、有预算的 `any/all`；整数溢出、除零或非有限 float 返回 `E_ARITH`。未分组 `aggregate` 与 `group ... aggregate` 支持 count/sum/min/max、typed 空输入、命名数值、scalar 输入和完整 ADT key，并限制 group、accumulator cell 和估算状态内存。查询局部 `let` 支持表达式常量与单/多参数非递归纯函数，通过有限推断、字段式注解和有界展开复用现有 expression IR。update/delete 已支持 filter/match/sort/take target、嵌套 record 路径、同时求值的 set、affected rows、主键/索引维护和请求级回滚；`set field = match source` 复用同一 typed match IR 原子转换 sum/option/递归 ADT 或产生 bool，顶层 `current => current` 可保留完整原值。upsert 按主键插入或完整替换 typed row，并保留命中行的 RowId。
- 查询与 mutation 共用绑定后的类型化索引访问计划；`explain` 可显示 full scan、主键／二级索引 lookup、当前候选数、源码 stage 顺序和结果 schema，且不执行数据行。planner 只跳过前置 `let`，不越过其他 stage。
- 已提供 parser 驱动的 `input_status` Rust API，区分 complete/incomplete/invalid 并保留错误 span；本地与 TCP REPL 共用 continuation/ready 状态机，空行和 EOF 不会提交未完成脚本。
- 已提供覆盖当前 v0.1 AST 的规范 `format_source` API 和 `fmt --check` CLI；输出采用固定的无分号布局和必要 precedence 括号，并保持 parse/format 幂等及 schema identity。
- `docs/SCENARIOS.md` 用任务队列、配置、事件、同步和 key/value 工作流维护查询覆盖；#35/#36、#59–#61 已补齐 ADT 派生、布尔/集合表达式、普通派生、基础汇总和查询局部纯函数。
- 已支持 typed unique index：`create unique index table (field.path)` 对 primitive、sum/product、tuple、option 与 list 的完整 typed value 强制唯一，`None` 也占用一个唯一值；ordinary/unique kind 进入 schema hash、migration/diff、backup 与 redb catalog v2，旧 v1 index 按 ordinary 兼容读取。
- 字段可用 `field type = value` 声明默认值；默认值在完整类型体建立后于 schema 阶段类型检查，因此可安全使用自递归类型的终止 constructor，insert 会对嵌套 record 和 sum record 负载逐层补齐。
- 已实现独立于 serde/Rust enum 布局的版本 1 ADT value codec；它用稳定 type/field/variant ID 编码，并已接入 redb `rows` 表。`insert many table <list>` 与 `upsert many table <list>` 可在同一事务校验并写入 typed row list；批量 upsert 拒绝输入内重复主键、保留更新行的 RowId，并返回逐项 action。insert/upsert/update/delete 可用 `returning` 返回完整行或字段投影；update/delete target 可按源码顺序组合 filter、sort 和 take，以稳定选择并修改有限行集。
- 已通过 `docs/adr/0001-redb-storage.md` 选定 redb 作为长期事务后端；`Engine::open_redb`、`run/cli/server --db` 使用固定的 meta/catalog/rows/secondary_index/migration_ledger 表和同步 two-phase 原子提交。提交按稳定 ID/RowId 计算前后状态差异，只删除或写入变化的 catalog/row/index 键，并在覆盖前核对旧值；现有 WAL/snapshot 只保留为过渡兼容入口。
- version 1 `Request`/`Response` 是与 transport 无关的数据协议；TCP 与 HTTP adapter 可复用同一执行入口。Rust 客户端可从 serde struct/enum 构造 typed params，并把无损 wire rows 直接解码回应用类型；`examples/todolist.rs` 通过真实 HTTP 服务验证该路径。
- Engine、version 1 TCP/HTTP 和 Rust request builder 已接入幂等 mutation 回执：相同 key/canonical wire digest 重放原 QueryResponse，不同 digest 冲突，失败不占 key；redb 把数据效果与版本化 receipt 放入同一事务，首个持久回执将 storage format 1 升为 2。打开、完整性检查、进程退出恢复和逻辑 backup v2 均保留并验证回执。status 与按 time/sequence cutoff 的有界 prune 可通过 version 1 和 CLI 预览，只有显式 confirm 才原子删除；HTTP todolist 覆盖提交后丢响应与重试。
- redb transaction commit 前的失败视为明确回滚并允许重试；commit 返回错误视为结果不确定，Engine 关闭句柄并阻止继续写。`check --db` 运行 redb 完整性检查后重新验证 unionid 逻辑状态；子进程测试覆盖提交前/成功提交后直接退出、未知版本、无效文件、索引不一致与跨进程 `E_BUSY`。
- macOS/Linux 子进程使用 OS `RLIMIT_FSIZE` 注入真实 redb 文件增长失败，验证 Engine 的确定／不确定错误分类；重开后完整检查 typed rows、indexes、schema 与 migration ledger 只接受完整旧／新状态。
- `tools/recovery-eval` 可重复生成 10k/100k ADT 工作集并在独立进程测量 open、完整 check、数据库大小和 peak RSS；100k 检查约 0.75 GiB 峰值，作为 v0.1 已测试上限而非日常目标。
- `tools/workload-eval` 在独立 release 进程测量 10k/100k 的 indexed query、scan、update、upsert、100-row batch 和深层 migration，保留完整 microsecond samples、p50/p95 与 peak RSS。实测表明约 10k 行是当前舒适范围；100k 写入接近 1 秒、migration 约 4 秒且峰值约 1.25 GiB，只作为已测试上限。
- `tests/release_scenarios.rs` 以真实 CLI 子进程覆盖任务队列、嵌套配置和 session/cache 的 query/DML → restart → migration → check → backup/restore，并比较源库与还原库的 schema identity、ledger、typed rows 和 explain。migration transform 只展开 binding 的最外层 record，保留内部命名 ADT 身份。
- row 使用表内单调 `u64` 稳定 RowId，索引 posting 不再依赖 `Vec` 位置；每表持久化下一分配值，删除形成的缺口合法且 ID 不复用。旧 redb/snapshot 可从原连续顺序升级。
- `docs/SCHEMA.md` 已定义类型演进契约；类型、字段、变体、表和索引使用统一稳定 ID，每次原子 schema 变更产生一个 revision 与 SHA-256 hash，Engine 响应携带版本信息。`migration name` 已支持 type/field/variant add/drop/rename、typed field/payload conversion、默认值及 key/index 变更，并在全部嵌套引用表中保留 RowId、重建索引和原子回滚；版本化文件 runner、不可变 checksum 和 redb ledger 已接入。
- v0.1 发布闭环使用 `scripts/package-release.py` 生成包含完整文档与示例的原生 target 压缩包、`RELEASE.json` 与 SHA-256；包内教程由 `scripts/verify-release.py` 从空目录验证本地 redb CLI 和 TCP，并由集成测试核对 Rust Engine 的 typed 语义。入门和升级契约分别见 `docs/GETTING_STARTED.md` 与 `docs/UPGRADING.md`；#103 跟踪实际 `v0.1.0` tag 与 GitHub Release。
- README 是面向用户的中英双语产品入口，优先解释“直接用 ADT 描述数据库数据”和“query language 直接理解 ADT”两个核心特点，并保留最短可运行路径。实现历史、测试矩阵、原型兼容、存储内部结构和实时计划分别放在 `docs/DEVELOPMENT.md`、专题文档与 GitHub issues，避免重新堆回 README。
- `Engine::open_redb_read_only`、`run/cli/server --db --read-only` 提供统一只读执行边界；完整解析和参数绑定后、创建候选状态或持久事务前以 `E_READ_ONLY` 拒绝 mutation，`introspection.read_only` 可验证实际状态。
- `ConcurrentEngine` 以 immutable `Arc<Database>`/receipt committed state 提供最多 8 个一致并发读快照；读取在 writer lock 外执行，写入和 maintenance 串行。active/queued read/write 与 peak readers 可观测，deadline/shutdown 限制等待和生命周期；设计与 10k 基准见 `docs/rfc/0006-consistent-read-snapshots.md`。
- 持久幂等写入契约见 `docs/rfc/0002-idempotent-write-receipts.md`：独立 key + canonical digest 保存完整成功回执，目标是 exactly-once effect；不自动 TTL/LRU，首次持久回执原子进入 storage format 2。`request_id` 仍只用于单次尝试关联。
- 稳定分页契约见 `docs/rfc/0003-stable-cursor-pagination.md`：首版使用主键收尾的唯一 keyset 顺序和 sequence-pinned traversal；任意成功写入使旧 cursor 在扫描前明确过期。语言 `page`、Rust API 与 version 1 结构映射同一 PageSpec；HMAC cursor、bounded page 由 #131 实现，接口旅程与取消/streaming 后续切片由 #132 跟踪。
- 六类生产标量已接通源码、Rust wrapper/serde、protocol v2、storage format 4、value/index/receipt codec、cursor、backup 与 migration。decimal 使用 `decimal P S` 和 context typed string literal，支持同类型 checked 加减/negation/sum；乘除、avg 与舍入保持 deferred。
- 生产标量兼容分支已接入 protocol v2 与完整 v1 typed-boundary 预检；新 redb 数据库使用 storage format 4 和 catalog/value/index-key/receipt codec 3/2/2/2，逻辑 backup 使用 codec 3。旧 format 1–3 保持可读且普通写入不隐式升到 4；`upgrade --db <path> --target 4` 在一个同步 two-phase transaction 中重写并验证全部 durable state。`.storage` 与 `check --db` 展示 codec versions；#137 继续跟踪中断和跨 binary 恢复验收。
- `version --format json` 与 `doctor [--db <path>] --format json` 提供 version 1 机器可读的软件、target、protocol、storage/codec 和可选 schema/ledger 摘要；doctor 只检查私有临时副本，不创建、修复或升级请求的数据库。非查询 JSON 命令共用脱敏错误 envelope 和 2–6 分类退出码，query JSON 保持 `QueryResponse` 兼容。
- #135 已拆为 #155 RFC → #156 核心 operation registry/cancellable read → #157 共享 TCP/HTTP NDJSON adapter。RFC 0007 选择 server-issued 128-bit bearer capability、进程内有界 registry、accepted/schema/row/complete/error frame、唯一 terminal 竞态线性化点，以及 frame/channel/bytes/deadline/idle-write 资源上限；stream 不开放 mutation、page 或隐式断线续传。
- 计划通过 GitHub issues 维护，勿因实现了部分能力就将完整阶段标为完成。

## 代码约定（当前）

- 优先小而清晰的模块边界：`model`, `db`, `query`, `server`, `cli`。
- 错误处理统一为可读字符串，保证 TCP/CLI 易观察。
- 默认 UTF-8 文本协议。
- GitHub PR 与 issue 使用中英双语标题；正文分别设置 `## 中文说明` 与 `## English Description`，两部分独立描述问题、范围、验收条件和验证结果。

## 测试与验证

- 至少保证 `cargo check` 通过。
- 当前同时运行 `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings` 和 `cargo test`。
- 集成测试使用隔离临时目录与动态 TCP 端口；不要访问开发者已有数据库。
- 手工验证：
  1. 启动 `server`
  2. 使用 `cli` 建表、插入、查询
