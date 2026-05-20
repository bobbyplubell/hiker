#!/usr/bin/env bash
# Verification entrypoint. Run ONCE from anywhere in the repo.
# Empty/quiet output on success is normal; a non-zero exit is the only failure signal.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

fail() { echo "check.sh: $1 FAILED" >&2; exit 1; }

echo "==> cargo test -p hiker-core --lib"
cargo test -p hiker-core --lib || fail "cargo test -p hiker-core --lib"

echo "==> cargo test -p hiker-core --test heap_ceiling (counting-allocator regression)"
cargo test -p hiker-core --test heap_ceiling -- --nocapture \
    || fail "heap-ceiling regression"

echo "==> cargo check -p hiker-app"
cargo check -p hiker-app || fail "cargo check -p hiker-app"

echo "==> cargo test -p hiker-app (smoke + unit)"
cargo test -p hiker-app || fail "cargo test -p hiker-app"

echo "==> cargo clippy (function-length budget + a few line-count-shaped lints)"
cargo clippy --workspace --all-targets -- \
    -D clippy::too_many_lines \
    -D clippy::derivable_impls \
    -D clippy::collapsible_if \
    -D clippy::field_reassign_with_default \
    || fail "cargo clippy"

echo "==> file-length budget (see scripts/check-lengths.py)"
python3 scripts/check-lengths.py || fail "file-length budget"

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
