# RFC 0017: Rust-shaped PRQL language / Rust 形状的 PRQL 查询语言

- Status / 状态: Accepted / 已接受
- Date / 日期: 2026-09-16
- Tracking / 跟踪: [#335](https://github.com/worktools/unionid/issues/335)
- Amends / 修订: [RFC 0005](0005-structured-prql-query-syntax.md)

## 中文说明

### 决策

unionid 保留 PRQL 的从上到下 pipeline，同时让类型、值、pattern、运算符和范围采用 Rust 用户熟悉的形状。规范源码不写分号；多行结构依靠花括号和换行分项，紧凑单行结构才使用逗号。

```text
struct Contact {
  email: text
  nickname: Option<text> = None
}

enum State {
  Pending
  Running { worker: text, attempt: int = 0 }
  Done { result: text }
}

struct Task {
  id: int
  owner: Contact
  state: State
}

table tasks: Task {
  key id
}
```

字段在类型和值中分别写成 `name: Type` 和 `name: value`。泛型内建类型写成 `Option<T>`、`List<T>`、`Decimal<P, S>`。限定构造器使用 `State::Running`；record payload 使用 `{}`，位置 payload 使用 `()`。一个 tuple 作为唯一 payload 时保留双层括号，例如 `Pair((left, right))`。

布尔运算符采用 `!`、`&&`、`||`。`take start..end` 是半开区间，`take start..=end` 包含末端。跨行表达式可以放进 `{}`，改变优先级时使用 `()`：

```text
from tasks
filter {
  owner.email == $email
  && (!archived || priority >= 10)
}
filter match state {
  State::Running {attempt, ..} => attempt >= 2
  _ => false
}
sort {-priority, id}
take 1..=20
```

### 闭包

闭包特意不采用 Rust 的 `|value| expression`。竖线已经连接单行 pipeline，同时承担闭包边界会增加扫描和报错恢复的歧义。unionid 保留箭头形式：

```text
let important = value -> value >= 10
let has_tag = (values: List<text>, tag: text) -> contains values tag

filter any history (
  attempt -> {
    attempt.score >= 10
    && is_none attempt.error
  }
)
```

单参数可省略参数括号；多参数使用 `()` 和逗号。参数类型在需要时写成 `name: Type`。箭头闭包仍是非递归纯函数，不产生可存储函数值。

### 兼容与迁移

这是面向 v0.7 的源码 breaking change。`unionid fmt` 输出新的规范形式，并是旧查询文件的迁移入口。parser 暂时继续读取旧的 `type` record/sum、`.` 限定构造器、文字布尔运算符、`field = value` 和缩进布局，原因是 migration ledger、WAL 与既有数据库必须可恢复；兼容输入不再定义语言风格。

范围语义不能只靠格式化迁移：旧的 `take 2..4` 曾表示闭区间，新语义是半开区间。需要保留旧结果的源码必须改成 `take 2..=4`。formatter、starter、示例、schema display 与文档均以本 RFC 为准。

## English Description

### Decision

Unionid keeps PRQL's top-to-bottom pipeline and adopts Rust-shaped types, values, patterns, operators, and ranges. Canonical source has no semicolons. Newlines separate items in multiline braces; commas remain for compact inline forms.

Fields use `name: Type` in declarations and `name: value` in values. Built-in generic types use `Option<T>`, `List<T>`, and `Decimal<P, S>`. Qualified constructors use `State::Running`. Record payloads use braces and positional payloads use parentheses. A tuple carried as one positional payload uses an extra pair of parentheses, such as `Pair((left, right))`.

Boolean operators are `!`, `&&`, and `||`. `take start..end` is half-open, while `take start..=end` includes the endpoint. Braces delimit multiline expressions and parentheses change precedence.

### Closures

Closures deliberately keep the arrow form instead of Rust's paired pipes. `|` already joins a compact pipeline, so reusing it around parameters would make scanning and error recovery less clear.

```text
let important = value -> value >= 10
let has_tag = (values: List<text>, tag: text) -> contains values tag
filter any history (attempt -> attempt.score >= 10)
```

A single parameter may omit parentheses. Multiple parameters use parentheses and commas. Optional annotations use `name: Type`. These remain non-recursive pure query-local functions rather than storable function values.

### Compatibility and migration

This is a source-level breaking change targeted at v0.7. `unionid fmt` emits the new canonical form and serves as the migration tool for old query files. The parser temporarily accepts legacy spellings so durable migrations, WAL records, and existing databases remain recoverable. Legacy acceptance does not define canonical style.

Range semantics require explicit review: legacy `take 2..4` was inclusive, while the new form is half-open. Source that needs the old result must use `take 2..=4`. The formatter, starter project, examples, schema display, and language reference follow this RFC.
