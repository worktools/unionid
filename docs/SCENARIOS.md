# 实际场景与查询覆盖矩阵

状态：v0.1 场景契约，2026-09-06。本文从应用代码会保存和读取的数据出发，检验 ADT 与查询语言是否实用。当前可执行语法仍以 [LANGUAGE.md](LANGUAGE.md) 和 [QUERY.md](QUERY.md) 为准；标有 issue 的片段是目标语法。

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
  payload Payload
  state JobState
```

常见工作流：

- 查找可运行任务：解构 `Queued`，比较 `scheduled_at`，按 priority、时间和 id 排序后取一页，并把状态派生为统一的 text 标签。失败任务可直接用 `Failed {error = Network {message}, retry_at = Some at}` 解构嵌套失败原因和重试时间。当前完整示例见 [job_queue.uid](../examples/job_queue.uid)。
- 按主键读取任务：当前可用 `filter id == "job-a" | take 1`，持久模式由主键索引执行。
- 原子 claim：按 id 和旧状态筛选，把 `Queued` 改成 `Running` 并返回新值，属于 #15；状态解构与新值表达式复用 #35。
- 查询高优先级且带 `sync` 标签的任务：`filter priority >= 10 and contains tags "sync"` 已实现并进入可执行示例；再与 `filter match state` 组合即可限定状态。
- 按状态计数：需要 #11 的 group/aggregate；sum 分支归一为状态名需要 #35。

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
- 判断某个完整 header 或 validation issue 是否存在可用 `contains`；按元素字段写谓词仍需要 #36 的 `any/all`。
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

常见工作流包括筛选 `InvoicePaid` 金额、为不同 payload 派生摘要、列出下一批 Pending 事件、追加投递失败、统计来源和清理过期记录。当前 `filter match` 能筛选单 record 负载并在 condition 中组合布尔、比较和集合判断；派生摘要与嵌套模式由 #35 承接，失败集合的元素谓词由 #36 承接，状态更新与保留期删除由 #15 承接，统计由 #11 承接。

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

常见查询是列出全部 Conflict 并提取双方 change、查找 changes 中触及某路径的 Dirty workspace、按 `last_sync` 找未同步项，以及原子提交一次冲突解决。[sync_conflicts.uid](../examples/sync_conflicts.uid) 已用同一个 `Conflict` constructor 的三个互补嵌套分支完整覆盖 local change 并派生类型化标签；list `any` 和 option helper 仍由 #36 跟踪，条件更新由 #15 跟踪。

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

功能开关可以把规则声明为 list of sum，例如 `User text | Group text | Percentage int`。按 key 读取整个 typed flag 很合适；跨所有 flag 搜索任意嵌套规则依赖 #36，复杂规则求值更适合在应用代码完成。

## 功能覆盖与优先级

| 应用需要 | 当前能力 | 缺口与任务 | v0.1 优先级 |
| --- | --- | --- | --- |
| 命名 sum/record/tuple/option/list 严格写入 | 已实现 | — | 已满足 |
| 固定 record 的嵌套路径过滤/投影 | 已实现 | option/sum 不能直接穿透 | 已满足基础 |
| 按 sum/option constructor 筛选 | 已实现 unit、record、位置负载、record/tuple/sum/option 嵌套 pattern，以及同 constructor 多分支的完整覆盖分析 | prepared plan 的 schema revision 重绑定 | #35，P0 |
| 从 ADT 分支派生统一结果 | 已实现递归 pattern，以及从 binding/typed arithmetic 构造 option/sum/record/tuple/list | 通用函数表达式 | #36，P0 |
| 多条件、标签和集合判断 | 已实现括号、not/and/or、比较、contains/length | any/all 元素谓词与 option helper | #36，P0 |
| 可复现列表顺序与分页 | 复合 sort、范围 take 已实现 | 索引辅助与大结果预算 | #34 → #16 |
| 参数化 key/time/user 输入 | 未实现 | typed params、schema revision 重绑定 | #10/#22，P0/P1 |
| 原子状态转换、upsert、delete | update/delete 已实现 filter/match target 与 typed simultaneous set；upsert 已实现按主键 insert/replace；三者维护约束、索引、affected rows、稳定 RowId 和 redb 增量键提交 | 扩大工作集时直接生成 mutation set | #15，P0 |
| count/sum/min/max 与分组 | 未实现 | aggregate/group | #11，P0 |
| schema evolution 与数据转换 | 已有显式 type/field/variant 演进、默认回填、typed conversion、全嵌套引用扫描及约束/索引维护 | 版本化 plan/apply/status、ledger 与 diff | #17–#19，P0/P1 |
| 持久提交、恢复和备份 | redb Engine、原子提交、完整性检查和进程退出恢复已实现 | 设备故障矩阵与备份还原 | #13/#14/#20，P0 |

## 对查询语言的约束

1. pipeline stage 保持正交：filter 改变行，derive 增加列，select 选择列，sort 建立顺序，take 选择位置，aggregate 缩减行。不会因在 select 中出现 aggregate 而隐式改变行数。
2. sum/option 的分支字段只能经 match 解构，不能用“缺失就 null”的路径访问破坏类型。list 元素查询也必须有类型化谓词。
3. 所有字段、pattern、函数和参数在扫描前绑定；空表不会掩盖错误。schema revision 改变时 plan 重新绑定。
4. 没有 sort 就没有跨请求顺序承诺；分页查询以唯一键结束排序。offset range 适合小工作集，大页或频繁翻页后续增加显式 cursor，而不是暗中改变 `take`。
5. 时间、随机数、网络和文件不是查询表达式的隐含副作用。当前时间由参数传入；外部 I/O 留在应用层。
6. v0.1 不以 join、window、递归、高阶泛型换取表面覆盖率。若一个场景主要依赖大规模关联、任意 JSON 分析或 OLAP，应选择 SQLite/DuckDB/PostgreSQL 等系统。

实现顺序按用户可完成的工作流安排：先完成 #34 列表读取，再完成 #35/#36 的 ADT 表达式，随后以 #13–#15 交付持久的状态修改；#11 的基础汇总可以与读取能力并行收敛，最终由 #24 用本页场景验收。
