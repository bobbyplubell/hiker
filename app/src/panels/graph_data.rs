//! Typed vault-graph data: the build-side model behind the vault Graph tab
//! (`vault-graph-typed-edges`, `vault-graph-kind-nodes`). The graph unions
//! five edge sets — body wikilinks (scanned), board membership (straight
//! from the `board_cards` derived table), trail membership (from
//! `trail_waypoints`), list membership (from `list_refs` — epics, plans, any
//! registered list-like kind), and spec references (`[[spec:slug]]` body
//! links resolved through the `spec_anchors` index,
//! `vault-graph-spec-edges`) — each tagged with a [`VaultEdgeKind`], and
//! types every node from its `hiker.kind` against the kind registry's
//! shapes ([`VaultKind`]). Pure data + filtering logic, kept apart from the
//! egui panel (`graph.rs`) so the union, the kind classification, the
//! query-scope mask, and the LOD mapping are unit-testable.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use eframe::egui;
use petgraph::graph::{DiGraph, NodeIndex};

use hiker_core::kinds::{Registry, Shape};
use hiker_core::store::dto::{BoardCardRow, ListRefRow, WaypointRow};
use hiker_core::wikilink;
use hiker_theme as theme;

use super::graph::basename;

/// The vault link graph itself, rebuilt from disk + the derived tables on
/// demand. Held apart from the engine state so a rebuild swaps the graph
/// without resetting the user's view options or layout.
pub struct VaultData {
    pub graph: DiGraph<NodeData, VaultEdgeKind>,
    /// Cached edge list (index pairs + kind) in petgraph edge order, for the
    /// layout worker, edge drawing, and the per-edge color lookup.
    pub edges: Vec<(u32, u32, VaultEdgeKind)>,
    /// `[slug]` spec anchors defined per note path (sorted slugs), inverted
    /// from the `spec_anchors` index read — the drift-badge fold and the
    /// spec-node menu read it. Empty when the vault defines none.
    /// status: vault-graph-spec-drift-badge
    pub anchors_by_note: HashMap<String, Vec<String>>,
    pub built_at: Instant,
}

pub struct NodeData {
    pub path: String,
    pub degree: u32,
    pub kind: VaultKind,
}

/// The typed edge sets the vault graph unions (`vault-graph-typed-edges`).
/// Board/trail membership comes straight from the derived tables — an
/// indexed store read, never a re-parse. Freeform board cards reference no
/// note, get no `board_cards` row, and so contribute no edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum VaultEdgeKind {
    /// A resolved `[[wikilink]]` in a note body — today's scan, unchanged.
    Wikilink,
    /// Board-doc → card note, from `board_cards`. Covers sprint membership
    /// too: a sprint IS a board-doc (`sprint-board-subtype`) and derives the
    /// same rows, so sprint → story rides this kind rather than a separate
    /// visual kind — one derivation mechanism, one legend row.
    Board,
    /// Trail-doc → waypoint SOURCE note, from `trail_waypoints`.
    Trail,
    /// List-doc → member note, from `list_refs` — epic → story, plan →
    /// epic/sprint/backlog, and any registered list-like kind generally
    /// (the table is shape-generic). Phase D of the graph-unification plan.
    List,
    /// Note carrying a `[[spec:slug]]` reference → the note defining the
    /// `[slug]` anchor, resolved through the `spec_anchors` index. Both ends
    /// are vault notes; spec → *code* edges stay out (code symbols aren't
    /// vault nodes — the governance overlay's job).
    /// status: vault-graph-spec-edges
    Spec,
}

impl VaultEdgeKind {
    /// Toolbar label (`vault-graph-edge-toggles`). Named for what the edge
    /// *carries* so the row reads apart from the node-kind filter beside it.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Wikilink => "Links",
            Self::Board => "Cards",
            Self::Trail => "Waypoints",
            Self::List => "Members",
            Self::Spec => "Spec refs",
        }
    }

    /// Toggle hover text: what the edge set actually connects.
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Wikilink => "Wikilink edges (note body links)",
            Self::Board => {
                "Board-membership edges (board-doc to card note; sprints included)"
            }
            Self::Trail => "Trail-membership edges (trail-doc to waypoint source note)",
            Self::List => {
                "List-membership edges (epic/plan list-doc to member note)"
            }
            Self::Spec => {
                "Spec-reference edges ([[spec:slug]] to the note defining the anchor)"
            }
        }
    }

    /// Per-kind stroke color. `None` keeps the style's (user-editable) edge
    /// color — wikilinks stay exactly as before; membership edges take their
    /// container kind's theme hue so an edge visually belongs to its doc.
    /// List edges span two container kinds (plans and epics share the
    /// `list_refs` mechanism) and take the epic hue — the common case.
    pub const fn color(self) -> Option<egui::Color32> {
        match self {
            Self::Wikilink => None,
            Self::Board => Some(theme::kind_board()),
            Self::Trail => Some(theme::kind_trail()),
            Self::List => Some(theme::kind_epic()),
            Self::Spec => Some(theme::kind_spec()),
        }
    }

    /// Stable discriminant for the persisted hidden-kind list
    /// (`graph-view-state-persist`).
    pub const fn persist_str(self) -> &'static str {
        match self {
            Self::Wikilink => "wikilink",
            Self::Board => "board",
            Self::Trail => "trail",
            Self::List => "list",
            Self::Spec => "spec",
        }
    }
}

/// A node's type, classified from its `hiker.kind` frontmatter (read off the
/// `note_meta` index, `vault-graph-kind-nodes`) plus the spec-anchor index
/// (`vault-graph-spec-edges`). Container kinds — machinery docs
/// (board/trail/query), registry containers (plan/epic/sprint), spec notes —
/// are square, larger-labelled, hued; stories are typed LEAVES (hued
/// circles); everything else renders as a plain note. Declared in display
/// order (containers first) so the data-driven filter rows sort naturally.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum VaultKind {
    Board,
    Trail,
    Query,
    /// The registered list-like `plan` kind — the PM root container
    /// (`core::pm::PLAN_KIND` carries name-special semantics, so it earns
    /// its own hue apart from epics).
    Plan,
    /// Any other registered LIST-LIKE kind — `epic` and user list-likes
    /// (the shape is what makes a note a structural container; the epic
    /// label names the canonical case).
    Epic,
    /// Any registered BOARD-LIKE kind — `sprint` and user board-likes.
    Sprint,
    /// A note defining `[slug]` spec anchors (the spec-anchor index), when
    /// no explicit kind claims it first. The vault registry ships no spec
    /// kind, so anchor-definition is the honest in-vault signal for "this
    /// note is a spec". status: vault-graph-spec-edges
    Spec,
    /// The registered leaf kinds named `story`/`task` (two names, one
    /// definition per `kind-builtin-pm-set`) — typed PM leaves: hued, but
    /// circles. Other leaf kinds are typed plain notes with no structural
    /// role and stay [`Self::Note`].
    Story,
    Note,
}

impl VaultKind {
    /// Classify a note: machinery discriminators first (`board`/`trail`/
    /// `query` are parse-gate discriminators, not registry kinds), then the
    /// registry by SHAPE — any board-like kind is a sprint-class container,
    /// any list-like kind is plan (the name-special root) or epic-class,
    /// the story/task leaf pair is a typed work leaf — then spec-anchor
    /// definition promotes an otherwise-plain note to [`Self::Spec`].
    /// Unregistered `hiker.kind` values (waypoints, sessions, captures,
    /// user strings nothing registered) and kindless notes are plain notes.
    /// status: vault-graph-kind-nodes
    pub fn classify(kind: Option<&str>, defines_anchors: bool, registry: &Registry) -> Self {
        let explicit = match kind {
            Some("board") => Some(Self::Board),
            Some("trail") => Some(Self::Trail),
            Some("query") => Some(Self::Query),
            Some(name) => registry.get(name).map(|k| match k.shape {
                Shape::BoardLike => Self::Sprint,
                Shape::ListLike if name == hiker_core::pm::PLAN_KIND => Self::Plan,
                Shape::ListLike => Self::Epic,
                Shape::Leaf if matches!(name, "story" | "task") => Self::Story,
                Shape::Leaf => Self::Note,
            }),
            None => None,
        };
        match explicit {
            Some(k) if k != Self::Note => k,
            // An explicit kind that classifies plain (or none at all) can
            // still be a spec note — anchor definition is the weaker signal,
            // never overriding a stronger type.
            _ if defines_anchors => Self::Spec,
            _ => Self::Note,
        }
    }

    /// Container kinds get the square + larger-label treatment the cluster
    /// graph uses for high-level nodes, and survive the coarse detail level.
    /// Stories are typed leaves — hued, but neither square nor coarse-level.
    pub const fn is_container(self) -> bool {
        !matches!(self, Self::Story | Self::Note)
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Board => "Boards",
            Self::Trail => "Trails",
            Self::Query => "Queries",
            Self::Plan => "Plans",
            Self::Epic => "Epics",
            Self::Sprint => "Sprints",
            Self::Spec => "Specs",
            Self::Story => "Stories",
            Self::Note => "Notes",
        }
    }

    /// Per-kind node fill from the theme. `None` keeps the engine's flat
    /// (user-editable) node color — plain notes are untouched.
    pub const fn color(self) -> Option<egui::Color32> {
        match self {
            Self::Board => Some(theme::kind_board()),
            Self::Trail => Some(theme::kind_trail()),
            Self::Query => Some(theme::kind_query()),
            Self::Plan => Some(theme::kind_plan()),
            Self::Epic => Some(theme::kind_epic()),
            Self::Sprint => Some(theme::kind_sprint()),
            Self::Spec => Some(theme::kind_spec()),
            Self::Story => Some(theme::kind_story()),
            Self::Note => None,
        }
    }

    /// Stable discriminant for the persisted hidden-kind list
    /// (`graph-view-state-persist`).
    pub const fn persist_str(self) -> &'static str {
        match self {
            Self::Board => "board",
            Self::Trail => "trail",
            Self::Query => "query",
            Self::Plan => "plan",
            Self::Epic => "epic",
            Self::Sprint => "sprint",
            Self::Spec => "spec",
            Self::Story => "story",
            Self::Note => "note",
        }
    }
}

/// The vault graph's coarse detail dial (`vault-graph-lod-containers`):
/// "Containers" shows only the container kinds (the vault analogue of the
/// code graph's structural-objects default), "Everything" shows all notes.
/// No new LOD machinery — it falls straight out of the kind map.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Detail {
    Containers,
    #[default]
    Everything,
}

impl Detail {
    /// Whether a node of `kind` survives this detail level.
    pub const fn shows(self, kind: VaultKind) -> bool {
        match self {
            Self::Everything => true,
            Self::Containers => kind.is_container(),
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Containers => "Containers",
            Self::Everything => "Everything",
        }
    }

    pub const fn persist_str(self) -> &'static str {
        match self {
            Self::Containers => "containers",
            Self::Everything => "everything",
        }
    }

    /// Junk (incl. the pre-feature empty string) falls back to Everything.
    pub fn from_persist_str(s: &str) -> Self {
        if s == "containers" { Self::Containers } else { Self::Everything }
    }
}

/// Incremental builder for a [`VaultData`]: seed the node set (with kinds +
/// the slug → defining-notes spec-anchor map), then feed each edge source —
/// wikilink/spec-link bodies, `board_cards` rows, `trail_waypoints` rows,
/// `list_refs` rows — and `finish`. Pure (no I/O): the caller reads files /
/// queries the store and hands the results in, so the union is
/// unit-testable. status: vault-graph-typed-edges
pub struct Assembler {
    graph: DiGraph<NodeData, VaultEdgeKind>,
    by_path: HashMap<String, NodeIndex>,
    /// basename (lowercase, no extension) → rel path. Last writer wins.
    by_basename: HashMap<String, String>,
    /// `[slug]` anchor → its defining note paths (sorted, the store's
    /// order), for `[[spec:slug]]` resolution. status: vault-graph-spec-edges
    anchors: HashMap<String, Vec<String>>,
    /// Dedupe set for the membership/spec edge sets (a note carded twice on
    /// one board is one edge). Wikilinks keep their historical duplicates.
    seen: HashSet<(NodeIndex, NodeIndex, VaultEdgeKind)>,
}

impl Assembler {
    /// One node per vault path, typed by the `kinds` map (a path missing
    /// from the map — unindexed yet — classifies as a plain note).
    /// `anchors` is the spec-anchor index's slug → defining-paths map
    /// (empty when the vault defines none).
    pub fn new(
        paths: &[String],
        kinds: &HashMap<String, VaultKind>,
        anchors: HashMap<String, Vec<String>>,
    ) -> Self {
        let mut graph: DiGraph<NodeData, VaultEdgeKind> =
            DiGraph::with_capacity(paths.len(), paths.len() * 2);
        let mut by_path = HashMap::with_capacity(paths.len());
        let mut by_basename = HashMap::with_capacity(paths.len());
        for p in paths {
            let idx = graph.add_node(NodeData {
                path: p.clone(),
                degree: 0,
                kind: kinds.get(p).copied().unwrap_or(VaultKind::Note),
            });
            by_path.insert(p.clone(), idx);
            by_basename.insert(basename(p).to_lowercase(), p.clone());
        }
        Self { graph, by_path, by_basename, anchors, seen: HashSet::new() }
    }

    /// Scan `body` for `[[…]]` links: plain wikilinks add one Wikilink edge
    /// per resolved, non-self link (today's behaviour — duplicates
    /// included); a `[[spec:slug]]` target resolves through the spec-anchor
    /// map to a deduped Spec edge (`vault-graph-spec-edges`); a
    /// `[[code:…]]` target adds nothing — code symbols aren't vault nodes,
    /// and letting one fall through to basename resolution would forge a
    /// wikilink edge to an unrelated note sharing the symbol's leaf name.
    pub fn add_wikilinks(&mut self, path: &str, body: &str) {
        let Some(&src) = self.by_path.get(path) else { return };
        for target in scan_wikilinks(body) {
            if let Some(slug) = wikilink::parse_spec_target(&target) {
                if let Some(rel) = pick_spec_anchor(self.anchors.get(slug), path)
                    && let Some(&dst) = self.by_path.get(&rel)
                {
                    self.add_edge(src, dst, VaultEdgeKind::Spec, true);
                }
                continue;
            }
            if wikilink::parse_code_target(&target).is_some() {
                continue;
            }
            if let Some(rel) = self.resolve_target(&target)
                && let Some(&dst) = self.by_path.get(&rel)
            {
                self.add_edge(src, dst, VaultEdgeKind::Wikilink, false);
            }
        }
    }

    /// Board membership: board-doc → card note, one (deduped) edge per
    /// distinct pair. Freeform cards never reach `board_cards`, so every row
    /// references a real note path; rows whose board or note isn't in the
    /// walk (e.g. deleted since the index wrote them) are skipped.
    pub fn add_board_cards(&mut self, rows: &[BoardCardRow]) {
        for row in rows {
            if let (Some(&src), Some(&dst)) = (
                self.by_path.get(&row.board_path),
                self.by_path.get(&row.card_note_path),
            ) {
                self.add_edge(src, dst, VaultEdgeKind::Board, true);
            }
        }
    }

    /// Trail membership: trail-doc → the waypoint's SOURCE note. Under
    /// path-as-identity the row's `trail_id` IS the trail-doc's vault path;
    /// `source_path` is the real note the waypoint captures (the
    /// `waypoint_path` companion snapshot is an internal pointer, not a
    /// graph node worth an edge). Deduped per (trail, source).
    pub fn add_trail_waypoints(&mut self, rows: &[WaypointRow]) {
        for row in rows {
            if let (Some(&src), Some(&dst)) = (
                self.by_path.get(&row.trail_id),
                self.by_path.get(&row.source_path),
            ) {
                self.add_edge(src, dst, VaultEdgeKind::Trail, true);
            }
        }
    }

    /// List membership: list-doc → member note, one (deduped) edge per
    /// distinct pair — epics, plans, and any registered list-like kind ride
    /// the same `list_refs` table (`pm-epic-derived-table` is shape-generic,
    /// so this union arm is too). Rows whose list or member isn't in the
    /// walk are skipped, the board posture.
    pub fn add_list_refs(&mut self, rows: &[ListRefRow]) {
        for row in rows {
            if let (Some(&src), Some(&dst)) = (
                self.by_path.get(&row.list_path),
                self.by_path.get(&row.member_path),
            ) {
                self.add_edge(src, dst, VaultEdgeKind::List, true);
            }
        }
    }

    /// Freeze into a [`VaultData`] with the cached edge list and the
    /// note → anchors inversion of the spec-anchor map.
    pub fn finish(self) -> VaultData {
        let graph = self.graph;
        let edges: Vec<(u32, u32, VaultEdgeKind)> = graph
            .edge_indices()
            .filter_map(|e| {
                graph
                    .edge_endpoints(e)
                    .map(|(a, b)| (a.index() as u32, b.index() as u32, graph[e]))
            })
            .collect();
        let mut anchors_by_note: HashMap<String, Vec<String>> = HashMap::new();
        for (slug, paths) in &self.anchors {
            for p in paths {
                anchors_by_note.entry(p.clone()).or_default().push(slug.clone());
            }
        }
        for slugs in anchors_by_note.values_mut() {
            slugs.sort_unstable();
        }
        VaultData { graph, edges, anchors_by_note, built_at: Instant::now() }
    }

    /// Add `src → dst`, bump both degrees. Self-edges never; `dedupe`
    /// collapses repeats (the membership sets) while wikilinks keep theirs.
    fn add_edge(&mut self, src: NodeIndex, dst: NodeIndex, kind: VaultEdgeKind, dedupe: bool) {
        if src == dst || (dedupe && !self.seen.insert((src, dst, kind))) {
            return;
        }
        self.graph.add_edge(src, dst, kind);
        self.graph[src].degree += 1;
        self.graph[dst].degree += 1;
    }

    /// Map a wikilink target to an existing vault rel-path.
    fn resolve_target(&self, target: &str) -> Option<String> {
        if self.by_path.contains_key(target) {
            return Some(target.to_string());
        }
        let with_md = format!("{target}.md");
        if self.by_path.contains_key(&with_md) {
            return Some(with_md);
        }
        let leaf = target.rsplit('/').next().unwrap_or(target);
        let key = leaf.strip_suffix(".md").unwrap_or(leaf).to_lowercase();
        self.by_basename.get(&key).cloned()
    }
}

/// Scan `body` for `[[Target]]` / `[[Target|Alias]]`, returning targets.
fn scan_wikilinks(body: &str) -> Vec<String> {
    let bytes = body.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'[' && bytes[i + 1] == b'[' {
            let start = i + 2;
            let mut j = start;
            let mut found = None;
            while j + 1 < bytes.len() {
                let c = bytes[j];
                if c == b'\n' || c == b']' && bytes[j + 1] == b']' {
                    if c == b']' {
                        found = Some(j);
                    }
                    break;
                }
                j += 1;
            }
            if let Some(end) = found {
                let span = &body[start..end];
                let target = match span.find('|') {
                    Some(p) => &span[..p],
                    None => span,
                };
                let trimmed = target.trim();
                if !trimmed.is_empty() {
                    out.push(trimmed.to_string());
                }
                i = end + 2;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Pick the note a `[[spec:slug]]` reference resolves to when the slug is
/// defined in more than one note: a definer sharing the referrer's parent
/// folder wins, else the first (the store hands them sorted) — the same
/// deterministic rule the editor's spec-link click uses
/// (`wikilink-spec-links`), so the graph edge and the click never disagree.
/// status: vault-graph-spec-edges
fn pick_spec_anchor(paths: Option<&Vec<String>>, referrer: &str) -> Option<String> {
    let paths = paths?;
    let dir = referrer.rsplit_once('/').map_or("", |(d, _)| d);
    paths
        .iter()
        .find(|p| p.rsplit_once('/').map_or("", |(d, _)| d) == dir)
        .or_else(|| paths.first())
        .cloned()
}

/// A query-doc scope over the vault graph (`graph-scoped-query`): the doc,
/// its display title, and the per-rebuild execution outcome — the member
/// path set, or the loud parse/run error (the smart-folder posture: an
/// error is surfaced, never a silent empty or match-everything fallback).
/// Pure data; the panel runs the query and hands the result in.
pub struct ScopeState {
    /// The query-doc's vault-relative path.
    pub path: String,
    /// Display title (the doc's filename stem).
    pub title: String,
    /// Member paths in the scope's universe, or the query error.
    pub result: Result<HashSet<String>, String>,
}

/// Restrict a base visibility mask to a query scope's node universe
/// (`graph-scoped-query`): a node survives when the base mask keeps it AND
/// its path is a scope member. Orthogonal to hops focus by construction —
/// the scope shapes the universe (this mask feeds [`focus_nodes`]' `base`),
/// the focus drills within it.
pub fn restrict_to_scope(data: &VaultData, base: &[bool], members: &HashSet<String>) -> Vec<bool> {
    data.graph
        .node_weights()
        .zip(base)
        .map(|(n, &on)| on && members.contains(&n.path))
        .collect()
}

/// The display mask for a FAILED scope query: only the query-doc itself —
/// the graph analogue of the smart folder's header + error row (the doc node
/// stays as the click target to open and fix the doc; the toolbar carries
/// the loud error text). status: graph-scoped-query
pub fn scope_error_mask(data: &VaultData, query_path: &str) -> Vec<bool> {
    data.graph.node_weights().map(|n| n.path == query_path).collect()
}

/// The node-kind filter rows for `data`: every distinct kind present, in
/// display order, all visible — the filter section offers exactly what the
/// data contains, nothing hardcoded (mirrors the code graph's
/// `kind_filter_for`). status: vault-graph-kind-filters
pub fn kind_filter_for(data: &VaultData) -> Vec<(VaultKind, bool)> {
    let mut kinds: Vec<VaultKind> = data.graph.node_weights().map(|n| n.kind).collect();
    kinds.sort_unstable();
    kinds.dedup();
    kinds.into_iter().map(|k| (k, true)).collect()
}

/// The edge-kind toggle rows for `data`: every distinct edge kind present,
/// in declaration order, all visible. A vault with no boards offers no dead
/// "Cards" toggle. status: vault-graph-edge-toggles
pub fn edge_filter_for(data: &VaultData) -> Vec<(VaultEdgeKind, bool)> {
    let mut kinds: Vec<VaultEdgeKind> = data.edges.iter().map(|&(_, _, k)| k).collect();
    kinds.sort_unstable();
    kinds.dedup();
    kinds.into_iter().map(|k| (k, true)).collect()
}

/// Carry the user's on/off choices across a rebuild: `fresh` is the
/// data-derived row set (all on); an entry that already existed keeps its
/// old state, a kind first appearing defaults to visible.
pub fn merge_filter<K: Copy + PartialEq>(
    old: &[(K, bool)],
    fresh: Vec<(K, bool)>,
) -> Vec<(K, bool)> {
    fresh
        .into_iter()
        .map(|(k, on)| {
            let kept = old.iter().find(|(ok, _)| *ok == k).map_or(on, |&(_, o)| o);
            (k, kept)
        })
        .collect()
}

/// Per-node visibility under the kind filter + the detail dial. Indices are
/// graph node indices (stable, so hidden nodes keep their layout slots). A
/// kind missing from the filter rows defaults to visible.
/// status: vault-graph-kind-filters, vault-graph-lod-containers
pub fn visible_nodes(
    data: &VaultData,
    kind_filter: &[(VaultKind, bool)],
    detail: Detail,
) -> Vec<bool> {
    let kind_on = |kind: VaultKind| {
        kind_filter.iter().find(|(k, _)| *k == kind).is_none_or(|&(_, on)| on)
    };
    data.graph
        .node_weights()
        .map(|n| detail.shows(n.kind) && kind_on(n.kind))
        .collect()
}

/// The drawn/laid-out edge list: an edge survives when its kind toggle is on
/// AND both endpoints are visible — a membership edge never dangles into a
/// hidden node. status: vault-graph-edge-toggles
pub fn visible_edges(
    data: &VaultData,
    edge_filter: &[(VaultEdgeKind, bool)],
    visible: &[bool],
) -> Vec<(u32, u32, VaultEdgeKind)> {
    let kind_on = |kind: VaultEdgeKind| {
        edge_filter.iter().find(|(k, _)| *k == kind).is_none_or(|&(_, on)| on)
    };
    data.edges
        .iter()
        .filter(|&&(a, b, k)| {
            kind_on(k)
                && visible.get(a as usize).copied().unwrap_or(false)
                && visible.get(b as usize).copied().unwrap_or(false)
        })
        .copied()
        .collect()
}

/// The focus-mode visibility mask (`graph-nav-extract`, vault side): the
/// depth-bounded undirected neighbourhood of `focus_path` over the typed
/// edge union, respecting the edge-kind toggles — a toggled-off kind carries
/// no reachability, so hiding board edges keeps a board's cards out of its
/// neighbourhood — and walking only the DRAWN topology (edges whose
/// endpoints survive `base`, the kind-filter + detail mask), so the
/// neighbourhood never includes nodes floating free of any visible edge.
/// The anchor itself always shows, even when its own kind is filtered out.
/// `None` when `focus_path` isn't in the graph (a stale focus) — the caller
/// falls back to the overview display.
pub fn focus_nodes(
    data: &VaultData,
    edge_filter: &[(VaultEdgeKind, bool)],
    base: &[bool],
    focus_path: &str,
    depth: u8,
) -> Option<Vec<bool>> {
    let focus = data
        .graph
        .node_indices()
        .find(|&i| data.graph[i].path == focus_path)?
        .index();
    let mut base_plus = base.to_vec();
    if let Some(slot) = base_plus.get_mut(focus) {
        *slot = true;
    }
    let kept = visible_edges(data, edge_filter, &base_plus);
    Some(crate::panels::graph_nav::hop_mask(
        base_plus.len(),
        kept.iter().map(|&(a, b, _)| (a as usize, b as usize)),
        focus,
        depth as usize,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(board: &str, note: &str, column: &str) -> BoardCardRow {
        BoardCardRow {
            board_id: board.to_string(),
            board_path: board.to_string(),
            card_note_path: note.to_string(),
            column_name: column.to_string(),
            ordinal: 0,
        }
    }

    fn waypoint(trail: &str, source: &str, id: &str) -> WaypointRow {
        WaypointRow {
            waypoint_path: format!(".hiker/trails/{id}.md"),
            waypoint_id: id.to_string(),
            trail_id: trail.to_string(),
            source_path: source.to_string(),
            parent_waypoint_id: None,
            tree_path: "1".to_string(),
        }
    }

    fn list_ref(list: &str, member: &str, position: i64) -> ListRefRow {
        ListRefRow {
            list_path: list.to_string(),
            member_path: member.to_string(),
            position,
        }
    }

    /// A small typed vault: one board, one trail, one query, two plain notes.
    fn built() -> VaultData {
        let paths: Vec<String> = ["boards/b.md", "trails/t.md", "q.md", "notes/a.md", "notes/c.md"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let kinds: HashMap<String, VaultKind> = [
            ("boards/b.md", VaultKind::Board),
            ("trails/t.md", VaultKind::Trail),
            ("q.md", VaultKind::Query),
        ]
        .into_iter()
        .map(|(p, k)| (p.to_string(), k))
        .collect();
        let mut asm = Assembler::new(&paths, &kinds, HashMap::new());
        asm.add_wikilinks("notes/a.md", "see [[c]] and [[missing]]");
        asm.add_board_cards(&[
            card("boards/b.md", "notes/a.md", "Doing"),
            card("boards/b.md", "notes/a.md", "Done"), // dup pair → one edge
            card("boards/b.md", "gone.md", "Doing"),   // target not in walk → skipped
            card("boards/b.md", "boards/b.md", "Doing"), // self → skipped
        ]);
        asm.add_trail_waypoints(&[
            waypoint("trails/t.md", "notes/c.md", "w1"),
            waypoint("trails/t.md", "notes/c.md", "w2"), // dup pair → one edge
            waypoint("stale/t2.md", "notes/c.md", "w3"), // trail-doc not in walk → skipped
        ]);
        asm.finish()
    }

    /// A PM + spec vault: a plan listing an epic and a sprint, the epic
    /// listing a story, the sprint carding the story, and a spec doc whose
    /// anchor two notes reference.
    fn pm_built() -> VaultData {
        let paths: Vec<String> = [
            "pm/plan.md",
            "pm/epic.md",
            "pm/sprint.md",
            "work/s1.md",
            "docs/spec.md",
            "notes/n.md",
        ]
        .iter()
        .map(ToString::to_string)
        .collect();
        let kinds: HashMap<String, VaultKind> = [
            ("pm/plan.md", VaultKind::Plan),
            ("pm/epic.md", VaultKind::Epic),
            ("pm/sprint.md", VaultKind::Sprint),
            ("work/s1.md", VaultKind::Story),
            ("docs/spec.md", VaultKind::Spec),
        ]
        .into_iter()
        .map(|(p, k)| (p.to_string(), k))
        .collect();
        let anchors: HashMap<String, Vec<String>> =
            [("x-feature".to_string(), vec!["docs/spec.md".to_string()])]
                .into_iter()
                .collect();
        let mut asm = Assembler::new(&paths, &kinds, anchors);
        asm.add_list_refs(&[
            list_ref("pm/plan.md", "pm/epic.md", 0),
            list_ref("pm/plan.md", "pm/sprint.md", 1),
            list_ref("pm/epic.md", "work/s1.md", 0),
            list_ref("pm/epic.md", "work/s1.md", 0), // dup pair → one edge
            list_ref("pm/epic.md", "gone.md", 1),    // member not in walk → skipped
            list_ref("stale/e2.md", "work/s1.md", 0), // list not in walk → skipped
        ]);
        asm.add_board_cards(&[card("pm/sprint.md", "work/s1.md", "Doing")]);
        asm.add_wikilinks(
            "notes/n.md",
            "see [[spec:x-feature]] twice [[spec:x-feature]], a dead \
             [[spec:no-such-slug]], and [[code:hiker/spec]] (never a node)",
        );
        asm.add_wikilinks("work/s1.md", "implements [[spec:x-feature]]");
        asm.add_wikilinks("docs/spec.md", "self [[spec:x-feature]]"); // self → skipped
        asm.finish()
    }

    fn edge_set(data: &VaultData) -> Vec<(String, String, VaultEdgeKind)> {
        data.edges
            .iter()
            .map(|&(a, b, k)| {
                (
                    data.graph[petgraph::graph::NodeIndex::new(a as usize)].path.clone(),
                    data.graph[petgraph::graph::NodeIndex::new(b as usize)].path.clone(),
                    k,
                )
            })
            .collect()
    }

    /// The union: one wikilink edge (unresolved targets dropped), one deduped
    /// board-membership edge to the card's note, one deduped trail-membership
    /// edge to the waypoint's SOURCE note (never the snapshot pointer), and
    /// rows referencing paths outside the walk are skipped.
    #[test]
    fn union_builds_typed_edges_from_all_three_sources() {
        let data = built();
        let edges = edge_set(&data);
        assert_eq!(
            edges,
            vec![
                ("notes/a.md".into(), "notes/c.md".into(), VaultEdgeKind::Wikilink),
                ("boards/b.md".into(), "notes/a.md".into(), VaultEdgeKind::Board),
                ("trails/t.md".into(), "notes/c.md".into(), VaultEdgeKind::Trail),
            ]
        );
        // Degrees count the union (a: wikilink + board card = 2).
        let degree_of = |path: &str| {
            data.graph
                .node_weights()
                .find(|n| n.path == path)
                .map(|n| n.degree)
                .unwrap()
        };
        assert_eq!(degree_of("notes/a.md"), 2);
        assert_eq!(degree_of("notes/c.md"), 2);
        assert_eq!(degree_of("q.md"), 0, "query doc with no members stays an orphan");
    }

    /// Phase D + E union arms: deduped list-membership edges from
    /// `list_refs` (plan → epic/sprint, epic → story — rows referencing
    /// paths outside the walk skipped), sprint → story riding the BOARD
    /// edge kind (a sprint is a board-doc; no separate visual kind), and
    /// deduped `[[spec:slug]]` edges resolved through the anchor map
    /// (unresolved slugs dropped, self-references skipped, `[[code:…]]`
    /// targets never resolve to a node even when a note shares the
    /// symbol's leaf name).
    #[test]
    fn union_adds_list_membership_and_spec_reference_edges() {
        let data = pm_built();
        let edges = edge_set(&data);
        assert_eq!(
            edges,
            vec![
                ("pm/plan.md".into(), "pm/epic.md".into(), VaultEdgeKind::List),
                ("pm/plan.md".into(), "pm/sprint.md".into(), VaultEdgeKind::List),
                ("pm/epic.md".into(), "work/s1.md".into(), VaultEdgeKind::List),
                ("pm/sprint.md".into(), "work/s1.md".into(), VaultEdgeKind::Board),
                ("notes/n.md".into(), "docs/spec.md".into(), VaultEdgeKind::Spec),
                ("work/s1.md".into(), "docs/spec.md".into(), VaultEdgeKind::Spec),
            ]
        );
        // The note → anchors inversion feeds the drift-badge fold.
        assert_eq!(
            data.anchors_by_note.get("docs/spec.md"),
            Some(&vec!["x-feature".to_string()])
        );
        assert!(!data.anchors_by_note.contains_key("notes/n.md"));
    }

    /// A multi-defined spec anchor resolves like the editor click: the
    /// definer sharing the referrer's folder wins, else the first.
    #[test]
    fn spec_anchor_pick_prefers_the_referrers_folder() {
        let paths = vec!["docs/a.md".to_string(), "guides/b.md".to_string()];
        assert_eq!(
            pick_spec_anchor(Some(&paths), "guides/reader.md").as_deref(),
            Some("guides/b.md")
        );
        assert_eq!(
            pick_spec_anchor(Some(&paths), "elsewhere/reader.md").as_deref(),
            Some("docs/a.md")
        );
        assert_eq!(pick_spec_anchor(None, "x.md"), None);
    }

    /// A registry with three custom kinds, one per shape — the
    /// shape-genericity probe for classification.
    fn custom_registry() -> Registry {
        let table: toml::value::Table = toml::from_str(
            "[milestone]\nshape = \"list-like\"\n\
             [qa-board]\nshape = \"board-like\"\n\
             [recipe]\nshape = \"leaf\"\n",
        )
        .unwrap();
        let entries: std::collections::BTreeMap<String, toml::Value> =
            table.into_iter().collect();
        Registry::compile(&entries).unwrap()
    }

    /// Classification reads the machinery discriminators, then the
    /// REGISTRY'S SHAPES — never a hardcoded name list: the built-in PM set
    /// maps (sprint board-like, plan/epic list-like, story+task the work
    /// leaves), custom kinds bucket by shape (a custom list-like classifies
    /// with epics, a custom board-like with sprints, a custom leaf is a
    /// typed plain note), and an UNREGISTERED "epic" string is a plain note
    /// (the registry, not the name, is the source).
    #[test]
    fn kind_classification_reads_registry_shapes() {
        use hiker_core::kinds::builtin_registry;
        let builtins = builtin_registry();
        let of = |k: Option<&str>| VaultKind::classify(k, false, &builtins);
        assert_eq!(of(Some("board")), VaultKind::Board);
        assert_eq!(of(Some("trail")), VaultKind::Trail);
        assert_eq!(of(Some("query")), VaultKind::Query);
        assert_eq!(of(Some("plan")), VaultKind::Plan);
        assert_eq!(of(Some("epic")), VaultKind::Epic);
        assert_eq!(of(Some("sprint")), VaultKind::Sprint);
        assert_eq!(of(Some("story")), VaultKind::Story);
        assert_eq!(of(Some("task")), VaultKind::Story);
        assert_eq!(of(Some("waypoint")), VaultKind::Note);
        assert_eq!(of(None), VaultKind::Note);

        let custom = custom_registry();
        let of = |k: Option<&str>| VaultKind::classify(k, false, &custom);
        assert_eq!(of(Some("milestone")), VaultKind::Epic, "list-like shape, not the name");
        assert_eq!(of(Some("qa-board")), VaultKind::Sprint, "board-like shape");
        assert_eq!(of(Some("recipe")), VaultKind::Note, "a leaf kind is a typed plain note");
        assert_eq!(of(Some("epic")), VaultKind::Note, "unregistered name stays plain");

        // Containers: every typed kind but the two leaf classes.
        assert!(VaultKind::Plan.is_container() && VaultKind::Sprint.is_container());
        assert!(VaultKind::Spec.is_container());
        assert!(!VaultKind::Story.is_container() && !VaultKind::Note.is_container());
    }

    /// Spec-anchor definition promotes an otherwise-plain note to Spec —
    /// and never overrides a stronger explicit kind.
    #[test]
    fn kind_classification_spec_promotion_is_the_weaker_signal() {
        let builtins = hiker_core::kinds::builtin_registry();
        let of = |k: Option<&str>, anchors| VaultKind::classify(k, anchors, &builtins);
        assert_eq!(of(None, true), VaultKind::Spec);
        assert_eq!(of(Some("waypoint"), true), VaultKind::Spec, "inert kind stays promotable");
        assert_eq!(of(Some("board"), true), VaultKind::Board, "machinery kind wins");
        assert_eq!(of(Some("story"), true), VaultKind::Story, "registered kind wins");
        assert_eq!(of(None, false), VaultKind::Note);
    }

    /// Filter rows auto-populate from the data — kinds present, all visible —
    /// and a merge across rebuilds keeps the user's offs while new kinds
    /// default on.
    #[test]
    fn filters_autopopulate_and_merge_keeps_choices() {
        let data = built();
        let kinds: Vec<VaultKind> = kind_filter_for(&data).into_iter().map(|(k, _)| k).collect();
        assert_eq!(
            kinds,
            vec![VaultKind::Board, VaultKind::Trail, VaultKind::Query, VaultKind::Note]
        );
        let edge_kinds: Vec<VaultEdgeKind> =
            edge_filter_for(&data).into_iter().map(|(k, _)| k).collect();
        assert_eq!(
            edge_kinds,
            vec![VaultEdgeKind::Wikilink, VaultEdgeKind::Board, VaultEdgeKind::Trail]
        );

        let old = vec![(VaultKind::Board, false)];
        let merged = merge_filter(&old, kind_filter_for(&data));
        assert!(!merged.iter().find(|(k, _)| *k == VaultKind::Board).unwrap().1, "off survives");
        assert!(merged.iter().find(|(k, _)| *k == VaultKind::Note).unwrap().1, "new defaults on");
    }

    /// The coarse detail level keeps container kinds only; "Everything" keeps
    /// all notes — the LOD mapping is just the kind map.
    #[test]
    fn detail_levels_map_through_kinds() {
        let data = built();
        let filter = kind_filter_for(&data);
        let coarse = visible_nodes(&data, &filter, Detail::Containers);
        let shown: Vec<&str> = data
            .graph
            .node_weights()
            .zip(&coarse)
            .filter(|&(_, &v)| v)
            .map(|(n, _)| n.path.as_str())
            .collect();
        assert_eq!(shown, vec!["boards/b.md", "trails/t.md", "q.md"]);
        let all = visible_nodes(&data, &filter, Detail::Everything);
        assert!(all.iter().all(|&v| v));
    }

    /// An edge draws only when its kind toggle is on and both endpoints are
    /// visible — hiding plain notes also drops the membership edges into them.
    #[test]
    fn visible_edges_honor_toggles_and_endpoints() {
        let data = built();
        let kind_filter = kind_filter_for(&data);
        let all_nodes = visible_nodes(&data, &kind_filter, Detail::Everything);

        // Toggle off board edges: the board-membership edge disappears.
        let toggles = vec![
            (VaultEdgeKind::Wikilink, true),
            (VaultEdgeKind::Board, false),
            (VaultEdgeKind::Trail, true),
        ];
        let kinds: Vec<VaultEdgeKind> = visible_edges(&data, &toggles, &all_nodes)
            .into_iter()
            .map(|(_, _, k)| k)
            .collect();
        assert_eq!(kinds, vec![VaultEdgeKind::Wikilink, VaultEdgeKind::Trail]);

        // Coarse detail hides plain notes → every edge into them goes too.
        let coarse = visible_nodes(&data, &kind_filter, Detail::Containers);
        let edge_filter = edge_filter_for(&data);
        assert!(visible_edges(&data, &edge_filter, &coarse).is_empty());
    }

    /// Names of the nodes a focus mask keeps, in node order.
    fn shown(data: &VaultData, mask: &[bool]) -> Vec<String> {
        data.graph
            .node_weights()
            .zip(mask)
            .filter(|&(_, &v)| v)
            .map(|(n, _)| n.path.clone())
            .collect()
    }

    /// The focus neighbourhood is depth-bounded BFS over the typed edge
    /// union: 1 hop from the board reaches its card; 2 hops follow the
    /// card's wikilink; 3 hops cross to the trail-doc through its waypoint.
    #[test]
    fn focus_neighbourhood_bounds_depth_over_typed_edges() {
        let data = built();
        let edges = edge_filter_for(&data);
        let base = visible_nodes(&data, &kind_filter_for(&data), Detail::Everything);
        let at = |d| shown(&data, &focus_nodes(&data, &edges, &base, "boards/b.md", d).unwrap());
        assert_eq!(at(1), vec!["boards/b.md", "notes/a.md"]);
        assert_eq!(at(2), vec!["boards/b.md", "notes/a.md", "notes/c.md"]);
        assert_eq!(at(3), vec!["boards/b.md", "trails/t.md", "notes/a.md", "notes/c.md"]);
    }

    /// A toggled-off edge kind carries no reachability: with board edges off
    /// the board's neighbourhood is just itself; with wikilinks off the
    /// 2-hop walk stops at the card.
    #[test]
    fn focus_neighbourhood_respects_edge_kind_toggles() {
        let data = built();
        let base = visible_nodes(&data, &kind_filter_for(&data), Detail::Everything);
        let boards_off = vec![
            (VaultEdgeKind::Wikilink, true),
            (VaultEdgeKind::Board, false),
            (VaultEdgeKind::Trail, true),
        ];
        let mask = focus_nodes(&data, &boards_off, &base, "boards/b.md", 2).unwrap();
        assert_eq!(shown(&data, &mask), vec!["boards/b.md"]);
        let links_off = vec![
            (VaultEdgeKind::Wikilink, false),
            (VaultEdgeKind::Board, true),
            (VaultEdgeKind::Trail, true),
        ];
        let mask = focus_nodes(&data, &links_off, &base, "boards/b.md", 2).unwrap();
        assert_eq!(shown(&data, &mask), vec!["boards/b.md", "notes/a.md"]);
    }

    /// The anchor always shows, even when its own kind is filtered out (a
    /// neighbourhood without its centre would read as a bug), and a stale
    /// focus path yields `None` (the caller falls back to the overview).
    #[test]
    fn focus_anchor_survives_kind_filter_and_stale_focus_falls_back() {
        let data = built();
        let edges = edge_filter_for(&data);
        let boards_hidden = vec![
            (VaultKind::Board, false),
            (VaultKind::Trail, true),
            (VaultKind::Query, true),
            (VaultKind::Note, true),
        ];
        let base = visible_nodes(&data, &boards_hidden, Detail::Everything);
        let mask = focus_nodes(&data, &edges, &base, "boards/b.md", 1).unwrap();
        assert_eq!(shown(&data, &mask), vec!["boards/b.md", "notes/a.md"]);
        assert!(focus_nodes(&data, &edges, &base, "gone.md", 2).is_none());
    }

    /// Query scope and hops focus COMPOSE (`graph-scoped-query`): the scope
    /// restricts the node universe, and a focus walk inside it follows only
    /// edges whose both ends survive the scope — an out-of-scope node is
    /// unreachable at any depth even when a typed edge points at it.
    #[test]
    fn scope_restricts_the_universe_and_focus_drills_within_it() {
        let data = pm_built();
        let base = visible_nodes(&data, &kind_filter_for(&data), Detail::Everything);
        let members: HashSet<String> =
            ["pm/epic.md", "work/s1.md", "docs/spec.md"].iter().map(ToString::to_string).collect();
        let scoped = restrict_to_scope(&data, &base, &members);
        assert_eq!(
            shown(&data, &scoped),
            vec!["pm/epic.md", "work/s1.md", "docs/spec.md"],
            "the scope is the node universe"
        );
        // Focus within the scope: 1 hop from the epic reaches its member
        // story; 2 hops follow the story's spec edge — but never the
        // out-of-scope plan/sprint, even though list/board edges exist.
        let edges = edge_filter_for(&data);
        let at = |d| {
            shown(&data, &focus_nodes(&data, &edges, &scoped, "pm/epic.md", d).unwrap())
        };
        assert_eq!(at(1), vec!["pm/epic.md", "work/s1.md"]);
        assert_eq!(at(2), vec!["pm/epic.md", "work/s1.md", "docs/spec.md"]);
        assert_eq!(at(3), vec!["pm/epic.md", "work/s1.md", "docs/spec.md"]);
    }

    /// A failed scope query displays only the query-doc node — the
    /// smart-folder error posture (header + loud error, never a silent
    /// fallback to the full vault).
    #[test]
    fn scope_error_mask_keeps_only_the_query_doc() {
        let data = built();
        let mask = scope_error_mask(&data, "q.md");
        assert_eq!(shown(&data, &mask), vec!["q.md"]);
        assert!(scope_error_mask(&data, "gone.md").iter().all(|&v| !v));
    }

    /// The detail dial round-trips its persisted discriminant; junk and the
    /// pre-feature empty string fall back to Everything.
    #[test]
    fn detail_persist_round_trip() {
        for d in [Detail::Containers, Detail::Everything] {
            assert_eq!(Detail::from_persist_str(d.persist_str()), d);
        }
        assert_eq!(Detail::from_persist_str(""), Detail::Everything);
        assert_eq!(Detail::from_persist_str("objects"), Detail::Everything);
    }
}
