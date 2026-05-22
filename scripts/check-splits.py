#!/usr/bin/env python3
"""Detect lazy file/function splitting used to dodge length budgets.

The file-length cap (`check-lengths.py`) and clippy's `too_many_lines`
cap push back on big tangled files and functions. They are easy to
game: rename `do_thing` to `do_thing` + `do_thing_part_2`, or shard
`foo.rs` into `foo_a.rs` / `foo_b.rs` with private helpers reaching
across siblings. This script detects the *symptoms* of that game.

It is paired with the clippy invocation in `scripts/check.sh`, which
enforces lints that target the same antipattern from a different angle
(`wildcard_imports`, `module_inception`, `module_name_repetitions`,
`pub_use`, `cognitive_complexity`, `unnecessary_wraps`,
`needless_pass_by_value`).

What this Python pass checks (all hard caps, no allowlist):

NAME-SHAPED CHECKS — easy to circumvent on their own, paired with
the structural checks below to close that gap.

1. **Function-name suffix smell.** `fn foo_part_2`, `fn foo_helper`,
   `fn foo_inner`, `fn foo_impl`, `fn foo_extra`, `fn foo_a`.
2. **File-name suffix smell.** Same idea: `foo_part2.rs`,
   `foo_helper.rs`, `foo_extra.rs`, `foo_misc.rs`, `foo_util2.rs`,
   `foo_new.rs`, `foo_tmp.rs`.

STRUCTURAL CHECKS — survive renames.

3. **Module-root doc presence + content.** Every `mod.rs` / `lib.rs`
   must start with `//!` doc in the first 5 lines, AND that doc must
   total >= MOD_DOC_MIN_WORDS words. A one-word doc passes #3's
   presence test but fails the content test. If you cannot write a
   real sentence about what the module is for, the split is arbitrary.
4. **Public-surface density.** A non-facade file packing 10+ `pub`
   items at > 0.15 density per non-comment line has the wrong
   boundary — most should be private helpers or move to siblings.
5. **Minimum file size.** Files in the Rust roots must have at least
   MIN_NONCOMMENT_LINES non-comment, non-blank lines. Exempt:
   `lib.rs`, `main.rs`, `mod.rs`, `build.rs`, `error.rs`.
6. **Sibling-only files.** A file in a multi-file module whose stem
   never appears as a path component outside its own module directory
   is a shard: it exists only to serve its siblings. Renaming the
   file does not change its call graph.
7. **Heavy `use super::` reach.** A file pulling SUPER_REACH_MAX or
   more names from its parent is announcing "I am a slice of my
   parent, not a self-contained module." `use super::*` is denied by
   clippy; this check catches the explicit-list workaround.
8. **Cross-sibling module coupling.** If two sibling module
   directories under the same parent both reference each other's
   names heavily, they are one module wearing two hats. Reported as
   coupled pairs.

Tightening or loosening these thresholds is a deliberate posture
change, not an agent's escape hatch — edit this file with intent.
"""

from __future__ import annotations

import os
import re
import sys
from collections import defaultdict
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent

RUST_ROOTS = [
    "core/src",
    "mcp-server/src",
    "cli/src",
    "app/src",
    "editor/editor-core/src",
    "editor/editor-view/src",
    "editor/editor-egui/src",
    "editor/editor-md/src",
    "editor/editor-diff/src",
    "editor/editor-ts/src",
    "egui-workbench/src",
]
SKIP_DIRS = ("node_modules", "dist", "target", "tests")

# (1) Function-name suffix smell.
FN_SUFFIX_RE = re.compile(
    r"^\s*(?:pub(?:\([^)]+\))?\s+)?(?:async\s+)?(?:unsafe\s+)?(?:const\s+)?"
    r"fn\s+(\w+_(?:part_?\d+|helper_?\d*|impl|inner|extra|[abc])\b)"
)

# (2) File-name suffix smell.
FILE_SUFFIX_RE = re.compile(
    r"_(?:part[_-]?\d+|helper\d*|extra|misc|util2?|new|tmp|temp)\.rs$"
)

# (3) Module-root doc requirement.
MOD_ROOTS = {"mod.rs", "lib.rs"}
MOD_DOC_WINDOW = 5
MOD_DOC_MIN_WORDS = 15

# (4) Public-surface density.
PUB_ITEM_RE = re.compile(
    r"^\s*pub(?:\([^)]+\))?\s+(?:async\s+|unsafe\s+|const\s+)*"
    r"(fn|struct|enum|trait|type|const|static|use|mod)\b"
)
PUB_DENSITY_MIN_ITEMS = 10
PUB_DENSITY_MAX_PER_NONCOMMENT_LINE = 0.15

# (5) Minimum file size.
MIN_NONCOMMENT_LINES = 20
MIN_SIZE_EXEMPT = {"lib.rs", "main.rs", "mod.rs", "build.rs", "error.rs"}

# (6) Sibling-only file detection.
# A file's stem must appear as a path component (`::stem` / `mod stem` /
# `use ...stem...`) somewhere outside its own module directory.
SIBLING_ONLY_EXEMPT_STEMS = {"mod", "lib", "main", "build", "tests"}

# (7) Heavy `use super::` reach.
USE_SUPER_RE = re.compile(r"^\s*use\s+super::")
USE_SUPER_GROUP_RE = re.compile(r"^\s*use\s+super::\{([^}]+)\}")
SUPER_REACH_MAX = 5

# (8) Cross-sibling module coupling.
# When two sibling module directories reference each other heavily,
# flag the pair. References counted via stem occurrences in source.
COUPLING_MIN_REFS_EACH_WAY = 4


# ----- file iteration & basic helpers ---------------------------------


def iter_rust_files() -> list[Path]:
    out: list[Path] = []
    for root in RUST_ROOTS:
        root_path = REPO_ROOT / root
        if not root_path.exists():
            continue
        for dirpath, dirnames, filenames in os.walk(root_path):
            dirnames[:] = [d for d in dirnames if d not in SKIP_DIRS]
            for fname in filenames:
                if fname.endswith(".rs"):
                    out.append(Path(dirpath) / fname)
    return sorted(out)


def count_noncomment_lines(text: str) -> int:
    n = 0
    in_block = False
    for raw in text.splitlines():
        line = raw.strip()
        if not line:
            continue
        if in_block:
            if "*/" in line:
                in_block = False
            continue
        if line.startswith("/*"):
            if "*/" not in line[2:]:
                in_block = True
            continue
        if line.startswith("//"):
            continue
        n += 1
    return n


def strip_strings_and_comments(text: str) -> str:
    """Coarse stripper: remove // line comments, /* */ block comments,
    "..." and r#"..."# string literals so identifier searches don't
    match inside them. Good enough for grep-shaped checks; not a real
    parser."""
    out: list[str] = []
    i = 0
    n = len(text)
    while i < n:
        c = text[i]
        nxt = text[i + 1] if i + 1 < n else ""
        if c == "/" and nxt == "/":
            j = text.find("\n", i)
            if j == -1:
                break
            i = j
            continue
        if c == "/" and nxt == "*":
            j = text.find("*/", i + 2)
            if j == -1:
                break
            i = j + 2
            continue
        if c == '"':
            j = i + 1
            while j < n:
                if text[j] == "\\":
                    j += 2
                    continue
                if text[j] == '"':
                    break
                j += 1
            i = j + 1
            continue
        out.append(c)
        i += 1
    return "".join(out)


# ----- individual checks ----------------------------------------------


def check_fn_suffix(rel: str, text: str, failures: list[str]) -> None:
    for i, line in enumerate(text.splitlines(), start=1):
        m = FN_SUFFIX_RE.match(line)
        if m:
            failures.append(
                f"  {rel}:{i}: suspicious split-pattern function name `{m.group(1)}`"
            )


def check_file_suffix(rel: str, failures: list[str]) -> None:
    if FILE_SUFFIX_RE.search(rel):
        failures.append(f"  {rel}: suspicious split-pattern file name")


def check_mod_doc(path: Path, rel: str, text: str, failures: list[str]) -> None:
    if path.name not in MOD_ROOTS:
        return
    lines = text.splitlines()
    head = lines[:MOD_DOC_WINDOW]
    if not any(line.lstrip().startswith("//!") for line in head):
        failures.append(
            f"  {rel}: module root missing `//!` doc comment in first "
            f"{MOD_DOC_WINDOW} lines"
        )
        return
    doc_words: list[str] = []
    for line in lines:
        stripped = line.lstrip()
        if stripped.startswith("//!"):
            doc_words.extend(stripped[3:].split())
        elif doc_words:
            break
    if len(doc_words) < MOD_DOC_MIN_WORDS:
        failures.append(
            f"  {rel}: module root `//!` doc is {len(doc_words)} words "
            f"(min {MOD_DOC_MIN_WORDS}); say what this module is for"
        )


def check_pub_density(path: Path, rel: str, text: str, failures: list[str]) -> None:
    if path.name in MOD_ROOTS:
        return
    pub_count = 0
    for line in text.splitlines():
        if PUB_ITEM_RE.match(line):
            pub_count += 1
    if pub_count < PUB_DENSITY_MIN_ITEMS:
        return
    nc = count_noncomment_lines(text)
    if nc == 0:
        return
    density = pub_count / nc
    if density > PUB_DENSITY_MAX_PER_NONCOMMENT_LINE:
        failures.append(
            f"  {rel}: {pub_count} pub items in {nc} non-comment lines "
            f"(density {density:.2f} > {PUB_DENSITY_MAX_PER_NONCOMMENT_LINE}); "
            f"either narrow the public surface or split into sibling modules"
        )


def check_min_size(path: Path, rel: str, text: str, failures: list[str]) -> None:
    if path.name in MIN_SIZE_EXEMPT:
        return
    nc = count_noncomment_lines(text)
    if nc < MIN_NONCOMMENT_LINES:
        failures.append(
            f"  {rel}: {nc} non-comment line(s) (min {MIN_NONCOMMENT_LINES}); "
            f"inline into the parent module or merge with a sibling"
        )


def check_super_reach(path: Path, rel: str, text: str, failures: list[str]) -> None:
    if path.name in MOD_ROOTS:
        return
    count = 0
    for line in text.splitlines():
        m = USE_SUPER_GROUP_RE.match(line)
        if m:
            count += sum(1 for tok in m.group(1).split(",") if tok.strip())
            continue
        if USE_SUPER_RE.match(line):
            count += 1
    if count >= SUPER_REACH_MAX:
        failures.append(
            f"  {rel}: pulls {count} names from `super` (max "
            f"{SUPER_REACH_MAX - 1}); file is leaning on its parent's "
            f"namespace, not standing on its own"
        )


def _externally_exposed(parent_root_src: str, stem: str) -> bool:
    """Does the parent `mod.rs` / `lib.rs` expose this file's
    contents past the module boundary? True if any of:
      - `pub mod <stem>`
      - `pub use <stem>::...`
      - `pub use <stem>;`
      - `pub(crate) use <stem>::...` (crate-wide visibility is still external to the module)
    """
    patterns = [
        rf"\bpub\s+mod\s+{re.escape(stem)}\b",
        rf"\bpub(?:\([^)]+\))?\s+use\s+{re.escape(stem)}\b",
    ]
    for pat in patterns:
        if re.search(pat, parent_root_src):
            return True
    return False


def _items_referenced_externally(
    f: Path, stripped: dict[Path, str], stripped_text: str
) -> bool:
    """Check whether ANY pub item defined in `f` is referenced from
    any file outside `f.parent`. Skips facade roots and tests."""
    item_names: set[str] = set()
    item_re = re.compile(
        r"^\s*pub(?:\([^)]+\))?\s+(?:async\s+|unsafe\s+|const\s+|extern\s+\"[^\"]+\"\s+)*"
        r"(?:fn|struct|enum|trait|type|const|static)\s+([A-Za-z_]\w*)",
        re.MULTILINE,
    )
    for m in item_re.finditer(stripped_text):
        item_names.add(m.group(1))
    if not item_names:
        return False
    own_dir = f.parent.resolve()
    name_pats = {
        n: re.compile(rf"(?<![A-Za-z0-9_]){re.escape(n)}(?![A-Za-z0-9_])")
        for n in item_names
    }
    for other, src in stripped.items():
        if other == f:
            continue
        try:
            other.resolve().relative_to(own_dir)
            continue  # inside own dir — doesn't count
        except ValueError:
            pass
        for pat in name_pats.values():
            if pat.search(src):
                return True
    return False


def check_sibling_only_files(
    all_files: list[Path],
    stripped: dict[Path, str],
    failures: list[str],
) -> None:
    """A file `foo.rs` in a multi-file module is "sibling-only" if:
      (a) the parent `mod.rs` / `lib.rs` does NOT expose `foo` via
          `pub mod foo` or `pub use foo`, AND
      (b) none of `foo.rs`'s `pub` items are referenced from any file
          outside `foo.rs`'s own directory.
    Such a file exists only to serve its siblings — renaming it does
    not change its call graph. Inline it or merge with a sibling.
    """
    by_dir: dict[Path, list[Path]] = defaultdict(list)
    for f in all_files:
        by_dir[f.parent].append(f)

    for f in all_files:
        if f.name in MOD_ROOTS:
            continue
        stem = f.stem
        if stem in SIBLING_ONLY_EXEMPT_STEMS:
            continue
        siblings = by_dir[f.parent]
        if len(siblings) < 3:
            continue
        # Skip if any potential parent root exposes the file.
        parent_root: Path | None = None
        for candidate in ("mod.rs", "lib.rs"):
            cand = f.parent / candidate
            if cand in stripped:
                parent_root = cand
                break
        if parent_root is None:
            continue
        if _externally_exposed(stripped[parent_root], stem):
            continue
        if _items_referenced_externally(f, stripped, stripped[f]):
            continue
        rel = str(f.relative_to(REPO_ROOT))
        failures.append(
            f"  {rel}: not exposed via `pub mod` / `pub use` and no pub "
            f"items used outside its dir; sibling-only shard — inline "
            f"into a sibling or expose what's actually used"
        )


def check_cross_sibling_coupling(
    all_files: list[Path],
    stripped: dict[Path, str],
    failures: list[str],
) -> None:
    """Find sibling module directories under a common parent that
    reference each other heavily. Two-way >= COUPLING_MIN_REFS_EACH_WAY
    is the threshold."""
    # Group: parent dir -> list of child *module dirs* (dirs with mod.rs)
    parent_to_modules: dict[Path, list[Path]] = defaultdict(list)
    for f in all_files:
        if f.name == "mod.rs":
            mod_dir = f.parent.resolve()
            parent_to_modules[mod_dir.parent].append(mod_dir)

    # Concatenate each module's stripped source for fast scanning.
    mod_src: dict[Path, str] = {}
    for f, src in stripped.items():
        for mod_dir in [p for ps in parent_to_modules.values() for p in ps]:
            try:
                f.resolve().relative_to(mod_dir)
            except ValueError:
                continue
            mod_src.setdefault(mod_dir, "")
            mod_src[mod_dir] += "\n" + src
            break

    reported: set[tuple[str, str]] = set()
    for parent, mods in parent_to_modules.items():
        for i, a in enumerate(mods):
            for b in mods[i + 1 :]:
                a_name, b_name = a.name, b.name
                a_pat = re.compile(
                    rf"(?<![A-Za-z0-9_]){re.escape(a_name)}(?![A-Za-z0-9_])"
                )
                b_pat = re.compile(
                    rf"(?<![A-Za-z0-9_]){re.escape(b_name)}(?![A-Za-z0-9_])"
                )
                a_to_b = len(b_pat.findall(mod_src.get(a, "")))
                b_to_a = len(a_pat.findall(mod_src.get(b, "")))
                if (
                    a_to_b >= COUPLING_MIN_REFS_EACH_WAY
                    and b_to_a >= COUPLING_MIN_REFS_EACH_WAY
                ):
                    rel_a = str(a.relative_to(REPO_ROOT))
                    rel_b = str(b.relative_to(REPO_ROOT))
                    key = tuple(sorted([rel_a, rel_b]))
                    if key in reported:
                        continue
                    reported.add(key)
                    failures.append(
                        f"  {rel_a} <-> {rel_b}: bidirectional sibling "
                        f"coupling ({a_to_b} refs one way, {b_to_a} the "
                        f"other; threshold {COUPLING_MIN_REFS_EACH_WAY}); "
                        f"sibling modules that reach across this much are "
                        f"one module wearing two hats"
                    )


# ----- driver ---------------------------------------------------------


def main() -> int:
    failures: list[str] = []
    all_files = iter_rust_files()

    raw: dict[Path, str] = {}
    stripped: dict[Path, str] = {}
    for f in all_files:
        try:
            text = f.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        raw[f] = text
        stripped[f] = strip_strings_and_comments(text)

    for f, text in raw.items():
        rel = str(f.relative_to(REPO_ROOT))
        check_file_suffix(rel, failures)
        check_fn_suffix(rel, text, failures)
        check_mod_doc(f, rel, text, failures)
        check_pub_density(f, rel, text, failures)
        check_min_size(f, rel, text, failures)
        check_super_reach(f, rel, text, failures)

    check_sibling_only_files(all_files, stripped, failures)
    check_cross_sibling_coupling(all_files, stripped, failures)

    if failures:
        print("split-pattern violations:", file=sys.stderr)
        for v in sorted(set(failures)):
            print(v, file=sys.stderr)
        print(
            f"\ncheck-splits: {len(set(failures))} violation(s). See "
            f"scripts/check-splits.py for the rules and why they exist.",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
