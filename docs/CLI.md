# CLI 与交互式 REPL

`unionid cli` 可以连接 TCP 服务，也可以通过 `--memory` 或 `--db <path>` 直接使用本地 Engine。文件和重定向 stdin 会作为一个原子脚本执行；终端输入进入带历史、补全和多行状态提示的 REPL。

```bash
unionid cli --memory
unionid cli --db app.redb
unionid cli --db app.redb --read-only
unionid cli --addr 127.0.0.1:7878
```

## 版本与部署诊断

部署脚本不需要解析面向人的句子。`version` 报告当前二进制及它明确支持的协议、存储和 codec；`doctor` 还可检查一个已经存在的数据库：

```bash
unionid version --format json
unionid doctor --format json
unionid doctor --db app.redb --format json
```

`version` 的 version 1 JSON 形态如下。未来版本可以增加字段，但不会改变或删除当前字段的含义；v0.2 的精确能力值同时固定在包内 [`release/contract.json`](../release/contract.json)：

```json
{"schema_version":1,"software_version":"0.2.0","target":"aarch64-apple-darwin","protocol_versions":[1,2],"stream_protocol_versions":[1],"readable_storage_formats":[1,2,3,4,5,6],"readable_backup_formats":[1,2,3,4],"current_storage":{"format":6,"catalog_codec":4,"value_codec":2,"index_key_codec":3,"migration_codec":1,"receipt_codec":2,"maintenance_codec":1,"backup_codec":4}}
```

`doctor` 成功结果增加 `ok`，并把同一个版本对象放在 `version`。指定 `--db` 后还会返回 `database.storage`、`storage_versions`、`read_only`、schema identity、migration 边界和 table/type 数量；不返回数据库路径、schema 源码、表名或类型名。未指定数据库时省略 `database`：

```json
{"schema_version":1,"ok":true,"version":{"schema_version":1,"software_version":"0.2.0","target":"aarch64-apple-darwin","protocol_versions":[1,2],"stream_protocol_versions":[1],"readable_storage_formats":[1,2,3,4,5,6],"readable_backup_formats":[1,2,3,4],"current_storage":{"format":6,"catalog_codec":4,"value_codec":2,"index_key_codec":3,"migration_codec":1,"receipt_codec":2,"maintenance_codec":1,"backup_codec":4}}}
```

`doctor --db` 要求路径已经存在且是文件。为避免 redb 的打开恢复改变原文件，它只读取一个权限受限的临时字节副本；不会创建、修复、升级或锁定请求的数据库，退出时删除副本。应对静止数据库或一致备份运行它；若源文件在复制时仍有写入，诊断结果不应作为一致快照。需要证明原文件自身可完整打开时使用 `check --db`。

## 离线文件压缩 / Offline file compaction

先停止持有数据库的 server 或本地进程，并创建、验证逻辑备份。确认 `migration status` 没有 unfinished maintenance 后运行：

```bash
unionid backup --db app.redb --output before-compact.backup.json
unionid compact --db app.redb
unionid compact --db app.redb --format json
```

`compact` 对现有 redb 文件执行原地物理空间回收，并自动完成压缩前后完整检查和身份比较。plain 输出包含 changed/no-op、压缩前后与回收字节、schema、sequence、storage format 和 codecs；JSON 使用 version 1 `StorageCompaction`。命令本身就是执行授权，不再要求交互确认。数据库仍被占用时返回 `E_BUSY`，存在 migration generation 时返回 `E_MAINTENANCE_REQUIRED`，不存在的路径返回 `E_CONFIG` 且不会创建文件。

该操作同步遍历完整数据多次，耗时随数据库增长，并需要 redb 搬页和同步提交所需的维护窗口与磁盘余量。generation reclaim 只删除旧逻辑 key，不保证文件缩小；`compact` 才处理底层页面。原生压缩开始内部提交后不能安全取消。若 Ctrl-C、进程退出、I/O 错误或 `E_STORAGE_REOPEN_REQUIRED` 使结果不确定，应重新打开并运行 `unionid check --db app.redb`，不能只凭文件变小判断成功。首版仅提供本地离线 CLI，不通过 TCP、HTTP、query language 或 ConcurrentEngine 暴露。

Stop the server or other local owner first, retain a verified logical backup, and resolve every unfinished migration before running `compact`. The command compacts the existing redb file in place, performs complete checks and identity comparison before and after native compaction, and returns exact byte counts plus schema and storage identity. It may scan the full database several times and requires a maintenance window and disk headroom. Generation reclamation removes obsolete logical keys; physical compaction is the separate step that can reduce the file. After interruption or an uncertain storage result, reopen the database and run `check`; file-size reduction alone is not proof of success.

## JSON 错误与退出码

支持 `--format json` 的非查询命令统一返回 version 1 错误 envelope，且只写 stdout：

```json
{"schema_version":1,"ok":false,"exit_code":5,"error":{"code":"E_IO","message":"open '<redacted>': No such file or directory (os error 2)"}}
```

错误可能增加 `error.span`。路径和引号包裹的输入会在该 envelope 中脱敏；参数解析错误使用固定提示，不回显参数。退出码是稳定的部署接口：

| 退出码 | 类别 | 典型情况 |
| --- | --- | --- |
| 0 | 成功 | 命令完成 |
| 1 | 未归类 | 未纳入下列稳定类别的内部错误 |
| 2 | 参数／配置 | CLI 参数无效、必要配置缺失 |
| 3 | 输入／schema | 语法、类型、schema 或查询错误 |
| 4 | 连接／占用 | TCP 连接、协议或数据库被占用 |
| 5 | 存储／不确定 | I/O、codec、backup 或提交结果不确定 |
| 6 | 完整性 | `check` 命令未通过，包括无法打开待检查文件 |

JSON 成功结果和错误只写 stdout，面向人的诊断只写 stderr。`run --format json` 和 `cli --format json` 为保持协议兼容，继续输出现有 `QueryResponse`，不会套入 CLI envelope；失败时仍使用上表的进程退出码。`--version` 保留 clap 的单行人类输出，自动化应使用显式的 `version --format json`。

`version --format json` 的 `stream_protocol_versions` 声明服务支持的独立 NDJSON 协议。当前 CLI query 命令仍等待完整 response，不把 partial stream 混入脚本输出；需要流式消费的应用使用 Rust `stream` API、TCP envelope 或 HTTP adapter。需要可靠续传时使用 bounded cursor page，不能把断开的 stream 行号当作 resume token。

持久幂等回执使用独立运维命令。prune 默认只预览，至少需要一个 cutoff，只有 `--confirm` 才删除：

```bash
unionid receipts status --db app.redb --format json
unionid receipts prune --db app.redb --through-sequence 1200 --max-receipts 500
unionid receipts prune --db app.redb --through-sequence 1200 --max-receipts 500 --confirm
```

无 cutoff、`max-receipts` 不在 1–1000，或存储／只读边界不允许操作时返回稳定错误和对应的分类退出码。清理后的 key 可以再次执行；命令不会按墙钟自动淘汰 receipt。

format-6 schema migration 可以在进程退出后从 durable checkpoint 继续，并提供显式状态与清理命令：

```bash
unionid migration apply --db app.redb --dir migrations
unionid migration status --db app.redb --dir migrations --format json
unionid migration abort --db app.redb --format json
```

`status.maintenance` 仅在存在 shadow generation 时出现，包含 phase、generation、row/index 进度、逻辑字节和可执行 actions。Building、Ready 或 Aborting 状态保留旧数据查询，但阻止普通 mutation；再次 `apply` 相同 migration 会恢复，不匹配文件返回 `E_MAINTENANCE_CONFLICT`。`abort` 分批删除尚未切换的 target；Reclaimable 时只完成旧 source 清理，不回滚已应用 migration。没有 maintenance 时是成功的 no-op。完整恢复契约见 [MIGRATIONS.md](MIGRATIONS.md#版本化-runner)。

`run`、本地 `cli` 和 `server` 都支持 `--db <path> --read-only`。该模式只打开已经存在的 redb 文件；路径不存在会返回 `E_CONFIG`，不会创建空数据库。查询、`explain` 和 introspection 正常工作，任何包含 DDL、DML 或待应用 migration 的请求都在构造候选状态或 durable transaction 前以 `E_READ_ONLY` 整批拒绝。远程 `cli` 是否只读取决于服务端配置，客户端参数不能替代服务端边界。

REPL 使用 `unionid>` 开始新脚本，`..>` 表示语法还需继续，`ready>` 表示当前脚本完整。完整脚本在空行后提交；Ctrl-C 清空当前缓冲区，Ctrl-D 按当前完整性执行或报告未完成输入。

## Introspection

以下命令在 memory、redb 和 TCP 会话使用同一个类型化 introspection 快照：

| 命令 | 输出 |
| --- | --- |
| `.schema` | 可重新解析的规范 schema |
| `.tables` | 按名称排列的表 |
| `.types` | 按名称排列的命名类型 |
| `.storage` | storage mode、read-only 状态、schema revision/hash、migration 数量/head 与可选 maintenance 进度 |
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
