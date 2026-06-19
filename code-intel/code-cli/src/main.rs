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
use hiker_lsp::LspAdapter;
use hiker_projects::Project;
use spec_engine::{DerivedNodeSource, EdgeKind, LinkStore, NodeHandle, Resolution, SourceId};

const USAGE: &str = "usage:\n\
  code-cli entities <index.scip> <repo_root>\n\
  code-cli impact   <index.scip> <repo_root> <name>\n\
  code-cli link     <index.scip> <repo_root> <spec> <name>\n\
  code-cli drift    <index.scip> <repo_root>\n\
  code-cli ack      <index.scip> <repo_root> <spec>|--all   (re-verified: move baselines to current)\n\
  code-cli trace    <index.scip> <repo_root> <spec>\n\
  code-cli churn    <index.scip> <repo_root> [--commits N]  (churn-vs-drift silence report)\n\
  code-cli graph    <index.scip> <repo_root> [--level objects|members|all] [--focus <name>] [--depth N] [--max N] [--svg FILE] [--dot FILE]\n\
  code-cli graph    --project <note.md>       [--level objects|members|all] [--focus <name>] [--depth N] [--max N] [--svg FILE] [--dot FILE]\n\
  code-cli lsp      <repo_root> <symbol> [probe]   (spawns rust-analyzer; lazy DerivedNodeSource)";

fn die(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(2);
}

/// SourceId = the index file stem, so distinct repos don't collide in a shared link store.
fn source_id(index: &str) -> SourceId {
    SourceId(Path::new(index).file_stem().and_then(|s| s.to_str()).unwrap_or("src").to_string())
}

/// The drift baseline lives in the REPO (durable, committable) — never beside the index, which is
/// disposable and often in /tmp. Regenerating the index must not reset drift.
fn store_path(repo: &str) -> PathBuf {
    Path::new(repo).join("links.json")
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
            if let Some(w) = a.grammar_gap_warning() {
                eprintln!("{w}");
            }
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
            // `--shorts`: dump EVERY entity as `kind\tshort-descriptor-path` (the `[[code:…]]`
            // body format) — for tooling/scripts that need to validate or author doc links.
            if g(4) == Some("--shorts") {
                let mut v: Vec<_> =
                    ad.entities().map(|(id, kind, _)| (kind, hiker_code::short_sym(id))).collect();
                v.sort();
                for (kind, short) in v {
                    println!("{kind}\t{short}");
                }
                return;
            }
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
            let path = store_path(repo);
            let mut store = match LinkStore::load(&path) {
                Ok(s) => s,
                Err(e) => die(&format!("load link store {}: {e}", path.display())),
            };
            // Optional 6th arg: C4 resolution — clamped by the relation floor (implements ⇒ Code).
            let asked = g(6).map(Resolution::parse);
            let res = Resolution::for_relation("implements", asked);
            if asked.is_some_and(|a| a != res) {
                eprintln!("[floor] implements is a body-level claim — resolution clamped to {res:?}");
            }
            match store.add_link(spec, "implements", &h, res, &ad) {
                spec_engine::AddOutcome::Added => {
                    store.save(&path).expect("save link store");
                    let fp = ad.fingerprint_at(&h, res).map(|f| f.0).unwrap_or_default();
                    println!("linked {spec} --implements--> {name}  [{}]  (fp {fp})", h.id);
                    println!("store: {}", path.display());
                }
                spec_engine::AddOutcome::Existing => {
                    println!("already linked: {spec} --implements--> {name}  (baseline kept; use `ack` to re-baseline)");
                }
                spec_engine::AddOutcome::Rescoped => {
                    store.save(&path).expect("save link store");
                    println!("rescoped: {spec} --implements--> {name} re-pinned at {res:?} (baseline recaptured)");
                }
                spec_engine::AddOutcome::NoFingerprint => {
                    die(&format!("refusing to link {spec} -> {name}: target won't fingerprint"))
                }
            }
        }
        Some("drift") => {
            let (Some(idx), Some(repo)) = (g(2), g(3)) else { die(USAGE) };
            let (ad, src) = load(idx, repo);
            let store = match LinkStore::load(&store_path(repo)) {
                Ok(s) => s,
                Err(e) => die(&format!("load link store: {e}")),
            };
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
        Some("ack") => {
            // "I re-verified this spec" — the ONLY operation that moves stored baselines.
            let (Some(idx), Some(repo), Some(spec)) = (g(2), g(3), g(4)) else { die(USAGE) };
            let (ad, src) = load(idx, repo);
            let path = store_path(repo);
            let mut store = match LinkStore::load(&path) {
                Ok(s) => s,
                Err(e) => die(&format!("load link store: {e}")),
            };
            let filter = (spec != "--all").then_some(spec);
            let updated = store.rebaseline(filter, &src, &ad);
            store.save(&path).expect("save link store");
            match filter {
                Some(s) => println!("acked '{s}': {updated} baseline(s) moved to current"),
                None => println!("acked ALL specs: {updated} baseline(s) moved to current"),
            }
        }
        Some("trace") => {
            let (Some(idx), Some(repo), Some(spec)) = (g(2), g(3), g(4)) else { die(USAGE) };
            let (ad, src) = load(idx, repo);
            let store = match LinkStore::load(&store_path(repo)) {
                Ok(s) => s,
                Err(e) => die(&format!("load link store: {e}")),
            };
            // The stored target IS the moniker the link points at — locate it directly. Round-
            // tripping through the display name (resolve) can land on a name-alike and make the
            // trace lie about which symbol the spec governs.
            let mut by_rel: std::collections::BTreeMap<&str, Vec<&spec_engine::Link>> =
                Default::default();
            for l in store.for_spec(spec) {
                by_rel.entry(l.relation.as_str()).or_default().push(l);
            }
            let total: usize = by_rel.values().map(Vec::len).sum();
            println!("spec '{spec}' — {total} link(s):");
            for (rel, ls) in &by_rel {
                println!("  {rel} ({}):", ls.len());
                for l in ls {
                    let h = NodeHandle { source: src.clone(), id: l.target.clone() };
                    let loc = ad
                        .locate(&h)
                        .map(|l| format!("{}:{}", l.file, l.start_line + 1))
                        .unwrap_or_else(|| "<missing from index>".into());
                    let blast = ad.neighbors(&h, &[EdgeKind::Calls, EdgeKind::Implements]).len();
                    let name = ad
                        .name_of(&l.target)
                        .map(str::to_string)
                        .unwrap_or_else(|| hiker_code::short_sym(&l.target));
                    println!(
                        "    - {name}  @ {loc}  [{:?}]  (blast radius: {blast})",
                        l.resolution
                    );
                }
            }
        }
        Some("churn") => {
            let (Some(idx), Some(repo)) = (g(2), g(3)) else { die(USAGE) };
            let commits = match (g(4), g(5)) {
                (Some("--commits"), Some(n)) => n
                    .parse()
                    .unwrap_or_else(|_| die(&format!("--commits wants a number, got '{n}'"))),
                (None, _) => 50,
                _ => die(USAGE),
            };
            let (ad, src) = load(idx, repo);
            let store = match LinkStore::load(&store_path(repo)) {
                Ok(s) => s,
                Err(e) => die(&format!("load link store: {e}")),
            };
            let window = match hiker_code::churn_window(Path::new(repo), commits) {
                Ok(w) => w,
                Err(e) => die(&format!("churn window: {e}")),
            };
            print_churn(&hiker_code::churn_report(&ad.code_graph(), &store, &src, &ad, &window));
        }
        Some("graph") => cmd_graph(&a[2..]),
        Some("lsp") => {
            let (Some(repo), Some(symbol)) = (g(2), g(3)) else { die(USAGE) };
            cmd_lsp(repo, symbol, g(4));
        }
        _ => die(USAGE),
    }
}

/// `churn` — render the churn-vs-drift silence report (`code-cli-churn-vs-drift`): specs sorted
/// by how many window commits touched their targets, with drift expected (churned links) vs
/// observed (links reading DRIFTED/MISSING now) and the altitude explaining each silence; then
/// the ungoverned in-index files by churn — the silence proper.
fn print_churn(r: &hiker_code::ChurnReport) {
    const ROWS: usize = 25;
    println!("== churn vs drift — last {} commits ==", r.commits);
    println!("\n-- specs whose targets churned (drift expected vs observed) --");
    println!("  {:>7} {:>7} {:>8}  {:18}  spec", "commits", "expect", "observe", "verdict");
    let churned: Vec<_> = r.specs.iter().filter(|s| s.commits > 0).collect();
    for s in churned.iter().take(ROWS) {
        let verdict = match (s.blind(), s.altitude) {
            (true, Some(alt)) => format!("BLIND({alt:?})"),
            _ => "watched".to_string(),
        };
        println!("  {:>7} {:>7} {:>8}  {verdict:18}  {}", s.commits, s.expected, s.observed, s.spec);
    }
    if churned.len() > ROWS {
        println!("  ... and {} more churned specs", churned.len() - ROWS);
    }
    let blind = churned.iter().filter(|s| s.blind()).count();
    println!(
        "({} specs churned, {} of them blind; {} quiet in this window)",
        churned.len(),
        blind,
        r.specs.len() - churned.len()
    );
    println!("\n-- ungoverned files with churn (high churn, no spec: the silence) --");
    for f in r.ungoverned.iter().take(ROWS) {
        println!("  {:>4} commit(s)  {}", f.commits, f.file);
    }
    if r.ungoverned.len() > ROWS {
        println!("  ... and {} more files", r.ungoverned.len() - ROWS);
    }
    println!("\n({} changed paths outside the index ignored)", r.unindexed);
}

/// `lsp` — spawn rust-analyzer on `repo_root`, resolve `symbol` through the lazy `LspAdapter`
/// (a live `DerivedNodeSource`), and print its location + content snippet + call-hierarchy
/// neighbors (each resolved via `locate`). The same generic resolve→neighbors path as the SCIP
/// commands, but driven live off the LSP server instead of a `.scip` index. First run is SLOW: RA
/// must `cargo metadata` + build proc-macros + index before answering.
fn cmd_lsp(repo: &str, symbol: &str, probe: Option<&str>) {
    let program = std::env::var("HOME")
        .map(|h| PathBuf::from(h).join(".local/bin/rust-analyzer"))
        .ok()
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("rust-analyzer"));
    let probe = probe.unwrap_or(symbol);
    let src = SourceId("lsp".to_string());
    eprintln!("[lsp] spawning rust-analyzer on {repo} (probe {probe:?}; first run is slow)...");
    let adapter = match LspAdapter::spawn(&program, Path::new(repo), probe, src.clone()) {
        Ok(a) => a,
        Err(e) => die(&format!("rust-analyzer not ready: {e}")),
    };
    eprintln!("[lsp] ready.");
    let Some(h) = adapter.resolve(symbol, &src) else { die(&format!("not found: {symbol}")) };
    if let Some(l) = adapter.locate(&h) {
        println!("entity: {symbol}  @ {}:{}-{}", l.file, l.start_line + 1, l.end_line + 1);
    }
    if let Some(snippet) = adapter.content(&h) {
        for line in snippet.lines().take(3) {
            println!("  | {line}");
        }
    }
    let neighbors = adapter.neighbors(&h, &[EdgeKind::Calls]);
    println!("call neighbors — {} found:", neighbors.len());
    for n in &neighbors {
        print_lsp_neighbor(&adapter, n);
    }
}

fn print_lsp_neighbor(adapter: &LspAdapter, n: &NodeHandle) {
    match adapter.locate(n) {
        Some(l) => println!("  - {}:{}", l.file, l.start_line + 1),
        None => println!("  - <unresolved> {}", n.id),
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
        if repo.backend != hiker_projects::repo::Backend::Scip {
            die("only the scip backend is implemented (lsp is not yet supported)");
        }
        let src = SourceId(repo.repo_id.clone());
        match ScipAdapter::load(&repo.index, &repo.root, src.clone()) {
            Ok(ad) => {
                eprintln!("[{}] {} entities | impl-edges: {}", ad.tool(), ad.node_count(), ad.impl_source());
                if let Some(w) = ad.grammar_gap_warning() {
                    eprintln!("{w}");
                }
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

    let full = adapter.code_graph();
    if full.nodes.is_empty() {
        die("empty graph — nothing to render");
    }

    // Level of detail (`--level objects|members|all`, default all). Collapse hidden members up to
    // their containing object, aggregating edges — the structural skeleton instead of the full mess.
    let level = flags.get("level").map(String::as_str).unwrap_or("all");
    let graph = match level {
        "all" => full,
        "objects" => collapse_to(&full, |n| n.is_object()),
        "members" => collapse_to(&full, |n| {
            n.is_object() || matches!(n.kind.as_str(), "code:function" | "code:method")
        }),
        other => die(&format!("unknown --level '{other}' (objects|members|all)")),
    };
    if level != "all" {
        eprintln!("[level {level}] collapsed to {} nodes ({} edges)", graph.nodes.len(), graph.edges.len());
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

/// Collapse `g` to the nodes matching `keep` (lifting hidden members' edges up to their object via
/// `hiker_code::collapse`) and materialize the result back into a `CodeGraph` for scoping/rendering.
fn collapse_to(
    g: &hiker_code::CodeGraph,
    keep: impl Fn(&hiker_code::GraphNode) -> bool,
) -> hiker_code::CodeGraph {
    let c = hiker_code::collapse(g, |i| keep(&g.nodes[i]));
    let nodes = c
        .nodes
        .iter()
        .map(|&i| {
            let mut n = g.nodes[i].clone();
            n.parent = None;
            n
        })
        .collect();
    hiker_code::CodeGraph { nodes, edges: c.edges }
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
