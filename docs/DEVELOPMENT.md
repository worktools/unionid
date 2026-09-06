# 第一轮开发记录

日期：2026-09-06。目标是交付无分号 ADT 语言的可运行预览；完整 v0.1 路线图仍在实施。当前语法见 [LANGUAGE.md](LANGUAGE.md)，查询的详细语义见 [QUERY.md](QUERY.md)，运行入口见 [README](../README.md)。

## 已实现

- 共享 Rust `Engine`，本地 CLI 与 TCP 复用同一个执行、类型校验和原子提交边界。
- Lexer、源码位置、缩进与换行 AST；命名 sum/record、tuple、option/list、完整值构造和严格校验。
- 类型、字段和变体的单调递增 catalog ID；命名类型相等检查身份，schema 展示可重新解析。
- `field type = value` 字段默认值在声明时完成递归类型检查，insert 对嵌套 record 和 sum record 负载逐层补齐；schema 展示、WAL/snapshot 恢复与 hash 均保留默认值。
- 版本 1 ADT value codec 以 catalog 和期望类型驱动，用稳定 type/field/variant ID 编码命名类型、record、sum、tuple、option/list；不依赖 serde 或 Rust enum 布局，字段重排和显式 rename 保持字节可解释。
- 换行及单行 pipeline、字段路径、filter/select/sort/take、sum 类型的模式过滤、主键唯一性和等值索引。
- 查询参考明确记录当前 grammar、stage schema、执行顺序、match 规则、错误类别和已实现/计划边界；任务、配置、事件三个示例都由语言测试执行。
- 原子脚本、可读 CLI 输出、JSON 输出、文件/stdin、多行 REPL、正确退出码及 EOF 处理。
- 修复大整数、浮点零值与 enum 负载的索引/扫描一致性；深层索引键按结构编码，避免重复转义造成指数增长；未知字段在空表上也报错。
- 用隔离的临时目录、动态 TCP 端口和子进程建立测试；增加 macOS/Linux 的 CI 配置。
- 连接数达到上限时返回结构化 `E_BUSY`；先半关闭写端，再有界排空输入，避免未读请求导致 TCP reset 吞掉错误响应。拒绝过程不创建额外 worker，写入超时和排空期限各为 100 ms。

## 现有持久化适配的修复

为让共享引擎安全接管已有 server 路径，本轮也修复了原型已复现的问题：

- 写入在候选状态中完成，WAL 同步成功后才发布；WAL 出错保留旧内存状态，禁用后续写入和 checkpoint，要求重新打开以解析不确定的提交结果。
- WAL 每行记录完整请求的 JSON 转义源码、格式版本和提交序号；一个多行批次只对应一条提交记录。回放的单条记录限制为 `6 × MAX_SOURCE_BYTES + 256` 字节，在 UTF-8/JSON 解析前限制读取；超长记录（包括空白和无换行的记录）拒绝打开并保留原文件。该上限容纳 1 MiB 合法源码的最坏 JSON 转义膨胀。读取原型日志仍支持当前兼容的单行语法，不承诺接受旧原型所有隐式转换或缺省 null。
- snapshot 带提交水位，回放跳过已包含的记录；临时文件写入、flush/sync、按当前 reader 验证可重新读取、原子 rename、目录同步之后才允许清理 WAL。极深的类型包装可能触发 JSON snapshot 读取深度限制，此时返回维护 warning，保留旧 snapshot 与完整 WAL。
- 已成功提交之后的 checkpoint 失败返回 warning，不把已提交的写入伪装成失败；受损/截断/未知版本的 WAL 明确拒绝打开，保留原文件。
- 文件占用锁阻止同一路径（含可解析符号链接）的第二写者；锁文件保留，但 OS 锁随 Engine／进程退出释放。数据库文件不应通过硬链接别名访问。

这是现有原型的过渡实现。[ADR 0001](adr/0001-redb-storage.md) 已比较当前实现、redb 和 SQLite，并按 ADT 模型匹配选择 redb 作为长期事务后端；主 Engine 还没有接入。当前 WAL 仍依赖源码回放，snapshot 也不是正式长期格式。redb 接入、跨版本升级、故障矩阵与备份还原仍属于 #13/#14/#20。不要将正常重启及指定进程退出模拟的通过视为完整掉电可靠性证明。

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
cargo run --example embedded
```

当前全部 68 项测试通过：`tests/language.rs` 33 项、`tests/storage.rs` 15 项、`tests/migration.rs` 4 项、`tests/interfaces.rs` 8 项、`tests/codec.rs` 8 项，分别验证语言/类型/查询、失败原子性/恢复、migration 历史约束、真实 CLI/TCP/并发请求，以及 ADT 值的稳定编码与损坏拒绝。TCP 测试实际启动服务，自动分配端口，并在结束时停止进程；所有持久化测试只使用临时数据库。

本机验证环境为 Rust 1.94.0、macOS。仓库包含 macOS/Linux CI 配置；远端验证状态以对应提交和 PR 的 workflow 结果为准。

## 后续工作

- #2：以当前查询参考完善完整语言 RFC；类型推断、更新与 migration 表面语法尚未冻结。#7 已形成 [Schema 身份与演进契约](SCHEMA.md)，实现稳定 table/index ID、原子 revision/hash 和响应元数据。
- #8/#10/#11：在已实现的默认值、版本化 value codec 与 `filter match` 子集上继续完善 typed IR、通用 match 表达式、tuple/嵌套模式、参数、let/derive/group/aggregate。#9/#12 的验收范围已经实现。
- #13/#14/#15/#16：先让 redb 固定内部表和单写事务接管 Engine，再补齐故障模型、CRUD/upsert、索引计划及 explain。
- #17–#20：实现 schema/data migration、ledger、diff、备份还原与显式旧数据转换。
- #21–#24：继续打磨 REPL 历史/补全/格式化、协议、服务预算和正式发布。

GitHub issues 保留各自的完整验收范围；实现了部分能力不等于对应里程碑全部完成。
