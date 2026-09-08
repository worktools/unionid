# unionid workload evaluation

This standalone tool prepares a representative typed ADT task database, then measures independent database open, primary-key lookup, secondary-index lookup, controlled full scan, composite range access, composite index order, forward/backward cursor seeks, conditional update, upsert, atomic batch insert, and deep migration in release-mode child processes.

```bash
cargo run --release --locked --manifest-path tools/workload-eval/Cargo.toml -- /tmp/unionid-workload-10k.redb 10000
cargo run --release --locked --manifest-path tools/workload-eval/Cargo.toml -- /tmp/unionid-workload-100k.redb 100000
```

The optional third argument is the sample count and defaults to 20; the optional fourth is preparation batch size and defaults to 1,500. Data construction, structured query-plan checks, page-cursor construction, source generation, database cloning, integrity checks, and result assertions are outside each timed interval. The JSON output preserves every sample in microseconds together with p50/p95, database bytes, schema identity, host environment, and per-process peak RSS. Write cases additionally report candidate-state construction and durable-commit samples separately. Each ordinary write validates its incremental/full-rebuild mode, touched-table count, and row/index write-set shape before the sample is accepted.

Each case runs on a fresh copy of the prepared database. Query and write cases collect repeated samples in one process after five warm-ups. Each open and migration sample uses a separate fresh copy and process. Every child reopens the database and verifies integrity outside the timed interval; ordered cases also assert the expected equality prefix, range, traversal, page boundary, and `limit + 1` read budget before samples are accepted. Peak RSS includes database open and warm-up state for that phase. Results characterize one build and machine rather than promising latency on other systems.
