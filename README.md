# unionid

一个基于 Rust、原生支持代数类型的轻量数据库语言预览：

- 无分号的命名和类型、嵌套 record/tuple、`option` 与 `list`
- PRQL 风格换行查询：`filter/filter match/select/sort/take`
- 共享 Rust 引擎、本地 CLI 与 TCP 服务
- 严格类型检查、字段默认值、主键、等值索引及原子脚本
- 稳定 catalog 身份、原子 schema revision 与可校验 hash
- 独立于 serde/Rust enum 布局的版本化 ADT value codec

## 先运行一个完整例子

需要 Rust 1.94 或更高版本（Cargo 已声明 `rust-version`，CI 使用 1.94.0）：

```bash
cargo run -- run --file examples/tasks.uid
cargo run -- run --file examples/config.uid --format json
cargo run -- run --file examples/events.uid
cargo run --example embedded
```

[tasks.uid](examples/tasks.uid) 包含类型定义、建表、插入与查询，最后返回：

```text
id | title | owner.email | state
1 | "同步目录" | "alice@example.com" | Running {attempt = 2, worker = "local"}
1 row(s)
```

`run` 每次创建一个新的内存库；一次文件或请求是一个原子批次，成功返回最后一条语句的结果，失败不保留该批次的任何写入。

开启保留会话状态的本地 REPL：

```bash
cargo run -- cli --memory
```

输入多行后用空行提交，`.schema` 查看类型与表，`.tables` 列出表，`.quit` 退出。文件或重定向 stdin 则读取到 EOF 后整体执行。查询失败返回非零退出码。

当前可执行语法见 [LANGUAGE.md](docs/LANGUAGE.md)，查询 stage、执行顺序、模式规则和能力状态见 [QUERY.md](docs/QUERY.md)。字段默认值使用 `field type = value`；sum 类型的 `filter match` 已支持 record 负载绑定和穷尽检查。通用 match 表达式、`let/derive/group`、参数绑定、update/delete/upsert 和 migration 还在后续计划中。

## 后续方向与计划

计划将原型演进为原生支持命名和类型、积类型、模式匹配及 schema migration 的轻量数据库。

新语言的类型声明与查询都朝 PRQL 风格收敛：无分号、少标点，优先用空格、换行和清晰的布局表达结构。

- [设计草案](docs/DESIGN.md)：定位、目标语法、类型语义、存储取舍与 migration 流程。
- [查询语言参考](docs/QUERY.md)：当前可执行的 pipeline grammar、stage 语义、模式和错误。
- [Schema 身份与演进契约](docs/SCHEMA.md)：类型／字段／变体／表／索引身份、版本与兼容矩阵。
- [ADT value codec](docs/CODEC.md)：稳定 ID 驱动的持久值格式、限制与 schema evolution 边界。
- [路线图与 GitHub issues](docs/ROADMAP.md)：阶段、依赖、验收条件及执行入口。
- [存储 ADR](docs/adr/0001-redb-storage.md)：redb 选型、ADT 存储边界、实验和限制。
- [原型基线与已知问题](docs/PROTOTYPE-AUDIT.md)：早期原型的验证结果和故障证据。
- [第一轮开发记录](docs/DEVELOPMENT.md)：已实现能力、验证方法与尚未完成的范围。

设计草案描述完整目标，部分语法已实现；整体能力边界以 LANGUAGE.md 为准，查询行为以 QUERY.md 为准。长期持久化后端已选定 redb，ADT value codec 已实现但尚未接入主 Engine；当前 WAL／snapshot 仍是过渡实现，不能作为正式长期格式。

## 运行

启动服务：

```bash
cargo run -- server --addr 127.0.0.1:7878
```

开启 WAL 持久化启动服务：

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
