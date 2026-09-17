# Unionid query language for LLMs

Use this compact reference when generating Unionid source. Unionid is not SQL: data has
algebraic types, and queries are ordered PRQL-style pipelines. Emit source only after the
application supplies its current schema. For a durable database, obtain that schema with:

```console
unionid schema print --db app.redb --format json
```

## Source rules

- Source is UTF-8 and semicolon-free. Newlines separate statements and multiline items.
- Use `#` for comments.
- Use `struct Name { field: Type }` for product types and
  `enum Name { Unit Tuple(Type) Record {field: Type} }` for sum types.
- Built-in types include `bool`, `int`, `float`, `text`, `uuid`, `bytes`, `date`,
  `timestamp`, `duration`, `Decimal<P, S>`, `Option<T>`, `List<T>`, and tuples such as
  `(int, text)`.
- Record values use `{field: value}`. Enum values use `Pending`, `Ready(value)`, or
  `Running {worker: "w1"}` when the expected enum type is known. Use a qualified form such
  as `State::Pending` in standalone or ambiguous expressions.
- Boolean operators are `!`, `&&`, and `||`. Parenthesize mixed `&&` and `||`.
- Closures use `value -> expression` or `(value: Type) -> expression`. Do not emit
  `|value|`; `|` joins compact pipeline stages.
- Braces make multiline schema, match, group, projection, assignment, and expression
  boundaries explicit. Commas are optional between multiline items.

## Schema and writes

```text
struct Task {
  id: int
  title: text
  state: State
}

table tasks: Task {
  key id
}

insert tasks {
  id: 1
  state: Pending
  title: "ship"
}
```

`insert many` and `upsert many` accept a `List<Row>`. Mutations may end in `returning`,
`returning field`, or `returning {field, nested.path}`. Update and delete targets may use
`filter`, `filter match`, `sort`, and `take` before the first `set` or `returning` stage.

## Query pipeline

A query begins with `from table`. Stages execute in source order:

```text
from tasks
filter priority >= $minimum
filter match state {
  Pending => true
  Running {attempt, ..} => attempt < 3
  _ => false
}
derive urgent = contains tags "urgent"
sort {-priority, id}
select {id, title, state, urgent}
take 11..31
```

Available stages are `let`, `filter`, `filter exists`, `filter not exists`, `filter match`, `derive`, `lookup`, `aggregate`,
`group`, `window`, `union`, `intersect`, `except`, `select`, `sort`, `take`, and `page`. A compact single-line query may join stages
with `|`. Do not reorder stages: `filter` after `take` observes only the taken rows, and
fields removed by `select` are unavailable later.

- `sort {-priority, created_at, id}` sorts descending by priority and ascending by the
  other keys. End stable production ordering in a unique key.
- `take 20` keeps at most 20 rows. One-based `take 11..21` is half-open; `take 11..=20`
  is the equivalent inclusive form.
- `page 100` performs stable keyset traversal and requires an explicit sort ending in the
  primary key. Continue with the opaque response cursor.
- `explain <query>` returns a plan without reading result rows. `explain analyze <query>`
  executes it and returns value-free work and timing measurements.

## Expressions and ADTs

Use `match` to inspect or construct ADTs. Matches are checked for exhaustiveness and
unreachable branches before scanning:

```text
from tasks
derive label = match state {
  Pending => "pending"
  Running {worker, ..} => worker
  Done {result} => result
}
```

Patterns recursively support enum payloads, records, tuples, `Some(value)`, `None`, and
lists. `_` is a catch-all. `{field, ..}` binds one record field and ignores the rest.

Arithmetic supports checked `+`, `-`, `*`, `/`, and unary `-`. Comparisons use `==`,
`!=`, `<`, `<=`, `>`, and `>=`. Collection helpers include `contains`, `length`,
`is_some`, `is_none`, `any`, and `all`. Parameters start with `$` and are type-checked
against the supplied schema before execution.

## Aggregation and bounded relation lookup

Use `aggregate` for ungrouped results and `group keys { aggregate {...} }` for grouped
results. Functions are `count`, `count_distinct`, `avg`, `sum`, `min`, and `max`.

```text
from tasks
group state {
  aggregate {
    rows = count
    total = sum cost
  }
}
sort state
```

Use a ranking window when rows must remain rows while receiving a position inside each
group. Always emit an explicit window `sort`; omit `partition` only for one global group:

```text
from tasks
window {
  partition state
  sort {-priority, created_at}
  position = row_number
  placing = rank
  dense = dense_rank
}
filter position <= 3
sort {state, position}
```

`row_number` distinguishes ties by the stable input order, `rank` leaves gaps after ties,
and `dense_rank` does not. A window appends `int` fields without reordering output rows.
Do not combine `window` with `page`, and do not invent frames, `lag`, or `lead`.

`lookup lines from order_lines on order_id == id take 100` attaches a bounded typed list.
The target key must be indexed. Unionid intentionally has no general SQL-style flattened
join.

Use a braced right-hand pipeline to combine two result sets with exactly the same field
names, order, and types:

```text
from active_tasks
select {state, tags}
union {
  from archived_tasks
  select {state, tags}
}
```

`union`, `intersect`, and `except` compare the complete typed row, including named and
nested ADTs. They remove duplicates while preserving first occurrence order. Do not emit
implicit casts, nested set operations, or `page` on either side; use `sort` after the set
stage when the caller needs an explicit final order.

Use an indexed correlated existence filter when the related rows are not needed in the result:

```text
from tasks
filter exists {
  from task_items
  filter task_id == outer.id
  filter state != Done
}
```

Use the complementary form when no target row may match:

```text
from tasks
filter not exists {
  from task_items
  filter task_id == outer.id
  filter state != Done
}
```

The inner pipeline currently accepts only `filter`. It must contain a typed equality between
an indexed target path and an explicit `outer.<path>`. Use `filter not exists { ... }` when the
driver row must have no matching target row. Do not invent nested subqueries or unindexed
correlations.

## Mutation

```text
update tasks
filter id == $id
set {
  attempts = attempts + 1
  state = match state {
    Pending => Running {worker: $worker, attempt: 1}
    current => current
  }
}
returning {id, state, attempts}
```

Assignments in one `set` block are simultaneous and read the pre-update row. A request is
an atomic script. Prefer an idempotency key through the client protocol when retrying a
mutation after a lost response.

## Generation checklist

1. Read the exact current schema; never invent tables, fields, variants, or indexes.
2. Choose a read or mutation and keep stages in the required source order.
3. Use exhaustive ADT matches or a final `_`/binding branch.
4. Bound potentially large reads with an indexed filter, `take`, or `page`.
5. Use `unionid fmt` to canonicalize generated source.
6. Use `unionid query describe --db app.redb --file query.unid` to bind a saved query
   without executing it, then run it only after diagnostics pass.
