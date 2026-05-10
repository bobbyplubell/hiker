#!/usr/bin/env bash
# Verification entrypoint for hiker-dev. Run ONCE from anywhere in the repo.
# Empty/quiet output on success is normal; a non-zero exit is the only failure signal.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

fail() { echo "check.sh: $1 FAILED" >&2; exit 1; }

echo "==> cargo test -p hiker-core --lib"
cargo test -p hiker-core --lib || fail "cargo test -p hiker-core --lib"

echo "==> cargo check -p hiker-ui"
cargo check -p hiker-ui || fail "cargo check -p hiker-ui"

echo "==> tsc --noEmit (via docker compose, from ui/)"
( cd ui && docker compose -f compose.yaml run --rm ui ./node_modules/.bin/tsc --noEmit ) \
    || fail "tsc --noEmit"

echo "==> all checks passed"
