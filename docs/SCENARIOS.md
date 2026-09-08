# 实际场景与查询覆盖矩阵

状态：核心场景契约，2026-09-07。本文从应用代码会保存和读取的数据出发，检验 ADT 与查询语言是否实用。当前可执行语法仍以 [LANGUAGE.md](LANGUAGE.md) 和 [QUERY.md](QUERY.md) 为准；标有 issue 的片段是目标语法。

unionid 最适合一个进程或少量客户端持有的应用状态：数据规模有限，表之间很少 join，但单行内部有明确的状态、分支和嵌套结构。重点不是替代分析型 SQL，而是让应用从 schema、查询到返回值都保持同一套代数类型。

## 1. 后台任务与本地队列

任务系统需要把“当前状态”及各状态独有的数据放在一起。拆成多个可空列会产生 `finished_at` 出现在 Queued、`worker` 出现在 Failed 等非法组合；sum type 可以排除这些状态。

```text
type Failure =
  Network {message text}
  | InvalidData {field text, message text}

type Payload =
  Sync {source text, target text}
  | Webhook {url text, body text}

type AttemptLog =
  worker text
  checkpoints list int
  note option text = None

type JobState =
  Queued {scheduled_at int, attempt int = 0}
  | Running {worker text, started_at int}
  | Succeeded {finished_at int}
  | Failed {error Failure, retry_at option int = None}

type Job =
  id text
  priority int = 0
  created_at int
  tags list text = []
  history list AttemptLog = []
  payload Payload
  state JobState
```

常见工作流：

- 查找可运行任务：解构 `Queued`，比较 `scheduled_at`，按 priority、时间和 id 排序后取一页，并把状态派生为统一的 text 标签。失败任务可直接用 `Failed {error = Network {message}, retry_at = Some at}` 解构嵌套失败原因和重试时间。当前完整示例见 [job_queue.uid](../examples/job_queue.uid)。
- 按主键读取任务：当前可用 `filter id == "job-a" | take 1`，持久模式由主键索引执行；`explain from jobs | filter id == "job-a" | take 1` 可验证 lookup、候选数和结果 schema。
- 原子 claim：按旧状态筛选，`sort {-priority, scheduled_at, id} | take 1` 稳定选出下一条，再用 `set state = match state {...}` 把 `Queued` 改成 `Running`。末尾 `returning {id, state}` 在同一请求中返回命中的新状态；没有候选时得到稳定 columns、空 rows 和 affected_rows 0，无需先查询 ID。
- 查询高优先级且带 `sync` 标签的任务：`filter priority >= 10 and contains tags "sync"` 已实现并进入可执行示例；再与 `filter (match state {...})` 组合即可限定状态。
- 复用业务判断：`let important = value -> value >= 10` 和 `let has_tag = (values list text, tag text) -> contains values tag` 可在当前 pipeline 后续的 filter、derive 与 aggregate 输入中重复调用，避免复制长条件。
- 检查嵌套执行历史：`derive has_retry = any history (attempt -> any attempt.checkpoints (checkpoint -> checkpoint >= 3) and is_some attempt.note)` 可以逐层绑定 record/list 元素并把判断追加成 typed bool 列；`all` 提供空 list 为 true 的全称语义。完整示例和预算边界见查询参考。
- 按状态计数：`derive state_label = match state {...}` 把 sum 分支归一为状态名，再用 `group state_label (aggregate {...})` 得到每种状态的任务数、总优先级和最早创建时间；可执行示例见 [job_queue.uid](../examples/job_queue.uid)。

列表查询必须提供唯一的最终排序键，例如 `sort {-priority, created_at, id}`。只按 priority 分页会让相同优先级的跨请求边界不稳定。

## 2. 嵌套配置与连接定义

应用配置通常是较深的积类型，连接方式和凭据来源则是 sum type。类型化结构能避免散落的字符串 key，也能区分“未配置”和“配置为空”。

```text
type SecretRef =
  Env text
  | File {path text}
  | Inline text

type Auth =
  Anonymous
  | Bearer SecretRef
  | Basic {username text, password SecretRef}

type Transport =
  Http {base_url text, auth Auth, headers list {name text, value text}}
  | UnixSocket {path text}

type Validation =
  Valid
  | Invalid {issues list {path text, message text}}

type ServiceConfig =
  name text
  environment text
  transport Transport
  owner option text = None
  validation Validation = Valid
```

常见工作流：

- 按 environment、owner 和固定 record 路径筛选；普通 record 路径当前已支持，option 需要 #35 显式解构。
- 只列出 HTTP 服务并投影 `base_url`；需要 #35 的 match expression，因为字段只存在于 `Http` 分支。
- 判断某个完整 header 或 validation issue 是否存在可用 `contains`；按元素字段筛选可用 `any headers (header -> header.name == "authorization")`。若 `headers` 位于 sum payload 中，先用 `filter (match ... {...})` 建立 list binding，再在 condition 中使用 `any/all`。
- 整体 upsert 一份配置并校验嵌套类型；当前按主键插入或完整替换，示例见 [`config.uid`](../examples/config.uid)。
- 把 `Bearer` 改名或给 `Http` 增加字段；身份保留、回填和转换属于 #17–#19。

密码值本身不应默认明文保存；`SecretRef` 表达引用来源。静态加密和访问控制不是 ADT 能自动解决的问题。

## 3. Webhook 与事件收件箱

事件表经常共享 envelope，但 payload 随事件类型变化。sum type 允许应用穷尽处理已知事件，同时可显式保留未知来源。

```text
type EventPayload =
  UserCreated {user_id text, email text}
  | InvoicePaid {invoice_id text, amount_cents int}
  | Unknown {kind text, raw text}

type DeliveryFailure =
  Timeout {after_ms int}
  | Response {status int, body text}

type DeliveryState =
  Pending
  | Attempting {attempt int, started_at int}
  | Delivered {at int}
  | DeadLetter {failures list DeliveryFailure}

type Event =
  id text
  received_at int
  source text
  payload EventPayload
  delivery DeliveryState = Pending
```

常见工作流包括批量接收事件、筛选 `InvoicePaid` 金额、为不同 payload 派生摘要、列出下一批 Pending 事件、追加投递失败、统计来源和清理过期记录。`insert many events $rows` 可从 Rust/TCP 一次提交 typed `list Event`，逐行补默认 delivery 并在整批主键验证后原子写入；literal 示例见 [events.uid](../examples/events.uid)。当前 `filter (match ... {...})` 能筛选单 record 负载并在 condition 中组合布尔、比较和集合判断；`DeadLetter {failures}` 分支可用 `any failures (failure -> failure == Timeout {after_ms = 5000})` 检查元素。状态更新与保留期删除已经可执行，来源统计可用 `group source (aggregate {...})`。

## 4. 离线同步与冲突状态

同步工具既有嵌套连接配置，也有多种 change 和互斥状态，是 ADT 比扁平表更直接的场景。

```text
type Remote =
  Git {url text, branch text}
  | Http {url text, token option text}

type Change =
  Added {path text, hash text}
  | Modified {path text, before text, after text}
  | Deleted {path text, before text}

type SyncState =
  Clean {revision text}
  | Dirty {base text, changes list Change}
  | Conflict {path text, local Change, remote Change}

type Workspace =
  id text
  remote Remote
  state SyncState
  last_sync option int = None
```

常见查询是列出全部 Conflict 并提取双方 change、查找 changes 中触及某路径的 Dirty workspace、按 `last_sync` 找未同步项，以及原子提交一次冲突解决。[sync_conflicts.uid](../examples/sync_conflicts.uid) 已用同一个 `Conflict` constructor 的三个互补嵌套分支完整覆盖 local change 并派生类型化标签；`Dirty {changes}` 的 condition 可用 `any changes (change -> change == Added {path = $path, hash = $hash})`，`is_none last_sync` 可检查未同步状态。条件 update 已复用同一表达式。

## 5. Session、缓存与功能开关

轻量 key/value 用法仍应按表声明业务类型，不提供绕开 catalog 的任意动态 blob。

```text
type Expiry = Never | At int

type SessionState =
  Active {user_id text, scopes list text}
  | Revoked {at int, reason text}

type Session =
  token text
  expires Expiry
  state SessionState
```

高频操作是按 key get、原子替换、到期扫描和批量删除。主键读取已有语义，更新/删除和返回值归 #15，`At` 分支与当前时间参数归 #35/#22。unionid v0.1 不内置后台 TTL 时钟；调用方以显式参数发起清理，保证查询仍是确定的。

功能开关可以把规则声明为 list of sum，例如 `User text | Group text | Percentage int`。按 key 读取整个 typed flag 很合适；固定规则值可用 `contains`，元素 predicate 可用 `any/all`，需要按不同 constructor 提取 payload 的复杂规则求值仍更适合在应用代码完成。

## 6. 有限树、原因链与规则 AST

固定层数的嵌套 record 无法表达目录、评论树或规则表达式。把节点拆成父子表会引入 join 和跨行一致性，而这些小型结构通常随所属对象整行读取和原子替换。直接自递归 named ADT 保留 constructor 约束：

```text
type Tree =
  Leaf text
  | Branch
    label text
    children list Tree

type Document =
  id int
  tree Tree

table documents Document
  key id
```

[recursive_tree.uid](../examples/recursive_tree.uid) 构造多层 Branch/Leaf 值，用递归 pattern 区分根节点并派生统一标签，还验证完整值的精确二级索引。相同机制可表达 `cause option Error` 的有限原因链，或 `Literal | All (list Rule) | Not Rule` 的规则 AST。

这些值没有对象身份、共享节点或循环边，并受 64 层值预算约束。当前查询可以在源码中写出已知深度的 pattern，不提供任意深度遍历、递归函数或 subtree 路径索引；需要频繁跨节点查询的任意图仍应拆表或交给应用代码。类型有效性、codec、migration 与 backup 规则见 [RFC 0001](rfc/0001-finite-recursive-adts.md)。

## 7. 生产标量：身份、时间、金额与 binary

现有场景用 `text` 表示 ID/hash、用 `int` 表示时间/金额，能验证 ADT 查询，却会把格式、单位与精度留给应用。生产 schema 需要在不削弱命名 ADT 的前提下把这些物理语义带到索引和协议边界。以下 schema 中 UUID、bytes、date、timestamp 与 duration 已是当前可执行语法；decimal 仍是 [RFC 0004](rfc/0004-production-scalars.md) 的后续目标：

```text
type Payment = {
  invoice_id uuid,
  issued_on date,
  received_at timestamp,
  retry_after duration,
  amount decimal 18 2,
  payload_hash bytes,
}

table payments Payment
  key invoice_id
```

它覆盖几类反复出现的操作：UUIDv4/v7 可作为分布式写入产生的稳定 key；带 offset 的事件时间在插入时规范为同一个 UTC instant；账单金额按固定 scale 精确求和并在溢出时整体失败；retry duration 保持单位；digest 以 bytes 做精确匹配，避免 hex 大小写造成两个逻辑 key。应用仍可用 `type InvoiceId = uuid`、`type Currency = CNY | USD` 和 `{amount decimal 18 2, currency Currency}` 保留领域约束。

[content_metadata.uid](../examples/content_metadata.uid) 已执行 UUID 主键、unique bytes digest、二进制子序列筛选和 octet 长度派生。UUID/bytes 也已覆盖 Rust serde 参数与结果、version 2 wire、redb 重开、backup/restore、cursor、聚合和显式 migration parse；任何普通、unique 或主键索引中的 bytes 值限制为 8192 octets，超限以 `E_INDEX_KEY_LIMIT` 原子失败，未索引值仍使用通用 16 MiB 上限。

[session_events.uid](../examples/session_events.uid) 用显式 offset timestamp 表达 session 打开/过期时间，用 exact duration 表达 retry delay，并派生下一次重试 instant 与 session lifetime。date 保持 civil day，不隐式转成午夜 instant；timestamp 输入规范为 UTC 微秒，不读取本地时区或 DST 数据；duration/timestamp 算术与 duration sum 全部 checked。

迁移场景必须验证已有 text/int 数据，而不是把 cast 隐藏在类型变化中。例如 text ID 通过 `uuid_parse old` 转换，旧 amount text 通过 `decimal_parse old 18 2` 转换；任一坏行会阻止 schema、数据、索引和 ledger 发布。wire、redb、backup、cursor 与旧版本兼容边界由 RFC 的 golden vectors 一起验收。

## 功能覆盖与优先级

| 应用需要 | 当前能力 | 缺口与任务 | v0.1 优先级 |
| --- | --- | --- | --- |
| 命名 sum/record/tuple/option/list 严格写入 | 已实现 | — | 已满足 |
| 有限自递归 sum/record 值 | 已实现直接自引用、有限性检查、match、精确索引、migration 和持久恢复 | 互递归、用户泛型、任意深度递归查询延后 | #81，核心切片 |
| 固定 record 的嵌套路径过滤/投影 | 已实现 | option/sum 不能直接穿透 | 已满足基础 |
| 按 sum/option constructor 筛选 | 已实现 unit、record、位置负载、record/tuple/sum/option 嵌套 pattern，以及同 constructor 多分支的完整覆盖分析 | — | #35，P0 |
| 派生普通值或从 ADT 分支归一结果 | 已实现 scalar/bool 普通 derive、递归 pattern，以及从 binding/typed arithmetic 构造 option/sum/record/tuple/list | — | #59，已满足 |
| 复用重复业务表达式 | 已实现查询局部常量、单/多参数非递归纯函数、有限推断、词法遮蔽和展开预算 | 泛型、高阶与递归函数延后 | #61，P0 |
| 多条件、标签和集合判断 | 已实现括号、not/and/or、比较、contains/length、any/all 与 is_some/is_none；filter、普通／match derive、typed set 和 migration conversion 共享这些 bool 结果 | 通用高阶函数延后 | #36/#100，已满足核心 |
| 可复现列表顺序与分页 | 复合 sort、范围 take、类型化索引访问计划、explain 与有界双向 cursor page 已实现 | 真正跨写入 snapshot 等待 #116；显式取消/streaming 见 #135 | #34/#16/#131/#132 |
| UUID、时间、定点数与 binary | RFC、兼容基础与 UUID/bytes 已完成；temporal 已接通语言、Rust、wire、redb、backup、cursor、索引、算术和 migration | 完成 #139 后继续 decimal | #115/#139–#140，M5 P1 |
| 参数化 key/time/user 输入 | 已实现 typed AST 参数、version 1 wire codec，以及 query/insert/upsert/update/delete 的 schema-aware prepared operation | option helper/元素谓词可继续扩展 | #10/#22/#36/#91 |
| 批量写入 typed row list | `insert many` 与 `upsert many` 已实现默认值、嵌套 ADT、输入内主键去重、整批主键／unique index 验证、稳定 RowId/returning/action 顺序和 memory/redb/TCP 原子提交 | 流式导入单独设计 | #89/#97，P1 核心 |
| 原子状态转换、upsert、delete | update/delete 已实现 filter/match/sort/take target、穷尽 ADT match assignment 与 typed simultaneous set；全部 DML 可 returning 完整行或投影；upsert 已实现按主键 insert/replace；它们维护约束、索引、affected rows、稳定 RowId 和 redb 增量键提交 | 多写者／skip-locked 不在当前单写模型内 | #15/#83/#85/#87 |
| count/sum/min/max 与分组 | 已实现 typed 空输入、命名数值、完整 ADT key、后续 stage 与有界资源 | distinct aggregate、window 和用户定义 aggregate 延后 | #60，P0 |
| schema evolution 与数据转换 | 已有显式 type/field/variant 演进、默认回填、typed conversion、全嵌套引用扫描及约束/索引维护 | 版本化 plan/apply/status、ledger 与 diff | #17–#19，P0/P1 |
| 持久提交、恢复和备份 | redb Engine、原子提交、完整性检查、进程退出恢复、备份还原与三条端到端升级恢复场景已实现 | 物理设备故障不在当前测试声明内 | #13/#14/#20/#74，P0 |

## 对查询语言的约束

1. pipeline stage 保持正交：filter 改变行，derive 增加列，select 选择列，sort 建立顺序，take 选择位置，aggregate 缩减行。不会因在 select 中出现 aggregate 而隐式改变行数。
2. sum/option 的分支字段只能经 match 解构，不能用“缺失就 null”的路径访问破坏类型。list 元素查询也必须有类型化谓词。
3. 所有字段、pattern、函数和参数在扫描前绑定；空表不会掩盖错误。schema revision 改变时 plan 重新绑定。
4. 没有 sort 就没有跨请求顺序承诺；分页查询以唯一键结束排序。offset range 适合小工作集，大页或频繁翻页后续增加显式 cursor，而不是暗中改变 `take`。
5. 时间、随机数、网络和文件不是查询表达式的隐含副作用。当前时间由参数传入；外部 I/O 留在应用层。
6. 核心版本不以 join、window、递归查询函数和高阶泛型换取表面覆盖率。有限自递归 ADT 只表示整行拥有的有限树；若一个场景主要依赖大规模关联、任意图遍历、任意 JSON 分析或 OLAP，应选择 SQLite/DuckDB/PostgreSQL 等系统。

实现顺序按用户可完成的工作流安排：#34–#36 与 #59–#61 已补齐列表读取、ADT 表达式、普通派生、基础汇总和查询局部纯函数；#11 已收口查询核心，#16 已补齐共享索引访问计划与 explain。#74 用任务队列、嵌套配置和 session/cache 走通持久重启、migration 与 backup/restore，#75、#70 和 #76 已收敛工作负载、日常体验与安装发布；#81 从 #25 中切出有限自递归 ADT，先补树形核心模型，再依据真实反馈决定互递归与泛型。

查询结果整理可使用 `derive {score = priority + bonus, urgent = score >= 10}` 顺序计算，或用 `select {id, label = match state {...}}` 同时派生与投影。事件、任务队列和递归树示例使用此形式；计算式 select 必须放在分页最终 sort 之前，覆盖主键后不能继续 page。
