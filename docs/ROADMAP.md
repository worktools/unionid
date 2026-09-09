# unionid 路线图

规划日期：2026-09-09。GitHub 使用总览、分阶段具体任务和里程碑维护计划；实施记录见 [开发记录](DEVELOPMENT.md)。后续完成状态以 GitHub 为准，本文只提供导航和依赖，不维护第二套进度。

总览：[#1](https://github.com/worktools/unionid/issues/1) · [全部 Issues](https://github.com/worktools/unionid/issues) · [里程碑](https://github.com/worktools/unionid/milestones)

[当前语言](LANGUAGE.md)和[查询参考](QUERY.md)描述可执行范围；[结构化查询语法 RFC](rfc/0005-structured-prql-query-syntax.md)收敛 PRQL 风格的 delimiter、field set、match expression 与 group inner pipeline；[实际场景与覆盖矩阵](SCENARIOS.md)用任务队列、配置、事件、同步和 key/value 工作流检验查询实用性；[生产标量 RFC](rfc/0004-production-scalars.md)冻结 UUID、时间、decimal、bytes 与格式升级边界；[Schema 身份与演进契约](SCHEMA.md)定义稳定 ID、revision/hash 和兼容规则；[redb 持久模式](STORAGE.md)记录事务入口与格式边界；[设计草案](DESIGN.md)说明完整目标和取舍；[原型审计](PROTOTYPE-AUDIT.md)保留早期原型的验证结果与问题证据。

v0.1.0 已通过 GitHub Actions 发布 crate、原生包和 GitHub Release，M0–M4 作为已完成历史保留。[M5 总览 #111](https://github.com/worktools/unionid/issues/111) 的核心范围也已完成。M6 的增量 mutation、typed ordered composite index、range/order/page seek 和 10k/100k 复验已合并；后续工作聚焦 [M7 总览 #177](https://github.com/worktools/unionid/issues/177) 的有界常驻状态与可恢复维护。#118 与 #120 保留为由真实需求触发的独立 P2 探索；join、window 和分布式不属于当前版本范围。

用户已明确语言方向：类型定义与查询都采用 PRQL 风格，不使用没有意义的语句末尾分号；花括号、圆括号、方括号和逗号在能明确结构、层级或 precedence 时正常使用。本轮草案采用 `field type`、`option text`／`list text`、结构化声明与换行 pipeline；具体布局和语句边界由 #2／#8 验证，不沿用 TypeScript 风格的密集字段注解或逐行 `|>`。

## 阶段与验收

| 阶段 | 交付内容 | 退出条件 |
| --- | --- | --- |
| [M0 · 设计收敛与正确性基线](https://github.com/worktools/unionid/milestone/1) | 场景/语言契约、回归基线、共享引擎、存储选型、类型演进规则 | 关键语义可测试，存储 ADR 选定一个后端 |
| [M1 · ADT 与查询语言预览](https://github.com/worktools/unionid/milestone/2) | 命名 ADT、嵌套 record/tuple、Option/List、match、typed pipeline、严格插入 | 三个场景可在内存模式走通；这是语言预览 |
| [M2 · 可靠读写与持久化](https://github.com/worktools/unionid/milestone/3) | 原子持久提交、恢复、主键/CRUD/upsert/批次、索引 | 提交/恢复边界经过故障验证，有无索引结果一致 |
| [M3 · Schema migration 与数据生命周期](https://github.com/worktools/unionid/milestone/4) | Schema/数据转换、runner、diff、备份还原、旧格式导入 | 真实旧库可升级，失败迁移不留下半个新 schema |
| [M4 · v0.1 日常可用版本](https://github.com/worktools/unionid/milestone/5) | CLI/REPL、Rust API、协议、服务限额、基准与发布 | 日常操作、恢复和升级均通过端到端验收 |
| [M5 · 生产边界与应用体验](https://github.com/worktools/unionid/milestone/6) | 只读边界、幂等写入、游标分页、生产标量、并发读快照与 CLI 诊断 | 核心风险有显式协议和故障测试，应用无需依赖隐式约定 |
| [M6 · 增量执行与有序访问](https://github.com/worktools/unionid/milestone/7) | 增量 mutation 候选状态、typed ordered composite index、range/page seek 与容量复验 | 小写集工作量不随完整数据库复制增长，常见有序读取有可验证的有界访问路径 |
| [M7 · 有界常驻状态与可恢复维护](https://github.com/worktools/unionid/milestone/8) | storage phase 证据、按需 typed row access、generation migration 与容量复验 | bounded read 不加载全表，维护失败只暴露完整旧/新 generation |

P0 表示所属阶段的正确性或契约门槛；P1 是重要可用性能力；P2 为后续语言探索。里程碑不填写未经验证的工期承诺。

## 任务与依赖

每个 GitHub issue 已包含目标、实现范围、前置依赖与可检查的验收清单。依赖链接表示完整验收的前置条件；允许先实现可运行子集来验证设计，但不能据此跳过阶段验收。实现中若改变范围，同步 issue 和关联设计。

### M0 · 设计收敛与正确性基线

| Issue | 任务 | 优先级 | 前置依赖 |
| --- | --- | --- | --- |
| [#2](https://github.com/worktools/unionid/issues/2) | [设计] 冻结 v0.1 产品边界与语言语义 | P0 | 无 |
| [#3](https://github.com/worktools/unionid/issues/3) | [质量] 建立原型回归基线与 CI | P0 | 无 |
| [#4](https://github.com/worktools/unionid/issues/4) | [修复] 统一数值比较与索引相等语义 | P0 | [#3](https://github.com/worktools/unionid/issues/3) |
| [#5](https://github.com/worktools/unionid/issues/5) | [核心] 提取共享 Engine 库与本地执行入口 | P0 | [#2](https://github.com/worktools/unionid/issues/2)、[#3](https://github.com/worktools/unionid/issues/3) |
| [#6](https://github.com/worktools/unionid/issues/6) | [设计] 验证并选定持久化后端；结论见 [ADR 0001](adr/0001-redb-storage.md) | P0 | [#3](https://github.com/worktools/unionid/issues/3)、[#5](https://github.com/worktools/unionid/issues/5) |
| [#7](https://github.com/worktools/unionid/issues/7) | [设计] 定义类型身份、Schema 版本与演进规则 | P0 | [#2](https://github.com/worktools/unionid/issues/2) |
| [#28](https://github.com/worktools/unionid/issues/28) | [文档] 整理 v0.1 查询语言规范与可执行示例 | P0 | [#2](https://github.com/worktools/unionid/issues/2) 的可执行规范子任务 |

### M1 · ADT 与查询语言预览

| Issue | 任务 | 优先级 | 前置依赖 |
| --- | --- | --- | --- |
| [#8](https://github.com/worktools/unionid/issues/8) | [语言] 实现 Lexer、AST、多行脚本与源码诊断 | P0 | [#2](https://github.com/worktools/unionid/issues/2)、[#5](https://github.com/worktools/unionid/issues/5) |
| [#9](https://github.com/worktools/unionid/issues/9) | [类型] 实现命名 ADT、嵌套积类型与 Catalog | P0 | [#5](https://github.com/worktools/unionid/issues/5)、[#7](https://github.com/worktools/unionid/issues/7) |
| [#10](https://github.com/worktools/unionid/issues/10) | [语言] 实现类型检查、表达式与穷尽模式匹配 | P0 | [#4](https://github.com/worktools/unionid/issues/4)、[#8](https://github.com/worktools/unionid/issues/8)、[#9](https://github.com/worktools/unionid/issues/9) |
| [#11](https://github.com/worktools/unionid/issues/11) | [查询] 实现可组合 Pipeline、纯函数与基础汇总 | P0 | [#10](https://github.com/worktools/unionid/issues/10) |
| [#12](https://github.com/worktools/unionid/issues/12) | [核心] 打通类型声明、建表与严格插入的内存切片 | P0 | [#9](https://github.com/worktools/unionid/issues/9)、[#10](https://github.com/worktools/unionid/issues/10)、[#11](https://github.com/worktools/unionid/issues/11) |
| [#34](https://github.com/worktools/unionid/issues/34) | [查询] 实现复合排序与范围分页 | P0 | [#11](https://github.com/worktools/unionid/issues/11) 的可独立子任务 |
| [#35](https://github.com/worktools/unionid/issues/35) | [查询] 实现 ADT 解构表达式与类型化派生 | P0 | [#10](https://github.com/worktools/unionid/issues/10)、[#11](https://github.com/worktools/unionid/issues/11) |
| [#36](https://github.com/worktools/unionid/issues/36) | [查询] 实现布尔表达式与集合函数 | P0 | [#10](https://github.com/worktools/unionid/issues/10)、[#11](https://github.com/worktools/unionid/issues/11) |
| [#59](https://github.com/worktools/unionid/issues/59) | [查询] 普通类型化派生列 | P0 | [#36](https://github.com/worktools/unionid/issues/36) |
| [#60](https://github.com/worktools/unionid/issues/60) | [查询] 分组与基础汇总 | P0 | [#36](https://github.com/worktools/unionid/issues/36)、[#59](https://github.com/worktools/unionid/issues/59) |
| [#61](https://github.com/worktools/unionid/issues/61) | [语言] 查询局部 let 与非递归纯函数 | P0 | [#59](https://github.com/worktools/unionid/issues/59)、[#60](https://github.com/worktools/unionid/issues/60) |

### M2 · 可靠读写与持久化

| Issue | 任务 | 优先级 | 前置依赖 |
| --- | --- | --- | --- |
| [#13](https://github.com/worktools/unionid/issues/13) | [存储] 实现持久原子提交与一致的错误语义 | P0 | [#6](https://github.com/worktools/unionid/issues/6)、[#9](https://github.com/worktools/unionid/issues/9)、[#12](https://github.com/worktools/unionid/issues/12) |
| [#14](https://github.com/worktools/unionid/issues/14) | [存储] 完善崩溃恢复、打开锁与格式校验 | P0 | [#13](https://github.com/worktools/unionid/issues/13) |
| [#15](https://github.com/worktools/unionid/issues/15) | [读写] 实现主键、CRUD、Upsert 与原子批次 | P0 | [#10](https://github.com/worktools/unionid/issues/10)、[#12](https://github.com/worktools/unionid/issues/12)、[#13](https://github.com/worktools/unionid/issues/13) |
| [#16](https://github.com/worktools/unionid/issues/16) | [查询] 实现类型化索引与 Explain | P1 | [#4](https://github.com/worktools/unionid/issues/4)、[#11](https://github.com/worktools/unionid/issues/11)、[#15](https://github.com/worktools/unionid/issues/15) |

### M3 · Schema migration 与数据生命周期

| Issue | 任务 | 优先级 | 前置依赖 |
| --- | --- | --- | --- |
| [#17](https://github.com/worktools/unionid/issues/17) | [迁移] 实现 Schema 变更与 ADT 数据转换 | P0 | [#7](https://github.com/worktools/unionid/issues/7)、[#10](https://github.com/worktools/unionid/issues/10)、[#14](https://github.com/worktools/unionid/issues/14)、[#15](https://github.com/worktools/unionid/issues/15) |
| [#18](https://github.com/worktools/unionid/issues/18) | [迁移] 实现版本化 Migration Runner 与 Ledger | P0 | [#14](https://github.com/worktools/unionid/issues/14)、[#17](https://github.com/worktools/unionid/issues/17) |
| [#19](https://github.com/worktools/unionid/issues/19) | [迁移] 实现声明式 Schema Diff 与可检查的迁移草稿 | P1 | [#17](https://github.com/worktools/unionid/issues/17)、[#18](https://github.com/worktools/unionid/issues/18) |
| [#20](https://github.com/worktools/unionid/issues/20) | [数据] 实现备份还原、格式升级与旧原型导入 | P0 | [#14](https://github.com/worktools/unionid/issues/14)、[#18](https://github.com/worktools/unionid/issues/18) |

### M4 · v0.1 日常可用版本

| Issue | 任务 | 优先级 | 前置依赖 |
| --- | --- | --- | --- |
| [#21](https://github.com/worktools/unionid/issues/21) | [体验] 完善本地 CLI、REPL、脚本与输出 | P1 | [#8](https://github.com/worktools/unionid/issues/8)、[#11](https://github.com/worktools/unionid/issues/11)、[#14](https://github.com/worktools/unionid/issues/14)、[#18](https://github.com/worktools/unionid/issues/18) |
| [#22](https://github.com/worktools/unionid/issues/22) | [接口] 定义版本化协议与 Rust 参数化 API | P1 | [#5](https://github.com/worktools/unionid/issues/5)、[#10](https://github.com/worktools/unionid/issues/10)、[#13](https://github.com/worktools/unionid/issues/13)、[#18](https://github.com/worktools/unionid/issues/18) |
| [#23](https://github.com/worktools/unionid/issues/23) | [服务] 限制资源、协调并发与优雅关闭 | P1 | [#14](https://github.com/worktools/unionid/issues/14)、[#15](https://github.com/worktools/unionid/issues/15)、[#22](https://github.com/worktools/unionid/issues/22) |
| [#24](https://github.com/worktools/unionid/issues/24) | [发布] 以真实场景、故障矩阵和基准验收 v0.1 | P0 | [#3](https://github.com/worktools/unionid/issues/3)、[#4](https://github.com/worktools/unionid/issues/4)、[#12](https://github.com/worktools/unionid/issues/12)、[#14](https://github.com/worktools/unionid/issues/14)、[#15](https://github.com/worktools/unionid/issues/15)、[#16](https://github.com/worktools/unionid/issues/16)、[#18](https://github.com/worktools/unionid/issues/18)、[#19](https://github.com/worktools/unionid/issues/19)、[#20](https://github.com/worktools/unionid/issues/20)、[#21](https://github.com/worktools/unionid/issues/21)、[#22](https://github.com/worktools/unionid/issues/22)、[#23](https://github.com/worktools/unionid/issues/23) |
| [#74](https://github.com/worktools/unionid/issues/74) | [发布] 验证端到端升级恢复场景 | P0 | [#24](https://github.com/worktools/unionid/issues/24) 的核心正确性子任务 |
| [#75](https://github.com/worktools/unionid/issues/75) | [发布] 测量代表性查询、写入与迁移成本 | P0 | [#74](https://github.com/worktools/unionid/issues/74) |
| [#76](https://github.com/worktools/unionid/issues/76) | [发布] 生成可校验产物与五分钟教程 | P0 | [#70](https://github.com/worktools/unionid/issues/70)、[#75](https://github.com/worktools/unionid/issues/75) |
| [#101](https://github.com/worktools/unionid/issues/101) | [协议] 统一源码、Rust serde ADT 与 HTTP 数据边界 | P1 | [#22](https://github.com/worktools/unionid/issues/22)、[#95](https://github.com/worktools/unionid/issues/95) |
| [#105](https://github.com/worktools/unionid/issues/105) | [发布] 升级 GitHub Actions 到 Node 24 版本 | P0 | [#76](https://github.com/worktools/unionid/issues/76)、[#101](https://github.com/worktools/unionid/issues/101) |
| [#108](https://github.com/worktools/unionid/issues/108) | [发布] 通过 GitHub Actions 发布 unionid crate | P0 | [#105](https://github.com/worktools/unionid/issues/105) |
| [#103](https://github.com/worktools/unionid/issues/103) | [发布] 发布可校验的 v0.1.0 产物 | P0 | [#24](https://github.com/worktools/unionid/issues/24)、[#76](https://github.com/worktools/unionid/issues/76)、[#101](https://github.com/worktools/unionid/issues/101)、[#105](https://github.com/worktools/unionid/issues/105)、[#108](https://github.com/worktools/unionid/issues/108) |

### M5 · 生产边界与应用体验

| Issue | 任务 | 优先级 | 前置依赖 |
| --- | --- | --- | --- |
| [#111](https://github.com/worktools/unionid/issues/111) | [路线图] M5 总览与验收顺序 | P0 | v0.1.0 发布基线 |
| [#112](https://github.com/worktools/unionid/issues/112) | [核心] 只读执行边界与可观测状态 | P0 | 无；首个实现切片 |
| [#113](https://github.com/worktools/unionid/issues/113) | [协议] 持久幂等写入 receipt | P0 | [#112](https://github.com/worktools/unionid/issues/112) |
| [#124](https://github.com/worktools/unionid/issues/124) | [设计] 幂等 effect、digest、receipt 与清理 RFC | P0 | [#112](https://github.com/worktools/unionid/issues/112)；[#113](https://github.com/worktools/unionid/issues/113) 的设计切片 |
| [#125](https://github.com/worktools/unionid/issues/125) | [存储] Engine/redb/backup 原子持久回执 | P0 | [#124](https://github.com/worktools/unionid/issues/124) |
| [#126](https://github.com/worktools/unionid/issues/126) | [协议] version 1、清理与 HTTP 丢响应场景 | P0 | [#125](https://github.com/worktools/unionid/issues/125) |
| [#114](https://github.com/worktools/unionid/issues/114) | [查询] 稳定 cursor 分页与取消 | P1 | [#113](https://github.com/worktools/unionid/issues/113) 的请求身份契约 |
| [#130](https://github.com/worktools/unionid/issues/130) | [设计] 冻结稳定 cursor、快照与取消契约 | P1 | [#114](https://github.com/worktools/unionid/issues/114) 的设计子任务；已由 #133 完成 |
| [#131](https://github.com/worktools/unionid/issues/131) | [查询] 有界 keyset page 与 version 1 cursor | P1 | [#130](https://github.com/worktools/unionid/issues/130) |
| [#132](https://github.com/worktools/unionid/issues/132) | [接口] Rust/TCP/HTTP 分页与取消旅程 | P1 | [#131](https://github.com/worktools/unionid/issues/131) |
| [#115](https://github.com/worktools/unionid/issues/115) | [类型] 生产标量契约 | 已完成 | [#147](https://github.com/worktools/unionid/pull/147)–[#151](https://github.com/worktools/unionid/pull/151) |
| [#137](https://github.com/worktools/unionid/issues/137) | [类型] 生产标量兼容与 codec 基础 | 已完成 | [#147](https://github.com/worktools/unionid/pull/147)、[#148](https://github.com/worktools/unionid/pull/148) |
| [#138](https://github.com/worktools/unionid/issues/138) | [类型] UUID 与 bytes 端到端能力 | 已完成 | [#149](https://github.com/worktools/unionid/pull/149) |
| [#139](https://github.com/worktools/unionid/issues/139) | [类型] date、timestamp 与 duration | 已完成 | [#150](https://github.com/worktools/unionid/pull/150) |
| [#140](https://github.com/worktools/unionid/issues/140) | [类型] 固定精度 decimal | 已完成 | [#151](https://github.com/worktools/unionid/pull/151) |
| [#116](https://github.com/worktools/unionid/issues/116) | [并发] 一致并发读快照 | 已完成 | [RFC 0006](rfc/0006-consistent-read-snapshots.md) |
| [#117](https://github.com/worktools/unionid/issues/117) | [体验] CLI 版本诊断与结构化错误 | 已完成 | version 1 JSON 与退出码契约 |
| [#135](https://github.com/worktools/unionid/issues/135) | [服务] 显式取消与有背压的流式读取 | 已完成 | [#155](https://github.com/worktools/unionid/issues/155) → [#156](https://github.com/worktools/unionid/issues/156) → [#157](https://github.com/worktools/unionid/issues/157) |
| [#155](https://github.com/worktools/unionid/issues/155) | [设计] 取消与 streaming 契约 RFC | 已完成 | [RFC 0007](rfc/0007-cancellable-backpressured-streams.md) |
| [#156](https://github.com/worktools/unionid/issues/156) | [并发] 有界 operation registry 与只读取消 | 已完成 | PR #159 |
| [#157](https://github.com/worktools/unionid/issues/157) | [接口] TCP/HTTP NDJSON 背压流 | 已完成 | [#156](https://github.com/worktools/unionid/issues/156)；PR #160 |
| [#122](https://github.com/worktools/unionid/issues/122) | [文档] 中英双语 README 与 ADT/query 产品入口 | P1 | 使用已发布 v0.1.0 和 [#112](https://github.com/worktools/unionid/issues/112) 的真实入口 |
| [#119](https://github.com/worktools/unionid/issues/119) | [语言] PRQL 风格结构化查询语法 RFC | P1 | 当前 typed IR 与持久源码兼容契约 |
| [#142](https://github.com/worktools/unionid/issues/142) | [语言] 结构化 delimiter 与 canonical formatter | P1 | [#119](https://github.com/worktools/unionid/issues/119) RFC；[#143](https://github.com/worktools/unionid/issues/143) 和 [#139](https://github.com/worktools/unionid/issues/139) 的语法前置 |
| [#143](https://github.com/worktools/unionid/issues/143) | [查询] field-set transforms 与 computed select | P1 | [#142](https://github.com/worktools/unionid/issues/142) |

### M6 · 增量执行与有序访问

| Issue | 任务 | 优先级 | 前置依赖 |
| --- | --- | --- | --- |
| [#167](https://github.com/worktools/unionid/issues/167) | [路线图] M6 总览与验收顺序 | P0 | M5 核心完成 |
| [#162](https://github.com/worktools/unionid/issues/162) | [设计] 增量候选状态与原子发布 RFC | 已完成 | [RFC 0008](rfc/0008-incremental-candidate-state.md)；[#168](https://github.com/worktools/unionid/pull/168) |
| [#163](https://github.com/worktools/unionid/issues/163) | [核心] 增量 row-only DML 候选状态 | 已完成 | [#162](https://github.com/worktools/unionid/issues/162)；[#169](https://github.com/worktools/unionid/pull/169) |
| [#164](https://github.com/worktools/unionid/issues/164) | [设计] 有序复合索引与范围访问 RFC | 已完成 | [RFC 0009](rfc/0009-ordered-composite-indexes.md)；[#170](https://github.com/worktools/unionid/pull/170) |
| [#165](https://github.com/worktools/unionid/issues/165) | [查询] 有序复合索引、range/ordered scan 与 page seek | 已合并 | [#174](https://github.com/worktools/unionid/pull/174)；[#175](https://github.com/worktools/unionid/pull/175) |
| [#166](https://github.com/worktools/unionid/issues/166) | [质量] 复验 10k/100k 写入与有序访问成本 | 已合并 | [#163](https://github.com/worktools/unionid/issues/163)、[#165](https://github.com/worktools/unionid/issues/165) |

### M7 · 有界常驻状态与可恢复维护

| Issue | 任务 | 优先级 | 前置依赖 |
| --- | --- | --- | --- |
| [#177](https://github.com/worktools/unionid/issues/177) | [路线图] M7 总览与阶段验收 | P0 | M6 容量证据 |
| [#178](https://github.com/worktools/unionid/issues/178) | [质量] 分段观测 open 与 full-rebuild migration | 已合并 | [#166](https://github.com/worktools/unionid/issues/166) |
| [#179](https://github.com/worktools/unionid/issues/179) | [设计] 冻结 bounded resident state 与 maintenance generation | 已合并 | [RFC 0010](rfc/0010-bounded-resident-state-and-maintenance-generations.md)、[#178](https://github.com/worktools/unionid/issues/178) |
| [#181](https://github.com/worktools/unionid/issues/181) | [核心] 统一 typed row source 与 committed view | 已合并 | [#179](https://github.com/worktools/unionid/issues/179) |
| [#182](https://github.com/worktools/unionid/issues/182) | [存储] Legacy0 有界 redb read 与 row cache | 已合并 | [#181](https://github.com/worktools/unionid/issues/181) |
| [#183](https://github.com/worktools/unionid/issues/183) | [执行] 有界 full pipeline、check 与 backup | 已合并 | [#182](https://github.com/worktools/unionid/issues/182) |
| [#184](https://github.com/worktools/unionid/issues/184) | [存储] format-6 generation envelope 与升级 | 已合并 | [#195](https://github.com/worktools/unionid/pull/195) |
| [#185](https://github.com/worktools/unionid/issues/185) | [迁移] 可恢复 shadow generation 与原子 cutover | P0 | [#183](https://github.com/worktools/unionid/issues/183)、[#184](https://github.com/worktools/unionid/issues/184) |
| [#186](https://github.com/worktools/unionid/issues/186) | [质量] M7 接口旅程与容量复验 | P1 | [#182](https://github.com/worktools/unionid/issues/182)–[#185](https://github.com/worktools/unionid/issues/185) |

### 独立 P2 探索

| Issue | 任务 | 推进条件 |
| --- | --- | --- |
| [#118](https://github.com/worktools/unionid/issues/118) | [语言] 用户泛型与互递归 ADT | 出现真实 schema 复用需求并能定义有限性、身份和 codec 预算 |
| [#120](https://github.com/worktools/unionid/issues/120) | [查询] 可复用命名查询 | 至少两个真实调用方需要共享同一参数化 typed pipeline |
| [#192](https://github.com/worktools/unionid/issues/192) | [集成] 宿主语言 typed 数据适配 | 真实调用方证明现有 versioned protocol 或 Rust API 无法满足，并列出具体 API 缺口 |

## M5 收口状态

#112 的显式只读边界、#113/#124–#126 的 exactly-once effect、#130–#132 的有界分页与完整 Rust/TCP/HTTP 旅程、#142–#143 的结构化语法与字段集、#115/#137–#140 的全部生产标量、#116 的一致并发读快照、#117 的机器可读 CLI 诊断，以及 #135/#155–#157 的显式取消和有背压 NDJSON stream 均已完成。#118 与 #120 已移出 M5，继续作为独立 P2 探索，不阻塞收口。

## 当前执行顺序

任务 #162／RFC 0008 与 #163/#169 已完成增量 DML。#164/#170 与 #171 已冻结并实现全部有限 ADT 的 typed total order；#172/#174 实现复合索引格式与升级，#173/#175 实现 equality-prefix range、index order 与 page seek，#166/#176 保存并核验 M6 的 10k/100k 原始样本。#178/#180 已完成 open/full rebuild 分阶段观测，#179/[RFC 0010](rfc/0010-bounded-resident-state-and-maintenance-generations.md) 据此冻结 M7 架构。#181/#188 建立共享 source seam，#182/#189 实现 format-5 Legacy0 bounded open、durable cursor、MVCC committed view 与 32 MiB cache，#183/#190 完成 bounded full pipeline/check/backup，#184 接入 format-6 generation envelope，#185 实现 resumable migration。#186 以真实接口矩阵和 10k/100k open/query/write/check/shadow-migration 原始样本完成 M7 复验，见 [验收记录](benchmarks/m7-acceptance-2026-09-10.md)。

## 维护约定

- 总览 issue 跟踪各任务；一个任务可拆成多个小 PR，关闭时附测试/验证证据。
- PR 引用对应 issue；语言或编码变化同步更新规范、示例、兼容说明和回归用例。
- 必须通过 cargo check；根据变更补充类型/执行/持久化/migration 测试，不能以正常重启替代崩溃恢复验证。
- 引擎格式升级与应用 schema migration 分开处理。未知格式拒绝或显式转换，保留原始数据。
- M1 只作为内存语言预览；M2 达到持久性门槛后再推荐保存真实数据；M3/M4 完成后发布日常可用版本。
