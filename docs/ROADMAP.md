# unionid 路线图

规划日期：2026-09-06。已在 GitHub 创建 1 个总览、28 个具体任务和 5 个里程碑；第一轮语言预览已开始实施，见 [开发记录](DEVELOPMENT.md)。后续完成状态以 GitHub 为准，本文只提供导航和依赖，不维护第二套进度。

总览：[#1](https://github.com/worktools/unionid/issues/1) · [全部 Issues](https://github.com/worktools/unionid/issues) · [里程碑](https://github.com/worktools/unionid/milestones)

[当前语言](LANGUAGE.md)和[查询参考](QUERY.md)描述可执行范围；[实际场景与覆盖矩阵](SCENARIOS.md)用任务队列、配置、事件、同步和 key/value 工作流检验查询实用性；[Schema 身份与演进契约](SCHEMA.md)定义稳定 ID、revision/hash 和兼容规则；[redb 持久模式](STORAGE.md)记录事务入口与格式边界；[设计草案](DESIGN.md)说明完整目标和取舍；[原型审计](PROTOTYPE-AUDIT.md)保留早期原型的验证结果与问题证据。

用户已明确语言方向：类型定义与查询都采用 PRQL 风格，无分号、减少标点。本轮草案采用 `field type`、`option text`／`list text`、缩进式声明与换行 pipeline；具体布局和语句边界由 #2／#8 验证，不再沿用 TypeScript 风格字段注解或逐行 `|>`。

## 阶段与验收

| 阶段 | 交付内容 | 退出条件 |
| --- | --- | --- |
| [M0 · 设计收敛与正确性基线](https://github.com/worktools/unionid/milestone/1) | 场景/语言契约、回归基线、共享引擎、存储选型、类型演进规则 | 关键语义可测试，存储 ADR 选定一个后端 |
| [M1 · ADT 与查询语言预览](https://github.com/worktools/unionid/milestone/2) | 命名 ADT、嵌套 record/tuple、Option/List、match、typed pipeline、严格插入 | 三个场景可在内存模式走通；这是语言预览 |
| [M2 · 可靠读写与持久化](https://github.com/worktools/unionid/milestone/3) | 原子持久提交、恢复、主键/CRUD/upsert/批次、索引 | 提交/恢复边界经过故障验证，有无索引结果一致 |
| [M3 · Schema migration 与数据生命周期](https://github.com/worktools/unionid/milestone/4) | Schema/数据转换、runner、diff、备份还原、旧格式导入 | 真实旧库可升级，失败迁移不留下半个新 schema |
| [M4 · v0.1 日常可用版本](https://github.com/worktools/unionid/milestone/5) | CLI/REPL、Rust API、协议、服务限额、基准与发布 | 日常操作、恢复和升级均通过端到端验收 |

P0 表示所属阶段的正确性、契约或发布门槛；P1 仍属于 v0.1 范围；P2 为后续探索。里程碑不填写未经验证的工期承诺，先完成 M0 后再按范围和实际速度估算。

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

### 后续探索

[#25](https://github.com/worktools/unionid/issues/25)：用户定义泛型与递归 ADT、模式查询简写、函数组合。P2，不阻塞 v0.1；先收集首版实际用例。

## 建议从哪里开始

[#3](https://github.com/worktools/unionid/issues/3)、[#4](https://github.com/worktools/unionid/issues/4) 和 [#5](https://github.com/worktools/unionid/issues/5) 已完成；实时状态和证据仍以 GitHub 为准。当前执行顺序是：

1. [#6](https://github.com/worktools/unionid/issues/6)、[#7](https://github.com/worktools/unionid/issues/7) 与 [#28](https://github.com/worktools/unionid/issues/28) 已完成；继续用当前可执行规范、示例和实际反馈收敛 [#2](https://github.com/worktools/unionid/issues/2)。
2. [#13](https://github.com/worktools/unionid/issues/13) 与 [#14](https://github.com/worktools/unionid/issues/14) 已交付 redb 原子提交、进程退出恢复、打开锁和格式校验子集，持久数据使用稳定 RowId；两项继续跟踪设备故障与恢复边界。
3. [#9](https://github.com/worktools/unionid/issues/9)、[#12](https://github.com/worktools/unionid/issues/12)、[#34](https://github.com/worktools/unionid/issues/34) 与 [#35](https://github.com/worktools/unionid/issues/35) 已完成；[#36](https://github.com/worktools/unionid/issues/36) 继续跟踪 option helper、元素谓词与通用函数。
4. [#15](https://github.com/worktools/unionid/issues/15)、[#17](https://github.com/worktools/unionid/issues/17)–[#20](https://github.com/worktools/unionid/issues/20) 已完成，M3 核心链路闭合。[#22](https://github.com/worktools/unionid/issues/22) 已实现 version 1 协议、typed 参数与 schema-aware prepared query；M4 下一步处理 #21 的日常 CLI 和 #23 的服务预算，再进入 #24 发布验收。

类型演进规则刻意放在 M0，避免 migration 被当作事后附加；完整 migration 执行要等原子存储与 DML 成熟。

## 维护约定

- 总览 issue 跟踪各任务；一个任务可拆成多个小 PR，关闭时附测试/验证证据。
- PR 引用对应 issue；语言或编码变化同步更新规范、示例、兼容说明和回归用例。
- 必须通过 cargo check；根据变更补充类型/执行/持久化/migration 测试，不能以正常重启替代崩溃恢复验证。
- 引擎格式升级与应用 schema migration 分开处理。未知格式拒绝或显式转换，保留原始数据。
- M1 只作为内存语言预览；M2 达到持久性门槛后再推荐保存真实数据；M3/M4 完成后发布日常可用版本。
