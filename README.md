# unionid

一个基于 Rust 的最小数据库原型：

- 存储结构使用 `enum`（sum type）+ `struct`（product type）
- PRQL 风格 pipeline 查询
- 独立 TCP 服务
- 命令行客户端

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

## Query 语言参考

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
- 当前支持 stage：`filter` / `select` / `limit`。

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
- 空值：`null`

### 4) 运算符

`filter` 支持：

- `=` / `==`
- `!=`
- `>` / `>=`
- `<` / `<=`

### 5) 索引加速规则（当前实现）

- 索引是单列倒排映射（内存结构）。
- 仅当 `filter` 是等值匹配（`=` 或 `==`）且命中已建索引列时走索引加速。
- 其他过滤条件仍走全表扫描。

### 6) Enum 约束规则（当前实现）

- Enum 类型写法：`enum(VariantA, VariantB(type1,type2), ...)`。
- 变体名必须是标识符（字母/数字/下划线，首字符不能是数字）。
- 插入或过滤时，值写法为 `Variant` 或 `Variant(arg1,arg2)`。
- 参数个数和参数类型必须与建表定义一致，否则写入会报错。
- `filter kind = SomeVariant(...)` 支持等值比较；`>`/`<` 不适用于 enum 值。

## 持久化说明

- 默认不持久化（纯内存）。
- 当提供 `--wal-path` 后，服务会把 `create table` / `insert` 追加到 WAL。
- 服务重启时会自动回放 WAL 并恢复数据。
- 当同时提供 `--snapshot-path` 与 `--snapshot-every N`（`N>0`）时，服务每 `N` 次成功写操作保存一次快照。
- 保存快照后会清空 WAL，后续只保留新的增量语句，减少下次启动回放成本。
- `create index` 也属于写操作，会被写入 WAL，并在恢复时回放。
