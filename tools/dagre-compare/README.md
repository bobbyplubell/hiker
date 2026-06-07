# dagre-compare

Conformance harness for the pure-Rust dagre port (`hiker_graph::LayeredEngine`,
the layered/Sugiyama layout behind mermaid flowchart/ER/class/state diagrams).

The layered layout is deterministic and was ported from dagre's own test suite,
so **given identical input** (same node sizes, edges, options) it should produce
the **same coordinates** as the reference `@dagrejs/dagre`. This tool feeds the
same graph fixtures to both engines and diffs the resulting layouts, so any
divergence in the port surfaces as a numeric pixel delta — *before* it shows up
as a visibly-wrong diagram.

It deliberately compares at the **layout** layer, not rendered pixels: rendered
diagrams differ in font metrics and our SVG paint (noise that drowns out real
bugs), whereas node/edge coordinates should match dagre closely. Comparing the
layout isolates exactly the part that *can* be identical.

## Why Docker

The reference is dagre.js (npm). Per the port's rules we **never run the
reference JS on the host** — it runs only inside the oracle container
(`oracle/`, pinned to `@dagrejs/dagre` 1.1.4). The fixture is piped to the
container over **stdin** (no bind mount), so the container never touches the
host filesystem — this also sidesteps SELinux mount-label denials.

Requires `docker` (or `DOCKER=podman`) and a cargo toolchain. No host Node.

## Usage

```sh
tools/dagre-compare/run.sh                # all fixtures, 1px tolerance
tools/dagre-compare/run.sh er-orders      # one fixture by name
TOL=0.5 tools/dagre-compare/run.sh        # custom tolerance (px)
```

`run.sh`:
1. builds the oracle image once,
2. for each `fixtures/*.json`: lays it out with our `LayeredEngine`
   (`dagre-compare emit`) and with real dagre.js (the container),
3. diffs the two (`dagre-compare diff`) and prints a per-fixture verdict.

Raw layout JSON for both sides is written under `out/{ours,theirs}/` (gitignored)
for manual inspection.

You can also drive the Rust side alone:

```sh
cargo run -p dagre-compare -- emit fixtures/diamond.json   # our layout as JSON
cargo run -p dagre-compare -- diff a.json b.json --tol 0.5 # compare two layouts
```

## Fixture schema

A fixture is a pure dagre input — explicit sizes so font metrics don't enter:

```jsonc
{
  "name": "diamond",
  "rankdir": "TB",            // TB | BT | LR | RL
  "ranksep": 50, "nodesep": 50, "edgesep": 20,
  "nodes": [ { "w": 100, "h": 40 }, ... ],
  "edges": [ { "v": 0, "w": 1, "label": { "w": 30, "h": 20 } }, ... ],
  "parents": [ 4, 4, 5, 5, null, null ]   // optional: subgraph membership
}
```

Node ids are array indices on both sides; edges are keyed by index so parallel
and self edges stay distinct and read back positionally. Output schema (emitted
by both sides):

```jsonc
{
  "nodes": [ { "x": .., "y": .., "w": .., "h": .. }, ... ],
  "edges": [ { "points": [ {"x":..,"y":..}, ... ], "label": {"x":..,"y":..} | null }, ... ],
  "size":  { "w": .., "h": .. }
}
```

The diff reports node-center delta (max/mean), node-size delta, graph-size delta,
edge-label-center delta, and edge-endpoint delta. Node centers + graph size are
the load-bearing verdict; edge-routing vertex counts are allowed to differ
(intermediate dummy routing is an implementation detail).

## Findings

- **`order` crossing-minimization mirror (FIXED).** The port overwrote its best
  ordering on *ties*, but dagre keeps the *earlier* ordering on a tie. The
  alternating down/up + left/right-bias sweeps produce equal-crossing
  mirror-image layouts, so this flipped diagrams left↔right vs dagre (e.g. the
  CUSTOMER/ORDER ER diagram). Fixed in `graph/src/layered/order/mod.rs`; the
  `chain`/`diamond`/`crossing`/`lr-flow`/`er-orders` fixtures now match dagre.js
  to 0.00px.
- **Cluster/subgraph vertical spacing (OPEN).** The `clusters` fixture matches
  dagre on x-position, ordering, and cluster width, but dagre spaces ranks
  inside/around clusters ~2× further apart (taller graph). Localized to the
  cluster border-segment / nesting path (intermediate border ranks appear to
  collapse). Does not affect non-subgraph diagrams. Run
  `tools/dagre-compare/run.sh clusters` to reproduce.
