# unionid

一个基于 Rust、原生支持代数类型的轻量数据库语言预览：

- 无分号的命名和类型、嵌套 record/tuple、`option` 与 `list`
- PRQL 风格换行查询与修改：`filter/filter match/derive/derive match/select/sort/take/set`
- 可组合的有类型表达式：int/float 算术、`not/and/or`、字段间比较、Option helper、`contains/length/any/all`
- 共享 Rust 引擎、本地 CLI 与 TCP 服务
- version 1 JSON Lines 协议、无损 ADT/i64 wire values、typed 参数与 schema-aware prepared query
- 有界连接/请求/查询/结果、执行 deadline、运行统计与 SIGINT/SIGTERM 优雅关闭
- 严格类型检查、字段默认值、主键、upsert/update/delete、等值索引及原子脚本
- 稳定 catalog 身份、原子 schema revision 与可校验 hash
- 独立于 serde/Rust enum 布局的版本化 ADT value codec
- redb 增量原子持久模式，可由本地命令、REPL 与 TCP 服务共同使用
- 显式 ADT schema migration：稳定身份 rename、默认回填、typed conversion、约束与索引变更
- 版本化 migration runner：`new/plan/apply/status`、不可变 checksum 与 redb ledger
- 可校验逻辑备份、只还原到新路径，以及显式原型 WAL/snapshot 导入

## 先运行一个完整例子

需要 Rust 1.94 或更高版本（Cargo 已声明 `rust-version`，CI 使用 1.94.0）：

```bash
cargo run -- run --file examples/tasks.uid
cargo run -- run --file examples/job_queue.uid
cargo run -- run --file examples/config.uid --format json
cargo run -- run --file examples/events.uid
cargo run -- run --file examples/sync_conflicts.uid
cargo run -- run --file examples/task_mutations.uid
cargo run -- run --file examples/schema_migration.uid

# apply the versioned example history to a durable database
cargo run -- migration plan --db app.redb --dir examples/migrations
cargo run -- migration apply --db app.redb --dir examples/migrations
cargo run -- migration status --db app.redb --dir examples/migrations
cargo run -- schema check --file examples/schema.uid
cargo run -- migration diff --db app.redb --schema examples/schema.uid --name sync_schema
cargo run -- backup --db app.redb --output app.backup.json
cargo run -- restore --backup app.backup.json --db restored.redb
cargo run --example embedded
```

[tasks.uid](examples/tasks.uid) 包含类型定义、建表、插入与查询，最后返回：

```text
id | title | owner.email | state
1 | "同步目录" | "alice@example.com" | Running {attempt = 2, worker = "local"}
1 row(s)
```

`run` 默认每次创建一个新的内存库；一次文件或请求是一个原子批次，成功返回最后一条语句的结果，失败不保留该批次的任何写入。需要保存数据时指定同一个 redb 文件：

```bash
cargo run -- run --db ./data/unionid.redb --file examples/tasks.uid
cargo run -- run --db ./data/unionid.redb --query 'from tasks | filter id == 1'
cargo run -- cli --db ./data/unionid.redb
cargo run -- check --db ./data/unionid.redb
```

开启保留会话状态的本地 REPL：

```bash
cargo run -- cli --memory
```

输入多行后用空行提交，`.schema` 查看类型与表，`.tables` 列出表，`.quit` 退出。文件或重定向 stdin 则读取到 EOF 后整体执行。查询失败返回非零退出码。

当前可执行语法见 [LANGUAGE.md](docs/LANGUAGE.md)，查询 stage、执行顺序、模式规则和能力状态见 [QUERY.md](docs/QUERY.md)，[version 1 协议与参数](docs/PROTOCOL.md)描述无损 ADT/i64 wire codec 和 Rust prepared query，schema 演进语法与版本化 runner 见 [MIGRATIONS.md](docs/MIGRATIONS.md)，声明式目标结构与草稿生成见 [SCHEMA-DIFF.md](docs/SCHEMA-DIFF.md)。字段默认值使用 `field type = value`；sum/option 的 match 支持 unit、record、位置负载和递归 pattern，同一个顶层 constructor 可以由多个互补嵌套分支完整覆盖。`derive x = match ...` 可从 binding 和有类型算术构造新的 option、sum、record、tuple 和 list；`derive x = expression` 可直接追加 scalar 或 bool 结果并供后续 stage 使用。普通 filter 与 match condition 支持括号、int/float 算术、`not/and/or`、字段间比较、`contains/length`、Option helper，以及有类型的嵌套 `any/all` 元素谓词。`update`/`delete` 复用 filter，多个 typed `set` 同时求值；`upsert` 按主键插入或整行替换。三者都原子维护主键与索引。显式 migration 可跨所有嵌套引用路径改名、回填和转换 ADT，并同步维护约束与索引；`new/plan/diff/apply/status` 管理不可变迁移历史。通用函数和 `let/group` 还在后续计划中。

[服务边界](docs/SERVICE.md)单独记录连接、请求、查询、响应和 deadline 限制，以及 SIGINT/SIGTERM 关闭与重试语义。

复杂条件推荐在 `filter` 或 match 分支的 `=>` 后换行并缩进；混用 `and` 与 `or` 时用括号写清分组。语言会减少无助于理解的标点，同时保留括号和集合边界等必要符号。

## 后续方向与计划

计划将原型演进为原生支持命名和类型、积类型、模式匹配及 schema migration 的轻量数据库。

新语言的类型声明与查询都朝 PRQL 风格收敛：无分号、少标点，优先用空格、换行和清晰的布局表达结构。

- [设计草案](docs/DESIGN.md)：定位、目标语法、类型语义、存储取舍与 migration 流程。
- [查询语言参考](docs/QUERY.md)：当前可执行的 pipeline grammar、stage 语义、模式和错误。
- [实际场景与覆盖矩阵](docs/SCENARIOS.md)：任务队列、配置、事件、同步和 key/value 用法所需的 ADT 与查询缺口。
- [Schema 身份与演进契约](docs/SCHEMA.md)：类型／字段／变体／表／索引身份、版本与兼容矩阵。
- [Schema migration 语言](docs/MIGRATIONS.md)：当前可执行的显式演进、typed conversion 与约束边界。
- [声明式 Schema 与 Diff](docs/SCHEMA-DIFF.md)：规范化 schema、影响报告和不可猜测的迁移草稿。
- [备份、还原与旧格式导入](docs/BACKUP.md)：逻辑备份校验、新路径恢复和显式原型转换。
- [ADT value codec](docs/CODEC.md)：稳定 ID 驱动的持久值格式、限制与 schema evolution 边界。
- [redb 持久模式](docs/STORAGE.md)：本地／服务入口、事务承诺、内部表与当前限制。
- [路线图与 GitHub issues](docs/ROADMAP.md)：阶段、依赖、验收条件及执行入口。
- [存储 ADR](docs/adr/0001-redb-storage.md)：redb 选型、ADT 存储边界、实验和限制。
- [原型基线与已知问题](docs/PROTOTYPE-AUDIT.md)：早期原型的验证结果和故障证据。
- [第一轮开发记录](docs/DEVELOPMENT.md)：已实现能力、验证方法与尚未完成的范围。

设计草案描述完整目标，部分语法已实现；整体能力边界以 LANGUAGE.md 为准，查询行为以 QUERY.md 为准。长期持久化入口使用 redb；当前 WAL／snapshot 仅为旧原型兼容入口，不与 redb 双写，也不能直接当作 redb 数据库打开。显式导入由 #20 跟踪。

## 运行

启动服务：

```bash
cargo run -- server --addr 127.0.0.1:7878
```

使用 redb 持久化启动服务：

```bash
cargo run -- server --addr 127.0.0.1:7878 --db ./data/unionid.redb
```

过渡 WAL 兼容入口：

```bash
cargo run -- server --addr 127.0.0.1:7878 --wal-path ./data/unionid.wal
```

开启 WAL + snapshot 压缩：

```bash
cargo run -- server \
	--addr 127.0.0.1:7878 \
	--wal-path ./target/tmp/unionid.wal \
	--snapshot-path ./target/tmp/unionid.snapshot.json \
	--snapshot-every 200
```

执行单条命令：

```bash
cargo run -- cli --addr 127.0.0.1:7878 --query 'create table users (id int, name text, age int, active bool)'
cargo run -- cli --addr 127.0.0.1:7878 --query 'create index users (id)'
cargo run -- cli --addr 127.0.0.1:7878 --query 'insert users {id:1,name:"alice",age:30,active:true}'
cargo run -- cli --addr 127.0.0.1:7878 --query 'from users | filter age >= 25 | select id,name | limit 10'
```

进入交互模式：

```bash
cargo run -- cli --addr 127.0.0.1:7878
```

## 兼容的原型语法

以下入口仍可运行；新项目优先使用 [当前语言文档](docs/LANGUAGE.md) 的无分号命名类型和换行查询。

### 1) DDL / DML

建表：

```text
create table <table> (<col> <type>, ...)
```

示例：

```text
create table users (id int, name text, age int, active bool)
```

带参数的 Rust 风格 enum 列：

```text
create table events (
  id int,
  kind enum(Login, Logout, Purchase(int,float), Error(text))
)
```

建索引（单列）：

```text
create index <table> (<col>)
```

示例：

```text
create index users (id)
create index users (name)
```

插入：

```text
insert <table> {key:value,...}
```

示例：

```text
insert users {id:1,name:"alice",age:30,active:true}
```

插入 enum 值：

```text
insert events {id:1,kind:Login}
insert events {id:2,kind:Purchase(42,19.9)}
insert events {id:3,kind:Error("network")}
```

### 2) Pipeline 查询

基础形态：

```text
from <table> | filter <col> <op> <value> | select <col,...> | limit <n>
```

说明：

- `from` 必须是第一个 stage。
- stage 从左到右执行。
- 当前支持 stage：`filter` / `select` / `sort` / `take`，保留 `limit` 别名。

示例：

```text
from users | filter age >= 20 | select id,name | limit 5
from users | filter id = 1 | select id,name,age
from events | filter kind = Purchase(42,19.9) | select id,kind
```

### 3) 类型与字面量

列类型：

- `int`
- `float`
- `bool`
- `text`

字面量：

- 整数：`1`
- 浮点：`3.14`
- 布尔：`true` / `false`
- 文本：`"alice"`
- 可选值：使用 `option` 类型与显式 `None` / `Some value`；普通类型不接受 `null`。

### 4) 运算符

`filter` 支持：

- `=` / `==`
- `!=`
- `>` / `>=`
- `<` / `<=`

### 5) 索引加速规则（当前实现）

- 索引是单列倒排映射（内存结构）。
- 仅当查询开头的 `filter` 是等值匹配（`=` 或 `==`）且命中已建索引列时走索引加速。
- 其他过滤条件仍走全表扫描。

### 6) Enum 约束规则（当前实现）

- Enum 类型写法：`enum(VariantA, VariantB(type1,type2), ...)`。
- 变体名必须以大写字母开头，其余字符为 ASCII 字母、数字或下划线。
- 插入或过滤时，值写法为 `Variant` 或 `Variant(arg1,arg2)`。
- 参数个数和参数类型必须与建表定义一致，否则写入会报错。
- `filter kind = SomeVariant(...)` 支持等值比较；`>`/`<` 不适用于 enum 值。

## 持久化说明

- 默认不持久化（纯内存）。
- [ADR 0001](docs/adr/0001-redb-storage.md) 已选定 redb 作为长期事务后端；当前命令尚未接入 redb。
- 提供 `--wal-path` 后，每个成功的写批次以一条带版本与提交序号的 JSON 记录追加到 WAL；源码中的换行转义保存，同步后才发布内存状态。
- 服务重启先加载 snapshot，再回放尚未包含的 WAL 提交；索引从数据重建。
- `--snapshot-path` 要求同时配置 WAL；`--snapshot-every N` 表示每 `N` 个成功写批次保存一次快照。
- 快照经临时文件、同步和原子替换发布后才清理 WAL；提交水位处理快照与旧 WAL 的重叠。
- WAL 提交错误后禁用继续写入和 checkpoint，重新打开数据库以确认提交结果；已提交之后的快照维护失败以 warning 返回。
- 当前有文件占用锁；数据库文件不应使用硬链接别名。源码回放、校验和、正式格式升级和完整掉电故障矩阵仍待完善，参见 [开发记录](docs/DEVELOPMENT.md)。

## 开发验证

```bash
cargo fmt --check
cargo check --locked
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

测试覆盖语言、索引一致性、原子性、恢复及真实 CLI/TCP。CI 配置包含 macOS/Linux；本轮本机实际验证为 macOS。
