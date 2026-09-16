# 五分钟开始使用 unionid / Start using unionid in five minutes

## 中文

这条首用路径从一个已安装的 `unionid` 二进制和空目录开始。它生成一个带 ADT schema、线性 migration、seed 和 typed query 的独立项目，然后验证重开、诊断、完整性检查与备份还原。`init` 和 `project check` 从 v0.6.0 起提供；运行项目不需要 clone unionid 仓库。

### 1. 选择安装入口

crates.io 安装需要 Rust 1.94 或更高版本：

```bash
cargo install unionid --locked
```

原生 release archive 解压后，把包内二进制加入当前 shell 的 `PATH`：

```bash
cd unionid-v<version>-<target>
export PATH="$(pwd)/bin:$PATH"
```

从源码构建时，在仓库根目录执行：

```bash
cargo build --locked
export PATH="$(pwd)/target/debug:$PATH"
```

三种入口从下一步开始使用完全相同的命令。先确认当前 shell 找到的版本与兼容范围：

```bash
unionid version --format json
unionid doctor --format json
```

这两条命令不会创建数据库。

### 2. 生成并检查项目

在任意空工作目录中运行：

```bash
mkdir unionid-first-use
cd unionid-first-use
unionid init tasks
cd tasks
unionid project check --dir .
```

`init` 只接受不存在或空目录，不覆盖已有文件。生成结果包含：

- `README.md`：简短的双语项目说明
- `schema.unid`：当前声明式 schema
- `migrations/0001_initial.unid`：可执行的初始 migration
- `seed.unid`：两条 typed task
- `queries/list_running.unid`：在明确的 `State` 上直接匹配 `Running` 的 query
- `data/`：被 `.gitignore` 忽略的本地数据目录

`project check` 不创建数据库。它按 schema → migrations → queries 的固定顺序检查规范格式、migration 最终 schema 和 query binding；三个阶段都通过才返回 0。

### 3. 建库、写入并从新进程重开

```bash
unionid migration apply --db data/tasks.redb --dir migrations
unionid run --db data/tasks.redb --file seed.unid
unionid run --db data/tasks.redb --file queries/list_running.unid
```

seed 写入两行，最后一条命令从新的 `unionid` 进程重新打开 redb，并返回 `id = 1`、标题为 `learn ADTs` 的 `Running` task。结果保留完整 sum variant 和 record payload；字段、constructor、pattern coverage 与 payload 类型会在扫描前检查。

### 4. 诊断并检查原数据库

```bash
unionid doctor --db data/tasks.redb --format json
unionid check --db data/tasks.redb
```

`doctor` 读取权限受限的临时副本，报告 storage、codec、schema identity 和 migration 摘要，不修改请求的文件。`check` 打开原数据库，执行 redb 完整性检查，并验证 catalog、schema hash、typed rows、RowId、索引和 migration ledger。

### 5. 备份、还原并比较 typed 结果

```bash
unionid backup \
  --db data/tasks.redb \
  --output data/tasks.backup.json \
  --format json
unionid restore \
  --backup data/tasks.backup.json \
  --db data/restored.redb \
  --format json
unionid run \
  --db data/restored.redb \
  --file queries/list_running.unid
unionid check --db data/restored.redb
```

restore 只写入不存在的新路径。还原后的 query rows、列类型和 schema identity 应与源数据库一致。增量 archive、按 sequence 恢复和保留策略见[备份说明](BACKUP.md)。

### 下一步

- 修改 schema 时新增 migration，再运行 `project check` 和 `migration plan/apply`；见[迁移说明](MIGRATIONS.md)。
- 让 LLM 或代码生成器协助编写查询时，先运行 `unionid docs query` 取得版本匹配的规则和示例，再把 `schema print --format json` 的实际 schema 一并提供；生成结果用 `query describe` 检查后再执行。
- 用 `unionid query rust --schema schema.unid --dir queries --output generated/queries.rs` 生成共享 ADT、typed 参数、结果 row 和调用函数。
- 用 `unionid server --db data/tasks.redb` 与 `unionid cli --addr 127.0.0.1:7878` 切换到 TCP；见 [CLI](CLI.md)、[协议](PROTOCOL.md)和[服务部署](DEPLOYMENT.md)。
- Rust 应用可直接使用 `Engine::open_redb`、prepared parameters、`Value::from_serde` 和 `typed_rows`；完整类型边界见[应用数据边界](APPLICATION_DATA.md)。

当前产品面向单机、一个数据库所有者和串行写入，约 10,000 行是舒适工作集；100,000 行只是已测试上限。通用扁平 join、window 和分布式执行不在当前范围内。

### 自动验证

源码构建可从空目录执行与本文相同的 starter 链路：

```bash
python3 scripts/validate-first-use.py \
  --binary "$PWD/target/debug/unionid" \
  --work-dir /tmp/unionid-first-use
```

release archive 提供同一个验证器：

```bash
python3 tutorial/validate-first-use.py \
  --binary "$PWD/bin/unionid" \
  --work-dir /tmp/unionid-first-use
```

验证器拒绝非空工作目录，并核对生成文件、项目检查阶段、typed rows、schema identity、完整性检查和备份还原。release archive 仍保留覆盖原子更新、TCP 与嵌入式 Engine 的进阶 `tutorial/validate.py`。

## English

This first-use path starts with an installed `unionid` binary and an empty directory. It generates a standalone project with an ADT schema, linear migration, seed, and typed query, then validates reopen, diagnostics, integrity checking, backup, and restore. `init` and `project check` are available from v0.6.0, and running the project requires no unionid repository checkout.

### 1. Choose an installation entry

A crates.io installation requires Rust 1.94 or newer:

```bash
cargo install unionid --locked
```

After extracting a native release archive, add its binary to the current shell's `PATH`:

```bash
cd unionid-v<version>-<target>
export PATH="$(pwd)/bin:$PATH"
```

For a source build, run these commands at the repository root:

```bash
cargo build --locked
export PATH="$(pwd)/target/debug:$PATH"
```

All three entries use exactly the same commands from the next step onward. First inspect the selected binary and compatibility range:

```bash
unionid version --format json
unionid doctor --format json
```

Neither command creates a database.

### 2. Generate and check a project

Run these commands from any empty working directory:

```bash
mkdir unionid-first-use
cd unionid-first-use
unionid init tasks
cd tasks
unionid project check --dir .
```

`init` accepts only a missing or empty directory and never overwrites existing files. It generates:

- `README.md`: a short bilingual project guide
- `schema.unid`: the current declarative schema
- `migrations/0001_initial.unid`: the executable initial migration
- `seed.unid`: two typed tasks
- `queries/list_running.unid`: a query that directly matches `Running` against a known `State`
- `data/`: a local data directory ignored by Git

`project check` creates no database. In the fixed schema → migrations → queries order, it checks canonical formatting, the migration target schema, and query binding. It exits zero only when all three phases pass.

### 3. Create, write, and reopen from a new process

```bash
unionid migration apply --db data/tasks.redb --dir migrations
unionid run --db data/tasks.redb --file seed.unid
unionid run --db data/tasks.redb --file queries/list_running.unid
```

The seed writes two rows. The final command reopens redb in a new `unionid` process and returns the `Running` task with `id = 1` and title `learn ADTs`. The result preserves the complete sum variant and record payload. Fields, constructors, pattern coverage, and payload types are checked before scanning rows.

### 4. Diagnose and check the original database

```bash
unionid doctor --db data/tasks.redb --format json
unionid check --db data/tasks.redb
```

`doctor` reads a permission-restricted temporary copy and reports storage, codecs, schema identity, and migration summary without changing the requested file. `check` opens the original database, runs redb integrity checking, and validates the catalog, schema hash, typed rows, RowIds, indexes, and migration ledger.

### 5. Back up, restore, and compare typed results

```bash
unionid backup \
  --db data/tasks.redb \
  --output data/tasks.backup.json \
  --format json
unionid restore \
  --backup data/tasks.backup.json \
  --db data/restored.redb \
  --format json
unionid run \
  --db data/restored.redb \
  --file queries/list_running.unid
unionid check --db data/restored.redb
```

Restore writes only to a missing destination. Query rows, column types, and schema identity should match the source database. See [Backup](BACKUP.md) for incremental archives, sequence restore, and retention.

### Next steps

- Add a migration when changing the schema, then run `project check` and `migration plan/apply`; see [Migrations](MIGRATIONS.md).
- When an LLM or generator helps write a query, first run `unionid docs query` for version-matched rules and examples, provide the actual `schema print --format json` output, and validate the generated file with `query describe` before execution.
- Generate shared ADTs, typed parameters, result rows, and call functions with `unionid query rust --schema schema.unid --dir queries --output generated/queries.rs`.
- Move to TCP with `unionid server --db data/tasks.redb` and `unionid cli --addr 127.0.0.1:7878`; see [CLI](CLI.md), [Protocol](PROTOCOL.md), and [Deployment](DEPLOYMENT.md).
- Rust applications can use `Engine::open_redb`, prepared parameters, `Value::from_serde`, and `typed_rows` directly; see [Application data boundaries](APPLICATION_DATA.md).

The current product targets one machine, one database owner, serialized writes, and a comfortable working set around 10,000 rows; 100,000 rows is a tested upper bound. General flattened joins, windows, and distributed execution remain outside the current scope.

### Automated validation

A source build can run the same starter journey from an empty directory:

```bash
python3 scripts/validate-first-use.py \
  --binary "$PWD/target/debug/unionid" \
  --work-dir /tmp/unionid-first-use
```

The release archive provides the same validator:

```bash
python3 tutorial/validate-first-use.py \
  --binary "$PWD/bin/unionid" \
  --work-dir /tmp/unionid-first-use
```

The validator rejects a non-empty work directory and checks generated files, ordered project-check phases, typed rows, schema identity, integrity checks, backup, and restore. The release archive retains the advanced `tutorial/validate.py` journey covering atomic updates, TCP, and the embedded Engine.
