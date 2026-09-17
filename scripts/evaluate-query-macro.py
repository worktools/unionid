#!/usr/bin/env python3
"""Measure and verify an independent inline-query macro consumer."""

import argparse
import json
import os
import pathlib
import platform
import subprocess
import tempfile
import time


ROOT = pathlib.Path(__file__).resolve().parent.parent

SCHEMA_V1 = """enum State {
  Pending
  Running {worker: text}
  Done {result: text}
}

struct Task {
  id: int
  title: text
  state: State
  priority: int
}

table tasks: Task {
  key id
}
"""


def main_source(limit: int) -> str:
    """Return one small but representative macro consumer."""
    return f'''unionid_query::queries! {{
    schema "schema.unid"

    query find_pending {{
        from tasks
        filter state == Pending && priority >= $minimum
        sort {{-priority, id}}
        select {{id, title, state}}
        take {limit}
    }}

    query running_for_worker {{
        from tasks
        filter match state {{
            Pending => false
            Running {{worker}} => worker == $worker
            Done {{..}} => false
        }}
        select {{id, state}}
    }}
}}

fn main() {{
    println!("{{}}", find_pending::FIND_PENDING_DIGEST);
    println!("{{}}", find_pending::FIND_PENDING_SCHEMA_HASH);
    println!("{{}}", find_pending::FIND_PENDING_SOURCE);
}}
'''


def run(command: list[str], *, cwd: pathlib.Path, env: dict[str, str]) -> tuple[int, str]:
    """Run one command and return elapsed milliseconds and stdout."""
    started = time.perf_counter_ns()
    completed = subprocess.run(
        command,
        cwd=cwd,
        env=env,
        capture_output=True,
        text=True,
    )
    elapsed_ms = (time.perf_counter_ns() - started) // 1_000_000
    if completed.returncode != 0:
        raise RuntimeError(
            f"command failed ({' '.join(command)}):\n{completed.stdout}{completed.stderr}"
        )
    return elapsed_ms, completed.stdout


def directory_bytes(path: pathlib.Path) -> int:
    """Return the total regular-file size below path."""
    return sum(item.stat().st_size for item in path.rglob("*") if item.is_file())


def macro_artifact_bytes(path: pathlib.Path) -> int:
    """Return the largest compiled proc-macro shared library."""
    candidates = []
    for suffix in (".so", ".dylib", ".dll"):
        candidates.extend(path.rglob(f"libunionid_query-*{suffix}"))
        candidates.extend(path.rglob(f"unionid_query-*{suffix}"))
    return max((item.stat().st_size for item in candidates), default=0)


def read_identity(stdout: str) -> tuple[str, str, str]:
    """Read digest, schema hash, and canonical source from the consumer."""
    digest, schema_hash, *source = stdout.splitlines()
    if not digest.startswith("sha256:") or not schema_hash.startswith("sha256:"):
        raise RuntimeError(f"unexpected generated identity output: {stdout}")
    return digest, schema_hash, "\n".join(source)


def evaluate() -> dict[str, object]:
    """Verify rebuild behavior and return value-free compile evidence."""
    with tempfile.TemporaryDirectory(prefix="unionid-query-macro-eval-") as temporary:
        work = pathlib.Path(temporary)
        baseline = work / "baseline"
        consumer = work / "consumer"
        source = consumer / "src" / "main.rs"
        schema = consumer / "schema.unid"
        target = work / "target"
        baseline.joinpath("src").mkdir(parents=True)
        baseline.joinpath("src", "main.rs").write_text("fn main() {}\n")
        baseline.joinpath("Cargo.toml").write_text(
            f'''[package]
name = "unionid-query-macro-baseline"
version = "0.0.0"
edition = "2024"
rust-version = "1.94"

[dependencies]
serde = {{ version = "1", features = ["derive"] }}
unionid = {{ path = {json.dumps(str(ROOT))} }}
'''
        )
        baseline_environment = os.environ.copy()
        baseline_target = work / "baseline-target"
        baseline_environment["CARGO_TARGET_DIR"] = str(baseline_target)
        baseline_environment["CARGO_INCREMENTAL"] = "1"
        run(
            ["cargo", "generate-lockfile", "--offline"],
            cwd=baseline,
            env=baseline_environment,
        )
        baseline_ms, _ = run(
            ["cargo", "check", "--locked", "--offline"],
            cwd=baseline,
            env=baseline_environment,
        )
        baseline_bytes = directory_bytes(baseline_target)

        source.parent.mkdir(parents=True)
        source.write_text(main_source(20))
        schema.write_text(SCHEMA_V1)
        consumer.joinpath("Cargo.toml").write_text(
            f'''[package]
name = "unionid-query-macro-eval"
version = "0.0.0"
edition = "2024"
rust-version = "1.94"

[dependencies]
serde = {{ version = "1", features = ["derive"] }}
unionid = {{ path = {json.dumps(str(ROOT))} }}
unionid-query = {{ path = {json.dumps(str(ROOT / "query-macro"))} }}
'''
        )
        environment = os.environ.copy()
        environment["CARGO_TARGET_DIR"] = str(target)
        environment["CARGO_INCREMENTAL"] = "1"
        run(["cargo", "generate-lockfile", "--offline"], cwd=consumer, env=environment)

        clean_ms, _ = run(
            ["cargo", "check", "--locked", "--offline"], cwd=consumer, env=environment
        )
        target_bytes = directory_bytes(target)
        proc_macro_bytes = macro_artifact_bytes(target)
        noop_ms, _ = run(
            ["cargo", "check", "--locked", "--offline"], cwd=consumer, env=environment
        )
        _, initial_output = run(
            ["cargo", "run", "--quiet", "--locked", "--offline"],
            cwd=consumer,
            env=environment,
        )
        initial_digest, initial_schema, initial_source = read_identity(initial_output)
        if "take 20" not in initial_source:
            raise RuntimeError("initial canonical query did not retain take 20")

        source.write_text(main_source(21))
        query_ms, query_output = run(
            ["cargo", "run", "--quiet", "--locked", "--offline"],
            cwd=consumer,
            env=environment,
        )
        query_digest, query_schema, query_source = read_identity(query_output)
        if query_digest == initial_digest or query_schema != initial_schema:
            raise RuntimeError("query edit did not update only the query digest")
        if "take 21" not in query_source:
            raise RuntimeError("query edit did not re-expand the inline macro")

        schema.write_text(
            SCHEMA_V1.replace("  priority: int\n", "  priority: int\n  note: option text = None\n")
        )
        schema_ms, schema_output = run(
            ["cargo", "run", "--quiet", "--locked", "--offline"],
            cwd=consumer,
            env=environment,
        )
        schema_digest, changed_schema, _ = read_identity(schema_output)
        if schema_digest != query_digest or changed_schema == query_schema:
            raise RuntimeError("schema edit did not update only the generated schema identity")
        if list(work.rglob("*.redb")):
            raise RuntimeError("query macro evaluation unexpectedly opened a redb database")

        return {
            "schema_version": 1,
            "ok": True,
            "environment": {
                "system": platform.system(),
                "machine": platform.machine(),
                "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
            },
            "consumer": {
                "queries": 2,
                "baseline_clean_check_ms": baseline_ms,
                "clean_check_ms": clean_ms,
                "macro_clean_overhead_ms": max(0, clean_ms - baseline_ms),
                "noop_check_ms": noop_ms,
                "query_rebuild_run_ms": query_ms,
                "schema_rebuild_run_ms": schema_ms,
                "target_bytes_after_check": target_bytes,
                "baseline_target_bytes_after_check": baseline_bytes,
                "macro_target_overhead_bytes": max(0, target_bytes - baseline_bytes),
                "proc_macro_artifact_bytes": proc_macro_bytes,
            },
            "verified": {
                "query_edit_changed_digest": True,
                "schema_edit_changed_schema_hash": True,
                "schema_edit_preserved_query_digest": True,
                "redb_opened_at_compile_time": False,
            },
        }


def main() -> None:
    """Write one JSON evidence record and echo it for CI logs."""
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=pathlib.Path, required=True)
    args = parser.parse_args()
    report = evaluate()
    encoded = json.dumps(report, indent=2, sort_keys=True) + "\n"
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(encoded)
    print(encoded, end="")


if __name__ == "__main__":
    main()
