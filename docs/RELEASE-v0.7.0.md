# unionid v0.7.0 发布说明

v0.7.0 把查询语言收敛为更一致的 Rust 形状，同时保留 PRQL 从上到下的 pipeline 和无分号源码。它还补齐了三个常见的类型化查询组合能力：集合成员判断、相关存在性过滤和同形结果集合运算。应用现在可以直接表达“状态属于一组值”“存在／不存在满足条件的子项”和“合并活跃与归档来源”，不需要把 ADT 降成字符串或拆成多次应用层查询。

## 源码 breaking change

规范源码现在使用 `struct`、`enum`、`name: Type`、`name: value`、`State::Running`、`Option<T>`、`List<T>`、`!`、`&&`、`||` 和 Rust range。期望 enum 类型已经明确时，`State::Pending` 可以简写为 `Pending`。多行结构使用花括号和换行；紧凑单行项才使用逗号；仍不使用分号。闭包继续使用 `value -> expression` 或 `(left, right) -> expression`，因为 `|` 已用于 pipeline。

parser 暂时继续读取 v0.6 的 `type` record/sum、`.` 限定 constructor、文字布尔运算符、`field = value` 和旧缩进布局，以保证已有 migration、WAL 与源码可以打开。`unionid fmt --file <path>` 会输出 v0.7 规范形式。

**范围需要人工检查：** v0.6 的 `take 2..4` 表示闭区间；v0.7 与 Rust 一致，表示半开区间。需要保留旧结果时，先把它改成 `take 2..=4`，再格式化和重新验收查询。不要只运行 formatter 后直接部署。

建议升级顺序：

1. 保留 v0.6 二进制、数据库副本和已验证 logical backup。
2. 搜索所有 `take <start>..<end>`，逐条确认应使用 `..` 还是 `..=`。
3. 对 schema、migration、seed 和 query 文件运行 `unionid fmt --check`；用 `unionid fmt` 生成规范源码。
4. 运行 `unionid project check --dir .`，重新执行业务查询和 mutation 验收。
5. 使用 v0.7 `query rust` 重新生成静态绑定并重新编译客户端。

## 用户可见变化

- `in` / `not in` 使用完整 typed equality，支持列表字面量、字段、命名 list 和 prepared list 参数，可用于 filter、derive、match condition 和 mutation target。
- `filter exists { ... }` / `filter not exists { ... }` 提供有界相关读取。关联目标必须由主键或二级／复合索引首项支持，每个 stage 最多接受 10,000 个 driver rows，并在首个匹配处短路。
- `union` / `intersect` / `except` 合并 schema 完全一致的 pipeline。它们使用 nominal ADT identity、返回 distinct rows、保留定义好的首次出现顺序，并计入现有 working/result/内存预算。
- `unionid docs query` 从已安装二进制输出版本匹配、离线可用的 LLM 查询参考和可运行示例；JSON 形式把 reference 与 examples 分开。生成后的查询仍应配合真实 schema 使用 `query describe` 静态绑定。
- formatter、explain、prepared 参数、CLI/TCP/HTTP、LLM reference 和中英文文档使用同一套 v0.7 语义。

## 兼容与边界

v0.7.0 不改变数据库内部格式、component codec、logical backup 或网络协议。最低 Rust 仍为 1.94，redb 仍固定为 4.1.0；新数据库仍创建为 storage format 6，二进制继续读取 format 1–7。logical backup 当前格式仍为 4、可读 1–4；JSON Lines protocol 仍为 1/2，stream protocol 仍为 1。v0.6 数据库无需 storage upgrade 或 schema migration 即可直接打开。

集合运算首版不支持 `union all`、隐式 coercion、嵌套 set tree、跨来源 cursor 或磁盘 spill。相关 exists 首版只接受有索引的 typed 等值关联和内层 filter，不提供任意 join 或任意嵌套子查询。产品边界仍是单机、单数据库所有者、串行写入和约 10,000 行舒适工作集；100,000 行只是已测试上限。

## English Description

v0.7.0 converges the query language on a consistent Rust-shaped form while retaining PRQL's top-to-bottom pipeline and semicolon-free source. It also completes three common typed composition paths: set membership, correlated existence filtering, and set operations over identical result shapes. Applications can express “state belongs to this set,” “a qualifying child exists/does not exist,” and “combine active and archived sources” without flattening ADTs into strings or issuing several application-side queries.

### Source-level breaking change

Canonical source now uses `struct`, `enum`, `name: Type`, `name: value`, `State::Running`, `Option<T>`, `List<T>`, `!`, `&&`, `||`, and Rust ranges. `State::Pending` may be shortened to `Pending` when the expected enum type is known. Multiline structures use braces and newlines; commas remain for compact inline items; semicolons remain absent. Closures keep `value -> expression` or `(left, right) -> expression` because `|` already denotes the pipeline.

The parser temporarily accepts v0.6 `type` records/sums, dot-qualified constructors, word boolean operators, `field = value`, and legacy indentation so existing migrations, WAL records, and source remain readable. `unionid fmt --file <path>` emits the v0.7 canonical form.

**Ranges require manual review:** v0.6 interpreted `take 2..4` as inclusive; v0.7 follows Rust and makes it half-open. Change it to `take 2..=4` before formatting when the old result must be preserved. Do not deploy solely on the strength of a formatter pass.

Recommended upgrade sequence:

1. Retain the v0.6 binary, a database copy, and a verified logical backup.
2. Find every `take <start>..<end>` and decide explicitly between `..` and `..=`.
3. Run `unionid fmt --check` across schemas, migrations, seeds, and queries, then use `unionid fmt` to produce canonical source.
4. Run `unionid project check --dir .` and repeat application query and mutation acceptance.
5. Regenerate static bindings with the v0.7 `query rust` command and rebuild clients.

### User-visible changes

- `in` / `not in` use complete typed equality across list literals, fields, named lists, and prepared list parameters, including filters, derives, match conditions, and mutation targets.
- `filter exists { ... }` / `filter not exists { ... }` provide bounded correlated reads. The target must be supported by a primary key or the leading component of a secondary/composite index. Each stage accepts at most 10,000 driver rows and stops at the first match.
- `union` / `intersect` / `except` combine exact-schema pipelines. They preserve nominal ADT identity, return distinct rows in defined first-occurrence order, and charge the existing working/result/memory budgets.
- `unionid docs query` emits version-matched offline LLM guidance and runnable examples from the installed binary. Its JSON form separates the reference from examples. Generated queries should still be bound against the real schema with `query describe`.
- The formatter, explain output, prepared parameters, CLI/TCP/HTTP paths, LLM reference, and bilingual documentation share the same v0.7 semantics.

### Compatibility and limits

v0.7.0 does not change the database format, component codecs, logical backup, or network protocols. Rust 1.94 remains the minimum and redb remains pinned to 4.1.0. Fresh databases still use storage format 6 and the binary reads formats 1–7. Logical backup remains current at 4 with formats 1–4 readable; JSON Lines protocols remain 1/2 and stream protocol remains 1. A v0.6 database opens directly without a storage upgrade or schema migration.

The first set-operation release excludes `union all`, implicit coercion, nested set trees, cross-source cursors, and disk spilling. Correlated exists accepts indexed typed equality correlations and inner filters rather than arbitrary joins or arbitrary nested subqueries. The product boundary remains one machine, one database owner, serialized writes, and a comfortable working set around 10,000 rows; 100,000 rows is only a tested upper bound.
