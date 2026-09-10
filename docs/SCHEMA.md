# Schema 身份、版本与演进契约

状态：v0.2 契约，2026-09-10。本文定义 catalog 中对象的身份、应用 schema 版本，以及 migration 必须遵守的兼容规则。当前已经实现稳定 ID、原子 schema revision、schema hash、共享名称空间、响应元数据、[版本化 ADT value codec](CODEC.md)、[显式 schema migration 与版本化 runner](MIGRATIONS.md)，以及[声明式 schema diff](SCHEMA-DIFF.md)。

存储格式版本、语言／协议版本与应用 schema revision 是三个独立概念：升级 unionid 二进制不自动修改应用 schema，读取目标 schema 文件也不会隐式迁移已有数据。

## 1. Catalog 身份

每个数据库有一个单调递增、从 1 开始的 `u64` catalog ID 空间。类型、字段、sum 变体、表和索引都从同一空间分配 ID。ID 永不复用，且不是用户数据的 RowId。

RowId 属于单张表的内部行身份，使用独立、从 0 开始的单调 `u64` 空间。它不出现在用户 record 中，也不等同于业务主键；移动内存位置或未来 migration 改写值时保留 RowId。删除 RowId 后不复用，索引始终引用该稳定身份。每表的下一分配值与 catalog definition 一同持久化，但不属于应用 schema hash。

| 对象 | 名称作用域 | 引用方式 |
| --- | --- | --- |
| 命名类型 | 数据库 | type ID |
| 表 | 与命名类型共享数据库级名称空间 | table ID |
| record 字段 | 所属 record | field ID 路径 |
| sum 变体 | 所属 sum type | variant ID |
| 索引 | 所属表 | index ID、table ID、field ID 路径与 ordinary/unique kind |

命名类型采用名义类型：形状相同但 type ID 不同的两个类型不能互换。字段与变体的运行时含义同样以 ID 为准；名称用于源码、诊断和显示。表引用命名 record 的 type ID，多个表可以安全共享同一个 row type。索引绑定 table ID 和逐层 field ID，不把点分隔名称当作长期身份。

普通 DDL 负责创建对象；migration 的显式 `rename` 保留 ID，`drop` 后创建同名对象得到新 ID。不得通过修改名称或调整声明顺序重新解释已有值。命名类型可在定义体内直接引用自己的 type ID；catalog 只有在不动点检查证明该类型至少存在一个有限值时才接受定义。sum 的终止变体以及 `option`/`list` 的空值可结束递归，纯别名循环和全部必需成员都回到自身的积类型以 `E_SCHEMA` 拒绝。其他命名类型仍须先声明，当前不支持互递归声明批次。详细规则见 [RFC 0001](rfc/0001-finite-recursive-adts.md)。

## 2. Revision 与 hash

空数据库的 schema revision 是 0。一次成功的原子脚本只要包含 type、table 或 index 变更，就在提交时把 revision 增加 1；同一脚本包含多个 schema 语句仍只产生一个 revision。纯 insert 或查询不改变 revision，解析、类型检查、约束或持久化失败也不发布新 revision。

`schema.hash` 是 `sha256:<hex>`。当前 hash manifest 的格式版本是 1，内容按稳定 ID 排序，包含类型及其完整结构、表定义和索引定义（包括 unique kind），不包含行、索引 posting、提交 sequence 或 schema revision。名称和对外可见的字段顺序属于 schema，因此会影响 hash；稳定 ID保证这种变化不会改变旧值的含义。

revision 用于同一数据库内的快速失效检查；hash 用于备份、导入、migration plan 和不同进程之间的精确 schema 对照。两者都不是 migration ID。客户端不得假设两个独立创建但文本相同的数据库具有可互换的 catalog ID。

每个由 Engine 执行的响应都带当前信息：

```json
{
  "ok": true,
  "message": "1 row(s)",
  "rows": [],
  "columns": [],
  "schema": {
    "revision": 3,
    "hash": "sha256:..."
  }
}
```

typed plan、prepared query 和长期连接在绑定时记录 revision 与 hash。执行前若不一致就重新绑定；重新绑定失败时返回 schema changed 错误，不使用旧的字段或变体位置继续执行。

## 3. 兼容性分开判断

“已有数据仍可解释”与“旧客户端不需要改动”不是同一件事。migration plan 必须分别报告数据安全、查询／客户端兼容和索引影响。

| 变化 | 已有数据 | 查询／客户端 | v0.2 要求 |
| --- | --- | --- | --- |
| 新增有默认值的字段 | 可回填 | 读取旧投影通常兼容 | 原子回填并保留其他 ID |
| 新增 `option T` 字段 | 仍需明确值 | 读取旧投影通常兼容 | 显式默认 `None`，不把遗漏当作 None |
| 字段／类型／变体重命名 | 无需改写值 | 使用旧名称的源码不兼容 | 显式 rename，保留 ID |
| record 字段重排 | 无需按位置改写值 | 列顺序敏感客户端可能不兼容 | 保留 field ID，plan 报告顺序变化 |
| 新增 sum 变体 | 旧数据安全 | 旧穷尽 match 可能失效 | 使相关 typed plan 失效 |
| 删除 sum 变体 | 可能存在旧值 | 破坏性 | 对全部引用路径做穷尽转换后才提交 |
| 修改变体负载或字段类型 | 通常需转换 | 破坏性 | 显式转换并验证每行 |
| 收紧 option／增加唯一约束 | 可能有违反数据 | 可能破坏写入方 | 预检全量数据，任一失败则回滚 |
| 删除字段、类型或表 | 数据丢失 | 破坏性 | 显式破坏性步骤并在 plan 中标记 |

同一个命名类型可能被多个表直接或嵌套引用。改变该类型时，plan 必须遍历所有引用表、相关字段路径与索引；转换和 catalog 更新在同一写事务提交。

字段默认值是 schema manifest 的一部分，因此会改变 schema hash。当前默认值在声明时检查并规范化为 typed value；insert、filter 等上下文构造 typed record 时逐层补齐，已经显式提供的值始终接受正常类型检查。未来新增字段的 migration 复用同一规则回填已有行，不在读取时临时伪造缺失字段。

## 4. 两个版本的例子

v1 中两个表共享 `State`，它们存储的是相同的 type ID 与 variant ID：

```text
type State =
  Pending
  | Failed {message text}

type Task =
  id int
  state State

table active Task
  key id

table archive Task
  key id
```

目标 v2 把 `Failed` 改名为 `Rejected`，并增加 `code`。下面语法当前可以直接执行：

```text
migration task_state_v2
  parent task_state_v1

  rename variant State.Failed to Rejected

  change variant State.Rejected to {code int, message text}
    using old -> {code = 0, message = old.message}
```

`State`、`Rejected`（原 `Failed`）以及 `message` 保留原 ID，`code` 获得新 ID。`active` 和 `archive` 的全部 `Failed` 值在同一事务转换；任何一行失败时两个表和 schema 都保持 v1。重新创建一个叫 `Rejected` 的变体不能替代 rename，因为那会得到新身份。

## 5. Migration 历史

每个 migration 文件包含不可变 ID、可选的唯一 parent、源码 checksum，以及应用后期望的 schema hash。数据库 ledger 记录 ID、parent、checksum、提交后的 revision/hash 和应用时间。

apply 前必须验证：

1. ledger 是从根到当前 head 的单链，ID 不重复且没有环。
2. 新 migration 的 parent 等于当前 head；同一 parent 出现另一个子项视为分叉并拒绝。
3. 已应用 ID 的 checksum 不得变化；完全相同的重复 apply 是 no-op。
4. 当前 schema hash 与 ledger head 记录一致；plan 计算每个待应用文件的目标 hash，apply 把实际提交后的同一 hash 写入 ledger。
5. schema、数据、索引和 ledger 在一个持久事务中提交。

`plan` 只读，展示前后 revision/hash、身份保留与新增列表、受影响表／索引、需扫描和转换的行数、客户端兼容影响与破坏性步骤。恢复旧 schema 通过备份还原或新的前向 migration 完成，不生成隐含 down migration。

## 6. 与 redb 的边界

redb catalog 表持久化上述 ID、索引 kind、每表 RowId 分配游标、revision、hash manifest 版本和 migration head。catalog codec v2 显式编码 ordinary/unique，仍可读取没有 kind 的 v1 index 并将其解释为 ordinary；下一次成功提交会写回 v2。已实现的 row codec 使用 type/field/variant ID，按 field ID 排序 record，并严格拒绝未知身份或形状；名称不进入值的身份编码。row key 使用 table ID 与 RowId，secondary index key 使用 index identity 与 RowId。一个 Engine schema 脚本对应一个 redb 写事务，提交成功后响应中的 revision/hash 才可见。

旧 WAL/snapshot 没有 revision 时，恢复过程把其中完整的非空 catalog 视作 revision 1 的导入基线；已有稳定 ID 保留，缺失的 table/index ID 按确定顺序补齐。#20 的正式导入工具还会校验行数、catalog、revision 和 hash，并保留原文件。
