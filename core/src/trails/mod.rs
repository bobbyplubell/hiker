//! Trails: curated, ordered walks through the vault. See `docs/trails.md`.
//!
//! This module owns the trail-doc / waypoint-note type definitions,
//! frontmatter parse/write helpers, storage-layout helpers, and the
//! read-only listing/detail surface (`list`, `get_trail`,
//! `containing_note_with_paths`). The mutation verbs (`create_trail`,
//! `append_waypoint`, `remove_waypoint`, `delete_trail`, the append-cursor
//! / activation stamps) and the note-move path-remap pass live in
//! `ops.rs`.
//!
//! A trail-doc is a regular markdown note with `hiker.kind: trail` in its
//! frontmatter; a waypoint-note lives in the trail-doc's *visible
//! companion folder* (`<dir>/<trail>/<source-basename>--<rand6>.md`, per
//! `note-companion-folder`) and carries `hiker.kind: waypoint`. Draft
//! trails are the exception: their doc + companion folder stay hidden
//! under `.hiker/trails/drafts/` until accepted. Per-spec (see
//! `docs/trails.md` §"Trail-doc shape"), a non-`.md` file with
//! `hiker.kind: trail` is NOT a trail — callers verify the extension
//! before parsing.
//
// status: trail-doc-shape
// status: waypoint-note-shape
// status: trail-double-link-references
// status: trail-storage-layout

use serde::{Deserialize, Serialize};
use serde_yml::Value as YamlValue;
use thiserror::Error;

use crate::errors::HikerError;
use crate::frontmatter::{assemble, merge_json_into_yaml, split, Error as FmError};
use crate::oplog::OpLog;
use crate::store::Store;
use crate::vault::Vault;

pub mod ops;
#[cfg(test)]
mod tests;

use ops::{resolve_reference, ResolutionOutcome};


/// One entry in the trail-doc's recursive `hiker.waypoints` tree. Each
/// entry is a vault-relative path to a waypoint-note and may carry its
/// own `waypoints:` array of children forming a side trail. Children
/// nest arbitrarily deep; an entry with no `waypoints:` key (or an
/// empty array) is a leaf.
///
/// Under path-as-identity (`wikilink-path-form`), references are
/// path-only — no ULID half. The waypoint's internal storage id is
/// op-log's `doc_id` for the waypoint-note's path, never written into
/// frontmatter.
///
/// status: trail-side-trail-shape
/// status: trail-path-references
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaypointEntry {
    pub path: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub waypoints: Vec<WaypointEntry>,
}

/// Parsed `hiker.*` frontmatter for a trail-doc. Sibling fields under
/// `hiker.*` (e.g. `hiker.author`, `hiker.provenance`) and any
/// non-`hiker` top-level keys round-trip via the source YAML and are
/// not part of this struct — round-trip is via `parse_trail_doc` /
/// `write_trail_doc_frontmatter` which preserve unknown siblings.
///
/// The trail's internal identifier is its op-log `doc_id`, read from
/// `doc-index.db` rather than stamped into frontmatter
/// (`op-log-document-identity`). No `hiker.id` field here.
///
/// status: trail-doc-shape
/// status: trail-side-trail-shape
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrailDocFrontmatter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activated_at: Option<String>,
    #[serde(default)]
    pub waypoints: Vec<WaypointEntry>,
    /// Append cursor — names the waypoint under which the next
    /// `append_waypoint(parent_waypoint_id: None)` call lands. `None`
    /// (or absent / explicit null in YAML) means "append at the root
    /// tail" — the original flat-trail behavior. See
    /// `docs/trails.md` §"Append cursor — branching the trail".
    ///
    /// The cursor names a waypoint by its vault-relative `path` (the
    /// waypoint-note's path in the trail-doc's companion folder).
    ///
    /// status: trail-append-cursor
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub append_under: Option<String>,
}

/// Parsed `hiker.*` frontmatter for a waypoint-note. References are
/// vault-relative paths — no ULID half — per `trail-path-references`.
///
/// status: waypoint-note-shape
/// status: trail-path-references
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaypointFrontmatter {
    /// Source note this waypoint annotates.
    pub references: String,
    /// Trail-doc this waypoint belongs to.
    pub in_trail: String,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("missing frontmatter (expected hiker.kind = trail | waypoint)")]
    MissingFrontmatter,
    #[error("hiker.kind expected `{expected}`, found `{found}`")]
    KindMismatch { expected: &'static str, found: String },
    #[error("required field `{0}` missing or wrong type")]
    MissingField(&'static str),
    #[error("frontmatter not a mapping")]
    NotMapping,
    #[error("non-.md path cannot be a trail-doc: {0}")]
    NotMarkdown(String),
    #[error("frontmatter assemble: {0}")]
    Assemble(#[from] FmError),
}

/// Parse a trail-doc's frontmatter. Caller MUST verify the source path
/// has a `.md` extension before calling this — a non-`.md` file with
/// `hiker.kind: trail` is not a trail per spec; `parse_trail_doc_for`
/// is the path-aware wrapper.
///
/// status: trail-doc-shape
pub fn parse_trail_doc(source: &str) -> Result<TrailDocFrontmatter, Error> {
    let split_view = split(source);
    let fm = split_view.frontmatter.ok_or(Error::MissingFrontmatter)?;
    let YamlValue::Mapping(map) = &fm else {
        return Err(Error::NotMapping);
    };
    let hiker = map
        .get("hiker")
        .ok_or(Error::MissingField("hiker"))?;
    let YamlValue::Mapping(hiker_map) = hiker else {
        return Err(Error::MissingField("hiker"));
    };

    let kind = hiker_map
        .get("kind")
        .and_then(|v| v.as_str())
        .ok_or(Error::MissingField("hiker.kind"))?;
    if kind != "trail" {
        return Err(Error::KindMismatch {
            expected: "trail",
            found: kind.to_string(),
        });
    }

    let last_activated_at = hiker_map
        .get("last_activated_at")
        .and_then(|v| v.as_str())
        .map(std::string::ToString::to_string);

    let waypoints = match hiker_map.get("waypoints") {
        None => Vec::new(),
        Some(YamlValue::Sequence(seq)) => seq.iter().filter_map(parse_waypoint_entry).collect(),
        Some(_) => return Err(Error::MissingField("hiker.waypoints")),
    };

    // status: trail-append-cursor
    // Missing key OR explicit `null` both map to None; a string maps to
    // Some. Anything else is silently treated as None (the cursor is
    // self-healing — see the stale-id branch in `append_waypoint`).
    let append_under = hiker_map
        .get("append_under")
        .and_then(|v| v.as_str())
        .map(std::string::ToString::to_string);

    Ok(TrailDocFrontmatter {
        last_activated_at,
        waypoints,
        append_under,
    })
}

/// Path-aware wrapper around `parse_trail_doc`: rejects non-`.md`
/// extensions before parsing, per the spec's "discriminator alone isn't
/// enough" rule.
pub fn parse_trail_doc_for(rel: &str, source: &str) -> Result<TrailDocFrontmatter, Error> {
    if !rel.ends_with(".md") {
        return Err(Error::NotMarkdown(rel.to_string()));
    }
    parse_trail_doc(source)
}

/// Parse a waypoint-note's frontmatter.
///
/// status: waypoint-note-shape
pub fn parse_waypoint(source: &str) -> Result<WaypointFrontmatter, Error> {
    let split_view = split(source);
    let fm = split_view.frontmatter.ok_or(Error::MissingFrontmatter)?;
    let YamlValue::Mapping(map) = &fm else {
        return Err(Error::NotMapping);
    };
    let hiker = map
        .get("hiker")
        .ok_or(Error::MissingField("hiker"))?;
    let YamlValue::Mapping(hiker_map) = hiker else {
        return Err(Error::MissingField("hiker"));
    };

    let kind = hiker_map
        .get("kind")
        .and_then(|v| v.as_str())
        .ok_or(Error::MissingField("hiker.kind"))?;
    if kind != "waypoint" {
        return Err(Error::KindMismatch {
            expected: "waypoint",
            found: kind.to_string(),
        });
    }

    let references = hiker_map
        .get("references")
        .and_then(parse_path_ref)
        .ok_or(Error::MissingField("hiker.references"))?;
    let in_trail = hiker_map
        .get("in_trail")
        .and_then(parse_path_ref)
        .ok_or(Error::MissingField("hiker.in_trail"))?;

    Ok(WaypointFrontmatter {
        references,
        in_trail,
    })
}

/// Pull `path: "<rel>"` from a `references` / `in_trail` YAML mapping.
/// status: trail-path-references
fn parse_path_ref(v: &YamlValue) -> Option<String> {
    let YamlValue::Mapping(m) = v else { return None };
    let path = m.get("path")?.as_str()?.to_string();
    Some(path)
}

/// Recursive YAML-to-`WaypointEntry` parser. Children at any depth are
/// parsed via the same function. Pre-tree-format YAML (entries with no
/// `waypoints:` key) parses cleanly with an empty children vec, so old
/// flat trail-docs round-trip as a tree of all-root entries. References
/// are path-only per `trail-path-references` — any legacy `id:` half is
/// silently dropped on parse.
///
/// status: trail-side-trail-shape
/// status: trail-path-references
fn parse_waypoint_entry(v: &YamlValue) -> Option<WaypointEntry> {
    let YamlValue::Mapping(m) = v else { return None };
    let path = m.get("path")?.as_str()?.to_string();
    let waypoints = match m.get("waypoints") {
        Some(YamlValue::Sequence(seq)) => {
            seq.iter().filter_map(parse_waypoint_entry).collect()
        }
        _ => Vec::new(),
    };
    Some(WaypointEntry { path, waypoints })
}

/// Serialize a trail-doc frontmatter back into the source. Preserves
/// non-`hiker.*` top-level fields and any unknown sibling fields under
/// `hiker.*` — only `hiker.{kind,id,last_activated_at,waypoints}` are
/// rewritten, mirroring `merge_agent_patch`'s deep-merge semantics.
///
/// status: trail-doc-shape
pub fn write_trail_doc_frontmatter(
    body_source: &str,
    fm: &TrailDocFrontmatter,
) -> Result<String, Error> {
    let split_view = split(body_source);
    let mut existing = match split_view.frontmatter {
        Some(v) => v,
        None => YamlValue::Mapping(Default::default()),
    };
    if !matches!(existing, YamlValue::Mapping(_)) {
        existing = YamlValue::Mapping(Default::default());
    }
    let mut hiker_patch = serde_json::Map::new();
    hiker_patch.insert("kind".into(), serde_json::Value::String("trail".into()));
    // status: trail-doc-shape
    // No `hiker.id` — the trail's storage key is op-log's `doc_id` for
    // the trail-doc's path (read via `oplog::doc_id_for_path`), kept in
    // `doc-index.db` rather than stamped into the file.
    if let Some(ts) = &fm.last_activated_at {
        hiker_patch.insert(
            "last_activated_at".into(),
            serde_json::Value::String(ts.clone()),
        );
    }
    // status: trail-append-cursor
    // Only emit `append_under` when set — keeps the frontmatter clean
    // for trails that have never moved the cursor.
    if let Some(cursor) = &fm.append_under {
        hiker_patch.insert(
            "append_under".into(),
            serde_json::Value::String(cursor.clone()),
        );
    }
    hiker_patch.insert(
        "waypoints".into(),
        serde_json::Value::Array(
            fm.waypoints
                .iter()
                .map(waypoint_entry_to_json)
                .collect::<Vec<_>>(),
        ),
    );
    let patch = serde_json::json!({ "hiker": serde_json::Value::Object(hiker_patch) });
    // Existing trail-docs may have a `waypoints` array from a prior
    // (flat-list) write. `merge_json_into_yaml` deep-merges maps but
    // *replaces* arrays — so the existing flat array is fully overwritten
    // with the new tree-shape. Good. But if the old YAML had children
    // baked into the same key, we need to ensure no stale tree is left:
    // strip the existing `hiker.waypoints` before the merge so the new
    // value is the one that lands.
    if let YamlValue::Mapping(top) = &mut existing
        && let Some(YamlValue::Mapping(hiker)) = top.get_mut("hiker")
    {
        hiker.remove("waypoints");
        // status: trail-doc-shape
        // Strip any legacy `hiker.id` so rewriting an old trail-doc whose
        // frontmatter stamped a ULID drops the field cleanly.
        hiker.remove("id");
        // status: trail-append-cursor
        // When fm.append_under is None, strip any pre-existing
        // `append_under` key so the rewritten frontmatter reflects
        // "cursor cleared" rather than holding a stale value. When
        // fm.append_under is Some, the patch's key overwrites
        // anything pre-existing via the deep-merge.
        if fm.append_under.is_none() {
            hiker.remove("append_under");
        }
    }
    merge_json_into_yaml(&mut existing, patch);
    Ok(assemble(&existing, split_view.body)?)
}

/// Recursive `WaypointEntry` → JSON encoder used by
/// `write_trail_doc_frontmatter`. Mirrors `parse_waypoint_entry`.
///
/// status: trail-side-trail-shape
fn waypoint_entry_to_json(e: &WaypointEntry) -> serde_json::Value {
    if e.waypoints.is_empty() {
        serde_json::json!({ "path": e.path })
    } else {
        let children: Vec<_> = e.waypoints.iter().map(waypoint_entry_to_json).collect();
        serde_json::json!({
            "path": e.path,
            "waypoints": children,
        })
    }
}

/// Serialize a waypoint-note frontmatter back into the source.
///
/// status: waypoint-note-shape
pub fn write_waypoint_frontmatter(
    body_source: &str,
    fm: &WaypointFrontmatter,
) -> Result<String, Error> {
    let split_view = split(body_source);
    let mut existing = match split_view.frontmatter {
        Some(v) => v,
        None => YamlValue::Mapping(Default::default()),
    };
    if !matches!(existing, YamlValue::Mapping(_)) {
        existing = YamlValue::Mapping(Default::default());
    }
    let mut hiker_patch = serde_json::Map::new();
    hiker_patch.insert("kind".into(), serde_json::Value::String("waypoint".into()));
    // status: waypoint-note-shape
    // No `hiker.id` — waypoints are addressed by their vault path.
    hiker_patch.insert("references".into(), path_ref_to_json(&fm.references));
    hiker_patch.insert("in_trail".into(), path_ref_to_json(&fm.in_trail));
    // Strip any pre-existing legacy `id` so a rewrite of an old waypoint
    // drops the stale field.
    if let YamlValue::Mapping(top) = &mut existing
        && let Some(YamlValue::Mapping(hiker)) = top.get_mut("hiker")
    {
        hiker.remove("id");
    }
    let patch = serde_json::json!({ "hiker": serde_json::Value::Object(hiker_patch) });
    merge_json_into_yaml(&mut existing, patch);
    Ok(assemble(&existing, split_view.body)?)
}

/// Serialize a path reference for waypoint frontmatter. Always emits the
/// single `{ path: "<rel>" }` shape — no `id:` half, per
/// `trail-path-references`.
fn path_ref_to_json(rel: &str) -> serde_json::Value {
    serde_json::json!({ "path": rel })
}

/// Leaf directory name of the hidden trails tree under `.hiker/`. Used now
/// only for the draft staging area (`.hiker/trails/drafts/`) and the
/// op-log bootstrap / watcher carve-out — accepted trails store their
/// waypoints in the trail-doc's *visible* companion folder instead.
///
/// status: trail-storage-layout
pub const TRAILS_DIRNAME: &str = "trails";

/// Leaf directory name holding draft trail-docs: `.hiker/trails/drafts/`.
///
/// status: trail-storage-layout
pub const DRAFTS_DIRNAME: &str = "drafts";

/// Vault-relative path of the hidden trails dir (`.hiker/trails`). Always
/// forward-slashed. Now only the parent of the drafts staging dir; route
/// every `.hiker/trails` literal through this (or the helpers below).
///
/// status: trail-storage-layout
pub fn dir() -> String {
    format!(".hiker/{TRAILS_DIRNAME}")
}

/// Vault-relative prefix used to match anything under the hidden trails
/// tree: `.hiker/trails/`. Use with `str::starts_with` (the watcher
/// carve-out for drafts, op-log bootstrap's second pass).
///
/// status: trail-storage-layout
pub fn dir_prefix() -> String {
    format!("{}/", dir())
}

/// Vault-relative path of a trail's waypoints directory: the trail-doc's
/// *companion folder* (`note-companion-folder` in `files.md`). A trail-doc
/// at `<dir>/<trail>.md` stores its waypoint-notes in `<dir>/<trail>/`.
/// This holds for both accepted trails (visible, e.g. `trails/my-trail/`)
/// and drafts (hidden, e.g. `.hiker/trails/drafts/<id>/`) — the companion
/// folder is always the sibling of the trail-doc, wherever it lives.
///
/// The waypoint dir is derived from the trail-doc's *path*, not its
/// `doc_id`: the identity is still the op-log `doc_id`, but the physical
/// home now follows the doc on disk (so a rename moves the folder via
/// `move_note`'s companion-folder pairing).
///
/// Returns `None` only for a non-`.md` trail-doc path (which can't be a
/// trail per spec, so it never reaches the waypoint writers).
///
/// status: trail-storage-layout
/// status: note-companion-folder
#[must_use]
pub fn waypoints_dir_for_doc(trail_doc_rel: &str) -> Option<String> {
    crate::vault::companion_folder_for(trail_doc_rel)
}

/// Filename for a waypoint-note. Per spec
/// (`docs/trails.md` §"Storage layout"), basename is
/// `<source-basename>--<rand6>.md` where `<rand6>` is a 6-char random
/// alphanumeric disambiguator. Filename is a stable identifier — never
/// renamed on reorder/re-parent — so order + tree shape live in the
/// trail-doc's frontmatter alone. Under path-as-identity
/// (`trail-path-references`) the waypoint carries no ULID, so the slot
/// previously held by the ULID-suffix is filled by a fresh random 6-char
/// token per call.
///
/// `source_basename` should be the source-note's basename *without* its
/// `.md` extension; callers that need to embed an arbitrary string
/// (e.g. for a non-md source-derived note) pass the basename verbatim.
///
/// status: trail-storage-layout
pub fn waypoint_filename(source_basename: &str) -> String {
    format!("{source_basename}--{}.md", random_alphanumeric_6())
}

/// 6-char random alphanumeric token used as the waypoint-filename
/// disambiguator. Cryptographic randomness isn't required — collision
/// is the only failure mode and the caller's suffix-loop handles the
/// vanishingly-rare clash. Derived from the random tail of a fresh ULID
/// (Crockford base32, so uppercase letters + digits only — already
/// filesystem-safe alphanumeric across every host fs hiker supports).
fn random_alphanumeric_6() -> String {
    let s = ulid::Ulid::new().to_string();
    // ULIDs are 26 chars; the last 10 are the random component. Take the
    // tail 6 of those 10 so two ULIDs produced microseconds apart still
    // disagree at this slot.
    let n = s.len();
    s[n - 6..].to_string()
}

/// Walk the recursive waypoint tree depth-first in reading order.
/// `f` receives `(parent_id, entry, tree_path)` for every node;
/// `parent_id` is `None` for root-level entries; `tree_path` is the
/// 1-based dotted index path (`"1"`, `"1.2"`, `"1.2.1"`).
///
/// Walk the recursive waypoint tree depth-first. `f` receives
/// `(parent_path, entry, tree_path)` for every node; `parent_path` is
/// `None` for root-level entries and the parent's vault-relative
/// waypoint-note path otherwise; `tree_path` is the 1-based dotted index
/// path (`"1"`, `"1.2"`, `"1.2.1"`).
///
/// Under path-as-identity (`trail-path-references`) the waypoint's
/// vault path is its identity; the previous `parent_id` ULID parameter
/// is renamed to `parent_path` accordingly.
///
/// status: trail-side-trail-shape
/// status: trail-path-references
pub fn walk_waypoints_depth_first<F>(entries: &[WaypointEntry], f: &mut F)
where
    F: FnMut(Option<&str>, &WaypointEntry, &str),
{
    fn walk<F: FnMut(Option<&str>, &WaypointEntry, &str)>(
        entries: &[WaypointEntry],
        parent_path: Option<&str>,
        prefix: &str,
        f: &mut F,
    ) {
        for (idx, entry) in entries.iter().enumerate() {
            let one_based = idx + 1;
            let tree_path = if prefix.is_empty() {
                format!("{one_based}")
            } else {
                format!("{prefix}.{one_based}")
            };
            f(parent_path, entry, &tree_path);
            if !entry.waypoints.is_empty() {
                walk(&entry.waypoints, Some(&entry.path), &tree_path, f);
            }
        }
    }
    walk(entries, None, "", f);
}

/// Find a waypoint entry by its vault path anywhere in the recursive
/// tree (mutable). Returns the `&mut` entry the caller can edit
/// (typically to push a child onto its `waypoints` array).
///
/// status: trail-side-trail-shape
pub fn find_waypoint_mut<'a>(
    entries: &'a mut [WaypointEntry],
    waypoint_path: &str,
) -> Option<&'a mut WaypointEntry> {
    for entry in entries.iter_mut() {
        if entry.path == waypoint_path {
            return Some(entry);
        }
        if let Some(found) = find_waypoint_mut(&mut entry.waypoints, waypoint_path) {
            return Some(found);
        }
    }
    None
}

/// Immutable variant of `find_waypoint_mut`.
///
/// status: trail-side-trail-shape
pub fn find_waypoint<'a>(
    entries: &'a [WaypointEntry],
    waypoint_path: &str,
) -> Option<&'a WaypointEntry> {
    for entry in entries.iter() {
        if entry.path == waypoint_path {
            return Some(entry);
        }
        if let Some(found) = find_waypoint(&entry.waypoints, waypoint_path) {
            return Some(found);
        }
    }
    None
}

/// Collect every descendant path of `entry` (including `entry`'s own
/// path), depth-first. Used by `remove_waypoint`'s cascade-delete pass.
///
/// status: trail-side-trail-shape
pub fn collect_descendant_paths(entry: &WaypointEntry) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(e: &WaypointEntry, out: &mut Vec<String>) {
        out.push(e.path.clone());
        for child in &e.waypoints {
            walk(child, out);
        }
    }
    walk(entry, &mut out);
    out
}

/// Remove the entry whose path is `waypoint_path` from the recursive
/// tree rooted at `entries`. Returns the removed entry on success.
/// Walks every level until the match is found.
fn remove_waypoint_from_tree(
    entries: &mut Vec<WaypointEntry>,
    waypoint_path: &str,
) -> Option<WaypointEntry> {
    if let Some(pos) = entries.iter().position(|e| e.path == waypoint_path) {
        return Some(entries.remove(pos));
    }
    for entry in entries.iter_mut() {
        if let Some(removed) = remove_waypoint_from_tree(&mut entry.waypoints, waypoint_path) {
            return Some(removed);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Listing / detail helpers (slice U1: drives `trails_list` /
// `trail_get` and the planned MCP `trails_list` / `trail_get` tools).
// Lives in `core` (not the adapter) because both surfaces share the same
// data-shaping policy: classify a vault note as a trail-doc by parsing
// its frontmatter, surface waypoint count + activation timestamp + title.
// ---------------------------------------------------------------------------

/// One row of `list`. Title is the trail-doc's basename without
/// `.md`; the UI may rewrite this once the trail-doc body grows a
/// markdown title affordance, but for v1 the basename is the user-facing
/// name (per the "create_trail names verbatim" path).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrailListItem {
    pub rel_path: String,
    pub trail_id: String,
    pub title: String,
    pub waypoint_count: u32,
    pub last_activated_at: Option<String>,
}

/// One waypoint of `get_trail`, post-resolution. The body is the
/// post-frontmatter slice of the waypoint-note (the user's annotation).
/// `children` carries the resolved sub-tree so the surface can render
/// side trails nested under their parent without re-walking the
/// frontmatter; `tree_path` is the dotted 1-based ordinal
/// (`"1"`, `"1.2"`, `"1.2.1"`).
///
/// status: trail-side-trail-shape
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedWaypoint {
    pub waypoint_rel: String,
    pub annotation_body: String,
    /// Vault-relative path of the source note this waypoint annotates.
    pub source_path: String,
    /// Vault-relative path of the trail-doc this waypoint belongs to.
    pub in_trail_path: String,
    pub resolution: ResolutionOutcome,
    pub children: Vec<ResolvedWaypoint>,
    pub tree_path: String,
}

/// Full detail bundle returned by `get_trail`. Mirrors the shape the
/// sidebar Trails-mode body needs (header chrome + ordered waypoint
/// cards) and the planned `mcp-tool-trail-get` tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrailDetail {
    pub rel_path: String,
    pub trail_id: String,
    pub last_activated_at: Option<String>,
    pub body: String,
    pub waypoints: Vec<ResolvedWaypoint>,
    /// Append cursor — names the waypoint under which the next
    /// unparented append lands. `None` = root-tail. Drives the
    /// little-person "you are here" glyph (`trail-append-cursor-indicator`).
    ///
    /// status: trail-append-cursor
    pub append_under: Option<String>,
}

/// Enumerate every trail-doc in the vault. Strategy: walk the indexer's
/// `path_ids` listing (cheap, already in memory) and try `parse_trail_doc_for`
/// on each `.md` file; rows that parse Ok are trail-docs. Notes whose
/// frontmatter doesn't carry `hiker.kind: trail` produce a parse error
/// and are silently skipped — same shape an external editor would see.
///
/// Pure data-shaping: the same listing drives the UI dropdown,
/// `mcp-tool-trails-list`, and `cli-trail-list`. Lives in core so the
/// three surfaces don't fork.
pub fn list(
    vault: &Vault,
    store: &Store,
    log: &OpLog,
) -> Result<Vec<TrailListItem>, HikerError> {
    let paths = store
        .all_note_paths()
        .map_err(|e| HikerError::Io(e.to_string()))?;
    let mut out = Vec::new();
    for rel in paths {
        if !rel.ends_with(".md") {
            continue;
        }
        let src = match vault.read_file(&rel) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let fm = match parse_trail_doc_for(&rel, &src) {
            Ok(fm) => fm,
            Err(_) => continue,
        };
        let mut count: u32 = 0;
        walk_waypoints_depth_first(&fm.waypoints, &mut |_, _, _| {
            count += 1;
        });
        // status: store-id-from-oplog
        let trail_id = match log.doc_id_for_path(&rel) {
            Ok(Some(id)) => id,
            _ => continue, // not yet seeded — skip rather than fabricate
        };
        out.push(TrailListItem {
            rel_path: rel.clone(),
            trail_id,
            title: {
                let base = rel.rsplit('/').next().unwrap_or(&rel);
                base.strip_suffix(".md").unwrap_or(base).to_string()
            },
            waypoint_count: count,
            last_activated_at: fm.last_activated_at,
        });
    }
    Ok(out)
}

/// Fetch a single trail's full body + ordered, resolved waypoints. The
/// trail-doc's `hiker.waypoints` array is the source of truth for order;
/// each entry is read from disk (annotation body) and resolved against
/// the index (`source_ref` resolution).
pub fn get_trail(
    vault: &Vault,
    store: &Store,
    log: &OpLog,
    trail_doc_rel: &str,
) -> Result<TrailDetail, HikerError> {
    let src = vault.read_file(trail_doc_rel)?;
    let fm = parse_trail_doc_for(trail_doc_rel, &src)
        .map_err(|e| HikerError::Io(format!("parse trail-doc: {e}")))?;
    // Body = post-frontmatter slice. `frontmatter::split` returns it.
    let body = split(&src).body.to_string();

    let waypoints = resolve_waypoint_tree(vault, store, trail_doc_rel, &fm.waypoints, "");

    // status: store-id-from-oplog
    let trail_id = log
        .doc_id_for_path(trail_doc_rel)
        .map_err(|e| HikerError::Io(e.to_string()))?
        .ok_or_else(|| {
            HikerError::NotFound(format!(
                "op-log doc_id missing for trail-doc: {trail_doc_rel}"
            ))
        })?;

    Ok(TrailDetail {
        rel_path: trail_doc_rel.to_string(),
        trail_id,
        last_activated_at: fm.last_activated_at,
        body,
        waypoints,
        append_under: fm.append_under,
    })
}

/// One row of `containing_note_with_paths`. Pairs the
/// derived-table hit's `trail_id` with the trail-doc's vault-relative
/// path so the UI can decide membership for any specific trail without
/// a second round-trip per trail.
///
/// status: trail-add-to-active-from-editor-verb
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainingNoteHit {
    pub trail_id: String,
    pub trail_doc_rel: String,
}

/// Reverse-lookup: which trails contain `source_rel` as a waypoint at
/// any depth. Resolves each derived-table `trail_id` to its trail-doc
/// rel-path via the same `list` walk the dropdown uses, so the
/// UI gets both halves in one call.
///
/// Drives the per-trail idempotency check used by the
/// "Add to active trail" verbs (tree row + editor pill) — `is the open
/// note already a waypoint of THIS trail?` is a `.some(h.trail_doc_rel
/// === active)` over the result.
///
/// status: trail-add-to-active-from-editor-verb
pub fn containing_note_with_paths(
    vault: &Vault,
    store: &Store,
    log: &OpLog,
    source_rel: &str,
) -> Result<Vec<ContainingNoteHit>, HikerError> {
    let hits = store
        .trails_containing_note(source_rel)
        .map_err(|e| HikerError::Io(e.to_string()))?;
    if hits.is_empty() {
        return Ok(Vec::new());
    }
    // Build `trail_id -> trail_doc_rel` map by walking the trail-doc
    // listing once. List-trails is the same data-shaping used by the
    // sidebar dropdown so the two stay consistent (e.g. trail-doc
    // renamed but indexer not yet caught up — both surfaces see the
    // same view).
    let listing = list(vault, store, log)?;
    let mut by_id: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for t in &listing {
        by_id.insert(t.trail_id.as_str(), t.rel_path.as_str());
    }
    let mut out = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for h in hits {
        if let Some(rel) = by_id.get(h.trail_id.as_str())
            && seen.insert(h.trail_id.clone())
        {
            out.push(ContainingNoteHit {
                trail_id: h.trail_id,
                trail_doc_rel: (*rel).to_string(),
            });
        }
        // Hit without a matching trail-doc row → trail-doc removed
        // since the indexer wrote the row; skip silently. The next
        // re-derive cleans the stale row up.
    }
    Ok(out)
}

/// Recursive resolver: walk a `Vec<WaypointEntry>` and produce a
/// matching `Vec<ResolvedWaypoint>` with `tree_path` filled in
/// (`"1"`, `"1.2"`, ...). Children are resolved depth-first; an
/// unreadable waypoint-note degrades to an `Orphan` resolution so the
/// UI can render a broken-reference card without crashing.
///
/// status: trail-side-trail-shape
fn resolve_waypoint_tree(
    vault: &Vault,
    store: &Store,
    trail_doc_rel: &str,
    entries: &[WaypointEntry],
    prefix: &str,
) -> Vec<ResolvedWaypoint> {
    let mut out: Vec<ResolvedWaypoint> = Vec::with_capacity(entries.len());
    for (idx, wp) in entries.iter().enumerate() {
        let one_based = idx + 1;
        let tree_path = if prefix.is_empty() {
            format!("{one_based}")
        } else {
            format!("{prefix}.{one_based}")
        };
        let (annotation_body, source_path, in_trail_path, resolution) =
            match vault.read_file(&wp.path) {
                Ok(wp_src) => match parse_waypoint(&wp_src) {
                    Ok(wfm) => {
                        let body = split(&wp_src).body.to_string();
                        let resolution =
                            resolve_reference(store, vault, &wfm.references)
                                .unwrap_or(ResolutionOutcome::Orphan);
                        (body, wfm.references, wfm.in_trail, resolution)
                    }
                    Err(_) => (
                        String::new(),
                        String::new(),
                        trail_doc_rel.to_string(),
                        ResolutionOutcome::Orphan,
                    ),
                },
                Err(_) => (
                    String::new(),
                    String::new(),
                    trail_doc_rel.to_string(),
                    ResolutionOutcome::Orphan,
                ),
            };
        let children = resolve_waypoint_tree(
            vault,
            store,
            trail_doc_rel,
            &wp.waypoints,
            &tree_path,
        );
        out.push(ResolvedWaypoint {
            waypoint_rel: wp.path.clone(),
            annotation_body,
            source_path,
            in_trail_path,
            resolution,
            children,
            tree_path,
        });
    }
    out
}
