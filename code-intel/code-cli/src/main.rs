//! code-cli — standalone harness over hiker-code + hiker-projects + spec-engine (non-UI). Proves
//! the engine without hiker. Examples (run from code-intel/):
//!   code-cli entities fixtures/sem.scip ../sem
//!   code-cli impact   fixtures/sem.scip ../sem EntityGraph
//!   code-cli link     fixtures/pyproj.scip fixtures/pyproj spec-areas total_area
//!   code-cli drift    fixtures/pyproj.scip fixtures/pyproj
//!   code-cli trace    fixtures/pyproj.scip fixtures/pyproj spec-areas
//!   code-cli graph    fixtures/pyproj.scip fixtures/pyproj --svg pyproj.svg
//!   code-cli graph    fixtures/sem.scip ../sem --focus EntityGraph --depth 2 --svg sem.svg
//!   code-cli graph    --project fixtures/pyproj-project.md --svg pyproj.svg

mod render;

use std::path::{Path, PathBuf};

use hiker_code::ScipAdapter;
use hiker_projects::Project;
use spec_engine::{DerivedNodeSource, EdgeKind, LinkStore, SourceId};

const USAGE: &str = "usage:\n\
  code-cli entities <index.scip> <repo_root>\n\
  code-cli impact   <index.scip> <repo_root> <name>\n\
  code-cli link     <index.scip> <repo_root> <spec> <name>\n\
  code-cli drift    <index.scip> <repo_root>\n\
  code-cli trace    <index.scip> <repo_root> <spec>\n\
  code-cli graph    <index.scip> <repo_root> [--focus <name>] [--depth N] [--max N] [--svg FILE] [--dot FILE]\n\
  code-cli graph    --project <note.md>       [--focus <name>] [--depth N] [--max N] [--svg FILE] [--dot FILE]";

fn die(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(2);
}

/// SourceId = the index file stem, so distinct repos don't collide in a shared link store.
fn source_id(index: &str) -> SourceId {
    SourceId(Path::new(index).file_stem().and_then(|s| s.to_str()).unwrap_or("src").to_string())
}

fn store_path(index: &str) -> PathBuf {
    Path::new(index).parent().unwrap_or_else(|| Path::new(".")).join("links.json")
}

fn load(index: &str, repo: &str) -> (ScipAdapter, SourceId) {
    let src = source_id(index);
    match ScipAdapter::load(Path::new(index), Path::new(repo), src.clone()) {
        Ok(a) => {
            eprintln!(
                "[{}] {} entities | impl-edges: {}",
                a.tool(),
                a.node_count(),
                a.impl_source()
            );
            (a, src)
        }
        Err(e) => {
            eprintln!("load error: {e}");
            std::process::exit(1);
        }
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let g = |i: usize| a.get(i).map(String::as_str);
    match g(1) {
        Some("entities") => {
            let (Some(idx), Some(repo)) = (g(2), g(3)) else { die(USAGE) };
            let (ad, _) = load(idx, repo);
            let mut v: Vec<_> = ad.entities().collect();
            v.sort_by(|x, y| x.1.cmp(y.1).then(x.2.cmp(y.2)));
            for (_, kind, name) in v.iter().take(60) {
                println!("{kind:14} {name}");
            }
            if v.len() > 60 {
                println!("... and {} more", v.len() - 60);
            }
        }
        Some("impact") => {
            let (Some(idx), Some(repo), Some(name)) = (g(2), g(3), g(4)) else { die(USAGE) };
            let (ad, src) = load(idx, repo);
            let Some(h) = ad.resolve(name, &src) else { die(&format!("not found: {name}")) };
            if let Some(l) = ad.locate(&h) {
                println!("entity: {name}  @ {}:{}-{}", l.file, l.start_line + 1, l.end_line + 1);
            }
            let blast = ad.neighbors(&h, &[EdgeKind::Calls, EdgeKind::Implements]);
            println!("blast radius — {} neighbors:", blast.len());
            for n in blast.iter().take(40) {
                println!("  - {}", ad.name_of(&n.id).unwrap_or(&n.id));
            }
            if blast.len() > 40 {
                println!("  ... and {} more", blast.len() - 40);
            }
        }
        Some("link") => {
            let (Some(idx), Some(repo), Some(spec), Some(name)) = (g(2), g(3), g(4), g(5)) else {
                die(USAGE)
            };
            let (ad, src) = load(idx, repo);
            let Some(h) = ad.resolve(name, &src) else { die(&format!("not found: {name}")) };
            let path = store_path(idx);
            let mut store = LinkStore::load(&path).unwrap_or_default();
            store.add_link(spec, "implements", &h, &ad);
            store.save(&path).expect("save link store");
            let fp = ad.fingerprint(&h).map(|f| f.0).unwrap_or_default();
            println!("linked {spec} --implements--> {name}  [{}]  (fp {fp})", h.id);
            println!("store: {}", path.display());
        }
        Some("drift") => {
            let (Some(idx), Some(repo)) = (g(2), g(3)) else { die(USAGE) };
            let (ad, src) = load(idx, repo);
            let store = LinkStore::load(&store_path(idx)).unwrap_or_default();
            let reports = store.check_drift(&src, &ad);
            if reports.is_empty() {
                println!("no links for source '{}'", src.0);
            }
            for r in &reports {
                let status = if r.missing {
                    "MISSING".to_string()
                } else if r.drifted {
                    format!("DRIFTED  {} -> {}", r.stored, r.current.clone().unwrap_or_default())
                } else {
                    "ok".to_string()
                };
                println!("[{status}]  {}  ->  {}", r.spec, ad.name_of(&r.target).unwrap_or(&r.target));
            }
        }
        Some("trace") => {
            let (Some(idx), Some(repo), Some(spec)) = (g(2), g(3), g(4)) else { die(USAGE) };
            let (ad, src) = load(idx, repo);
            let store = LinkStore::load(&store_path(idx)).unwrap_or_default();
            let links: Vec<_> = store.for_spec(spec).collect();
            println!("spec '{spec}' implements {} entit{}:", links.len(), if links.len() == 1 { "y" } else { "ies" });
            for l in links {
                let h = ad.resolve(ad.name_of(&l.target).unwrap_or(&l.target), &src);
                let loc = h
                    .as_ref()
                    .and_then(|h| ad.locate(h))
                    .map(|l| format!("{}:{}", l.file, l.start_line + 1))
                    .unwrap_or_else(|| "<unresolved>".into());
                let blast = h
                    .as_ref()
                    .map(|h| ad.neighbors(h, &[EdgeKind::Calls, EdgeKind::Implements]).len())
                    .unwrap_or(0);
                println!("  - {}  @ {loc}  (blast radius: {blast})", ad.name_of(&l.target).unwrap_or(&l.target));
            }
        }
        Some("graph") => cmd_graph(&a[2..]),
        _ => die(USAGE),
    }
}

/// `graph` — render a code project's entity graph to a self-contained SVG (and/or Graphviz DOT).
/// Source is either an explicit `<index.scip> <repo_root>` pair, or `--project <note.md>` which
/// binds the first repo source via hiker-projects (proving the external-projects path end-to-end).
fn cmd_graph(args: &[String]) {
    // Tiny flag parser: collect `--k v` / `--k` (flag) and leading positionals.
    let mut pos: Vec<&str> = Vec::new();
    let mut flags: std::collections::HashMap<&str, String> = std::collections::HashMap::new();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if let Some(key) = arg.strip_prefix("--") {
            let val = args.get(i + 1).filter(|v| !v.starts_with("--"));
            match val {
                Some(v) => {
                    flags.insert(key, v.clone());
                    i += 2;
                }
                None => {
                    flags.insert(key, String::new());
                    i += 1;
                }
            }
        } else {
            pos.push(arg);
            i += 1;
        }
    }

    // Resolve the adapter + source id, from a project note or an explicit index/repo pair.
    let (adapter, src) = if let Some(note) = flags.get("project") {
        let project = match Project::load(Path::new(note)) {
            Ok(p) => p,
            Err(e) => die(&format!("project load error: {e}")),
        };
        let Some(repo) = project.repo_sources().next() else {
            die("project note has no `kind: repo` source")
        };
        eprintln!(
            "[project {}] repo_id={} backend={:?} index={}",
            note,
            repo.repo_id,
            repo.backend,
            repo.index.display()
        );
        if let Some(stale) = repo.is_stale() {
            eprintln!("[index staleness] {}", if stale { "STALE (reindex)" } else { "current" });
        }
        // Bind the repo descriptor → SCIP adapter here (the consumer composes; hiker-projects stays
        // code-intel-free). Only the SCIP backend is implemented.
        if repo.backend != hiker_projects::Backend::Scip {
            die("only the scip backend is implemented (lsp is not yet supported)");
        }
        let src = SourceId(repo.repo_id.clone());
        match ScipAdapter::load(&repo.index, &repo.root, src.clone()) {
            Ok(ad) => {
                eprintln!("[{}] {} entities | impl-edges: {}", ad.tool(), ad.node_count(), ad.impl_source());
                (ad, src)
            }
            Err(e) => die(&format!("load index: {e}")),
        }
    } else {
        let (Some(idx), Some(repo)) = (pos.first().copied(), pos.get(1).copied()) else {
            die(USAGE)
        };
        load(idx, repo)
    };

    let graph = adapter.code_graph();
    if graph.nodes.is_empty() {
        die("empty graph — nothing to render");
    }

    // Scope: focus-node neighborhood (the legibility default for large graphs), else full graph
    // with a guard cap so we never silently emit a 2,900-node hairball.
    let depth: usize = flags.get("depth").and_then(|s| s.parse().ok()).unwrap_or(2);
    let max: usize = flags.get("max").and_then(|s| s.parse().ok()).unwrap_or(400);
    let (sub, title) = if let Some(name) = flags.get("focus") {
        let Some(h) = adapter.resolve(name, &src) else { die(&format!("focus not found: {name}")) };
        let Some(focus_idx) = graph.nodes.iter().position(|n| n.id == h.id) else {
            die(&format!("focus '{name}' not a graph node"))
        };
        let sub = render::SubGraph::neighborhood(&graph, focus_idx, depth, max);
        eprintln!("[scope] {}-hop neighborhood of '{name}' → {} nodes", depth, sub.len());
        (sub, format!("{}  ·  {name} (≤{depth} hops)", src.0))
    } else if graph.nodes.len() > max {
        // Whole-repo dump would be a hairball — keep the highest-degree nodes and SAY so.
        let sub = top_degree_subgraph(&graph, max);
        let kept = sub.len();
        eprintln!(
            "[scope] graph has {} nodes (> max {}); rendering the {} highest-degree nodes. \
             Use --focus <name> for a neighborhood view.",
            graph.nodes.len(),
            max,
            kept
        );
        (sub, format!("{}  ·  top {} hubs of {}", src.0, kept, graph.nodes.len()))
    } else {
        let sub = render::SubGraph::full(&graph);
        (sub, format!("{}  ·  {} entities", src.0, graph.nodes.len()))
    };

    // Outputs: default to graph.svg if neither --svg nor --dot given.
    let svg_out = flags.get("svg").cloned();
    let dot_out = flags.get("dot").cloned();
    let (svg_out, dot_out) = match (svg_out, dot_out) {
        (None, None) => (Some("graph.svg".to_string()), None),
        other => other,
    };
    if let Some(path) = svg_out.filter(|p| !p.is_empty()) {
        let svg = render::to_svg(&sub, &title);
        if let Err(e) = std::fs::write(&path, svg) {
            die(&format!("write svg {path}: {e}"));
        }
        println!("wrote SVG: {path}  ({} nodes, {} edges)", sub.len(), sub.local_edges.len());
    }
    if let Some(path) = dot_out.filter(|p| !p.is_empty()) {
        let dot = render::to_dot(&sub, &title);
        if let Err(e) = std::fs::write(&path, dot) {
            die(&format!("write dot {path}: {e}"));
        }
        println!("wrote DOT: {path}  (render with: dot -Tsvg {path} > out.svg)");
    }
}

/// Pick the `max` highest-degree nodes (by total in+out edges) as a fallback scope when the full
/// graph is too large and no focus was given. Induces the edges among them.
fn top_degree_subgraph(graph: &hiker_code::CodeGraph, max: usize) -> render::SubGraph<'_> {
    let mut degree = vec![0usize; graph.nodes.len()];
    for &(a, b, _) in &graph.edges {
        degree[a] += 1;
        degree[b] += 1;
    }
    let mut idx: Vec<usize> = (0..graph.nodes.len()).collect();
    idx.sort_by(|&a, &b| degree[b].cmp(&degree[a]).then(a.cmp(&b)));
    idx.truncate(max);
    render::SubGraph::from_subset(graph, idx)
}
