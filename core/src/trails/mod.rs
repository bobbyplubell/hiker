//! Trails: curated, ordered walks through the vault. See `docs/trails.md`.
//!
//! This slice (slice 1 of 3) lands type definitions, frontmatter
//! parse/write helpers, and storage-layout helpers only. The ops
//! (`create_trail`, `append_waypoint`, `delete_trail`, `on_note_moved`)
//! and the derived `trail_waypoints` index table land in slice 2.
//!
//! A trail-doc is a regular markdown note with `hiker.kind: trail` in its
//! frontmatter; a waypoint-note lives under
//! `<vault>/.hiker/trails/<trail-id>/waypoints/<seq>--<source-basename>.md`
//! and carries `hiker.kind: waypoint`. Per-spec (see `docs/trails.md`
//! §"Trail-doc shape"), a non-`.md` file with `hiker.kind: trail` is NOT a
//! trail — callers verify the extension before parsing.
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
use crate::store::Store;
use crate::vault::Vault;

pub mod ops;
#[cfg(test)]
mod tests;

use ops::{resolve_reference, ResolutionOutcome};


/// A double-link reference: ULID is the canonical pointer (survives
/// renames via `path-ids`); rel-path is the externally-interoperable
/// half so a trail-doc opened in any other markdown editor stays legible.
///
/// status: trail-double-link-references
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoubleLinkRef {
    pub id: String,
    pub path: String,
}

/// One entry in the trail-doc's recursive `hiker.waypoints` tree. Each
/// entry is a double-link to a waypoint-note and may carry its own
/// `waypoints:` array of children forming a side trail. Children nest
/// arbitrarily deep; an entry with no `waypoints:` key (or an empty
/// array) is a leaf.
///
/// status: trail-side-trail-shape
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaypointEntry {
    pub id: String,
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
/// status: trail-doc-shape
/// status: trail-side-trail-shape
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrailDocFrontmatter {
    pub id: String,
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
    /// status: trail-append-cursor
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub append_under: Option<String>,
}

/// Parsed `hiker.*` frontmatter for a waypoint-note.
///
/// status: waypoint-note-shape
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaypointFrontmatter {
    pub id: String,
    pub references: DoubleLinkRef,
    pub in_trail: DoubleLinkRef,
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

    let id = hiker_map
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or(Error::MissingField("hiker.id"))?
        .to_string();

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
        id,
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

    let id = hiker_map
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or(Error::MissingField("hiker.id"))?
        .to_string();

    let references = hiker_map
        .get("references")
        .and_then(parse_double_link)
        .ok_or(Error::MissingField("hiker.references"))?;
    let in_trail = hiker_map
        .get("in_trail")
        .and_then(parse_double_link)
        .ok_or(Error::MissingField("hiker.in_trail"))?;

    Ok(WaypointFrontmatter {
        id,
        references,
        in_trail,
    })
}

fn parse_double_link(v: &YamlValue) -> Option<DoubleLinkRef> {
    let YamlValue::Mapping(m) = v else { return None };
    let id = m.get("id")?.as_str()?.to_string();
    let path = m.get("path")?.as_str()?.to_string();
    Some(DoubleLinkRef { id, path })
}

/// Recursive YAML-to-`WaypointEntry` parser. Children at any depth are
/// parsed via the same function. Pre-tree-format YAML (entries with no
/// `waypoints:` key) parses cleanly with an empty children vec, so old
/// flat trail-docs round-trip as a tree of all-root entries.
///
/// status: trail-side-trail-shape
fn parse_waypoint_entry(v: &YamlValue) -> Option<WaypointEntry> {
    let YamlValue::Mapping(m) = v else { return None };
    let id = m.get("id")?.as_str()?.to_string();
    let path = m.get("path")?.as_str()?.to_string();
    let waypoints = match m.get("waypoints") {
        Some(YamlValue::Sequence(seq)) => {
            seq.iter().filter_map(parse_waypoint_entry).collect()
        }
        _ => Vec::new(),
    };
    Some(WaypointEntry { id, path, waypoints })
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
    hiker_patch.insert("id".into(), serde_json::Value::String(fm.id.clone()));
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
        serde_json::json!({ "id": e.id, "path": e.path })
    } else {
        let children: Vec<_> = e.waypoints.iter().map(waypoint_entry_to_json).collect();
        serde_json::json!({
            "id": e.id,
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
    hiker_patch.insert("id".into(), serde_json::Value::String(fm.id.clone()));
    hiker_patch.insert("references".into(), double_link_to_json(&fm.references));
    hiker_patch.insert("in_trail".into(), double_link_to_json(&fm.in_trail));
    let patch = serde_json::json!({ "hiker": serde_json::Value::Object(hiker_patch) });
    merge_json_into_yaml(&mut existing, patch);
    Ok(assemble(&existing, split_view.body)?)
}

fn double_link_to_json(d: &DoubleLinkRef) -> serde_json::Value {
    serde_json::json!({ "id": d.id, "path": d.path })
}

/// Vault-relative path of the hidden waypoints dir for `trail_id`.
/// Always uses forward slashes, matching the rest of the vault path
/// surface.
///
/// status: trail-storage-layout
pub fn waypoints_dir_for(trail_id: &str) -> String {
    format!(".hiker/trails/{trail_id}/waypoints")
}

/// Filename for a waypoint-note. Per spec
/// (`docs/trails.md` §"Storage layout"), basename is
/// `<source-basename>--<short-id>.md` where `short-id` is the
/// upper-cased last 6 chars of the waypoint's ULID. Filename is a
/// stable identifier — never renamed on reorder/re-parent — so order +
/// tree shape live in the trail-doc's frontmatter alone.
///
/// `source_basename` should be the source-note's basename *without*
/// its `.md` extension; callers that need to embed an arbitrary string
/// (e.g. for a non-md source-derived note) pass the basename verbatim.
///
/// status: trail-storage-layout
pub fn waypoint_filename(source_basename: &str, waypoint_id: &str) -> String {
    let short_id = short_id_of(waypoint_id);
    format!("{source_basename}--{short_id}.md")
}

/// Last 6 chars of a ULID, upper-cased. ULIDs are 26 chars; the last 6
/// chars are random enough for the disambiguation purpose. Falls back
/// to the upper-cased full string for ULIDs shorter than 6 chars
/// (defensive — should never happen in production).
fn short_id_of(ulid: &str) -> String {
    let n = ulid.len();
    if n < 6 {
        ulid.to_uppercase()
    } else {
        ulid[n - 6..].to_uppercase()
    }
}

/// Walk the recursive waypoint tree depth-first in reading order.
/// `f` receives `(parent_id, entry, tree_path)` for every node;
/// `parent_id` is `None` for root-level entries; `tree_path` is the
/// 1-based dotted index path (`"1"`, `"1.2"`, `"1.2.1"`).
///
/// status: trail-side-trail-shape
pub fn walk_waypoints_depth_first<F>(entries: &[WaypointEntry], f: &mut F)
where
    F: FnMut(Option<&str>, &WaypointEntry, &str),
{
    fn walk<F: FnMut(Option<&str>, &WaypointEntry, &str)>(
        entries: &[WaypointEntry],
        parent_id: Option<&str>,
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
            f(parent_id, entry, &tree_path);
            if !entry.waypoints.is_empty() {
                walk(&entry.waypoints, Some(&entry.id), &tree_path, f);
            }
        }
    }
    walk(entries, None, "", f);
}

/// Find a waypoint entry by id anywhere in the recursive tree
/// (mutable). Returns the `&mut` entry the caller can edit (typically
/// to push a child onto its `waypoints` array).
///
/// status: trail-side-trail-shape
pub fn find_waypoint_mut<'a>(
    entries: &'a mut [WaypointEntry],
    waypoint_id: &str,
) -> Option<&'a mut WaypointEntry> {
    for entry in entries.iter_mut() {
        if entry.id == waypoint_id {
            // Reborrow through the indexed slot; the borrow checker is
            // happy with this shape.
            return Some(entry);
        }
        if let Some(found) = find_waypoint_mut(&mut entry.waypoints, waypoint_id) {
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
    waypoint_id: &str,
) -> Option<&'a WaypointEntry> {
    for entry in entries.iter() {
        if entry.id == waypoint_id {
            return Some(entry);
        }
        if let Some(found) = find_waypoint(&entry.waypoints, waypoint_id) {
            return Some(found);
        }
    }
    None
}

/// Collect every descendant id of `entry` (including `entry`'s own id),
/// depth-first. Used by `remove_waypoint`'s cascade-delete pass.
///
/// status: trail-side-trail-shape
pub fn collect_descendant_ids(entry: &WaypointEntry) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(e: &WaypointEntry, out: &mut Vec<String>) {
        out.push(e.id.clone());
        for child in &e.waypoints {
            walk(child, out);
        }
    }
    walk(entry, &mut out);
    out
}

/// Remove the entry whose id is `waypoint_id` from the recursive tree
/// rooted at `entries`. Returns the removed entry on success (the
/// caller typically already has a clone of its descendants from
/// `collect_descendant_ids`). Walks every level until the match is
/// found.
fn remove_waypoint_from_tree(
    entries: &mut Vec<WaypointEntry>,
    waypoint_id: &str,
) -> Option<WaypointEntry> {
    if let Some(pos) = entries.iter().position(|e| e.id == waypoint_id) {
        return Some(entries.remove(pos));
    }
    for entry in entries.iter_mut() {
        if let Some(removed) = remove_waypoint_from_tree(&mut entry.waypoints, waypoint_id) {
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
    pub waypoint_id: String,
    pub annotation_body: String,
    pub source_ref: DoubleLinkRef,
    pub in_trail: DoubleLinkRef,
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
pub fn list(vault: &Vault, store: &Store) -> Result<Vec<TrailListItem>, HikerError> {
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
        out.push(TrailListItem {
            rel_path: rel.clone(),
            trail_id: fm.id,
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
    trail_doc_rel: &str,
) -> Result<TrailDetail, HikerError> {
    let src = vault.read_file(trail_doc_rel)?;
    let fm = parse_trail_doc_for(trail_doc_rel, &src)
        .map_err(|e| HikerError::Io(format!("parse trail-doc: {e}")))?;
    // Body = post-frontmatter slice. `frontmatter::split` returns it.
    let body = split(&src).body.to_string();

    let waypoints = resolve_waypoint_tree(
        vault,
        store,
        trail_doc_rel,
        &fm.id,
        &fm.waypoints,
        "",
    );

    Ok(TrailDetail {
        rel_path: trail_doc_rel.to_string(),
        trail_id: fm.id,
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
    let listing = list(vault, store)?;
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
    trail_id: &str,
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
        let (annotation_body, source_ref, in_trail, resolution) =
            match vault.read_file(&wp.path) {
                Ok(wp_src) => match parse_waypoint(&wp_src) {
                    Ok(wfm) => {
                        let body = split(&wp_src).body.to_string();
                        let resolution = resolve_reference(store, vault, &wfm.references)
                            .unwrap_or(ResolutionOutcome::Orphan);
                        (body, wfm.references, wfm.in_trail, resolution)
                    }
                    Err(_) => (
                        String::new(),
                        DoubleLinkRef { id: String::new(), path: String::new() },
                        DoubleLinkRef {
                            id: trail_id.to_string(),
                            path: trail_doc_rel.to_string(),
                        },
                        ResolutionOutcome::Orphan,
                    ),
                },
                Err(_) => (
                    String::new(),
                    DoubleLinkRef { id: String::new(), path: String::new() },
                    DoubleLinkRef {
                        id: trail_id.to_string(),
                        path: trail_doc_rel.to_string(),
                    },
                    ResolutionOutcome::Orphan,
                ),
            };
        let children = resolve_waypoint_tree(
            vault,
            store,
            trail_doc_rel,
            trail_id,
            &wp.waypoints,
            &tree_path,
        );
        out.push(ResolvedWaypoint {
            waypoint_rel: wp.path.clone(),
            waypoint_id: wp.id.clone(),
            annotation_body,
            source_ref,
            in_trail,
            resolution,
            children,
            tree_path,
        });
    }
    out
}
