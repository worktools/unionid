# CLI 与交互式 REPL

`unionid cli` 可以连接 TCP 服务，也可以通过 `--memory` 或 `--db <path>` 直接使用本地 Engine。文件和重定向 stdin 会作为一个原子脚本执行；终端输入进入带历史、补全和多行状态提示的 REPL。

```bash
unionid cli --memory
unionid cli --db app.redb
unionid cli --addr 127.0.0.1:7878
```

REPL 使用 `unionid>` 开始新脚本，`..>` 表示语法还需继续，`ready>` 表示当前脚本完整。完整脚本在空行后提交；Ctrl-C 清空当前缓冲区，Ctrl-D 按当前完整性执行或报告未完成输入。

## Introspection

以下命令在 memory、redb 和 TCP 会话使用同一个类型化 introspection 快照：

| 命令 | 输出 |
| --- | --- |
| `.schema` | 可重新解析的规范 schema |
| `.tables` | 按名称排列的表 |
| `.types` | 按名称排列的命名类型 |
| `.storage` | storage mode、schema revision/hash、migration 数量与 head |
| `.help` | 交互命令和提交方式 |
| `.quit` | 退出 |

空 catalog 会明确显示 `(empty schema)`、`(no tables)` 或 `(no types)`。TCP introspection 是 version 1 JSON Lines 协议的一部分；旧 query 请求继续兼容。远端断开时，meta command 显示连接错误，Tab 仍可补全本地语言关键字。

## 补全

Tab 补全当前语言关键字、meta command，以及最近一次 introspection 得到的表、类型和字段名称。schema 成功变化后，REPL 会刷新 catalog 候选。补全只替换光标前的当前标识符，不提交或改写脚本；空 catalog 仍提供关键字和 meta command。

## 持久历史

交互历史默认写入 `$HOME/.unionid/history.jsonl`。可以指定路径或完全关闭：

```bash
unionid cli --memory --history ./local-history.jsonl
unionid cli --memory --no-history
```

历史使用 version 1 JSON Lines，每项保存一个已经提交的完整脚本。新文件在 Unix 上使用 `0600` 权限。默认持久化策略有意保守：DDL、DML、migration、包含注释、文本或数字字面量的查询都不写入文件；只读且不含这些内容的 pipeline、`.schema`、`.tables`、`.types`、`.storage` 和 `.help` 可以保存。带 `$name` 的查询源码可以保存，因为绑定值不在源码中。

历史文件限制为 8 MiB、10,000 项，每项仍受 1 MiB 源码限制。无效 JSON、未知版本、超限或违反安全策略的既有记录会禁用本次会话的持久历史，打印原因并保持原文件不变。运行中的行编辑历史仍可使用。

## 少标点脚本

类型、表和查询继续使用无分号的换行／缩进语法；复杂条件仅在需要表达优先级时使用括号。可执行示例：

```text
type Task =
  id int
  title text

table tasks Task
  key id

from tasks
filter id > 0
select {id, title}
take 10
```
