#!/usr/bin/env python3
"""Reject migration number collisions and gaps without building the API."""
import argparse
from pathlib import Path
import re
import sys

MIGRATIONS = Path(__file__).resolve().parents[1] / "apps/api/migrations"


def check(directory):
    versions = {}
    errors = []
    files = sorted(path.name for path in directory.iterdir() if path.is_file() and path.suffix == ".sql")
    if not files:
        return [f"no SQL migrations found in {directory}"]
    for name in files:
        match = re.fullmatch(r"([0-9]{4})_.+\.sql", name)
        if not match or int(match[1]) == 0:
            errors.append(f"invalid migration filename {name}; expected NNNN_description.sql starting at 0001")
            continue
        versions.setdefault(int(match[1]), []).append(name)
    for version, names in sorted(versions.items()):
        if len(names) > 1:
            errors.append(f"duplicate migration prefix {version:04d}: {', '.join(names)}")
    missing = sorted(set(range(1, max(versions, default=0) + 1)) - versions.keys())
    if missing:
        errors.append("missing migration prefixes: " + ", ".join(f"{version:04d}" for version in missing))
    return errors


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", nargs="?", type=Path, default=MIGRATIONS)
    args = parser.parse_args()
    try:
        errors = check(args.directory)
    except OSError as error:
        parser.error(str(error))
    if errors:
        for error in errors:
            print(error, file=sys.stderr)
        print("Fetch origin/main, rebase, and give only new migrations the next contiguous numbers. "
              "Never renumber a shipped migration.", file=sys.stderr)
        return 1
    print("Migration prefixes are unique and contiguous from 0001.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
