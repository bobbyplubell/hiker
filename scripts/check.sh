#!/usr/bin/env bash
# Verification entrypoint for hiker-dev. Run ONCE from anywhere in the repo.
# Empty/quiet output on success is normal; a non-zero exit is the only failure signal.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

fail() { echo "check.sh: $1 FAILED" >&2; exit 1; }

echo "==> cargo test -p hiker-core --lib"
cargo test -p hiker-core --lib || fail "cargo test -p hiker-core --lib"

# `ui/src/generatedTypes/` is the TS mirror of `core::config::*`,
# emitted by the `gen_ts_types` test under the `ts-export` feature.
# Re-run the codegen and refuse to pass if the working tree drifts —
# that's the signal that a Rust field changed without a regen.
echo "==> generated TS types in sync with core::config"
cargo test -p hiker-core --lib --features ts-export gen_ts_types \
    || fail "ts-export codegen"
if ! git diff --quiet -- ui/src/generatedTypes/; then
    echo "check.sh: ui/src/generatedTypes/ is out of date" >&2
    echo "  Run: cargo test -p hiker-core --lib --features ts-export gen_ts_types" >&2
    echo "  Then commit the regenerated files." >&2
    fail "generated TS types out of sync"
fi

echo "==> cargo check -p hiker-ui"
cargo check -p hiker-ui || fail "cargo check -p hiker-ui"

echo "==> cargo clippy (function-length budget, see clippy.toml)"
cargo clippy --workspace --all-targets -- \
    -D clippy::too_many_lines \
    -D clippy::cognitive_complexity \
    -D clippy::derivable_impls \
    -D clippy::collapsible_if \
    -D clippy::field_reassign_with_default \
    || fail "cargo clippy"

echo "==> file-length budget (see scripts/check-lengths.py)"
python3 scripts/check-lengths.py || fail "file-length budget"

echo "==> tsc --noEmit (via ui/compose.yaml)"
( cd ui && docker compose run --rm --no-deps -T ui ./node_modules/.bin/tsc --noEmit ) \
    || fail "tsc --noEmit"

echo "==> all checks passed"
