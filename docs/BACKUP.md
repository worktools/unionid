# 备份、还原与旧格式导入

当前可执行入口仍是下述完整 logical backup。v0.5 的增量备份采用“可选数据库内原子 journal + 外部 portable baseline/segment chain”，只把实际连续记录并封存的 sequence 声明为恢复点；完整契约与实现顺序见 [RFC 0016](rfc/0016-incremental-backup-chains.md)。RFC 已接受但实现由 [#304](https://github.com/worktools/unionid/issues/304) 跟踪，在命令落地前不要把文档中的预定 `backup incremental` 语法当作可用功能。

前两层 Rust primitives 已落地。`backup::incremental` 提供 archive codec 1：`UIB1` baseline、`UIS1` segment、canonical manifest/header、`none`/zstd level 3、payload/stored SHA-256、严格 frame 顺序和解压前资源上限。`Engine::enable_backup_journal` 可显式把 format 6 升为 format 7，并从已发布 baseline 的 sequence/checksum 开始记录；`backup_journal_status` 和 introspection 只暴露状态、范围、容量与 checksum，不暴露业务值。启用后，DDL、DML、receipt 变化和 migration cutover 与 canonical journal delta 在同一个 redb transaction 提交；派生索引不重复记录。容量不足会在业务 transaction 前返回 `E_BACKUP_JOURNAL_FULL`。format 6 默认行为不变，禁用 journal 也不会把 format 7 隐式降级。外部 archive 的 `init/export/restore` CLI 仍由 [#308](https://github.com/worktools/unionid/issues/308) 实现，应用不应自行拼接低层 records。

unionid 使用版本化逻辑备份保存完整 `Database` 状态，包括稳定 catalog/type/field/variant/table/index ID、ADT 行与 RowId 水位、schema revision/hash 和 migration ledger。存在幂等写入回执时，备份还会保存完整 receipt map，确保恢复后的重试不会重复 effect。备份包含格式版本、payload SHA-256 和 schema 元数据；它不是 CSV 投影，也不丢失 i64、sum tag、Option 或嵌套 product。

```text
unionid backup --db app.redb --output app.backup.json
unionid restore --backup app.backup.json --db restored.redb
```

`backup` 通过独占打开 redb 获得一致 committed view，先执行有界 full check，再从同一 view 顺序读取 typed rows。第一次读取直接计算 format-4 payload checksum，第二次直接写出 envelope；两次都不构造完整 resident `Database` 或 JSON byte buffer。输出只在完整写入、flush 与 sync 成功后发布，失败会删除未完成文件。`restore` 在内存校验格式版本、checksum、schema、所有 typed rows、RowId、索引定义和 ledger head，成功后才创建新的 redb 目标。命令从不覆盖已有备份或数据库；损坏或未知版本的输入不会创建目标。两条命令都支持 `--format json`。

没有 receipt 的逻辑备份继续使用 format 1，并保持原 checksum 算法；含 receipt 的备份使用 format 2，checksum 同时覆盖 Database 与 receipt map。新实现读取两种格式，但旧二进制必须拒绝 format 2，不能在 restore 时静默丢失重试身份。恢复得到原 revision、migration history 和 receipt；之后继续使用相同 migration 目录执行 `migration status/plan/apply`。

应用 schema migration 与备份格式版本彼此独立。CLI 的文本或 JSON 结果会报告 `receipt_count`，便于在切换恢复库前核对重试身份是否被保留。

## 显式导入原型 WAL/snapshot

```text
unionid import-legacy \
  --snapshot old.snapshot \
  --wal old.wal \
  --db imported.redb
```

`import-legacy` 只接受当前明确支持的原型 JSON snapshot 和 version 1／已知早期单行 WAL，然后把恢复结果通过完整逻辑校验写入新的 redb。未知 WAL 版本、无法由当前兼容语法解释的语句、含糊类型或损坏数据会停止导入并报告来源位置。输入 snapshot/WAL 始终只读保留，目标必须不存在。可只传 snapshot 或 WAL；两者同时提供时先读 snapshot，再按 sequence 回放增量 WAL。

本工具不承诺修复已经损坏或语义含糊的数据。需要手工映射的匿名 enum、隐式 null 或未知类型应先在原版本中导出为明确脚本，或编写一次性转换程序，再导入 unionid。

## English Description

The currently executable interface remains the complete logical backup described above. The v0.5 incremental design uses an optional transactionally embedded journal plus an external portable baseline/segment chain and declares only actually contiguous, sealed sequences as restore points. See [RFC 0016](rfc/0016-incremental-backup-chains.md) for the contract and delivery order. The RFC is accepted, while implementation is tracked by [#304](https://github.com/worktools/unionid/issues/304); do not treat its proposed `backup incremental` syntax as available until that issue lands.

The first two Rust layers are now available. `backup::incremental` provides archive codec 1 with `UIB1` baselines, `UIS1` segments, canonical manifests/headers, `none`/zstd level 3, payload/stored SHA-256, strict frame ordering, and pre-allocation decode limits. `Engine::enable_backup_journal` explicitly upgrades format 6 to format 7 and starts from a published baseline sequence/checksum. `backup_journal_status` and introspection expose only state, ranges, capacity, and checksums. Once enabled, DDL, DML, receipt changes, and migration cutover commit their canonical journal delta in the same redb transaction; derived indexes are omitted. Capacity admission returns `E_BACKUP_JOURNAL_FULL` before the business transaction. Default format-6 behavior remains unchanged, and disabling the journal does not implicitly downgrade format 7. The external archive `init/export/restore` CLI remains tracked by [#308](https://github.com/worktools/unionid/issues/308); applications should not assemble low-level records themselves.

Logical backup preserves the complete `Database` state, stable catalog/type/field/variant/table/index IDs, ADT rows and RowId watermarks, schema identity, migration ledger, and idempotency receipts. It streams from one exclusively opened, fully checked committed redb view, publishes only a flushed and synced complete output, and never overwrites an existing backup or database. Restore validates format, checksum, schema, typed rows, RowIds, indexes, ledger, and receipts before creating a new redb target. Formats 1–4 remain readable according to their frozen compatibility rules.

`import-legacy` is the explicit conversion path for supported prototype snapshot/WAL inputs. It preserves its inputs, requires a nonexistent redb target, and fails on unknown versions, ambiguous values, unsupported syntax, corruption, or noncontiguous sequence history rather than guessing a repair.
