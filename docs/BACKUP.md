# 备份、还原与旧格式导入

完整 logical backup 和增量 archive 都可使用。v0.5 的增量备份采用“可选数据库内原子 journal + 外部 portable baseline/segment chain”，只把实际连续记录并封存的 sequence 声明为恢复点；完整契约与后续 restore/retention 工作见 [RFC 0016](rfc/0016-incremental-backup-chains.md) 与 [#304](https://github.com/worktools/unionid/issues/304)。

`backup::incremental` 提供 archive codec 1 以及 `init`、`export`、`list`、`verify` Rust API。CLI 对应如下：

```text
unionid backup incremental init --db app.redb --repo backups/
unionid backup incremental export --db app.redb --repo backups/
unionid backup incremental export --db app.redb --repo backups/ --through-sequence 42
unionid backup incremental list --repo backups/ --format json
unionid backup incremental verify --repo backups/
```

`init` 先从独占、完整检查的 committed view 写出并重读验证 baseline，再发布 `prepared` manifest，最后用同步事务启用 journal 并把 manifest 切为 `active`。它是把 format 6 升到 format 7 的显式授权。中断后重试会对账两侧 chain ID、baseline sequence 与 checksum；数据库在没有 durable baseline 时不会进入 active。`export` 只读取完整连续 commit，先发布不可变 segment，再原子更新 manifest，最后裁剪源 journal。中断留下的重复 journal 会在下次 export 补做裁剪；未被 manifest 引用的文件由 `verify` 扫描并作为 orphan 报告。

`list` 只读取最多 1 MiB 的 manifest，不读取业务 records。`verify` 有界读取所有已引用 artifact，核对 magic、codec、压缩与大小限制、stored/payload checksum、archive 父链、sequence 和逐 commit checksum。默认每个 segment 最多 1,000 commits/64 MiB，可用 `--max-segment-commits` 与 `--max-segment-bytes` 调低；journal 容量可在 init 时设置。所有 JSON report 都有独立 `version`。增量 restore 和 retention 由后续 [#309](https://github.com/worktools/unionid/issues/309) 交付。

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

Both complete logical backups and incremental archives are available. The v0.5 design uses an optional transactionally embedded journal plus an external portable baseline/segment chain and declares only contiguous, sealed sequences as restore points. See [RFC 0016](rfc/0016-incremental-backup-chains.md) and [#304](https://github.com/worktools/unionid/issues/304) for the full contract and remaining restore/retention work.

`backup::incremental` exposes archive codec 1 plus the `init`, `export`, `list`, and `verify` Rust APIs. The matching CLI is `backup incremental init/export/list/verify`, as shown in the Chinese command block above. Init writes and verifies a baseline from an exclusively opened, fully checked committed view, publishes a prepared manifest, enables the journal in a synchronous transaction, and activates the manifest. This is explicit authorization to upgrade format 6 to format 7. A retry reconciles chain ID, baseline sequence, and checksum; the database cannot become active without a durable baseline.

Export reads only complete contiguous commits, publishes immutable segments and the manifest before pruning the source journal. A retry safely prunes duplicate retained entries after a crash. List reads only the bounded manifest. Verify scans the archive, reports unreferenced files as orphans, and checks every referenced artifact's format, limits, stored and payload checksums, archive parent chain, sequences, and commit checksum chain. Segments default to 1,000 commits/64 MiB and reports carry an independent version. Incremental restore and retention remain in [#309](https://github.com/worktools/unionid/issues/309).

Logical backup preserves the complete `Database` state, stable catalog/type/field/variant/table/index IDs, ADT rows and RowId watermarks, schema identity, migration ledger, and idempotency receipts. It streams from one exclusively opened, fully checked committed redb view, publishes only a flushed and synced complete output, and never overwrites an existing backup or database. Restore validates format, checksum, schema, typed rows, RowIds, indexes, ledger, and receipts before creating a new redb target. Formats 1–4 remain readable according to their frozen compatibility rules.

`import-legacy` is the explicit conversion path for supported prototype snapshot/WAL inputs. It preserves its inputs, requires a nonexistent redb target, and fails on unknown versions, ambiguous values, unsupported syntax, corruption, or noncontiguous sequence history rather than guessing a repair.
