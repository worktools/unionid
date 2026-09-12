#!/usr/bin/env python3
"""Generate, compile, and execute static queries from an independent crate."""

import hashlib
import json
import os
import pathlib
import subprocess
import tempfile


ROOT = pathlib.Path(__file__).resolve().parent.parent
FIXTURE = ROOT / "tests" / "fixtures" / "query-binding-consumer"


def main():
    """Verify generated bindings, drift diagnostics, and v1/v2 execution."""
    with tempfile.TemporaryDirectory(prefix="unionid-query-binding-") as temporary:
        temporary = pathlib.Path(temporary)
        generated = temporary / "find_task.rs"
        mutation = temporary / "create_task.rs"
        query_v1 = temporary / "classify_task_v1.rs"
        query_v2 = temporary / "classify_task_v2.rs"
        evolved_db = temporary / "evolved.redb"
        subprocess.run(
            [
                "cargo",
                "run",
                "--quiet",
                "--bin",
                "unionid",
                "--",
                "query",
                "rust",
                "--schema",
                str(FIXTURE / "schema.uid"),
                "--file",
                str(FIXTURE / "find_task.uid"),
                "--output",
                str(generated),
            ],
            cwd=ROOT,
            check=True,
        )
        subprocess.run(
            [
                "cargo",
                "run",
                "--quiet",
                "--bin",
                "unionid",
                "--",
                "query",
                "rust",
                "--schema",
                str(FIXTURE / "schema.uid"),
                "--file",
                str(FIXTURE / "create_task.uid"),
                "--output",
                str(mutation),
            ],
            cwd=ROOT,
            check=True,
        )
        subprocess.run(
            [
                "cargo",
                "run",
                "--quiet",
                "--bin",
                "unionid",
                "--",
                "query",
                "rust",
                "--schema",
                str(FIXTURE / "schema.uid"),
                "--file",
                str(FIXTURE / "classify_task_v1.uid"),
                "--output",
                str(query_v1),
            ],
            cwd=ROOT,
            check=True,
        )
        for source in ["schema.uid", "migrate_v2.uid"]:
            subprocess.run(
                [
                    "cargo",
                    "run",
                    "--quiet",
                    "--bin",
                    "unionid",
                    "--",
                    "run",
                    "--db",
                    str(evolved_db),
                    "--file",
                    str(FIXTURE / source),
                    "--format",
                    "json",
                ],
                cwd=ROOT,
                stdout=subprocess.DEVNULL,
                check=True,
            )
        stale_match = subprocess.run(
            [
                "cargo",
                "run",
                "--quiet",
                "--bin",
                "unionid",
                "--",
                "query",
                "rust",
                "--db",
                str(evolved_db),
                "--file",
                str(FIXTURE / "classify_task_v1.uid"),
                "--output",
                str(temporary / "stale.rs"),
            ],
            cwd=ROOT,
            capture_output=True,
            text=True,
        )
        stale_diagnostic = stale_match.stdout + stale_match.stderr
        if stale_match.returncode == 0 or "non-exhaustive" not in stale_diagnostic:
            raise RuntimeError(
                "adding a variant did not reject the stale exhaustive query: "
                + stale_diagnostic
            )
        subprocess.run(
            [
                "cargo",
                "run",
                "--quiet",
                "--bin",
                "unionid",
                "--",
                "query",
                "rust",
                "--db",
                str(evolved_db),
                "--file",
                str(FIXTURE / "classify_task_v2.uid"),
                "--output",
                str(query_v2),
            ],
            cwd=ROOT,
            check=True,
        )
        expected = json.loads((FIXTURE / "generated-sha256.json").read_text())
        actual = {
            "classify_task_v1": hashlib.sha256(query_v1.read_bytes()).hexdigest(),
            "classify_task_v2": hashlib.sha256(query_v2.read_bytes()).hexdigest(),
            "create_task": hashlib.sha256(mutation.read_bytes()).hexdigest(),
            "find_task": hashlib.sha256(generated.read_bytes()).hexdigest(),
        }
        if actual != expected:
            raise RuntimeError(
                "generated query bindings drifted; review the API and update "
                f"generated-sha256.json: expected {expected}, got {actual}"
            )
        consumer = temporary / "consumer"
        (consumer / "src").mkdir(parents=True)
        (consumer / "src" / "main.rs").write_bytes((FIXTURE / "main.rs").read_bytes())
        root = json.dumps(str(ROOT))
        (consumer / "Cargo.toml").write_text(
            f'''[package]
name = "unionid-query-binding-consumer"
version = "0.0.0"
edition = "2024"
rust-version = "1.94"

[dependencies]
serde = {{ version = "1", features = ["derive"] }}
unionid = {{ path = {root} }}
'''
        )
        environment = os.environ.copy()
        environment["CARGO_TARGET_DIR"] = str(ROOT / "target")
        environment["UNIONID_GENERATED_QUERY"] = str(generated)
        environment["UNIONID_GENERATED_MUTATION"] = str(mutation)
        environment["UNIONID_GENERATED_QUERY_V1"] = str(query_v1)
        environment["UNIONID_GENERATED_QUERY_V2"] = str(query_v2)
        environment["UNIONID_SCHEMA"] = str(FIXTURE / "schema.uid")
        environment["UNIONID_EVOLVED_DB"] = str(evolved_db)
        subprocess.run(
            ["cargo", "run", "--quiet", "--offline", "--manifest-path", str(consumer / "Cargo.toml")],
            cwd=consumer,
            env=environment,
            check=True,
        )
        print(
            json.dumps(
                {
                    "ok": True,
                    "consumer": "independent",
                    "queries": [
                        "classify_task_v1",
                        "classify_task_v2",
                        "create_task",
                        "find_task",
                    ],
                }
            )
        )


if __name__ == "__main__":
    main()
