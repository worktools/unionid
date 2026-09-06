# 第一轮开发记录

日期：2026-09-06。目标是交付无分号 ADT 语言的可运行预览；完整 v0.1 路线图仍在实施。当前语法见 [LANGUAGE.md](LANGUAGE.md)，查询的详细语义见 [QUERY.md](QUERY.md)，运行入口见 [README](../README.md)。

## 已实现

- 共享 Rust `Engine`，本地 CLI 与 TCP 复用同一个执行、类型校验和原子提交边界。
- Lexer、源码位置、缩进与换行 AST；命名 sum/record、tuple、option/list、完整值构造和严格校验。
- 类型、字段和变体的单调递增 catalog ID；命名类型相等检查身份，schema 展示可重新解析。
- `field type = value` 字段默认值在声明时完成递归类型检查，insert 对嵌套 record 和 sum record 负载逐层补齐；schema 展示、WAL/snapshot 恢复与 hash 均保留默认值。
- 版本 1 ADT value codec 以 catalog 和期望类型驱动，用稳定 type/field/variant ID 编码命名类型、record、sum、tuple、option/list；不依赖 serde 或 Rust enum 布局，字段重排和显式 rename 保持字节可解释。
- redb 4.1 已接入共享 Engine：`run --db`、`cli --db` 与 `server --db` 使用同一个持久文件；每个写脚本按稳定 ID/RowId 计算已提交状态与候选状态的差异，只删除或写入变化的 catalog、ADT row 和 secondary-index 键，再与 meta 一起放入一个 `Immediate`、two-phase write transaction。版本化 migration 在同一事务追加 ledger entry，普通数据提交不触碰 ledger。覆盖前核对旧值，打开时校验存储／catalog／value／索引／migration 编码版本、schema hash、ledger head、RowId 水位和派生索引一致性。
- 每条 row 带表内稳定 `u64` RowId，每表持久化单调分配游标；索引 posting 不再保存 `Vec` 下标。删除形成的 ID 缺口可安全恢复，后续插入不会复用旧身份；旧 redb 和 snapshot 会从原有连续顺序升级。
- `update table`/`delete table` 复用普通与 match filter；多个 typed `set` 从原 row 同时求值，可设置嵌套 record 路径。执行器先生成全部候选 row，再检查完整类型和主键唯一性并重建该表索引；失败请求不发布任何修改。DML 响应包含 `affected_rows`。
- `upsert table value` 要求声明主键，输入按完整 row 与默认值规则检查；未命中时分配 RowId，命中时整行替换并保留 RowId。响应以 `upsert_action` 区分 inserted/updated，索引和 redb 状态服从同一请求级提交边界。
- redb 提交错误按边界区分：transaction commit 前的确定失败回滚候选状态并保留句柄，commit 调用返回错误时标记结果不确定、关闭句柄并阻止后续写入。可注入 backend 单元测试验证两条 Engine 状态路径；真实子进程测试分别在未提交多表 transaction 与成功 Engine commit 后直接退出并重开。
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
- 修复大整数、浮点零值与 enum 负载的索引/扫描一致性；深层索引键按结构编码，避免重复转义造成指数增长；未知字段在空表上也报错。
- 用隔离的临时目录、动态 TCP 端口和子进程建立测试；增加 macOS/Linux 的 CI 配置。
- 服务使用 64 个活动连接的有界等待集合；达到上限时返回结构化 `E_BUSY`，拒绝过程不创建 worker。请求/源码/value/query working rows/result rows/response bytes 与 pattern analysis 都有明确上限，服务执行 deadline 返回 `E_TIMEOUT` 且不发布候选事务。response 通过限长 writer 直接编码，避免先创建无界 JSON byte buffer。
- SIGINT/SIGTERM 停止 accept，关闭空闲连接，等待已进入 Engine 的请求完成或原子放弃，再释放 redb 锁；关闭时输出 accepted/rejected/requests/failed 统计。真实子进程测试验证 idle client 存在时仍能退出并立即 reopen。
- JSON Lines version 1 请求包含 request ID、完整多行源码、typed params 与可选 schema 前置条件；响应回显 ID，并通过独立 wire codec 无损表示 i64、命名 sum/record、tuple、option/list。未知版本、缺少/多余/错误参数分别使用稳定错误码；旧 `{query}` 与纯文本入口保留兼容。
- 查询语言支持 `$name` AST 参数，insert/upsert 可用完整 row 参数。Rust `prepare/query` 在准备时检查表、字段、stage 与参数上下文类型，记录 schema revision/hash；schema 改变后拒绝旧 plan，避免使用失效的字段或 constructor 位置。

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

当前全部 179 项测试通过：lib 单元测试 10 项、`tests/language.rs` 75 项、`tests/formatter.rs` 4 项、`tests/storage.rs` 28 项、`tests/migration.rs` 16 项、`tests/schema.rs` 8 项、`tests/backup.rs` 3 项、`tests/interfaces.rs` 18 项、`tests/codec.rs` 8 项、`tests/protocol.rs` 9 项，分别验证 Engine 注入提交失败、REPL complete/incomplete/invalid 状态与 EOF、formatter 全 AST 覆盖／幂等／语义 round-trip、稳定 RowId 与过渡 snapshot 升级、增量持久差异、语言/类型/查询及原子 update/delete/upsert、基础汇总、局部纯函数及其预算、类型化索引计划/explain、WAL 与 redb 的失败原子性/恢复、多表 schema+DML、ADT schema/data conversion、版本化 migration 文件/ledger/断点续跑、schema 规范化/diff/影响报告、备份还原/旧格式导入、真实 CLI/TCP/并发与优雅关闭、typed 参数/prepared query/deadline，以及 ADT 的稳定存储与网络编码。TCP 测试实际启动服务，自动分配端口，并在结束时停止进程；所有持久化测试只使用隔离临时数据库。

本机验证环境为 Rust 1.94.0、macOS。仓库包含 macOS/Linux CI 配置；远端验证状态以对应提交和 PR 的 workflow 结果为准。

## 后续工作

- #2：以当前查询参考完善完整语言 RFC；更广泛的泛型/高阶函数与 migration 表面语法尚未冻结。#7 已形成 [Schema 身份与演进契约](SCHEMA.md)，实现稳定 table/index ID、原子 revision/hash 和响应元数据。
- #8/#10/#11/#34–#36：已实现默认值、版本化 value codec、完整嵌套 ADT 覆盖分析、普通 scalar/bool derive、typed arithmetic/value construction、多键 sort、范围 take、布尔组合、`contains/length/any/all`、Option helper、typed 参数、schema-aware prepared query、有界的 group/aggregate 与查询局部纯函数。#9/#11/#12/#34–#36 和 #59–#61 已完成。
- #13/#14/#15/#16：redb 固定内部表、版本化 codec、稳定键增量提交、单写事务、稳定 RowId、原子 update/delete/upsert、明确／不确定提交错误、进程退出恢复矩阵、`check --db`、共享类型化索引访问计划与 explain 已接入；#13/#14 继续跟踪真实空间不足／同步故障与恢复时间边界。
- #17–#20：显式 schema/data migration、版本化 runner/ledger、声明式 schema diff、逻辑备份还原和显式旧原型导入已实现。
- #21–#24：#22 的版本化协议与 Rust 参数 API、#23 的服务预算与优雅关闭已实现；#66 补齐 parser 驱动的 REPL 续写，#69 提供规范 formatter，#70 继续历史、补全和远程 introspection，再用 #24 的真实负载和故障矩阵形成发布证据。

GitHub issues 保留各自的完整验收范围；实现了部分能力不等于对应里程碑全部完成。
