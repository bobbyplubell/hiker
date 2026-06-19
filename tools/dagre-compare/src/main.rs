//! `dagre-compare` — validate the pure-Rust dagre port (`hiker_graph::LayeredEngine`)
//! against real `@dagrejs/dagre` on *identical* graph inputs.
//!
//! The layered layout is deterministic and was ported from dagre's own test
//! suite, so given the same nodes / sizes / edges / options the two should
//! produce the same coordinates. Pixel-level diffs of *rendered* diagrams are
//! noisy (font metrics, our SVG paint), but the layout layer should match
//! closely — this tool isolates exactly that layer.
//!
//! Two subcommands, glued together by `run.sh`:
//!   * `emit <fixture.json>`        run a fixture through `LayeredEngine`, print
//!                                  the layout as JSON on stdout.
//!   * `diff <ours.json> <theirs.json> [--tol PX]`
//!                                  compare two layout JSONs (ours vs the
//!                                  oracle's), print a per-fixture report, exit
//!                                  non-zero if any delta exceeds the tolerance.
//!
//! The fixture and output JSON schemas are shared with the oracle's `run.js`
//! (see this tool's README), so both sides read the same input and emit the
//! same shape.

use std::process::ExitCode;

use hiker_graph::layered::types::RankDir;
use hiker_graph::{GraphInput, LayeredEngine, LayoutEngine, Vec2};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Shared JSON schemas (fixture input + layout output), identical to `run.js`.
// ---------------------------------------------------------------------------

/// A graph fixture: explicit node sizes + edges + layout options. Both the Rust
/// engine and the JS oracle consume this verbatim, so the only variable left is
/// the layout algorithm itself.
#[derive(Debug, Deserialize)]
struct Fixture {
    #[allow(dead_code)]
    #[serde(default)]
    name: String,
    #[serde(default = "default_rankdir")]
    rankdir: String,
    #[serde(default = "default_ranksep")]
    ranksep: f32,
    #[serde(default = "default_nodesep")]
    nodesep: f32,
    #[serde(default = "default_edgesep")]
    edgesep: f32,
    nodes: Vec<NodeSpec>,
    #[serde(default)]
    edges: Vec<EdgeSpec>,
    /// Optional per-node parent index (cluster membership). Length must match
    /// `nodes` when present.
    #[serde(default)]
    parents: Option<Vec<Option<usize>>>,
}

fn default_rankdir() -> String {
    "TB".to_string()
}
const fn default_ranksep() -> f32 {
    50.0
}
const fn default_nodesep() -> f32 {
    50.0
}
const fn default_edgesep() -> f32 {
    // dagre's own default `edgesep` is 20.
    20.0
}

#[derive(Debug, Deserialize)]
struct NodeSpec {
    w: f32,
    h: f32,
}

#[derive(Debug, Deserialize)]
struct EdgeSpec {
    v: u32,
    w: u32,
    /// Optional edge-label box. dagre reserves a gap for it between ranks and
    /// reports where it placed the label center.
    #[serde(default)]
    label: Option<NodeSpec>,
}

/// The layout result — emitted by both sides, consumed by `diff`.
#[derive(Debug, Serialize, Deserialize)]
struct Layout {
    nodes: Vec<NodeOut>,
    edges: Vec<EdgeOut>,
    size: SizeOut,
}

#[derive(Debug, Serialize, Deserialize)]
struct NodeOut {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

#[derive(Debug, Serialize, Deserialize)]
struct EdgeOut {
    points: Vec<Pt>,
    label: Option<Pt>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct Pt {
    x: f32,
    y: f32,
}

#[derive(Debug, Serialize, Deserialize)]
struct SizeOut {
    w: f32,
    h: f32,
}

// ---------------------------------------------------------------------------
// `emit`: run a fixture through our LayeredEngine.
// ---------------------------------------------------------------------------

fn rankdir_of(s: &str) -> RankDir {
    match s.to_ascii_uppercase().as_str() {
        "BT" => RankDir::Bt,
        "LR" => RankDir::Lr,
        "RL" => RankDir::Rl,
        _ => RankDir::Tb,
    }
}

fn run_engine(fx: &Fixture) -> Layout {
    let node_sizes: Vec<Vec2> = fx.nodes.iter().map(|n| Vec2::new(n.w, n.h)).collect();
    let edges: Vec<(u32, u32)> = fx.edges.iter().map(|e| (e.v, e.w)).collect();
    let edge_label_sizes: Vec<Option<Vec2>> = fx
        .edges
        .iter()
        .map(|e| e.label.as_ref().map(|l| Vec2::new(l.w, l.h)))
        .collect();
    let has_labels = edge_label_sizes.iter().any(std::option::Option::is_some);

    let engine = LayeredEngine {
        rankdir: rankdir_of(&fx.rankdir),
        ranksep: fx.ranksep,
        nodesep: fx.nodesep,
        edgesep: fx.edgesep,
        default_node_size: Vec2::new(50.0, 50.0),
        // Conformance harness: must exercise the dagre-faithful path only.
        transpose: false,
    };

    let input = GraphInput {
        node_count: fx.nodes.len(),
        edges: &edges,
        node_sizes: Some(&node_sizes),
        edge_label_sizes: if has_labels {
            Some(&edge_label_sizes)
        } else {
            None
        },
        node_parents: fx.parents.as_deref(),
        directed: true,
    };

    let out = engine.layout(&input);

    let nodes = out
        .positions
        .iter()
        .zip(out.node_sizes.iter())
        .map(|(p, s)| NodeOut {
            x: p.x,
            y: p.y,
            w: s.x,
            h: s.y,
        })
        .collect();

    let edges_out = out
        .edge_routes
        .iter()
        .zip(out.edge_label_positions.iter())
        .map(|(route, label)| EdgeOut {
            points: route.iter().map(|p| Pt { x: p.x, y: p.y }).collect(),
            label: label.map(|p| Pt { x: p.x, y: p.y }),
        })
        .collect();

    Layout {
        nodes,
        edges: edges_out,
        size: SizeOut {
            w: out.size.x,
            h: out.size.y,
        },
    }
}

// ---------------------------------------------------------------------------
// `diff`: compare two layouts.
// ---------------------------------------------------------------------------

/// A running max/mean accumulator over a set of scalar deltas.
#[derive(Default)]
struct Stat {
    max: f32,
    sum: f32,
    n: usize,
}

impl Stat {
    fn add(&mut self, v: f32) {
        self.max = self.max.max(v);
        self.sum += v;
        self.n += 1;
    }
    fn mean(&self) -> f32 {
        if self.n == 0 {
            0.0
        } else {
            self.sum / self.n as f32
        }
    }
}

fn diff(ours: &Layout, theirs: &Layout, tol: f32) -> bool {
    let mut ok = true;
    println!("  nodes: ours={} theirs={}", ours.nodes.len(), theirs.nodes.len());
    if ours.nodes.len() != theirs.nodes.len() {
        println!("  !! node count mismatch");
        return false;
    }

    // Node centers + sizes (positionally aligned: both sides use index ids).
    let mut center = Stat::default();
    let mut size = Stat::default();
    let mut worst: Option<(usize, f32)> = None;
    for (i, (a, b)) in ours.nodes.iter().zip(theirs.nodes.iter()).enumerate() {
        let d = ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt();
        center.add(d);
        size.add((a.w - b.w).abs().max((a.h - b.h).abs()));
        if worst.map(|(_, wd)| d > wd).unwrap_or(true) {
            worst = Some((i, d));
        }
    }
    println!(
        "  node center delta:  max={:.2}px  mean={:.2}px",
        center.max,
        center.mean()
    );
    println!(
        "  node size   delta:  max={:.2}px  mean={:.2}px",
        size.max,
        size.mean()
    );
    if let Some((i, d)) = worst {
        if d > tol {
            let a = &ours.nodes[i];
            let b = &theirs.nodes[i];
            println!(
                "    worst node #{i}: ours=({:.1},{:.1}) theirs=({:.1},{:.1})",
                a.x, a.y, b.x, b.y
            );
        }
    }

    // Graph bounding size.
    let dw = (ours.size.w - theirs.size.w).abs();
    let dh = (ours.size.h - theirs.size.h).abs();
    println!(
        "  graph size:         ours=({:.1}x{:.1}) theirs=({:.1}x{:.1})  delta=({:.2},{:.2})",
        ours.size.w, ours.size.h, theirs.size.w, theirs.size.h, dw, dh
    );

    // Edge label centers (only edges that both placed).
    let mut label = Stat::default();
    let mut label_count_mismatch = 0;
    for (a, b) in ours.edges.iter().zip(theirs.edges.iter()) {
        match (a.label, b.label) {
            (Some(pa), Some(pb)) => {
                label.add(((pa.x - pb.x).powi(2) + (pa.y - pb.y).powi(2)).sqrt())
            }
            (None, None) => {}
            _ => label_count_mismatch += 1,
        }
    }
    if label.n > 0 {
        println!(
            "  edge label delta:   max={:.2}px  mean={:.2}px  (over {} labels)",
            label.max,
            label.mean(),
            label.n
        );
    }
    if label_count_mismatch > 0 {
        println!("  !! {label_count_mismatch} edges where only one side placed a label");
    }

    // Edge polyline endpoints (start/end — the points that touch node borders;
    // intermediate dummy routing can legitimately differ in vertex count).
    let mut endpoint = Stat::default();
    for (a, b) in ours.edges.iter().zip(theirs.edges.iter()) {
        if let (Some(a0), Some(b0)) = (a.points.first(), b.points.first()) {
            endpoint.add(((a0.x - b0.x).powi(2) + (a0.y - b0.y).powi(2)).sqrt());
        }
        if let (Some(a1), Some(b1)) = (a.points.last(), b.points.last()) {
            endpoint.add(((a1.x - b1.x).powi(2) + (a1.y - b1.y).powi(2)).sqrt());
        }
    }
    if endpoint.n > 0 {
        println!(
            "  edge endpoint delta:max={:.2}px  mean={:.2}px",
            endpoint.max,
            endpoint.mean()
        );
    }

    // Verdict against tolerance (node centers + graph size are the load-bearing
    // signals; edge routing vertex counts are allowed to differ).
    if center.max > tol {
        println!("  VERDICT: FAIL — node centers diverge beyond {tol}px", tol = tol);
        ok = false;
    } else if dw > tol || dh > tol {
        println!("  VERDICT: FAIL — graph size diverges beyond {tol}px");
        ok = false;
    } else {
        println!("  VERDICT: ok (within {tol}px)");
    }
    ok
}

// ---------------------------------------------------------------------------

fn usage() -> ExitCode {
    eprintln!(
        "usage:\n  \
         dagre-compare emit <fixture.json>\n  \
         dagre-compare diff <ours.json> <theirs.json> [--tol PX]"
    );
    ExitCode::from(2)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &str) -> T {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    serde_json::from_slice(&bytes).unwrap_or_else(|e| panic!("parse {path}: {e}"))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("emit") => {
            let Some(path) = args.get(1) else {
                return usage();
            };
            let fx: Fixture = read_json(path);
            let layout = run_engine(&fx);
            println!("{}", serde_json::to_string_pretty(&layout).unwrap());
            ExitCode::SUCCESS
        }
        Some("diff") => {
            let (Some(a), Some(b)) = (args.get(1), args.get(2)) else {
                return usage();
            };
            let tol = args
                .iter()
                .position(|x| x == "--tol")
                .and_then(|i| args.get(i + 1))
                .and_then(|s| s.parse::<f32>().ok())
                .unwrap_or(1.0);
            let ours: Layout = read_json(a);
            let theirs: Layout = read_json(b);
            if diff(&ours, &theirs, tol) {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        _ => usage(),
    }
}
