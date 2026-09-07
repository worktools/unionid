# 当前可运行的语言预览

本页是 unionid 当前可执行语言的规范入口。第一次使用可先走完[五分钟持久数据库教程](GETTING_STARTED.md)。示例和规则都由现有实现支持；查询的完整语义见 [QUERY.md](QUERY.md)，schema 演进见 [MIGRATIONS.md](MIGRATIONS.md)，声明式目标结构见 [SCHEMA-DIFF.md](SCHEMA-DIFF.md)，实际应用覆盖见 [SCENARIOS.md](SCENARIOS.md)，未来设计单独放在 [DESIGN.md](DESIGN.md)，不能据此推断当前语法。完整脚本可运行：[任务](../examples/tasks.uid)、[任务修改](../examples/task_mutations.uid)、[schema migration](../examples/schema_migration.uid)、[后台队列](../examples/job_queue.uid)、[配置](../examples/config.uid)、[事件](../examples/events.uid)、[同步冲突](../examples/sync_conflicts.uid)、[有限递归树](../examples/recursive_tree.uid)。

当前包含类型与表声明、insert/upsert/update/delete 及 typed `returning`、版本化 schema migration、布尔 filter、sum/option 的 `filter match`、查询局部 let/纯函数、普通与 ADT `derive`、group/aggregate、select、sort、take，以及结构化 `explain`。filter 与 match/derive/set/migration conversion 表达式支持有类型的 int/float 算术；filter 和普通 derive 还支持 `not/and/or`、字段间比较、Option helper 及 `contains/length/any/all`。`$name` 参数通过 Rust API 或版本化 TCP 协议绑定。

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
- 原子类型为 `int`（i64）、`float`（有限 f64）、`bool`、`text`。其他命名类型必须先声明；类型定义可以直接引用自身。当前不支持两个或多个类型的互递归，也不支持用户自定义泛型。
- 支持命名 record/sum、嵌套积类型、tuple，以及内建 `option T`、`list T`，例如 `type Point = (float, float)`、`option (list Contact)`。
- record 类型可内联为 `{email text, nickname option text}`；变体负载也可缩进：在 `| Running` 的下一层写 `worker text` 和 `attempt int`。
- record 值使用 `field = value`，内联字段之间用逗号；列表如 `[1, 2]`，tuple 如 `(1, "x")`。位置负载写成 `Pair(1, "x")`；单个 tuple 负载与多个位置参数通过括号区分。
- 字段默认值写成 `field type = value`，例如 `nickname option text = None`、`tags list text = []`。默认值必须是可按字段类型检查的纯字面值，在 schema 声明时完成校验并存为完整 typed value；不能引用其他字段、参数、时钟或函数。
- 没有默认值的字段全部必填，即使类型为 option 也必须显式写 `None`。缺失字段逐层使用它自身声明的默认值；显式值不会因为类型错误而退回默认值。重复、缺失、未知字段及错误负载均报错。
- 命名类型保留身份；有歧义时可用 `State.Pending` 或 `State.Running {...}` 限定构造器。
- `table tasks Task` 要求 Task 是 record；可选的缩进 `key id` 声明 int/text 主键，拒绝重复键。无 key 时允许重复行。

直接自递归沿用同一套无分号声明语法，不增加 `rec` 标记。递归类型必须至少能构造一个有限值：sum 需要终止变体，record/tuple 的每个必需成员都必须可终止，`option` 的 `None` 与 `list` 的空列表可作为终止路径。例如：

```text
type Tree =
  Leaf text
  | Branch
    label text
    children list Tree

type Chain =
  value int
  next option Chain = None
```

`type Loop = Loop` 和 `type Endless = Next Endless` 会返回 `E_SCHEMA`。值仍是有界的有限树，不具有指针身份、共享子树或循环对象图；声明、值、codec 与查询的嵌套深度上限为 64。完整设计边界见 [RFC 0001：有限自递归命名 ADT](rfc/0001-finite-recursive-adts.md)。查询局部函数仍为非递归函数。

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
| 布尔过滤 | `filter any attempts (attempt -> attempt.failed)` | 组合 bool、比较、Option 检查、list/text 长度、成员判断与元素字段谓词 |
| 模式过滤 | `filter match state` | 按 sum 变体及其 record 负载判断 |
| 普通派生 | `derive score = priority + bonus` | 产生 scalar 或 bool typed 列并加入后续 stage 作用域 |
| ADT 派生 | `derive label = match state ...` | 穷尽解构 sum/option，追加统一类型的结果列 |
| 局部定义 | `let retryable = attempt -> attempt < 3` | 定义常量或有类型、非递归纯函数，供后续 stage 展开复用 |
| 汇总 | `aggregate` / `group state` | count/sum/min/max；分组键保留完整 ADT 类型和值 |
| 投影 | `select {id, owner.email}` | 保留列，响应按声明的列顺序展示 |
| 排序 | `sort id` / `sort {-priority, created_at, id}` | 单列或多列词典序；支持 int、float、text |
| 截取 | `take 20` / `take 11..20` | 保留前 N 行或一基闭区间内的行 |
| 单行 pipeline | `from tasks \| filter id == 1 \| take 1` | 与多行 pipeline 同语义 |
| 计划 | `explain from tasks \| filter id == 1` | 只绑定查询并返回 full scan／索引 lookup、候选数、stage 顺序和结果 schema |

支持 `==`、`!=`、`>`、`>=`、`<`、`<=`，以及括号、`not`、`and`、`or`。数值表达式支持 `+`、`-`、`*`、`/` 与一元负号，乘除优先于加减，复杂算术可在括号内换行。运算数必须归一为同一个 int 或 float 类型；整数除法向零截断，溢出、除零或非有限 float 返回 `E_ARITH`。`contains tags value` 判断 list 成员，`length value` 接受 list 或 text；`any items (item -> condition)` 和 `all ...` 提供有类型、可嵌套且有预算的元素字段谓词，`is_some`/`is_none` 显式检查 Option。函数使用空格传参。复杂条件可放在 `filter` 或 match 分支 `=>` 后的缩进块中，括号内部也可跨行；混用 `and` 与 `or` 时规范写法加括号明确分组。所有 stage 从左到右执行；`take` 和 `filter` 不可交换，未排序查询不承诺稳定行序。多键排序按书写顺序比较；跨请求分页应以唯一主键结束排序。范围 `take` 是一基闭区间，例如 `11..20` 返回当前结果的第 11 到 20 行。字段和类型在扫描前校验，空表也会报错；`select` 之后不能访问已移除字段。

模式支持 sum 的 unit/record/位置负载和 option 的 `None`/`Some value`；record 可用 `{field = binding, ..}` 重命名绑定，也可递归写成 `{retry_at = Some at, point = (x, y), ..}`。多个同名顶层 constructor 可以用互补的嵌套 pattern 覆盖完整值域；非穷尽与被前序分支完全覆盖的情况会在扫描前报错。match condition 与普通 filter 共用布尔、集合和数值表达式。`derive name = expression` 直接追加 scalar 或 bool 列；结果保留命名类型，typed 参数在扫描前推导，后续 filter/derive/select/sort 可立即引用。`derive match` 分支还可返回 binding/literal/算术表达式，或用 binding 和算术结果构造 `Some (attempt + 1)`、`State.Done`、`Summary {label = message}`、tuple、record 和 list。

查询局部定义使用 `let name = expression` 或 `let name = argument -> expression`。多个参数写成 `(left, right) ->`；通常从调用字段推断类型，歧义时使用 `let missing option int = None`、`let present = (value option int) -> is_some value` 这样的字段式注解。调用使用空格，嵌套调用加括号。定义只作用于当前 pipeline 的后续 stage，只能调用更早定义的函数，不支持递归、泛型或函数值；详见[查询局部 let 与纯函数](QUERY.md#查询局部-let-与纯函数)。

`explain` 可直接放在单行查询前，也可把完整查询缩进到下一层。它执行与真实查询相同的静态绑定，返回结构化访问计划，但不读取或执行数据行。只有开头的单纯有索引等值 filter（允许前置 let）使用 lookup；planner 不越过其他 stage。完整字段与测量规则见 [Explain 与类型化索引计划](QUERY.md#explain-与类型化索引计划)。

未分组 `aggregate` 在空输入上返回一行：count 为 0、sum 为输入数值类型的零、min/max 为 `None`；分组空输入返回零行。aggregate 后可继续 filter/select/sort/take。输入类型、顺序语义和资源上限见[分组与基础汇总](QUERY.md#分组与基础汇总)。

兼容入口 `=`、`limit` 和无花括号的 `select id,name` 仍可执行。新代码与文档使用 `==`、`take` 和 `select {id, name}`。filter、match condition、derive 数值表达式与 update `set` 可引用 `$name`；完整 insert/upsert row 写成 `insert tasks $row`。参数由调用端提供 typed value，在 AST 上绑定并在扫描前按上下文检查，详见[版本化接口与参数](PROTOCOL.md)。

## 更新与删除

`update` 和 `delete` 从目标表开始，后续 `filter` 与查询使用相同的 bool 或穷尽 match 语义。多行更新先写完筛选，再写一个或多个 `set`：

```text
update tasks
filter match state
  Pending => true
  _ => false
set attempts = attempts + 1
set state =
  match state
    Pending => Done {result = "ok"}
    current => current
returning id, state

delete tasks | filter id == 2 | returning
```

- `update table` 与 `delete table` 不带 filter 时作用于整张表；这是显式有效操作。
- 当前 mutation target 只接受 `filter` 与 `filter match`，并保持书写顺序。`select`、`derive`、`sort` 和 `take` 不属于修改目标。
- 所有 filter 必须写在第一个 `set` 前。多个 `set` 同时求值：每个右侧读取该行修改前的值，因此 `set left = right` 和 `set right = left` 会交换两列。
- `set` 右侧接受字段、literal、ADT constructor、`length` 和有类型算术，也可在下一层写 `match source`，使用与 `derive match` 相同的递归 pattern 和 option/sum/product/list 构造。目标字段给出分支结果类型，未知字段、非穷尽／不可达分支和错误结果即使目标表为空也报错。
- 顶层小写 binding 是带类型的不可反驳 pattern，必须是最后一支；`current => current` 可保留其余 constructor 的完整原值。分支可使用 typed 参数和自己的 pattern bindings。
- 可直接设置 record 的嵌套路径，如 `set owner.email = "new@example.com"`。路径不能穿过 sum/option；修改 variant 时设置完整值。父路径与子路径不能在同一 update 中同时赋值，避免依赖隐含顺序。
- 每条候选 row 更新完成后重新检查完整 row 类型；全表重新检查主键唯一性，再原子替换 rows 与派生 indexes。任一行除零、溢出、类型或约束失败时，该请求不修改任何行。
- 成功 insert/upsert/update/delete 的 JSON 响应包含 `affected_rows`；update/delete 未命中时返回 0。末尾的 `returning` 返回完整受影响行，`returning id, state` 按给定顺序投影字段：insert/upsert 返回默认值补齐后的新行，update 返回后像，delete 返回前像。空命中仍返回稳定 columns 和空 rows。
- returning 字段在扫描前按表 schema 检查，行数和 8 MiB typed wire 预算也在提交前检查；失败不会发布 row 或索引。内部稳定 RowId 不出现在用户 record 中，删除后不会被后续插入复用。

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

同层的 `from` / `explain` / `type` / `table` / `insert` / `upsert` / `update` / `delete` / `migration` / `create` 开始新语句；查询中的同层 `let/filter/derive/aggregate/group/select/sort/take/limit` 延续读取 pipeline，update 中的同层 `filter/set` 延续修改语句，delete 中的同层 `filter` 延续删除语句。声明体、explain 查询、insert/upsert 的多行 record、migration、嵌套 record、变体负载、filter 条件、filter/derive/set 的 match 分支和 group/aggregate block 通过缩进确定范围；退格必须回到已有缩进层级，缩进不能使用 tab。括号内允许换行，字符串中的管道和逗号不是语法分隔符。

空行与 `#` 注释不改变文件中的语句边界。多行字符串暂用 JSON 转义（例如 `"first\nsecond"`），不支持跨物理行的字符串字面量。局部函数只复用当前纯 expression IR；当前不支持全局函数、递归、泛型、高阶函数或持久化闭包。`any/all` 的元素 predicate 与 let 函数都不产生可存储的函数值。

Rust 客户端可以调用 `unionid::input_status(source)`，在执行前得到 `InputStatus::Complete`、`InputStatus::Incomplete(error)` 或 `InputStatus::Invalid(error)`。判断直接复用 lexer 的 delimiter/layout 状态和 parser 的 EOF 状态：未闭合的括号、等待缩进体的声明、match 分支或尾部 `|` 属于 incomplete；不匹配的退格、tab 缩进、未知 stage 和尾部垃圾属于 invalid。语法错误保留 `span`，incomplete 使用 `E_INCOMPLETE`。这个 API 只判断语法能否继续，不打开数据库，也不检查表、字段或值的类型。

## 规范格式

`unionid::format_source(source)` 先解析完整脚本，再输出确定的无分号源码。顶层语句用一个空行分隔，缩进固定为两个空格，pipeline 每个 stage 独占一行，select 和多键 sort 使用 `{}`，兼容的 `=`/`limit` 会归一为 `==`/`take`。formatter 按 bool 与算术 precedence 生成必要括号；嵌套函数、ADT pattern/value、migration transform 和 explain 都可再次解析。格式化后的第二次输出保持字节不变，schema-only 脚本保持 schema identity。

`unionid fmt --file path.uid` 把结果写到 stdout，不修改源文件；不提供 `--file` 时读取 stdin。`unionid fmt --file path.uid --check` 只校验，格式漂移或语法错误返回非零，语法错误保留 span。注释文本会保留；当前 parser AST 不保存 trivia 的精确节点归属，因此 inline 或 block 内注释会稳定移动到随后的顶层语句边界，文件尾注释保留在末尾。

一次 `Engine.execute`、一次 `run` 或一个 TCP 请求是一个原子批次：先解析全部源码，再在候选状态中执行；任一步失败则不发布此次请求的任何修改。成功返回最后一条语句的结果，批次中的查询可以看到前面的写入。当前通过复制内存数据库实现写批次隔离，适合小工作集，尚未优化大批量写入的内存成本。

错误包含 `code`、可读 `message`、可选 `span {line, column}`。语法错误标出 token 位置；执行前类型校验目前定位到所属语句，并给出具体字段路径，精确表达式 span 后续完善。源码最多 1 MiB、100,000 tokens、64 层类型／值／布局嵌套，超限返回错误。

## 本地与 TCP

```bash
cargo run -- run --file examples/tasks.uid
cargo run -- run --file examples/config.uid --format json
cargo run -- run --file examples/events.uid
cargo run -- run --file examples/sync_conflicts.uid
cargo run -- run --file examples/task_mutations.uid
cargo run -- fmt --file examples/tasks.uid --check
cargo run -- cli --memory
cargo run --example embedded
cargo run --example parameters
```

`run` 每次创建一个内存库；`cli --memory` 的交互会话保留内存状态。本地和 TCP REPL 使用同一个输入状态机：`unionid>` 等待新脚本，`..>` 表示仍需续写，`ready>` 表示当前缓冲区语法完整。空行只提交完整缓冲区；对 incomplete 输入按空行会显示原因并继续，对 invalid 输入会立即显示位置并清空缓冲区。交互 EOF 执行一次完整缓冲区；若缓冲区 incomplete，则报告错误后退出。`.quit` 退出，`.schema` 查看声明，`.tables` 列表。在管道或重定向 stdin 中仍读取到 EOF 后一次执行整个原子脚本，不按空行拆分。CLI 查询失败返回非零退出码。

```bash
cargo run -- server --addr 127.0.0.1:7878
cargo run -- cli --addr 127.0.0.1:7878 --file examples/tasks.uid
```

TCP 的稳定客户端入口是 [JSON Lines version 1](PROTOCOL.md)：请求包含 `version/request_id/query/params` 和可选 schema 前置条件，响应回显 ID，并以独立 wire codec 无损编码 ADT 与 i64。JSON 中的换行不会被压平。服务暂时兼容 `{"query":"..."}` 和旧纯文本单行请求。

响应包含 `ok/message/columns/rows/error/warnings/schema`，成功 DML 还包含 `affected_rows`，upsert 额外包含 `upsert_action`；带 returning 的 DML 同时填充 typed `columns/rows`。`schema` 提供当前应用 schema 的 revision 与 SHA-256 hash；原子 schema 脚本只推进一次 revision，行写入与失败请求不推进，完整规则见 [Schema 身份与演进契约](SCHEMA.md)。`columns` 保留投影顺序和类型描述；version 1 的 `rows` 使用与内部 serde/存储 codec 分离的 typed wire value，i64 和稳定 ID 以十进制 string 传输。服务默认本机监听；连接、frame、working/result rows、response bytes、execution deadline、socket timeout、SIGINT/SIGTERM 关闭和重试语义见[服务运行边界](SERVICE.md)。
