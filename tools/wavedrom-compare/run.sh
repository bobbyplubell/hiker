#!/usr/bin/env bash
# Drive the WaveDrom visual-parity check end to end:
#   1. build the oracle container (real wavedrom.js / wavedrom-cli) once,
#   2. build wavedrom-compare (release),
#   3. for each fixture, render the SAME raw WaveJSON with BOTH our pure-Rust
#      hiker-wavedrom and the oracle, rasterize both SVGs, and write a
#      side-by-side composite PNG + a palette histogram-intersection report.
#
# Usage:
#   tools/wavedrom-compare/run.sh                 # all fixtures
#   tools/wavedrom-compare/run.sh busses          # one fixture by name
#
# Requires docker (or set DOCKER=podman) and a cargo toolchain. Never runs the
# reference JS on the host — only inside the container. The fixture is piped to
# the container over stdin (NO bind mount) so it never touches the host FS,
# sidestepping SELinux mount-label denials.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
DOCKER="${DOCKER:-docker}"
IMAGE="wavedrom-compare-oracle"

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

echo "==> building wavedrom-compare (release)"
( cd "$REPO" && cargo build -q --release -p wavedrom-compare )
BIN="$REPO/target/release/wavedrom-compare"

for fx in "${FIXTURES[@]}"; do
  name="$(basename "$fx" .json)"
  echo
  echo "=== $name ==="

  # Ours: render the raw WaveJSON through hiker_wavedrom::render → SVG.
  "$BIN" emit-svg "$fx" >"$OUT/ours/$name.svg"

  # Theirs: render the SAME raw WaveJSON with real wavedrom.js in the container.
  # The fixture is piped in over stdin (NO bind mount) — the container never
  # touches the host filesystem.
  "$DOCKER" run --rm -i "$IMAGE" <"$fx" >"$OUT/theirs/$name.svg"

  # Rasterize both, write the side-by-side composite, print palette metrics.
  "$BIN" compose \
    --ours "$OUT/ours/$name.svg" \
    --ref "$OUT/theirs/$name.svg" \
    --out "$OUT/$name.png" \
    --name "$name"
done

echo
echo "composites + raw SVGs in $OUT/ (gitignored)"
