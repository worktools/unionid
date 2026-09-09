# 五分钟开始使用 unionid

这份教程从空目录开始，用同一组无分号 ADT 脚本完成建库、查询、更新、关闭重开和完整性检查。发布包内含 `bin/unionid` 和 `tutorial/`。解压后先进入包目录并把二进制加入当前 shell 的 PATH：

```bash
cd unionid-v0.1.0-<target>
export PATH="$PWD/bin:$PATH"
```

从源码运行时，先在仓库根目录执行 `cargo build --locked` 和 `export PATH="$PWD/target/debug:$PATH"`，再把下文的 `../tutorial` 替换为 `../examples/getting-started`。

先让自动化确认它拿到的二进制及兼容范围：

```bash
unionid version --format json
unionid doctor --format json
```

两条命令都不创建数据库。JSON 包含软件版本、Rust target、协议版本，以及可读和当前写入的 storage/codec 版本。

## 1. 创建持久数据库

```bash
mkdir unionid-demo
cd unionid-demo
unionid run --db tasks.redb --file ../tutorial/01_setup.uid
```

这个脚本声明两个积类型 `Contact`、`Task` 和一个和类型 `State`，再建立有主键的 `tasks` 表并写入嵌套数据：

```text
type State =
  Pending
  | Running {
    worker text,
    attempt int,
  }
  | Done {
    result text,
  }

table tasks Task
  key id
```

声明和写入在一个原子脚本内完成。成功后，`tasks.redb` 是 redb 持久数据库。

## 2. 查询和类型化解构

```bash
unionid run --db tasks.redb --file ../tutorial/02_running.uid
```

查询使用换行 pipeline。braced `match` 穷尽匹配 `State`，`select` 保留嵌套字段：

```text
from tasks
filter (
  match state {
    Running {worker, attempt} => attempt >= 1,
    _ => false,
  }
)
select {id, title, owner.email, state}
sort id
```

结果是一条 `Running` 任务。字段和 pattern 在扫描前按 schema 检查；拼错字段或遗漏 sum 分支会返回错误，而不会退化成动态值。

## 3. 原子更新

```bash
unionid run --db tasks.redb --file ../tutorial/03_update.uid
```

这次更新按主键找到第二条任务，并把 `Pending` 替换为带 record payload 的 `Running`。完整语句要么提交，要么不改变数据库。

## 4. 关闭后重开

每次 `run --db` 都在新进程中打开和关闭数据库。再次运行查询就验证了持久恢复：

```bash
unionid run --db tasks.redb --file ../tutorial/04_reopen.uid
unionid doctor --db tasks.redb --format json
unionid check --db tasks.redb
```

最终查询返回两行，并通过 `derive worker = match state` 解构 ADT 产生新列。`doctor` 不改动原文件，只通过临时副本报告存储版本、schema identity 和 ledger 摘要；`check` 则打开原数据库，先执行 redb 完整性检查，再验证 catalog、schema hash、row、稳定 RowId、索引和 migration ledger 的逻辑一致性。

## 5. 改用 TCP 服务

服务与本地命令共享同一个 `Engine` 语义。在一个终端启动：

```bash
unionid server --addr 127.0.0.1:7878 --db server.redb
```

另一个终端通过 TCP 执行同样的脚本：

```bash
unionid cli --addr 127.0.0.1:7878 --file ../tutorial/01_setup.uid
unionid cli --addr 127.0.0.1:7878 --file ../tutorial/03_update.uid
unionid cli --addr 127.0.0.1:7878 --file ../tutorial/04_reopen.uid
```

省略 `--query` 和 `--file` 可进入 REPL；Tab 补全类型、表和字段，`.schema`、`.tables`、`.types`、`.storage` 查看当前 catalog。

## 嵌入 Rust

源码仓库中的 `examples/getting_started.rs` 使用 `Engine::open_redb` 执行完全相同的四个脚本：

```bash
cargo run --locked --example getting_started -- /tmp/unionid-embedded.redb
```

Rust API、本地 CLI 和 TCP 在同一 schema 下返回相同的 typed rows、列和 schema identity。应用可进一步使用 `prepare`、typed parameters 和 schema 前置条件，见[版本化接口与参数](PROTOCOL.md)。

冷热分离的 Rust 持久化示例见 [应用数据边界](APPLICATION_DATA.md)：摘要与 ADT 正文原子写入，摘要有界读取，正文按需获取并携带实际版本。

## 自动验证整段教程

发布包可在一个空目录中自检上面的本地和 TCP 链路：

```bash
python3 tutorial/validate.py \
  --binary "$PWD/bin/unionid" \
  --work-dir /tmp/unionid-tutorial
```

验证脚本拒绝非空工作目录，避免覆盖已有数据库。生产升级前请继续阅读[升级与格式兼容](UPGRADING.md)和[备份说明](BACKUP.md)。

## English walkthrough

The four files under `tutorial/` form one executable five-minute journey. Start with `unionid version --format json`, run `01_setup.uid` against a new `--db` path, query the `Running` variant with `02_running.uid`, atomically change the pending row with `03_update.uid`, then launch a new process with `04_reopen.uid`. Finish with `unionid doctor --db tasks.redb --format json` and `unionid check --db tasks.redb`: doctor reports compatibility and schema/ledger summaries from a private copy without changing the source, while check opens and verifies the actual database. The scripts declare named product and sum types, insert nested values, exhaustively match an ADT, project a nested field, update a variant payload, and derive a typed column.

The TCP commands above execute the same files through `unionid cli --addr`. The Rust example calls `Engine::open_redb` with those same sources. `tutorial/validate.py` runs both paths from an empty directory and compares their typed rows, columns, and schema identity.

For atomic summary/content writes and on-demand versioned ADT reads, see the Rust example in [Application data boundaries](APPLICATION_DATA.md).
