# 声明式 Schema 与 Diff

应用可以把期望结构保存在普通 `.unid` schema 文件中（兼容窗口内也接受 `.uid`）。文件只包含 类型、`table`、`create index`、`create unique index` 和 `create reference` 声明，沿用数据库语言的无分号、缩进式语法；不能包含数据写入、查询或 migration。为保证规范输出能以相同身份顺序重建，声明依次放置 named types、tables、secondary indexes、references。示例见 [`examples/schema.unid`](../examples/schema.unid)。

```text
type State = Pending | Running | Complete

type Task =
  id int
  title text
  state State
  priority int = 0

table tasks Task
  key id

create index tasks (state)
create unique index tasks (priority)
```

## 检查和规范化

```text
unionid schema check --file schema.unid
unionid schema print --db app.redb
```

`schema check` 会完整解析并类型检查声明，然后输出可再次解析的规范格式和 schema hash。空行、注释、record 的 inline/multiline 写法等格式差异会归一化，不会生成虚假 diff。`schema print` 从现有 redb 数据库输出相同格式。两者都支持 `--format json`。

主键自动拥有的内部等值索引只通过 `key` 表达，不额外输出 `create index`；用户显式创建的 secondary index 会保留在规范输出中。

## 生成 migration 草稿

```text
unionid migration diff \
  --db app.redb \
  --schema schema.unid \
  --name add_task_priority
```

diff 读取 live schema 和 migration ledger，但不写数据库。数据库文件不存在时，以空库生成 initial migration。命令在默认 `migrations` 目录创建下一个带正确 ID/parent 的文件，并报告操作、破坏性标记、受影响类型、表、现有行数和索引数。增加 sum variant 时还会提示旧客户端穷尽 match 的兼容风险。JSON 输出包含完整规范化目标 schema 和结构化报告。

以下确定性变化可直接生成可运行操作：

- 增删命名类型、表、字段、变体和 ordinary/unique secondary index；同一路径约束 kind 的变化生成 drop 后 add
- 有明确默认值的新字段和默认值变更
- 主键增加、删除或替换
- 从空库创建完整命名 ADT schema

diff 不猜测身份或数据转换。疑似 type/table/field/variant rename、required 字段回填、字段类型或 variant payload 变化、字段／变体重排会生成 `todo ...` 行，并把草稿标记为 `runnable: false`。`todo` 故意不属于 migration 语法，因此 plan/apply 会拒绝未补全草稿。开发者需要把它改为明确的 `rename`、带 `using old -> ...` 的转换或其他显式操作。

草稿补全后走正常 runner：

```text
unionid migration plan --db app.redb
unionid migration apply --db app.redb
```

plan 在数据库副本上验证最终数据、约束和目标结构；apply 才会原子提交。diff 文件只是期望状态，migration 文件仍是不可修改的执行历史。

## 引用约束（v0.14 开发中） / References (v0.14 in development)

引用声明放在表和目标唯一索引之后，例如：

```text
struct Parent {id: int}
struct Child {id: int, parent: Option<int>}
table parents: Parent {key id}
table children: Child {key id}
create reference children (parent) references parents (id)
```

diff 可生成 `add reference` 和 `drop reference`。删除表或替换引用依赖的主键／唯一索引时，先移除引用，再变更目标，最后按目标 schema 重建引用；仅调整引用声明的顺序不产生操作。保留的引用不因无关 schema 变更而重建。新增引用仍检查现有数据：若已有孤儿行，diff 的候选验证会拒绝生成可运行迁移，源库保持不变。疑似 rename 和类型转换继续要求显式业务决策。

引用路径不变，但目标类型或 Exact/Optional 模式改变时，草稿也会生成显式 drop/add，并将字段转换 TODO 放在两者之间。例如把 `Child.parent` 从 `Option<int>` 改为 `int`，填写原 `None` 值的实际目标后即可按生成顺序执行；无需另外猜测引用的重绑定步骤。未补全转换前仍为 `runnable: false`，补全后通过 plan 检查实际数据。

Declare references after tables and target unique indexes. Diff emits `add reference` and `drop reference`, removing dependencies before a table or pinned key is removed and adding the target declarations after keys/indexes exist. Reordering reference declarations alone produces no operations; unrelated schema changes preserve existing references. Adding a reference validates existing rows and rejects orphaned candidates without changing the source database. Renames and type conversions continue to require explicit decisions. This feature remains under development for v0.14.

Even when paths are unchanged, a changed target type or Exact/Optional mode generates explicit drop/add operations with conversion TODOs between them. For example, changing Child.parent from Option<int> to int requires supplying an existing target for former None values, but no additional rebinding steps or ordering fixes. The draft remains non-runnable until conversions are supplied; use plan to validate the resulting data.
