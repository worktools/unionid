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
    with tempfile.TemporaryDirectory(prefix="unionid-query-binding-") as temporary:
        temporary = pathlib.Path(temporary)
        generated = temporary / "find_task.rs"
        mutation = temporary / "create_task.rs"
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
        expected = json.loads((FIXTURE / "generated-sha256.json").read_text())
        actual = {
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
        environment["UNIONID_SCHEMA"] = str(FIXTURE / "schema.uid")
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
                    "queries": ["create_task", "find_task"],
                }
            )
        )


if __name__ == "__main__":
    main()
