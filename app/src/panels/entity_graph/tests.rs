//! Unit tests for the unified entity graph (split out to keep `mod.rs` under the line cap).

use super::*;
use hiker_code::GraphNode;

/// A `component`/`container` target (trailing `/`) expands to every code id under that prefix; a
/// leaf target passes through verbatim; results are deduped + sorted. status: code-graph-spec-lighting
#[test]
fn resolve_governed_expands_component_prefixes() {
    let code_ids = [
        "repo mod activity/op_a().",
        "repo mod activity/op_b().",
        "repo mod buffer/save().",
    ];
    // A module prefix → all its members; a leaf → itself; a non-member leaf still passes through
    // (the caller checks membership against real nodes).
    let targets = vec![
        "repo mod activity/".to_string(),
        "repo mod buffer/save().".to_string(),
        "repo mod missing/leaf().".to_string(),
    ];
    let got = resolve_governed(&targets, &code_ids);
    assert_eq!(
        got,
        vec![
            "repo mod activity/op_a().",
            "repo mod activity/op_b().",
            "repo mod buffer/save().",
            "repo mod missing/leaf().",
        ]
    );
}

fn code_node(id: &str, kind: &str) -> GraphNode {
    code_node_p(id, kind, None)
}

fn code_node_p(id: &str, kind: &str, parent: Option<usize>) -> GraphNode {
    GraphNode {
        id: id.into(),
        name: id.into(),
        kind: kind.into(),
        file: format!("{id}.rs"),
        start_line: 0,
        lines: 1,
        parent,
    }
}

fn idx(g: &EntityGraph, id: &str) -> usize {
    g.nodes.iter().position(|n| n.id == id).unwrap_or_else(|| panic!("no node {id}"))
}

fn has_edge(g: &EntityGraph, from: &str, to: &str, kind: EntityEdge) -> bool {
    let (a, b) = (idx(g, from), idx(g, to));
    g.edges.contains(&(a, b, kind))
}

/// Drive the spec-body scan directly (no Store/Vault): start from a code graph, append spec
/// nodes for `specs`, wire `governs` against the code index, then scan `docs` for references +
/// spans. Mirrors what `build` does after IO.
fn assemble(
    code: &CodeGraph,
    specs: &[(&str, Option<&str>, &[&str])], // (slug, status, governed monikers)
    docs: &[&str],
) -> EntityGraph {
    let mut nodes: Vec<EntityNode> = code.nodes.iter().map(node_from_code).collect();
    let mut edges: Vec<(usize, usize, EntityEdge)> =
        code.edges.iter().map(|&(a, b, k)| (a, b, EntityEdge::from_code(k))).collect();
    let code_index: HashMap<&str, usize> =
        code.nodes.iter().enumerate().map(|(i, n)| (n.id.as_str(), i)).collect();
    let mut spec_index = HashMap::new();
    for (slug, status, _) in specs {
        spec_index.insert((*slug).to_string(), nodes.len());
        nodes.push(EntityNode {
            id: (*slug).into(),
            name: (*slug).into(),
            kind: SPEC_KIND.into(),
            file: String::new(),
            start_line: 0,
            lines: 0,
            status: status.map(str::to_string),
            parent: None,
        });
    }
    for (slug, _, governed) in specs {
        let from = spec_index[*slug];
        for m in *governed {
            if let Some(&to) = code_index.get(m) {
                edges.push((from, to, EntityEdge::Governs));
            }
        }
    }
    let bodies: HashMap<String, String> =
        docs.iter().enumerate().map(|(i, d)| (i.to_string(), (*d).to_string())).collect();
    scan_spec_bodies(&bodies, &spec_index, &mut nodes, &mut edges);
    EntityGraph { nodes, edges }
}

/// Code + spec merge: code nodes stay index-stable, spec nodes append, `governs` binds to an
/// EXISTING code node (a missing target draws no edge), `[[spec:]]` references attach to the
/// nearest anchor (self skipped), and each spec's `lines` is its `[slug]` span.
#[test]
fn build_merges_code_and_spec_layers() {
    let code = CodeGraph {
        nodes: vec![code_node("sym/a", "code:type"), code_node("sym/b", "code:method")],
        edges: vec![(0, 1, EdgeKind::Calls)],
    };
    let docs = [
        "[spec-a]\nbody line\nmore\n- see [[spec:spec-b]] and [[spec:spec-a]] (self)\n[spec-b]\ntail\n",
    ];
    let g = assemble(
        &code,
        &[("spec-a", Some("planned"), &["sym/a", "sym/gone"]), ("spec-b", None, &["sym/b"])],
        &docs,
    );
    // Code nodes keep index 0/1; the code edge survives folded.
    assert_eq!(idx(&g, "sym/a"), 0);
    assert!(has_edge(&g, "sym/a", "sym/b", EntityEdge::Calls));
    // Governs binds to existing code; the absent `sym/gone` target adds no edge/node.
    assert!(has_edge(&g, "spec-a", "sym/a", EntityEdge::Governs));
    assert!(has_edge(&g, "spec-b", "sym/b", EntityEdge::Governs));
    assert!(g.nodes.iter().all(|n| n.id != "sym/gone"), "missing target → no phantom node");
    // Reference edge from the body wikilink (attributed to `[spec-a]`); self skipped.
    assert!(has_edge(&g, "spec-a", "spec-b", EntityEdge::Reference));
    assert!(!has_edge(&g, "spec-a", "spec-a", EntityEdge::Reference), "self skipped");
    // Status + span captured: spec-a's `[spec-a]` section spans lines [0,4) = 4.
    assert_eq!(g.nodes[idx(&g, "spec-a")].status.as_deref(), Some("planned"));
    assert_eq!(g.nodes[idx(&g, "spec-a")].lines, 4, "anchor → next-anchor span");
    // The spec kind sorts into the present kinds.
    assert!(g.kinds_present().contains(&SPEC_KIND.to_string()));
}

/// A lens filters nodes + edges to its selected kinds while keeping the full index space; an
/// Hiding a member kind LIFTS its edges to the nearest visible ancestor (so connectivity
/// survives), and overview orphan-hiding drops degree-0 nodes.
#[test]
fn filter_for_lifts_edges_and_hides_orphans() {
    // T (type) contains M (method); M calls U (type). T itself has no direct edge.
    let code = CodeGraph {
        nodes: vec![
            code_node_p("T", "code:type", None),       // 0
            code_node_p("M", "code:method", Some(0)),  // 1 (contained by T)
            code_node_p("U", "code:type", None),       // 2
        ],
        edges: vec![(1, 2, EdgeKind::Calls)], // M → U
    };
    let g = assemble(&code, &[], &[]);

    // Hide methods, show disconnected: M's edge LIFTS to its parent T, so T → U appears even
    // though M is gone — connectivity is preserved, not orphaned.
    let mut lens = Lens::all(&g);
    lens.kinds.iter_mut().for_each(|(k, on)| *on = k != "code:method");
    lens.hide_orphans = false;
    let d = filter_for(&g, &lens, None, None, None, &[]);
    assert!(d.nodes.iter().all(|n| n.id != "M"), "method hidden");
    let t = d.nodes.iter().position(|n| n.id == "T").unwrap();
    let u = d.nodes.iter().position(|n| n.id == "U").unwrap();
    assert!(d.edges.contains(&(t, u, EntityEdge::Calls)), "M→U lifted to T→U");

    // All kinds, overview, hide_orphans (default): only M↔U are connected; T (degree 0) drops.
    let d2 = filter_for(&g, &Lens::all(&g), None, None, None, &[]);
    assert!(d2.nodes.iter().any(|n| n.id == "M") && d2.nodes.iter().any(|n| n.id == "U"));
    assert!(d2.nodes.iter().all(|n| n.id != "T"), "disconnected T dropped in overview");

    // The source draws the pre-filtered graph as-is (no further filtering).
    let src = EntityGraphSource::new(&d2, false, None, None);
    let positions = vec![egui::Vec2::ZERO; d2.nodes.len()];
    assert_eq!(src.nodes(&positions, &Style::flat()).len(), d2.nodes.len());
}

fn pkg_node(id: &str, name: &str, kind: &str, parent: Option<usize>) -> EntityNode {
    EntityNode {
        id: id.into(),
        name: name.into(),
        kind: kind.into(),
        file: String::new(),
        start_line: 0,
        lines: 0,
        status: None,
        parent,
    }
}

/// `synthesize_packages` adds one `code:package` per distinct package (≥2 present) and re-parents
/// each package-root module into it, leaving non-top-level nodes and indices/edges untouched. The
/// label is the indexer's raw package name (language-neutral). status: code-graph-package-tier
#[test]
fn synthesize_packages_adds_tier_and_reparents() {
    // Two packages (hiker-core, hiker-app); a moniker with no package slot is skipped.
    let mut nodes = vec![
        pkg_node("ra cargo hiker-core 0.0.0 buffer/", "buffer", "code:module", None),
        // already nested → untouched
        pkg_node("ra cargo hiker-core 0.0.0 buffer/save().", "save", "code:function", Some(0)),
        pkg_node("ra cargo hiker-app 0.0.0 main/", "main", "code:module", None),
        // no package slot → no package, parent stays None
        pkg_node("no-package", "x", "code:module", None),
    ];
    synthesize_packages(&mut nodes);
    // Two package nodes appended, existing indices preserved.
    assert_eq!(nodes.len(), 6);
    let core = nodes.iter().position(|n| n.id == "package:hiker-core").unwrap();
    let app = nodes.iter().position(|n| n.id == "package:hiker-app").unwrap();
    assert_eq!(nodes[core].kind, PACKAGE_KIND);
    assert_eq!(nodes[core].name, "hiker-core");
    assert_eq!(nodes[core].parent, None);
    // The package-root modules re-parent into their package; the nested fn is untouched.
    assert_eq!(nodes[0].parent, Some(core), "buffer module → hiker-core package");
    assert_eq!(nodes[2].parent, Some(app), "main module → hiker-app package");
    assert_eq!(nodes[1].parent, Some(0), "nested fn unchanged");
    assert_eq!(nodes[3].parent, None, "no-package module gets no package");
    // Package nodes carry no edges (none added here) — verified by their absence in any edge set.
}

/// The ≥2-package guard: a SINGLE-package SCIP (the common TS/Python/Go/Java case) synthesizes NO
/// package node and leaves parents unchanged, so its top modules stay the structural roots (depth 0)
/// — exactly as the graph rendered before this tier existed. status: code-graph-package-tier
#[test]
fn single_package_synthesizes_no_package_tier() {
    let code = CodeGraph {
        nodes: vec![
            code_node("scip py pyproj 1.0 mod_a/", "code:module"),
            code_node_p("scip py pyproj 1.0 mod_a/T#", "code:type", Some(0)),
            code_node("scip py pyproj 1.0 mod_b/", "code:module"),
        ],
        edges: vec![(0, 1, EdgeKind::TypeRef)],
    };
    let g = EntityGraph::from_code(&code);
    // No package node was synthesized (only one distinct package, "pyproj").
    assert!(g.nodes.iter().all(|n| n.kind != PACKAGE_KIND), "single package → no package tier");
    assert!(g.nodes.iter().all(|n| !n.id.starts_with("package:")), "no synthetic package id");
    // Top modules stay parentless → structural roots (depth 0 in the display).
    let mod_a = idx(&g, "scip py pyproj 1.0 mod_a/");
    let mod_b = idx(&g, "scip py pyproj 1.0 mod_b/");
    assert_eq!(g.nodes[mod_a].parent, None, "top module stays a root");
    assert_eq!(g.nodes[mod_b].parent, None, "top module stays a root");

    // In the display, those top modules are depth-0 roots → their LABELS always show (label_min_zoom
    // 0), so the overview is NOT blank/empty. (Node VISIBILITY is now the engine's spatial bundling,
    // not a per-node zoom threshold.)
    let mut lens = Lens::all(&g);
    lens.hide_orphans = false;
    let d = filter_for(&g, &lens, None, None, None, &[]);
    let positions = vec![egui::Vec2::ZERO; d.nodes.len()];
    let src = EntityGraphSource::new(&d, false, None, None);
    let descs = src.nodes(&positions, &Style::flat());
    let di = |id: &str| d.nodes.iter().position(|n| n.id == id).unwrap();
    let get = |id: &str| descs.iter().find(|x| x.index == di(id)).unwrap();
    assert_eq!(get("scip py pyproj 1.0 mod_a/").label_min_zoom, 0.0, "top module labels at overview");
    assert_eq!(get("scip py pyproj 1.0 mod_b/").label_min_zoom, 0.0, "top module labels at overview");
    // For a single-package graph the always-labelled skeleton spans the top modules AND their
    // immediate members (depth <= SKELETON_DEPTH), so the nested type labels too.
    assert_eq!(
        get("scip py pyproj 1.0 mod_a/T#").label_min_zoom,
        0.0,
        "nested type is within the label skeleton for a single-package graph"
    );
}

/// End-to-end: with package synthesis (≥2 packages) the source positions a package over its members'
/// centroid (a package carries no edges, so its own force position is unused) and labels the package +
/// module at the overview (depth-0/1 → label_min_zoom 0). Node visibility itself is the engine's
/// spatial bundling, tested in `hiker-graph-view`. status: code-graph-package-tier
#[test]
fn package_tier_centroids_and_labels() {
    // Two packages so the tier IS synthesized (the ≥2-package guard).
    let code = CodeGraph {
        nodes: vec![
            GraphNode {
                id: "ra cargo hiker-core 0.0.0 buffer/".into(),
                name: "buffer".into(),
                kind: "code:module".into(),
                file: "buffer.rs".into(),
                start_line: 0,
                lines: 1,
                parent: None,
            },
            GraphNode {
                id: "ra cargo hiker-core 0.0.0 buffer/T#".into(),
                name: "T".into(),
                kind: "code:type".into(),
                file: "buffer.rs".into(),
                start_line: 0,
                lines: 1,
                parent: Some(0),
            },
            GraphNode {
                id: "ra cargo hiker-app 0.0.0 main/".into(),
                name: "main".into(),
                kind: "code:module".into(),
                file: "main.rs".into(),
                start_line: 0,
                lines: 1,
                parent: None,
            },
        ],
        edges: vec![(0, 1, EdgeKind::TypeRef)],
    };
    let g = EntityGraph::from_code(&code);
    let core_pkg = g
        .nodes
        .iter()
        .position(|n| n.id == "package:hiker-core")
        .expect("package synthesized");
    assert_eq!(g.nodes[0].parent, Some(core_pkg), "module re-parented into package");

    let mut lens = Lens::all(&g);
    lens.hide_orphans = true; // overview: package must survive despite degree 0
    let d = filter_for(&g, &lens, None, None, None, &[]);
    let dpkg =
        d.nodes.iter().position(|n| n.id == "package:hiker-core").expect("package kept at overview");
    let dmod = d.nodes.iter().position(|n| n.id.ends_with("buffer/")).expect("module kept");
    assert_eq!(d.nodes[dmod].parent, Some(dpkg), "module's display parent is the package");

    // Position members away from origin; the package sits at their centroid.
    let mut positions = vec![egui::Vec2::ZERO; d.nodes.len()];
    for (i, p) in positions.iter_mut().enumerate() {
        if i != dpkg {
            *p = egui::vec2(10.0, 20.0);
        }
    }
    let src = EntityGraphSource::new(&d, false, None, None);
    let descs = src.nodes(&positions, &Style::flat());
    let pkg_desc = descs.iter().find(|x| x.index == dpkg).unwrap();
    assert_eq!(pkg_desc.label_min_zoom, 0.0, "package (depth-0 root) labels at overview");
    assert_eq!(pkg_desc.world_pos, egui::vec2(10.0, 20.0), "package over member centroid");
    // The package + module label at the overview; the leaf type (deeper than the skeleton) gates its
    // label on zoom-in. Node CULLING is now spatial (engine-side), not depth-stepped here.
    let mod_desc = descs.iter().find(|x| x.index == dmod).unwrap();
    assert_eq!(mod_desc.label_min_zoom, 0.0, "module is in the label skeleton");
    let type_desc = descs.iter().find(|x| x.index == di_kind(&d, "code:type")).unwrap();
    assert!(type_desc.label_min_zoom > 0.0, "the type (below the skeleton) labels on zoom-in");
}

fn di_kind(g: &EntityGraph, kind: &str) -> usize {
    g.nodes.iter().position(|n| n.kind == kind).unwrap()
}

fn spec_node(slug: &str, file: &str) -> EntityNode {
    EntityNode {
        id: slug.into(),
        name: slug.into(),
        kind: SPEC_KIND.into(),
        file: file.into(),
        start_line: 0,
        lines: 0,
        status: None,
        parent: None,
    }
}

/// `synthesize_specdocs` adds one `spec:document` per distinct spec `file` and re-parents each spec
/// into its document; the container's `name` is the doc basename without extension, its `id` is
/// `specdoc:<file>`, it has `parent = None`, and a spec with an EMPTY `file` is left parentless.
/// status: code-graph-spec-tier
#[test]
fn synthesize_specdocs_groups_specs_by_doc() {
    let mut nodes = vec![
        spec_node("canvas-zoom", "docs/canvas.md"),
        spec_node("canvas-pan", "docs/canvas.md"),
        spec_node("editor-caret", "docs/editor.md"),
        spec_node("orphan", ""), // empty file → no specdoc, stays parentless
    ];
    synthesize_specdocs(&mut nodes);
    // Two specdoc containers appended; existing indices preserved.
    assert_eq!(nodes.len(), 6);
    let canvas = nodes.iter().position(|n| n.id == "specdoc:docs/canvas.md").unwrap();
    let editor = nodes.iter().position(|n| n.id == "specdoc:docs/editor.md").unwrap();
    assert_eq!(nodes[canvas].kind, SPECDOC_KIND);
    assert_eq!(nodes[canvas].name, "canvas", "basename without extension");
    assert_eq!(nodes[editor].name, "editor");
    assert_eq!(nodes[canvas].parent, None, "specdoc is a structural root");
    // The two canvas specs re-parent into the canvas doc; the editor spec into the editor doc.
    assert_eq!(nodes[0].parent, Some(canvas), "canvas-zoom → canvas doc");
    assert_eq!(nodes[1].parent, Some(canvas), "canvas-pan → canvas doc");
    assert_eq!(nodes[2].parent, Some(editor), "editor-caret → editor doc");
    assert_eq!(nodes[3].parent, None, "empty-file spec gets no specdoc");
}

/// A spec-free node vec is a no-op for `synthesize_specdocs`. status: code-graph-spec-tier
#[test]
fn synthesize_specdocs_noop_without_specs() {
    let mut nodes = vec![pkg_node("m/", "m", "code:module", None)];
    synthesize_specdocs(&mut nodes);
    assert_eq!(nodes.len(), 1, "no spec → no specdoc tier");
}

/// End-to-end via the source: a specdoc is a depth-0 root (labels at the overview, `label_min_zoom
/// 0`), it sits over its specs' centroid (the centroid `world_pos` override, since it carries no
/// edges), its specs are depth 1 (no longer force-bumped), and a parentless spec keeps the small
/// fallback bump BELOW the skeleton. status: code-graph-spec-tier
#[test]
fn specdoc_tier_centroids_depths_and_labels() {
    let mut nodes = vec![
        spec_node("canvas-zoom", "docs/canvas.md"),
        spec_node("canvas-pan", "docs/canvas.md"),
        spec_node("orphan", ""), // parentless spec → fallback depth bump
    ];
    synthesize_specdocs(&mut nodes);
    // One reference edge so the specs aren't orphan-hidden (keep the test on the overview path).
    let g = EntityGraph { nodes, edges: vec![(0, 1, EntityEdge::Reference)] };
    let dpkg = idx(&g, "specdoc:docs/canvas.md");

    let mut lens = Lens::all(&g);
    lens.hide_orphans = false; // keep the parentless `orphan` spec for the depth check
    let d = filter_for(&g, &lens, None, None, None, &[]);
    let di = |id: &str| d.nodes.iter().position(|n| n.id == id).unwrap();
    let dspecdoc = di("specdoc:docs/canvas.md");
    assert_eq!(d.nodes[di("canvas-zoom")].parent, Some(dspecdoc), "spec's display parent is its doc");

    // Position the specs away from origin; the specdoc sits at their centroid.
    let mut positions = vec![egui::Vec2::ZERO; d.nodes.len()];
    for (i, p) in positions.iter_mut().enumerate() {
        if i != dspecdoc {
            *p = egui::vec2(6.0, 8.0);
        }
    }
    let src = EntityGraphSource::new(&d, false, None, None);
    let descs = src.nodes(&positions, &Style::flat());
    let get = |id: &str| descs.iter().find(|x| x.index == di(id)).unwrap();
    // The specdoc is a depth-0 root → labels at the overview, square shape, over the centroid.
    assert_eq!(get("specdoc:docs/canvas.md").label_min_zoom, 0.0, "specdoc labels at overview");
    assert!(matches!(get("specdoc:docs/canvas.md").shape, NodeShape::Square), "specdoc draws as a container square");
    assert_eq!(get("specdoc:docs/canvas.md").world_pos, egui::vec2(6.0, 8.0), "specdoc over its specs' centroid");
    // A spec inside the doc is now depth 1 (its specdoc parent) — within the label skeleton, like a
    // MODULE under a package (the old `SPEC_KIND` force-bump to a leaf tier is gone). Its label is a
    // candidate at the overview; the budget LOD bounds how many actually place.
    assert_eq!(
        get("canvas-zoom").label_min_zoom,
        0.0,
        "a doc's spec is depth-1 (skeleton), no longer force-bumped below the overview"
    );
    // A parentless spec (empty file → no specdoc) keeps the fallback bump BELOW the skeleton, so it
    // gates its label on zoom-in instead of flooding the overview tier.
    assert!(get("orphan").label_min_zoom > 0.0, "a parentless spec keeps its fallback depth bump");
    // Sanity: the dpkg index alias is the same node.
    assert_eq!(dpkg, idx(&g, "specdoc:docs/canvas.md"));
}

/// `primary_default` turns the specdoc container ON by default (so spec documents show in the
/// primary view alongside the crate tier). status: code-graph-spec-tier
#[test]
fn primary_default_shows_specdocs() {
    let mut nodes = vec![spec_node("s", "docs/d.md")];
    synthesize_specdocs(&mut nodes);
    let g = EntityGraph { nodes, edges: vec![] };
    let lens = Lens::primary_default(&g);
    assert!(lens.kind_on(SPECDOC_KIND), "specdoc kind on by default in the primary lens");
    assert!(lens.kind_on(SPEC_KIND), "specs still on by default");
}

/// `one_hop` keeps the center + direct neighbours + the edges among them.
#[test]
fn one_hop_slices_neighbourhood() {
    let code = CodeGraph { nodes: vec![code_node("sym/a", "code:type")], edges: vec![] };
    let g = assemble(
        &code,
        &[("spec-a", None, &["sym/a"]), ("spec-b", None, &[])],
        &["[spec-a]\n- [[spec:spec-b]]\n[spec-b]\n"],
    );
    let hop = g.one_hop("spec-a");
    assert!(hop.nodes.iter().any(|n| n.id == "spec-a"));
    assert!(hop.nodes.iter().any(|n| n.id == "spec-b"), "reference neighbour kept");
    assert!(hop.nodes.iter().any(|n| n.id == "sym/a"), "governs neighbour kept");
    assert_eq!(g.one_hop("no-such"), EntityGraph::default(), "unknown center → empty");
}

/// `filter_for` rewrites each kept node's `parent` to a DISPLAY-index ancestor, so the bundling
/// rollup can find a node's nearest module after the reindex. status: code-graph-bundling
#[test]
fn filter_for_rewrites_parent_to_display_indices() {
    // module Mod (0) contains type T (1) which contains method Me (2); T calls free type U (3).
    let code = CodeGraph {
        nodes: vec![
            code_node_p("Mod", "code:module", None),
            code_node_p("T", "code:type", Some(0)),
            code_node_p("Me", "code:method", Some(1)),
            code_node_p("U", "code:type", None),
        ],
        edges: vec![(1, 3, EdgeKind::Calls), (2, 1, EdgeKind::TypeRef)],
    };
    let g = assemble(&code, &[], &[]);
    let mut lens = Lens::all(&g);
    lens.hide_orphans = false;
    let d = filter_for(&g, &lens, None, None, None, &[]);
    let di = |id: &str| d.nodes.iter().position(|n| n.id == id).unwrap();
    // T's parent points at Mod's DISPLAY index; Me's at T's; Mod/U have no parent.
    assert_eq!(d.nodes[di("T")].parent, Some(di("Mod")));
    assert_eq!(d.nodes[di("Me")].parent, Some(di("T")));
    assert_eq!(d.nodes[di("Mod")].parent, None);
    assert_eq!(d.nodes[di("U")].parent, None);
}

/// The source bakes LABELS by structural depth (the kept label-LOD machinery): a root module +
/// depth-1 type label at the overview (`label_min_zoom == 0`), a deeper method gates its label on
/// zoom-in (`> 0`), and every node's label is just its PLAIN NAME — the engine appends the live
/// `· N` SPATIAL-cluster count per frame, so the source bakes no count. Node CULLING is the engine's
/// spatial bundling (tested in `hiker-graph-view`), not a per-node threshold here.
/// status: code-graph-bundling
#[test]
fn source_bakes_labels_by_depth() {
    // module Mod with a type A (depth 1) and a method Me nested INSIDE A (depth 2), plus a sibling
    // type B — so the structural depth ladder (root → type → method) drives the LABEL reveal order.
    let code = CodeGraph {
        nodes: vec![
            code_node_p("Mod", "code:module", None),
            code_node_p("A", "code:type", Some(0)),
            code_node_p("B", "code:type", Some(0)),
            code_node_p("Me", "code:method", Some(1)), // nested in A → deeper
        ],
        edges: vec![(1, 2, EdgeKind::Calls), (3, 1, EdgeKind::TypeRef)],
    };
    let g = assemble(&code, &[], &[]);
    let mut lens = Lens::all(&g);
    lens.hide_orphans = false;
    let d = filter_for(&g, &lens, None, None, None, &[]);
    let positions = vec![egui::Vec2::ZERO; d.nodes.len()];
    let di = |id: &str| d.nodes.iter().position(|n| n.id == id).unwrap();

    let src = EntityGraphSource::new(&d, false, None, None);
    let descs = src.nodes(&positions, &Style::flat());
    let get = |id: &str| descs.iter().find(|x| x.index == di(id)).unwrap();
    // module (depth 0) + its types (depth 1) label at the overview; the deeper method gates on zoom.
    assert_eq!(get("Mod").label_min_zoom, 0.0, "root module labels at overview");
    assert_eq!(get("Mod").label.as_deref(), Some("Mod"), "plain name — engine owns the live count");
    assert_eq!(get("A").label_min_zoom, 0.0, "depth-1 type is within the label skeleton");
    assert!(get("Me").label_min_zoom > 0.0, "a depth-2 method labels on zoom-in");
    // No node bakes a `·` count suffix — that's the engine's live job.
    assert!(descs.iter().all(|x| !x.label.as_deref().unwrap_or("").contains('·')), "no baked count");
}

/// Compact / minimap mode (`with_dot_radius`) draws fixed-radius dots, suppresses badges, and keeps
/// plain labels (the corner minimap renders on its own non-bundling path). status: spec-minimap-swap
#[test]
fn minimap_mode_is_fixed_dots_no_badges() {
    let code = CodeGraph {
        nodes: vec![
            code_node_p("Mod", "code:module", None),
            code_node_p("A", "code:type", Some(0)),
            code_node_p("B", "code:type", Some(0)),
            code_node_p("C", "code:type", Some(0)),
        ],
        edges: vec![(1, 2, EdgeKind::Calls), (3, 1, EdgeKind::Calls)],
    };
    let g = assemble(&code, &[], &[]);
    let mut lens = Lens::all(&g);
    lens.hide_orphans = false;
    let d = filter_for(&g, &lens, None, None, None, &[]);
    let positions = vec![egui::Vec2::ZERO; d.nodes.len()];
    let src = EntityGraphSource::new(&d, false, None, None).with_dot_radius(3.0);
    let descs = src.nodes(&positions, &Style::flat());
    assert!(descs.iter().all(|x| (x.radius - 3.0).abs() < 1e-6), "minimap: fixed dot radius");
    assert!(descs.iter().all(|x| x.badge.is_none() && x.bug_badge.is_none()), "minimap: no badges");
    assert!(
        descs.iter().all(|x| !x.label.as_deref().unwrap_or("").contains('·')),
        "minimap: no count suffix"
    );
}

/// `layout_edges` adds a containment spring `(parent, child)` for every node with a `parent`, each
/// repeated `CONTAINMENT_STRENGTH` times, and is a SUPERSET of the drawn `edges` (which carry no
/// containment). A source with no containment yields `layout_edges == edges`. status: code-graph-containment-layout
#[test]
fn layout_edges_adds_containment_springs() {
    // Mod contains A, B, C; A→B is a real call edge (the only DRAWN edge).
    let code = CodeGraph {
        nodes: vec![
            code_node_p("Mod", "code:module", None),
            code_node_p("A", "code:type", Some(0)),
            code_node_p("B", "code:type", Some(0)),
            code_node_p("C", "code:type", Some(0)),
        ],
        edges: vec![(1, 2, EdgeKind::Calls)],
    };
    let g = assemble(&code, &[], &[]);
    let mut lens = Lens::all(&g);
    lens.hide_orphans = false; // keep C (degree-0) so its containment spring is testable
    let d = filter_for(&g, &lens, None, None, None, &[]);
    let src = EntityGraphSource::new(&d, false, None, None);

    let draw = src.edges();
    let layout = src.layout_edges();

    // Drawn edges carry NO containment — only the one call edge.
    assert_eq!(draw.len(), 1, "draw set is just the call edge");

    // layout_edges is a superset of edges().
    for e in &draw {
        assert!(layout.contains(e), "layout_edges must contain every drawn edge {e:?}");
    }

    // Each of the 3 members (A,B,C) contributes one containment spring (parent=Mod), repeated
    // CONTAINMENT_STRENGTH times.
    let mi = idx(&d, "Mod");
    for child in ["A", "B", "C"] {
        let ci = idx(&d, child) as u32;
        let count = layout.iter().filter(|&&(p, c)| p == mi as u32 && c == ci).count();
        assert_eq!(
            count, CONTAINMENT_STRENGTH,
            "{child}'s containment spring repeated CONTAINMENT_STRENGTH times"
        );
    }
    assert_eq!(
        layout.len(),
        draw.len() + 3 * CONTAINMENT_STRENGTH,
        "layout = draw + 3 members × CONTAINMENT_STRENGTH containment springs"
    );
}

/// A Source WITHOUT a `layout_edges` override (the default) returns `edges()` verbatim — so the
/// canvas / vault / cluster graphs are unaffected by the code-graph containment change.
/// status: code-graph-containment-layout
#[test]
fn default_layout_edges_equals_edges() {
    struct Plain;
    impl Source for Plain {
        fn node_count(&self) -> usize {
            2
        }
        fn nodes(&self, _p: &[egui::Vec2], _s: &Style) -> Vec<NodeDescriptor> {
            Vec::new()
        }
        fn edges(&self) -> Vec<(u32, u32)> {
            vec![(0, 1)]
        }
        fn layout_tree(&self, _k: hiker_graph::LayoutKind) -> hiker_graph::LayoutTree {
            hiker_graph::LayoutTree::from_parents(&[None, None])
        }
        fn preview_for(&self, _i: usize) -> Option<(String, String)> {
            None
        }
    }
    let p = Plain;
    assert_eq!(p.layout_edges(), p.edges(), "default layout_edges == edges");
}
