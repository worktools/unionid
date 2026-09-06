# unionid recovery evaluation

This tool creates a representative typed ADT task database in bounded atomic batches, then measures normal reopen and full `check_integrity` in separate child processes. Each phase reports wall time, process peak RSS, and database bytes as JSON.

Run release measurements outside the test suite:

```bash
cargo run --release --locked --manifest-path tools/recovery-eval/Cargo.toml -- /tmp/unionid-recovery-10k.redb 10000
cargo run --release --locked --manifest-path tools/recovery-eval/Cargo.toml -- /tmp/unionid-recovery-100k.redb 100000
```

The workload contains named record and sum types, list and text fields, a primary key, and secondary indexes over a sum value and text. Preparation time is reported separately and is not included in open/check time. On macOS, `ru_maxrss` is already bytes; on Linux it is converted from KiB to bytes.

Run each size on an otherwise idle machine and preserve the command, commit, OS, CPU, memory, Rust version, database size, and complete JSON report. These measurements characterize this implementation and machine; they do not replace physical power-loss or filesystem durability testing.
