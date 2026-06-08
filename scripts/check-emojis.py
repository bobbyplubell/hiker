#!/usr/bin/env python3
"""Fail when any tracked source file contains emoji characters.

Scope: Rust + Python + shell sources under `app/`, `core/`, and
`scripts/`. The legacy `ui/` tree predates this rule and is excluded.

Why: emoji glyphs render as tofu squares in egui's default font, and
they have a habit of slipping into UI label strings via comments and
copy-paste. Banning the codepoint range outright catches both the
strings and the comments before they ship.

Detected codepoint ranges (the "emoji-y" + known-tofu Unicode blocks):
  U+1F000 - U+1FFFF  (Supplementary plane: emoticons, pictographs, etc.)
  U+1F100 - U+1F2FF  (Enclosed alphanumerics / ideographic supplement)
  U+25A0  - U+25FF   (Geometric Shapes — small triangles, bullet, etc.)
  U+2600  - U+26FF   (Misc Symbols — sun, warning sign, etc.)
  U+2700  - U+27BF   (Dingbats — checkmarks, crosses)

Chars that DO render in egui's bundled font and are intentionally
allowed (so the rule doesn't churn the existing comment-heavy code):
  - em/en dashes (U+2013, U+2014)
  - arrows (U+2190-U+21FF) — used freely in doc comments
  - ellipsis (U+2026) — used in many UI labels
  - misc technical symbols already in active use

Test code is exempt: tests never render in the UI, and emoji are useful
there as UTF-8 / multibyte fixtures. Exempt = whole test files (a `tests.rs`,
a `*_test(s).rs`, or anything under a `tests/` dir) plus inline
`#[cfg(test)]` modules in any file. Non-test code stays banned: a single
tofu char in a real label breaks rendering.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SCAN_DIRS = ("app", "core", "scripts")
SCAN_EXTS = (".rs", ".py", ".sh", ".toml")
SELF = Path(__file__).resolve()

EMOJI_RANGES = [
    (0x1F000, 0x1FFFF),
    (0x25A0, 0x25FF),
    (0x2600, 0x26FF),
    (0x2700, 0x27BF),
]


def is_emoji(cp: int) -> bool:
    return any(lo <= cp <= hi for lo, hi in EMOJI_RANGES)


EMOJI_RE = re.compile(
    "[" + "".join(f"\\U{lo:08X}-\\U{hi:08X}" for lo, hi in EMOJI_RANGES) + "]"
)


def is_test_file(rel: Path) -> bool:
    """Whole-file exemption: a dedicated test file or anything under tests/."""
    name = rel.name
    return (
        "tests" in rel.parts
        or name in ("tests.rs", "test.rs")
        or name.endswith("_tests.rs")
        or name.endswith("_test.rs")
    )


def cfg_test_lines(lines: list[str]) -> set[int]:
    """1-based line numbers that fall inside a `#[cfg(test)]` module.

    Brace-counts from each `#[cfg(test)]` attribute through the matching close
    of the module it guards. Heuristic (counts braces in strings/comments too),
    which is fine here: emoji inside a test module are allowed regardless.
    """
    exempt: set[int] = set()
    i, n = 0, len(lines)
    while i < n:
        if "#[cfg(test)]" not in lines[i]:
            i += 1
            continue
        depth, opened = 0, False
        j = i
        while j < n:
            depth += lines[j].count("{") - lines[j].count("}")
            opened = opened or "{" in lines[j]
            exempt.add(j + 1)
            if opened and depth <= 0:
                break
            j += 1
        i = j + 1
    return exempt


def scan_file(path: Path) -> list[tuple[int, int, str, str]]:
    """Return (line_no, col, char, line_text) tuples for every hit."""
    hits: list[tuple[int, int, str, str]] = []
    try:
        text = path.read_text(encoding="utf-8", errors="strict")
    except (UnicodeDecodeError, OSError):
        return hits
    lines = text.splitlines()
    exempt = cfg_test_lines(lines)
    for line_no, line in enumerate(lines, start=1):
        if line_no in exempt:
            continue
        for m in EMOJI_RE.finditer(line):
            hits.append((line_no, m.start() + 1, m.group(0), line))
    return hits


def main() -> int:
    total_hits = 0
    for sub in SCAN_DIRS:
        base = ROOT / sub
        if not base.is_dir():
            continue
        for ext in SCAN_EXTS:
            for path in base.rglob(f"*{ext}"):
                if path.resolve() == SELF:
                    continue
                if is_test_file(path.relative_to(ROOT)):
                    continue
                hits = scan_file(path)
                if not hits:
                    continue
                rel = path.relative_to(ROOT)
                for line_no, col, ch, line in hits:
                    total_hits += 1
                    cp = ord(ch)
                    print(
                        f"{rel}:{line_no}:{col}: tofu-prone U+{cp:04X} ({ch}) - {line.strip()}",
                        file=sys.stderr,
                    )
    if total_hits:
        print(
            f"\ncheck-emojis: {total_hits} tofu-prone codepoint(s) found in tracked sources.",
            file=sys.stderr,
        )
        print(
            "These characters render as tofu squares in egui's default font. "
            "Replace with ASCII or remove.",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
