# 当前可运行的语言预览

本页描述已实现子集。更完整的表达式、match、函数、更新与 migration 仍在 [路线图](ROADMAP.md) 中。可运行示例：[任务](../examples/tasks.uid)、[配置](../examples/config.uid)、[事件](../examples/events.uid)。

## 类型、表与值

```text
type Contact =
  email text
  nickname option text

type State =
  Pending
  | Running {worker text, attempt int}
  | Done {result text}

type Task =
  id int
  owner Contact
  tags list text
  state State

table tasks Task
  key id

insert tasks
  id = 1
  owner =
    email = "alice@example.com"
    nickname = Some "Alice"
  tags = ["local", "sync"]
  state = Running {worker = "local", attempt = 2}
```

- 无分号。类型名和变体名以大写字母开头；缩进式 record 以小写字段名开头。当前标识符为 ASCII 字母、数字与下划线，首字符不能是数字；文本值支持 UTF-8。
- 原子类型为 `int`（i64）、`float`（有限 f64）、`bool`、`text`。类型引用必须先声明；当前拒绝递归引用和用户自定义泛型。
- 支持命名 record/sum、嵌套积类型、tuple，以及内建 `option T`、`list T`，例如 `type Point = (float, float)`、`option (list Contact)`。
- record 类型可内联为 `{email text, nickname option text}`；变体负载也可缩进：在 `| Running` 的下一层写 `worker text` 和 `attempt int`。
- record 值使用 `field = value`，内联字段之间用逗号；列表如 `[1, 2]`，tuple 如 `(1, "x")`。位置负载写成 `Pair(1, "x")`；单个 tuple 负载与多个位置参数通过括号区分。
- `None` 和 `Some value` 显式构造 option。全部字段必填，即使类型为 option 也必须写 `None`；默认值尚未实现。重复、缺失、未知字段及错误负载均报错。
- 命名类型保留身份；有歧义时可用 `State.Pending` 或 `State.Running {...}` 限定构造器。
- `table tasks Task` 要求 Task 是 record；可选的缩进 `key id` 声明 int/text 主键，拒绝重复键。无 key 时允许重复行。

## 查询

```text
from tasks
filter state == Running {worker = "local", attempt = 2}
select {id, owner.email, state}
sort id
take 20
```

已支持：

| 操作 | 形式 | 语义 |
| --- | --- | --- |
| 数据源 | `from tasks` | 开始查询 |
| 过滤 | `filter id >= 1` | 当前为字段路径与字面量比较 |
| 投影 | `select {id, owner.email}` | 保留列，响应按声明的列顺序展示 |
| 排序 | `sort id` / `sort -id` | 单列升序／降序；支持 int、float、text |
| 截取 | `take 20` | 保留当前结果前 20 行 |
| 单行 pipeline | `from tasks \| filter id == 1 \| take 1` | 与多行 pipeline 同语义 |

支持 `==`、`!=`、`>`、`>=`、`<`、`<=`；旧式 `=`、`limit`、无花括号 `select id,name` 仍可用。所有 stage 从左到右执行，`take` 和 `filter` 不可交换。未排序查询不承诺稳定行序。

字段和谓词类型在扫描之前校验，空表也会报未知字段；投影之后不能访问已移除列。选择嵌套字段时结果列名是完整路径，如 `owner.email`。和类型、option 和容器可做完整值相等比较，不能排序或直接穿过 variant/option 提取负载；模式匹配仍待实现。

Int 精确比较，Float 使用精确数值相等而非 epsilon，统一两种浮点零值；拒绝非有限 Float 和超出 i64 的整数字面量。Float 位置可以接受能够精确表示的整数常量；Int 位置不接受浮点字面量，也不会把数字静默变成文本。

索引兼容入口为 `create index tasks (owner.email)`，也支持整个 enum 值的等值索引；主键自动建索引。查询开头的等值 filter 可以通过索引直接取得候选行，其他情况使用扫描。索引与扫描共用类型化相等规则。

## 脚本边界与错误

同层的 `from` / `type` / `table` / `insert` / `create` 开始新语句；查询中的同层 `filter/select/sort/take/limit` 延续 pipeline。声明体、嵌套 record 与变体负载通过缩进确定范围；退格必须回到已有缩进层级，缩进不能使用 tab。括号内允许换行，字符串中的管道和逗号不是语法分隔符。

空行与 `#` 注释不改变文件中的语句边界。多行字符串暂用 JSON 转义（例如 `"first\nsecond"`），不支持跨物理行的字符串字面量。当前不支持任意运算表达式跨行、match/let/derive、参数占位符、update/delete/upsert 或 migration 语句，不能把设计文档的完整示例当作当前语法。

一次 `Engine.execute`、一次 `run` 或一个 TCP 请求是一个原子批次：先解析全部源码，再在候选状态中执行；任一步失败则不发布此次请求的任何修改。成功返回最后一条语句的结果，批次中的查询可以看到前面的写入。当前通过复制内存数据库实现写批次隔离，适合小工作集，尚未优化大批量写入的内存成本。

错误包含 `code`、可读 `message`、可选 `span {line, column}`。语法错误标出 token 位置；执行前类型校验目前定位到所属语句，并给出具体字段路径，精确表达式 span 后续完善。源码最多 1 MiB、100,000 tokens、64 层类型／值／布局嵌套，超限返回错误。

## 本地与 TCP

```bash
cargo run -- run --file examples/tasks.uid
cargo run -- run --file examples/config.uid --format json
cargo run -- cli --memory
cargo run --example embedded
```

`run` 每次创建一个内存库；`cli --memory` 的交互会话保留内存状态。在交互终端连续输入多行，空行显式提交整个缓冲区；`.quit` 退出，`.schema` 查看声明，`.tables` 列表。在管道或重定向 stdin 中读取到 EOF 后一次执行整个脚本，不按空行拆分。CLI 查询失败返回非零退出码。

```bash
cargo run -- server --addr 127.0.0.1:7878
cargo run -- cli --addr 127.0.0.1:7878 --file examples/tasks.uid
```

TCP 当前预览协议：每行一个 JSON 对象 `{"query":"完整源码（换行转义）"}`，每行一个 QueryResponse JSON 响应，也接受旧版纯文本单行请求。未知请求字段报错，结构化参数、request ID、完整协议版本协商仍待实现；JSON 中的换行不会被压平。

响应包含 `ok/message/columns/rows/error/warnings`。`columns` 保留投影顺序和类型描述；`rows` 采用有 tag 的值编码及名义类型 ID。该编码是预览接口，尚未提供跨客户端的 i64 兼容封装，JavaScript 等客户端需自行无损读取大整数。服务默认本机监听，当前限制 64 个活动连接、请求大小和 30 秒 socket 读写超时；查询预算、取消、优雅关闭等仍待完善。
