// Oracle: lay out a shared fixture with the REAL @dagrejs/dagre and print the
// result as JSON on stdout, in the exact schema `dagre-compare diff` expects.
//
// This is the ground truth the pure-Rust port (`hiker_graph::LayeredEngine`) is
// measured against. It is run ONLY inside the container (never on the host) —
// see this tool's README.
//
//   node run.js <fixture.json>     # read a file
//   node run.js                    # read the fixture from stdin
//
// Fixture schema (shared with the Rust side):
//   { rankdir, ranksep, nodesep, edgesep,
//     nodes: [{w,h}], edges: [{v,w,label?:{w,h}}], parents?: [int|null] }
// Output schema:
//   { nodes: [{x,y,w,h}], edges: [{points:[{x,y}], label:{x,y}|null}], size:{w,h} }

const fs = require("fs");
const dagre = require("@dagrejs/dagre");

// Read the fixture from a file argument, or stdin when none is given (stdin
// avoids bind-mount permission/SELinux headaches — see run.sh).
const path = process.argv[2];
const raw =
  path && path !== "-"
    ? fs.readFileSync(path, "utf8")
    : fs.readFileSync(0, "utf8");
const fx = JSON.parse(raw);

// Match LayeredEngine: a directed compound multigraph. Edge `name` = index so
// parallel/self edges stay distinct and read back positionally, exactly like
// the Rust adapter keys them.
const g = new dagre.graphlib.Graph({
  directed: true,
  multigraph: true,
  compound: true,
});

g.setGraph({
  rankdir: fx.rankdir || "TB",
  ranksep: fx.ranksep ?? 50,
  nodesep: fx.nodesep ?? 50,
  edgesep: fx.edgesep ?? 20,
  // dagre defaults marginx/marginy to 0; the Rust engine sets no margins
  // either. Pin them so neither side drifts on a default change.
  marginx: 0,
  marginy: 0,
});
g.setDefaultEdgeLabel(() => ({}));

fx.nodes.forEach((n, i) => {
  g.setNode(String(i), { width: n.w, height: n.h });
});

if (Array.isArray(fx.parents)) {
  fx.parents.forEach((p, i) => {
    if (p !== null && p !== undefined && p !== i) {
      g.setParent(String(i), String(p));
    }
  });
}

(fx.edges || []).forEach((e, idx) => {
  const lbl = {};
  if (e.label && e.label.w > 0 && e.label.h > 0) {
    lbl.width = e.label.w;
    lbl.height = e.label.h;
    lbl.labelpos = "c";
  }
  g.setEdge(String(e.v), String(e.w), lbl, String(idx));
});

dagre.layout(g);

const nodes = fx.nodes.map((_, i) => {
  const nd = g.node(String(i));
  return { x: nd.x, y: nd.y, w: nd.width, h: nd.height };
});

const edges = (fx.edges || []).map((e, idx) => {
  const ed = g.edge({ v: String(e.v), w: String(e.w), name: String(idx) });
  const points = (ed && ed.points ? ed.points : []).map((p) => ({
    x: p.x,
    y: p.y,
  }));
  const label =
    ed && ed.x !== undefined && ed.y !== undefined
      ? { x: ed.x, y: ed.y }
      : null;
  return { points, label };
});

const gl = g.graph();
const out = { nodes, edges, size: { w: gl.width, h: gl.height } };
process.stdout.write(JSON.stringify(out, null, 2) + "\n");
