#!/usr/bin/env bash
# Drive the dagre-port conformance check end to end:
#   1. build the oracle container (real @dagrejs/dagre) once,
#   2. for each fixture, lay it out with BOTH our Rust engine and the oracle,
#   3. diff the two layouts and print a per-fixture verdict.
#
# Usage:
#   tools/dagre-compare/run.sh                 # all fixtures, 1px tolerance
#   tools/dagre-compare/run.sh er-orders       # one fixture by name
#   TOL=0.5 tools/dagre-compare/run.sh         # custom tolerance
#
# Requires docker (or set DOCKER=podman) and a cargo toolchain. Never runs the
# reference JS on the host — only inside the container.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
DOCKER="${DOCKER:-docker}"
TOL="${TOL:-1.0}"
IMAGE="dagre-compare-oracle"

OUT="$HERE/out"
mkdir -p "$OUT/ours" "$OUT/theirs"

# Pick fixtures: an explicit name, or every *.json in fixtures/.
if [[ $# -ge 1 ]]; then
  FIXTURES=("$HERE/fixtures/$1.json")
else
  FIXTURES=("$HERE"/fixtures/*.json)
fi

echo "==> building oracle image ($IMAGE)"
"$DOCKER" build -q -t "$IMAGE" "$HERE/oracle" >/dev/null

echo "==> building dagre-compare (release)"
( cd "$REPO" && cargo build -q --release -p dagre-compare )
BIN="$REPO/target/release/dagre-compare"

fail=0
for fx in "${FIXTURES[@]}"; do
  name="$(basename "$fx" .json)"
  echo
  echo "### $name"

  # Ours: run the fixture through hiker_graph::LayeredEngine.
  "$BIN" emit "$fx" >"$OUT/ours/$name.json"

  # Theirs: run the SAME fixture through real dagre.js in the container. The
  # fixture is piped in over stdin (NO bind mount) so the container never
  # touches the host filesystem — avoids SELinux mount-label denials entirely.
  "$DOCKER" run --rm -i "$IMAGE" <"$fx" >"$OUT/theirs/$name.json"

  # Diff (non-zero exit if beyond tolerance).
  if ! "$BIN" diff "$OUT/ours/$name.json" "$OUT/theirs/$name.json" --tol "$TOL"; then
    fail=1
  fi
done

echo
if [[ $fail -eq 0 ]]; then
  echo "ALL FIXTURES WITHIN ${TOL}px"
else
  echo "DIVERGENCE DETECTED (see per-fixture reports above; raw JSON in $OUT/)"
fi
exit $fail
