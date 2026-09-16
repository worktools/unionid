#!/usr/bin/env python3
"""Rename legacy `.uid` Unionid source files to the canonical `.unid` extension.

Migration checksums are content-only, so renaming a migration file never
changes what the ledger has already applied. This script only renames files;
update content references (source, scripts, generated commands) afterwards and
run `unionid migration status` to confirm applied/pending is unchanged.

Usage:
    python3 scripts/migrate-uid-to-unid.py [root] [--dry-run] [--keep PREFIX ...]
"""

from __future__ import annotations

import argparse
import pathlib
import sys


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("root", nargs="?", default=".", help="directory to search")
    parser.add_argument("--dry-run", action="store_true", help="print changes only")
    parser.add_argument(
        "--keep",
        action="append",
        default=[],
        help="path prefix to skip (for compatibility fixtures); repeatable",
    )
    args = parser.parse_args()

    root = pathlib.Path(args.root)
    if not root.is_dir():
        parser.error(f"'{root}' is not a directory")

    def skipped(path: pathlib.Path) -> bool:
        text = str(path)
        return any(text.startswith(prefix) for prefix in args.keep)

    files = sorted(
        path for path in root.rglob("*.uid") if not skipped(path)
    )
    renamed = 0
    skipped = 0
    for path in files:
        target = path.with_suffix(".unid")
        if target.exists():
            print(f"skip {path}: destination {target} already exists")
            skipped += 1
            continue
        if args.dry_run:
            print(f"would rename {path} -> {target}")
        else:
            path.rename(target)
            print(f"renamed {path} -> {target}")
        renamed += 1
    if not args.dry_run and renamed:
        print(
            "Update references to the renamed paths, then run "
            "`unionid migration status` to confirm the ledger is unchanged."
        )
    print(f"{renamed} file(s) renamed, {skipped} skipped")
    return 0


if __name__ == "__main__":
    sys.exit(main())
