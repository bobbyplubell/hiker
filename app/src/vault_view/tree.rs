//! The derived Vault-view tree model: pure, side-effect-free construction
//! of the logical node forest the lens renders, from the store's
//! relationship/provenance projection (`NoteMetaRow`) plus the resolved
//! trail-waypoint rows (`WaypointRow`). See `docs/vault-view.md`.
//!
//! Nesting authority lives here, not in the renderer, so it can be unit
//! tested in isolation (the renderer below is a thin egui walk over the
//! forest this module returns):
//!
//! - **Crawl/feed** (`vault-view-crawl-nesting`): a child nests under the
//!   note whose id equals its `hiker.parent` stamp — *not* by companion-
//!   folder membership. A note whose parent stamp resolves to nothing is a
//!   normal top-level node, never a false child.
//! - **Trails** (`vault-view-trail-nesting`): a trail-doc's waypoints nest
//!   by the resolved `trail_waypoints` tree (`parent_waypoint_id` +
//!   `tree_path`), which the indexer re-derives from the trail-doc's
//!   `hiker.waypoints` frontmatter — preserving order and side-trail shape.
//! - **Sidecars** (`vault-view-sidecar-surfacing`): a `<src>.<ext>.md`
//!   extracted-text sidecar nests under a synthetic node for its non-md
//!   source `<src>.<ext>`, so the pair reads as one entry.
//! - **Source/provenance groups** (`vault-view-source-groups`): everything
//!   left over groups under virtual nodes by the authorship trichotomy /
//!   source kind; chat sessions land in a "Sessions" bucket.

use std::collections::{BTreeMap, HashMap, HashSet};

use hiker_core::store::dto::{NoteMetaRow, WaypointRow};

/// What a node represents — drives icon choice in the renderer and lets
/// tests assert structure without string-matching labels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeKind {
    /// A virtual grouping node with no underlying file (source-type /
    /// provenance bucket, or a synthetic non-md source for a sidecar).
    Group,
    /// A capture-spec note (`hiker.kind: capture`) acting as a crawl/feed
    /// parent.
    Capture,
    /// A trail-doc.
    Trail,
    /// A trail waypoint-note.
    Waypoint,
    /// A chat session note.
    Session,
    /// Any other ordinary note.
    Note,
}

/// One node in the derived Vault tree. `path` is the vault-relative path to
/// open on click; `None` marks a virtual group node (no file to open).
#[derive(Clone, Debug, PartialEq)]
pub struct VaultNode {
    pub label: String,
    pub path: Option<String>,
    pub kind: NodeKind,
    pub children: Vec<VaultNode>,
}

impl VaultNode {
    fn group(label: impl Into<String>, children: Vec<VaultNode>) -> Self {
        Self { label: label.into(), path: None, kind: NodeKind::Group, children }
    }
    fn leaf(path: &str, kind: NodeKind) -> Self {
        Self { label: display_label(path), path: Some(path.into()), kind, children: Vec::new() }
    }
}

/// Display label for a note path: filename with the indexable extension
/// stripped. Sessions get a title+date label upstream (see `session_label`).
fn display_label(path: &str) -> String {
    let base = path.rsplit('/').next().unwrap_or(path);
    base.strip_suffix(".md")
        .or_else(|| base.strip_suffix(".markdown"))
        .or_else(|| base.strip_suffix(".txt"))
        .unwrap_or(base)
        .to_string()
}

/// The non-md source path a `<src>.<ext>.md` extracted-text sidecar derives
/// from, or `None` when `path` isn't a sidecar. A sidecar is a `.md` whose
/// stem still carries a (non-indexable) extension: `rm0090.pdf.md` →
/// `rm0090.pdf`. The bare `.md`-on-`.md` and indexable-on-indexable cases
/// (`notes.md.md`, `readme.txt.md`) are deliberately not treated as
/// sidecars — those extensions never produce extractor sidecars.
fn sidecar_source(path: &str) -> Option<String> {
    let stem = path.strip_suffix(".md")?;
    let dot = stem.rfind('.')?;
    let inner_ext = &stem[dot + 1..];
    let slash = stem.rfind('/').map_or(0, |s| s + 1);
    // The inner extension must sit within the basename (not be a folder dot)
    // and not itself be an indexable extension.
    if dot < slash || inner_ext.is_empty() {
        return None;
    }
    let indexable = ["md", "markdown", "txt"];
    if indexable.iter().any(|e| inner_ext.eq_ignore_ascii_case(e)) {
        return None;
    }
    Some(stem.to_string())
}

/// Coarse authorship bucket label for the source-groups lens. Reads the
/// `hiker.author` trichotomy; falls back to the fine `hiker.provenance`
/// label, then to "Notes" for hand-authored content with no stamp at all.
fn author_bucket(row: &NoteMetaRow) -> &'static str {
    match row.author.as_deref() {
        Some("agent-authored") => "Agent-authored",
        Some("imported") => "Imported",
        Some("user-authored") => "My notes",
        _ if row.provenance.is_some() => "Imported",
        _ => "Notes",
    }
}

/// True when a note is a chat session. Sessions carry `hiker.kind: session`
/// when stamped; older sessions are recognised by living under a `chats/`
/// path segment as a fallback (the visible sessions folder).
fn is_session(row: &NoteMetaRow) -> bool {
    row.kind.as_deref() == Some("session")
        || row.path.starts_with("chats/")
        || row.path.contains("/chats/")
}

/// Imported sessions live in a `imported/` subfolder of the chats dir
/// (`chat-session-imported-visibility`); they get their own sub-bucket.
fn is_imported_session(row: &NoteMetaRow) -> bool {
    row.path.contains("/imported/") || row.path.starts_with("chats/imported/")
}

/// Build the composed default lens forest from the store projections. This
/// is the data half of `vault-view-crawl-nesting` / `-trail-nesting` /
/// `-sidecar-surfacing` / `-source-groups`; the renderer below walks the
/// result. Pure: same inputs always yield the same forest.
///
/// status: vault-view-crawl-nesting
/// status: vault-view-trail-nesting
/// status: vault-view-sidecar-surfacing
/// status: vault-view-source-groups
pub fn build_composed(notes: &[NoteMetaRow], waypoints: &[WaypointRow]) -> Vec<VaultNode> {
    let by_id: HashMap<&str, &NoteMetaRow> =
        notes.iter().map(|n| (n.id.as_str(), n)).collect();
    let by_path: HashMap<&str, &NoteMetaRow> =
        notes.iter().map(|n| (n.path.as_str(), n)).collect();

    // Notes already placed under a parent/trail/source — excluded from the
    // top-level source-group pass so they appear exactly once.
    let mut consumed: HashSet<String> = HashSet::new();

    let mut roots: Vec<VaultNode> = Vec::new();

    roots.extend(build_trail_nodes(waypoints, &by_id, &mut consumed));
    roots.extend(build_crawl_nodes(notes, &by_id, &mut consumed));
    roots.extend(build_sidecar_nodes(notes, &by_path, &mut consumed));
    roots.extend(build_source_groups(notes, &consumed));
    roots
}

/// Trail-nesting: one node per trail-doc, waypoints nested by the resolved
/// `trail_waypoints` tree (order + side-trail branching). The trail-doc and
/// every waypoint note are marked consumed.
fn build_trail_nodes(
    waypoints: &[WaypointRow],
    by_id: &HashMap<&str, &NoteMetaRow>,
    consumed: &mut HashSet<String>,
) -> Vec<VaultNode> {
    // Group waypoint rows by trail_id, preserving the store's
    // (trail_id, tree_path) order.
    let mut by_trail: BTreeMap<&str, Vec<&WaypointRow>> = BTreeMap::new();
    for wp in waypoints {
        by_trail.entry(wp.trail_id.as_str()).or_default().push(wp);
    }
    let mut out = Vec::new();
    for (trail_id, rows) in by_trail {
        // The trail-doc is the note whose id is the trail_id. If it isn't
        // indexed (e.g. a draft), skip — its waypoints stay available via
        // the source-group pass.
        let Some(trail_note) = by_id.get(trail_id) else { continue };
        consumed.insert(trail_note.path.clone());
        let children = nest_waypoints(&rows);
        for wp in &rows {
            consumed.insert(wp.waypoint_path.clone());
        }
        out.push(VaultNode {
            label: display_label(&trail_note.path),
            path: Some(trail_note.path.clone()),
            kind: NodeKind::Trail,
            children,
        });
    }
    out
}

/// Turn a flat, tree-path-ordered waypoint list into a nested forest by
/// `parent_waypoint_id`. Rows arrive ordered by `tree_path`, so a parent
/// always precedes its children and same-parent children keep reading order.
fn nest_waypoints(rows: &[&WaypointRow]) -> Vec<VaultNode> {
    // waypoint_id -> index into a flat node vec; build nodes, then splice
    // children into parents by id.
    let mut nodes: Vec<VaultNode> = rows
        .iter()
        .map(|wp| VaultNode::leaf(&wp.waypoint_path, NodeKind::Waypoint))
        .collect();
    let idx_of: HashMap<&str, usize> = rows
        .iter()
        .enumerate()
        .map(|(i, wp)| (wp.waypoint_id.as_str(), i))
        .collect();
    // Walk in reverse so a child is fully assembled before being moved into
    // its parent (children always appear after parents in tree-path order).
    let mut roots = Vec::new();
    for i in (0..rows.len()).rev() {
        let parent = rows[i].parent_waypoint_id.as_deref();
        let node = std::mem::replace(
            &mut nodes[i],
            VaultNode::group(String::new(), Vec::new()),
        );
        match parent.and_then(|p| idx_of.get(p)).copied() {
            Some(pi) if pi < i => nodes[pi].children.insert(0, node),
            // Parent missing or out-of-order (shouldn't happen): treat as root.
            _ => roots.insert(0, node),
        }
    }
    roots
}

/// Crawl/feed nesting: every `hiker.kind: capture` note becomes a parent;
/// any note stamped `hiker.parent: <capture-id>` nests beneath it. The
/// STAMP is the authority — a note in a capture's companion folder without
/// the stamp is *not* nested here (it falls through to source groups).
fn build_crawl_nodes(
    notes: &[NoteMetaRow],
    by_id: &HashMap<&str, &NoteMetaRow>,
    consumed: &mut HashSet<String>,
) -> Vec<VaultNode> {
    // parent-id -> child paths (stable order by path).
    let mut children_of: BTreeMap<&str, Vec<&NoteMetaRow>> = BTreeMap::new();
    for n in notes {
        if let Some(parent) = n.parent.as_deref() {
            // Only nest when the parent resolves to an indexed note (stray /
            // dangling stamps stay top-level).
            if by_id.contains_key(parent) {
                children_of.entry(parent).or_default().push(n);
            }
        }
    }
    let mut out = Vec::new();
    // Render a parent node for any note that has at least one stamped child.
    // Capture-kind notes are the common case; we don't *require* the kind so
    // a manifest parent works too, but children attach purely by stamp.
    for n in notes {
        let Some(kids) = children_of.get(n.id.as_str()) else { continue };
        if consumed.contains(&n.path) {
            continue; // already placed (e.g. a trail-doc); don't double-render
        }
        consumed.insert(n.path.clone());
        let mut child_nodes: Vec<VaultNode> = Vec::new();
        let mut kids_sorted = kids.clone();
        kids_sorted.sort_by(|a, b| a.path.cmp(&b.path));
        for c in kids_sorted {
            consumed.insert(c.path.clone());
            child_nodes.push(VaultNode::leaf(&c.path, NodeKind::Note));
        }
        let kind = if n.kind.as_deref() == Some("capture") {
            NodeKind::Capture
        } else {
            NodeKind::Note
        };
        out.push(VaultNode {
            label: display_label(&n.path),
            path: Some(n.path.clone()),
            kind,
            children: child_nodes,
        });
    }
    out.sort_by(|a, b| a.label.cmp(&b.label));
    out
}

/// Sidecar surfacing: a `<src>.<ext>.md` sidecar renders nested under a
/// synthetic node for its non-md source `<src>.<ext>`. The synthetic node
/// carries the source path (clicking reveals the original); the sidecar is
/// the openable child.
fn build_sidecar_nodes(
    notes: &[NoteMetaRow],
    by_path: &HashMap<&str, &NoteMetaRow>,
    consumed: &mut HashSet<String>,
) -> Vec<VaultNode> {
    let mut out = Vec::new();
    for n in notes {
        if consumed.contains(&n.path) {
            continue;
        }
        let Some(source) = sidecar_source(&n.path) else { continue };
        // If the "source" path is itself an indexed note, it's not a real
        // extractor sidecar pair — leave both as ordinary notes.
        if by_path.contains_key(source.as_str()) {
            continue;
        }
        consumed.insert(n.path.clone());
        out.push(VaultNode {
            label: display_label(&source),
            // Open path is the source; the renderer reveals it in Files.
            path: Some(source.clone()),
            kind: NodeKind::Group,
            children: vec![VaultNode::leaf(&n.path, NodeKind::Note)],
        });
    }
    out.sort_by(|a, b| a.label.cmp(&b.label));
    out
}

/// Source-type / provenance groups for every note not already placed.
/// Sessions collapse into a "Sessions" bucket (imported sessions as a
/// sub-bucket); the rest group by the authorship trichotomy.
fn build_source_groups(notes: &[NoteMetaRow], consumed: &HashSet<String>) -> Vec<VaultNode> {
    let mut buckets: BTreeMap<&str, Vec<&NoteMetaRow>> = BTreeMap::new();
    let mut sessions: Vec<&NoteMetaRow> = Vec::new();
    let mut imported_sessions: Vec<&NoteMetaRow> = Vec::new();
    for n in notes {
        if consumed.contains(&n.path) {
            continue;
        }
        if is_session(n) {
            if is_imported_session(n) {
                imported_sessions.push(n);
            } else {
                sessions.push(n);
            }
            continue;
        }
        buckets.entry(author_bucket(n)).or_default().push(n);
    }

    let mut out = Vec::new();
    for (label, mut members) in buckets {
        members.sort_by(|a, b| a.path.cmp(&b.path));
        let children = members
            .iter()
            .map(|n| VaultNode::leaf(&n.path, NodeKind::Note))
            .collect();
        out.push(VaultNode::group(format!("{label}  ({})", members.len()), children));
    }

    if !sessions.is_empty() || !imported_sessions.is_empty() {
        out.push(build_sessions_bucket(&mut sessions, &mut imported_sessions));
    }
    out
}

/// The "Sessions" group: top-level sessions plus an "Imported" sub-bucket,
/// each labelled by session title + date rather than the on-disk filename.
fn build_sessions_bucket(
    sessions: &mut Vec<&NoteMetaRow>,
    imported: &mut Vec<&NoteMetaRow>,
) -> VaultNode {
    sessions.sort_by(|a, b| a.path.cmp(&b.path));
    imported.sort_by(|a, b| a.path.cmp(&b.path));
    let mut children: Vec<VaultNode> = sessions
        .iter()
        .map(|n| VaultNode {
            label: session_label(&n.path),
            path: Some(n.path.clone()),
            kind: NodeKind::Session,
            children: Vec::new(),
        })
        .collect();
    if !imported.is_empty() {
        let sub: Vec<VaultNode> = imported
            .iter()
            .map(|n| VaultNode {
                label: session_label(&n.path),
                path: Some(n.path.clone()),
                kind: NodeKind::Session,
                children: Vec::new(),
            })
            .collect();
        children.push(VaultNode::group(format!("Imported  ({})", sub.len()), sub));
    }
    let count = sessions.len() + imported.len();
    VaultNode::group(format!("Sessions  ({count})"), children)
}

/// Session display label: the session filename encodes `YYYY-MM-DD-<id>`
/// (`session_rel_path`); surface the date plus a short id rather than the
/// raw ULID-bearing filename. Falls back to the basename when the shape
/// doesn't match.
fn session_label(path: &str) -> String {
    let base = path.rsplit('/').next().unwrap_or(path);
    let stem = base.strip_suffix(".md").unwrap_or(base);
    // `YYYY-MM-DD-<id...>`: split off the date prefix if present.
    if stem.len() > 11 && stem.as_bytes().get(10) == Some(&b'-') {
        let (date, rest) = stem.split_at(10);
        if is_iso_date(date) {
            let id = rest.trim_start_matches('-');
            let short = &id[..id.len().min(6)];
            return format!("{date} · {short}");
        }
    }
    stem.to_string()
}

/// True for a `YYYY-MM-DD` 10-char string (digits with dashes at 4 and 7).
fn is_iso_date(s: &str) -> bool {
    s.len() == 10
        && s.bytes().enumerate().all(|(i, c)| match i {
            4 | 7 => c == b'-',
            _ => c.is_ascii_digit(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(id: &str, path: &str) -> NoteMetaRow {
        NoteMetaRow {
            id: id.into(),
            path: path.into(),
            parent: None,
            author: None,
            provenance: None,
            kind: None,
        }
    }

    fn wp(trail: &str, id: &str, src: &str, parent: Option<&str>, tree: &str) -> WaypointRow {
        WaypointRow {
            waypoint_path: format!("trails/t/{id}.md"),
            waypoint_id: id.into(),
            trail_id: trail.into(),
            source_path: src.into(),
            parent_waypoint_id: parent.map(str::to_string),
            tree_path: tree.into(),
        }
    }

    #[test]
    fn sidecar_source_detection() {
        assert_eq!(sidecar_source("docs/rm0090.pdf.md").as_deref(), Some("docs/rm0090.pdf"));
        assert_eq!(sidecar_source("clip.html.md").as_deref(), Some("clip.html"));
        // Not sidecars: plain note, indexable inner ext, folder-dot only.
        assert_eq!(sidecar_source("notes/plain.md"), None);
        assert_eq!(sidecar_source("readme.txt.md"), None);
        assert_eq!(sidecar_source("a.b/plain.md"), None);
    }

    #[test]
    fn crawl_children_nest_by_parent_stamp() {
        let mut job = note("JOB", "captures/job.md");
        job.kind = Some("capture".into());
        let mut child = note("C1", "captures/job/page-1.md");
        child.parent = Some("JOB".into());
        // A stray file in the folder with NO parent stamp must NOT nest.
        let stray = note("S1", "captures/job/stray.md");

        let forest = build_composed(&[job, child, stray], &[]);
        // job (with 1 child) + stray surfaces top-level via source groups.
        let job_node = forest
            .iter()
            .find(|n| n.path.as_deref() == Some("captures/job.md"))
            .expect("job node present");
        assert_eq!(job_node.kind, NodeKind::Capture);
        assert_eq!(job_node.children.len(), 1);
        assert_eq!(job_node.children[0].path.as_deref(), Some("captures/job/page-1.md"));

        // The stray appears exactly once, somewhere, but never under the job.
        assert!(!job_node.children.iter().any(|c| c.path.as_deref() == Some("captures/job/stray.md")));
        let all = flatten(&forest);
        assert_eq!(all.iter().filter(|p| *p == "captures/job/stray.md").count(), 1);
    }

    #[test]
    fn dangling_parent_stamp_is_not_nested() {
        let mut child = note("C1", "orphan.md");
        child.parent = Some("NOPE".into()); // no note with this id
        let forest = build_composed(&[child], &[]);
        // Appears as a normal top-level note (under a source group), not lost.
        let all = flatten(&forest);
        assert_eq!(all.iter().filter(|p| *p == "orphan.md").count(), 1);
        // No node claims it as a child.
        assert!(forest.iter().all(|n| n.path.as_deref() != Some("orphan.md") || n.children.is_empty()));
    }

    #[test]
    fn trail_waypoints_nest_with_side_trail_order() {
        let mut trail = note("T1", "trails/my-trail.md");
        trail.kind = Some("trail".into());
        let waypoints = vec![
            wp("T1", "w1", "research/a.md", None, "1"),
            wp("T1", "w2", "research/b.md", Some("w1"), "1.1"), // side trail under w1
            wp("T1", "w3", "research/c.md", None, "2"),
        ];
        let forest = build_composed(&[trail], &waypoints);
        let t = forest
            .iter()
            .find(|n| n.kind == NodeKind::Trail)
            .expect("trail node");
        assert_eq!(t.children.len(), 2, "two root waypoints");
        assert_eq!(t.children[0].path.as_deref(), Some("trails/t/w1.md"));
        assert_eq!(t.children[1].path.as_deref(), Some("trails/t/w3.md"));
        // w2 nests under w1 (the side trail), preserving order.
        assert_eq!(t.children[0].children.len(), 1);
        assert_eq!(t.children[0].children[0].path.as_deref(), Some("trails/t/w2.md"));
    }

    #[test]
    fn sidecar_pairs_with_synthetic_source() {
        let sidecar = note("S1", "docs/rm0090.pdf.md");
        let forest = build_composed(&[sidecar], &[]);
        let src = forest
            .iter()
            .find(|n| n.path.as_deref() == Some("docs/rm0090.pdf"))
            .expect("synthetic source node");
        assert_eq!(src.kind, NodeKind::Group);
        assert_eq!(src.children.len(), 1);
        assert_eq!(src.children[0].path.as_deref(), Some("docs/rm0090.pdf.md"));
        // The sidecar does not ALSO appear as a top-level note.
        assert_eq!(
            flatten(&forest).iter().filter(|p| *p == "docs/rm0090.pdf.md").count(),
            1
        );
    }

    #[test]
    fn source_groups_split_by_authorship_and_sessions() {
        let mut mine = note("U1", "ideas/note.md");
        mine.author = Some("user-authored".into());
        let mut agent = note("A1", "agent/out.md");
        agent.author = Some("agent-authored".into());
        let session = note("SE1", "chats/2026-05-30-01ABCDEF.md");
        let imported = note("SE2", "chats/imported/2026-05-29-01ZZZZZZ.md");

        let forest = build_composed(&[mine, agent, session, imported], &[]);
        let labels: Vec<&str> = forest.iter().map(|n| n.label.as_str()).collect();
        assert!(labels.iter().any(|l| l.starts_with("My notes")));
        assert!(labels.iter().any(|l| l.starts_with("Agent-authored")));

        let sessions = forest
            .iter()
            .find(|n| n.label.starts_with("Sessions"))
            .expect("sessions bucket");
        // One top-level session + one Imported sub-bucket.
        assert!(sessions.children.iter().any(|c| c.kind == NodeKind::Session));
        let imp = sessions
            .children
            .iter()
            .find(|c| c.label.starts_with("Imported"))
            .expect("imported sub-bucket");
        assert_eq!(imp.children.len(), 1);
        // Session labelled by date, not raw filename.
        let dated = sessions
            .children
            .iter()
            .find(|c| c.kind == NodeKind::Session)
            .unwrap();
        assert!(dated.label.starts_with("2026-05-30"));
    }

    /// Collect every note path (leaves with a path) in the forest.
    fn flatten(forest: &[VaultNode]) -> Vec<String> {
        let mut out = Vec::new();
        fn walk(n: &VaultNode, out: &mut Vec<String>) {
            if let Some(p) = &n.path {
                out.push(p.clone());
            }
            for c in &n.children {
                walk(c, out);
            }
        }
        for n in forest {
            walk(n, &mut out);
        }
        out
    }
}
