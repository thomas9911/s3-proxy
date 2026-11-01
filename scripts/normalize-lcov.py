#!/usr/bin/env python3

from __future__ import annotations

import os
import sys
from pathlib import Path


def normalize_sf_line(line: str, repo_root: Path) -> str:
    if not line.startswith("SF:"):
        return line

    raw_path = line[3:].rstrip("\r\n")
    full_path = Path(raw_path)
    if not full_path.is_absolute():
        full_path = (repo_root / full_path).resolve()

    try:
        relative = full_path.relative_to(repo_root)
    except ValueError:
        return line

    return f"SF:.{os.sep}{relative}\n"


def main() -> int:
    if len(sys.argv) != 3:
        print("usage: normalize-lcov.py <input> <output>", file=sys.stderr)
        return 2

    input_path = Path(sys.argv[1])
    output_path = Path(sys.argv[2])
    repo_root = Path.cwd().resolve()

    with input_path.open("r", encoding="utf-8") as src, output_path.open(
        "w", encoding="utf-8", newline=""
    ) as dst:
        for line in src:
            dst.write(normalize_sf_line(line, repo_root))

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
