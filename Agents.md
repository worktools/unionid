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
- 并发事务
- 索引

## 当前进展（2026-03-05）

- 已完成内存引擎 + TCP + CLI 的 MVP。
- 已增加可选 WAL 持久化：`server --wal-path <path>`。
- 服务启动时会自动回放 WAL，恢复 `create table` 与 `insert` 语句。
- 已增加可选 snapshot 压缩：`server --snapshot-path <path> --snapshot-every <n>`。
- 启动恢复流程为：先加载 snapshot，再回放 WAL 增量。
- 已支持单列索引：`create index <table> (<col>)`，用于等值过滤加速。
- 已支持 Rust 风格参数化 enum 列类型：`enum(A, B(int), C(text,float))`。

## 代码约定

- 优先小而清晰的模块边界：`model`, `db`, `query`, `server`, `cli`。
- 错误处理统一为可读字符串，保证 TCP/CLI 易观察。
- 默认 UTF-8 文本协议。

## 测试与验证

- 至少保证 `cargo check` 通过。
- 手工验证：
  1. 启动 `server`
  2. 使用 `cli` 建表、插入、查询
