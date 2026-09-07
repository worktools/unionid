# 备份、还原与旧格式导入

unionid 使用版本化逻辑备份保存完整 `Database` 状态，包括稳定 catalog/type/field/variant/table/index ID、ADT 行与 RowId 水位、schema revision/hash 和 migration ledger。存在幂等写入回执时，备份还会保存完整 receipt map，确保恢复后的重试不会重复 effect。备份包含格式版本、payload SHA-256 和 schema 元数据；它不是 CSV 投影，也不丢失 i64、sum tag、Option 或嵌套 product。

```text
unionid backup --db app.redb --output app.backup.json
unionid restore --backup app.backup.json --db restored.redb
```

`backup` 通过独占打开 redb 获得一致状态，先验证逻辑行、codec、索引和 ledger，再发布到不存在的输出路径。`restore` 在内存校验格式版本、checksum、schema、所有 typed rows、RowId、索引定义和 ledger head，成功后才创建新的 redb 目标。命令从不覆盖已有备份或数据库；损坏或未知版本的输入不会创建目标。两条命令都支持 `--format json`。

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
