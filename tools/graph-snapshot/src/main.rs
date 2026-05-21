//! Headless force-layout snapshotter. Builds a synthetic clustered
//! graph (or accepts CLI knobs for shape), runs the production
//! `force_layout` worker to convergence, and dumps a PNG so we can
//! iterate on FA2 params without firing up the full app.
//!
//! The two algorithm modules are pulled in via `#[path]` so we're
//! testing the same code the app runs — no copy/paste drift.

#[path = "../../../app/src/widgets/force_layout.rs"]
mod force_layout;

#[path = "../../../app/src/widgets/graph_layouts.rs"]
mod graph_layouts;

use std::io::Write;
use std::time::{Duration, Instant};

use egui::Vec2;
use image::{Rgba, RgbaImage};

use force_layout::{LayoutParams, LayoutWorker};
use graph_layouts::{
    bfs_tree, dfs_tree, horizontal_tree_positions, radial_positions, vertical_tree_positions,
};

/// CLI configuration. Hand-parsed instead of pulling clap to keep the
/// crate's deps minimal.
#[derive(Clone)]
struct Args {
    /// Number of clusters in the synthetic graph.
    clusters: usize,
    /// Nodes per cluster.
    per_cluster: usize,
    /// Number of inter-cluster bridge edges.
    bridges: usize,
    /// Number of hub-and-spoke subgraphs (n leaves around a single hub).
    hubs: usize,
    /// Leaves per hub.
    hub_leaves: usize,
    /// Intra-cluster edge density (0..1). 0.25 = roughly each pair has
    /// a 25% chance of an edge.
    intra_density: f32,

    /// "force" | "radial" | "vertical" | "horizontal"
    layout: String,
    /// "graph" (clustered with hubs) or "tree" (cluster-tree shape:
    /// synthetic root → N branching levels). Force-directed on a tree
    /// topology is a different regime from force-directed on a graph.
    synth_kind: String,
    tree_branching: usize,
    tree_depth: usize,

    out: String,
    width: u32,
    height: u32,

    // Layout params (mirrors `LayoutParams`).
    scaling_ratio: f32,
    gravity: f32,
    strong_gravity: bool,
    slow_down: f32,
    lin_log: bool,
    outbound_attr: bool,
    degree_repulsion: bool,
    max_iters: u32,
    theta: f32,
    /// Hard timeout — bail out of the wait loop after this even if the
    /// worker is still thinking.
    timeout_secs: u64,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            clusters: 5,
            per_cluster: 18,
            bridges: 4,
            hubs: 2,
            hub_leaves: 12,
            intra_density: 0.18,
            layout: "force".into(),
            synth_kind: "graph".into(),
            tree_branching: 4,
            tree_depth: 3,
            out: "/tmp/graph.png".into(),
            width: 1400,
            height: 1000,
            scaling_ratio: 100.0,
            gravity: 1.0,
            strong_gravity: false,
            slow_down: 5.0,
            lin_log: false,
            outbound_attr: false,
            degree_repulsion: true,
            max_iters: 800,
            theta: 0.9,
            timeout_secs: 30,
        }
    }
}

fn parse_args() -> Args {
    let mut a = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let mut v = || it.next().expect("missing value for arg");
        match k.as_str() {
            "--clusters" => a.clusters = v().parse().unwrap(),
            "--per-cluster" => a.per_cluster = v().parse().unwrap(),
            "--bridges" => a.bridges = v().parse().unwrap(),
            "--hubs" => a.hubs = v().parse().unwrap(),
            "--hub-leaves" => a.hub_leaves = v().parse().unwrap(),
            "--intra-density" => a.intra_density = v().parse().unwrap(),
            "--layout" => a.layout = v(),
            "--synth" => a.synth_kind = v(),
            "--branching" => a.tree_branching = v().parse().unwrap(),
            "--depth" => a.tree_depth = v().parse().unwrap(),
            "--out" => a.out = v(),
            "--size" => {
                let s = v();
                let (w, h) = s.split_once('x').expect("size form is WxH");
                a.width = w.parse().unwrap();
                a.height = h.parse().unwrap();
            }
            "--scaling" => a.scaling_ratio = v().parse().unwrap(),
            "--gravity" => a.gravity = v().parse().unwrap(),
            "--strong-gravity" => a.strong_gravity = true,
            "--slow-down" => a.slow_down = v().parse().unwrap(),
            "--lin-log" => a.lin_log = true,
            "--no-outbound" => a.outbound_attr = false,
            "--no-degree-repulsion" => a.degree_repulsion = false,
            "--iters" => a.max_iters = v().parse().unwrap(),
            "--theta" => a.theta = v().parse().unwrap(),
            "--timeout" => a.timeout_secs = v().parse().unwrap(),
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown arg: {other}");
                print_help();
                std::process::exit(2);
            }
        }
    }
    a
}

fn print_help() {
    eprintln!(
        "graph-snapshot — render synthetic force-layout to PNG\n\n\
         Graph shape:\n  \
         --clusters N        # tightly-connected clusters\n  \
         --per-cluster N     nodes per cluster\n  \
         --bridges N         edges between random clusters\n  \
         --hubs N            extra hub-and-spoke subgraphs\n  \
         --hub-leaves N      leaves per hub\n  \
         --intra-density F   per-pair intra-cluster edge probability\n\n\
         Force params (all map onto force_layout::LayoutParams):\n  \
         --scaling F  --gravity F  --strong-gravity\n  \
         --slow-down F  --lin-log  --no-outbound\n  \
         --no-degree-repulsion  --iters N  --theta F  --timeout SECS\n\n\
         Output:\n  --out PATH (default /tmp/graph.png)  --size WxH (default 1400x1000)"
    );
}

/// Tiny LCG. Sufficient for reproducible test graphs.
struct Lcg(u64);
impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }
    fn next_u32(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) as u32
    }
    fn next_f32(&mut self) -> f32 {
        self.next_u32() as f32 / u32::MAX as f32
    }
    fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + (self.next_u32() as usize) % (hi - lo).max(1)
    }
}

struct Graph {
    n: usize,
    edges: Vec<(u32, u32)>,
    /// Cluster index per node (for colouring). `usize::MAX` = unassigned.
    cluster_of: Vec<usize>,
    /// Marks hub nodes for a different visual treatment.
    is_hub: Vec<bool>,
}

fn synth(args: &Args) -> Graph {
    let mut rng = Lcg::new(0xC0FFEE);
    let mut edges = Vec::new();
    let mut cluster_of = Vec::new();
    let mut is_hub = Vec::new();

    // 1) Cluster blocks.
    let cluster_starts: Vec<usize> = (0..args.clusters)
        .map(|c| c * args.per_cluster)
        .collect();
    for c in 0..args.clusters {
        for _ in 0..args.per_cluster {
            cluster_of.push(c);
            is_hub.push(false);
        }
    }
    // Random intra-cluster edges.
    for &start in cluster_starts.iter().take(args.clusters) {
        for i in start..(start + args.per_cluster) {
            for j in (i + 1)..(start + args.per_cluster) {
                if rng.next_f32() < args.intra_density {
                    edges.push((i as u32, j as u32));
                }
            }
        }
    }

    // 2) Inter-cluster bridges.
    for _ in 0..args.bridges {
        let c1 = rng.range(0, args.clusters);
        let mut c2 = rng.range(0, args.clusters);
        if c2 == c1 {
            c2 = (c2 + 1) % args.clusters;
        }
        let a = cluster_starts[c1] + rng.range(0, args.per_cluster);
        let b = cluster_starts[c2] + rng.range(0, args.per_cluster);
        edges.push((a as u32, b as u32));
    }

    // 3) Hub-and-spoke subgraphs — connected to a random cluster so
    // they're not floating, and with a few leaf-to-leaf edges so the
    // flower pattern breaks symmetry the way a real vault's local
    // topology would.
    for _ in 0..args.hubs {
        let hub = cluster_of.len();
        cluster_of.push(usize::MAX);
        is_hub.push(true);
        let leaf_start = cluster_of.len();
        for _ in 0..args.hub_leaves {
            let leaf = cluster_of.len();
            cluster_of.push(usize::MAX);
            is_hub.push(false);
            edges.push((hub as u32, leaf as u32));
        }
        // Bridge hub into a random cluster.
        let c = rng.range(0, args.clusters);
        let target = cluster_starts[c] + rng.range(0, args.per_cluster);
        edges.push((hub as u32, target as u32));
        // A few leaf-leaf edges to break the ring symmetry.
        for _ in 0..(args.hub_leaves / 4).max(1) {
            let i = leaf_start + rng.range(0, args.hub_leaves);
            let j = leaf_start + rng.range(0, args.hub_leaves);
            if i != j {
                edges.push((i as u32, j as u32));
            }
        }
    }

    Graph {
        n: cluster_of.len(),
        edges,
        cluster_of,
        is_hub,
    }
}

fn build_params(a: &Args) -> LayoutParams {
    LayoutParams {
        bound: 10_000.0,
        max_iters: a.max_iters,
        theta: a.theta,
        convergence_eps: 0.5,
        convergence_streak: 20,
        scaling_ratio: a.scaling_ratio,
        gravity: a.gravity,
        strong_gravity: a.strong_gravity,
        slow_down: a.slow_down,
        lin_log: a.lin_log,
        outbound_attraction_distribution: a.outbound_attr,
        degree_repulsion: a.degree_repulsion,
    }
}

fn run_layout(g: &Graph, params: LayoutParams, timeout_secs: u64) -> Vec<Vec2> {
    // Small randomised seed; FA2 settles into its own scale.
    let mut rng = Lcg::new(0xBEEF);
    let seed: Vec<Vec2> = (0..g.n)
        .map(|_| Vec2::new((rng.next_f32() - 0.5) * 50.0, (rng.next_f32() - 0.5) * 50.0))
        .collect();
    let worker = LayoutWorker::spawn(seed.clone(), g.edges.clone(), params);
    let start = Instant::now();
    let mut last_iter = 0u32;
    while worker.is_running() {
        if start.elapsed() >= Duration::from_secs(timeout_secs) {
            eprintln!("(timeout — bailing with last snapshot)");
            break;
        }
        let it = worker.iters_done();
        if it != last_iter && it % 50 == 0 {
            print!("\r  iter {}…", it);
            std::io::stdout().flush().ok();
            last_iter = it;
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    println!(
        "\r  converged in {} iters ({:.2}s)        ",
        worker.iters_done(),
        start.elapsed().as_secs_f32()
    );
    let mut pos = seed;
    worker.snapshot_into(&mut pos);
    pos
}

/// Cluster palette — 8 hand-picked hues so blocks are visually distinct.
const PALETTE: &[Rgba<u8>] = &[
    Rgba([0xe6, 0x4d, 0x4d, 0xff]),
    Rgba([0x4d, 0xa3, 0xe6, 0xff]),
    Rgba([0x66, 0xc6, 0x6e, 0xff]),
    Rgba([0xe6, 0xc4, 0x4d, 0xff]),
    Rgba([0xb8, 0x6d, 0xe6, 0xff]),
    Rgba([0xe6, 0x88, 0x4d, 0xff]),
    Rgba([0x4d, 0xe6, 0xd0, 0xff]),
    Rgba([0xe6, 0x4d, 0xa8, 0xff]),
];

fn render_png(g: &Graph, pos: &[Vec2], args: &Args) {
    let mut img: RgbaImage = RgbaImage::from_pixel(
        args.width,
        args.height,
        Rgba([0x14, 0x18, 0x1d, 0xff]),
    );

    // Fit to image with margin.
    let margin = 30.0_f32;
    let (mut lo, mut hi) = (Vec2::splat(f32::INFINITY), Vec2::splat(f32::NEG_INFINITY));
    for &p in pos {
        lo.x = lo.x.min(p.x);
        lo.y = lo.y.min(p.y);
        hi.x = hi.x.max(p.x);
        hi.y = hi.y.max(p.y);
    }
    let span_x = (hi.x - lo.x).max(1.0);
    let span_y = (hi.y - lo.y).max(1.0);
    let avail_w = (args.width as f32) - margin * 2.0;
    let avail_h = (args.height as f32) - margin * 2.0;
    let scale = (avail_w / span_x).min(avail_h / span_y);
    let center_world = (lo + hi) * 0.5;
    let center_screen = Vec2::new(args.width as f32 * 0.5, args.height as f32 * 0.5);
    let to_screen = |w: Vec2| -> (f32, f32) {
        let s = center_screen + (w - center_world) * scale;
        (s.x, s.y)
    };

    // Edges first.
    let edge_col = Rgba([0x55, 0x5d, 0x68, 0x90]);
    for &(a, b) in &g.edges {
        let (a, b) = (a as usize, b as usize);
        if a >= g.n || b >= g.n {
            continue;
        }
        let (x1, y1) = to_screen(pos[a]);
        let (x2, y2) = to_screen(pos[b]);
        draw_line(&mut img, x1, y1, x2, y2, edge_col);
    }

    // Nodes on top.
    for i in 0..g.n {
        let (x, y) = to_screen(pos[i]);
        let color = if g.is_hub[i] {
            Rgba([0xff, 0xff, 0xff, 0xff])
        } else if g.cluster_of[i] == usize::MAX {
            Rgba([0xa0, 0xa0, 0xa0, 0xff])
        } else {
            PALETTE[g.cluster_of[i] % PALETTE.len()]
        };
        let radius = if g.is_hub[i] { 7.0 } else { 4.5 };
        fill_circle(&mut img, x, y, radius, color);
    }

    // Title overlay: render params at top-left so PNGs are self-labelled.
    let label = format!(
        "scaling={}  gravity={}{}  slow_down={}  lin_log={}  outbound={}  deg_rep={}",
        args.scaling_ratio,
        args.gravity,
        if args.strong_gravity { " (strong)" } else { "" },
        args.slow_down,
        args.lin_log,
        args.outbound_attr,
        args.degree_repulsion,
    );
    draw_label(&mut img, 8, 8, &label, Rgba([0xc6, 0xcc, 0xd5, 0xff]));

    img.save(&args.out).expect("failed to write PNG");
    eprintln!("wrote {}", args.out);
}

// ── Minimal pixel ops ───────────────────────────────────────────────

fn put_px(img: &mut RgbaImage, x: i32, y: i32, color: Rgba<u8>) {
    if x < 0 || y < 0 || x as u32 >= img.width() || y as u32 >= img.height() {
        return;
    }
    // Alpha blend over existing pixel.
    let dst = img.get_pixel_mut(x as u32, y as u32);
    let a = color[3] as f32 / 255.0;
    let inv = 1.0 - a;
    for c in 0..3 {
        dst[c] = ((color[c] as f32) * a + (dst[c] as f32) * inv) as u8;
    }
    dst[3] = 0xff;
}

fn fill_circle(img: &mut RgbaImage, cx: f32, cy: f32, r: f32, color: Rgba<u8>) {
    let r2 = r * r;
    let x0 = (cx - r).floor() as i32;
    let x1 = (cx + r).ceil() as i32;
    let y0 = (cy - r).floor() as i32;
    let y1 = (cy + r).ceil() as i32;
    for y in y0..=y1 {
        for x in x0..=x1 {
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            if dx * dx + dy * dy <= r2 {
                put_px(img, x, y, color);
            }
        }
    }
}

fn draw_line(img: &mut RgbaImage, x1: f32, y1: f32, x2: f32, y2: f32, color: Rgba<u8>) {
    let dx = x2 - x1;
    let dy = y2 - y1;
    let steps = dx.abs().max(dy.abs()).ceil() as i32;
    if steps <= 0 {
        return;
    }
    for s in 0..=steps {
        let t = s as f32 / steps as f32;
        let x = (x1 + dx * t).round() as i32;
        let y = (y1 + dy * t).round() as i32;
        put_px(img, x, y, color);
    }
}

/// 5×7 bitmap font for the params label. Only enough glyphs to render
/// what we actually put in the label.
fn draw_label(img: &mut RgbaImage, x0: i32, y0: i32, s: &str, color: Rgba<u8>) {
    let mut x = x0;
    for ch in s.chars() {
        if let Some(glyph) = glyph(ch) {
            for (row, bits) in glyph.iter().enumerate() {
                for col in 0..5 {
                    if bits & (1 << (4 - col)) != 0 {
                        put_px(img, x + col, y0 + row as i32, color);
                    }
                }
            }
        }
        x += 6;
    }
}

fn glyph(c: char) -> Option<[u8; 7]> {
    // 5-bit-wide rows, MSB = leftmost pixel. Bare-bones; unknown
    // characters render as a blank cell.
    Some(match c {
        'a' => [0x00, 0x00, 0x0E, 0x01, 0x0F, 0x11, 0x0F],
        'b' => [0x10, 0x10, 0x1E, 0x11, 0x11, 0x11, 0x1E],
        'c' => [0x00, 0x00, 0x0E, 0x10, 0x10, 0x11, 0x0E],
        'd' => [0x01, 0x01, 0x0F, 0x11, 0x11, 0x11, 0x0F],
        'e' => [0x00, 0x00, 0x0E, 0x11, 0x1F, 0x10, 0x0E],
        'f' => [0x06, 0x09, 0x08, 0x1E, 0x08, 0x08, 0x08],
        'g' => [0x00, 0x00, 0x0F, 0x11, 0x0F, 0x01, 0x0E],
        'h' => [0x10, 0x10, 0x16, 0x19, 0x11, 0x11, 0x11],
        'i' => [0x04, 0x00, 0x0C, 0x04, 0x04, 0x04, 0x0E],
        'l' => [0x0C, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E],
        'n' => [0x00, 0x00, 0x16, 0x19, 0x11, 0x11, 0x11],
        'o' => [0x00, 0x00, 0x0E, 0x11, 0x11, 0x11, 0x0E],
        'p' => [0x00, 0x00, 0x1E, 0x11, 0x1E, 0x10, 0x10],
        'r' => [0x00, 0x00, 0x16, 0x19, 0x10, 0x10, 0x10],
        's' => [0x00, 0x00, 0x0F, 0x10, 0x0E, 0x01, 0x1E],
        't' => [0x08, 0x08, 0x1C, 0x08, 0x08, 0x09, 0x06],
        'u' => [0x00, 0x00, 0x11, 0x11, 0x11, 0x13, 0x0D],
        'v' => [0x00, 0x00, 0x11, 0x11, 0x11, 0x0A, 0x04],
        'w' => [0x00, 0x00, 0x11, 0x11, 0x15, 0x15, 0x0A],
        'x' => [0x00, 0x00, 0x11, 0x0A, 0x04, 0x0A, 0x11],
        'y' => [0x00, 0x00, 0x11, 0x11, 0x0F, 0x01, 0x0E],
        'z' => [0x00, 0x00, 0x1F, 0x02, 0x04, 0x08, 0x1F],
        'A' => [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        '0' => [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
        '1' => [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
        '2' => [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F],
        '3' => [0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E],
        '4' => [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
        '5' => [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
        '6' => [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E],
        '7' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        '8' => [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
        '9' => [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C],
        '.' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x0C, 0x0C],
        '_' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1F],
        '=' => [0x00, 0x00, 0x1F, 0x00, 0x1F, 0x00, 0x00],
        ' ' => [0; 7],
        '(' => [0x02, 0x04, 0x08, 0x08, 0x08, 0x04, 0x02],
        ')' => [0x08, 0x04, 0x02, 0x02, 0x02, 0x04, 0x08],
        '-' => [0x00, 0x00, 0x00, 0x1F, 0x00, 0x00, 0x00],
        _ => return None,
    })
}

/// Build a synthetic cluster tree: synthetic root → branching N at
/// each of `depth` levels. Mirrors the topology `panels/cluster_graph`
/// receives from `trees::list_nodes`.
fn synth_tree(branching: usize, depth: usize) -> Graph {
    let mut edges = Vec::new();
    let mut cluster_of = Vec::new();
    let mut is_hub = Vec::new();

    // BFS construction so node indices are contiguous per level.
    let mut frontier: Vec<usize> = Vec::new();
    cluster_of.push(0);
    is_hub.push(true);
    frontier.push(0);

    let mut next_level: Vec<usize> = Vec::new();
    let mut color = 0usize;
    for level in 0..depth {
        next_level.clear();
        let mut leaf_color = color;
        for &parent in &frontier {
            for _ in 0..branching {
                let child = cluster_of.len();
                let is_leaf = level == depth - 1;
                cluster_of.push(if is_leaf { leaf_color } else { usize::MAX });
                is_hub.push(false);
                edges.push((parent as u32, child as u32));
                next_level.push(child);
            }
            leaf_color = (leaf_color + 1) % 8;
        }
        color = leaf_color;
        std::mem::swap(&mut frontier, &mut next_level);
    }

    Graph {
        n: cluster_of.len(),
        edges,
        cluster_of,
        is_hub,
    }
}

fn pick_root(g: &Graph) -> usize {
    // Highest-degree node (matches the vault-graph fallback).
    let mut deg = vec![0u32; g.n];
    for &(a, b) in &g.edges {
        if (a as usize) < g.n {
            deg[a as usize] += 1;
        }
        if (b as usize) < g.n {
            deg[b as usize] += 1;
        }
    }
    deg.iter()
        .enumerate()
        .max_by_key(|(_, d)| **d)
        .map(|(i, _)| i)
        .unwrap_or(0)
}

fn main() {
    let args = parse_args();
    eprintln!(
        "synth: {} clusters × {} nodes + {} hubs × {} leaves + {} bridges",
        args.clusters, args.per_cluster, args.hubs, args.hub_leaves, args.bridges,
    );
    let g = match args.synth_kind.as_str() {
        "graph" => synth(&args),
        "tree" => synth_tree(args.tree_branching, args.tree_depth),
        other => {
            eprintln!("unknown --synth: {other}");
            std::process::exit(2);
        }
    };
    eprintln!("  {} nodes, {} edges", g.n, g.edges.len());
    eprintln!("  layout: {}  synth: {}", args.layout, args.synth_kind);

    let pos = match args.layout.as_str() {
        "force" => run_layout(&g, build_params(&args), args.timeout_secs),
        "radial" | "vertical" | "horizontal" => {
            let root = pick_root(&g);
            let area = 1000.0 * 1000.0;
            let tree = match args.layout.as_str() {
                "radial" => bfs_tree(g.n, &g.edges, root),
                _ => dfs_tree(g.n, &g.edges, root),
            };
            match args.layout.as_str() {
                "radial" => radial_positions(&tree, area),
                "vertical" => vertical_tree_positions(&tree, area),
                "horizontal" => horizontal_tree_positions(&tree, area),
                _ => unreachable!(),
            }
        }
        other => {
            eprintln!("unknown layout: {other}");
            std::process::exit(2);
        }
    };
    render_png(&g, &pos, &args);
}
