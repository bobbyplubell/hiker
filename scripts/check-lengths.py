#!/usr/bin/env python3
"""File-length budget enforcement.

Hard caps; no allowlist, no per-file overrides. Tighten thresholds by
editing the constants below; loosening them is a deliberate posture
change, not an agent's escape hatch. See scripts/check.sh.

Function-length budgets are enforced by clippy (`clippy::too_many_lines`,
configured in clippy.toml) for Rust.

File cap:
- Rust: 1500 lines (covers the production Rust crates listed in
  `_rust_roots.py`, shared with `check-splits.py`)
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

from _rust_roots import RUST_ROOTS

RUST_FILE_CAP = 1500

SKIP_DIRS = ("node_modules", "dist", "target")

REPO_ROOT = Path(__file__).resolve().parent.parent


def iter_files(roots: list[str], suffix: str) -> list[Path]:
    out: list[Path] = []
    for root in roots:
        root_path = REPO_ROOT / root
        if not root_path.exists():
            continue
        for dirpath, dirnames, filenames in os.walk(root_path):
            dirnames[:] = [d for d in dirnames if d not in SKIP_DIRS]
            for fname in filenames:
                if not fname.endswith(suffix):
                    continue
                out.append(Path(dirpath) / fname)
    return sorted(out)


def main() -> int:
    failures: list[str] = []

    for f in iter_files(RUST_ROOTS, ".rs"):
        lines = sum(1 for _ in f.open(errors="replace"))
        if lines > RUST_FILE_CAP:
            rel = f.relative_to(REPO_ROOT)
            failures.append(f"  {rel}: {lines} lines (cap {RUST_FILE_CAP})")

    if failures:
        print("file-length violations:", file=sys.stderr)
        for v in failures:
            print(v, file=sys.stderr)
        print(f"\nfile-length: {len(failures)} violation(s)", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
