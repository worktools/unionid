# unionid concurrency evaluation

This standalone tool compares the old whole-request `Mutex<Engine>` behavior
with bounded read snapshots for the same typed full-scan, derive, sort, and
aggregate workload. Each mode runs in a separate process so peak RSS remains
comparable.

```bash
cargo run --release --locked --manifest-path tools/concurrency-eval/Cargo.toml -- 10000 8 10
```

Arguments are row count, concurrent readers (maximum 8), and queries per reader.
The JSON report includes wall-clock throughput, throughput ratio, peak RSS,
memory ratio, and observed peak read concurrency. Results characterize one
machine and build; they are not capacity promises.
