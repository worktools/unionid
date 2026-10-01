# 发布 Discussion 规范 / Release Discussion requirements

## 中文说明

每个正式版本（包括补丁版本）发布时，在仓库 Discussions 的 Announcements 分类发布一篇中英双语使用指南。标题带准确版本号；正文分别使用 `## 中文说明` 和 `## English Description`，两部分独立面向用户说明用法。正文源文件保存为 `docs/releases/v<version>-user-guide.md`，便于 review、修订和后续打包；不能只转贴 changelog 或内部 issue 清单。

发布前准备和验证指南；只有公开安装包/版本实际可用后，才发布正式 Discussion。发布后将 Discussion URL 补到 GitHub Release body 与发布验收 issue。必须核验链接和发布后的内容，再关闭发布 issue/milestone。Discussion 失败不重复发布 crate/tag；修复文档发布步骤，同版本已有 Discussion 优先更新，避免重复主题。这个步骤只增加文档，不为普通 PR 增加 macOS CI。

### 用户应该在指南中找到什么

1. 谁会用到：本版解决的真实场景、行为变化和适用边界。
2. 怎样取得：准确版本、平台包/安装命令、版本确认与入门入口。
3. 怎样使用：从空目录开始，完整 schema、文件名、命令、执行顺序；包含至少一个成功结果和关键失败/重试分支。补丁版展示修复前后的具体行为，不必重新复制整套教程。
4. 怎样接入应用：受影响的 CLI/Rust/protocol 路径，返回数据如何理解；涉及写入时说明请求原子边界与结果不确定时的处理。
5. 怎样升级：前一版到本版的备份、migration/storage/protocol/Rust API 兼容与 breaking changes；不需要某项操作时明确说明。
6. 怎样判断问题：错误码、定位字段、已知限制、可执行的验证方式。
7. 去哪里继续：对应版本的离线 docs 命令、在线专题链接、发布证据和反馈方式。

用已发布或待发布的匹配版本在隔离临时目录执行所有新例子；检查正反结果和失败后的状态。示例中的钱、重试、唯一性等领域语义需明确，避免把数据库机制描述成完整业务保证。已发布文档的语法/专题链接固定到版本 tag，不用持续变化的 main 解释旧版本。

### 完成清单

- [ ] 双语用户指南已 review，版本和安装入口正确
- [ ] 新例子实际执行，成功/失败结果和状态均符合描述
- [ ] 公共发布包和 crates 已核验
- [ ] Announcements 中该版本的 Discussion 已发布/更新，记录 URL
- [ ] Release body 与验收 issue 已补链接，在线文档链接可访问
- [ ] 完成以上步骤后关闭发布 issue/milestone

已发布 Discussion：[v0.12.0 使用指南](https://github.com/worktools/unionid/discussions/418)。源文件：[v0.12.0-user-guide.md](releases/v0.12.0-user-guide.md)。

## English Description

Every official release, including patches, must publish a bilingual user guide in the repository's Announcements Discussions category. Use the exact version in its title and independent Chinese/English sections. Review the source at docs/releases/v<version>-user-guide.md; do not substitute a changelog or internal issue list for user-facing instructions.

Prepare and validate the guide before publication; publish its final Discussion only when public binaries/version are available. Link the Discussion from the GitHub Release body and acceptance issue, verify the published content/links, then close the release issue/milestone. If publishing the documentation fails, repair that step without republishing crates/tags; update an existing same-version Discussion rather than creating duplicates. Routine PRs do not gain macOS jobs.

The guide must explain the target workflow and behavior change, exact installation/version checks, complete runnable setup and commands, success and important failure/retry outcomes, application integration and response interpretation, upgrades and compatibility/breaking changes, errors and limits, versioned offline/online documentation, validation evidence, and feedback channels. For patches, demonstrate the corrected behavior without repeating an unnecessary full tutorial. For mutations, explain request atomicity and uncertain outcomes.

Execute new examples with the matching binary in isolated temporary directories and verify both results and state after failure. Be precise about domain assumptions; database guards/constraints are not complete business guarantees. Pin online syntax/reference links to the release tag.

Before closing a release, review the bilingual guide, validate examples, verify public packages/crates, publish/update the Announcements Discussion and record its URL, then link it from the Release body and acceptance issue and verify the public links. The v0.12 guide linked above is the first maintained example.
