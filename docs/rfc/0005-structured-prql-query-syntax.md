# RFC 0005：PRQL 风格的结构化查询语法

- 状态：已接受，分阶段实现
- 日期：2026-09-08
- 父任务：[#119](https://github.com/worktools/unionid/issues/119)
- 实现任务：[#142](https://github.com/worktools/unionid/issues/142)、[#143](https://github.com/worktools/unionid/issues/143)；时间 literal 随 [#139](https://github.com/worktools/unionid/issues/139) 交付

## 中文说明

### 1. 决策摘要

unionid 采用 PRQL 的结构化语法习惯，而不是只借用 `from/filter/select` 等关键字：

- 换行是规范 pipeline 连接，`|` 是等价的单行连接；
- `{}` 表示有命名或有顺序的字段集合，包括 record、projection、sort keys、derive/aggregate/set fields 和 match branches；
- `[]` 只表示同质 list；
- `()` 表示 precedence、tuple、嵌套函数调用或传给 `group` 的子 pipeline；
- 空格表示函数应用，`name:value` 表示真正需要名字的可选参数；
- `=` 只表示绑定/赋值，`==` 表示相等；
- 源文件没有语句末尾分号。逗号分隔 `{}`/`[]`/tuple 内的相邻项，允许 trailing comma。

这些符号都有明确语义。目标不是最少字符，而是让同一种结构在类型、值和查询中保持同一种视觉边界。PRQL 官方也把换行/`|` 视为 pipeline，把 `{}` 用于 tuple/field sets、`[]` 用于 arrays、`()` 用于 inner transforms，并用空格调用函数：

- [Pipes](https://prql-lang.org/book/reference/syntax/pipes.html)
- [Tuples](https://prql-lang.org/book/reference/syntax/tuples.html)
- [Function calls](https://prql-lang.org/book/reference/syntax/function-calls.html)
- [Transforms](https://prql-lang.org/book/reference/stdlib/transforms/index.html)

unionid 不是 PRQL dialect，也不编译到 SQL。它保留原生 ADT pattern、名义类型、严格 Option、mutation、migration 和有界执行语义。

### 2. 规范查询形态

普通读取使用正交 transform：

```text
from tasks
filter (
  priority >= $minimum
  and match state {
    Pending => true,
    Running {attempt, ..} => attempt < 3,
    _ => false,
  }
)
derive {
  urgent = priority >= 10,
  label = match state {
    Pending => "pending",
    Running {..} => "running",
    Done {..} => "done",
  },
}
select {
  id,
  title,
  state,
  label,
}
sort {-priority, created_at, id}
page 50
```

短查询仍可写成一行：

```text
from tasks | filter id == $id | select {id, title}
```

换行在 pipeline 顶层连接 transform；在 `{}`、`[]`、`()` 内只用于布局，不隐含 pipe。跨行表达式必须处于一个明确 delimiter 内，避免依赖反斜线或尾部 operator 猜测语句是否继续。

### 3. Field set

`derive`、`select`、`aggregate`、`set` 和 `returning` 共用 field-set 形态。单项可以省略花括号，多项必须使用花括号：

```text
derive score = priority + bonus

derive {
  score = priority + bonus,
  retryable = attempts < 3,
}

select id

select {
  id,
  display = title,
  retryable,
}
```

field set 按源码从上到下绑定。后续项可引用前面产生的名字；所有项也能读取 stage 输入。`select` block 最终只输出列出的字段，`derive` 保留输入列并追加/替换声明列。重复输出名在扫描前返回 `E_QUERY`。

computed `select` 不引入第二套 expression：`select {display = title}` 降为现有 typed derive + projection IR，使用同样的参数、算术、match、预算和错误。单纯字段 projection 保持原字段的稳定类型和名义身份。

`returning` 首版仍只接受路径，不接受任意表达式；它复用 braces/commas 形态，不因此绕过 mutation 提交前的响应预算。以后若需要 computed returning，必须复用 computed-select lowering，而不是增加 mutation-only evaluator。

### 4. Match 是普通 typed expression

`match` 使用花括号包围 branch set，逗号分隔 branch。pattern 中的 record payload 继续使用花括号，因此嵌套层级在视觉上闭合：

```text
derive next_state = match state {
  Pending => Running {worker = $worker, attempt = 1},
  Running {worker, attempt} => Running {
    worker = worker,
    attempt = attempt + 1,
  },
  current => current,
}
```

`filter match` 不再作为概念上的特殊 transform。规范形式是 filter 接收一个返回 bool 的 match expression：

```text
filter (
  match state {
    Running {attempt, ..} => attempt >= 2,
    _ => false,
  }
)
```

parser 仍把它降到现有 typed match IR；穷尽性、不可达分支、binding scope 和扫描前检查完全不变。现有缩进式 `filter match state` 与 `derive x =` 后的缩进 match 作为兼容输入保留，formatter 输出 braced expression。

### 5. Group 与 aggregate

`group` 与 PRQL 一样接收 key field set 和一个括号包围的 inner pipeline：

```text
from tasks
derive state_label = match state {
  Pending => "pending",
  Running {..} => "running",
  Done {..} => "done",
}
group state_label (
  aggregate {
    task_count = count,
    priority_total = sum priority,
    first_created = min created_at,
  }
)
sort {-priority_total, state_label}
```

多个 key 写 `group {owner, state_label} (...)`。inner pipeline 的 `()` 是 relation boundary，不是普通 tuple。首个实现只允许当前已有的 `aggregate` inner pipeline；语法为以后在组内组合 `sort/take` 留出结构，但不能在执行器支持前宣称可用。

### 6. 函数、参数与 operator

普通函数继续使用空格应用，嵌套调用用括号：

```text
derive normalized = clamp min:0 max:100 (score + bonus)
filter contains tags $tag
```

`name:value` 只用于函数声明过的 named parameter，colon 不用于 record field type 或赋值。位置参数仍优先用于短而稳定的调用。`$name` 是 runtime typed parameter，与 PRQL 的参数形式一致。

unionid 保留文字形式 `not`、`and`、`or` 作为规范 bool operator；它们在长条件中比 `!`、`&&`、`||` 更容易扫描，也避免 `!` 同时承担 exclusion。混用 `and`/`or` 时 formatter 加括号明确 grouping。算术和比较 precedence 保持现有规则，复杂表达式主动使用 `()`。

`take 11..20` 沿用 inclusive range，`sort {-priority, +created_at, id}` 允许显式方向，`+` 可省略。`page` 是 unionid 的有界 keyset extension，继续作为最后一个 row-producing stage。

### 7. 类型和值使用同一结构符号

命名 product type、record value 和 constructor record payload 都使用 `{}`；字段以逗号分隔，不使用冒号或分号：

```text
type Contact = {
  email text,
  nickname option text = None,
}

type State =
  Pending
  | Running {
      worker text,
      attempt int,
    }
  | Done {result text}

insert tasks {
  id = 1,
  owner = {
    email = "alice@example.com",
  },
  state = Running {
    worker = "local",
    attempt = 1,
  },
}
```

位置 tuple 与位置 constructor payload 继续使用 `()`，同质 list 使用 `[]`。`{}` 不同时表示匿名 list；这保留 ADT product/list 的静态差异。

类型 constructor 使用空格应用：`option text`、`list Contact`、`decimal 18 2`。嵌套 application 在需要时用括号，例如 `option (list Contact)`。

### 8. 生产标量 literal 修订

日期和 timestamp 采用 PRQL 的 `@` literal，因为该符号直接消除普通 text 的歧义：

```text
issued_on = @2026-09-07
created_at = @2026-09-07T09:30:15.123456+08:00
```

unionid 不接受 time-only 或无 offset timestamp；`@YYYY-MM-DD` 唯一推断为 date，带 `T` 和 `Z`/numeric offset 唯一推断为 timestamp。验证、UTC 规范化和微秒精度仍由 RFC 0004 定义。

精确 duration 使用 PRQL 风格的 number-unit literal：

```text
retry_after = 30seconds
retention = 7days
timeout = 1500milliseconds
```

首版单位为 `microsecond(s)`、`millisecond(s)`、`second(s)`、`minute(s)`、`hour(s)`、`day(s)` 和 `week(s)`；它们都精确换算为微秒。compound duration 写成普通算术 `1day + 2hours`。year/month 保持拒绝，因为需要 calendar context。formatter 按最大的可整除单位输出单个 literal，否则输出 duration 项的加减表达式。

UUID、decimal 和 bytes 没有无歧义的通用 token，继续使用 typed literal `uuid "..."`、`decimal "..."`、`bytes "..."`。

### 9. Mutation 形态

mutation 保留显式 statement root，避免把读取 pipeline 尾部悄悄变成写入。selection stages 仍按顺序应用，多个 assignment 使用 field set：

```text
update tasks
filter id == $id
set {
  attempts = attempts + 1,
  state = match state {
    Pending => Running {worker = $worker, attempt = 1},
    current => current,
  },
}
returning {id, state}
```

同一 `set {}` 的右侧都读取修改前的 row，保持现有 simultaneous assignment；这里不同于 derive 的从上到下 scope，避免 swap/update 依赖书写顺序。单项 `set field = value` 保持规范。insert/upsert 的 record braces 明确 payload 结束位置。

### 10. 兼容、formatter 与持久源码

这是 canonical source 调整，不改变 typed AST、查询结果或 schema identity：

- parser 接受当前缩进 record/match/group、重复 `set` 和单项 transform；formatter 输出本 RFC 的 braces/commas/inner-pipeline 形式；
- `unionid fmt --check` 会把旧布局视为非 canonical，但执行不会发 deprecation warning；
- 已应用 migration 的 checksum 不能被 formatter 重写，migration loader 必须长期读取其原始旧语法；
- transitional WAL 中保存的旧 source 必须继续回放，直到完成显式导入；
- idempotency digest 基于精确 query source。调用方不能用同一个 key 把旧布局重试成新布局，否则应得到既有 conflict；
- formatter 必须 idempotent，parse(format(parse(source))) 保持同一 AST、schema hash、plan 和执行结果。

结构 parser/formatter 切片由 #142 实现后，新文档与示例统一使用 canonical syntax。尚未落地的 field-set transforms 与 literal 仍必须明确标为目标语法，不能假装已经可运行。

### 11. 实现切片与验收

1. 结构 parser/formatter：braced multiline type/value/match、逗号、group inner pipeline、兼容输入和 source roundtrip。
2. Field-set transforms：多项 derive/select/aggregate/set、computed select、稳定列顺序、作用域和 typed lowering。
3. Function/literal polish：named arguments、`@` temporal 和 exact duration unit literal；时间 literal 随 RFC 0004 的 temporal 实现交付。

验收场景必须覆盖 task/event/config 的短查询、复杂 match、多列 derive/select、group aggregate、update set、formatter、REPL incomplete 判断、旧 migration/WAL、prepared params、idempotency digest 和 source diagnostics。所有语法糖必须在扫描前降为现有 typed IR，不增加第二套执行语义。

## English Description

### Decision

unionid adopts PRQL's structural syntax conventions rather than borrowing only transform names. Newlines are the canonical pipeline connection and `|` is the inline equivalent. Braces delimit named field sets and record-like products, brackets delimit homogeneous lists, parentheses express precedence, tuples, nested calls, or an inner group pipeline, whitespace applies functions, and `name:value` is reserved for meaningful named arguments. Commas separate adjacent delimited items and may trail; statement-ending semicolons do not exist.

The canonical query form adds braced multi-field `derive`, `select`, `aggregate`, and `set`; computed select items lower to the existing typed derive-plus-projection IR. Match becomes an ordinary typed expression with a braced branch set. `filter (match value {...})` therefore shares exactly the same exhaustiveness, reachability, binding, parameter, and budget semantics as derive and mutation expressions. Group follows `group keys (inner pipeline)`, initially restricting the inner pipeline to the already implemented aggregate behavior.

Function calls remain whitespace-based, nested calls use parentheses, and named arguments use colon only when the function declares them. unionid keeps readable `not`/`and`/`or` operators instead of importing PRQL's symbolic boolean aliases. Sort direction, inclusive take ranges, and the unionid-specific bounded page stage retain their current semantics.

Types and values use the same structural boundaries: named records and constructor record payloads use braces and commas, positional tuples use parentheses, and homogeneous lists use brackets. Mutations keep an explicit statement root. A braced `set` is simultaneous—all right-hand sides read the old row—even though derive/select field sets bind from top to bottom.

Temporal source literals move to PRQL-style `@YYYY-MM-DD` and `@...T...Z/offset`. Exact durations use number-unit literals such as `30seconds`, `1500milliseconds`, and `7days`; year/month calendar intervals remain excluded. UUID, decimal, and bytes retain explicit typed string literals.

### Compatibility

The parser keeps accepting existing indentation-based record/match/group forms, repeated sets, applied migration files, and transitional WAL source. The formatter emits the new canonical braces, commas, and inner-pipeline layout and must remain idempotent. Formatting does not change the typed AST or schema identity, but idempotency receipts intentionally retain exact-source digests, so callers must not change query layout while retrying the same key.

Implementation is split into structural parser/formatter work, field-set transform lowering, and function/literal polish. Every new form must lower before scanning to the current typed IR and share its errors, resource budgets, persistence, and execution behavior.
