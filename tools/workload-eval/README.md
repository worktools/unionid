# unionid workload evaluation

This standalone tool prepares a representative typed ADT task database, then measures primary-key lookup, secondary-index lookup, controlled full scan, conditional update, upsert, atomic batch insert, and deep migration in release-mode child processes.

```bash
cargo run --release --locked --manifest-path tools/workload-eval/Cargo.toml -- /tmp/unionid-workload-10k.redb 10000
cargo run --release --locked --manifest-path tools/workload-eval/Cargo.toml -- /tmp/unionid-workload-100k.redb 100000
```

The optional third argument is the sample count and defaults to 20; the optional fourth is preparation batch size and defaults to 3,000. Data construction, query-plan checks, source generation, database cloning, and result assertions are outside each timed interval. The JSON output preserves every sample in microseconds together with p50/p95, database bytes, and per-process peak RSS.

Each case runs on a fresh copy of the prepared database. Query and write cases collect repeated samples in one process after five warm-ups. Each migration sample uses a separate fresh copy and process. Peak RSS includes database open and warm-up state for that phase. Results characterize one build and machine rather than promising latency on other systems.
