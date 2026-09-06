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

## 当前开发进展（2026-09-06）

- 已开始新的无分号语言预览；当前可执行子集见 `docs/LANGUAGE.md`，查询细则见 `docs/QUERY.md`，完整目标见 `docs/DESIGN.md`。
- 类型声明和查询采用 PRQL 风格，优先空格、换行与缩进，不引入分号或 TypeScript 风格的密集注解。
- 复杂 filter 和 match condition 可用缩进块或跨行括号组织；混用 `and` 与 `or` 时规范写法使用括号显式表达分组。括号属于帮助理解的必要符号，应谨慎使用而非一味移除。
- `Engine` 是本地 Rust API、CLI 和 TCP 的共享入口；每次请求是一个原子脚本。
- 已实现命名 sum/record、tuple、option/list、严格插入、主键、filter/select、单键/多键 sort、前 N 行/范围 take，以及支持递归 pattern 与 option/sum/product/list 新值构造的 `filter match` 和 ADT derive。同一顶层 constructor 可由多个互补嵌套分支覆盖；有预算的 pattern matrix 在扫描前检查穷尽性、不可达分支和积类型相关性。普通 filter 与 match condition 共用布尔表达式，支持括号、`not/and/or`、字段／binding 间比较和 `contains/length`；算术、通用函数、更新操作及 migration 尚未实现。
- `docs/SCENARIOS.md` 用任务队列、配置、事件、同步和 key/value 工作流维护查询覆盖；#35/#36 跟踪 ADT 派生及布尔/集合表达式。
- 字段可用 `field type = value` 声明默认值；默认值在 schema 阶段类型检查，insert 会对嵌套 record 和 sum record 负载逐层补齐。
- 已实现独立于 serde/Rust enum 布局的版本 1 ADT value codec；它用稳定 type/field/variant ID 编码，下一步接入 redb `rows` 表。
- 已通过 `docs/adr/0001-redb-storage.md` 选定 redb 作为长期事务后端；主 Engine 尚未接入。现有 WAL/snapshot 仍是过渡实现。
- `docs/SCHEMA.md` 已定义类型演进契约；类型、字段、变体、表和索引使用统一稳定 ID，每次原子 schema 变更产生一个 revision 与 SHA-256 hash，Engine 响应携带版本信息。
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
