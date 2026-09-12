#!/usr/bin/env python3
"""Generate and run the repository-owned typed application acceptance."""

import hashlib
import json
import os
import pathlib
import subprocess
import tempfile


ROOT = pathlib.Path(__file__).resolve().parent.parent
FIXTURE = ROOT / "tests" / "fixtures" / "typed-application"


def run_unionid(*arguments, stdout=None):
    """Run the workspace binary with one explicit argument vector."""
    return subprocess.run(
        ["cargo", "run", "--quiet", "--bin", "unionid", "--", *arguments],
        cwd=ROOT,
        stdout=stdout,
        check=True,
    )


def main():
    """Verify generation, drift, persistence, evolution, and typed execution."""
    with tempfile.TemporaryDirectory(prefix="unionid-typed-application-") as temporary:
        temporary = pathlib.Path(temporary)
        reference_db = temporary / "reference.redb"
        application_db = temporary / "application.redb"
        generated_v1 = temporary / "queries_v1.rs"
        generated_v2 = temporary / "queries_v2.rs"
        run_unionid(
            "query", "rust", "--schema", str(FIXTURE / "schema_v1.uid"),
            "--dir", str(FIXTURE / "queries_v1"), "--output", str(generated_v1),
        )
        for source in ["schema_v1.uid", "migrate_v2.uid"]:
            run_unionid(
                "run", "--db", str(reference_db), "--file", str(FIXTURE / source),
                "--format", "json", stdout=subprocess.DEVNULL,
            )

        stale_output = temporary / "stale.rs"
        stale = subprocess.run(
            [
                "cargo", "run", "--quiet", "--bin", "unionid", "--",
                "query", "rust", "--db", str(reference_db),
                "--dir", str(FIXTURE / "queries_v1"), "--output", str(stale_output),
            ],
            cwd=ROOT,
            capture_output=True,
            text=True,
        )
        diagnostic = stale.stdout + stale.stderr
        if stale.returncode == 0 or "non-exhaustive" not in diagnostic or stale_output.exists():
            raise RuntimeError("stale v1 bundle was not rejected without output: " + diagnostic)

        run_unionid(
            "query", "rust", "--db", str(reference_db),
            "--dir", str(FIXTURE / "queries_v2"), "--output", str(generated_v2),
        )
        actual = {
            "queries_v1": hashlib.sha256(generated_v1.read_bytes()).hexdigest(),
            "queries_v2": hashlib.sha256(generated_v2.read_bytes()).hexdigest(),
        }
        expected = json.loads((FIXTURE / "generated-sha256.json").read_text())
        if actual != expected:
            raise RuntimeError(f"typed application bindings drifted: expected {expected}, got {actual}")

        consumer = temporary / "consumer"
        (consumer / "src").mkdir(parents=True)
        (consumer / "src" / "main.rs").write_bytes((FIXTURE / "main.rs").read_bytes())
        root = json.dumps(str(ROOT))
        (consumer / "Cargo.toml").write_text(
            f'''[package]
name = "unionid-typed-application"
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
        environment["UNIONID_TYPED_APP_V1"] = str(generated_v1)
        environment["UNIONID_TYPED_APP_V2"] = str(generated_v2)
        environment["UNIONID_TYPED_APP_DB"] = str(application_db)
        environment["UNIONID_TYPED_APP_SCHEMA"] = str(FIXTURE / "schema_v1.uid")
        environment["UNIONID_TYPED_APP_MIGRATION"] = str(FIXTURE / "migrate_v2.uid")
        subprocess.run(
            ["cargo", "run", "--quiet", "--offline", "--manifest-path", str(consumer / "Cargo.toml")],
            cwd=consumer,
            env=environment,
            check=True,
        )
        run_unionid("check", "--db", str(application_db), "--format", "json", stdout=subprocess.DEVNULL)
        print(json.dumps({"ok": True, "consumer": "repository-owned", "queries": 6, "versions": 2}))


if __name__ == "__main__":
    main()
