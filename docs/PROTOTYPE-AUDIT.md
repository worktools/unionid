# 原型基线与可复现问题

检查日期：2026-09-06；检查基线：`dfb1a28`。本页保留初次审计时的证据，描述的是该历史版本。本轮已修复数值/索引一致性、WAL 失败可见性、带水位快照恢复、解析及 CLI 的若干问题，当前实现和验证边界见 [开发记录](DEVELOPMENT.md)。

## 验证结果

`cargo check`、`cargo test`、`cargo build` 均通过；`cargo test` 实际运行 0 个测试。

在隔离的临时数据库及 loopback 端口中，使用实际 server／cli 验证了建表、插入、过滤、投影、参数化 enum、WAL 重启及 snapshot 重启。测试数据均为本次生成。

## 1. 建索引改变查询结果

```text
create table nums (n int)
insert nums {n:9007199254740992}
insert nums {n:9007199254740993}
from nums | filter n = 9007199254740992
create index nums (n)
from nums | filter n = 9007199254740992
```

实测：索引前返回两行，索引后返回一行。预期始终只返回精确匹配的一行。`model.rs::cmp_eq` 将整数转 f64，丢失精度，索引却以整数文本编码。

```text
create table floats (f float)
insert floats {f:0.0}
insert floats {f:-0.0}
from floats | filter f = 0.0
create index floats (f)
from floats | filter f = 0.0
```

实测：索引前两行，索引后一行。`db.rs::index_key` 使用 Float 原始位模式，与当前数值相等规则不一致。

源码：[数值相等](https://github.com/worktools/unionid/blob/dfb1a28/src/model.rs#L119)、[查询执行与索引](https://github.com/worktools/unionid/blob/dfb1a28/src/db.rs#L173)。

## 2. WAL 写入失败后未记录的行仍可查询

复现方法：临时服务使用 `--wal-path`，成功建表后把该 WAL 文件移到同一临时目录另一个文件名，并在原路径建一个目录，制造追加失败；随后执行插入。

实测：插入连接关闭，没有 JSON 响应；下一次查询能读到刚插入的行。这说明错误路径没有回滚内存变化。测试结束后停止临时进程，没有操作已有数据库。

源码：[`server.rs`](https://github.com/worktools/unionid/blob/dfb1a28/src/server.rs#L118) 先 `guard.execute` 再 `wal.append`；[`wal.rs`](https://github.com/worktools/unionid/blob/dfb1a28/src/wal.rs#L34) 只 `flush`，没有持久同步。

## 3. 快照和旧 WAL 重叠会使启动失败

使用 `--snapshot-every 2` 建表并插入一行，停止服务；保留成功快照，在临时 WAL 中重建这两条已被快照包含的语句，然后重新启动。

这是对“快照已发布、WAL 尚未截断”磁盘状态的模拟，并非声称本次恰好触发了真实断电。实测启动退出码 1：`apply wal line 1 failed: table 'snap' already exists`。若重叠 WAL 只含 insert，还需回归验证重复数据问题。

源码：[`snapshot.rs::save`](https://github.com/worktools/unionid/blob/dfb1a28/src/snapshot.rs#L38) 直接覆盖文件；[`server.rs`](https://github.com/worktools/unionid/blob/dfb1a28/src/server.rs#L131) 保存后截断 WAL，无快照水位。直接覆盖还存在中途失败损坏唯一快照的风险，这是代码审查结论。

## 4. 类型诊断、解析和 CLI

| 输入或操作 | 实测／审查结果 | 目标 |
| --- | --- | --- |
| `from users \| select typo` | 实测成功并返回 Null | 执行前报未知字段，空表同样检查 |
| `from users \| filter name = "a\|b"` | 实测字面量解析失败 | Lexer 正确处理字符串中的管道符 |
| CLI 查询不存在的表 | 实测输出 error，但退出码为 0 | 结构化错误、非零退出码 |
| 缺失插入字段、`null` | 代码中默认 Null，可赋给任意类型 | 必填／Option／default 明确区分 |
| 重复字段与尾部垃圾 | parser／BTreeMap 路径缺乏完整验证 | 在语法／类型检查阶段拒绝 |
| REPL stdin EOF | 代码未按 read_line 返回 0 退出 | EOF 正常退出，无空循环 |
| 大量连接／超长一行 | 每连接建线程、read_line 无界 | 有界连接、请求大小和超时 |

源码：[parser](https://github.com/worktools/unionid/blob/dfb1a28/src/query.rs)、[类型转换](https://github.com/worktools/unionid/blob/dfb1a28/src/model.rs#L96)、[CLI](https://github.com/worktools/unionid/blob/dfb1a28/src/cli.rs)。

## 5. 规划处理原则

先将这些问题转换成回归用例与存储方案的准入条件，再在新实现上关闭对应 issue。正常重启成功不能替代故障恢复验证；新增语法或索引必须保持同一数据语义。具体任务与优先级见 [路线图](ROADMAP.md)。
