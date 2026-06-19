//! Spec coverage for hiker: which in-scope objects have *no* spec link (directly or via any member).
//! Reads the seeded link store + the scip; reports object/function coverage scoped to core+app, and
//! lists the biggest uncovered objects (the gaps worth a look). status: spec-coverage
//! Run: cargo run -p hiker-code --example coverage_specs -- <scip> <repo_root>

use std::collections::HashSet;
use std::path::Path;

use std::collections::HashMap;

use hiker_code::{GraphNode, ScipAdapter};
use spec_engine::{LinkStore, Resolution, SourceId};

const SCOPE: &[&str] = &["core/", "app/"];

fn in_scope(n: &GraphNode) -> bool {
    SCOPE.iter().any(|p| n.file.starts_with(p))
}

fn main() {
    let scip = std::env::args().nth(1).expect("usage: coverage_specs <scip> <repo_root>");
    let repo = std::env::args().nth(2).expect("repo_root");
    let ad = ScipAdapter::load(Path::new(&scip), Path::new(&repo), SourceId("hiker".into()))
        .expect("load scip");
    let g = ad.code_graph();
    let store_path = Path::new(&repo).join("links.json"); // durable baseline lives in the repo
    let store = LinkStore::load(&store_path).expect("load link store");
    let targets: HashSet<&str> = store.links.iter().map(|l| l.target.as_str()).collect();

    let is_target = |i: usize| targets.contains(g.nodes[i].id.as_str());

    // Direct/member coverage: the node, or some descendant, is a link target (propagate UP).
    let mut covered = vec![false; g.nodes.len()];
    for i in 0..g.nodes.len() {
        if is_target(i) {
            let mut cur = Some(i);
            while let Some(j) = cur {
                if covered[j] {
                    break;
                }
                covered[j] = true;
                cur = g.nodes[j].parent;
            }
        }
    }

    // Governed coverage: the node, or some ancestor (its module/type), is a link target — i.e. it
    // lives under a spec'd subsystem even if nothing tagged it directly (propagate DOWN).
    let mut governed = vec![false; g.nodes.len()];
    for i in 0..g.nodes.len() {
        let mut cur = Some(i);
        while let Some(j) = cur {
            if is_target(j) {
                governed[i] = true;
                break;
            }
            cur = g.nodes[j].parent;
        }
    }
    let either: Vec<bool> =
        (0..g.nodes.len()).map(|i| covered[i] || governed[i]).collect();

    let pct = |c: usize, t: usize| if t == 0 { 0.0 } else { 100.0 * c as f64 / t as f64 };
    let kinds: [(&str, fn(&GraphNode) -> bool); 3] = [
        ("objects (type/module)", |n| n.is_object()),
        ("functions/methods", |n| matches!(n.kind.as_str(), "code:function" | "code:method")),
        ("types only", |n| n.kind == "code:type"),
    ];
    println!("== spec coverage (scope: {SCOPE:?}) ==\n");
    println!("{:24} {:>16}   {:>16}", "", "direct/member", "governed (subsys)");
    for (label, pred) in kinds {
        let total: Vec<usize> =
            (0..g.nodes.len()).filter(|&i| in_scope(&g.nodes[i]) && pred(&g.nodes[i])).collect();
        let direct = total.iter().filter(|&&i| covered[i]).count();
        let gov = total.iter().filter(|&&i| either[i]).count();
        println!(
            "{label:24} {direct:5} ({:5.1}%)      {gov:5} ({:5.1}%)",
            pct(direct, total.len()),
            pct(gov, total.len()),
        );
    }

    // Per-altitude breakdown (`spec-resolution-c4`): how closely is each object watched? Altitude =
    // the FINEST resolution among links targeting the node or any ancestor. "members-only" = no
    // link on self/ancestors but members are pinned (action: add a Component `touches::` on the
    // container). This replaces a binary exempt bit: coarse governance stays in the graph, visible.
    let mut res_of: HashMap<&str, Resolution> = HashMap::new();
    for l in &store.links {
        res_of
            .entry(l.target.as_str())
            .and_modify(|r| *r = (*r).max(l.resolution))
            .or_insert(l.resolution);
    }
    let altitude = |mut i: usize| -> Option<Resolution> {
        let mut best: Option<Resolution> = None;
        loop {
            if let Some(&r) = res_of.get(g.nodes[i].id.as_str()) {
                best = Some(best.map_or(r, |b: Resolution| b.max(r)));
            }
            match g.nodes[i].parent {
                Some(p) => i = p,
                None => return best,
            }
        }
    };
    println!("\n== governance altitude (finest link on self/ancestors) ==\n");
    println!("{:24} {:>7} {:>11} {:>12} {:>13} {:>12}", "", "code", "component", "container+", "members-only", "ungoverned");
    for (label, pred) in kinds {
        let idxs: Vec<usize> =
            (0..g.nodes.len()).filter(|&i| in_scope(&g.nodes[i]) && pred(&g.nodes[i])).collect();
        let (mut code, mut comp, mut coarse, mut members, mut none) = (0, 0, 0, 0, 0);
        for &i in &idxs {
            match altitude(i) {
                Some(Resolution::Code) => code += 1,
                Some(Resolution::Component) => comp += 1,
                Some(_) => coarse += 1,
                None if covered[i] => members += 1,
                None => none += 1,
            }
        }
        let t = idxs.len();
        println!(
            "{label:24} {:6.1}% {:10.1}% {:11.1}% {:12.1}% {:11.1}%",
            pct(code, t), pct(comp, t), pct(coarse, t), pct(members, t), pct(none, t)
        );
    }

    // The gaps worth a look: largest uncovered types (big surface, zero spec).
    let mut gaps: Vec<&GraphNode> = g
        .nodes
        .iter()
        .enumerate()
        .filter(|(i, n)| in_scope(n) && n.kind == "code:type" && !covered[*i])
        .map(|(_, n)| n)
        .collect();
    gaps.sort_by(|a, b| b.lines.cmp(&a.lines));
    println!("\n-- largest uncovered types (gaps) --");
    for n in gaps.iter().take(15) {
        println!("  {:4} lines  {}  @ {}:{}", n.lines, n.name, n.file, n.start_line + 1);
    }
    println!("\n({} uncovered types total)", gaps.len());

    // Subsystem grouping (path component after core/src|app/src) — the fan-out work-list: uncovered
    // significant objects (types + free functions/methods) per subsystem, for disjoint agent assignment.
    use std::collections::BTreeMap;
    let mut by_sub: BTreeMap<String, usize> = BTreeMap::new();
    for (i, n) in g.nodes.iter().enumerate() {
        let significant = matches!(n.kind.as_str(), "code:type" | "code:function" | "code:method");
        if !in_scope(n) || covered[i] || !significant {
            continue;
        }
        // core/src/<sub>/... or app/src/<sub>/...
        let parts: Vec<&str> = n.file.split('/').collect();
        let sub = match (parts.first(), parts.get(2)) {
            (Some(c), Some(s)) => format!("{c}/{s}"),
            _ => n.file.clone(),
        };
        *by_sub.entry(sub).or_default() += 1;
    }
    let mut subs: Vec<_> = by_sub.into_iter().collect();
    subs.sort_by(|a, b| b.1.cmp(&a.1));
    println!("\n-- uncovered significant objects by subsystem (fan-out work-list) --");
    for (sub, n) in subs.iter().take(30) {
        println!("  {n:4}  {sub}");
    }
}
