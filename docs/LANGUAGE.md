# 当前可运行的语言预览

本页是 unionid 当前可执行语言的规范入口。示例和规则都由现有实现支持；查询的完整语义见 [QUERY.md](QUERY.md)，schema 演进见 [MIGRATIONS.md](MIGRATIONS.md)，声明式目标结构见 [SCHEMA-DIFF.md](SCHEMA-DIFF.md)，实际应用覆盖见 [SCENARIOS.md](SCENARIOS.md)，未来设计单独放在 [DESIGN.md](DESIGN.md)，不能据此推断当前语法。完整脚本可运行：[任务](../examples/tasks.uid)、[任务修改](../examples/task_mutations.uid)、[schema migration](../examples/schema_migration.uid)、[后台队列](../examples/job_queue.uid)、[配置](../examples/config.uid)、[事件](../examples/events.uid)、[同步冲突](../examples/sync_conflicts.uid)。

当前包含类型与表声明、insert/upsert/update/delete、版本化 schema migration、布尔 filter、sum/option 的 `filter match`、ADT `derive match`、select、sort 和 take。filter 与 match/derive/set/migration conversion 表达式支持有类型的 int/float 算术；filter 还支持 `not/and/or`、字段间比较及 `contains/length`。`$name` 参数通过 Rust API 或版本化 TCP 协议绑定。

## 类型、表与值

```text
type Contact =
  email text
  nickname option text = None

type State =
  Pending
  | Running {worker text, attempt int}
  | Done {result text}

type Task =
  id int
  owner Contact
  tags list text = []
  state State

table tasks Task
  key id

insert tasks
  id = 1
  owner =
    email = "alice@example.com"
  state = Running {worker = "local", attempt = 2}
```

- 无分号。类型名和变体名以大写字母开头；缩进式 record 以小写字段名开头。当前标识符为 ASCII 字母、数字与下划线，首字符不能是数字；文本值支持 UTF-8。
- 原子类型为 `int`（i64）、`float`（有限 f64）、`bool`、`text`。类型引用必须先声明；当前拒绝递归引用和用户自定义泛型。
- 支持命名 record/sum、嵌套积类型、tuple，以及内建 `option T`、`list T`，例如 `type Point = (float, float)`、`option (list Contact)`。
- record 类型可内联为 `{email text, nickname option text}`；变体负载也可缩进：在 `| Running` 的下一层写 `worker text` 和 `attempt int`。
- record 值使用 `field = value`，内联字段之间用逗号；列表如 `[1, 2]`，tuple 如 `(1, "x")`。位置负载写成 `Pair(1, "x")`；单个 tuple 负载与多个位置参数通过括号区分。
- 字段默认值写成 `field type = value`，例如 `nickname option text = None`、`tags list text = []`。默认值必须是可按字段类型检查的纯字面值，在 schema 声明时完成校验并存为完整 typed value；不能引用其他字段、参数、时钟或函数。
- 没有默认值的字段全部必填，即使类型为 option 也必须显式写 `None`。缺失字段逐层使用它自身声明的默认值；显式值不会因为类型错误而退回默认值。重复、缺失、未知字段及错误负载均报错。
- 命名类型保留身份；有歧义时可用 `State.Pending` 或 `State.Running {...}` 限定构造器。
- `table tasks Task` 要求 Task 是 record；可选的缩进 `key id` 声明 int/text 主键，拒绝重复键。无 key 时允许重复行。

## 查询

完整的 stage、类型检查、执行顺序、模式规则、错误和测试映射见 [查询语言参考](QUERY.md)。下面是规范的多行写法：

```text
from tasks
filter match state
  Running {attempt, ..} =>
    attempt >= 2
    and attempt < 5
  _ => false
derive state_label =
  match state
    Pending => "pending"
    Running {..} => "running"
    Done {..} => "done"
select {id, owner.email, state, state_label}
sort id
take 20
```

当前 transform：

| 操作 | 规范形式 | 语义 |
| --- | --- | --- |
| 数据源 | `from tasks` | 开始查询 |
| 布尔过滤 | `filter priority >= 10 and contains tags "sync"` | 组合 bool、比较、list/text 长度与 list 成员判断 |
| 模式过滤 | `filter match state` | 按 sum 变体及其 record 负载判断 |
| ADT 派生 | `derive label = match state ...` | 穷尽解构 sum/option，追加统一类型的结果列 |
| 投影 | `select {id, owner.email}` | 保留列，响应按声明的列顺序展示 |
| 排序 | `sort id` / `sort {-priority, created_at, id}` | 单列或多列词典序；支持 int、float、text |
| 截取 | `take 20` / `take 11..20` | 保留前 N 行或一基闭区间内的行 |
| 单行 pipeline | `from tasks \| filter id == 1 \| take 1` | 与多行 pipeline 同语义 |

支持 `==`、`!=`、`>`、`>=`、`<`、`<=`，以及括号、`not`、`and`、`or`。数值表达式支持 `+`、`-`、`*`、`/` 与一元负号，乘除优先于加减，复杂算术可在括号内换行。运算数必须归一为同一个 int 或 float 类型；整数除法向零截断，溢出、除零或非有限 float 返回 `E_ARITH`。`contains tags value` 判断 list 成员，`length value` 接受 list 或 text；函数使用空格传参。复杂条件可放在 `filter` 或 match 分支 `=>` 后的缩进块中，括号内部也可跨行；混用 `and` 与 `or` 时规范写法加括号明确分组。所有 stage 从左到右执行；`take` 和 `filter` 不可交换，未排序查询不承诺稳定行序。多键排序按书写顺序比较；跨请求分页应以唯一主键结束排序。范围 `take` 是一基闭区间，例如 `11..20` 返回当前结果的第 11 到 20 行。字段和类型在扫描前校验，空表也会报错；`select` 之后不能访问已移除字段。

模式支持 sum 的 unit/record/位置负载和 option 的 `None`/`Some value`；record 可用 `{field = binding, ..}` 重命名绑定，也可递归写成 `{retry_at = Some at, point = (x, y), ..}`。多个同名顶层 constructor 可以用互补的嵌套 pattern 覆盖完整值域；非穷尽与被前序分支完全覆盖的情况会在扫描前报错。match condition 与普通 filter 共用布尔、集合和数值表达式。`derive` 分支可返回 binding/literal/算术表达式，或用 binding 和算术结果构造 `Some (attempt + 1)`、`State.Done`、`Summary {label = message}`、tuple、record 和 list；通用函数仍由 #36 跟踪。

兼容入口 `=`、`limit` 和无花括号的 `select id,name` 仍可执行。新代码与文档使用 `==`、`take` 和 `select {id, name}`。filter、match condition、derive 数值表达式与 update `set` 可引用 `$name`；完整 insert/upsert row 写成 `insert tasks $row`。参数由调用端提供 typed value，在 AST 上绑定并在扫描前按上下文检查，详见[版本化接口与参数](PROTOCOL.md)。`group/aggregate` 与完整表达式的当前状态统一记录在 [查询能力表](QUERY.md#能力状态)。

## 更新与删除

`update` 和 `delete` 从目标表开始，后续 `filter` 与查询使用相同的 bool 或穷尽 match 语义。多行更新先写完筛选，再写一个或多个 `set`：

```text
update tasks
filter match state
  Pending => true
  _ => false
set attempts = attempts + 1
set state = Done {result = "ok"}

delete tasks | filter id == 2
```

- `update table` 与 `delete table` 不带 filter 时作用于整张表；这是显式有效操作。
- 当前 mutation target 只接受 `filter` 与 `filter match`，并保持书写顺序。`select`、`derive`、`sort` 和 `take` 不属于修改目标。
- 所有 filter 必须写在第一个 `set` 前。多个 `set` 同时求值：每个右侧读取该行修改前的值，因此 `set left = right` 和 `set right = left` 会交换两列。
- `set` 右侧当前接受字段、literal、ADT constructor、`length` 和有类型算术。literal 按目标字段类型检查，未知字段和错误类型即使目标表为空也报错。
- 可直接设置 record 的嵌套路径，如 `set owner.email = "new@example.com"`。路径不能穿过 sum/option；修改 variant 时设置完整值。父路径与子路径不能在同一 update 中同时赋值，避免依赖隐含顺序。
- 每条候选 row 更新完成后重新检查完整 row 类型；全表重新检查主键唯一性，再原子替换 rows 与派生 indexes。任一行除零、溢出、类型或约束失败时，该请求不修改任何行。
- 成功 insert/update/delete 的 JSON 响应包含 `affected_rows`；update/delete 未命中时返回 0。内部稳定 RowId 不出现在用户 record 中，删除后不会被后续插入复用。

单行形式可用必要的 pipeline 分隔符，例如 `update tasks | filter id == 1 | set attempts = attempts + 1`。多项修改推荐换行，避免长表达式掩盖目标范围。

## 按主键 Upsert

`upsert` 接受一份行值，并按表声明的主键决定插入或替换：

```text
upsert config
  name = "worker"
  endpoint = {host = "worker.internal", port = 9000}
  tags = ["sync", "durable"]
```

- 表必须用 `key field` 声明主键；无主键时返回 `E_CONSTRAINT`。
- 输入按 insert 的完整 row 类型检查和默认值规则规范化。未命中主键时分配新 RowId；命中时替换完整 row、保留原 RowId，不产生第二条记录。
- 替换是整行语义。输入中省略的非主键字段只有在 schema 声明了默认值时才合法，并使用默认值，而非保留旧值；局部修改使用 `update ... set`。
- 成功响应的 `affected_rows` 为 1，`upsert_action` 明确返回 `inserted` 或 `updated`。重复提交同一主键仍走 `updated` 分支。
- 主键和派生索引与 row 在同一请求中原子更新；后续语句失败时，新插入或替换也会一起回滚。

## Schema migration

`migration name` 使用缩进 block 执行显式 schema 操作和 typed 数据转换：

```text
migration task_state_v2
  rename variant State.Failed to Rejected
  add field Task.priority int = 0
  change variant State.Rejected to {code int, message text}
    using old -> {code = 0, message = old.message}
```

当前支持 type/table/field/variant 的 add/drop/rename、field/variant 类型或 payload 转换、默认值变更，以及 index/primary-key 变更。命名 ADT 在所有表的嵌套路径中统一转换，保留稳定身份和 RowId；任一行或约束失败会回滚当前 migration 文件。完整语法、删除保护、版本化文件、checksum、plan/apply/status 与持久 ledger 见 [Schema migration 语言](MIGRATIONS.md)。

## 脚本边界与错误

同层的 `from` / `type` / `table` / `insert` / `upsert` / `update` / `delete` / `migration` / `create` 开始新语句；查询中的同层 `filter/derive/select/sort/take/limit` 延续读取 pipeline，update 中的同层 `filter/set` 延续修改语句，delete 中的同层 `filter` 延续删除语句。声明体、insert/upsert 的多行 record、migration、嵌套 record、变体负载、filter 条件和 match 分支通过缩进确定范围；退格必须回到已有缩进层级，缩进不能使用 tab。括号内允许换行，字符串中的管道和逗号不是语法分隔符。

空行与 `#` 注释不改变文件中的语句边界。多行字符串暂用 JSON 转义（例如 `"first\nsecond"`），不支持跨物理行的字符串字面量。当前不支持任意函数、list 元素 lambda 或 let/group。

一次 `Engine.execute`、一次 `run` 或一个 TCP 请求是一个原子批次：先解析全部源码，再在候选状态中执行；任一步失败则不发布此次请求的任何修改。成功返回最后一条语句的结果，批次中的查询可以看到前面的写入。当前通过复制内存数据库实现写批次隔离，适合小工作集，尚未优化大批量写入的内存成本。

错误包含 `code`、可读 `message`、可选 `span {line, column}`。语法错误标出 token 位置；执行前类型校验目前定位到所属语句，并给出具体字段路径，精确表达式 span 后续完善。源码最多 1 MiB、100,000 tokens、64 层类型／值／布局嵌套，超限返回错误。

## 本地与 TCP

```bash
cargo run -- run --file examples/tasks.uid
cargo run -- run --file examples/config.uid --format json
cargo run -- run --file examples/events.uid
cargo run -- run --file examples/sync_conflicts.uid
cargo run -- run --file examples/task_mutations.uid
cargo run -- cli --memory
cargo run --example embedded
cargo run --example parameters
```

`run` 每次创建一个内存库；`cli --memory` 的交互会话保留内存状态。在交互终端连续输入多行，空行显式提交整个缓冲区；`.quit` 退出，`.schema` 查看声明，`.tables` 列表。在管道或重定向 stdin 中读取到 EOF 后一次执行整个脚本，不按空行拆分。CLI 查询失败返回非零退出码。

```bash
cargo run -- server --addr 127.0.0.1:7878
cargo run -- cli --addr 127.0.0.1:7878 --file examples/tasks.uid
```

TCP 的稳定客户端入口是 [JSON Lines version 1](PROTOCOL.md)：请求包含 `version/request_id/query/params` 和可选 schema 前置条件，响应回显 ID，并以独立 wire codec 无损编码 ADT 与 i64。JSON 中的换行不会被压平。服务暂时兼容 `{"query":"..."}` 和旧纯文本单行请求。

响应包含 `ok/message/columns/rows/error/warnings/schema`，成功 DML 还包含 `affected_rows`，upsert 额外包含 `upsert_action`。`schema` 提供当前应用 schema 的 revision 与 SHA-256 hash；原子 schema 脚本只推进一次 revision，行写入与失败请求不推进，完整规则见 [Schema 身份与演进契约](SCHEMA.md)。`columns` 保留投影顺序和类型描述；version 1 的 `rows` 使用与内部 serde/存储 codec 分离的 typed wire value，i64 和稳定 ID 以十进制 string 传输。服务默认本机监听，当前限制 64 个活动连接、请求大小和 30 秒 socket 读写超时。连接数达到上限时，新连接收到一行 `E_BUSY` 响应后关闭，可以在已有连接释放后重试；拒绝过程使用独立的短超时，不执行请求。查询预算、取消、优雅关闭等仍待完善。
