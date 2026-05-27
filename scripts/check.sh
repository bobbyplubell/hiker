#!/usr/bin/env bash
# Verification entrypoint. Run ONCE from anywhere in the repo.
# Empty/quiet output on success is normal; a non-zero exit is the only failure signal.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

fail() { echo "check.sh: $1 FAILED" >&2; exit 1; }

# Run EVERY test in the workspace, in one pass: lib unit tests for every
# crate, the per-crate integration tests under `tests/` (editor-core / view /
# egui / diff, mcp-server smoke, app smoke, …), the doc tests, AND the
# heap-ceiling regression binaries.
#
# Heap ceilings: each `tests/heap_ceiling.rs` installs a counting global
# allocator in its own test binary (cargo runs each as a separate process) and
# asserts peak heap stays under a fixed ceiling. Bumping a ceiling is
# intentional friction — fix the regression or justify the headroom in the
# test file. They run as part of the workspace pass below.
echo "==> cargo test --workspace (every unit, integration, doc, and heap-ceiling test)"
cargo test --workspace || fail "cargo test --workspace"

echo "==> cargo clippy (length budget + anti-arbitrary-split lints)"
# Two clusters of lints below:
#   (A) the original length-budget lints, and
#   (B) lints that target arbitrary splits made to dodge those budgets.
# Group (B) is the clippy counterpart to scripts/check-splits.py — the
# two reinforce each other. Tighten or loosen here with intent;
# `#[allow(...)]` at the call site is not a sanctioned escape.
cargo clippy --workspace --all-targets -- \
    -D clippy::too_many_lines \
    -D clippy::derivable_impls \
    -D clippy::collapsible_if \
    -D clippy::field_reassign_with_default \
    -D clippy::wildcard_imports \
    -D clippy::module_inception \
    -D clippy::module_name_repetitions \
    -D clippy::pub_use \
    -D clippy::cognitive_complexity \
    -D clippy::unnecessary_wraps \
    -D clippy::needless_pass_by_value \
    -D clippy::too_many_arguments \
    -D clippy::trivially_copy_pass_by_ref \
    -D clippy::needless_late_init \
    -D clippy::redundant_closure_for_method_calls \
    -D clippy::missing_const_for_fn \
    -D clippy::large_stack_arrays \
    || fail "cargo clippy"

echo "==> file-length budget (see scripts/check-lengths.py)"
python3 scripts/check-lengths.py || fail "file-length budget"

echo "==> split-pattern detector (see scripts/check-splits.py)"
python3 scripts/check-splits.py || fail "split-pattern detector"

echo "==> emoji ban (see scripts/check-emojis.py)"
python3 scripts/check-emojis.py || fail "emoji ban"

# --- Opt-in memory steps ---------------------------------------------------
#
# These cost extra time / require a non-stable toolchain, so the default
# `check.sh` invocation skips them. Toggle them on with the env var named
# in the heading.

if [[ "${HIKER_LSAN:-0}" == "1" ]]; then
    # LeakSanitizer requires nightly + an instrumented build. The
    # `LSAN_OPTIONS=exitcode=23` is the default-suppression-free signal:
    # any leak from a test run flips the process exit. We rebuild std so
    # standard-library allocations don't drown the user's signal.
    echo "==> LSan: cargo +nightly test -p hiker-core --lib"
    if ! command -v cargo +nightly &> /dev/null; then
        fail "HIKER_LSAN=1 but nightly toolchain not installed (rustup toolchain install nightly)"
    fi
    RUSTFLAGS="-Z sanitizer=leak" \
        cargo +nightly test -p hiker-core --lib \
        -Zbuild-std --target "$(rustc -vV | sed -n 's|host: ||p')" \
        || fail "LSan: hiker-core leaks detected"
fi

if [[ "${HIKER_DHAT:-0}" == "1" ]]; then
    # dhat-heap snapshot of the indexer's full-scan path against a
    # caller-supplied vault (defaults to repo root). Writes
    # `dhat-heap.json` to the cwd; open in
    # https://nnethercote.github.io/dh_view/dh_view.html.
    vault="${HIKER_DHAT_VAULT:-$repo_root}"
    echo "==> dhat: profile-indexer against $vault"
    cargo run --release -p profile-indexer -- "$vault" \
        || fail "dhat heap profile"
    echo "    dhat-heap.json written to $(pwd)"
fi

echo "==> all checks passed"
