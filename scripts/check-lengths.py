#!/usr/bin/env python3
"""File-length budget enforcement.

Hard caps; no allowlist, no per-file overrides. Tighten thresholds by
editing the constants below; loosening them is a deliberate posture
change, not an agent's escape hatch. See scripts/check.sh.

Function-length budgets are enforced by clippy (`clippy::too_many_lines`,
configured in clippy.toml) for Rust. TypeScript function-length isn't
enforced today; file-length pressures the worst TS files.

File caps:
- Rust: 1500 lines (covers `core/`, `cli/`, `mcp-server/`, `ui/src-tauri/`)
- TypeScript: 1200 lines (denser per line than Rust)
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

RUST_FILE_CAP = 1500
TS_FILE_CAP = 1200

RUST_ROOTS = ["core/src", "mcp-server/src", "cli/src", "ui/src-tauri/src"]
TS_ROOTS = ["ui/src"]
TS_SKIP_SUFFIXES = (".d.ts",)
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
                if any(fname.endswith(s) for s in TS_SKIP_SUFFIXES):
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

    for f in iter_files(TS_ROOTS, ".ts"):
        lines = sum(1 for _ in f.open(errors="replace"))
        if lines > TS_FILE_CAP:
            rel = f.relative_to(REPO_ROOT)
            failures.append(f"  {rel}: {lines} lines (cap {TS_FILE_CAP})")

    if failures:
        print("file-length violations:", file=sys.stderr)
        for v in failures:
            print(v, file=sys.stderr)
        print(f"\nfile-length: {len(failures)} violation(s)", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
