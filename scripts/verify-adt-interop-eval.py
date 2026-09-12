#!/usr/bin/env python3
"""Run paired unionid and SQLite+SQLx samples in fresh release processes."""

import argparse
import json
import pathlib
import platform
import subprocess
import tempfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
MANIFEST = ROOT / "tools" / "adt-interop-eval" / "Cargo.toml"
BINARY = ROOT / "tools" / "adt-interop-eval" / "target" / "release" / "unionid-adt-interop-eval"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--rows", type=int, default=100)
    parser.add_argument("--samples", type=int, default=2)
    parser.add_argument("--output", type=pathlib.Path)
    args = parser.parse_args()
    if args.rows <= 0 or args.samples <= 0:
        parser.error("rows and samples must be positive")

    subprocess.run(
        ["cargo", "build", "--release", "--locked", "--manifest-path", str(MANIFEST)],
        cwd=ROOT,
        check=True,
    )
    records = []
    with tempfile.TemporaryDirectory(prefix="unionid-adt-pair-") as temporary:
        temporary = pathlib.Path(temporary)
        for sample in range(args.samples):
            for backend, extension in (("unionid", "redb"), ("sqlite-sqlx", "sqlite")):
                result = subprocess.run(
                    [str(BINARY), backend, str(temporary / f"{backend}-{sample}.{extension}"), str(args.rows)],
                    check=True,
                    capture_output=True,
                    text=True,
                )
                record = json.loads(result.stdout)
                record["sample"] = sample
                records.append(record)

    expected = (args.rows // 2, (args.rows // 2) * 1025, args.rows)
    for record in records:
        actual = (record["matched_rows"], record["total_price_cents"], record["migrated_rows"])
        if actual != expected:
            raise RuntimeError(f"semantic mismatch for {record['backend']}: expected {expected}, got {actual}")

    report = {
        "format_version": 1,
        "base_commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
        "rows": args.rows,
        "samples_per_backend": args.samples,
        "environment": {"system": platform.system(), "machine": platform.machine(), "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip()},
        "records": records,
    }
    encoded = json.dumps(report, ensure_ascii=False, indent=2) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(encoded)
    print(json.dumps({"ok": True, "rows": args.rows, "samples_per_backend": args.samples, "records": len(records)}))


if __name__ == "__main__":
    main()
