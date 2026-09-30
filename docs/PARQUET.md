# 本地 Parquet 查看

`unionid parquet` 直接读取一个本地 Parquet 文件，输出文件行数、row group 数量、推断后的 UnionID 结构类型，以及有限行预览。它不会创建 redb 文件、修改源文件或把文件访问开放给 TCP/HTTP 服务。

```bash
unionid parquet events.parquet
unionid parquet events.parquet --limit 50
unionid parquet events.parquet --limit 20 --format json
```

默认读取前 20 行；`--limit 0` 只检查 metadata 和 schema；硬上限为 1,000 行，预览工作内存上限为 64 MiB。读取按最多 1,024 行的 Arrow batch 进行，因此不会为了显示少量数据先把完整文件物化到内存。

当前映射保持无损：

| Parquet / Arrow | UnionID |
| --- | --- |
| signed integer、可装入 `i64` 的 unsigned integer | `int` |
| `float32`、`float64` | `float` |
| UTF-8 string | `text` |
| binary、fixed-size binary | `bytes` |
| decimal128，precision 1–38 且 scale 非负 | `Decimal<P, S>` |
| date32/date64 | `date` |
| 带 UTC timezone metadata 的 timestamp | `timestamp` |
| duration，可精确表示到 microsecond | `duration` |
| struct | 匿名 record/product type |
| list | `List<T>` |
| text-key map | `Map<text, T>` |
| nullable field | `Option<T>` |

第三方 Parquet schema 只携带结构信息时，UnionID 不会根据字符串或 tagged record 猜测命名 enum。无法无损映射的 Arrow 类型、非 UTC timestamp、超过 microsecond 精度的值、超出 `i64` 的 unsigned integer，以及非 text key map 都会返回 `E_PARQUET_TYPE` 或 `E_PARQUET_VALUE`，并带字段路径。

JSON 输出是 version 1 inspection envelope，包含 `path`、`rows_total`、`row_groups`、`columns`、`preview_rows` 和 `preview_truncated`。预览值使用 UnionID 的 typed `Value` 表示，因此 `Option::None`、null、bytes、decimal 和 temporal 值不会退化成含糊字符串。

当前命令只负责 schema 和有界预览。通过 `from data` 执行 UnionID pipeline、列投影下推和交互查询由后续 v0.11 工作补充。多文件/glob、Hive partition、schema union、远程对象存储、并行扫描、predicate pruning 和 Parquet 导出属于按真实需求推进的探索范围。

# Local Parquet inspection

`unionid parquet` reads one local Parquet file and prints its row count, row-group count, inferred structural UnionID types, and a bounded row preview. It does not create a redb database, modify the source file, or expose file access through the TCP or HTTP service.

```bash
unionid parquet events.parquet
unionid parquet events.parquet --limit 50
unionid parquet events.parquet --limit 20 --format json
```

The command previews 20 rows by default. `--limit 0` reads metadata and schema only. The hard limit is 1,000 rows and preview working memory is capped at 64 MiB. Input is decoded in Arrow batches of at most 1,024 rows, so a small preview does not materialize the complete file first.

The current mapping is lossless: signed integers and unsigned integers that fit `i64` become `int`; float32/64 become `float`; UTF-8 becomes `text`; binary becomes `bytes`; compatible decimal128, date, UTC timestamp, and duration values retain their production scalar types. Struct, list, text-key map, and nullable fields become anonymous records, `List<T>`, `Map<text, T>`, and `Option<T>` respectively.

UnionID does not guess nominal enums when a third-party Parquet schema only carries structural information. Unsupported Arrow types, timestamps without UTC metadata, values finer than microseconds, unsigned values outside `i64`, and maps without text keys fail with `E_PARQUET_TYPE` or `E_PARQUET_VALUE` and identify the field path.

JSON output is a version 1 inspection envelope containing `path`, `rows_total`, `row_groups`, `columns`, `preview_rows`, and `preview_truncated`. Preview cells retain UnionID's typed `Value` representation, preserving `Option::None`, null, bytes, decimal, and temporal distinctions.

This command currently covers schema and bounded preview only. Querying the file with `from data`, projection pushdown, and an interactive workflow are separate v0.11 work. Multiple files and globs, Hive partitions, schema union, remote object storage, parallel scans, predicate pruning, and Parquet export remain demand-driven exploration.
