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

use hiker_core::queries::SmartFolder;
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
    /// A query-doc rendered as a smart-folder header (`smart-folder-view`).
    Query,
    /// A note appearing under a smart folder as a virtual member — marked
    /// (italic + badge) as a reference, never a residence.
    QueryMember,
    /// A smart folder's loud error row (malformed filter / unreadable doc).
    QueryError,
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

/// Derive a source-note path from a waypoint snapshot's `waypoint_path` alone.
/// The snapshot is `<source-rel>--<rand6>.md` under `/waypoints/`, so the part
/// after `/waypoints/` with the `--<rand6>.md` suffix stripped is the source.
/// LOSSY: a snapshot stored without its source's directory (legacy `.md`
/// waypoints were flattened to a bare basename) yields only the basename stem,
/// with no directory and no original extension. [`resolve_waypoint_source`]
/// re-resolves that against the note index to recover the real path.
fn derive_snapshot_source(waypoint_path: &str) -> String {
    let after = waypoint_path
        .rsplit_once("/waypoints/")
        .map_or(waypoint_path, |(_, rel)| rel);
    let stem = after.strip_suffix(".md").unwrap_or(after);
    match stem.rfind("--") {
        Some(i) => stem[..i].to_string(),
        None => stem.to_string(),
    }
}

/// The real vault note a waypoint row points at, resolved against the indexed
/// notes so the row opens / reveals / previews the live note instead of a dead
/// path. Resolution order:
///
/// 1. The indexer-derived `source_path`, when it names an indexed note (the
///    normal case: the waypoint-note's `hiker.references.path` was ingested).
/// 2. The snapshot-filename derivation, when it itself names an indexed note
///    (a snapshot that kept the source's directory + extension).
/// 3. The note whose filename stem equals the derived basename — recovers the
///    source when the snapshot flattened away its directory / extension. The
///    `-<hash>` suffix the indexer stamps on note filenames keeps these unique.
/// 4. Best effort: the non-empty `source_path`, else the raw derivation, so the
///    row still labels even when nothing resolves (it just can't open/preview).
///
/// Without (2)/(3), waypoints in vaults whose `.hiker/trails/…` snapshots are
/// gone (so `source_path` is empty) carried un-openable paths — a hover or
/// click produced a "No such file" error. status: vault-view-trail-nesting
fn resolve_waypoint_source(
    wp: &WaypointRow,
    by_path: &HashMap<&str, &NoteMetaRow>,
    by_stem: &HashMap<String, &str>,
) -> String {
    if !wp.source_path.is_empty() && by_path.contains_key(wp.source_path.as_str()) {
        return wp.source_path.clone();
    }
    let derived = derive_snapshot_source(&wp.waypoint_path);
    if by_path.contains_key(derived.as_str()) {
        return derived;
    }
    if let Some(path) = by_stem.get(&display_label(&derived)) {
        return (*path).to_string();
    }
    if !wp.source_path.is_empty() {
        return wp.source_path.clone();
    }
    derived
}

/// Index from a note's filename stem (basename minus the indexable extension,
/// per [`display_label`]) to its vault path, keeping only stems that map to a
/// single note. Powers step 3 of [`resolve_waypoint_source`]; ambiguous stems
/// are dropped so a guess is never wrong.
fn build_stem_index(notes: &[NoteMetaRow]) -> HashMap<String, &str> {
    let mut by_stem: HashMap<String, &str> = HashMap::new();
    let mut ambiguous: HashSet<String> = HashSet::new();
    for n in notes {
        let stem = display_label(&n.path);
        if by_stem.insert(stem.clone(), n.path.as_str()).is_some() {
            ambiguous.insert(stem);
        }
    }
    for s in &ambiguous {
        by_stem.remove(s);
    }
    by_stem
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
pub fn build_composed(
    notes: &[NoteMetaRow],
    waypoints: &[WaypointRow],
    folders: &[SmartFolder],
) -> Vec<VaultNode> {
    let by_id: HashMap<&str, &NoteMetaRow> =
        notes.iter().map(|n| (n.id.as_str(), n)).collect();
    let by_path: HashMap<&str, &NoteMetaRow> =
        notes.iter().map(|n| (n.path.as_str(), n)).collect();
    // Filename-stem index so a trail waypoint whose snapshot path lost the
    // source's directory can still resolve to the real note.
    let by_stem = build_stem_index(notes);

    // Notes already placed under a parent/trail/source — excluded from the
    // top-level source-group pass so they appear exactly once.
    let mut consumed: HashSet<String> = HashSet::new();

    let mut roots: Vec<VaultNode> = Vec::new();

    roots.extend(build_trail_nodes(waypoints, &by_id, &by_path, &by_stem, &mut consumed));
    roots.extend(build_smart_folder_nodes(folders, &mut consumed));
    roots.extend(build_crawl_nodes(notes, &by_id, &mut consumed));
    roots.extend(build_sidecar_nodes(notes, &by_path, &mut consumed));
    roots.extend(build_source_groups(notes, &consumed));
    roots
}

/// Smart folders: one virtual folder per query-doc. The header row IS the
/// query-doc (consumed so it renders exactly once, like a trail-doc) and
/// carries the live match count; member rows are *virtual* — their paths
/// are deliberately NOT consumed, so a match also keeps appearing in every
/// other grouping that claims it. A query-doc whose filter failed renders
/// a loud error child instead of an empty (or match-all) folder.
///
/// status: smart-folder-view
fn build_smart_folder_nodes(
    folders: &[SmartFolder],
    consumed: &mut HashSet<String>,
) -> Vec<VaultNode> {
    let mut out = Vec::new();
    for folder in folders {
        consumed.insert(folder.rel_path.clone());
        let (label, children) = match &folder.result {
            Ok(rows) => (
                format!("{}  ({})", folder.title, rows.len()),
                rows.iter()
                    .map(|r| VaultNode::leaf(&r.path, NodeKind::QueryMember))
                    .collect(),
            ),
            Err(e) => (
                folder.title.clone(),
                // The error row opens the query-doc so the fix is one
                // click away.
                vec![VaultNode {
                    label: format!("query error: {e}"),
                    path: Some(folder.rel_path.clone()),
                    kind: NodeKind::QueryError,
                    children: Vec::new(),
                }],
            ),
        };
        out.push(VaultNode {
            label,
            path: Some(folder.rel_path.clone()),
            kind: NodeKind::Query,
            children,
        });
    }
    out
}

/// Trail-nesting: one node per trail-doc, waypoints nested by the resolved
/// `trail_waypoints` tree (order + side-trail branching). The trail-doc and
/// every waypoint note are marked consumed.
fn build_trail_nodes(
    waypoints: &[WaypointRow],
    by_id: &HashMap<&str, &NoteMetaRow>,
    by_path: &HashMap<&str, &NoteMetaRow>,
    by_stem: &HashMap<String, &str>,
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
        let children = nest_waypoints(&rows, by_path, by_stem);
        for wp in &rows {
            // Consume the *source* note (the real vault note the waypoint
            // captures), not the internal `.hiker/trails/…` snapshot pointer —
            // so the note shows only under its trail, and its row opens/reveals
            // the actual note rather than the un-openable snapshot file.
            consumed.insert(resolve_waypoint_source(wp, by_path, by_stem));
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
fn nest_waypoints(
    rows: &[&WaypointRow],
    by_path: &HashMap<&str, &NoteMetaRow>,
    by_stem: &HashMap<String, &str>,
) -> Vec<VaultNode> {
    // waypoint_id -> index into a flat node vec; build nodes, then splice
    // children into parents by id.
    let mut nodes: Vec<VaultNode> = rows
        .iter()
        // The node points at the source note (resolved via
        // `resolve_waypoint_source`), so opening, revealing, copy-path, and
        // Properties act on the real note — never the un-openable
        // `.hiker/trails/…` snapshot pointer.
        .map(|wp| VaultNode::leaf(&resolve_waypoint_source(wp, by_path, by_stem), NodeKind::Waypoint))
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
    fn derive_snapshot_source_strips_rand_suffix() {
        // Snapshot that kept the source's dir + extension.
        assert_eq!(
            derive_snapshot_source(
                ".hiker/trails/01ABC/waypoints/networking/tcp-deep-dive-abb4b0.txt--YHN2N2.md"
            ),
            "networking/tcp-deep-dive-abb4b0.txt"
        );
        // Flattened `.md` snapshot: only the basename stem survives (dir +
        // original `.md` lost) — `resolve_waypoint_source` recovers the rest.
        assert_eq!(
            derive_snapshot_source(
                ".hiker/trails/01ABC/waypoints/b-tree-index-anatomy-1a4b4e--DJAVGK.md"
            ),
            "b-tree-index-anatomy-1a4b4e"
        );
    }

    #[test]
    fn resolve_waypoint_source_recovers_real_note() {
        let notes = vec![
            note("A", "research/a.md"),
            note("BT", "indexing/b-tree-index-anatomy-1a4b4e.md"),
        ];
        let by_path: HashMap<&str, &NoteMetaRow> =
            notes.iter().map(|n| (n.path.as_str(), n)).collect();
        let by_stem = build_stem_index(&notes);

        // 1. source_path that names an indexed note → used verbatim.
        let w = wp("T1", "w1", "research/a.md", None, "1");
        assert_eq!(resolve_waypoint_source(&w, &by_path, &by_stem), "research/a.md");

        // 2/3. Empty source_path + flattened snapshot filename → recovered by
        // stem, restoring the lost `indexing/` directory + `.md` extension.
        let mut w2 = wp("T1", "w2", "", None, "1");
        w2.waypoint_path =
            ".hiker/trails/01ABC/waypoints/b-tree-index-anatomy-1a4b4e--DJAVGK.md".into();
        assert_eq!(
            resolve_waypoint_source(&w2, &by_path, &by_stem),
            "indexing/b-tree-index-anatomy-1a4b4e.md"
        );

        // 4. Nothing resolves → best-effort raw derivation (row still labels).
        let mut w3 = wp("T1", "w3", "", None, "1");
        w3.waypoint_path = ".hiker/trails/01ABC/waypoints/ghost-note-zzzzzz--QQQQQQ.md".into();
        assert_eq!(resolve_waypoint_source(&w3, &by_path, &by_stem), "ghost-note-zzzzzz");
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

        let forest = build_composed(&[job, child, stray], &[], &[]);
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
        let forest = build_composed(&[child], &[], &[]);
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
        let forest = build_composed(&[trail], &waypoints, &[]);
        let t = forest
            .iter()
            .find(|n| n.kind == NodeKind::Trail)
            .expect("trail node");
        assert_eq!(t.children.len(), 2, "two root waypoints");
        // Waypoint nodes carry the SOURCE note path, not the internal
        // `.hiker/trails/…` snapshot pointer, so their rows open the real note.
        assert_eq!(t.children[0].path.as_deref(), Some("research/a.md"));
        assert_eq!(t.children[1].path.as_deref(), Some("research/c.md"));
        // w2 nests under w1 (the side trail), preserving order.
        assert_eq!(t.children[0].children.len(), 1);
        assert_eq!(t.children[0].children[0].path.as_deref(), Some("research/b.md"));
    }

    #[test]
    fn sidecar_pairs_with_synthetic_source() {
        let sidecar = note("S1", "docs/rm0090.pdf.md");
        let forest = build_composed(&[sidecar], &[], &[]);
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

        let forest = build_composed(&[mine, agent, session, imported], &[], &[]);
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

    fn folder(
        rel: &str,
        title: &str,
        result: Result<Vec<&str>, hiker_core::queries::Error>,
    ) -> SmartFolder {
        SmartFolder {
            rel_path: rel.into(),
            title: title.into(),
            result: result.map(|paths| {
                paths
                    .into_iter()
                    .map(|p| hiker_core::store::dto::NoteQueryRow {
                        note_id: p.into(),
                        path: p.into(),
                        title: display_label(p),
                        mtime: 0,
                        fields: Default::default(),
                    })
                    .collect()
            }),
        }
    }

    #[test]
    fn smart_folder_members_are_virtual_and_header_is_consumed() {
        let mut doc = note("Q1", "queries/rust.md");
        doc.kind = Some("query".into());
        let mut member = note("M1", "notes/lang.md");
        member.author = Some("user-authored".into());
        let folders = vec![folder("queries/rust.md", "rust", Ok(vec!["notes/lang.md"]))];

        let forest = build_composed(&[doc, member], &[], &folders);
        let header = forest
            .iter()
            .find(|n| n.kind == NodeKind::Query)
            .expect("smart-folder header");
        // Header row IS the query-doc, with the live match count.
        assert_eq!(header.path.as_deref(), Some("queries/rust.md"));
        assert_eq!(header.label, "rust  (1)");
        assert_eq!(header.children.len(), 1);
        assert_eq!(header.children[0].kind, NodeKind::QueryMember);
        assert_eq!(header.children[0].path.as_deref(), Some("notes/lang.md"));

        let all = flatten(&forest);
        // Membership is virtual: the member ALSO stays in its source group
        // (two appearances), while the query-doc renders exactly once.
        assert_eq!(all.iter().filter(|p| *p == "notes/lang.md").count(), 2);
        assert_eq!(all.iter().filter(|p| *p == "queries/rust.md").count(), 1);
    }

    #[test]
    fn smart_folder_error_renders_loud_error_row() {
        let mut doc = note("Q1", "queries/broken.md");
        doc.kind = Some("query".into());
        let folders = vec![folder(
            "queries/broken.md",
            "broken",
            Err(hiker_core::queries::Error::UnknownClause("nonsense".into())),
        )];
        let forest = build_composed(&[doc], &[], &folders);
        let header = forest.iter().find(|n| n.kind == NodeKind::Query).unwrap();
        // No match count on a failed query; one error child naming the
        // failure, opening the query-doc.
        assert_eq!(header.label, "broken");
        assert_eq!(header.children.len(), 1);
        let err = &header.children[0];
        assert_eq!(err.kind, NodeKind::QueryError);
        assert!(err.label.contains("nonsense"), "{}", err.label);
        assert_eq!(err.path.as_deref(), Some("queries/broken.md"));
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
