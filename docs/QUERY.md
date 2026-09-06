# 查询语言参考

本页描述 **当前版本可以执行** 的查询语法与语义，是查询行为的规范入口。类型、表和写入语法见 [LANGUAGE.md](LANGUAGE.md)；尚未实现的表达式、transform、DML 和 migration 提案见 [DESIGN.md](DESIGN.md)。设计草案中的代码不能当作当前命令执行。

unionid 的查询从表开始，按书写顺序经过一组 transform：

```text
from tasks
filter match state
  Running {attempt, ..} => attempt >= 2
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
| 值过滤 | `filter field >= literal` | 已实现 | #10 扩展为通用表达式 |
| sum/option 模式过滤 | `filter match field` | 已实现 unit、record、位置负载、递归 record/tuple/sum/option pattern 和穷尽检查 | #35 完善嵌套穷尽分析 |
| 投影 | `select {field, nested.field}` | 已实现 | #11 与派生列组合 |
| 排序 | `sort field` / `sort {-priority, created_at, id}` | 已实现单列与多列 | #16 增加索引计划 |
| 截取 | `take 20` / `take 11..20` | 已实现前 N 行与一基闭区间 | — |
| 单行 pipeline | `from tasks \| filter id == 1 \| take 1` | 已实现 | — |
| 参数 | `$id` | 未实现 | #10、#22 |
| ADT 派生列 | `derive x = match ...` | 已实现递归 pattern，分支返回 binding 或 typed literal | #35/#36 增加新值构造和通用表达式 |
| 布尔表达式与集合函数 | `and/or/not`、`contains/length` | 未实现 | #36 |
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

value-filter      = "filter" field-path comparison literal
match-filter      = "filter" "match" field-path newline indent match-arm+ dedent
match-arm         = arm-pattern "=>" condition newline?
derive-match      = "derive" identifier "=" nested-match-expression
nested-match-expression = match-expression | newline indent match-expression dedent
match-expression  = "match" field-path newline indent match-value-arm+ dedent
match-value-arm   = arm-pattern "=>" match-value newline?
match-value       = binding-path | literal
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
condition         = bool | binding | binding comparison literal
comparison        = "==" | "!=" | ">" | ">=" | "<" | "<="
field-path        = identifier ("." identifier)*
```

`=`、`limit` 和不带花括号的 `select id,name` 是兼容入口。新文档和格式化输出应使用 `==`、`take` 与 `select {id, name}`。

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

## 值过滤

值过滤比较字段路径和一个完整字面量：

```text
from config
filter endpoint.port >= 8000
select {name, endpoint.host}
```

- `==` 和 `!=` 支持能够按字段类型校验的完整值，包括 record、tuple、sum、option 和 list。
- `>`、`>=`、`<`、`<=` 当前只支持 int、float 和 text。
- Int 使用精确 i64 比较。Float 使用精确数值相等，`-0.0` 与 `0.0` 相等；拒绝 NaN、Infinity 和超出 i64 的整数。
- Float 字段可接受能够精确表示的整数字面量；Int 字段不接受浮点字面量。数字不会自动转换成 text。
- 普通字段路径只能穿过 record。variant 和 option 的内容必须用显式模式处理。

如果查询的第一个 stage 是等值 filter，并且该字段有索引，引擎可以直接读取候选行。索引和扫描共用相同的类型化相等规则；是否存在索引不能改变结果。

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
  Running {attempt, ..} => attempt >= 2
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
- 顶层 `_` 覆盖尚未出现的变体，必须位于最后。没有 `_` 时必须覆盖全部变体。包含嵌套 constructor 的分支只覆盖满足该嵌套模式的值，因此当前需要最后的 `_` 处理其余值；不会把 `Failed {retry_at = Some at, ..}` 误认为覆盖了所有 `Failed`。
- 构造器由被匹配字段的命名类型确定，也可写成 `State.Running`。其他命名 sum 的同名构造器不会混用。
- condition 当前只能是 `true`、`false`、一个 bool 绑定，或者 `binding <op> literal`。绑定只在所属分支内有效。
- 每个顶层 constructor 当前最多出现一次。需要为同一个 constructor 写多个嵌套分支的完整穷尽分析仍由 #35 跟踪；现阶段用一个嵌套分支加最终 `_` 表达优先匹配和兜底。

未知构造器、重复分支、通配分支后的不可达分支、遗漏 constructor、错误负载字段及分支作用域错误会在扫描前返回 `E_MATCH`。非 sum/option 来源或非 bool 条件返回类型错误。

## ADT 派生列

`derive name = match ...` 解构一个 sum/option，并把所有分支归一成一个有静态类型的新字段：

```text
from jobs
derive retry_at =
  match state
    Failed {retry_at, ..} => retry_at
    _ => None
filter retry_at == Some 30
select {id, retry_at}
```

`match` 可以与 `=` 写在同一行，也可像上例多缩进一层。派生字段追加到当前 schema，后续 filter、derive、select 和 sort 都可以引用它；名称与已有字段冲突时拒绝。

当前分支结果可以是一个局部 binding（含嵌套 record 路径）或完整 literal。引擎先从 binding、primitive literal 或限定 constructor 推导一个结果类型，再按该类型检查全部分支；`None`、空 list/record 等不能单独确定类型，但可在其他分支已经给出类型时使用。不同命名类型不会因结构相同而统一。

分支必须穷尽且结果类型一致，这些检查在扫描前完成。pattern 可以递归解构 record、tuple、sum 和 option；当前不能在结果中进行算术/函数调用，也不能用 binding 构造新的 record/sum，例如返回 `Some at` 会等 #36 的统一表达式 IR。为同一顶层 constructor 写多个互补嵌套分支和 prepared plan 的 schema revision 重绑定仍由 #35 后续完成。

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
- `filter match` 与 `derive ... match` 的分支通过缩进进入和退出；退格必须回到已有缩进层级。缩进不能使用 tab。
- 空行与 `#` 注释不结束查询。文件和非交互 stdin 在 EOF 提交完整脚本。
- 括号和集合内允许换行。字符串里的 `|`、逗号和 `#` 都是文本，不参与分隔。
- 同层出现新的 `from`、`type`、`table`、`insert` 或 `create` 时，前一条查询结束并开始新语句。
- REPL 的空行是提交当前完整缓冲区的交互手势，不是文件语法的一部分。

当前不支持任意表达式跨行。源码最多 1 MiB、100,000 tokens 和 64 层类型、值或布局嵌套；超限返回受控错误。

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
| 事件记录 | [events.uid](../examples/events.uid) | sum 完整值比较和字符串中的 `|` | `tests/language.rs::executable_examples` |
| 后台任务队列 | [job_queue.uid](../examples/job_queue.uid) | 嵌套 sum/record/option/list、嵌套 pattern、字段默认值、ADT derive、多键 sort 与范围 take | `tests/language.rs::executable_examples` |
| Pipeline 顺序 | 测试内脚本 | take/filter 顺序与投影作用域 | `stage_order_and_projection_paths_are_preserved` |
| 单行/多行 | 测试内脚本 | 两种 pipeline 布局等价 | `newline_and_inline_pipelines_have_identical_results` |
| 模式检查 | 测试内脚本 | 穷尽性、名义构造器、绑定和错误路径 | `match_filters_*`、`match_is_checked_*`、`match_rejects_*` |
| ADT 派生 | 测试内脚本 | 递归 sum/option/record/tuple pattern、类型统一、空表诊断与后续 stage | `derive_match_*`、`option_and_positional_*`、`nested_patterns_*` |
| 列表分页 | 测试内脚本 | 嵌套多键排序、一基闭区间、兼容语法和空表错误 | `multi_key_sort_*`、`sort_keys_and_take_ranges_*` |

新增语法只有在 parser、执行器、正反测试和本页同步后，才能从“未实现”移动到“已实现”。
