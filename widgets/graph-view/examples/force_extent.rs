//! Diagnostic: run the REAL engine force layout (`hiker_graph::force_to_convergence`)
//! on a `.scip` code graph and report the settled extent + how many nodes pile
//! up at the `bound` wall — under weak vs strong gravity. Proves the "square
//! border during settle" is the `bound` clamp catching a weak-gravity layout
//! whose natural radius (~M/gravity) blows past it.
//! Run: `cargo run --release -p hiker-graph-view --example force_extent -- <index.scip> <repo>`

use hiker_code::ScipAdapter;
use hiker_graph::{force_to_convergence, LayoutParams, Vec2};
use spec_engine::SourceId;

fn scatter(n: usize, box_size: f32) -> Vec<Vec2> {
    // Deterministic LCG scatter in a centred box (no Math::random in examples-as-bench).
    let mut s: u64 = 0x9E3779B97F4A7C15;
    let mut next = || {
        s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((s >> 33) as f32 / (1u64 << 31) as f32) - 1.0 // ~[-1,1]
    };
    (0..n).map(|_| Vec2::new(next() * box_size, next() * box_size)).collect()
}

fn report(label: &str, pos: &[Vec2], bound: f32) {
    let mut max_r = 0.0f32;
    let mut sum_r = 0.0f64;
    let mut at_wall = 0usize;
    for p in pos {
        let r = (p.x * p.x + p.y * p.y).sqrt();
        max_r = max_r.max(r);
        sum_r += r as f64;
        // "On the wall": within 1% of the per-axis clamp on either axis.
        if p.x.abs() >= bound * 0.99 || p.y.abs() >= bound * 0.99 {
            at_wall += 1;
        }
    }
    let mean = sum_r / pos.len() as f64;
    println!(
        "  {label:<16} max_r={max_r:>10.0}  mean_r={mean:>9.0}  at_wall={at_wall:>6} ({:.1}%)",
        100.0 * at_wall as f32 / pos.len() as f32
    );
}

fn main() {
    let mut args = std::env::args().skip(1);
    let index = args.next().unwrap_or_else(|| "code-intel/fixtures/hiker.scip".into());
    let repo = args.next().unwrap_or_else(|| "code-intel/fixtures/hiker".into());

    let adapter = ScipAdapter::load(index.as_ref(), repo.as_ref(), SourceId(repo.clone()))
        .expect("load scip");
    let graph = adapter.code_graph();
    let n = graph.nodes.len();
    let edges: Vec<(u32, u32)> =
        graph.edges.iter().map(|&(a, b, _)| (a as u32, b as u32)).collect();
    let mass = n + 2 * edges.len();
    let bound = 50_000.0;
    println!(
        "graph: {n} nodes, {} edges | mass≈{mass} | weak-gravity equilibrium ~M/g={mass} vs bound={bound:.0}",
        edges.len()
    );

    // Neighbourhood (hops) sanity: replicate code_graph::neighborhood's BFS over a
    // few nodes and print depth-1/2/3 sizes — to see if "1 hop has no nodes".
    {
        let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
        for &(a, b, _) in &graph.edges {
            adj[a].push(b);
            adj[b].push(a);
        }
        let mut deg: Vec<(usize, usize)> = (0..n).map(|i| (adj[i].len(), i)).collect();
        deg.sort_unstable_by(|a, b| b.0.cmp(&a.0));
        let bfs = |focus: usize, depth: usize| -> usize {
            let mut seen = vec![false; n];
            let mut q = std::collections::VecDeque::from([(focus, 0usize)]);
            seen[focus] = true;
            let mut keep = 0;
            while let Some((nd, d)) = q.pop_front() {
                keep += 1;
                if d == depth { continue; }
                for &m in &adj[nd] {
                    if !seen[m] { seen[m] = true; q.push_back((m, d + 1)); }
                }
            }
            keep
        };
        println!("neighbourhood sizes (focus = top-degree nodes):");
        for &(d, i) in deg.iter().take(3) {
            println!(
                "  node[{i}] '{}' ({}) deg={d}: 1hop={} 2hop={} 3hop={}",
                graph.nodes[i].name, graph.nodes[i].kind, bfs(i, 1), bfs(i, 2), bfs(i, 3)
            );
        }
        // Specifically: how do code:type nodes (what you drill into) fare?
        let types: Vec<usize> = (0..n).filter(|&i| graph.nodes[i].kind == "code:type").collect();
        let type_deg0 = types.iter().filter(|&&i| adj[i].is_empty()).count();
        println!("code:type nodes: {} total, {} with degree 0 ({:.0}% have NO edges)",
            types.len(), type_deg0, 100.0 * type_deg0 as f32 / types.len().max(1) as f32);
        for &i in types.iter().filter(|&&i| !adj[i].is_empty()).take(3) {
            println!("  type[{i}] '{}' deg={}: 1hop={} 2hop={}", graph.nodes[i].name, adj[i].len(), bfs(i, 1), bfs(i, 2));
        }
    }

    // LOC weighting feasibility: is the SCIP enclosing_range populated, and does it vary?
    {
        let locs: Vec<u32> = graph.nodes.iter().map(|n| n.lines).collect();
        let multi = locs.iter().filter(|&&l| l > 1).count();
        let max = locs.iter().copied().max().unwrap_or(0);
        let sum: u64 = locs.iter().map(|&l| l as u64).sum();
        let mut sorted = locs.clone();
        sorted.sort_unstable();
        let p50 = sorted[sorted.len() / 2];
        let p95 = sorted[sorted.len() * 95 / 100];
        println!(
            "LOC per node: {}% have >1 line | mean={:.1} p50={} p95={} max={}",
            100 * multi / n, sum as f64 / n as f64, p50, p95, max
        );
        // A few biggest-body nodes.
        let mut by_loc: Vec<(u32, &str, &str)> = graph.nodes.iter().map(|x| (x.lines, x.name.as_str(), x.kind.as_str())).collect();
        by_loc.sort_unstable_by(|a, b| b.0.cmp(&a.0));
        for (l, name, kind) in by_loc.iter().take(4) {
            println!("  {l:>4} loc  {name} ({kind})");
        }
    }

    let seed = scatter(n, 80.0);

    // Current: weak gravity, bound=50k (clamps — the wall).
    let clamped = LayoutParams { bound, max_iters: 400, ..LayoutParams::default() };
    let cores = std::thread::available_parallelism().map(std::num::NonZero::get).unwrap_or(0);
    let t0 = std::time::Instant::now();
    let clamped_pos = force_to_convergence(seed.clone(), &edges, &clamped, || false);
    let elapsed = t0.elapsed();
    println!(
        "TIMING weak bound=50k layout: {:.3}s wall ({n} nodes, {} edges, max_iters=400, cores={cores})",
        elapsed.as_secs_f64(),
        edges.len()
    );
    report("weak bound=50k", &clamped_pos, bound);

    // Weak gravity, effectively-unbounded — the TRUE natural extent it wants.
    let huge = 50_000_000.0;
    let free = LayoutParams { bound: huge, max_iters: 400, ..LayoutParams::default() };
    let free_pos = force_to_convergence(seed.clone(), &edges, &free, || false);
    report("weak free", &free_pos, huge);

    // Spread via repulsion strength (scaling_ratio): radius ~ sqrt(scaling_ratio).
    for spread in [0.5f32, 1.0, 2.0, 4.0] {
        let sr = 100.0 * spread * spread; // radius ∝ sqrt(sr) ⇒ ∝ spread
        let b = (3.0 * spread * mass as f32).max(50_000.0);
        let p = LayoutParams { bound: b, scaling_ratio: sr, max_iters: 400, ..LayoutParams::default() };
        report(&format!("spread={spread:.1} (sr={sr:.0})"), &force_to_convergence(seed.clone(), &edges, &p, || false), b);
    }
}
