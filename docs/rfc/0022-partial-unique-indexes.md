# RFC 0022：部分唯一索引 / Partial unique indexes

- 状态 / Status: proposed
- 日期 / Date: 2026-09-19
- 跟踪 / Tracking: [#248](https://github.com/worktools/unionid/issues/248)

## 中文说明

### 1. 问题与目标

普通 unique index 会约束表中每一行，但日常业务常只要求一个状态集合内唯一。例如用户邮箱只在账号尚未删除时唯一，外部任务 ID 只在任务仍处于 `Active` 状态时唯一。把已删除行物理移走、给 key 拼接状态或在应用层先查再写都会泄漏存储细节，并且无法在并发和批量 mutation 下提供原子保证。

本 RFC 增加只有谓词为真时才包含一行的 partial unique index。首版只解决条件唯一约束，不增加普通 partial index、通用 expression index、动态时间窗口、用户函数或基于统计信息的 optimizer。

### 2. 声明语法

谓词使用 Rust match guard 风格的 `if`，继续遵守无分号语言约束：

```text
create unique index users (email) if deleted_at == None
create unique index jobs (provider, external_id) if state == Active
```

复杂条件用必要括号和换行表达层级：

```text
create unique index sessions (tenant, token) if (
  revoked_at == None
  && state == Active
)
```

`if` 比索引 component 列表绑定得更松，谓词在行类型作用域内解析。formatter 对单个短 atom 保持单行；多个 atom 输出带括号的多行 `&&`。不引入 `where`、分号或另一套表达式语法。普通 index 和普通 unique index 的现有源码保持不变。

Migration 使用同一形状：

```text
migration m0002_active_email
  parent m0001_users
  add unique index users (email) if deleted_at == None
```

删除 partial index 必须重复其规范谓词：

```text
drop index users (email) if deleted_at == None
```

谓词变化是显式 drop/add。字段改名通过 stable field path 更新显示文本，不改变 index ID。

### 3. 首版谓词子集

DDL 接受的谓词不是任意查询表达式，而是可规范化的 conjunction：

```text
atom && atom && ...
```

atom 只允许：

- `field.path == typed_literal`
- `field.path == None`
- `is_some field.path`

`typed_literal` 必须在 schema 建立时完整绑定，不能包含参数、字段引用、算术、局部函数、`any`/`all`、`contains`、map lookup 或运行时错误。unit enum constructor 可在字段类型明确时简写，例如 `state == Active`。Option 字段也接受查询语言已有的 `is_none field` 输入，但 formatter 统一输出更接近 Rust 的 `field == None`；`is_some field` 保持现有 helper 写法。允许必要括号，但拒绝 `||`、`!`、`!=`、range 比较和 bool 字段的裸引用。

这组限制使谓词确定、row-local、无错误且可在 schema、mutation、migration、restore 和 check 中得到同一结果。超出范围返回稳定的 `E_INDEX_PREDICATE`，诊断指出第一个不支持的结构。类型错误继续返回 `E_TYPE` 并保留源码 span。

一个谓词最多包含 64 个源码 atom。该边界在规范化前执行，避免重复或恶意输入放大绑定、排序和矛盾检查成本；超过边界返回 `E_INDEX_PREDICATE`。

绑定后表示为有序 atom 列表，而不是保存通用 `BoolExpression`：

```text
IndexPredicate
  atoms [
    Equal {field_path, value_type, value}
    IsNone {field_path}
    IsSome {field_path}
  ]
```

atom 按 stable field-ID path、operator 和 canonical typed value 排序。因此交换 `&&` 两侧、改变空白、省略明确的 enum 类型前缀，或在输入中使用 `== None` 都不会改变 schema identity。`Equal(None)` 不进入 bound IR；它总是规范化为 `IsNone`。完全重复的 atom 被折叠；同一路径上的不同 equality、`IsNone`/`IsSome`、`IsNone`/非 None equality 冲突在 DDL 前返回 `E_INDEX_PREDICATE_CONTRADICTION`。首版不尝试完整 SAT 求解。

### 4. 索引身份与 schema

index shape 扩展为：

```text
(table stable ID, ordered components, normalized predicate)
```

kind 继续是定义属性；同一 shape 不能同时存在 ordinary 与 unique 两种 kind。无谓词使用 `predicate = None`。由于 predicate 属于 shape，普通全表 index `(email)` 和 partial unique index `(email) if deleted_at == None` 可以共存。

`IndexDefinition` 增加可选的 `predicate`。schema source、hash、diff、migration ledger、`.schema`、`schema print` 和 introspection 都输出 canonical predicate。portable schema 增加 version 2；version 1 仍可读取不含 predicate 的 schema，但不能无损描述 partial index。Rust ADT codegen 不因 index predicate 改变生成的行类型。

predicate 引用 stable field paths。rename 只更新显示名称；drop/change 被引用字段前必须先 drop index。改变字段类型时，即使新类型表面兼容，也要求显式重建 predicate index，以重新绑定 literal 和 schema identity。

### 5. 写入和唯一性

只有 predicate 对最终行求值为 true 时才生成 index key。状态转换如下：

| 旧值 | 新值 | index 操作 |
| --- | --- | --- |
| false | false | 无 |
| false | true | 插入 key 并验证唯一 |
| true | false | 删除旧 key |
| true | true | 按现有规则删除旧 key、插入新 key |

insert、upsert、update、delete、批量 mutation、prepared mutation 和 migration 都在候选最终状态上维护索引。一个请求中多行交换 key 时按最终状态验证，不因中间顺序产生伪冲突。重复继续沿用现有 unique index 契约返回 `E_CONSTRAINT`，包括 mutation 冲突和创建／migration 扫描已有数据时发现的冲突；消息包含 table、canonical component shape 和 canonical predicate，但不泄漏冲突行内容。整个请求不发布 rows、indexes、schema、ledger 或 receipt。

index key codec 不编码 predicate，也不需要为未命中的行保存占位符。它继续编码 index stable ID、typed component tuple 和 RowId；predicate 由 catalog definition 决定一行是否应有 posting。memory 与 redb 必须共享同一个 predicate evaluator。

### 6. Planner 的安全使用

partial index 只能在查询的已绑定过滤条件可机械证明蕴含 index predicate 时参与访问计划。首版证明规则有意保守：

1. planner 只读取现有 stage barrier 之前的简单 filter；
2. 将多个 filter 和 filter 内纯 `&&` 展平成已绑定 atom 集合；
3. index predicate 的每个 atom 必须在 query atom 集合中有完全相同的 stable path、operator、静态类型和 canonical value；
4. 额外 query atom 不影响证明；`||`、`!`、match、函数和计算表达式不产生证明；
5. `field == Some(value)` 可以蕴含 `is_some field`，其他跨 operator 推理保持 deferred。

例如：

```text
from users
filter deleted_at == None
filter email == $email
```

可使用 `(email) if deleted_at == None`。只过滤 `email`、使用 `is_some deleted_at`，或在 stage barrier 后才添加 `deleted_at == None` 时不能使用该索引。

`explain.plan.access` 在选择 partial index 时增加 canonical `index_predicate` 和 `predicate_proven = true`。回退计划可列出被拒绝的 partial index 及有界 reason code，例如 `predicate_not_implied`，但不输出 literal、参数值或业务数据。现有 equality/range/order/page 规则在证明完成后照常应用。

partial unique index 只有在查询已证明 predicate 时才能提供 unique page order。`fetch_by_key` 没有额外 predicate 输入，因此首版不把 partial unique index 当作全表唯一证明。

### 7. 持久格式与升级

旧 binary 若忽略 predicate，会把 partial index 当成全表 unique index，并可能错误拒绝合法写入或重建错误 posting，因此不能只依靠 serde optional field 做向后兼容。

实现使用以下显式边界：

- storage format 10：format 8 加 partial-index catalog；
- storage format 11：format 9 的 backup-journal 对应版本；
- catalog codec 6：编码 normalized predicate；
- logical backup format 6：保存 predicate definition；
- ordered index key、value、receipt、migration 和 journal codec 保持当前版本，因为其字节结构不变。

新数据库创建为 format 10；启用 journal 进入 11。旧 8/9 数据库继续只读写已有能力，创建或恢复 partial index 返回 `E_STORAGE_UPGRADE_REQUIRED`。显式 `upgrade --target 10` 与 `9→11` 在一个同步 two-phase transaction 中升级 catalog 能力且保留 database identity、cursor secret、sequence、RowId、schema、ledger、receipt 和 journal。升级本身不创建 partial index；旧 binary 对 10/11 fail closed。

`check` 为每个 partial index 重新绑定 predicate，扫描全部行，证明 true 行恰有一个正确 posting、false 行没有 posting，并验证 true 行的 unique key 无重复。logical backup/restore、incremental journal、maintenance generation 和 compact 都保留定义与 posting。

### 8. 实现阶段与验收

实现按以下阶段推进，每阶段独立 PR 并在 #248 中记录：

1. RFC、parser/formatter、normalized predicate IR、schema identity 与稳定错误；
2. memory index 与所有 mutation/migration 的最终状态约束；
3. storage format 10/11、catalog codec 6、backup 6、upgrade/restart/check/restore；
4. planner implication、explain 和 page uniqueness；
5. 软删除 email 与 Active external ID 的完整业务场景、故障注入和文档。

最终验收必须证明：相同 key 可同时存在于 predicate=false 行；最多一条 predicate=true 行；false↔true 状态转换、批量 key 交换和 migration 冲突都原子；不满足 implication 的查询结果与 forced scan 一致；format 8/9 升级、中断恢复、backup/restore 和 journal 保持完整状态。

## English Description

### 1. Problem and goal

A full unique index constrains every row, while common applications need uniqueness only within a state domain: an email among non-deleted users, or an external ID among active jobs. Moving inactive rows elsewhere, adding state to the key, or checking in application code leaks storage policy and cannot provide an atomic guarantee for concurrent or batched mutations.

This RFC adds a partial unique index that contains a row only when its predicate is true. The first release does not add ordinary partial indexes, general expression indexes, dynamic time windows, user functions, or a statistics-driven optimizer.

### 2. Source syntax

The predicate uses a Rust match-guard-shaped `if` and keeps the language semicolon-free:

```text
create unique index users (email) if deleted_at == None
create unique index jobs (provider, external_id) if state == Active
```

Necessary parentheses make multiline precedence explicit:

```text
create unique index sessions (tenant, token) if (
  revoked_at == None
  && state == Active
)
```

Migrations use the same suffix. Dropping a partial index repeats its canonical predicate. Predicate changes are explicit drop/add operations. A field rename updates display text through stable paths without changing the index ID.

### 3. Predicate subset and normalization

The accepted predicate is a conjunction of `field.path == typed_literal`, `field.path == None`, and `is_some field.path` atoms. Literals bind completely at schema time. The existing query helper `is_none field` is also accepted as input, but the formatter emits the more Rust-shaped `field == None`; `is_some field` retains the existing helper syntax. Parameters, other field references, arithmetic, local functions, collection operations, maps, `||`, `!`, `!=`, ranges, and bare boolean fields are rejected with `E_INDEX_PREDICATE`; ordinary type errors remain `E_TYPE` with source spans.

A predicate contains at most 64 source atoms. The limit applies before normalization so duplicate or hostile input cannot amplify binding, sorting, or contradiction checks; exceeding it returns `E_INDEX_PREDICATE`.

The bound representation is a sorted list of equality/presence atoms keyed by stable field path, operator, and canonical typed value. `Equal(None)` never enters this IR; it becomes `IsNone`. Reordering conjunctions, changing whitespace, shortening an unambiguous enum constructor, or spelling the input as `== None` does not change schema identity. Duplicate atoms collapse. Different equalities on one path, None/Some conflicts, and None/non-None equality conflicts return `E_INDEX_PREDICATE_CONTRADICTION`; the first release does not implement a general SAT solver.

The index shape becomes `(table stable ID, ordered components, normalized predicate)`. A full index and a partial unique index may therefore share components. The same shape cannot exist with two kinds. Schema source/hash/diff, migrations, introspection, and portable schema version 2 preserve the predicate. Dropping or changing a referenced field requires dropping the index first.

### 4. Mutation semantics

Only rows whose predicate evaluates to true produce an index key. Mutations apply the false-to-false, false-to-true, true-to-false, and true-to-true transitions against the candidate final state. Insert, upsert, update, delete, batches, prepared mutations, and migrations validate final keys before publishing any rows, indexes, schema, ledger, or receipt. Mutation conflicts and duplicate rows found while creating an index or applying a migration preserve the existing unique-index contract and return `E_CONSTRAINT`, with the table, component shape, and predicate but no conflicting row data.

The ordered-key codec remains unchanged: it stores the index ID, typed component tuple, and RowId. The catalog predicate determines posting membership. Memory and redb use one predicate evaluator.

### 5. Planner proof

A partial index is eligible only when bound query filters mechanically imply its predicate. The initial proof flattens simple filters and pure conjunctions before the existing stage barrier, then requires every predicate atom to have an identical stable path, operator, static type, and canonical value in the query atom set. Extra query atoms are harmless. Disjunction, negation, match, calls, computed expressions, and conditions after a barrier do not contribute. Equality to `Some(value)` may imply `is_some field`; other cross-operator reasoning is deferred.

Explain reports the canonical `index_predicate` and `predicate_proven = true` for a selected partial index. A fallback may expose a bounded `predicate_not_implied` reason without literals, parameter values, or business data. Existing equality, range, ordering, and page rules apply only after the proof. Partial uniqueness proves page order only inside an implied predicate domain and does not become a global `fetch_by_key` uniqueness proof.

### 6. Durable compatibility

An old binary that ignores the predicate could enforce uniqueness on the wrong rows, so an optional serde field is not a safe compatibility boundary. Storage formats 10 and 11 extend formats 8 and 9 respectively; catalog codec 6 and logical backup format 6 encode normalized predicates. Ordered-key, value, receipt, migration, and journal codecs do not change.

New databases use format 10, with 11 for an active backup journal. Existing formats 8/9 keep their current capabilities until an explicit 8-to-10 or 9-to-11 upgrade. Creating or restoring a partial index before upgrade returns `E_STORAGE_UPGRADE_REQUIRED`. The synchronous two-phase upgrade preserves durable identity, cursor secret, sequence, RowIds, schema, ledger, receipts, and journal. Old binaries reject formats 10/11.

Integrity checking rebinds every predicate and verifies that true rows have exactly one correct posting, false rows have none, and qualifying unique keys do not collide. Logical backup/restore, incremental journal, maintenance generations, and compaction preserve definitions and postings.

### 7. Delivery and acceptance

Delivery is split into: RFC plus parser/formatter/normalized IR; memory and mutation/migration semantics; formats 10/11 plus backup/upgrade/check; planner implication plus explain/page proof; and full soft-delete/active-state journeys with fault coverage and user documentation.

Final acceptance demonstrates duplicate keys outside the predicate, at most one qualifying key, atomic false/true transitions and batch key swaps, atomic migration conflicts, forced-scan equivalence when implication is absent, and complete format-8/9 upgrade, interrupted recovery, backup/restore, and journal behavior.
