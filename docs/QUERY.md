# 查询语言参考

本页只描述当前可执行语义。[RFC 0005](rfc/0005-structured-prql-query-syntax.md) 的 braced match、group inner pipeline 与 canonical formatter 已实现；多项 derive/select、computed select 与 set 字段集已实现；`@` date/timestamp 与整数 duration unit literal 已实现。

本页描述 **当前版本可以执行** 的查询与 pipeline DML 语法，是查询行为的规范入口。第一次使用可先运行[五分钟教程](GETTING_STARTED.md)中的持久查询、更新和重开链路。类型、表和 insert/upsert 见 [LANGUAGE.md](LANGUAGE.md)，schema 演进见 [MIGRATIONS.md](MIGRATIONS.md)；尚未实现的表达式与 runner 提案见 [DESIGN.md](DESIGN.md)。设计草案中的代码不能当作当前命令执行。

需要从静态查询文件生成客户端绑定时，可先用 `query describe --schema <schema.unid> --file <query.unid>` 离线取得参数、结果和 cardinality 契约，再用 `query rust` 生成可调用代码。长期 migration 的数据库可将 `--schema` 换成 `--db <db.redb>`，保留 live catalog 的稳定 ID 与真实 revision/hash；完整定义见 [RFC 0015](rfc/0015-static-query-contract.md)。这些命令直接复用本页语法的 parser 和 binder，不维护另一套查询推断规则。

unionid 的查询从表开始，按书写顺序经过一组 transform：

```text
from tasks
filter match state {
  Running {attempt, ..} => attempt >= 2 && attempt < 5
  _ => false
}
derive state_label = match state {
  Pending => "pending"
  Running {..} => "running"
  Done {..} => "done"
  Failed {..} => "failed"
}
select {id, title, owner.email, state_label}
sort id
take 20
```

## 能力状态

| 能力 | 当前形式 | 状态 | 后续任务 |
| --- | --- | --- | --- |
| 数据源 | `from table` | 已实现 | — |
| 布尔过滤 | `filter id in $ids` | 已实现括号、`!`/`&&`/`\|\|`、有类型算术、字段间比较、`in`/`not in`、Option 辅助函数与集合谓词 | — |
| sum/option 模式过滤 | `filter match field {...}` | 已实现 braced branches、unit/record/位置负载、递归 record/tuple/sum/option pattern，以及完整嵌套穷尽与不可达检查 | — |
| 投影 | `select {field, nested.field}` | 已实现并可选择普通或 ADT 派生列 | — |
| 排序 | `sort field` / `sort {-priority, created_at, id}` | 已实现单列与多列 | — |
| 截取 | `take 20` / `take 10..20` / `take 10..=20` | 已实现前 N 行与 Rust 风格半开／闭区间 | — |
| 稳定分页 | `page 100` / `page 100 after "u1..."` | 已实现有界 keyset page、正反向 opaque cursor、sequence-pinned 一致性 | #114/#131 |
| 有界关联展开 | `lookup lines from order_lines on order_id == id take 100` | 已实现索引驱动、一致快照、嵌套 typed list 与显式逐行基数上限 | #241 |
| 有界相关 existence | `filter exists { from items ... }` / `filter not exists {...}` | 已实现显式 `outer.path`、索引前置、首行短路、取反与 10,000 driver 上限 | #337/#339 |
| 类型化集合运算 | `union { from archived ... }` / `intersect` / `except` | 已实现精确 schema 检查、完整 typed row 去重、稳定首次出现顺序和共享资源预算 | #341 |
| 单行 pipeline | `from tasks \| filter id == 1 \| take 1` | 已实现 | — |
| 参数 | `$id` / `insert table $row` / `upsert many table $rows` | 已实现 typed AST 绑定、缺失/多余检查、versioned protocol，以及 query/insert/upsert/update/delete 的 schema-aware prepared operation | #22/#89/#91/#97 |
| ADT 派生列 | `derive x = match field {...}` | 已实现递归 pattern、完整嵌套覆盖分析、数值表达式与 option/sum/product/list 值构造，并可在 scalar result 中调用局部函数 | — |
| 布尔表达式与集合函数 | `&&`/`\|\|`/`!`、`contains/length`、`any/all`、`is_some/is_none` | 已实现于 filter、普通／match derive、typed set 和 migration conversion；bytes 支持连续子序列 contains 与 octet length | #100/#138 |
| 其他派生列 | `derive score = priority + bonus` | 已实现 scalar 与 bool expression、typed 参数及后续 stage 作用域 | — |
| 分组与汇总 | `aggregate {...}` / `group {key} { aggregate {...} }` | 已实现 count/sum/min/max、typed 空输入语义与资源上限 | — |
| 查询局部定义 | `let retryable = attempt -> attempt < 3` | 已实现常量、单/多参数非递归纯函数、有限推断、词法遮蔽与展开预算 | — |
| 有限自递归 ADT | `enum Tree { Leaf(text) Branch { children: List<Tree> } }` | 已实现声明、严格值、match coverage、精确索引、持久化与 migration；运行时值仍是有限树 | #81 |
| 执行计划 | `explain from tasks \| filter id == 1` | 已实现 full scan、主键／二级索引 lookup、候选行估计、stage 顺序与结果 schema | — |
| 实际执行剖析 | `explain analyze from tasks \| filter id == 1` | 已实现同快照真实执行、结构化耗时／行数／索引／缓存／批次／内存统计，且不返回业务行 | #295 |
| 更新与删除 | `update table ... set`、`delete table ...` | 已实现 filter/match/sort/take target、typed set、穷尽 match assignment、嵌套 record 路径、typed returning、原子约束与增量持久维护 | #15/#83/#85/#87 |
| Upsert | `upsert table value` | 已实现按主键 insert/完整 row replace、稳定 RowId、结构化 action、typed returning 与增量持久维护 | #15/#85 |
| 批量插入 | `insert many table <list>` | 已实现 literal／参数 row list、逐行默认值和 ADT 检查、整批约束、稳定 RowId／returning 顺序与 memory/redb/TCP 原子提交 | #89 |
| 批量 Upsert | `upsert many table <list>` | 已实现 literal／参数 row list、输入内主键去重、完整 replace、稳定 RowId、逐项 action 与整批原子约束 | #97 |
| schema migration | `migration name` | 已实现显式 ADT schema 操作、typed conversion、全引用路径重写、版本化 runner/ledger 与原子索引维护 | #19 继续补声明式 diff 与更细 plan 报告 |
| join、window、递归查询函数和高阶函数 | — | 延后；自递归数据类型已实现，不包含任意深度 fold/map | #25 |

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
explain           = "explain" "analyze"? query | "explain" "analyze"? newline indent query dedent
pipeline-stage    = newline stage | "|" stage
stage             = local-binding | value-filter | exists-filter | match-filter | derive-expression | derive-match | lookup | aggregate | group-aggregate | set-operation | select | sort | take | page

update            = "update" table update-stage* set-stage+
update-stage      = newline filter-stage | "|" filter-stage
set-stage         = (newline | "|") "set" (set-assignment | "{" set-assignment (separator+ set-assignment)* separator* "}")
set-assignment    = field-path "=" (result-expression | match-expression)
delete            = "delete" table delete-stage*
delete-stage      = newline filter-stage | "|" filter-stage
filter-stage      = value-filter | match-filter
upsert            = "upsert" (table (record-value | parameter) | "many" table (list-value | parameter))

value-filter      = "filter" nested-bool-expression
exists-filter     = "filter" "exists" "{" pipeline "}"
set-operation     = ("union" | "intersect" | "except") "{" pipeline "}"
local-binding     = "let" identifier type? "=" (local-parameters "->")? nested-bool-expression
local-parameters  = binding | "(" local-parameter (separator+ local-parameter)* separator* ")"
local-parameter   = binding type?
match-filter      = "filter" match-predicate
match-predicate   = "match" field-path "{" match-arm (separator+ match-arm)* separator* "}"
match-arm         = arm-pattern "=>" bool-expression
derive-match      = "derive" identifier "=" nested-match-expression
derive-expression = "derive" (derived-field | "{" derived-field (separator+ derived-field)* separator* "}")
derived-field     = identifier "=" (nested-bool-expression | match-expression)
lookup            = "lookup" identifier "from" table "on" field-path "==" field-path "take" positive-integer
aggregate         = "aggregate" "{" aggregate-field (separator+ aggregate-field)* separator* "}"
group-aggregate   = "group" group-fields "{" aggregate "}"
group-fields      = field-path | "{" field-path (separator+ field-path)* separator* "}"
aggregate-field   = identifier "=" ("count" | (("sum" | "min" | "max") scalar-expression)) newline?
nested-match-expression = match-expression
match-expression  = "match" field-path "{" match-value-arm (separator+ match-value-arm)* separator* "}"
match-value-arm   = arm-pattern "=>" result-expression
result-expression = bool-expression | match-value
match-value       = binding-path | literal | constructor-value | record-value | tuple-value | list-value
constructor-value = qualified-variant (record-value | "(" (match-value (separator+ match-value)* separator*)? ")")?
record-value      = "{" value-field (separator+ value-field)* separator* "}"
value-field       = identifier ":" match-value
tuple-value       = "(" match-value separator+ match-value (separator+ match-value)* separator* ")"
list-value        = "[" (match-value (separator+ match-value)* separator*)? "]"
select            = "select" select-field | "select" "{" select-field (separator+ select-field)* separator* "}"
select-field      = field-path | derived-field
sort              = "sort" sort-key | "sort" "{" sort-key (separator+ sort-key)* separator* "}"
sort-key          = "-"? field-path
take              = "take" nonnegative-integer | "take" positive-integer (".." | "..=") positive-integer
page              = "page" positive-integer (("after" | "before") string)?

arm-pattern       = "_" | constructor-pattern
pattern           = "_" | binding | constructor-pattern | record-pattern | tuple-pattern
constructor-pattern = qualified-variant (record-pattern | "(" (pattern ("," pattern)*)? ")")?
qualified-variant = Variant | Type "::" Variant
record-pattern    = "{" (field-pattern ("," field-pattern)* ("," "..")? | "..")? "}"
field-pattern     = identifier (":" pattern)?
tuple-pattern     = "(" pattern "," (pattern ("," pattern)*)? ")"
binding           = lowercase-identifier
nested-bool-expression = bool-expression | newline indent bool-expression dedent
bool-expression   = or-expression
or-expression     = and-expression ("||" and-expression)*
and-expression    = not-expression ("&&" not-expression)*
not-expression    = "!" not-expression | bool-primary
bool-primary      = "(" bool-expression ")" | "{" bool-expression "}" | contains-call | list-predicate | option-predicate | scalar-expression (comparison scalar-expression)?
contains-call     = "contains" scalar-argument scalar-argument
list-predicate    = ("any" | "all") scalar-argument "(" binding "->" bool-expression ")"
option-predicate  = ("is_some" | "is_none") scalar-expression
scalar-expression = additive-expression
additive-expression = multiplicative-expression (("+" | "-") multiplicative-expression)*
multiplicative-expression = unary-expression (("*" | "/") unary-expression)*
unary-expression  = "-" unary-expression | "length" unary-expression | function-call | scalar-primary
function-call     = identifier scalar-argument+
scalar-argument   = scalar-primary | "-" scalar-argument | "length" scalar-argument
scalar-primary    = field-path | literal | "(" scalar-expression ")"
comparison        = "==" | "!=" | ">" | ">=" | "<" | "<="
field-path        = identifier ("." identifier)*
separator         = newline | ","
```

`=`、`limit`、不带花括号的多字段 `select id,name`，以及旧缩进 record/match/group 是持久数据与旧源码的兼容入口。新文档和 formatter 输出使用 `==`、`take`、`select {id, name}`、braced match、braced group inner pipeline、可选的 `::` 限定构造器和 Rust 风格布尔运算符。期望 enum 类型明确时允许省略类型前缀。

复杂条件的规范格式使用表达式块明确边界，并把 `&&` 或 `||` 放在续行开头：

```text
from jobs
filter {
  priority >= 10
  && contains tags "sync"
  && !archived
}
select {id}
```

混用 `&&` 与 `||` 时应加括号直接表达分组，不要求读者仅凭优先级判断：

```text
filter {
  (urgent || priority >= 50)
  && contains tags "sync"
}
```

括号内部可以跨行，因此更深的组合也能保持一个条件一行：

```text
filter {
  urgent
  || (
    priority >= 50
    && !archived
  )
}
```

单个比较或很短的同类组合仍可写在 `filter` 同一行。`{}` 表示跨行表达式边界，`()` 改变优先级；语言不会为了追求“零符号”而隐藏结构。

## 数值表达式

`+`、`-`、`*`、`/` 和一元 `-` 可用于普通 filter、match condition、普通 derive，以及 `derive name = match source {...}` 的分支结果和嵌套构造值。乘除优先于加减；需要改变顺序时使用括号。中缀运算符的规范格式在两侧留空格，复杂算术可在括号内换行：

```text
filter {
  priority
  + bonus * 2
  >= threshold
}

derive next_attempt = match state {
  Queued {attempt, ..} => Some(attempt + 1)
  _ => None
}
```

运算数必须归一为同一个 int 或 float 类型。字段和 binding 决定类型时，同类型的数字 literal 会在扫描前转换；两个不同类型的字段不会隐式混合。命名数值类型保留名义身份，例如 `Attempts + 1` 的结果仍是 `Attempts`。

`int / int` 使用向零截断的整数除法。整数加减乘除和一元负号执行 checked 运算；溢出与除零返回 `E_ARITH`。float 运算拒绝除以正负零，也拒绝产生 NaN 或无限值；负零归一为正零。`&&`/`||` 继续短路求值，因此未执行分支中的算术错误不会触发。

`decimal P S` 只与完全相同的 decimal 类型做 `+`、`-` 和一元负号；literal 可由字段上下文精确补零到目标 scale。每个中间结果与 `sum` 的每一步都检查 P 位范围，超限返回 `E_ARITH` 并回滚请求。乘法、除法、avg、隐式 rescale 和舍入未定义，使用时返回 `E_TYPE`。

## Typed 批量写入

`insert many` 使用已有 list/record 值语法，一次提交多行：

```text
insert many events [
  {id: 1, event: Event::Login {user: "alice"}}
  {id: 2, event: Event::Purchase {item: 42, amount_cents: 1990}}
]
returning {id, event}
```

Rust API 与 version 1 TCP 可传入 `insert many events $rows`；prepared operation 会把 `$rows` 推导为 `List<Event>` 并绑定当前 schema identity。每个输入 record 先按表 row type 递归补默认值和检查命名 ADT，再对“旧 rows + 完整新批次”统一检查主键并重建派生索引。输入顺序决定新 RowId 和 returning 行顺序；空 list 返回 `affected_rows = 0`，有 returning 时仍返回稳定 columns。

`upsert many events <list>` 使用同一完整 row list，并要求表声明主键。每项按输入顺序更新已有主键或插入新主键；更新保留 RowId，新行按该顺序分配 RowId。输入 list 内的主键必须互不重复，否则返回 `E_CONSTRAINT`。成功响应的 `upsert_actions` 与输入和 returning 行逐项对齐，每项为 `inserted` 或 `updated`。

单批最多 100,000 行，并继续受 1 MiB source／TCP frame、16 MiB 单值 codec、8 MiB returning 与请求 deadline 约束。字段、类型、批内／最终主键或 unique index 冲突、预算、deadline 或 redb commit 失败时，候选数据库不会发布，因此没有部分 rows、indexes 或 RowId 游标缺口。prepared 写入只支持 memory/redb；过渡 WAL 无法重放绑定后的参数值并返回 `E_CONFIG`。当前没有流式导入或跨请求事务。

Rust `prepare` 也可在相同 schema identity 下绑定单行 `insert table $row`、`upsert table $row` 和下文的 update/delete。完整 row 参数显示为表的命名类型；filter、set 和 match 结果参数从字段位置推导。prepare 不扫描或修改行，但会检查表与主键要求、target stage、字段、constructor、穷尽性和 returning，所以空表不会延迟 DML 错误。执行继续使用 `execute_prepared` 或带 deadline 的 `execute_prepared_until`，并通过同一候选事务提交。

## Pipeline 更新与删除

`update`/`delete` 以目标表开头，并复用查询的普通 filter 与 braced match filter：

```text
update jobs
filter match state {
  Queued {..} => true
  _ => false
}
sort {-priority, scheduled_at, id}
take 1
set {
  attempts = attempts + 1
  state = match state {
    Queued {attempt, ..} => Running {worker: "local", attempt: attempt + 1}
    current => current
  }
}
returning {id, state}

delete jobs | filter archived == true | returning
```

不写 filter 时从整表开始选择。mutation target 接受普通 filter、braced match filter、sort 和 take，并严格按源码顺序执行；sort 并列保持稳定 RowId 输入顺序，生产语句仍应以唯一键结束排序。`take n`、半开区间 `take start..end` 与闭区间 `take start..=end` 和读取 pipeline 一致。update 的全部 target stage 必须位于第一个 set 之前；derive/select/group/aggregate 不属于 mutation target。单行形式使用 `|`，例如 `update jobs | filter id == 1 | take 1 | set attempts = attempts + 1`。复杂选择和多项 set 推荐逐行写。

多字段更新规范写成 `set {left = right, right = left}`，每个右侧读取旧行，可安全交换字段。单项 `set left = right` 保持可用；旧的重复 `set` 也继续执行，formatter 将其合并为一个字段集。空字段集、缺少逗号和重复路径会提供源码诊断；重复检查跨越同一 update 的所有 set。

set 的字段路径按表的完整 row schema 绑定。右侧可使用字段引用、literal、ADT constructor、`length`、int/float 算术和完整 bool 表达式，也可写 `match source {...}`，复用 ADT derive 的 pattern、coverage、bool 结果和 option/sum/product/list 值构造。assignment 目标给出结果类型，bool 表达式只能写入 bool 字段；每个分支在扫描前检查，因此空表仍会拒绝未知路径、非穷尽／不可达 pattern、错误 constructor 和类型不匹配。

match assignment 的顶层小写 binding 是不可反驳 pattern，必须放在最后。`current => current` 同时绑定并返回完整源值，适合只转换部分 constructor；`_` 仍可用于不需要原值的最终分支。结果可引用该分支的嵌套 binding 与 typed 参数。嵌套更新路径只穿过 record；要修改 sum/option/list 内部内容，应匹配并构造完整目标值。

同一 update 的多个 set 同时求值，右侧全部读取修改前的行。父路径与子路径不能同时赋值，例如 `set owner = {...}` 与 `set owner.email = ...` 会返回 `E_QUERY`。每行形成完整候选 record 后重新类型检查，全部候选形成后检查主键与 unique indexes，再一起替换 rows 和 indexes。unique index 使用完整 typed ADT 相等语义，`None` 也是一个受唯一约束的值。任何 filter、算术、类型或约束错误都会由 Engine 丢弃整个请求的候选状态；redb 模式在同一事务提交。

末尾可写 `returning` 返回完整受影响行，或写 `returning {id, state}` 返回有序字段路径投影。单行／批量 insert 与 upsert 返回默认值补齐后的新行，update 返回后像，delete 返回前像；空批次或未匹配 update/delete 仍提供投影 columns、空 rows 和 `affected_rows = 0`。批量 insert 遵循输入顺序，update/delete 遵循 mutation target 选择顺序；没有 sort 时即稳定 RowId 顺序。字段绑定、100,000 行上限和 8 MiB typed wire rows 预算均在候选状态提交前检查，失败不发布修改。

insert/upsert/update/delete 成功时响应包含 `affected_rows`；批量写入返回输入行数。单行 upsert 返回结构化 `upsert_action: inserted|updated`，批量 upsert 返回同输入顺序的 `upsert_actions`。upsert 输入是按 schema 默认值补齐的完整 row：命中主键时替换整个值并保留 RowId，未命中时分配新 RowId。局部修改仍使用 update，删除后的 RowId 不复用。当前执行器会重建受影响表的内存索引；redb 在请求提交时只删除或写入前后状态中变化的 catalog/row/index 稳定键。

## 执行模型

Pipeline 严格从左到右执行。每个 stage 接收前一个 stage 的行和 schema，再产生下一个 stage 的输入。优化不能跨越会改变结果的边界，例如：

```text
from tasks | take 1 | filter id > 1
```

与下面的查询语义不同：

```text
from tasks | filter id > 1 | take 1
```

引擎在读取任何行之前，按 stage 顺序检查整条 pipeline。空表上的未知字段、类型错误、非穷尽 match 和分支结果类型冲突仍然报错。`derive` 将新增或替换字段加入后续 schema；`select` 会改变后续 schema，因此投影掉的字段不能再用于 filter、match、derive 或 sort。

| Stage | 输出 schema | 行与顺序语义 | 空输入 |
| --- | --- | --- | --- |
| `from` | 表的完整行类型 | 读取表；未排序行序不构成承诺 | 返回带完整 schema 的空结果 |
| `let` | 不变 | 注册供后续 stage 展开的局部表达式或纯函数，不读取或改变行 | 仍检查可确定的引用、类型、递归和预算 |
| `filter` | 不变 | 只保留条件为真的行 | 仍执行字段与类型检查 |
| `filter match ... {...}` | 不变 | 每行按其 sum/option constructor 执行唯一分支的条件 | 仍执行模式绑定和穷尽检查 |
| `derive` | 追加或替换有静态类型的字段 | 每行求值一次 scalar 或 bool expression，行数与顺序不变 | 仍推导结果类型并检查完整表达式 |
| `derive name = match source {...}` | 追加或替换有静态类型的字段 | 每行执行唯一分支，行数与顺序不变 | 仍统一分支结果类型 |
| `lookup` | 追加目标表 row type 的 `list` 字段 | 按索引为每个 driver row 读取有界关联行；driver 行数与顺序不变 | 仍检查表、字段、索引、类型和上限 |
| `filter exists` / `filter not exists` | 不变 | 分别保留内层目标表有匹配或无匹配的 driver row | 仍检查目标表、`outer` 路径、相关键类型、索引和 stage 边界；plan 用 `negated` 区分 |
| `aggregate` | 只保留 aggregate 输出 | 未分组时把全部输入行归约为一行 | count/sum 为类型化零；min/max 为 None |
| `group ... { aggregate {...} }` | group key 后接 aggregate 输出 | 按完整 typed equality 分组；无 sort 时组顺序不承诺 | 返回零行但仍检查 key、输入和输出类型 |
| `select` | 按书写顺序组成新 schema | 每行只保留选择的字段 | 返回带投影 schema 的空结果 |
| `sort` | 不变 | 单列或多列词典序；全部键相同的次序不承诺 | 返回空结果但仍检查全部键 |
| `take` | 不变 | 保留前 N 行，或一基 Rust 风格半开／闭区间内的行；无 sort 时位置不稳定 | 返回空结果但仍检查范围 |
| `page` | 不变 | 按唯一 sort tuple 返回有界页和 opaque cursor；之后只允许 lookup/select | 返回空页且不产生新 cursor |

最终响应的 `columns` 来自最后一个 stage 的 schema，并保持 `select` 的字段顺序。嵌套字段的结果列名保留完整路径，例如 `owner.email`。

## Explain、实际剖析与类型化索引计划

`explain` 在相同的 schema、字段、pattern、局部函数和参数绑定规则下准备查询，但不读取、复制或执行数据行：

```text
explain from tasks | filter id == 1 | select {id, state}
```

复杂查询也可缩进：

```text
explain
  from tasks
  let target int = 1
  filter id == target
  select {id, state}
```

需要测量真实执行时，在 `explain` 后增加 `analyze`：

```text
explain analyze
  from tasks
  filter id == 1
  select {id, state}
```

`explain analyze` 在同一个 immutable read snapshot 上生成 `plan` 并走普通查询执行器。响应仍不包含业务 `rows`、结果 `columns`、page cursor、参数值或 literal；额外的 `analysis` 包含：

- `execution_micros`：完成一次 pipeline 执行的 wall-clock 微秒数，不包含请求解析和参数绑定。
- `returned_rows`：被分析的 pipeline 实际产生、随后丢弃的结果行数。
- `rows_examined`：为表达式求值加载的行数，等于本次解码行加 durable row cache 命中行；`rows_decoded` 单独表示本次真正解码的行数。
- `index_entries_examined`、`row_cache_hits`、`row_cache_misses`、`batches` 与 `working_peak_bytes`：普通执行器在这次运行中记录的其他无值计数。

取消、deadline、行数和工作内存上限与普通查询完全相同；执行失败时直接返回对应错误，不返回不完整的分析对象。计数描述 unionid 执行器可观察到的逻辑工作量，不代表物理磁盘读取或分配器精确 RSS。重复测量可比较计划与工作量，wall-clock 时间仍会受机器负载和缓存状态影响。

成功响应的 `plan` 是结构化值，包含源表、访问方式、索引 shape、equality prefix、可选 range 字段与上下界 inclusivity、遍历方向、sort/page 覆盖状态、当前快照的候选行估计、源表行数、保持源码顺序的 stage 列表、关联读 `lookups`、相关存在过滤 `exists`、集合运算 `set_operations`，以及最终结果 schema。每个 lookup 计划公开 stage、输出字段、目标表/字段、driver 字段、实际目标索引和逐行上限；每个 exists 计划公开目标表、相关路径、实际索引和 driver 上限；每个 set-operation 计划公开 operator、右侧表与右侧访问计划。主访问方式为 `full_scan`、`primary_key_lookup`、`secondary_index_lookup`、`composite_lookup`、`range_scan`、`ordered_scan` 或 `page_seek`。CLI 会把这些字段打印成可读计划；JSON、Rust API 与 version 1 TCP 响应保留同一结构。prepared query 同样支持 explain，参数在选择索引前按静态类型绑定。计划只显示 `<bound>`／`<range>`，不暴露 literal、parameter、cursor boundary 或数据行。

planner 跳过开头的 row-independent `let`，然后只从连续的简单 `field op bound` filter 提取边界；`op` 可以是 `==`、`>`、`>=`、`<` 或 `<=`。复合索引使用从首 component 开始的连续 equality prefix，并在紧邻的下一个 component 上合并至多一组 lower/upper range；range 后或 key gap 后的条件仍作为 residual filter。遇到 compound bool、`filter match`、derive、select、aggregate/group、sort、take 或其他语义边界后停止抽取，也不从 `&&`／`||` 内部拆条件。

去掉 equality-fixed sort 字段后，剩余 sort 只有与索引 suffix 的连续前缀方向完全相同或全部相反时才消费 index order。多个候选依次按 equality component 数、是否有 range、是否满足 sort/page、key component 数和 stable index ID 选择。原 filter 始终按源码顺序再次执行；如果后续 stage 会改变行集或顺序，执行器不会提前按 `take` 截断。

当前可执行版本支持 `create index tasks (status, -priority, id)` 的 inline/multiline 语法、方向化 schema identity、复合 unique 约束、memory/redb typed tuple key、migration/diff、storage format 5 与 backup 4。符合前缀规则的 equality、range 和 sort 会直接读取有界 tuple span；`take` 在所有前置 residual filter 通过后计数。稳定分页可以用主键收尾，或用 equality-fixed fields 加完整 unique-index visible suffix 证明唯一，并把 typed cursor boundary 下推为严格的 forward/backward seek。

所有可存储的静态类型都使用与 `cmp_eq` 相同的稳定结构键，包括命名类型、record、tuple、sum、option 和 list。Option 的 `None` 与 sum 的不同 constructor 有不同键，不会按 null 或缺失值混合。索引从 row 派生，insert/update/delete、redb 恢复和 migration 后都会维护或重建；是否存在索引不能改变查询结果。

`estimated_rows` 是当前快照中 B-tree span 的 entry 数：full scan 等于表行数，lookup 等于 posting 长度，range/page seek 等于应用 typed boundary 后的 span。它不扣除 residual filter，也不是基于统计信息的长期基数预测。执行器对可安全下推的 `take/page` 按索引方向逐项读取，只保留足够的通过行；普通 `explain` 自身不读取或执行数据行，只有显式 `explain analyze` 会执行。

## 查询局部 let 与纯函数

`let` 给重复表达式命名。没有参数时定义表达式别名；一个参数可直接写在箭头前，多个参数用括号和逗号明确边界：

```text
from jobs
let max_attempts = 3
let visible = !archived
let retryable = attempt -> attempt < max_attempts
let add = (value, bonus) -> value + bonus
derive can_retry = retryable attempts
derive score = add priority 2
filter can_retry && visible
```

调用使用空格应用。调用本身作为另一个调用的参数时加括号，例如 `twice (add_one attempts)`；负的复合参数同样写成 `adjust (-attempts)`，让减法与参数边界保持明确。`contains`、`any/all` 等前缀形式收到函数调用结果时写成 `contains (normalize tags) "sync"`。函数只能接收和返回数据值，不产生可存储、可传输或可返回的函数值。

引擎优先从调用参数的字段类型推断每个参数，并在同一局部定义的后续调用中保持该类型。无法从 `None`、空 list/record 等值确定类型时，使用与字段声明一致的低标点注解：

```text
let missing: Option<int> = None
let present = (value: Option<int>) -> is_some value
let no_value: Option<int> = value -> None
```

局部名称后的类型是结果类型；括号中参数名称后的类型是参数类型。单个无注解参数写成 `value ->`，多参数即使都无注解也写成 `(value, lower, upper) ->`。显式类型只接受 primitive、已声明的命名类型及 option/list/tuple 组合。类型仍无法确定或同一函数收到不兼容类型时返回 `E_TYPE`。

作用域从 let 所在位置延续到当前 pipeline 结束。定义只能调用更早出现的函数，因此前向引用和直接递归会在扫描前返回 `E_QUERY`，间接调用环也无法形成。后续同名 let 会遮蔽旧定义，但旧函数保留定义时捕获的词法环境；函数参数和 `any/all` 元素 binding 遮蔽同名外层局部值。局部表达式按使用位置展开，不是隐藏的 derive 列；如果中间 select 或 aggregate 移除了它引用的字段，之后使用仍会得到字段错误。

let 可用于普通 filter/derive、match condition、ADT derive 的 scalar result，以及 sum/min/max 输入。所有定义只包含现有纯 expression IR，没有写入、文件、网络、时钟、随机或全局状态入口。每条 pipeline 最多 256 个 local binding、32 层函数调用和 100,000 个展开步骤；超限返回 `E_LIMIT`。调用错误定位到调用 token，定义期错误至少定位到对应 let；执行前展开完成，因此运行时不存在动态函数分派。

## 布尔过滤表达式

普通 `filter` 接受一个静态类型为 bool 的表达式：

```text
from jobs
filter {
  !archived
  && priority >= threshold
}
filter {
  contains tags "sync"
  || length tags == 0
}
select {id, priority}
```

按一组 ID、状态或完整 ADT 值筛选时，使用比较级的 `in`／`not in`：

```text
from jobs
filter id in $ids
filter state not in [Done, Failed {retryable: false}]
select {id, state}
```

- 比较的两侧可以是字段路径、literal 或 `length`。因此支持字段与字段、字段与 literal，以及 `length tags >= minimum_tags`；至少一侧必须能确定类型。
- `==` 和 `!=` 支持类型一致的完整值，包括 record、tuple、sum、option 和 list。literal 会按另一侧的类型检查，因此 `contains [] 1` 是类型明确且恒为 false 的合法表达式。
- `item in collection` 与 `item not in collection` 要求右侧是 `List<T>`，并按完整 typed equality 从左到右短路。列表字面量、字段和 prepared list 参数都可作为右侧；空列表分别恒为 false／true。元素和列表类型在扫描前统一，`id in $ids` 会把 `$ids` 推导为 `List<int>`，不会做数字／文本或命名 ADT 的隐式转换。
- `>`、`>=`、`<`、`<=` 当前只支持 int、float 和 text。
- Int 使用精确 i64 比较。Float 使用精确数值相等，`-0.0` 与 `0.0` 相等；拒绝 NaN、Infinity 和超出 i64 的整数。
- Float 字段可接受能够精确表示的整数字面量；Int 字段不接受浮点字面量。数字不会自动转换成 text。
- `contains collection item` 只接受 list，并按 list 元素的完整类型化相等语义判断；元素可以是命名 ADT。`length` 接受 list 或 text，分别返回元素数或 Unicode scalar 数。
- `any items (item -> condition)` 与 `all items (item -> condition)` 对 list 元素建立有类型的词法绑定。绑定可访问 record 字段，predicate 也可引用外层行字段或 match binding，并可继续嵌套 `any/all`。局部绑定遮蔽同名外层字段。
- `in`、`not in` 和 `any/all` 从左到右执行并短路。空 list 上 `in`/`any` 为 false，`not in`/`all` 为 true。每条 pipeline 或 DML target 最多执行 100,000 次 list 元素比较／predicate，组合与嵌套调用共享预算，超限返回 `E_LIMIT`。
- `is_some value` 与 `is_none value` 只接受静态类型为 `Option<T>` 的值。二者不提取 payload；需要读取 payload 时仍使用显式 match。孤立的 `None` 没有元素类型，必须从字段、binding 或 typed 参数获得类型。
- bool 字段可以直接作为条件。其他类型不隐式转换为 bool；option 也不提供 truthiness，必须用 `is_some/is_none` 或显式 match。
- 优先级从高到低为括号／比较／函数、`!`、`&&`、`||`。混用 `&&` 与 `||` 的规范源码使用括号明确分组。逻辑运算在运行时从左到右短路；两侧仍会在扫描前完成类型检查，短路不会隐藏未知字段或类型错误。
- 普通字段路径只能穿过 record。variant 和 option 的内容必须用显式模式处理。

如果查询的第一个有效数据 stage 是单纯的有索引等值条件，引擎会按上述类型化计划直接读取候选行；前置 let 不阻止 lookup。复合布尔表达式暂时扫描候选表。具体选择可用 `explain` 检查。

## 模式过滤

`match` 检查一个 sum 或 option 字段，并在当前 constructor 的负载中建立局部绑定：

```text
from tasks
filter match state {
  Pending => false
  Running {worker, attempt} => worker == "local"
  Done {result} => result == "ok"
  Failed {retryable, ..} => retryable
}
select {id, title}
```

花括号明确 branch set 的起止位置，多行分支由换行分隔。后续 stage 在右花括号后继续：

```text
from tasks
filter match state {
  Running {attempt, ..} => attempt >= 2 && attempt < 5
  _ => false
}
select {id}
take 1
```

当前规则如下：

- unit 变体直接写 `Pending` 或限定形式 `State::Pending`。match scrutinee 已确定 enum 类型，因此带 record 负载通常简写为 `Running {worker, attempt}`；也可写完整的 `State::Running {...}`。位置负载写 `Pair(left, right)`。
- option 使用 `None` 与 `Some(value)`；写成 `Some(_)` 可以只判断存在而忽略内容。
- record 字段名默认也是局部绑定；`{retry_at: at, ..}` 把字段重命名为 `at`。冒号右侧也可递归使用 constructor、record 或 tuple pattern，例如 `{error: Network {message}, retry_at: Some(at)}`。
- tuple 用自身的积类型标点分解，例如 `Some((at, reason))`。一个 constructor 携带多个位置参数时写 `Outer(Some(value), other)`；单个 tuple payload 通过双层括号区分。
- `{attempt, ..}` 绑定 `attempt` 并显式忽略其他字段。不写 `..` 时必须列出该负载的全部字段，避免 schema 新增字段后被静默忽略。
- 顶层 `_` 覆盖尚未出现的值，必须位于最后。顶层小写 binding 同样不可反驳并必须位于最后，但会把完整源值绑定到该名称；`current => current` 可在 derive 或 update assignment 中保留其他 constructor。没有不可反驳分支时，多个同名顶层 constructor 分支可以用互补的嵌套 pattern 覆盖完整值域。例如 `Failed {retry_at: Some(at), ..}` 与 `Failed {retry_at: None, ..}` 可以共同覆盖 `Failed`；只写其中一个仍然是非穷尽 match。
- 构造器由被匹配字段的命名类型确定，因此可省略类型前缀；也可写成 `State::Running`。其他命名 sum 的同名构造器不会混用。
- condition 与普通 filter 共用布尔表达式 binder 和 evaluator，支持绑定间比较、括号、`!`/`&&`/`||`、`contains/length`、`any/all` 与 `is_some/is_none`。
- 分支按源码顺序选择第一个匹配项。检查器使用有预算的 pattern matrix 分析 sum、option、record 与 tuple 的组合关系，允许可到达的重叠分支，拒绝被先前分支完全覆盖的分支。非穷尽错误会同时列出仍未完全覆盖的顶层 constructor，并给出一个具体嵌套值样例。覆盖分析最多执行 100,000 步，超限返回 `E_LIMIT`。

未知构造器、重复分支、通配分支后的不可达分支、遗漏 constructor、错误负载字段及分支作用域错误会在扫描前返回 `E_MATCH`。非 sum/option 来源或非 bool 条件返回类型错误。

## 普通派生列

`derive name = expression` 直接从当前行计算一个有静态类型的新字段，不要求人为添加 match：

```text
from jobs
derive score = priority + bonus * 2
derive needs_retry = {
  attempts < max_attempts
  && !archived
}
derive has_failure = any history (attempt -> is_some attempt.error)
filter needs_retry && has_failure
select {id, score, needs_retry}
```

只有单个 scalar expression 时，结果保留它的类型；例如复制 `history` 仍得到 `List<Attempt>`，命名的 `Score + 1` 仍得到 `Score`。比较、`!`/`&&`/`||`、`contains`、`any/all` 与 `is_some/is_none` 产生 bool。`$name` 参数从字段或运算数推导类型；孤立的 `$value`、`None`、空 list 或空 record 没有足够类型信息，会在扫描前返回 `E_TYPE`。

新列追加到当前 schema，后续 filter、derive、select 和 sort 可以直接引用。同名派生会替换已有列并保留其列位置，可改变查询结果类型；数据库 schema 与原始行不受影响。一个字段集内不允许重复输出名；`select` 已移除的字段也不能再引用。每行只读取求值开始时的当前行，表达式没有写入或其他副作用。布尔短路、算术错误和 `any/all` 共享预算都沿用 filter 语义。

## 字段集与计算式 select

`derive { ... }` 按从上到下的顺序求值；后项可读取前项的新值。同名字段替换当前查询列，列位置不变，新列追加到末尾。每个字段集内的重复输出名返回 `E_QUERY`；不同 derive stage 可以逐次替换同名列。

```text
from jobs
derive {
  score = priority + bonus,
  urgent = score >= 10,
}
select {
  id,
  display_score = score * 2,
  label = match state {
    Queued {..} => "waiting",
    current => "active",
  },
}
```

计算式 `select` 的赋值项按顺序展开为 derive，最后按所列路径投影，响应列顺序保持书写顺序。别名是简单标识符，普通投影可用嵌套路径。想保留覆盖前的值，先写 `original = score`，再写 `score = score + 1`；普通路径投影读取全部赋值完成后的行。此语义与 `set {}` 所有右侧读取旧行不同。单项 `select display = title` 也可用。类型推断、ADT 名义身份、prepared 参数、短路与算术错误均复用原有表达式规则。

formatter 将名字不重复的相邻 derive 合并成字段集；若紧接的 select 包含这些派生名且顺序一致，则输出计算式 select。其他顺序保留 derive 加投影，避免改变求值顺序或丢弃可能报错的表达式。`explain` 显示展开后的 derive/projection stages。

使用 `page` 时，计算式 select 必须放在最终 sort 之前；sort 与 page 之间仍只允许纯字段投影。覆盖过源表主键的查询返回 `E_PAGE_ORDER`，因为覆盖后的同名列不再保证唯一。此检查也适用于单项 derive 和 ADT match 派生。

## ADT 派生列

`derive name = match ...` 解构一个 sum/option，并把所有分支归一成一个有静态类型的新字段。分支可以直接从 binding 构造新值：

```text
from jobs
derive retry_at = match state {
  Failed {retry_at: Some(at), ..} => Some(at)
  _ => None
}
filter retry_at == Some 30
select {id, retry_at}
```

`match` 的 branch set 使用花括号，多行分支由换行分隔，派生字段追加到当前 schema，后续 filter、derive、select 和 sort 都可以引用它；名称与已有字段冲突时拒绝。旧缩进分支仍是兼容输入。

当前分支结果可以是局部 binding（含嵌套 record 路径）、literal，或递归的 constructor/record/tuple/list 值。位置 payload 使用括号，例如 `Some(at)`、`Display::Retrying(Summary {label: message})`。命名 record 可写成 `Summary {label: message}`，省略字段会使用其 schema 默认值；缺少必填字段和未知字段仍报错。

引擎先从 binding、primitive literal、`Some(value)`、结构化 product 或限定 constructor 推导结果类型，再按该类型检查全部分支。`None`、空 list/record 和未限定的普通 sum constructor 不能单独确定类型，但可在其他分支已经给出类型时使用；需要主动确定命名 sum 时写 `Type::Variant`，命名 record 写 `TypeName {...}`。不同命名类型不会因结构相同而统一。

分支必须穷尽且结果类型一致，constructor 归属、参数数量、嵌套覆盖关系和每个值都在扫描前检查。pattern 可以递归解构 record、tuple、sum 和 option，同一个顶层 constructor 可以由多个互补嵌套分支覆盖。派生结果支持 scalar arithmetic、完整 bool/Option/list predicate 和查询局部纯函数；短路与集合求值预算沿用普通 derive。复杂结果可在 `=>` 后换行缩进。prepared query 记录 schema revision/hash，schema 改变后以 `E_SCHEMA_CHANGED` 拒绝旧 plan。

## 分组与基础汇总

未分组汇总使用 braced field set 命名输出列：

```text
from events
filter received_at >= $since
aggregate {
  events = count
  amount = sum amount_cents
  earliest = min received_at
  latest = max received_at
}
```

分组时把 aggregate 放进 group 的 braced inner pipeline。单个 key 可直接写字段路径；多个 key 使用内层花括号明确 key set：

```text
from events
derive day = received_at / 86400
group {source, day} {
  aggregate {
    events = count
    amount = sum amount_cents
  }
}
filter events >= 10
sort {source, day}
take 20
```

当前签名和空输入语义：

| 函数 | 输入 | 输出 | 未分组空输入 |
| --- | --- | --- | --- |
| `count` | 不接参数，统计输入行 | `int` | `0` |
| `sum expression` | `int` / `float`，包括命名数值类型 | 与输入相同 | 同类型的零 |
| `min expression` | `int` / `float` / `text`，包括相应命名类型 | `Option<T>` | `None` |
| `max expression` | `int` / `float` / `text`，包括相应命名类型 | `Option<T>` | `None` |

`sum` 的 int 使用 checked addition，溢出返回 `E_ARITH`；float 每一步都必须保持有限，正负零最终归一为正零。min/max 接受所有具有静态类型的有限 ADT，并与 filter、sort 和 cursor boundary 共用 total order；它们用 Option 明确表达空输入，不引入 null 或三值逻辑。分组空输入没有 group，因此返回零行。

aggregate 输入是 scalar expression，可以引用字段路径、前一 stage 的普通或 ADT 派生列，以及查询局部纯函数。group key 使用完整 typed equality，可包含 record、tuple、sum、option 或 list；输出中保留原静态类型。未显式 sort 时不承诺 group 行顺序。group inner pipeline 当前必须包含一个 aggregate，aggregate 后的 filter/select/sort/take 针对汇总后的 schema 执行。

每条 aggregate 最多声明 256 个输出、产生 100,000 个 group 和 1,000,000 个 accumulator cell，估算 group state 上限为 64 MiB；同时仍受 250,000 输入工作行、100,000 结果行与服务 deadline 限制。任一边界超限返回 `E_LIMIT`。distinct aggregate、用户定义 aggregate、window 和 join 不进入 v0.2。

## 投影、排序与截取

`select` 接受一个或多个字段路径，拒绝重复或未知字段。输出列顺序就是选择顺序：

```text
from tasks
select {title, id, owner.email}
```

`sort field` 升序排列，`sort -field` 降序排列。多个排序键写成 `sort {key, -descending_key, final_key}`，按书写顺序做词典序比较。键可以是嵌套 record 路径；primitive、命名类型、sum、record、tuple、option、list 与有限递归 ADT 使用同一类型化 total order。sum 先比较稳定 variant ID 再比较 payload，record 按稳定 field ID，`None < Some`，list 使用短前缀优先的词典序。重复键在解析时拒绝，未知键即使在空表上也会报错。

全部排序键相同时，引擎不承诺原行顺序。分页或队列查询需要可复现顺序时，应把 int/text 主键作为最后一个键：

```text
from jobs
sort {-priority, created_at, id}
page 100
```

`take N` 接受非负整数并保留前 N 行，`take 0` 返回空行但仍保留当前结果 schema。范围使用一基位置和 Rust 端点规则：`take 11..21` 是半开区间，返回第 11 到 20 行；`take 11..=20` 是等价的闭区间。尾部越界返回剩余行。start 必须至少为 1，end 不能小于 start；范围总是相对于该 stage 收到的当前结果。

### 有界 lookup 关联展开

`lookup` 把目标表中匹配的完整 typed row 装进当前行的 list 字段，适合订单/明细、任务/事件等小型一对多读取：

```text
from orders
sort id
page 100
lookup lines from order_lines on order_id == id take 100
select {id, customer, lines}
```

`on` 左侧 `order_id` 属于目标表 `order_lines`，右侧 `id` 属于当前 pipeline schema。两侧必须是同一个静态类型，目标字段必须是主键、二级索引或复合索引的第一项。`lines` 不能覆盖现有字段，其类型为 `List<Line>`；匿名 record 表则得到对应匿名 record list。每个 driver row 保留一次，目标无匹配时是 `[]`，不会产生 null 或扁平重复行。每个目标 RowId 在 list 中最多出现一次，相同 value 的不同行不会被去重。首版 list 使用目标索引的稳定 traversal 顺序：单字段索引按 RowId，复合索引按剩余 component 后接 RowId；当前语法不接受目标侧自定义 sort。

末尾 `take` 是逐个 driver row 的强制基数上限，取值 1..=1000。执行器最多读取 `take + 1` 个匹配；多出一行就返回 `E_RELATION_LIMIT`，不静默丢数据。一个 lookup stage 最多接收 10,000 个 driver rows，并继续受 working bytes、结果行、deadline 与相同 committed snapshot 约束。多个 lookup 可按顺序嵌套组合；如果前一层 list 还要做任意深度遍历，应在应用的 typed ADT 上处理。

稳定分页时 lookup 必须放在 page 后面，让引擎先取出有界 driver page，再读取关联；page 后只允许 lookup 和 select。非分页查询应先用 indexed filter 或 take 限制 driver。`explain` 的 `lookups` 可确认目标索引和逐行预算。lookup 不能用于 update/delete target，也不是 SQL 的 inner/left/right/full join。

### 有界相关 exists 与 not exists

`filter exists` 处理“只保留至少有一个匹配子项的任务”一类查询，不把子项装进结果：

```text
from tasks
filter exists {
  from task_items
  filter task_id == outer.id
  filter state != Done
}
sort id
```

内层普通字段属于 `task_items`；`outer.id` 明确引用进入 exists stage 时的任务。至少一个内层 filter 必须包含类型一致的 `target.path == outer.path`，目标路径必须是主键或二级索引的首个 component。相关条件可以与其他纯 filter 组合，执行器始终把选中的相关等值条件下推到索引，并在第一条通过 residual filters 的目标行处停止。

使用同一边界的 `filter not exists { ... }` 可以保留没有匹配子项的 driver row，包括完全没有子项，以及存在子项但都未通过 residual filters 的情况。它不是任意 bool expression 中的取反，而是语义明确的 anti-existence stage：

```text
from tasks
filter not exists {
  from task_items
  filter task_id == outer.id
  filter state != Done
}
sort id
```

一个 exists/not-exists stage 最多接收 10,000 个 driver rows，沿用同一 committed snapshot、deadline、取消、decode 与 working-memory 限制。首版内层只允许 filter；不支持嵌套 exists、derive、lookup、aggregate、sort、take、page、select，也不支持 mutation target。多个普通 filter 和 existence stage 可按 pipeline 顺序表达 conjunction。

`explain` 的 `exists` 数组公开 stage、`negated`、目标表、`target`/`outer` 相关路径、实际索引和 driver limit；普通 explain 只绑定和规划，不执行目标查询，也不显示外层字段值。完整契约见 [RFC 0018](rfc/0018-bounded-correlated-exists.md)。

### 类型化集合运算

`union`、`intersect` 和 `except` 用花括号界定独立的右侧 pipeline。两侧在集合 stage 处必须具有完全相同的字段名、字段顺序和字段类型；命名 struct/enum 比较 stable type ID，不做 int/float、匿名/named record 或不同命名 ADT 之间的隐式提升：

```text
from active_tasks
select {state, tags}
union {
  from archived_tasks
  filter state != Done
  select {state, tags}
}
sort state
```

三种运算都以完整 typed row 为单位去重，包括 enum variant/payload、record、tuple、Option 和 list。`union` 先输出左侧第一次出现的值，再输出右侧尚未出现的值；`intersect` 按左侧首次出现顺序保留也存在于右侧的值；`except` 按左侧首次出现顺序保留右侧不存在的值。该顺序只定义集合 stage 自身的确定行为；调用方需要业务排序时仍应在其后显式 `sort`。

首版右侧可以使用普通 read pipeline stage，但不允许嵌套集合运算。任一侧都不能使用 cursor `page`，因为跨来源结果不属于单表 sequence-pinned traversal；可在两侧先 `filter`/`take`，集合运算后再 `sort`/`take`。左右 materialized rows、typed membership 索引和 encoded bytes 合并计入 working-state 上限，并沿用 deadline、取消与最终结果上限。

`explain` 的 `set_operations` 数组公开 stage、operator、右侧源表与右侧访问计划，且不会读取任一侧的数据行。完整边界见 [RFC 0019](rfc/0019-bounded-typed-set-operations.md)。

### 有界 keyset page

`page N` 开始正向遍历，N 必须在 1..=1000。查询必须有显式 `sort`，最后一个排序键必须是源表主键；绑定器根据 schema 证明完整 tuple 唯一，不根据当前样本数据猜测。最终 sort 与 page 之间只允许 `select`，page 后只允许上述有界 lookup 和 `select`，因此可以投影掉排序字段而不把主键暴露给客户端。page 不与 `take`、`aggregate`、`group` 或 mutation target 混用。

成功响应的 `page` 包含 `limit`、`direction`、十进制 string `snapshot_sequence`、`has_more` 以及可用的 `next_cursor`／`previous_cursor`。继续向后读取时使用：

```text
from jobs
filter archived == false
sort {-priority, id}
page 100 after "u1.payload.mac"
```

返回上一页时使用 `page 100 before "..."`。正反向响应都保持声明的 sort 顺序。cursor 绑定数据库实例、schema identity、绑定后的规范查询与 typed 参数、方向、limit、commit sequence 和最后一行的完整 typed sort tuple；它由数据库 secret 使用 HMAC-SHA-256 验证，内容不加密。token 最多 8192 bytes、payload 最多 6144 bytes、sort key 最多 16 项。

首版使用 sequence-pinned 一致性：第一页固定当前 commit sequence；其后任何成功 data/schema mutation 都让旧 cursor 在扫描前返回 `E_CURSOR_STALE`。失败或回滚的 mutation、普通 read 和幂等 replay 不推进 sequence。redb 重开后 cursor 仍可使用；逻辑 backup/restore 会轮换数据库身份和 secret，因此源库 cursor 在恢复副本上返回完整性错误。完整契约与错误检查顺序见 [RFC 0003](rfc/0003-stable-cursor-pagination.md)。

Rust 应用可用 `QueryResponse::typed_page::<T>()` 或协议 `Response::typed_page::<T>()` 一次取得应用类型 rows 与 `PageInfo`。`PageInfo::next_page()` 和 `previous_page()` 返回可直接传给 `Engine::execute_page` 或 `Request::with_page` 的 `PageSpec`。没有 page 元数据时 `typed_page` 返回 `E_PAGE_SHAPE`，查询失败时保留原错误。

page response 总是一个完整、有界的 JSON 结果，不是 streaming。deadline 超时返回 `E_TIMEOUT` 且不产生 cursor；需要可靠续读时，客户端应保存成功 page 返回的 cursor。并发读取使用一致快照，独立的 version 1 stream 协议提供显式取消和带背压的 NDJSON frame；断开、取消、deadline 与 shutdown 会通过 operation registry 传播到仍在执行或发送的读取。stream 的部分结果不能续传，完整结果在进入发送阶段前释放快照。协议帧与资源边界见 [协议参考](PROTOCOL.md)和 [RFC 0007](rfc/0007-cancellable-backpressured-streams.md)。

如果业务依赖“前 N 行”，必须先 sort：

```text
from tasks
sort id
take 20
```

## 布局与语句边界

- 顶层 `from` 开始一条查询。同层 `let`、`filter`、`derive`、`group`、`aggregate`、`select`、`sort`、`take`、`page` 或兼容的 `limit` 延续当前 pipeline。
- 顶层 `update table` 开始修改，后续同层 filter、sort、take、set 和 returning 延续当前语句；顶层 `delete table` 开始删除，后续同层 filter、sort、take 和 returning 延续当前语句。
- `{}` 包围 record、projection、match branches 与 aggregate fields，`()` 包围 precedence、tuple、位置 payload 和嵌套调用，`[]` 包围 list；delimiter 内换行分隔多行项，逗号分隔紧凑单行项。旧缩进 match/group 与表达式 block 继续作为兼容输入。table、migration 和 explain 的外层 block 仍由缩进进入和退出；缩进不能使用 tab。
- 空行与 `#` 注释不结束查询。文件和非交互 stdin 在 EOF 提交完整脚本。
- 括号和集合内允许换行。字符串里的 `|`、逗号和 `#` 都是文本，不参与分隔。
- 同层出现新的 `from`、`explain`、`struct`、`enum`、`type`、`table`、`insert`、`upsert`、`update`、`delete` 或 `create` 时，前一条语句结束并开始新语句。
- REPL 使用与 parser 相同的 token、layout 和 EOF 状态判断 complete、incomplete、invalid。`ready>` 后的空行提交；incomplete 保留缓冲区继续输入，invalid 立即带 span 报错并清空。交互 EOF 执行 complete 缓冲区，或报告 incomplete 后退出。
- REPL 的空行是提交当前完整缓冲区的交互手势，不是文件语法的一部分，也不引入分号。

布尔表达式可在 `filter { ... }`、`=>` 的缩进块或 precedence 括号内跨行；`any/all` 的 predicate 括号也可跨行。标量函数参数和比较两侧当前保持在同一逻辑行。源码最多 1 MiB、100,000 tokens 和 64 层类型、值、表达式或布局嵌套；每条 pipeline 或 DML target 最多求值 100,000 个 list predicate 元素，超限返回受控错误。

## 错误与响应

查询失败返回结构化 `error`，其中包含错误码、可读消息和可选 `span {line, column}`。语法错误定位到 token；局部函数调用错误定位到调用名，其他执行前检查至少定位到所属语句，并在消息中给出字段、绑定或函数名。

常见错误类别：

| 错误 | 例子 |
| --- | --- |
| `E_FIELD` | 未知字段，或 `select` 后访问已经移除的字段 |
| `E_TYPE` | 比较或算术类型不匹配、typed value 与绑定类型不符、match 非 sum 字段、局部函数参数无法推断 |
| `E_QUERY` | 局部函数前向引用／递归／参数数量错误，或 pipeline stage 的作用域冲突 |
| `E_MATCH` | 未知/重复构造器、非穷尽 match、错误负载字段或分支绑定 |
| `E_ARITH` | 整数溢出、除零或产生非有限 float |
| `E_CONSTRAINT` | insert/update 后出现重复主键，或 upsert 的表未声明主键；整个请求回滚 |
| `E_SYNTAX` | 缺少操作符、错误缩进、未闭合结构或尾部多余 token |
| `E_LIMIT` | 源码、token、嵌套、局部定义/展开、集合谓词或聚合资源超过限制 |
| `E_RELATION_KEY` / `E_RELATION_LIMIT` | lookup/exists 相关目标字段没有可用索引，或某个 lookup driver row 的匹配数超过显式 take |
| `E_PAGE_SHAPE` / `E_PAGE_ORDER` | page 位置、limit、组合不合法，或排序既没有以主键收尾，也没有覆盖 equality-fixed prefix 后的完整 unique-index suffix |
| `E_CURSOR_LIMIT` / `E_CURSOR_CODEC` / `E_CURSOR_INTEGRITY` | cursor 超限、编码无效或 HMAC 验证失败 |
| `E_CURSOR_DATABASE` / `E_CURSOR_SCHEMA` / `E_CURSOR_QUERY` / `E_CURSOR_STALE` | cursor 的数据库、schema、绑定查询/参数/方向/limit 或 commit sequence 不匹配 |

查询成功响应包含 `rows` 和有序的 `columns {name, ty}`，未命中任何行时仍返回推导后的 columns。分页查询另带 `page` 元数据；`explain` 的 `plan.page` 显示 limit、方向、唯一排序 tuple、resume boundary、snapshot sequence、候选行、`limit + 1` 读取预算、cursor 上限，以及 `sorted_scan` 或 `index_seek`。`explain analyze` 另带 value-free `analysis`，但不返回业务 rows/columns/page cursor。insert/upsert/update/delete 成功响应包含 `affected_rows`；单行 upsert 还包含 `upsert_action`，批量 upsert 包含 `upsert_actions`。DML 默认不返回 rows/columns；使用 returning 后按其完整行或字段投影返回 typed columns/rows。primary/secondary lookup、full scan、复合 range/order、前后向 page seek、写入和 migration 的 M6 实测边界见 [工作负载成本记录](benchmarks/workload-2026-09-09.md)；format-5 bounded open、按需 decode 和 committed-view cache 的 10k/100k 结构证据见 [M7 Legacy0 有界读取记录](benchmarks/bounded-legacy-read-2026-09-09.md)。

以下片段是故意失败的反例：

| 反例 | 结果 |
| --- | --- |
| `from tasks \| filter missing == 1` | `E_FIELD`：字段不存在，即使 tasks 为空也会报错 |
| `from tasks \| select {id} \| sort state` | `E_FIELD`：state 已被投影移除 |
| `from tasks \| sort state` | `E_TYPE`：sum 类型没有排序语义 |

非穷尽模式在扫描前返回 `E_MATCH`：

```text
from tasks
filter match state {
  Pending => true
}
```

## 可执行示例与测试

| 场景 | 示例 | 覆盖内容 | 自动验证 |
| --- | --- | --- | --- |
| 任务状态 | [tasks.unid](../examples/tasks.unid) | sum、record、option/list、braced match、select/sort/take | `tests/language.rs::executable_examples`、CLI/TCP/恢复测试 |
| 嵌套配置 | [config.unid](../examples/config.unid) | 嵌套字段过滤与投影 | `tests/language.rs::executable_examples` |
| 事件记录 | [events.unid](../examples/events.unid) | typed 批量 insert、sum 完整值比较、typed derive 和字符串中的 `\|` | `tests/language.rs::executable_examples`、`typed_bulk_insert_*`、`versioned_tcp_bulk_inserts_*` |
| 后台任务队列 | [job_queue.unid](../examples/job_queue.unid) | 嵌套 sum/record/option/list、局部纯函数、布尔/集合 filter、普通与 ADT derive、group/aggregate、typed arithmetic、嵌套 pattern、多键 sort 与范围 take | `tests/language.rs::executable_examples` |
| 离线同步冲突 | [sync_conflicts.unid](../examples/sync_conflicts.unid) | 同一 `Conflict` constructor 的互补嵌套分支、typed derive 与 Option | `tests/language.rs::executable_examples` |
| Pipeline 顺序 | 测试内脚本 | take/filter 顺序与投影作用域 | `stage_order_and_projection_paths_are_preserved` |
| 单行/多行 | 测试内脚本 | 两种 pipeline 布局等价 | `newline_and_inline_pipelines_have_identical_results` |
| 模式检查 | 测试内脚本 | 嵌套穷尽性、不可达分支、积类型相关性、名义构造器、绑定和错误路径 | `match_filters_*`、`match_is_checked_*`、`match_rejects_*`、`complementary_nested_*`、`nested_pattern_coverage_*` |
| ADT 派生 | 测试内脚本 | 递归 pattern、option/sum/product/list 构造、类型统一、空表诊断与后续 stage | `derive_match_*`、`option_and_positional_*`、`nested_patterns_*`、`constructed_match_*` |
| 普通派生 | [job_queue.unid](../examples/job_queue.unid) 与测试内脚本 | scalar/bool 结果、命名类型、完整 ADT 复制、typed 参数、短路、空表检查和后续 stage 作用域 | `regular_derives_*` |
| 分组汇总 | [job_queue.unid](../examples/job_queue.unid) 与测试内脚本 | count/sum/min/max、命名数值、ADT key、空输入、溢出、后续 stage 与资源上限 | `basic_aggregates_*`、`grouped_aggregates_*`、`aggregates_reject_*`、`aggregate_group_limits_*` |
| 查询局部定义 | [job_queue.unid](../examples/job_queue.unid) 与测试内脚本 | 常量、单/多参数纯函数、match binding、aggregate 输入、显式类型、词法遮蔽、prepared 参数、调用与展开预算 | `query_local_*`、`local_function_*`、`prepared_queries_infer_parameters_through_local_functions` |
| 布尔与集合表达式 | [job_queue.unid](../examples/job_queue.unid) 与测试内脚本 | 优先级、括号、短路结构、字段间比较、命名 ADT list、`contains/length`、嵌套 `any/all`、Option helper、词法作用域、typed 参数、预算和空表错误 | `boolean_filters_*`、`list_predicates_*`、`list_and_option_predicates_*`、`match_conditions_share_*`、`boolean_expressions_are_checked_*` |
| 数值表达式 | [invoices.unid](../examples/invoices.unid) 与测试内脚本 | int/float/decimal 类型、固定 scale、逐步 precision、优先级、跨行括号、整数除法、短路及运行时错误 | `decimal_*`、`typed_arithmetic_*`、`arithmetic_*`、`boolean_short_circuit_*` |
| 列表分页 | 测试内脚本 | 嵌套多键排序、一基 Rust 风格范围、兼容语法和空表错误 | `multi_key_sort_*`、`sort_keys_and_take_ranges_*` |
| 稳定游标分页 | HTTP todolist 与接口测试 | typed page helper、重复排序前缀、正反向、redb 重开、TCP/HTTP 断开、deadline、migration 和 cursor 错误 | `tests/pagination.rs`、`tests/pagination_interfaces.rs`、`examples/todolist.rs` |
| UUID 与二进制 metadata | [content_metadata.unid](../examples/content_metadata.unid) | UUID 主键、hex bytes、unique index、contains/length、typed protocol v2、cursor、migration 与 backup/restore | `tests/uuid_bytes.rs` |
| 原子修改 | [task_mutations.unid](../examples/task_mutations.unid) 与测试内脚本 | typed/nested/simultaneous set、match target、穷尽 ADT match assignment、顶层保留 binding、typed 参数、主键冲突、运行时回滚、索引维护、稳定 RowId、TCP 和 redb 重开 | `update_*`、`failed_multi_row_updates_*`、`versioned_tcp_updates_*`、`adt_match_updates_*`、`redb_update_delete_*` |
| 主键 Upsert | [config.unid](../examples/config.unid) 与测试内脚本 | insert/replace action、完整 row 默认值、重复执行、回滚、索引更新、RowId/cursor 和 redb 重开 | `upsert_*`、`local_cli_reports_the_structured_upsert_action`、`redb_update_delete_*` |
| 批量插入 | [events.unid](../examples/events.unid) 与测试内脚本 | literal／参数 list、默认值、嵌套 ADT、空批次、批内冲突、预算、deadline、RowId/index 原子性、prepared/redb/TCP 与 returning 顺序 | `typed_bulk_insert_*`、`bulk_insert_validates_*`、`prepared_bulk_insert_*`、`parameterized_rows_*`、`versioned_tcp_bulk_inserts_*` |
| 批量 Upsert | 测试内脚本 | literal／参数 list、完整替换、输入内主键去重、unique index 冲突、稳定 RowId、逐项 action、deadline、CLI/TCP 与 redb 重开 | `typed_bulk_upsert_*`、`bulk_upsert_preserves_*`、`prepared_bulk_upsert_*`、`versioned_tcp_bulk_upserts_*` |
| Prepared DML | [parameters.rs](../examples/parameters.rs) 与测试内脚本 | 命名 row/list 参数、filter/set/match 推导、空表预检、主键/returning、schema 失效、deadline、WAL 拒绝和 redb 重开 | `prepared_dml_*`、`prepared_bulk_insert_*`、`parameterized_rows_*` |

新增语法只有在 parser、执行器、正反测试和本页同步后，才能从“未实现”移动到“已实现”。
