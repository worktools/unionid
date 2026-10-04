## 中文说明

说明问题、最终行为、范围与验证结果。

### 架构与改动位置

展示一张可直接阅读的 Mermaid 图：从[架构地图](https://github.com/worktools/unionid/blob/main/docs/ARCHITECTURE.md)保留相关上下游，标出本次修改的节点或箭头。复杂 PR 展示全图；局部 PR 展示对应使用链路。节点标记 `【修改】`，必要时标记 `【受影响】`，不能只靠颜色。图下说明 1–3 个关键位置：职责 → 文件 → 行为变化 → 验证。仅文档或流程修改可画文档/审查链路，明确是否影响运行时。

- [ ] 新特性已加入 `cargo test --locked --test cross_feature` 的相关组合场景，或在正文说明不适用的原因。

## English Description

Describe the problem, resulting behavior, scope and validation results.

### Architecture and change locations

Include a readable Mermaid diagram with relevant upstream/downstream context from the [architecture map](https://github.com/worktools/unionid/blob/main/docs/ARCHITECTURE.md). Show the full map for complex changes or the relevant user journey for a local change. Label changed nodes/edges `[Changed]` and, when useful, `[Affected]`; color alone is insufficient. Below the diagram, connect 1–3 key responsibilities to files, behavior changes and validation. Documentation/process changes may show their own journey, stating any runtime impact. One diagram with bilingual labels can serve both descriptions.

- [ ] New features extend the relevant combination scenario in `cargo test --locked --test cross_feature`, or the description explains why this does not apply.
