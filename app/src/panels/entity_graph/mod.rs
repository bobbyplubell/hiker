//! The **unified entity graph** (`entity-graph-source`): one node-link graph holding *all*
//! entities — every code symbol the SCIP adapter knows AND every spec slug — with all their
//! edges (code→code calls/impls, spec→code `governs`, spec→spec `references`). The code-graph
//! view renders it through the shared `hiker_graph_view` engine, and the user picks what to draw
//! through a [`Lens`] (a per-kind / per-edge-kind selection). There is no separate spec graph and
//! no fill "overlay": a spec is a real node you can select to see its edges, governance drift is a
//! direct edge color, and "changed vs HEAD" is a direct node ring — never a whole-graph recolor.
//!
//! Built by merging the adapter's [`CodeGraph`] (the code half) with the spec layer derived from a
//! warm [`Governance`] rollup, the store's spec-anchor index, and the spec docs' bodies:
//!
//! - **code nodes** — every `CodeGraph` node, index-stable (node `i` stays index `i`).
//! - **spec nodes** — the union of `Governance::specs()` and the `spec_anchors` index, kind
//!   [`SPEC_KIND`], carrying the spec's `status::` and the line-length of its `[slug]` section.
//! - **`Governs` edges** — spec → the EXISTING code node for each `Governance::targets_of` moniker
//!   (a target absent from this index simply draws no edge — one universe, no phantom code nodes).
//! - **`Reference` edges** — spec → spec from `[[spec:slug]]` body wikilinks, attributed to the
//!   nearest-preceding `[slug]` anchor (the same line-walk `governance::doc_statuses` uses).
//!
//! Pure data + the engine [`Source`] adapter; the git-change ring data comes from
//! [`code_governance::Changes`](crate::panels::code_governance::Changes).

use std::collections::HashMap;

use eframe::egui;

use hiker_code::governance::{slug_in_line, Governance};
use hiker_code::{CodeGraph, GovState};
use hiker_core::store::Store;
use hiker_core::vault::Vault;
use hiker_core::wikilink;
use hiker_graph::{LayoutKind, LayoutTree};
use hiker_graph_view::graph_view::source::{NodeDescriptor, NodeShape, Source};
use hiker_graph_view::graph_view::styling::Style;
use hiker_theme as theme;
use spec_engine::EdgeKind;

use crate::panels::code_governance::{gov_color, Changes};

/// The node `kind` string for a spec slug — a sibling of the SCIP `code:*` kinds, so it flows
/// through the same auto-populated kind filter / coloring as every other entity.
pub(crate) const SPEC_KIND: &str = "spec";

/// The node `kind` string for a synthetic SPEC-DOCUMENT container — the spec-side mirror of
/// [`PACKAGE_KIND`]: it groups every spec slug DEFINED IN one note (a spec's `file`) under one
/// container, so the overview rolls spec slugs up to their owning document the way a package rolls
/// up its modules. Specdoc nodes are SYNTHESIZED (one per distinct spec `file`) so the zoomed-out
/// overview reads `canvas` / `editor` / `code` doc names rather than a flat scatter of slugs.
/// status: code-graph-spec-tier
pub(crate) const SPECDOC_KIND: &str = "spec:document";

/// How many times each CONTAINMENT spring `(parent, child)` is added to the FORCE-layout edge set
/// (see [`EntityGraphSource::layout_edges`]). The ForceAtlas2 worker sums duplicate springs, so a
/// value of `N` makes a containment pull `N`× as hard as one cross-module call edge — biasing a
/// module's members to cluster around the module (and modules around their package) so that, on
/// zoom-in, an unbundling container reveals its members RIGHT THERE rather than scattered off-screen
/// by their cross-module calls. Kept modest: too high collapses a cluster toward its container point
/// (repulsion stops separating members); `3` settles members tightly around the container while the
/// cluster stays readable. Tuned via the `bundle-open` graph-harness scenario. status: code-graph-containment-layout
const CONTAINMENT_STRENGTH: usize = 3;

/// Node-radius gain for the default importance sizing: `radius = 4 + IMPORTANCE_RADIUS_K · importance`
/// (importance ∈ 0..1). Sets how much bigger a hub/crate (importance ≈ 1) reads than a leaf
/// (importance ≈ 0); tuned so the structurally-significant nodes are large enough to clear the
/// on-screen label gate at the overview while leaves stay small dots. status: graph-label-dim
const IMPORTANCE_RADIUS_K: f32 = 18.0;

/// A typed edge in the unified graph. The first four mirror the SCIP `EdgeKind`; the last two are
/// the spec layer. Kept app-local (rather than extending `spec_engine::EdgeKind`, which is used
/// broadly) so the spec semantics stay in the consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum EntityEdge {
    Calls,
    TypeRef,
    Imports,
    Implements,
    /// spec → code: the `implements`/`touches` baseline (`Governance::targets_of`).
    Governs,
    /// spec → spec: a `[[spec:slug]]` body wikilink.
    Reference,
}

impl EntityEdge {
    /// Fold a SCIP code edge kind into the unified edge kind.
    const fn from_code(kind: EdgeKind) -> Self {
        match kind {
            EdgeKind::Calls => Self::Calls,
            EdgeKind::TypeRef => Self::TypeRef,
            EdgeKind::Imports => Self::Imports,
            EdgeKind::Implements => Self::Implements,
            EdgeKind::Link => Self::Reference,
        }
    }
}

/// One node of the unified graph: a code symbol or a spec slug. Flat (no containment parent — the
/// view no longer collapses; layout is force-directed over the whole universe).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EntityNode {
    /// Stable identity: the SCIP moniker (code) or the spec slug (spec).
    pub(crate) id: String,
    pub(crate) name: String,
    /// `code:*` for a code symbol, [`SPEC_KIND`] for a spec.
    pub(crate) kind: String,
    /// Code: the source file (relative to the repo root). Spec: its defining note path.
    pub(crate) file: String,
    pub(crate) start_line: u32,
    /// Code: the SCIP enclosing-range body length. Spec: the line span of its `[slug]` section
    /// (anchor line → next anchor / EOF) — "how much spec text defines it". Drives "size by LOC".
    pub(crate) lines: u32,
    /// Spec nodes only: the `status::` value when present.
    pub(crate) status: Option<String>,
    /// The CONTAINING node's index (a type/module for a method/field, the module for a free fn) —
    /// from the SCIP moniker nesting. `None` for top-level code + all spec nodes. Drives the
    /// filter's edge-LIFTING: a hidden member's edges lift to its nearest visible ancestor, so
    /// hiding a kind keeps the higher-level connectivity instead of orphaning it.
    pub(crate) parent: Option<usize>,
}

impl EntityNode {
    fn is_spec(&self) -> bool {
        self.kind == SPEC_KIND
    }
}

/// The unified entity graph: nodes plus index-pair typed edges. Pure data — unit-testable, no
/// engine state. [`EntityGraphSource`] renders it through the engine.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct EntityGraph {
    pub(crate) nodes: Vec<EntityNode>,
    /// `(from_index, to_index, kind)` into `nodes`.
    pub(crate) edges: Vec<(usize, usize, EntityEdge)>,
}

impl EntityGraph {
    /// Build the unified graph: the adapter's `code` graph plus the spec layer derived from
    /// `governance` (when warm), the store's spec-anchor index, and the spec docs' bodies. With
    /// `governance` `None` (no `links.json`), only the spec-anchor nodes + `[[spec:]]` reference
    /// edges are added — no `Governs` edges (those need the folded baseline a `Governance` carries).
    pub(crate) fn build(
        code: &CodeGraph,
        governance: Option<&Governance>,
        store: &Store,
        vault: &Vault,
    ) -> Self {
        // Code half: every code node keeps its original index, every code edge folds into the
        // unified edge kind.
        let mut nodes: Vec<EntityNode> = code.nodes.iter().map(node_from_code).collect();
        let mut edges: Vec<(usize, usize, EntityEdge)> =
            code.edges.iter().map(|&(a, b, k)| (a, b, EntityEdge::from_code(k))).collect();
        // moniker → code-node index, for binding `Governs` edges onto existing code nodes.
        let code_index: HashMap<&str, usize> =
            code.nodes.iter().enumerate().map(|(i, n)| (n.id.as_str(), i)).collect();

        // Spec half: union of governance's lightable specs and every `[slug]` anchor, deduped +
        // sorted for a stable order. `anchor_bodies` reads each defining note once for the
        // `[[spec:]]` scan.
        let (anchor_paths, bodies) = anchor_bodies(store, vault);
        let mut spec_slugs: Vec<String> = governance
            .into_iter()
            .flat_map(Governance::specs)
            .cloned()
            .chain(anchor_paths.keys().cloned())
            .collect();
        spec_slugs.sort();
        spec_slugs.dedup();

        // Spec nodes (index recorded for edge wiring); the first defining-note path is the node's
        // `file` so its detail/preview can point at the doc.
        let mut spec_index: HashMap<String, usize> = HashMap::new();
        for slug in &spec_slugs {
            let file = anchor_paths.get(slug).and_then(|p| p.first()).cloned().unwrap_or_default();
            spec_index.insert(slug.clone(), nodes.len());
            nodes.push(EntityNode {
                id: slug.clone(),
                name: slug.clone(),
                kind: SPEC_KIND.to_string(),
                file,
                start_line: 0,
                lines: 0, // filled by the anchor-span walk below
                status: governance.and_then(|g| g.status_of(slug)).map(str::to_string),
                parent: None,
            });
        }

        // `Governs` edges: spec → the code node(s) for each governed moniker. A leaf symbol binds to
        // its exact node; a `component`/`container` moniker (a module namespace ending in `/`, which
        // SCIP exposes as a synthetic prefix with no node of its own) expands to EVERY code node
        // under that prefix — so a module-grain spec governs the module's members instead of binding
        // to nothing. status: code-graph-spec-lighting
        if let Some(gov) = governance {
            let code_ids: Vec<&str> = code.nodes.iter().map(|n| n.id.as_str()).collect();
            for slug in &spec_slugs {
                let from = spec_index[slug];
                edges.extend(
                    resolve_governed(gov.targets_of(slug), &code_ids)
                        .into_iter()
                        .filter_map(|id| code_index.get(id).copied())
                        .map(|to| (from, to, EntityEdge::Governs)),
                );
            }
        }

        // `Reference` edges + each spec node's `lines` span, from the defining-note bodies.
        scan_spec_bodies(&bodies, &spec_index, &mut nodes, &mut edges);

        // Synthesize the PACKAGE tier (only when ≥2 distinct packages): one `code:package` node per
        // distinct package, with every current top-level code module re-parented into it (so the
        // overview rolls up to package names). Done last — it only APPENDS package nodes + rewrites
        // `parent` on existing package-root modules, so every code/spec index recorded above stays
        // valid. status: code-graph-package-tier
        synthesize_packages(&mut nodes);

        // Synthesize the SPEC-DOCUMENT tier: one `spec:document` container per distinct spec `file`,
        // with every spec node re-parented into its document (so the overview rolls spec slugs up to
        // their owning note). Like packages, done last — it only APPENDS specdoc nodes + rewrites
        // `parent` on existing spec nodes, so every index recorded above stays valid. Specs only
        // exist in this `build` path, but the helper is robust to a spec-free graph (a no-op).
        // status: code-graph-spec-tier
        synthesize_specdocs(&mut nodes);

        Self { nodes, edges }
    }

    /// The code-only universe (no spec layer) — the fast build at tab open / the store-lock-failed
    /// fallback. Spec nodes/edges are added later by a full [`build`](EntityGraph::build).
    pub(crate) fn from_code(code: &CodeGraph) -> Self {
        let mut nodes: Vec<EntityNode> = code.nodes.iter().map(node_from_code).collect();
        let edges = code.edges.iter().map(|&(a, b, k)| (a, b, EntityEdge::from_code(k))).collect();
        synthesize_packages(&mut nodes);
        Self { nodes, edges }
    }

    /// The code-node ids `spec` governs — its lit footprint. Leaf targets pass through; a
    /// `component`/`container` prefix (trailing `/`) expands to every code node under it (the same
    /// expansion the `Governs` edges use), so a module-grain spec lights the module's members rather
    /// than nothing. Deduped + sorted; borrows `self` + `gov`. status: code-graph-spec-lighting
    pub(crate) fn governed_ids<'a>(&'a self, gov: &'a Governance, spec: &str) -> Vec<&'a str> {
        let code_ids: Vec<&str> =
            self.nodes.iter().filter(|n| n.kind != SPEC_KIND).map(|n| n.id.as_str()).collect();
        resolve_governed(gov.targets_of(spec), &code_ids)
    }

    /// The deduped code-node ids governed by ANY of `specs` — the code-id list is built once (vs
    /// per-spec), so a multi-spec hover (a whole section/doc) stays cheap. status: code-graph-spec-lighting
    pub(crate) fn governed_ids_for<'a>(
        &'a self,
        gov: &'a Governance,
        specs: &[String],
    ) -> Vec<&'a str> {
        let code_ids: Vec<&str> =
            self.nodes.iter().filter(|n| n.kind != SPEC_KIND).map(|n| n.id.as_str()).collect();
        let mut ids: Vec<&str> = Vec::new();
        for spec in specs {
            ids.extend(resolve_governed(gov.targets_of(spec), &code_ids));
        }
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    /// The 1-hop slice around `node_id`: the node, its direct neighbours, and the edges among that
    /// set. The unit the link preview renders. An unknown id yields an empty graph.
    #[must_use]
    pub(crate) fn one_hop(&self, node_id: &str) -> EntityGraph {
        let Some(center) = self.nodes.iter().position(|n| n.id == node_id) else {
            return EntityGraph::default();
        };
        let mut keep: Vec<usize> = vec![center];
        for &(a, b, _) in &self.edges {
            if a == center && !keep.contains(&b) {
                keep.push(b);
            } else if b == center && !keep.contains(&a) {
                keep.push(a);
            }
        }
        let remap: HashMap<usize, usize> =
            keep.iter().enumerate().map(|(new, &old)| (old, new)).collect();
        let nodes = keep.iter().map(|&old| self.nodes[old].clone()).collect();
        let edges = self
            .edges
            .iter()
            .filter_map(|&(a, b, kind)| Some((*remap.get(&a)?, *remap.get(&b)?, kind)))
            .collect();
        EntityGraph { nodes, edges }
    }

    /// The distinct node kinds present, sorted — the auto-populated source of a lens's kind
    /// toggles (spec sorts in alongside the `code:*` kinds).
    pub(crate) fn kinds_present(&self) -> Vec<String> {
        let mut kinds: Vec<String> = self.nodes.iter().map(|n| n.kind.clone()).collect();
        kinds.sort();
        kinds.dedup();
        kinds
    }

    /// Per-node total degree (in + out). Graph-invariant — the host caches it so the per-frame
    /// source isn't an O(E) re-scan.
    pub(crate) fn degrees(&self) -> Vec<u32> {
        let mut degree = vec![0u32; self.nodes.len()];
        for &(a, b, _) in &self.edges {
            degree[a] += 1;
            degree[b] += 1;
        }
        degree
    }

    /// The largest node `lines` (≥1), the normaliser for √-scaled LOC sizing. Graph-invariant.
    pub(crate) fn max_lines(&self) -> f32 {
        self.nodes.iter().map(|n| n.lines).max().unwrap_or(1).max(1) as f32
    }
}

/// Map a code [`GraphNode`](hiker_code::GraphNode) into an [`EntityNode`].
fn node_from_code(n: &hiker_code::GraphNode) -> EntityNode {
    EntityNode {
        id: n.id.clone(),
        name: n.name.clone(),
        kind: n.kind.clone(),
        file: n.file.clone(),
        start_line: n.start_line,
        lines: n.lines,
        status: None,
        // Code containment is preserved 1:1 — code nodes keep their original indices in the
        // unified graph (they're appended first), so `GraphNode.parent` stays valid.
        parent: n.parent,
    }
}

/// The node `kind` string for a synthetic PACKAGE container — the top tier of the code hierarchy,
/// above `code:module`. "Package" is SCIP's own language-neutral term for the moniker's package
/// field (Rust crate, npm package, Python package, Go module path, Java artifact). Package nodes are
/// SYNTHESIZED (the SCIP package-root symbol isn't reliably present) so the zoomed-out overview rolls
/// up to package names — but only when there are ≥2 distinct packages (see [`synthesize_packages`]).
/// status: code-graph-package-tier
pub(crate) const PACKAGE_KIND: &str = "code:package";

/// The package name a code moniker belongs to: field index 2 of the
/// `<scheme> <manager> <package> <version> <descriptors…>` SCIP moniker — language-neutral (Rust
/// crate, npm/Python package, Go module path, Java artifact). Mirrors
/// `hiker_code::scip_adapter::crate_qualified_sym`'s extraction (incl. its empty / `"."` guard).
/// `None` for a moniker with no usable package slot. status: code-graph-package-tier
fn package_of(moniker: &str) -> Option<&str> {
    let pkg = moniker.splitn(5, ' ').nth(2)?;
    if pkg.is_empty() || pkg == "." {
        return None;
    }
    Some(pkg)
}

/// Append the synthetic PACKAGE tier to a code-node vec and re-parent the package-root modules into
/// it — but ONLY when the index spans ≥2 distinct packages, so this tier adds real structure.
///
/// For every code node (spec nodes are skipped) we read its package via [`package_of`]. If fewer
/// than 2 distinct non-empty packages are present (the common single-package TS/Python/Go/Java —
/// and a single-crate Rust — case), NOTHING is synthesized and parents are left unchanged: the
/// graph renders with its top modules at the structural root, exactly as before this tier existed.
/// With ≥2 packages, one `code:package` node (`id = "package:<name>"`, `kind` [`PACKAGE_KIND`],
/// `parent = None`, empty file/0 lines) is created per distinct package, and every CURRENT top-level
/// code node (`parent == None` — i.e. a package-root module) is re-pointed at its package node's
/// index. Package nodes are APPENDED (stable indices; existing nodes/edges undisturbed) and carry NO
/// edges of their own — they'd otherwise distort the force layout (their position is fixed to a
/// member centroid at draw time instead). status: code-graph-package-tier
fn synthesize_packages(nodes: &mut Vec<EntityNode>) {
    // Distinct package name → its (eventual) node index, in first-seen order for a stable layout.
    let mut package_index: HashMap<String, usize> = HashMap::new();
    let mut package_order: Vec<String> = Vec::new();
    for node in nodes.iter() {
        if node.is_spec() {
            continue;
        }
        if let Some(name) = package_of(&node.id) {
            if !package_index.contains_key(name) {
                package_index.insert(name.to_string(), 0); // index backfilled below
                package_order.push(name.to_string());
            }
        }
    }
    // The ≥2-package guard: a single-package SCIP gets NO package tier (its top modules stay the
    // structural roots), avoiding the useless one-giant-bundle collapse. status: code-graph-package-tier
    if package_order.len() < 2 {
        return;
    }
    // Assign package node indices (appended after all current nodes) before re-parenting.
    let base = nodes.len();
    for (offset, name) in package_order.iter().enumerate() {
        package_index.insert(name.clone(), base + offset);
    }
    // Re-point each package-root module (a top-level code node) into its package.
    for node in nodes.iter_mut() {
        if node.is_spec() || node.parent.is_some() {
            continue;
        }
        if let Some(&pi) = package_of(&node.id).and_then(|c| package_index.get(c)) {
            node.parent = Some(pi);
        }
    }
    // Append the package nodes (no edges).
    for name in &package_order {
        nodes.push(EntityNode {
            id: format!("package:{name}"),
            name: name.clone(),
            kind: PACKAGE_KIND.to_string(),
            file: String::new(),
            start_line: 0,
            lines: 0,
            status: None,
            parent: None,
        });
    }
}

/// The display name for a spec document container: the defining note's basename without its
/// extension (`docs/canvas.md` → `canvas`). Falls back to the whole path when it has no basename.
fn doc_basename(file: &str) -> String {
    let base = file.rsplit(['/', '\\']).next().unwrap_or(file);
    base.rsplit_once('.').map_or(base, |(stem, _)| stem).to_string()
}

/// Append the synthetic SPEC-DOCUMENT tier to a node vec and re-parent each spec node into its
/// document. The spec-side mirror of [`synthesize_packages`]: every spec node with a non-empty
/// `file` is grouped by that path into one `spec:document` container (`id = "specdoc:<file>"`,
/// `name` = the doc basename, `kind` [`SPECDOC_KIND`], `parent = None`, no edges). Specdoc nodes
/// are APPENDED (stable indices; existing nodes/edges undisturbed) and carry NO edges of their own
/// (the Governs/Reference edges stay on the individual specs) — they'd otherwise distort the force
/// layout, so their position is fixed to a member centroid at draw time instead. A spec with an
/// empty `file` is left parentless (it can't be grouped — it falls back to the parentless small
/// depth bump in `node_depths`). No-op when no spec carries a `file`. status: code-graph-spec-tier
fn synthesize_specdocs(nodes: &mut Vec<EntityNode>) {
    // Distinct spec `file` → its (eventual) node index, in first-seen order for a stable layout.
    let mut doc_index: HashMap<String, usize> = HashMap::new();
    let mut doc_order: Vec<String> = Vec::new();
    for node in nodes.iter() {
        if node.is_spec() && !node.file.is_empty() && !doc_index.contains_key(&node.file) {
            doc_index.insert(node.file.clone(), 0); // index backfilled below
            doc_order.push(node.file.clone());
        }
    }
    if doc_order.is_empty() {
        return;
    }
    // Assign specdoc node indices (appended after all current nodes) before re-parenting.
    let base = nodes.len();
    for (offset, file) in doc_order.iter().enumerate() {
        doc_index.insert(file.clone(), base + offset);
    }
    // Re-point each spec node into its document container.
    for node in nodes.iter_mut() {
        if node.is_spec() && !node.file.is_empty() {
            if let Some(&di) = doc_index.get(&node.file) {
                node.parent = Some(di);
            }
        }
    }
    // Append the specdoc nodes (no edges).
    for file in &doc_order {
        nodes.push(EntityNode {
            id: format!("specdoc:{file}"),
            name: doc_basename(file),
            kind: SPECDOC_KIND.to_string(),
            file: file.clone(),
            start_line: 0,
            lines: 0,
            status: None,
            parent: None,
        });
    }
}

/// Read each spec-defining note body once (deduped by path) from the spec-anchor index. Returns
/// `(slug → defining note paths, note path → body)`. An unreadable body is logged and dropped.
fn anchor_bodies(
    store: &Store,
    vault: &Vault,
) -> (HashMap<String, Vec<String>>, HashMap<String, String>) {
    let anchors = store.all_spec_anchors().unwrap_or_else(|err| {
        tracing::warn!(%err, "entity_graph: all_spec_anchors failed; spec nodes/edges dropped");
        Vec::new()
    });
    let mut anchor_paths: HashMap<String, Vec<String>> = HashMap::new();
    for (slug, note_path) in &anchors {
        anchor_paths.entry(slug.clone()).or_default().push(note_path.clone());
    }
    let mut bodies: HashMap<String, String> = HashMap::new();
    for paths in anchor_paths.values() {
        for note_path in paths {
            if bodies.contains_key(note_path) {
                continue;
            }
            match vault.read_file(note_path) {
                Ok(text) => {
                    bodies.insert(note_path.clone(), text);
                }
                Err(err) => {
                    tracing::warn!(%note_path, %err, "entity_graph: note body unreadable; its spec edges dropped");
                }
            }
        }
    }
    (anchor_paths, bodies)
}

/// Scan the defining-note bodies for `[[spec:slug]]` wikilinks (→ `Reference` edges) and capture
/// each `[slug]` anchor's line span into its spec node's `lines`. Each wikilink is attributed to
/// the nearest-preceding `[slug]` anchor; an edge lands only when BOTH endpoints are known spec
/// nodes; self-references skip.
fn scan_spec_bodies(
    bodies: &HashMap<String, String>,
    spec_index: &HashMap<String, usize>,
    nodes: &mut [EntityNode],
    edges: &mut Vec<(usize, usize, EntityEdge)>,
) {
    for text in bodies.values() {
        let mut anchor: Option<String> = None;
        let mut span_start: u32 = 0;
        for (line_no, line) in text.lines().enumerate() {
            let line_no = line_no as u32;
            if let Some(s) = slug_in_line(line) {
                close_span(nodes, spec_index, anchor.as_deref(), span_start, line_no);
                anchor = Some(s);
                span_start = line_no;
            }
            let Some(src_slug) = &anchor else { continue };
            let Some(&from) = spec_index.get(src_slug) else { continue };
            for link in wikilink::parse_links(line) {
                let Some(target) = wikilink::parse_spec_target(&link.target) else { continue };
                if target == src_slug {
                    continue; // self-reference
                }
                let Some(&to) = spec_index.get(target) else { continue };
                edges.push((from, to, EntityEdge::Reference));
            }
        }
        let eof = text.lines().count() as u32;
        close_span(nodes, spec_index, anchor.as_deref(), span_start, eof);
    }
}

/// Record `[start, end)` as the line span of `slug`'s `[slug]`-section into its spec node's
/// `lines` (first non-empty span wins). No-op without an open known-spec anchor.
fn close_span(
    nodes: &mut [EntityNode],
    spec_index: &HashMap<String, usize>,
    anchor: Option<&str>,
    start: u32,
    end: u32,
) {
    let Some(slug) = anchor else { return };
    let Some(&i) = spec_index.get(slug) else { return };
    let span = end.saturating_sub(start);
    if nodes[i].lines == 0 {
        nodes[i].lines = span;
    }
}

/// A **lens**: which node kinds and edge kinds a view draws, plus its sizing and an optional
/// flag. Each lens drives its OWN filtered display subgraph ([`filter_for`]) + force layout, so
/// toggling a filter re-runs FA on the visible subset. The view holds two lenses (the interactive
/// primary + the corner-minimap secondary) and swaps them via a toolbar button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Lens {
    /// Per-kind visibility, auto-populated from the kinds present (incl. [`SPEC_KIND`]).
    pub(crate) kinds: Vec<(String, bool)>,
    pub(crate) show_calls: bool,
    pub(crate) show_impls: bool,
    pub(crate) show_governs: bool,
    pub(crate) show_refs: bool,
    /// Weight node radius by LOC (`lines`) instead of degree.
    pub(crate) size_by_loc: bool,
    /// Draw only entities changed vs HEAD (needs the change data; no-op without it).
    pub(crate) changed_only: bool,
    /// Hide degree-0 nodes (the disconnected ring) in overview scope. On by default.
    pub(crate) hide_orphans: bool,
    /// Spatial auto-bundling: collapse on-screen-close nodes into one cluster rep (labelled
    /// `name · N`), splitting on zoom-in. Mirrored onto the engine's `bundling` flag. OFF by default —
    /// the default is the full dense graph (all nodes shown, decluttered by the label LOD only);
    /// bundling is an opt-in simplification toggled from the toolbar. status: code-graph-bundling
    pub(crate) bundling: bool,
}

impl Lens {
    /// A lens drawing everything present in `graph`.
    pub(crate) fn all(graph: &EntityGraph) -> Self {
        Self {
            kinds: graph.kinds_present().into_iter().map(|k| (k, true)).collect(),
            show_calls: true,
            show_impls: true,
            show_governs: true,
            show_refs: true,
            size_by_loc: false,
            changed_only: false,
            hide_orphans: true,
            bundling: false,
        }
    }

    /// The interactive primary lens's default: the higher-altitude entities (types / modules /
    /// functions + specs) on, the leaf members (methods / fields / constants / macros / imports)
    /// off. This is the legibility default the old collapse gave AND the perf default — the leaf
    /// members are the bulk of a repo's symbols, so drawing them all every frame is what made the
    /// view sluggish; the user toggles any kind on. Every kind is still loaded in the graph.
    pub(crate) fn primary_default(graph: &EntityGraph) -> Self {
        let kinds = graph
            .kinds_present()
            .into_iter()
            .map(|k| {
                let on = matches!(
                    k.as_str(),
                    PACKAGE_KIND
                        | SPECDOC_KIND
                        | "code:type"
                        | "code:module"
                        | "code:function"
                        | SPEC_KIND
                );
                (k, on)
            })
            .collect();
        Self { kinds, ..Self::all(graph) }
    }

    /// The default secondary lens: specs + their reference/governs edges only (the "spec map" the
    /// corner minimap shows out of the box). Code kinds start hidden.
    pub(crate) fn specs_only(graph: &EntityGraph) -> Self {
        let kinds = graph
            .kinds_present()
            .into_iter()
            .map(|k| (k.clone(), k == SPEC_KIND))
            .collect();
        Self { kinds, ..Self::all(graph) }
    }

    /// Whether `kind` is drawn by this lens (a kind missing from the rows defaults to visible).
    fn kind_on(&self, kind: &str) -> bool {
        self.kinds.iter().find(|(k, _)| k == kind).is_none_or(|(_, on)| *on)
    }

    /// Whether an edge of `kind` is drawn (before the both-endpoints-drawn check).
    const fn edge_on(&self, kind: EntityEdge) -> bool {
        match kind {
            // Calls / TypeRef / Imports ride the "Calls" toggle for v1, like the old code graph.
            EntityEdge::Calls | EntityEdge::TypeRef | EntityEdge::Imports => self.show_calls,
            EntityEdge::Implements => self.show_impls,
            EntityEdge::Governs => self.show_governs,
            EntityEdge::Reference => self.show_refs,
        }
    }
}

/// Build the **displayed subgraph** for `lens` — the unit the engine lays out + renders, so
/// toggling a filter rebuilds it and re-runs the force layout ("redo FA on filter").
///
/// A node is *visible* when its kind is on (plus the hops `anchor` always), it's within `mask`
/// (hops scope), and — when `lens.changed_only` — it actually changed vs HEAD. Edges **lift**: a
/// hidden member's edge re-attaches to its nearest visible ancestor (`parent` chain), so hiding a
/// kind keeps the higher-level connectivity instead of orphaning everything that referenced it.
/// Finally, in overview scope with `hide_orphans`, degree-0 nodes are dropped (the noisy
/// disconnected ring). Reindexes to a dense `0..n`. status: spec-graph-lens
pub(crate) fn filter_for(
    full: &EntityGraph,
    lens: &Lens,
    mask: Option<&[bool]>,
    changes: Option<&Changes>,
    anchor: Option<&str>,
    force_show: &[String],
) -> EntityGraph {
    let n = full.nodes.len();
    let visible: Vec<bool> = full
        .nodes
        .iter()
        .enumerate()
        .map(|(i, node)| {
            // The hops centre, and a selected spec's governed entities (`force_show`), always show —
            // even when their kind is filtered — so the selection's footprint is complete.
            if anchor == Some(node.id.as_str()) || force_show.iter().any(|m| m == &node.id) {
                return true;
            }
            let in_scope = mask.is_none_or(|m| m.get(i).copied().unwrap_or(false));
            let changed =
                !lens.changed_only || changes.is_some_and(|c| c.touches(&node.file, &node.id));
            lens.kind_on(&node.kind) && in_scope && changed
        })
        .collect();
    // The nearest visible ancestor of `i` (itself if visible), walking the containment chain. The
    // chain is shallow (module → type → member), so this is cheap without memoizing.
    let nearest = |mut i: usize| -> Option<usize> {
        loop {
            if visible[i] {
                return Some(i);
            }
            i = full.nodes[i].parent?;
        }
    };
    // Lift every edge onto visible endpoints; dedup (many member edges collapse to one ancestor
    // pair) and drop the self-loops lifting can create.
    let mut lifted: std::collections::HashSet<(usize, usize, EntityEdge)> =
        std::collections::HashSet::new();
    for &(a, b, k) in &full.edges {
        if !lens.edge_on(k) {
            continue;
        }
        if let (Some(ra), Some(rb)) = (nearest(a), nearest(b)) {
            if ra != rb {
                lifted.insert((ra, rb, k));
            }
        }
    }
    // Orphan-hiding (overview only — a hops neighbourhood keeps its in-reach members): drop visible
    // nodes with no lifted edge. Orphans have degree 0, so dropping them removes no edges (no
    // cascade). The anchor is never dropped.
    let mut keep = visible.clone();
    if mask.is_none() && lens.hide_orphans {
        let mut degree = vec![0u32; n];
        for &(a, b, _) in &lifted {
            degree[a] += 1;
            degree[b] += 1;
        }
        for (i, node) in full.nodes.iter().enumerate() {
            // Container nodes (packages + spec documents) carry no edges by design (they'd distort
            // the force layout) — they're not orphans to hide; they roll their members up at the
            // overview. status: code-graph-package-tier, code-graph-spec-tier
            if keep[i]
                && degree[i] == 0
                && !is_container_kind(&node.kind)
                && anchor != Some(node.id.as_str())
            {
                keep[i] = false;
            }
        }
    }
    let mut remap = vec![usize::MAX; n];
    let mut nodes = Vec::new();
    for (i, node) in full.nodes.iter().enumerate() {
        if keep[i] {
            remap[i] = nodes.len();
            nodes.push(node.clone()); // parent rewritten to display indices in the pass below
        }
    }
    // Rewrite each kept node's `parent` to its nearest KEPT ancestor in display-index space — so the
    // containment chain survives the reindex for the bundling rollup (`code:module` containers). A
    // node whose ancestors were all filtered out gets `None`. status: code-graph-bundling
    for node in &mut nodes {
        // `node.parent` is still a full-graph index here (cloned above); walk it up to the first kept
        // ancestor, then map to its display index.
        let mut p = node.parent;
        node.parent = loop {
            match p {
                Some(fi) if fi < n => {
                    if keep[fi] {
                        break Some(remap[fi]);
                    }
                    p = full.nodes[fi].parent;
                }
                _ => break None,
            }
        };
    }
    let mut edges: Vec<(usize, usize, EntityEdge)> = lifted
        .into_iter()
        .filter(|&(a, b, _)| keep[a] && keep[b])
        .map(|(a, b, k)| (remap[a], remap[b], k))
        .collect();
    edges.sort_by_key(|&(a, b, _)| (a, b)); // stable order (the HashSet is unordered)
    EntityGraph { nodes, edges }
}

/// The engine [`Source`] over an ALREADY-FILTERED graph (the [`filter_for`] display subgraph): it
/// draws every node + edge it's given. Node fill is by kind; the node ring (`resting_stroke`)
/// carries the git change; a `Governs` edge carries the drift color of its code target. Precomputes
/// per-edge colors so `edge_color` is O(1)-indexed.
pub(crate) struct EntityGraphSource<'a> {
    graph: &'a EntityGraph,
    /// Weight node radius by LOC (`lines`) instead of degree.
    size_by_loc: bool,
    /// Git change-ring data (`Some` when "show changes" is on).
    changes: Option<&'a Changes>,
    /// Warm governance, for `Governs` edge drift colors + the open-bug badge.
    governance: Option<&'a Governance>,
    /// Per-node degree of THIS (filtered) graph + its max (≥1).
    degree: Vec<u32>,
    maxd: f32,
    /// The largest node `lines` (for √-scaled LOC sizing).
    max_loc: f32,
    /// When `Some`, every node draws at this fixed radius (the corner minimap's small dots).
    /// status: spec-minimap-swap
    dot_radius: Option<f32>,
    /// Per-edge stroke colors, aligned to `graph.edges`.
    edge_colors: Vec<Option<egui::Color32>>,
    /// Focus "spotlight" (`Some` = active): a per-node mask of what stays at full strength. Nodes
    /// NOT in the set dim to faint context (the selection's footprint pops by contrast — the
    /// dim-the-rest pattern every graph UX uses, not an additive glow). status: code-graph-spec-lighting
    focus_nodes: Option<Vec<bool>>,
    /// Per-edge "both endpoints in focus" mask (only meaningful when `focus_nodes` is `Some`).
    focus_edges: Vec<bool>,
    /// User color overrides per kind (`code:type`, `spec`, …); a missing kind uses the built-in
    /// palette. status: graph-view-state-persist
    palette: Option<&'a HashMap<String, egui::Color32>>,
    /// Per-node-id containment-subtree weight (0..1) for the label LOD — computed once on the FULL
    /// graph (parent is dropped in the display) + cached on the view. status: graph-label-dim
    importance: Option<&'a HashMap<String, f32>>,
}

impl<'a> EntityGraphSource<'a> {
    /// Build a source over the pre-filtered display `graph`.
    pub(crate) fn new(
        graph: &'a EntityGraph,
        size_by_loc: bool,
        changes: Option<&'a Changes>,
        governance: Option<&'a Governance>,
    ) -> Self {
        let degree = graph.degrees();
        let maxd = degree.iter().copied().max().unwrap_or(1).max(1) as f32;
        let edge_colors =
            graph.edges.iter().map(|&(_, b, kind)| edge_color_for(kind, graph, governance, b)).collect();
        Self {
            graph,
            size_by_loc,
            changes,
            governance,
            degree,
            maxd,
            max_loc: graph.max_lines(),
            dot_radius: None,
            edge_colors,
            focus_nodes: None,
            focus_edges: Vec::new(),
            palette: None,
            importance: None,
        }
    }

    /// Supply the cached per-node-id subtree-weight map that drives the label LOD.
    /// status: graph-label-dim
    #[must_use]
    pub(crate) const fn with_importance(mut self, importance: &'a HashMap<String, f32>) -> Self {
        self.importance = Some(importance);
        self
    }

    /// Apply user color overrides per kind (a missing kind keeps the built-in palette).
    /// status: graph-view-state-persist
    #[must_use]
    pub(crate) const fn with_palette(mut self, palette: &'a HashMap<String, egui::Color32>) -> Self {
        self.palette = Some(palette);
        self
    }

    /// The fill for a node `kind`: the user override if set, else the built-in [`kind_color`].
    fn fill_for(&self, kind: &str) -> egui::Color32 {
        self.palette.and_then(|p| p.get(kind).copied()).unwrap_or_else(|| kind_color(kind))
    }

    /// Draw every node at a fixed `r` (small uniform dots) — the corner minimap, where the main
    /// view's degree/LOC sizing overlaps badly. status: spec-minimap-swap
    #[must_use]
    pub(crate) const fn with_dot_radius(mut self, r: f32) -> Self {
        self.dot_radius = Some(r);
        self
    }

    /// Spotlight the `focus` node indices: they draw at full strength, everything else dims to
    /// faint context. Empty `focus` clears the spotlight (everything full). status: code-graph-spec-lighting
    #[must_use]
    pub(crate) fn with_focus(mut self, focus: &[usize]) -> Self {
        if focus.is_empty() {
            return self;
        }
        let n = self.graph.nodes.len();
        let mut node = vec![false; n];
        for &i in focus {
            if i < n {
                node[i] = true;
            }
        }
        self.focus_edges = self.graph.edges.iter().map(|&(a, b, _)| node[a] && node[b]).collect();
        self.focus_nodes = Some(node);
        self
    }

    /// Whether node `index` is dimmed by the focus spotlight (focus active + not in the set).
    fn dimmed(&self, index: usize) -> bool {
        self.focus_nodes.as_ref().is_some_and(|f| !f.get(index).copied().unwrap_or(false))
    }

    /// The render radius for node `index`: the fixed [`Self::dot_radius`] when set (minimap), else
    /// √-scaled LOC (`lines`) in size-by-LOC mode or the degree-normalised default (~4..13px).
    fn radius(&self, index: usize) -> f32 {
        if let Some(r) = self.dot_radius {
            r
        } else if self.size_by_loc {
            4.0 + 9.0 * (self.graph.nodes[index].lines as f32 / self.max_loc).sqrt()
        } else if let Some(imp) = self.importance {
            // Default: size by STRUCTURAL IMPORTANCE (the subtree+connectivity blend, 0..1) — crates
            // and hub modules read big, leaf symbols stay small dots. This gives the dense full graph
            // its organic varied-cell texture AND makes the structurally-significant nodes the big
            // ones, which is what the on-screen-radius label gate then labels at the overview.
            let w = imp.get(&self.graph.nodes[index].id).copied().unwrap_or(0.0);
            4.0 + IMPORTANCE_RADIUS_K * w
        } else {
            4.0 + 7.0 * (self.degree.get(index).copied().unwrap_or(0) as f32 / self.maxd)
        }
    }

    /// Per-display-node CONTAINMENT DEPTH: the number of `parent` hops to a root (depth 0 = a node
    /// with `parent == None`). Computed over the DISPLAY graph (where `filter_for` rewrote `parent`
    /// to display indices), so it reflects whatever structure the SCIP actually has — `package →
    /// module → type → …` for a multi-package index, or `module → type → …` for a single-package one,
    /// for ANY language. This REPLACES the old kind→level ladder, which regressed single-package
    /// (TS/Python/Go/Java) SCIPs. Spec nodes (parent `None`) are depth 0; they get a `+1` bump so
    /// they sit just BELOW the structural roots instead of flooding the overview tier. Cycles are
    /// guarded (capped walk → treated as a root). Cheap O(n) with shallow walks; once per `nodes()`.
    /// status: graph-label-dim
    fn node_depths(&self) -> Vec<f32> {
        let nodes = &self.graph.nodes;
        let n = nodes.len();
        let mut depths = vec![0u32; n];
        for i in 0..n {
            let mut d = 0u32;
            let mut p = nodes[i].parent;
            while let Some(a) = p {
                if a >= n || d > 64 {
                    break; // cycle / corrupt parent → treat as a root from here
                }
                d += 1;
                p = nodes[a].parent;
            }
            // A spec is normally parented into its `spec:document` container (depth 1), so its depth
            // comes from the real parent — the specdoc reads as a structural root (depth 0) at the
            // overview like a package, and individual spec slugs reveal on zoom-in via the label
            // budget, no special-casing. A spec with NO specdoc (empty `file` — shouldn't happen)
            // keeps the old fallback bump BELOW the skeleton so it doesn't flood the overview tier.
            // status: code-graph-spec-tier
            depths[i] = if nodes[i].kind == SPEC_KIND && nodes[i].parent.is_none() {
                d + SKELETON_DEPTH as u32 + 1
            } else {
                d
            };
        }
        depths.into_iter().map(|d| d as f32).collect()
    }

    /// For each CONTAINER node (`code:package` or `spec:document`), the display indices of every node
    /// whose nearest-container ancestor is that container — its rendered MEMBERS. Drives the centroid
    /// `world_pos` override: a container carries no edges, so its force position is junk; we sit its
    /// label over its members' cluster (a package over its modules, a specdoc over its specs)
    /// instead. Cheap O(n) with shallow parent walks; computed once per `nodes()` call.
    /// status: code-graph-package-tier, code-graph-spec-tier
    fn container_members(&self) -> HashMap<usize, Vec<usize>> {
        let nodes = &self.graph.nodes;
        let n = nodes.len();
        let mut members: HashMap<usize, Vec<usize>> = HashMap::new();
        for i in 0..n {
            // Walk up to the nearest container ancestor (excluding `i` itself if it IS one).
            let mut p = nodes[i].parent;
            let mut guard = 0;
            while let Some(a) = p {
                if a >= n || guard > 64 {
                    break;
                }
                if is_container_kind(&nodes[a].kind) {
                    members.entry(a).or_default().push(i);
                    break;
                }
                p = nodes[a].parent;
                guard += 1;
            }
        }
        members
    }
}

/// Whether `kind` is a synthetic CONTAINER tier (carries no edges; sits over a member centroid) —
/// a `code:package` or a `spec:document`. status: code-graph-package-tier, code-graph-spec-tier
fn is_container_kind(kind: &str) -> bool {
    kind == PACKAGE_KIND || kind == SPECDOC_KIND
}

/// `Implements` edge hue (muted violet) — distinct from calls/typeref so an `impl` reads at a
/// glance, but thin + unsaturated so it stays faint like the rest.
const IMPLEMENTS_EDGE: egui::Color32 = egui::Color32::from_rgb(0x8a, 0x6f, 0xb0);
/// Default `calls`/typeref/import edge hue shown in the picker (the engine's faint default).
const CALLS_EDGE_DEFAULT: egui::Color32 = egui::Color32::from_rgb(0x90, 0x96, 0xa0);
/// Alpha for the translucent `Governs`/`Reference` edges — fainter than the call edges' ~0xa0 so the
/// many-to-many spec fan-out recedes into a wash instead of a saturated hairball. status: code-graph-governance-overlay
const GOV_EDGE_ALPHA: u8 = 0x36;
/// Alpha for `Implements` edges — a touch more present than governance (fewer of them, structural),
/// still translucent so they blend with the call haze rather than dominate. status: code-graph-governance-overlay
const IMPL_EDGE_ALPHA: u8 = 0x82;

/// A translucent copy of `c` at alpha `a` (unmultiplied), so an edge hue recedes into the faint wash
/// instead of drawing as an opaque saturated line.
fn translucent(c: egui::Color32, a: u8) -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a)
}

/// The palette key for an edge KIND whose colour the user can override — `None` for `Governs`,
/// which is coloured by drift state (not a single colour). status: graph-view-state-persist
pub(crate) const fn edge_palette_key(kind: EntityEdge) -> Option<&'static str> {
    match kind {
        EntityEdge::Calls | EntityEdge::TypeRef | EntityEdge::Imports => Some("edge:calls"),
        EntityEdge::Implements => Some("edge:implements"),
        EntityEdge::Reference => Some("edge:reference"),
        EntityEdge::Governs => None,
    }
}

/// The built-in default colour for an edge-palette key — what the picker swatch shows before any
/// override. status: graph-view-state-persist
pub(crate) fn edge_default_color(key: &str) -> egui::Color32 {
    match key {
        "edge:implements" => IMPLEMENTS_EDGE,
        "edge:reference" => theme::kind_spec().gamma_multiply(0.7),
        _ => CALLS_EDGE_DEFAULT,
    }
}

/// Resolve a spec's governance `targets` to the code-node ids they govern within `code_ids`: a leaf
/// target passes through (membership is checked by the caller); a `component`/`container` moniker —
/// a module namespace ending in `/`, which SCIP exposes as a synthetic PREFIX with no node of its
/// own — expands to every id under that prefix. Deduped + sorted. The shared rule behind both the
/// `Governs` edge build and the lit footprint. status: code-graph-spec-lighting
fn resolve_governed<'a>(targets: &'a [String], code_ids: &[&'a str]) -> Vec<&'a str> {
    let mut ids: Vec<&str> = Vec::new();
    for target in targets {
        if target.ends_with('/') {
            ids.extend(code_ids.iter().copied().filter(|id| id.starts_with(target.as_str())));
        } else {
            ids.push(target.as_str());
        }
    }
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// The stroke color of an edge (thin + faint, so edges read without hiding the nodes): a `Governs`
/// edge takes the drift color of its code target (`to`), a `Reference` edge the muted spec hue, an
/// `Implements` edge the muted violet, and code call/typeref/import edges fall back to the engine's
/// faint default. status: code-graph-governance-overlay
fn edge_color_for(
    kind: EntityEdge,
    graph: &EntityGraph,
    governance: Option<&Governance>,
    to: usize,
) -> Option<egui::Color32> {
    match kind {
        // Governs / Reference fan out many-to-many across the whole graph (one spec → many spread-out
        // symbols), so at full opacity they pile into a saturated hairball. Draw them TRANSLUCENT so
        // they recede into a faint wash like the call edges (which use the engine's ~0xa0 default) —
        // the drift hue stays readable in aggregate, and selecting/hovering a spec lights its own
        // edges bright via the highlight overlay. status: code-graph-governance-overlay
        EntityEdge::Governs => {
            let target = &graph.nodes.get(to)?.id;
            let state = governance.map_or(GovState::Ungoverned, |g| g.state_of(target));
            Some(translucent(gov_color(state), GOV_EDGE_ALPHA))
        }
        EntityEdge::Reference => Some(translucent(theme::kind_spec(), GOV_EDGE_ALPHA)),
        EntityEdge::Implements => Some(translucent(IMPLEMENTS_EDGE, IMPL_EDGE_ALPHA)),
        _ => None,
    }
}

/// Flat fill per node kind: the SCIP `code:*` palette, or the spec hue for a spec node.
pub(crate) fn kind_color(kind: &str) -> egui::Color32 {
    match kind {
        SPEC_KIND => theme::kind_spec(),
        // Deep indigo — a strong "container" hue clearly distinct from the module violet, the type
        // blue, and the spec slate, so a package reads as the top tier. status: code-graph-package-tier
        PACKAGE_KIND => egui::Color32::from_rgb(0x3b, 0x3f, 0x9e),
        // Warm teal — a "document" accent clearly distinct from the package indigo and the spec
        // slate, so a spec-document container reads as the top tier of the spec side without being
        // confused for a package. status: code-graph-spec-tier
        SPECDOC_KIND => egui::Color32::from_rgb(0x2f, 0x9e, 0x8f),
        "code:type" => egui::Color32::from_rgb(0x4f, 0x83, 0xcc),
        "code:function" => egui::Color32::from_rgb(0x4c, 0xaf, 0x72),
        "code:method" => egui::Color32::from_rgb(0x3f, 0xb6, 0xa8),
        "code:module" => egui::Color32::from_rgb(0x95, 0x75, 0xcd),
        "code:macro" => egui::Color32::from_rgb(0xc9, 0x8b, 0x3a),
        "code:constant" => egui::Color32::from_rgb(0xc7, 0x5b, 0x6d),
        "code:field" => egui::Color32::from_rgb(0xb0, 0x89, 0x4a),
        _ => egui::Color32::from_rgb(0x9e, 0x9e, 0x9e),
    }
}

/// Zoom-per-TIER (depth beyond [`SKELETON_DEPTH`]) for the label LOD, in FIT-RELATIVE (ratio) units:
/// the always-shown skeleton (packages + top modules) always labels, and a node `T` tiers deeper
/// labels once the fit-relative zoom (`view.zoom / fitted-overview zoom`, `1.0` = overview) reaches
/// `T * HIER_LOD_STEP` (minus a small importance bonus). Depth is the number of `parent` hops to a
/// root — language-neutral, reflecting the ACTUAL nesting the SCIP carries, not a fixed kind ladder.
/// This LABEL LOD is independent of the SPATIAL node bundling (which the engine drives off on-screen
/// proximity): it only governs which NAMES appear as you zoom, so a dense cluster's rep can still
/// carry a legible label while its members stay collapsed. status: graph-label-dim
const HIER_LOD_STEP: f32 = 1.0;
/// How much a node's degree percentile pulls its label threshold EARLIER — a within-depth tiebreak
/// so the bigger members of a depth surface first instead of at spatial random. Kept below
/// `HIER_LOD_STEP` so it can't reorder the depth tiers. status: graph-label-dim
const HIER_LOD_IMPORTANCE: f32 = 1.0;

/// The structural-skeleton depth whose LABELS always show (the always-labelled tier): containers up
/// to this containment depth label at the overview, and only nodes DEEPER than this gate their label
/// on zoom-in. `1.0` = packages (depth 0) + their top modules (depth 1) keep labels at the overview,
/// so it reads as a module map with crate names on top; deeper symbols' names reveal as you zoom. This
/// is LABEL-only — node visibility is the engine's spatial bundling, not depth. In a single-package
/// SCIP (modules at depth 0) this keeps modules + their immediate members labelled. status: graph-label-dim
const SKELETON_DEPTH: f32 = 1.0;

/// Per-node-id "structural weight" (0..1) for the label LOD — a SIZE + CONNECTIVITY blend, the way
/// general graph viewers (Gephi et al.) rank label/size priority by a centrality measure rather than
/// a raw child count. Adapted to a code containment hierarchy: code edges live at the leaf/symbol
/// level (calls/type-refs), not on container nodes, so we accumulate BOTH signals UP the `parent`
/// chain to each ancestor:
///   - `subtree[i]` = how many symbols the node contains (its size as a container), and
///   - `conn[i]`    = the total degree of those contained symbols (how connected its contents are).
/// A module then scores high when it's big AND its contents are well-connected — so a central hub
/// module outranks an equally-large but isolated one, and a small leaf module scores low on both
/// (which is what was wrong before: subtree-size alone let peripheral leaf modules tie the big ones).
/// Both are log-normalized (leaf-heavy distribution) and blended size-leads/connectivity-breaks-ties.
/// Computed on the FULL graph (`parent` is valid there; `filter_for` drops it in the display) and
/// cached on the view, keyed by id so either lens's display can look it up. status: graph-label-dim
pub(crate) fn label_importance(graph: &EntityGraph) -> HashMap<String, f32> {
    let n = graph.nodes.len();
    let degree = graph.degrees();
    // subtree[i] = 1 (self) + descendants; conn[i] = degree of self + all descendants. Both are
    // built by walking each node's parent chain upward and crediting every ancestor.
    let mut subtree = vec![1u32; n];
    let mut conn = vec![0u64; n];
    for i in 0..n {
        let deg = degree.get(i).copied().unwrap_or(0) as u64;
        conn[i] += deg; // self
        let mut parent = graph.nodes[i].parent;
        let mut guard = 0;
        while let Some(a) = parent {
            if a >= n || guard > 64 {
                break;
            }
            subtree[a] += 1;
            conn[a] += deg;
            parent = graph.nodes[a].parent;
            guard += 1;
        }
    }
    let max_sub = subtree.iter().copied().max().unwrap_or(1).max(1) as f32;
    let max_conn = conn.iter().copied().max().unwrap_or(1).max(1) as f32;
    let sden = (1.0 + max_sub).ln().max(1e-3);
    let cden = (1.0 + max_conn as f32).ln().max(1e-3);
    graph
        .nodes
        .iter()
        .enumerate()
        .map(|(i, node)| {
            let size = (1.0 + subtree[i] as f32).ln() / sden;
            let central = (1.0 + conn[i] as f32).ln() / cden;
            // Size leads (cleanest container signal); connectivity breaks ties between same-size
            // containers so a hub outranks an isolated peer.
            (node.id.clone(), 0.6 * size + 0.4 * central)
        })
        .collect()
}

/// Font-size multiplier per kind, so high-level nodes read as larger text. (Presentation only — the
/// LOD/bundling tiers are driven by structural DEPTH, not by kind.) status: graph-label-dim
fn label_scale_for(kind: &str) -> f32 {
    match kind {
        PACKAGE_KIND => 1.8,
        // A spec document is a container like a package — read its label largest. status: code-graph-spec-tier
        SPECDOC_KIND => 1.8,
        "code:module" => 1.5,
        "code:type" => 1.15,
        // Specs tie with functions for label priority (not above types) so code structure leads the
        // de-confliction; they're still distinguished by colour/shape. status: graph-label-dim
        SPEC_KIND => 1.0,
        "code:constant" | "code:field" => 0.9,
        _ => 1.0,
    }
}

impl Source for EntityGraphSource<'_> {
    fn node_count(&self) -> usize {
        self.graph.nodes.len()
    }

    fn nodes(&self, positions: &[egui::Vec2], _style: &Style) -> Vec<NodeDescriptor> {
        // Structural containment depth per display node — the language-neutral LOD driver (roots at
        // depth 0 always show; deeper tiers reveal on zoom-in). status: graph-label-dim
        let depths = self.node_depths();
        // Container nodes (packages + spec documents) carry no edges, so their force position is
        // meaningless — sit each over the centroid of its members instead. Precomputed once per
        // frame. status: code-graph-package-tier, code-graph-spec-tier
        let container_members = self.container_members();
        let container_centroid = |container_idx: usize| -> Option<egui::Vec2> {
            let members = container_members.get(&container_idx)?;
            let (sum, count) = members
                .iter()
                .filter_map(|&m| positions.get(m).copied())
                .fold((egui::Vec2::ZERO, 0u32), |(s, c), p| (s + p, c + 1));
            (count > 0).then(|| sum / count as f32)
        };
        self.graph
            .nodes
            .iter()
            .enumerate()
            .filter(|(i, _)| *i < positions.len())
            .map(|(index, n)| {
                // The change ring (`resting_stroke`) is the DIRECT git-change channel — independent
                // of the kind fill; `None` (no change / no data) leaves the node un-ringed.
                let ring = self
                    .changes
                    .and_then(|c| c.ring(&n.file, &n.id))
                    .unwrap_or(egui::Stroke::NONE);
                // Badges are marks for the main view; in the corner minimap (compact dot mode)
                // they're unhoverable noise that reads as phantom nodes, so suppress them there.
                let compact = self.dot_radius.is_some();
                // The open-bugs badge: governance only, on nodes with an open bug edge.
                let bug = (!compact
                    && self.governance.is_some_and(|g| !g.open_bugs_of(&n.id).is_empty()))
                .then_some(BUG_BADGE);
                // The status badge: a spec node whose status is planned/partial.
                let badge = (!compact
                    && n.is_spec()
                    && n.status.as_deref().is_some_and(hiker_code::governance::status_flagged))
                .then_some(STATUS_BADGE);
                // Focus spotlight: a dimmed (out-of-focus) node fades to faint context — low-alpha
                // fill, no label, no ring/badges — so the focus set reads by contrast.
                let dim = self.dimmed(index);
                let is_container = is_container_kind(&n.kind);
                // The base degree/LOC radius. A container drawn as a bundle is inflated PER-FRAME by
                // the engine from its live rolled-up count (so it shrinks as members reveal); the
                // source no longer bakes a √count bump here. status: code-graph-bundling
                let radius = self.radius(index);
                let in_focus =
                    self.focus_nodes.as_ref().is_some_and(|f| f.get(index).copied().unwrap_or(false));
                // Containment-DEPTH LOD: a node's reveal/label zoom is set by its structural depth
                // (hops to a root) in the DISPLAY graph — language-neutral, reflecting whatever
                // nesting the SCIP carries (`package → module → type → …` multi-package, or `module →
                // type → …` single-package). Roots (depth 0 — a package, or in a single-package SCIP
                // a top module) always show; deeper tiers reveal on zoom-in. `importance` = the node's
                // containment-SUBTREE weight (a container root scores high even with few direct
                // edges), used both to pull its threshold earlier and to win the de-confliction
                // within a depth — so peers aren't picked at spatial random. A focus node always
                // labels. status: graph-label-dim
                let importance =
                    self.importance.and_then(|m| m.get(&n.id)).copied().unwrap_or(0.0);
                let depth = depths.get(index).copied().unwrap_or(0.0);
                // The structural SKELETON (containers up to `SKELETON_DEPTH` — packages + their top
                // modules) is always shown, so the overview reads as a rich module map rather than a
                // handful of package dots. Only the deeper LEAF symbols (types/functions/methods,
                // depth > skeleton) bundle into those containers and reveal on zoom-in. `tier` is the
                // depth measured from the skeleton floor (0 = first bundling tier). status: graph-label-dim
                let tier = (depth - SKELETON_DEPTH).max(0.0);
                let label_min_zoom = if in_focus || depth <= SKELETON_DEPTH {
                    0.0
                } else {
                    (tier * HIER_LOD_STEP - HIER_LOD_IMPORTANCE * importance).max(0.0)
                };
                // Plain name only — the engine appends the LIVE `· N` cluster count per frame (so the
                // suffix tracks the dissolving SPATIAL rollup, not a frozen total). status: code-graph-bundling
                let label = if dim { None } else { Some(n.name.clone()) };
                NodeDescriptor {
                    index,
                    // A container (package / spec document) sits over its members' centroid (its own
                    // force position is junk — no edges); everything else uses its laid-out position.
                    // status: code-graph-package-tier, code-graph-spec-tier
                    world_pos: if is_container {
                        container_centroid(index).unwrap_or(positions[index])
                    } else {
                        positions[index]
                    },
                    radius,
                    shape: if n.is_spec() || n.kind == "code:type" || is_container {
                        NodeShape::Square
                    } else {
                        NodeShape::Circle
                    },
                    fill: if dim { self.fill_for(&n.kind).gamma_multiply(FADE) } else { self.fill_for(&n.kind) },
                    resting_stroke: if dim { egui::Stroke::NONE } else { ring },
                    hover_stroke: egui::Stroke::new(1.5, egui::Color32::WHITE),
                    badge: if dim { None } else { badge },
                    bug_badge: if dim { None } else { bug },
                    label,
                    label_min_zoom,
                    // Priority + font biased by importance, so within a depth the bigger container
                    // (e.g. the package root) wins the de-confliction + reads larger. status: graph-label-dim
                    label_scale: label_scale_for(&n.kind) * (0.8 + 0.6 * importance),
                    click_path: Some(n.id.clone()),
                    tooltip: Some(if n.is_spec() { n.id.clone() } else { n.file.clone() }),
                }
            })
            .collect()
    }

    fn edges(&self) -> Vec<(u32, u32)> {
        self.graph.edges.iter().map(|&(a, b, _)| (a as u32, b as u32)).collect()
    }

    /// The FORCE-layout spring set: the drawn edges (calls / type-refs / governs /
    /// references) PLUS a containment spring `(parent, child)` for every display
    /// node carrying a `parent`. Containment is NOT a drawn edge (it'd litter the
    /// view with tree lines and is already encoded by the bundling rollup), but
    /// without it the layout only knows call/type-ref edges — so a module's
    /// members drift wherever their cross-module calls pull them, landing FAR from
    /// the module. When a bundle unbundles on zoom-in its members would then appear
    /// off-viewport. Each containment spring is added [`CONTAINMENT_STRENGTH`] times
    /// (the worker sums duplicate springs) so containment pulls a touch harder than
    /// a single cross-module call edge — enough to cluster a module's members around
    /// it, and modules around their package, without collapsing the cluster to a
    /// point (ForceAtlas2 repulsion keeps the members spread inside the cluster).
    /// Applies uniformly to the package→module and module→type→method chains.
    /// status: code-graph-containment-layout
    fn layout_edges(&self) -> Vec<(u32, u32)> {
        let mut edges = self.edges();
        for (child, node) in self.graph.nodes.iter().enumerate() {
            if let Some(parent) = node.parent {
                for _ in 0..CONTAINMENT_STRENGTH {
                    edges.push((parent as u32, child as u32));
                }
            }
        }
        edges
    }

    fn edge_color(&self, index: usize) -> Option<egui::Color32> {
        // Focus spotlight: an edge not entirely within the focus set fades to faint context.
        if self.focus_nodes.is_some() && !self.focus_edges.get(index).copied().unwrap_or(false) {
            return Some(FADED_EDGE);
        }
        // User edge-colour override per kind (Governs has none — it's drift-coloured).
        if let Some(p) = self.palette {
            if let Some(&(_, _, kind)) = self.graph.edges.get(index) {
                if let Some(c) = edge_palette_key(kind).and_then(|k| p.get(k)) {
                    return Some(*c);
                }
            }
        }
        self.edge_colors.get(index).copied().flatten()
    }

    fn layout_tree(&self, _kind: LayoutKind) -> LayoutTree {
        LayoutTree::from_parents(&vec![None; self.graph.nodes.len()])
    }

    fn node_key(&self, index: usize) -> Option<String> {
        self.graph.nodes.get(index).map(|n| n.id.clone())
    }

    fn preview_for(&self, index: usize) -> Option<(String, String)> {
        let n = self.graph.nodes.get(index)?;
        let detail = if n.is_spec() {
            match &n.status {
                Some(s) => format!("spec · {s}"),
                None => "spec".to_string(),
            }
        } else {
            format!("{} · {}", n.kind, n.file)
        };
        Some((n.name.clone(), detail))
    }
}

/// Status-badge dot (top-right): violet, distinct from every fill — a planned/partial spec.
const STATUS_BADGE: egui::Color32 = egui::Color32::from_rgb(0xb1, 0x7f, 0xe8);
/// Open-bugs badge dot (top-left): hot coral.
const BUG_BADGE: egui::Color32 = egui::Color32::from_rgb(0xf0, 0x62, 0x4d);
/// Alpha factor a dimmed (out-of-focus) node's fill keeps under the focus spotlight (~12% — a
/// faint ghost). status: code-graph-spec-lighting
const FADE: f32 = 0.12;
/// The faint stroke an out-of-focus edge takes under the spotlight (low-alpha neutral).
const FADED_EDGE: egui::Color32 = egui::Color32::from_rgba_premultiplied(0x12, 0x14, 0x17, 0x14);

#[cfg(test)]
mod tests;
