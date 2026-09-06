# 查询语言参考

本页描述 **当前版本可以执行** 的查询语法与语义，是查询行为的规范入口。类型、表和写入语法见 [LANGUAGE.md](LANGUAGE.md)；尚未实现的表达式、transform、DML 和 migration 提案见 [DESIGN.md](DESIGN.md)。设计草案中的代码不能当作当前命令执行。

unionid 的查询从表开始，按书写顺序经过一组 transform：

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
    Failed {..} => "failed"
select {id, title, owner.email, state_label}
sort id
take 20
```

## 能力状态

| 能力 | 当前形式 | 状态 | 后续任务 |
| --- | --- | --- | --- |
| 数据源 | `from table` | 已实现 | — |
| 布尔过滤 | `filter priority >= 10 and contains tags "sync"` | 已实现括号、`not/and/or`、字段间比较、`contains/length` | #36 扩展算术、option helper 与元素谓词 |
| sum/option 模式过滤 | `filter match field` | 已实现 unit、record、位置负载、递归 record/tuple/sum/option pattern，以及完整嵌套穷尽与不可达检查 | #35 跟踪 prepared plan 重绑定 |
| 投影 | `select {field, nested.field}` | 已实现 | #11 与派生列组合 |
| 排序 | `sort field` / `sort {-priority, created_at, id}` | 已实现单列与多列 | #16 增加索引计划 |
| 截取 | `take 20` / `take 11..20` | 已实现前 N 行与一基闭区间 | — |
| 单行 pipeline | `from tasks \| filter id == 1 \| take 1` | 已实现 | — |
| 参数 | `$id` | 未实现 | #10、#22 |
| ADT 派生列 | `derive x = match ...` | 已实现递归 pattern、完整嵌套覆盖分析与 option/sum/product/list 值构造 | #35 跟踪 prepared plan 重绑定；#36 增加算术/函数表达式 |
| 布尔表达式与集合函数 | `and/or/not`、`contains/length` | 已实现于普通 filter 与 match condition | #36 扩展 `any/all` 等能力 |
| 其他派生列 | `derive` | 未实现 | #11 |
| 分组与汇总 | `group`、`aggregate` | 未实现 | #11 |
| 更新与删除 | `update`、`delete`、`upsert` | 未实现 | #15 |
| migration 查询与转换 | `migration` | 未实现 | #17、#18 |
| join、window、递归和高阶函数 | — | v0.1 延后 | #25 |

“未实现”的词只在状态表和限制说明中出现。除明确标为反例的片段外，本页其余查询代码均可由当前 parser 执行。

## Pipeline 语法

推荐的多行形式每行写一个 stage，不使用分号或逐行管道符：

```text
from config
filter endpoint.port >= 8000
select {name, endpoint.host, mode}
sort name
take 10
```

紧凑查询可在同一行使用 `|`：

```text
from config | filter endpoint.port >= 8000 | select {name, mode} | take 10
```

两种形式按相同顺序产生相同结果。规范语法轮廓如下：

```text
query             = "from" table pipeline-stage*
pipeline-stage    = newline stage | "|" stage
stage             = value-filter | match-filter | derive-match | select | sort | take

value-filter      = "filter" nested-bool-expression
match-filter      = "filter" "match" field-path newline indent match-arm+ dedent
match-arm         = arm-pattern "=>" nested-bool-expression newline?
derive-match      = "derive" identifier "=" nested-match-expression
nested-match-expression = match-expression | newline indent match-expression dedent
match-expression  = "match" field-path newline indent match-value-arm+ dedent
match-value-arm   = arm-pattern "=>" match-value newline?
match-value       = binding-path | literal | constructor-value | record-value | tuple-value | list-value
constructor-value = qualified-variant (record-value | value-argument*)?
value-argument    = binding-path | literal | "(" match-value ")" | tuple-value | list-value
record-value      = "{" value-field ("," value-field)* ","? "}"
value-field       = identifier "=" match-value
tuple-value       = "(" match-value "," (match-value ("," match-value)*)? ")"
list-value        = "[" (match-value ("," match-value)* ","?)? "]"
select            = "select" "{" field-path ("," field-path)* ","? "}"
sort              = "sort" sort-key | "sort" "{" sort-key ("," sort-key)* ","? "}"
sort-key          = "-"? field-path
take              = "take" nonnegative-integer | "take" positive-integer ".." positive-integer

arm-pattern       = "_" | constructor-pattern
pattern           = "_" | binding | constructor-pattern | record-pattern | tuple-pattern
constructor-pattern = qualified-variant (record-pattern | pattern-argument*)?
qualified-variant = Variant | Type "." Variant
pattern-argument  = binding | "_" | "(" pattern ")" | tuple-pattern
record-pattern    = "{" (field-pattern ("," field-pattern)* ("," "..")? | "..")? "}"
field-pattern     = identifier ("=" pattern)?
tuple-pattern     = "(" pattern "," (pattern ("," pattern)*)? ")"
binding           = lowercase-identifier
nested-bool-expression = bool-expression | newline indent bool-expression dedent
bool-expression   = or-expression
or-expression     = and-expression ("or" and-expression)*
and-expression    = not-expression ("and" not-expression)*
not-expression    = "not" not-expression | bool-primary
bool-primary      = "(" bool-expression ")" | contains-call | scalar-expression (comparison scalar-expression)?
contains-call     = "contains" scalar-expression scalar-expression
scalar-expression = field-path | literal | "length" scalar-expression
comparison        = "==" | "!=" | ">" | ">=" | "<" | "<="
field-path        = identifier ("." identifier)*
```

`=`、`limit` 和不带花括号的 `select id,name` 是兼容入口。新文档和格式化输出应使用 `==`、`take` 与 `select {id, name}`。

复杂条件的规范格式是 `filter` 后缩进一层，每行写一个逻辑项并把 `and` 或 `or` 放在续行开头：

```text
from jobs
filter
  priority >= 10
  and contains tags "sync"
  and not archived
select {id}
```

混用 `and` 与 `or` 时应加括号直接表达分组，不要求读者仅凭优先级判断：

```text
filter
  (urgent or priority >= 50)
  and contains tags "sync"
```

括号内部可以跨行，因此更深的组合也能保持一个条件一行：

```text
filter (
  urgent
  or (
    priority >= 50
    and not archived
  )
)
```

单个比较或很短的同类组合仍可写在 `filter` 同一行。括号用于说明分组，record/list/tuple 继续使用各自已有的必要标点；语言不会为了追求“零符号”而隐藏结构。

## 执行模型

Pipeline 严格从左到右执行。每个 stage 接收前一个 stage 的行和 schema，再产生下一个 stage 的输入。优化不能跨越会改变结果的边界，例如：

```text
from tasks | take 1 | filter id > 1
```

与下面的查询语义不同：

```text
from tasks | filter id > 1 | take 1
```

引擎在读取任何行之前，按 stage 顺序检查整条 pipeline。空表上的未知字段、类型错误、非穷尽 match 和分支结果类型冲突仍然报错。`derive` 将新字段加入后续 schema；`select` 会改变后续 schema，因此投影掉的字段不能再用于 filter、match、derive 或 sort。

| Stage | 输出 schema | 行与顺序语义 | 空输入 |
| --- | --- | --- | --- |
| `from` | 表的完整行类型 | 读取表；未排序行序不构成承诺 | 返回带完整 schema 的空结果 |
| `filter` | 不变 | 只保留条件为真的行 | 仍执行字段与类型检查 |
| `filter match` | 不变 | 每行按其 sum/option constructor 执行唯一分支的条件 | 仍执行模式绑定和穷尽检查 |
| `derive match` | 追加一个有静态类型的字段 | 每行执行唯一分支，行数与顺序不变 | 仍统一分支结果类型 |
| `select` | 按书写顺序组成新 schema | 每行只保留选择的字段 | 返回带投影 schema 的空结果 |
| `sort` | 不变 | 单列或多列词典序；全部键相同的次序不承诺 | 返回空结果但仍检查全部键 |
| `take` | 不变 | 保留前 N 行，或一基闭区间内的行；无 sort 时位置不稳定 | 返回空结果但仍检查范围 |

最终响应的 `columns` 来自最后一个 stage 的 schema，并保持 `select` 的字段顺序。嵌套字段的结果列名保留完整路径，例如 `owner.email`。

## 布尔过滤表达式

普通 `filter` 接受一个静态类型为 bool 的表达式：

```text
from jobs
filter
  not archived
  and priority >= threshold
filter
  contains tags "sync"
  or length tags == 0
select {id, priority}
```

- 比较的两侧可以是字段路径、literal 或 `length`。因此支持字段与字段、字段与 literal，以及 `length tags >= minimum_tags`；至少一侧必须能确定类型。
- `==` 和 `!=` 支持类型一致的完整值，包括 record、tuple、sum、option 和 list。literal 会按另一侧的类型检查，因此 `contains [] 1` 是类型明确且恒为 false 的合法表达式。
- `>`、`>=`、`<`、`<=` 当前只支持 int、float 和 text。
- Int 使用精确 i64 比较。Float 使用精确数值相等，`-0.0` 与 `0.0` 相等；拒绝 NaN、Infinity 和超出 i64 的整数。
- Float 字段可接受能够精确表示的整数字面量；Int 字段不接受浮点字面量。数字不会自动转换成 text。
- `contains collection item` 只接受 list，并按 list 元素的完整类型化相等语义判断；元素可以是命名 ADT。`length` 接受 list 或 text，分别返回元素数或 Unicode scalar 数。
- bool 字段可以直接作为条件。其他类型不隐式转换为 bool；option 也不提供 truthiness，必须显式 match。
- 优先级从高到低为括号／比较／函数、`not`、`and`、`or`。混用 `and` 与 `or` 的规范源码使用括号明确分组。`and` 和 `or` 在运行时从左到右短路；两侧仍会在扫描前完成类型检查，短路不会隐藏未知字段或类型错误。
- 普通字段路径只能穿过 record。variant 和 option 的内容必须用显式模式处理。

如果查询的第一个 stage 是单纯的 `field == literal` 或 `literal == field`，并且该字段有索引，引擎可以直接读取候选行。复合布尔表达式暂时扫描候选表。索引和扫描共用相同的类型化相等规则；是否存在索引不能改变结果。

## 模式过滤

`filter match` 检查一个 sum 或 option 字段，并在当前 constructor 的负载中建立局部绑定：

```text
from tasks
filter match state
  Pending => false
  Running {worker, attempt} => worker == "local"
  Done {result} => result == "ok"
  Failed {retryable, ..} => retryable
select {id, title}
```

模式分支必须比 `filter match` 多缩进一层。分支块结束后，后续 stage 回到 `from` 的 pipeline 缩进：

```text
from tasks
filter match state
  Running {attempt, ..} =>
    attempt >= 2
    and attempt < 5
  _ => false
select {id}
take 1
```

当前规则如下：

- unit 变体直接写 `Pending`。带单个 record 负载的变体写 `Running {worker, attempt}`；位置负载用空格绑定，例如 `Pair left right`。
- option 使用 `None` 与 `Some value`；写成 `Some _` 可以只判断存在而忽略内容。
- record 字段名默认也是局部绑定；`{retry_at = at, ..}` 把字段重命名为 `at`。等号右侧也可递归使用 constructor、record 或 tuple pattern，例如 `{error = Network {message}, retry_at = Some at}`。绑定可以继续访问嵌套 record，例如 `meta.attempts >= 3`。
- tuple 用自身的积类型标点分解，例如 `Some (at, reason)`；`_` 可出现在任意嵌套位置并忽略该值。位置 constructor 的参数仍用空格，例如 `Pair left right`。一个位置参数本身又是 constructor 时用括号明确边界，例如 `Outer (Some value) other`。
- `{attempt, ..}` 绑定 `attempt` 并显式忽略其他字段。不写 `..` 时必须列出该负载的全部字段，避免 schema 新增字段后被静默忽略。
- 顶层 `_` 覆盖尚未出现的值，必须位于最后。没有 `_` 时，多个同名顶层 constructor 分支可以用互补的嵌套 pattern 覆盖完整值域。例如 `Failed {retry_at = Some at, ..}` 与 `Failed {retry_at = None, ..}` 可以共同覆盖 `Failed`；只写其中一个仍然是非穷尽 match。
- 构造器由被匹配字段的命名类型确定，也可写成 `State.Running`。其他命名 sum 的同名构造器不会混用。
- condition 与普通 filter 共用布尔表达式 binder 和 evaluator，支持绑定间比较、括号、`not/and/or`、`contains/length`。复杂 condition 可在 `=>` 后换行并缩进一层；绑定只在所属分支内有效。
- 分支按源码顺序选择第一个匹配项。检查器使用有预算的 pattern matrix 分析 sum、option、record 与 tuple 的组合关系，允许可到达的重叠分支，拒绝被先前分支完全覆盖的分支。非穷尽错误会同时列出仍未完全覆盖的顶层 constructor，并给出一个具体嵌套值样例。覆盖分析最多执行 100,000 步，超限返回 `E_LIMIT`。

未知构造器、重复分支、通配分支后的不可达分支、遗漏 constructor、错误负载字段及分支作用域错误会在扫描前返回 `E_MATCH`。非 sum/option 来源或非 bool 条件返回类型错误。

## ADT 派生列

`derive name = match ...` 解构一个 sum/option，并把所有分支归一成一个有静态类型的新字段。分支可以直接从 binding 构造新值：

```text
from jobs
derive retry_at =
  match state
    Failed {retry_at = Some at, ..} => Some at
    _ => None
filter retry_at == Some 30
select {id, retry_at}
```

`match` 可以与 `=` 写在同一行，也可像上例多缩进一层。派生字段追加到当前 schema，后续 filter、derive、select 和 sort 都可以引用它；名称与已有字段冲突时拒绝。

当前分支结果可以是局部 binding（含嵌套 record 路径）、literal，或递归的 constructor/record/tuple/list 值。空格表示 constructor 应用，例如 `Some at`；位置参数本身是复合值时用括号划定边界，例如 `Display.Retrying (Summary {label = message})`。命名 record 可写成 `Summary {label = message}`，省略字段会使用其 schema 默认值；缺少必填字段和未知字段仍报错。

引擎先从 binding、primitive literal、`Some value`、结构化 product 或限定 constructor 推导结果类型，再按该类型检查全部分支。`None`、空 list/record 和未限定的普通 sum constructor 不能单独确定类型，但可在其他分支已经给出类型时使用；需要主动确定命名 sum 时写 `Type.Variant`，命名 record 写 `TypeName {...}`。不同命名类型不会因结构相同而统一。

分支必须穷尽且结果类型一致，constructor 归属、参数数量、嵌套覆盖关系和每个值都在扫描前检查。pattern 可以递归解构 record、tuple、sum 和 option，同一个顶层 constructor 可以由多个互补嵌套分支覆盖。派生结果尚未复用 filter 的布尔／函数节点，也不支持算术，这些由 #36 的通用 value expression 继续扩展；prepared plan 的 schema revision 重绑定仍由 #35 后续完成。

## 投影、排序与截取

`select` 接受一个或多个字段路径，拒绝重复或未知字段。输出列顺序就是选择顺序：

```text
from tasks
select {title, id, owner.email}
```

`sort field` 升序排列，`sort -field` 降序排列。多个排序键写成 `sort {key, -descending_key, final_key}`，按书写顺序做词典序比较。键可以是嵌套 record 路径，当前只接受 int、float 和 text；重复键在解析时拒绝，未知或不可排序键即使在空表上也会报错。

全部排序键相同时，引擎不承诺原行顺序。分页或队列查询需要可复现顺序时，应把 int/text 主键作为最后一个键：

```text
from jobs
sort {-priority, created_at, id}
take 11..20
```

`take N` 接受非负整数并保留前 N 行，`take 0` 返回空行但仍保留当前结果 schema。`take start..end` 使用一基闭区间，因此 `take 11..20` 跳过前 10 行并最多返回 10 行；尾部越界返回剩余行。start 必须至少为 1，end 不能小于 start。范围总是相对于该 stage 收到的当前结果。

如果业务依赖“前 N 行”，必须先 sort：

```text
from tasks
sort id
take 20
```

## 布局与语句边界

- 顶层 `from` 开始一条查询。同层 `filter`、`derive`、`select`、`sort`、`take` 或兼容的 `limit` 延续当前 pipeline。
- `filter` 条件块、`filter match`／`derive ... match` 的分支，以及 `=>` 后的 condition 块通过缩进进入和退出；退格必须回到已有缩进层级。缩进不能使用 tab。
- 空行与 `#` 注释不结束查询。文件和非交互 stdin 在 EOF 提交完整脚本。
- 括号和集合内允许换行。字符串里的 `|`、逗号和 `#` 都是文本，不参与分隔。
- 同层出现新的 `from`、`type`、`table`、`insert` 或 `create` 时，前一条查询结束并开始新语句。
- REPL 的空行是提交当前完整缓冲区的交互手势，不是文件语法的一部分。

布尔表达式可在 `filter`／`=>` 的缩进块或括号内跨行；标量函数参数和比较两侧当前保持在同一逻辑行。源码最多 1 MiB、100,000 tokens 和 64 层类型、值、表达式或布局嵌套；超限返回受控错误。

## 错误与响应

查询失败返回结构化 `error`，其中包含错误码、可读消息和可选 `span {line, column}`。语法错误定位到 token；执行前检查目前定位到所属语句，并在消息中给出字段或绑定路径。

常见错误类别：

| 错误 | 例子 |
| --- | --- |
| `E_FIELD` | 未知字段，或 `select` 后访问已经移除的字段 |
| `E_TYPE` | 对 sum 排序、比较类型不匹配、match 非 sum 字段 |
| `E_MATCH` | 未知/重复构造器、非穷尽 match、错误负载字段或分支绑定 |
| `E_SYNTAX` | 缺少操作符、错误缩进、未闭合结构或尾部多余 token |
| `E_LIMIT` | 源码、token 数或嵌套深度超过限制 |

成功响应包含 `rows` 和有序的 `columns {name, ty}`。未命中任何行时仍返回推导后的 columns。

以下片段是故意失败的反例：

| 反例 | 结果 |
| --- | --- |
| `from tasks \| filter missing == 1` | `E_FIELD`：字段不存在，即使 tasks 为空也会报错 |
| `from tasks \| select {id} \| sort state` | `E_FIELD`：state 已被投影移除 |
| `from tasks \| sort state` | `E_TYPE`：sum 类型没有排序语义 |

非穷尽模式在扫描前返回 `E_MATCH`：

```text
from tasks
filter match state
  Pending => true
```

## 可执行示例与测试

| 场景 | 示例 | 覆盖内容 | 自动验证 |
| --- | --- | --- | --- |
| 任务状态 | [tasks.uid](../examples/tasks.uid) | sum、record、option/list、`filter match`、select/sort/take | `tests/language.rs::executable_examples`、CLI/TCP/恢复测试 |
| 嵌套配置 | [config.uid](../examples/config.uid) | 嵌套字段过滤与投影 | `tests/language.rs::executable_examples` |
| 事件记录 | [events.uid](../examples/events.uid) | sum 完整值比较、typed derive 和字符串中的 `|` | `tests/language.rs::executable_examples` |
| 后台任务队列 | [job_queue.uid](../examples/job_queue.uid) | 嵌套 sum/record/option/list、布尔/集合 filter、嵌套 pattern、ADT derive、多键 sort 与范围 take | `tests/language.rs::executable_examples` |
| 离线同步冲突 | [sync_conflicts.uid](../examples/sync_conflicts.uid) | 同一 `Conflict` constructor 的互补嵌套分支、typed derive 与 Option | `tests/language.rs::executable_examples` |
| Pipeline 顺序 | 测试内脚本 | take/filter 顺序与投影作用域 | `stage_order_and_projection_paths_are_preserved` |
| 单行/多行 | 测试内脚本 | 两种 pipeline 布局等价 | `newline_and_inline_pipelines_have_identical_results` |
| 模式检查 | 测试内脚本 | 嵌套穷尽性、不可达分支、积类型相关性、名义构造器、绑定和错误路径 | `match_filters_*`、`match_is_checked_*`、`match_rejects_*`、`complementary_nested_*`、`nested_pattern_coverage_*` |
| ADT 派生 | 测试内脚本 | 递归 pattern、option/sum/product/list 构造、类型统一、空表诊断与后续 stage | `derive_match_*`、`option_and_positional_*`、`nested_patterns_*`、`constructed_match_*` |
| 布尔与集合表达式 | 测试内脚本 | 优先级、括号、短路结构、字段间比较、命名 ADT list、`contains/length` 和空表错误 | `boolean_filters_*`、`match_conditions_share_*`、`boolean_expressions_are_checked_*` |
| 列表分页 | 测试内脚本 | 嵌套多键排序、一基闭区间、兼容语法和空表错误 | `multi_key_sort_*`、`sort_keys_and_take_ranges_*` |

新增语法只有在 parser、执行器、正反测试和本页同步后，才能从“未实现”移动到“已实现”。
