# ADT application paired evaluator / ADT 应用配对评估器

## 中文说明

此工具为 issue #290 在独立 release 进程中运行同一任务应用。两端均使用单文件数据库、同步持久提交、同一行数和操作顺序。unionid 保存原生 sum/product/option/decimal/timestamp；SQLite + SQLx 使用受约束的关系列、整数缩放 decimal、tag/payload 列和显式嵌套 option presence bit。

运行 `python3 scripts/verify-adt-interop-eval.py` 执行小型 Ubuntu 验收。带版本的原始样本由该脚本保存，日常 CI 只做较小的重复性验证。

## English Description

This tool runs the same task application for issue #290 in independent release processes. Both sides use a single database file, synchronous durable commits, the same row count, and the same operation sequence. unionid stores native sum/product/option/decimal/timestamp values; SQLite + SQLx uses constrained relational columns, scaled-integer decimals, tag/payload columns, and an explicit nested-option presence bit.

Run `python3 scripts/verify-adt-interop-eval.py` for the small Ubuntu acceptance. The script can retain versioned raw samples; ordinary CI runs only the smaller repeatability check.
