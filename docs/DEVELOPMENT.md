# 开发与验证记录

本文保留 v0.1 开发期间的实现与验证细节，不承担实时状态导航。v0.1.0 已通过 GitHub Actions 发布；发布后的生产边界与应用体验工作由 [ROADMAP.md](ROADMAP.md) 和 GitHub issues 跟踪。当前语法见 [LANGUAGE.md](LANGUAGE.md)，查询的详细语义见 [QUERY.md](QUERY.md)，面向用户的入口见 [README](../README.md)。

## 发布后进展

- filter comparison、sort、min/max 与 cursor boundary 现在保存绑定静态类型，并共用 catalog-aware ADT total order。primitive、命名 scalar、sum、record、tuple、option、list 和有限递归值均可排序；sum/record 使用稳定 variant/field ID，避免运行时名称或 map 次序改变语义。复合 index schema、memory tuple key、redb index-key codec 3、storage format 5、backup 4 与显式 4→5 upgrade 已接入。planner 支持 equality prefix、紧邻 range、声明方向／全局反向的 index order，以及主键或完整 unique suffix 证明下的 page seek；query 和 mutation target 会在 residual filter 后有界满足 `take/page`。
- 普通 row-only DML 使用按路径复制的 persistent row/index/receipt roots 和请求级合并 write set；Engine 通过一个 committed root 同时发布 database 与 receipt。redb 直接编码并核对变化的 catalog/row/index/receipt stable keys，常驻 durable head 只保留 layout、meta 与兼容状态；DDL、migration、upgrade、restore 和 receipt prune 明确走临时 full-rebuild 路径。`MutationProfile` 与 `tools/workload-eval` 分别记录 candidate build、durable commit、增量模式及不含业务值的 write-set 计数。
- `unionid::scalars` 提供六种生产标量的 Rust 值域、规范 serde payload 与边界校验；原生 ScalarType/Value、无损 serde、protocol v2、完整 v1 typed-boundary 预检、按 boundary 选择的 `u1`/`u2` cursor、storage format 4 标量兼容层、当前 format 5 durable codec、显式 upgrader、源码声明与精确运算均已接通。#137–#140 的验收由 PR #147–#151 完成，已实现范围见 [SCALARS.md](SCALARS.md)。
- v0.1.0 crate、原生 target 包、校验清单和 GitHub Release 已由 tag workflow 发布。
- Engine、本地 run/CLI 和 TCP 服务已提供统一只读执行边界；`introspection.read_only` 可验证实际状态，mutation 在创建候选状态或持久事务前返回 `E_READ_ONLY`。
- 查询语言、Rust Engine 和 version 1 协议共享有界 keyset `page`：唯一排序以主键收尾，`u1` cursor 绑定 schema/query/params/sequence 并使用数据库 HMAC secret；redb 重开保留身份，逻辑 restore 轮换身份。
- Rust/TCP/HTTP 可用 `TypedPage<T>`、`PageInfo::next_page/previous_page` 和结构化 `PageSpec` 遍历相同 typed rows；HTTP adapter 通过 `ConcurrentEngine::execute_protocol_request_until` 设置绝对 deadline，并在一致 committed snapshot 上并发读取。真实接口场景覆盖重开续页、cursor 拒绝、migration 失效和客户端断开。
- stream protocol version 1 以 server-issued capability、共享有界 NDJSON producer 和独立 cancel control 扩展长只读查询；TCP 内置服务与真实 Axum todolist adapter 消费同一 typed frame receiver，慢 consumer 不延长 snapshot 生命周期。
- 后续工作按生产正确性、用户体验和语言探索拆分，不再把宽泛目标或已完成发布步骤保留为“当前任务”。

## 已实现

- `set { ... }` 支持多字段、嵌套 record 路径与 braced match；所有 assignment 继续从旧行同时求值。formatter 把重复 set 归并为字段集，单项保持简写；prepared 参数、returning 预算、约束与算术失败、redb 重开及 TCP/HTTP 更新路径有回归覆盖。derive/select 字段集与 computed select 也已降为现有顺序 derive/projection IR，允许查询内同名列替换并保持列顺序；覆盖主键的 page 在扫描前拒绝。

- PRQL 风格结构语法已扩展到当前可执行子集：命名 record 与 constructor record payload、insert/upsert record、match branch、aggregate field 和 group inner pipeline 使用有语义的 `{}`／`()` 与逗号边界；旧缩进 record/match/group 继续解析以兼容 migration 与过渡 WAL。canonical formatter 输出新布局，过长布尔表达式会在括号与 `and`／`or` 边界换行，缺少 branch/aggregate 逗号时返回带上下文的源码诊断。
- 共享 Rust `Engine`，本地 CLI 与 TCP 复用同一个执行、类型校验和原子提交边界。
- Lexer、源码位置、缩进与换行 AST；命名 sum/record、tuple、option/list、有限直接自递归 ADT、完整值构造和严格校验。自递归类型必须至少有一个有限值，互递归与用户泛型仍延后。
- 类型、字段和变体的单调递增 catalog ID；命名类型相等检查身份，schema 展示可重新解析。
- `field type = value` 字段默认值在完整命名类型体建立后完成递归类型检查，可使用自递归类型的终止 constructor；insert 对嵌套 record 和 sum record 负载逐层补齐，schema 展示、WAL/snapshot 恢复与 hash 均保留默认值。
- 版本 1 ADT value codec 以 catalog 和期望类型驱动，用稳定 type/field/variant ID 编码命名类型、record、sum、tuple、option/list 及有限递归子值；不依赖 serde 或 Rust enum 布局，字段重排和显式 rename 保持字节可解释。
- redb 4.1 已接入共享 Engine：`run --db`、`cli --db` 与 `server --db` 使用同一个持久文件；普通 DML 根据合并 write set 只准备变化的 catalog、ADT row、secondary-index 与 receipt 键，并与 meta 放入一个 `Immediate`、two-phase write transaction。版本化 migration 在同一事务追加 ledger entry，普通数据提交不触碰 ledger。所有增量覆盖都核对旧值与 meta head；打开时校验存储／catalog／value／索引／migration 编码版本、schema hash、ledger head、RowId 水位和派生索引一致性。
- 每条 row 带表内稳定 `u64` RowId，每表持久化单调分配游标；索引 posting 不再保存 `Vec` 下标。删除形成的 ID 缺口可安全恢复，后续插入不会复用旧身份；旧 redb 和 snapshot 会从原有连续顺序升级。
- `update table`/`delete table` 复用普通与 match filter，并可按源码顺序 sort/take 出稳定 RowId 子集；多个 typed `set` 从原 row 同时求值，可设置嵌套 record 路径。`set field = match source` 直接复用 derive 的 pattern coverage 和 typed value construction，顶层 binding 可保留完整 sum/option/递归 ADT 原值，分支参数经 version 1 TCP 绑定。执行器先生成全部候选 row，再检查完整类型和主键唯一性并重建该表索引；失败请求不发布任何修改。全部 DML 响应包含 `affected_rows`，可选 `returning` 在提交前完成投影、行数和 typed wire 大小检查，并返回 insert/upsert/update 后像或 delete 前像。
- `upsert table value` 与 `upsert many table <list>` 要求声明主键，输入按完整 row 与默认值规则检查；未命中时按输入顺序分配 RowId，命中时整行替换并保留 RowId。批量形式拒绝输入内重复主键，完成全部候选值后统一验证主键和 unique indexes；响应以 `upsert_action` 或逐项 `upsert_actions` 区分 inserted/updated，索引和 redb 状态服从同一请求级提交边界。
- redb 提交错误按边界区分：transaction commit 前的确定失败回滚候选状态并保留句柄，commit 调用返回错误时标记结果不确定、关闭句柄并阻止后续写入。可注入 backend 单元测试验证两条 Engine 状态路径；真实子进程测试分别在未提交多表 transaction 与成功 Engine commit 后直接退出并重开。
- macOS/Linux 子进程通过 OS `RLIMIT_FSIZE` 强制真实 redb 文件增长失败；Engine 按失败点返回确定中止或结果不确定，父进程重开并验证 typed row、索引、schema、ledger 与完整性只处于完整旧／新状态。
- `tools/recovery-eval` 在独立进程测量完整 ADT 工作集的 open/check 与 peak RSS；本机三次中位数为 10k 行 66/78 ms、52.67/81.48 MiB，100k 行 653/741 ms、459.28/745.84 MiB。CI 在 macOS/Linux 跑 100 行 smoke，完整环境与容量解释见恢复 benchmark 记录。
- `tests/release_scenarios.rs` 从隔离临时目录驱动真实 CLI 子进程，覆盖任务条件状态转换、深层命名 ADT 配置迁移和 session key 生命周期；每条链路均跨重启执行 migration、check、backup/restore，并比较 schema identity、ledger、typed rows 与 explain。场景暴露并修复了 migration transform 递归展开嵌套命名 record、导致 `old.retry` 丢失类型身份的问题。
- `tools/workload-eval` 使用新数据库副本和 release 子进程保留原始样本，测量 indexed query/full scan、条件 update、upsert、100-row batch 与深层 migration，并将 candidate build 与 durable commit 分段报告。M6 之前的基线是 10k 行写入 p95 约 72–94 ms、migration 344 ms；100k 行写入 p95 约 0.51–0.88 s、migration 4.15 s，写入／迁移 peak RSS 约 1.1–1.29 GiB。更新后的容量结论由 #166 在复合有序访问完成后统一重测。
- `check --db` 调用 redb `check_integrity`，再重新加载并验证 unionid 逻辑状态；无效数据库文件、未知 codec 版本、索引不一致和跨进程占用都有结构化诊断。
- 换行及单行 pipeline、字段路径、filter/select、单键/多键 sort、前 N 行/范围 take、普通 scalar/bool derive、sum/option 的模式过滤与 ADT derive、主键唯一性和等值索引。普通 filter、match condition 与 derive result 共用有类型的 scalar expression，支持带 checked 错误的 int/float 算术；filter/match condition 和普通 derive 还支持括号、`not/and/or`、字段或 binding 间比较、list `contains`、list/text `length`、Option helper 与有预算的嵌套 `any/all`。复杂条件可使用 `filter`／`=>` 后的缩进块或跨行括号。
- 未分组 `aggregate` 与 `group ... aggregate` 支持 count/sum/min/max；绑定阶段确定输入与输出类型，sum 保留命名数值类型，min/max 返回 option。group key 使用完整 typed equality，可包含 ADT；后续 filter/select/sort/take 使用汇总后的 schema。group 数量、accumulator cell 与估算状态内存都有显式上限。
- 查询局部 `let` 支持表达式常量与单/多参数非递归纯函数；调用以空格应用，参数优先从字段驱动的使用位置推断，歧义时使用字段式参数/结果注解。定义按词法作用域捕获更早的定义并允许后续遮蔽，函数可在 filter、derive、match binding 与 aggregate scalar 输入中复用。绑定阶段展开为现有 typed expression IR，并限制定义数、调用深度和展开步骤。
- 查询和 mutation 共用绑定后的访问计划；开头的单纯有索引等值 filter（允许前置 let）选择主键或二级索引 posting，否则全表扫描。`explain` 不执行数据行，返回结构化访问方式、lookup 条件、当前候选数、源码 stage 顺序和结果 schema，并可经 Rust prepared query 与 version 1 TCP 传递。
- match 支持 sum 的 unit/record/位置负载、option 的 None/Some，以及 record/tuple/sum/option 的递归 pattern；有预算的 pattern matrix 允许同一顶层 constructor 使用互补嵌套分支，并检查完整穷尽性、不可达分支和积类型相关性。record 字段可重命名绑定，分支可从 binding 递归构造 option/sum/record/tuple/list，并静态统一结果类型。命名 record 构造会补齐字段默认值；派生列可继续 filter/sort/select，空表也执行全部检查。
- 以任务队列、嵌套配置、事件收件箱、离线同步和 session/cache 推演 ADT 与日常操作，形成 [实际场景与查询覆盖矩阵](SCENARIOS.md)；ADT 派生和集合查询分别拆为 #35/#36。
- 查询参考明确记录当前 grammar、stage schema、执行顺序、match 规则、错误类别和已实现/计划边界；任务、配置、事件三个示例都由语言测试执行。
- 原子脚本、可读 CLI 输出、JSON 输出、文件/stdin、多行 REPL、正确退出码及 EOF 处理。公开 `input_status` 以 parser 状态区分 complete/incomplete/invalid；本地与 TCP REPL 共用 ready/continuation 状态机，空行和 EOF 不会误提交未完成脚本。
- 覆盖当前 v0.1 AST 的规范 formatter 与 `fmt --check` 已接入；类型、DML、migration、query/explain、pattern/value 和表达式按固定布局及 precedence 输出，并验证幂等、执行结果和 schema identity round-trip。
- REPL 使用 parser 状态驱动多行输入，并增加关键字／catalog Tab 补全与版本化 JSON Lines 历史。历史默认排除 DDL/DML/migration 和含注释、文本／数字字面量的输入；损坏或超限文件会禁用本次持久化且保持原文件。`.schema`、`.tables`、`.types`、`.storage` 共用 Engine introspection，在 memory、redb 和 version 1 TCP 下保持一致。
- 修复大整数、浮点零值与 enum 负载的索引/扫描一致性；深层索引键按结构编码，避免重复转义造成指数增长；未知字段在空表上也报错。
- 用隔离的临时目录、动态 TCP 端口和子进程建立测试；增加 macOS/Linux 的 CI 配置。
- 服务使用 64 个活动连接的有界等待集合；达到上限时返回结构化 `E_BUSY`，拒绝过程不创建 worker。请求/源码/value/query working rows/result rows/response bytes 与 pattern analysis 都有明确上限，服务执行 deadline 返回 `E_TIMEOUT` 且不发布候选事务。response 通过限长 writer 直接编码，避免先创建无界 JSON byte buffer。
- SIGINT/SIGTERM 停止 accept，关闭空闲连接，等待已进入 Engine 的请求完成或原子放弃，再释放 redb 锁；关闭时输出 accepted/rejected/requests/failed 统计。真实子进程测试验证 idle client 存在时仍能退出并立即 reopen。
- JSON Lines version 1 请求包含 request ID、完整多行源码、typed params 与可选 schema 前置条件；也支持独立且 1 MiB 有界的 introspection 动作。响应回显 ID，并通过独立 wire codec 无损表示 i64、命名 sum/record、tuple、option/list。未知版本、缺少/多余/错误参数分别使用稳定错误码；旧 `{query}` 与纯文本入口保留兼容。
- 查询语言支持 `$name` AST 参数，insert/upsert 可用完整 row 参数，`insert many` 与 `upsert many` 可用 `list RowType` 参数。Rust `prepare/query` 在准备时检查表、字段、stage 与参数上下文类型，记录 schema revision/hash；prepared operation 覆盖 read/explain 和核心 insert/upsert/update/delete，在不扫描 row 的情况下绑定 mutation target、set/match、returning 与参数类型，并用 `execute_prepared_until` 传播 deadline。schema 改变后拒绝旧 plan，避免使用失效的字段或 constructor 位置；prepared 写入支持 memory/redb，过渡 WAL 拒绝无法按原源码重放的绑定值。
- 五分钟教程用同一组 ADT 源码覆盖持久建库、穷尽匹配、嵌套字段、原子更新、关闭重开和完整检查；集成测试比较 Rust Engine、本地 redb CLI 与 TCP 返回的 typed rows、列和 schema identity。发布脚本构建带精确 target/codec 清单的原生压缩包和 SHA-256，包内验证器从空目录重跑教程；macOS/Linux CI 与 tag release workflow 均执行该自检。

## 现有持久化适配的修复

为让共享引擎安全接管已有 server 路径，本轮也修复了原型已复现的问题：

- 写入在候选状态中完成，WAL 同步成功后才发布；WAL 出错保留旧内存状态，禁用后续写入和 checkpoint，要求重新打开以解析不确定的提交结果。
- WAL 每行记录完整请求的 JSON 转义源码、格式版本和提交序号；一个多行批次只对应一条提交记录。回放的单条记录限制为 `6 × MAX_SOURCE_BYTES + 256` 字节，在 UTF-8/JSON 解析前限制读取；超长记录（包括空白和无换行的记录）拒绝打开并保留原文件。该上限容纳 1 MiB 合法源码的最坏 JSON 转义膨胀。读取原型日志仍支持当前兼容的单行语法，不承诺接受旧原型所有隐式转换或缺省 null。
- snapshot 带提交水位，回放跳过已包含的记录；临时文件写入、flush/sync、按当前 reader 验证可重新读取、原子 rename、目录同步之后才允许清理 WAL。极深的类型包装可能触发 JSON snapshot 读取深度限制，此时返回维护 warning，保留旧 snapshot 与完整 WAL。
- 已成功提交之后的 checkpoint 失败返回 warning，不把已提交的写入伪装成失败；受损/截断/未知版本的 WAL 明确拒绝打开，保留原文件。
- 文件占用锁阻止同一路径（含可解析符号链接）的第二写者；锁文件保留，但 OS 锁随 Engine／进程退出释放。数据库文件不应通过硬链接别名访问。

这是现有原型的过渡实现。[ADR 0001](adr/0001-redb-storage.md) 已比较当前实现、redb 和 SQLite，并按 ADT 模型匹配选择 redb 作为长期事务后端；新的 `--db` 入口已经接入主 Engine。当前 WAL 仍依赖源码回放，snapshot 也不是正式长期格式，并且不会与 redb 双写。跨版本升级、完整故障矩阵、备份还原和旧格式导入仍属于 #14/#20。不要将正常重启及指定进程退出模拟的通过视为完整掉电可靠性证明。

## 持久化后端验证

`tools/storage-eval` 使用同一逻辑批次验证 redb 4.1.0 与 rusqlite 0.40.2：一次事务同时写入 schema、catalog、row、index 和 migration ledger；分别在提交前及提交返回后立即退出子进程，并检查重新打开、完整批次、一致备份和第二实例打开行为。macOS 本机三次串行的详细结果、官方保证与实验限制记录在 ADR；CI 在 macOS/Linux 运行小规模同类验证。

选型优先保证运行时 ADT、类型化索引和查询执行只有 unionid 一套语义，不以单机耗时定胜负。正式 redb 写事务显式使用 `Durability::Immediate` 与 two-phase commit；redb 自身的独占打开行为与单所有者产品模型一致。

## 验证方式

```bash
cargo fmt --check
cargo check --locked
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo run -- run --file examples/tasks.uid
cargo run -- fmt --file examples/tasks.uid
cargo run --example embedded
```

测试矩阵验证 Engine 注入提交失败、REPL 状态、安全历史与补全、formatter round-trip、真实磁盘增长失败分类、稳定 RowId、语言和类型检查、prepared DML、原生 Rust serde ADT、索引计划、原子恢复、schema migration、备份还原、真实 CLI/TCP 与优雅关闭，以及 transport-neutral typed wire 协议。TCP 测试使用动态端口；持久化测试只使用隔离临时数据库；HTTP todolist 另行覆盖 typed ADT、重启、migration、完整性检查与备份还原。具体数量随功能增长，以当前测试运行和 CI 为准，不在文档中维护易过时的固定计数。

本机验证环境为 Rust 1.94.0、macOS。仓库包含 macOS/Linux CI 配置；远端验证状态以对应提交和 PR 的 workflow 结果为准。

## 后续工作

实时执行顺序、依赖和验收范围统一维护在 [ROADMAP.md](ROADMAP.md) 与 GitHub issues。本文只记录已经实现并验证过的事实，避免复制 issue 状态后再次过时。
