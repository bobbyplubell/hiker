// Debug helper (manual use): replicate dagre's runLayout pass order on a
// fixture from stdin, dumping node ranks after the rank-affecting passes, so a
// divergence in the Rust port can be pinned to a specific pass.
//
//   docker run --rm -i --entrypoint node dagre-compare-oracle /oracle/debug-ranks.js < fixture.json
const fs = require("fs");
const dagre = require("@dagrejs/dagre");

const acyclic = require("@dagrejs/dagre/lib/acyclic");
const normalize = require("@dagrejs/dagre/lib/normalize");
const rank = require("@dagrejs/dagre/lib/rank");
const nestingGraph = require("@dagrejs/dagre/lib/nesting-graph");
const util = require("@dagrejs/dagre/lib/util");

const fx = JSON.parse(fs.readFileSync(0, "utf8"));

const g = new dagre.graphlib.Graph({ directed: true, multigraph: true, compound: true });
g.setGraph({ rankdir: fx.rankdir || "TB", ranksep: fx.ranksep ?? 50, nodesep: fx.nodesep ?? 50, edgesep: fx.edgesep ?? 20, marginx: 0, marginy: 0 });
g.setDefaultEdgeLabel(() => ({}));
fx.nodes.forEach((n, i) => g.setNode(String(i), { width: n.w, height: n.h }));
if (Array.isArray(fx.parents)) fx.parents.forEach((p, i) => { if (p !== null && p !== undefined && p !== i) g.setParent(String(i), String(p)); });
(fx.edges || []).forEach((e, idx) => {
  const lbl = {};
  if (e.label && e.label.w > 0 && e.label.h > 0) { lbl.width = e.label.w; lbl.height = e.label.h; lbl.labelpos = "c"; }
  g.setEdge(String(e.v), String(e.w), lbl, String(idx));
});

// Mirror lib/layout.js buildLayoutGraph defaults enough for the rank passes.
const lg = new dagre.graphlib.Graph({ multigraph: true, compound: true });
lg.setGraph(Object.assign({ ranksep: 50, edgesep: 20, nodesep: 50, rankdir: "tb" }, (() => { const o = {}; for (const k of ["ranksep","edgesep","nodesep","rankdir","marginx","marginy"]) if (g.graph()[k] !== undefined) o[k] = g.graph()[k]; return o; })()));
g.nodes().forEach(v => { const n = g.node(v); lg.setNode(v, { width: n.width || 0, height: n.height || 0 }); lg.setParent(v, g.parent(v)); });
g.edges().forEach(e => { const ed = g.edge(e); lg.setEdge(e, Object.assign({ minlen: 1, weight: 1, width: 0, height: 0, labeloffset: 10, labelpos: "r" }, ed)); });

const dump = (tag) => {
  const ranks = lg.nodes().map(v => `${v}:${lg.node(v).rank}`).sort();
  console.log(tag, ranks.join(" "));
};

// makeSpaceForEdgeLabels
lg.graph().ranksep /= 2;
lg.edges().forEach(e => { const edge = lg.edge(e); edge.minlen *= 2; if (edge.labelpos.toLowerCase() !== "c") { if (lg.graph().rankdir === "TB" || lg.graph().rankdir === "BT") edge.width += edge.labelwidth || 0; else edge.height += edge.labelheight || 0; } });

acyclic.run(lg);
nestingGraph.run(lg);
console.log("nesting: nodeRankFactor =", lg.graph().nodeRankFactor);
lg.edges().forEach(e => console.log("  edge", e.v, "->", e.w, "minlen", lg.edge(e).minlen, "weight", lg.edge(e).weight, lg.edge(e).nestingEdge ? "(nesting)" : ""));
rank(util.asNonCompoundGraph(lg));
dump("after-rank:");
util.removeEmptyRanks(lg);
dump("after-removeEmptyRanks:");
nestingGraph.cleanup(lg);
util.normalizeRanks(lg);
dump("after-normalizeRanks:");
