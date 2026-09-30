# 本地 Parquet 查看

`unionid parquet` 直接读取一个本地 Parquet 文件，输出文件行数、row group 数量、推断后的 UnionID 结构类型，以及有限行预览。它不会创建 redb 文件、修改源文件或把文件访问开放给 TCP/HTTP 服务。

```bash
unionid parquet events.parquet
unionid parquet events.parquet --limit 50
unionid parquet events.parquet --limit 20 --format json
```

默认读取前 20 行；`--limit 0` 只检查 metadata 和 schema；硬上限为 1,000 行，转换后保留的 preview payload 上限为 64 MiB。读取按最多 1,024 行的 Arrow batch 进行，因此不会为了显示少量数据先把完整文件物化到内存；64 MiB 限制不表示 Arrow 解码过程的瞬时内存上限。

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

## 直接查询

`--query` 把当前文件暴露为请求级只读表 `data`，并复用数据库本身的 parser、类型绑定、表达式和 pipeline 执行器：

```bash
unionid parquet events.parquet \
  --query 'from data | filter active | select {id, profile.city} | take 20'

unionid parquet events.parquet \
  --query 'from data | group category {aggregate {events = count, total = sum amount}} | sort -total' \
  --format json

unionid parquet events.parquet --interactive
```

查询是只读的；insert、upsert、update、delete、DDL 和 migration 会在读取 row batch 前返回 `E_READ_ONLY`。未被 filter、derive、aggregate、window、sort 或最终结果使用的顶层列会下推到 Parquet reader，不参与 Arrow 解码。`explain` 的 plan 会显示 `external_scan: "parquet_scan"` 和 `projected_columns`；表格输出显示同样的 scan 与 projection。

扫描保持最多 1,024 行、16 MiB typed batch 的边界，并沿用查询执行器的结果行数、工作内存、deadline 和 cancellation 检查。当前只接受可直接写进 UnionID query 的 ASCII 字段名；不符合标识符规则的字段返回 `E_PARQUET_TYPE`，提示先重命名。多文件/glob、Hive partition、schema union、远程对象存储、并行扫描、predicate pruning、稳定 page cursor 和 Parquet 导出仍属于按真实需求推进的探索范围。

# Local Parquet inspection

`unionid parquet` reads one local Parquet file and prints its row count, row-group count, inferred structural UnionID types, and a bounded row preview. It does not create a redb database, modify the source file, or expose file access through the TCP or HTTP service.

```bash
unionid parquet events.parquet
unionid parquet events.parquet --limit 50
unionid parquet events.parquet --limit 20 --format json
```

The command previews 20 rows by default. `--limit 0` reads metadata and schema only. The hard limit is 1,000 rows, and the retained serialized preview payload is capped at 64 MiB. Input is decoded in Arrow batches of at most 1,024 rows, so a small preview does not materialize the complete file first; the 64 MiB retained-payload bound is not a bound on transient Arrow decode memory.

The current mapping is lossless: signed integers and unsigned integers that fit `i64` become `int`; float32/64 become `float`; UTF-8 becomes `text`; binary becomes `bytes`; compatible decimal128, date, UTC timestamp, and duration values retain their production scalar types. Struct, list, text-key map, and nullable fields become anonymous records, `List<T>`, `Map<text, T>`, and `Option<T>` respectively.

UnionID does not guess nominal enums when a third-party Parquet schema only carries structural information. Unsupported Arrow types, timestamps without UTC metadata, values finer than microseconds, unsigned values outside `i64`, and maps without text keys fail with `E_PARQUET_TYPE` or `E_PARQUET_VALUE` and identify the field path.

JSON output is a version 1 inspection envelope containing `path`, `rows_total`, `row_groups`, `columns`, `preview_rows`, and `preview_truncated`. Preview cells retain UnionID's typed `Value` representation, preserving `Option::None`, null, bytes, decimal, and temporal distinctions.

## Direct queries

`--query` exposes the current file as the request-local, read-only table `data`. It uses the database's normal parser, type binder, expressions, and pipeline executor:

```bash
unionid parquet events.parquet \
  --query 'from data | filter active | select {id, profile.city} | take 20'

unionid parquet events.parquet \
  --query 'from data | group category {aggregate {events = count, total = sum amount}} | sort -total' \
  --format json

unionid parquet events.parquet --interactive
```

Queries are read-only. Insert, upsert, update, delete, DDL, and migration statements fail with `E_READ_ONLY` before a row batch is read. Top-level columns unused by filters, derives, aggregates, windows, sorting, or the final result are pushed into the Parquet reader and are not decoded by Arrow. An `explain` plan reports `external_scan: "parquet_scan"` and `projected_columns`; table output presents the same scan and projection.

Scanning retains the executor's limits: at most 1,024 rows and 16 MiB of typed data per source batch, plus the existing result-row, working-memory, deadline, and cancellation checks. Field names must currently be ASCII identifiers that can be written directly in a UnionID query; incompatible names fail with `E_PARQUET_TYPE` and a rename hint. Multiple files and globs, Hive partitions, schema union, remote object storage, parallel scans, predicate pruning, stable page cursors, and Parquet export remain demand-driven exploration.
