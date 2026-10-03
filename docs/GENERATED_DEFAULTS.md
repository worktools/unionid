# Generated defaults / 生成默认值

## 中文说明

当前开发分支支持表级受控默认值，尚未发版。查看本页：`unionid docs show generated-defaults`。memory 可直接执行；已有 redb 数据库需要显式升级并安装能力，普通 DDL 不会隐式改变存储格式。新数据库默认仍为 format 10。

```sh
unionid upgrade --db app.redb --target 14
unionid upgrade --db app.redb --target 14 --require generated_defaults
```

如果数据库有活跃增量备份 journal，先导出 archive 至当前 head，并在升级／安装命令提供 `--repo <archive>`；不要跳过该校验。旧软件无法打开带新能力声明的文件，升级前保留备份。

REPL 的 Tab 补全提供 `sequence`、`start`、`next`、`uuid_v7`、`now` 和当前 sequence 名；rename 后会刷新候选。

保存下例为 `accounts.unid`，执行 `unionid run --db app.redb --file accounts.unid`：

```unionid
sequence account_ids {start 1}
struct Account {
  id: int
  public_id: uuid
  owner: text
  created_at: timestamp
}
table accounts: Account {
  key id
  default id = next(account_ids)
  default public_id = uuid_v7()
  default created_at = now()
}
insert accounts {owner: "Ada"} | returning {id, public_id, owner, created_at}
```

- 只对缺失的顶层字段生成；显式提供的值优先，嵌套字段不自动调用生成器。表级生成策略优先于 struct 的常量默认值。
- `next(sequence)` 返回 int（也支持以 int 为底层的命名类型）；按脚本、批次行和字段声明顺序分配。到达 i64 最大值后，下一次请求返回 `E_ARITH`。
- `uuid_v7()` 返回 uuid，每次分配使用独立随机部分。`now()` 返回 timestamp，整个原子请求共用一次 UTC 墙钟取样；不保证单调或提交顺序。时间／随机源失败返回 `E_GENERATION`。
- 约束、expect、算术或存储明确回滚时，行和计数器一起回滚。存储结果不确定时先重开并 check；使用相同幂等 key 与请求重试可取回完整首次响应，不再次生成。
- prepare、explain、只读拒绝和回执重放不分配值。upsert 的主键必须显式提供，其他缺失字段可以生成，命中行仍是完整替换。
- migration 附加生成策略只影响未来写入；历史字段用确定性默认值／conversion 填充。备份、restore 和 shadow resume 保存已经生成的值与计数器，不重新调用生成器。

TCP/HTTP 返回 uuid/timestamp 时使用 protocol v2。Rust `queries!` 生成的 mutation 输入将可省略的生成字段表示为 `Option<T>`；`None` 表示省略字段，输出仍是完整 ADT。数据库自身的 `option T` 与输入省略是两种不同语义。详见 `unionid docs show rust-query-macro`。

## English Description

The development branch implements controlled table defaults; this is not released yet. Read this page with `unionid docs show generated-defaults`. Memory execution needs no installation. For an existing redb database, explicitly upgrade to physical format 14 and then install `generated_defaults` using the two commands above. New databases still default to format 10; DDL never upgrades implicitly. For an active incremental journal, export the archive through the current head and provide `--repo <archive>` during upgrade/installation. Keep a backup: older software rejects the new requirement.

REPL Tab completion includes generator keywords and current sequence names, refreshed after rename.

Save the example as `accounts.unid`, then run `unionid run --db app.redb --file accounts.unid`. Generated defaults apply only to omitted top-level fields. Explicit values override them; table policies override struct constant defaults. `next(sequence)` produces int or an int-backed nominal type in script, batch-row and declared-field order; allocation beyond i64 maximum fails with `E_ARITH`. `uuid_v7()` uses independent random bits per allocation. `now()` shares one UTC wall-clock sample per atomic request without promising monotonic or commit order. Clock/entropy failures report `E_GENERATION`.

Definite failures roll back rows and counters together. An uncertain storage result requires reopen/check; retrying the same idempotency key and request recovers the original complete response without regeneration. Prepare, explain, read-only rejection and receipt replay never allocate. Upsert requires an explicit primary key and fully replaces matching rows, generating other omitted fields if configured.

Migration policies affect future writes; fill historical fields with deterministic defaults/conversions. Backup, restore and shadow resume retain materialized values/counters. Use protocol v2 for UUID/timestamp output over TCP/HTTP. Rust `queries!` inputs use `Option<T>` for omittable generated fields: `None` omits the field, while outputs remain complete ADTs. Input omission differs from the database's `option T` value. See `unionid docs show rust-query-macro`.
