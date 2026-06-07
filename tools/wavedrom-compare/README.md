# wavedrom-compare

Visual-parity harness for the pure-Rust WaveDrom renderer
(`hiker_wavedrom::render`, behind the app's `wavedrom` / `wavedrom-bitfield`
code-fences) against the real **wavedrom.js** (via `wavedrom-cli`, run in a
Docker oracle).

This is the WaveDrom analog of `tools/dagre-compare/`, but the comparison is
**visual + palette-based**, not a coordinate diff: WaveDrom emits SVG, not
numeric layout coords, so we render the *same* WaveJSON with both engines,
rasterize both SVGs identically, compose them side by side, and quantify how
close the **palettes** match (the main thing we're driving — our bus/`type`
fill colors and skin vs wavedrom's).

## What it does

For each fixture (raw WaveJSON):

1. **OURS** — `wavedrom-compare emit-svg <fixture>` → our SVG.
2. **REFERENCE** — `docker run --rm -i <oracle> < fixture` → wavedrom.js SVG.
3. **Rasterize both** with resvg 0.47.0 (the version `hiker-wavedrom` pins),
   using the same font setup as the crate's own example: system fonts +
   bundled Liberation Sans, `set_sans_serif_family("Liberation Sans")`. Both
   sides rasterize under identical font resolution so text metrics don't bias
   the comparison.
4. **Compose** a side-by-side PNG (`ours | wavedrom.js`) with a divider and
   labels → `out/<name>.png`.
5. **Color histogram** per side + a histogram-intersection score.

## Why Docker

The reference is wavedrom.js (npm). Per the project's rules we **never run the
reference JS on the host** — it runs only inside the oracle container
(`oracle/`, pinned to `wavedrom-cli` 3.2.0). The fixture is piped to the
container over **stdin** (no bind mount), so the container never touches the
host filesystem — this also sidesteps SELinux mount-label denials.

`wavedrom-cli` flags used (v3.2.0): `wavedrom-cli -i <in> -s <out.svg>`. The
oracle entrypoint reads WaveJSON from stdin → `/tmp/in.json5` → runs the CLI →
`cat`s the SVG to stdout (CLI progress noise is routed to stderr).

Requires `docker` (or `DOCKER=podman`) and a cargo toolchain. No host Node.

## Usage

```sh
tools/wavedrom-compare/run.sh             # all fixtures
tools/wavedrom-compare/run.sh busses      # one fixture by name
DOCKER=podman tools/wavedrom-compare/run.sh
```

Rust side alone:

```sh
cargo run -p wavedrom-compare -- emit-svg fixtures/busses.json   # our SVG
cargo run -p wavedrom-compare -- compose \
  --ours out/ours/busses.svg --ref out/theirs/busses.svg \
  --out out/busses.png --name busses
```

Composites + raw SVGs land under `out/` (gitignored).

## Color-histogram metric

Both rasterized images are reduced to a coarse RGB histogram over their **ink**
(non-transparent pixels, excluding near-white canvas and near-black
lines/text — those structural pixels dominate every diagram and would drown
out the palette signal). Each surviving pixel's RGB is quantized into 32-step
bins (`QUANT = 32`), so the histogram is robust to antialiasing fringes while
still separating WaveDrom's distinct bus/`type` fills.

- **Top colors per side** — the 6 most-common buckets (bin-center hex + share
  of ink), so you can read off our palette vs wavedrom's directly.
- **Histogram-intersection score (0..1)** — sum of `min(shareOurs, shareRef)`
  over shared buckets, normalized by share so the two sides' differing ink
  areas don't bias it. `1.0` = identical palette distribution, `0.0` = no
  shared colors. This is the headline parity number.

## Fixtures

Raw WaveJSON, passed verbatim to both sides:

- `clock_data.json` — clock + data lane (`x.34.5x` w/ labels) + control.
- `busses.json` — data codes `2`..`5` with labels (palette parity).
- `xz.json` — don't-care `x` + hi-Z `z`.
- `period_phase.json` — `period` and `phase` modifiers.
- `edges.json` — `node` + `edge` arrows.
- `group.json` — nested-array signal groups.
- `reg.json` — bitfield with a typed (`type:2`) field.

## Note

Do not edit `hiker-render/wavedrom/` from here — this is a harness only. Findings
live alongside the slugs in `docs/` / `status.md`.
