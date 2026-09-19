# Unionid query language for LLMs

Use this compact reference when generating Unionid source. Unionid is not SQL: data has
algebraic types, and queries are ordered PRQL-style pipelines. Emit source only after the
application supplies its current schema. For a durable database, obtain that schema with:

```console
unionid schema print --db app.redb --format json
unionid cli --db app.redb --read-only --query .schema
```

The first command returns the schema-specific machine-readable contract. The second prints
canonical source and is convenient for a human or an LLM context. Use `.tables`, `.types`, or
`.storage` in the same one-shot form when only catalog or storage context is needed. With
`--format json`, these CLI introspection commands return a stable envelope containing `kind` and
`introspection`; do not infer schema from raw redb bytes.

## Source rules

- Source is UTF-8 and semicolon-free. Newlines separate statements and multiline items.
- Use `#` for comments.
- Use `struct Name { field: Type }` for product types and
  `enum Name { Unit Tuple(Type) Record {field: Type} }` for sum types.
- Built-in types include `bool`, `int`, `float`, `text`, `uuid`, `bytes`, `date`,
  `timestamp`, `duration`, `Decimal<P, S>`, `Option<T>`, `List<T>`, `Map<text, T>`, and
  tuples such as `(int, text)`. Map values use `map {"key": value}`; keys are text.
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

## Indexes and conditional uniqueness

Declare secondary indexes with 1–16 ordered field paths. `-` marks descending order:

```text
create index tasks (state)
create unique index tasks (owner.email)
create index tasks (state, -priority, id)
```

A `unique` index may carry a row-local `if` predicate, which constrains only rows whose
predicate is true. Use it for conditional uniqueness such as non-deleted emails or active
external IDs:

```text
create unique index users (email) if deleted_at == None
create unique index jobs (provider, external_id) if state == Active
```

The first-release predicate is a conjunction of `field.path == literal`, `field.path == None`,
and `is_some field.path`; `== None` normalizes to `is_none`. Do not emit `||`, `!`, `!=`,
ranges, parameters, field references, arithmetic, calls, `any`/`all`, `contains`, or map lookups
inside a predicate. A predicate change is an explicit `drop index` plus `create unique index`;
renaming a referenced field keeps the index through its stable field path. `explain` uses the
partial index only when the query filters mechanically imply its predicate and then reports
`index_predicate` and `predicate_proven`.

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
`is_some`, `is_none`, `any`, and `all`. For `Map<text, T>`, use `contains_key map key`,
`get map key`, `keys map`, `values map`, or `entries map`; `get` returns `Option<T>`, and
traversal follows canonical key order. `length` also accepts maps. Parameters start with `$`
and are type-checked against the supplied schema before execution.

Decimal `*` and `/` are intentionally explicit: use `decimal_mul left right P S "mode"`,
`decimal_div left right P S "mode"`, or `decimal_round value P S "mode"`. Modes are
`exact`, `toward_zero`, `away_from_zero`, `floor`, `ceil`, `half_up`, and `half_even`.

## Aggregation and bounded relation lookup

Use `aggregate` for ungrouped results and `group keys { aggregate {...} }` for grouped
results. Functions are `count`, `count_distinct`, `avg`, `decimal_avg`, `sum`, `min`, and `max`.
Decimal average requires the explicit form `decimal_avg amount 18 2 "half_even"`.

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

## Diagnostics and errors

Failures return a stable `code`, a readable `message`, an optional source `span`, and an optional
value-free `hint` with the next step. `E_CONSTRAINT` errors also carry a `constraint` class:
`unique`, `partial_unique`, `primary_key`, or `primary_key_missing`. Read these fields instead of
parsing prose, and never place literal or parameter values into generated context.

Common codes:

- `E_SYNTAX`, `E_INCOMPLETE`: fix the source and re-run; `E_INCOMPLETE` means more input is needed.
- `E_TYPE`, `E_FIELD`: a field, variant, or type does not match the bound schema.
- `E_TABLE`: unknown table; an empty-database hint says to apply migrations first.
- `E_CONSTRAINT`: a key or unique conflict; inspect `constraint` and prefer `upsert` for a primary key.
- `E_INDEX_PREDICATE`, `E_INDEX_PREDICATE_CONTRADICTION`: the partial predicate is unsupported or self-contradictory.
- `E_PAGE_ORDER`: end the final sort in the primary key, or cover a complete unique-index suffix after equality-fixed fields.
- `E_PAGE_SHAPE`: keep `page` as the only read pipeline and the only statement, with a valid position and stage composition; this is structural and does not require changing the sort.
- `E_STORAGE_UPGRADE_REQUIRED`: an older database needs an explicit `unionid upgrade`.
- `E_READ_ONLY`: the target is read-only.
- `E_ARITH`, `E_DECIMAL_RANGE`: checked arithmetic failed before any commit.

## Generation checklist

1. Read the exact current schema; never invent tables, fields, variants, or indexes.
2. Choose a read or mutation and keep stages in the required source order.
3. Use exhaustive ADT matches or a final `_`/binding branch.
4. Bound potentially large reads with an indexed filter, `take`, or `page`.
5. Use `unionid fmt` to canonicalize generated source.
6. Use `unionid query describe --db app.redb --file query.unid` to bind a saved query
   without executing it, then run it only after diagnostics pass.
7. After a failure, branch on the stable `code` and `constraint`, follow `hint`, and fix the
   source instead of retrying blindly. After a lost response, retry a mutation only with the
   client idempotency key.

