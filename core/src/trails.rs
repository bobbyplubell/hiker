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

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_yml::Value as YamlValue;
use thiserror::Error;

use crate::changes::{ChangeAppend, ChangeOp, Changes};
use crate::config::TrailsConfig;
use crate::error::HikerError;
use crate::frontmatter::{assemble, merge_json_into_yaml, split, FrontmatterError};
use crate::hash::hash_str;
use crate::indexer::{IndexJob, IndexJobTx};
use crate::store::{new_id, Store};
use crate::trash::{Trash, TrashEntry};
use crate::vault::Vault;
use crate::watcher::Watcher;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

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
pub enum TrailsError {
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
    Assemble(#[from] FrontmatterError),
}

/// Parse a trail-doc's frontmatter. Caller MUST verify the source path
/// has a `.md` extension before calling this — a non-`.md` file with
/// `hiker.kind: trail` is not a trail per spec; `parse_trail_doc_for`
/// is the path-aware wrapper.
///
/// status: trail-doc-shape
pub fn parse_trail_doc(source: &str) -> Result<TrailDocFrontmatter, TrailsError> {
    let split_view = split(source);
    let fm = split_view.frontmatter.ok_or(TrailsError::MissingFrontmatter)?;
    let YamlValue::Mapping(map) = &fm else {
        return Err(TrailsError::NotMapping);
    };
    let hiker = map
        .get("hiker")
        .ok_or(TrailsError::MissingField("hiker"))?;
    let YamlValue::Mapping(hiker_map) = hiker else {
        return Err(TrailsError::MissingField("hiker"));
    };

    let kind = hiker_map
        .get("kind")
        .and_then(|v| v.as_str())
        .ok_or(TrailsError::MissingField("hiker.kind"))?;
    if kind != "trail" {
        return Err(TrailsError::KindMismatch {
            expected: "trail",
            found: kind.to_string(),
        });
    }

    let id = hiker_map
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or(TrailsError::MissingField("hiker.id"))?
        .to_string();

    let last_activated_at = hiker_map
        .get("last_activated_at")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let waypoints = match hiker_map.get("waypoints") {
        None => Vec::new(),
        Some(YamlValue::Sequence(seq)) => seq.iter().filter_map(parse_waypoint_entry).collect(),
        Some(_) => return Err(TrailsError::MissingField("hiker.waypoints")),
    };

    // status: trail-append-cursor
    // Missing key OR explicit `null` both map to None; a string maps to
    // Some. Anything else is silently treated as None (the cursor is
    // self-healing — see the stale-id branch in `append_waypoint`).
    let append_under = hiker_map
        .get("append_under")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

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
pub fn parse_trail_doc_for(rel: &str, source: &str) -> Result<TrailDocFrontmatter, TrailsError> {
    if !rel.ends_with(".md") {
        return Err(TrailsError::NotMarkdown(rel.to_string()));
    }
    parse_trail_doc(source)
}

/// Parse a waypoint-note's frontmatter.
///
/// status: waypoint-note-shape
pub fn parse_waypoint(source: &str) -> Result<WaypointFrontmatter, TrailsError> {
    let split_view = split(source);
    let fm = split_view.frontmatter.ok_or(TrailsError::MissingFrontmatter)?;
    let YamlValue::Mapping(map) = &fm else {
        return Err(TrailsError::NotMapping);
    };
    let hiker = map
        .get("hiker")
        .ok_or(TrailsError::MissingField("hiker"))?;
    let YamlValue::Mapping(hiker_map) = hiker else {
        return Err(TrailsError::MissingField("hiker"));
    };

    let kind = hiker_map
        .get("kind")
        .and_then(|v| v.as_str())
        .ok_or(TrailsError::MissingField("hiker.kind"))?;
    if kind != "waypoint" {
        return Err(TrailsError::KindMismatch {
            expected: "waypoint",
            found: kind.to_string(),
        });
    }

    let id = hiker_map
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or(TrailsError::MissingField("hiker.id"))?
        .to_string();

    let references = hiker_map
        .get("references")
        .and_then(parse_double_link)
        .ok_or(TrailsError::MissingField("hiker.references"))?;
    let in_trail = hiker_map
        .get("in_trail")
        .and_then(parse_double_link)
        .ok_or(TrailsError::MissingField("hiker.in_trail"))?;

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
) -> Result<String, TrailsError> {
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
    if let YamlValue::Mapping(top) = &mut existing {
        if let Some(YamlValue::Mapping(hiker)) = top.get_mut("hiker") {
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
) -> Result<String, TrailsError> {
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
    entries: &'a mut Vec<WaypointEntry>,
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
// Ops (slice 2): create_trail, append_waypoint, remove_waypoint, delete_trail
// ---------------------------------------------------------------------------

/// Outcome of a successful `create_trail` call. `trail_doc_rel` is the
/// vault-relative path of the just-written trail-doc; `trail_id` is the
/// minted ULID.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateTrailOutcome {
    pub trail_doc_rel: String,
    pub trail_id: String,
}

/// Outcome of a successful `append_waypoint` call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppendWaypointOutcome {
    pub waypoint_rel: String,
    pub waypoint_id: String,
    pub trail_id: String,
}

/// Outcome of a successful `remove_waypoint` call. `removed_count`
/// includes the target itself; `removed_paths` lists the vault-relative
/// paths of every waypoint-note moved to trash.
///
/// status: trails-mode-remove-waypoint-verb
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoveWaypointOutcome {
    pub removed_count: u32,
    pub removed_paths: Vec<String>,
}

fn append_change_best_effort(changes: Option<&Arc<Changes>>, append: ChangeAppend<'_>) {
    if let Some(c) = changes {
        if let Err(e) = c.append(append) {
            tracing::warn!(error = %e, "trails: changes append failed");
        }
    }
}

fn empty_trail_doc(trail_id: &str) -> String {
    // Minimal valid trail-doc frontmatter — no last_activated_at yet.
    format!("---\nhiker:\n  kind: trail\n  id: {trail_id}\n  waypoints: []\n---\n")
}

fn empty_waypoint_note(
    waypoint_id: &str,
    source_ref: &DoubleLinkRef,
    in_trail: &DoubleLinkRef,
) -> Result<String, TrailsError> {
    let fm = WaypointFrontmatter {
        id: waypoint_id.to_string(),
        references: source_ref.clone(),
        in_trail: in_trail.clone(),
    };
    // Body-source is just the empty string — no body, no extra newlines
    // beyond the closing `---\n` that `assemble` produces. Per spec,
    // `trail-empty-waypoint-body` requires zero bytes after the FM.
    write_waypoint_frontmatter("", &fm)
}

/// Create a new trail. Mints a ULID, writes the trail-doc to
/// `<new_trail_dir>/<name>.md` (auto-suffixed on collision), seeds the
/// hidden `.hiker/trails/<trail-id>/waypoints/` directory, appends a
/// `'created'` changes row (`author='user'`), and re-indexes the
/// trail-doc.
///
/// `name` is used verbatim as the basename; the function appends
/// `-N.md` (1..1000) only when there is a collision, mirroring
/// `core::ops::create_with_suffix`.
///
/// status: trails-default-location
/// status: trail-doc-shape
pub async fn create_trail(
    watcher: &Watcher,
    jobs: &IndexJobTx,
    vault: &Vault,
    changes: Option<&Arc<Changes>>,
    config: &TrailsConfig,
    name: &str,
) -> Result<CreateTrailOutcome, HikerError> {
    let folder = config.new_trail_dir.trim_end_matches('/');
    // Auto-create the folder (if non-empty) so the very first trail in a
    // vault doesn't fail with NotFound on the parent.
    if !folder.is_empty() {
        let abs = vault.abs_path(folder)?;
        if !abs.exists() {
            std::fs::create_dir_all(&abs)
                .map_err(|e| HikerError::Io(format!("create new_trail_dir: {e}")))?;
        }
    }

    let trail_id = new_id();
    let body = empty_trail_doc(&trail_id);

    // Write the trail-doc with auto-suffix on collision.
    let mut chosen: Option<String> = None;
    let base_candidate = if folder.is_empty() {
        format!("{name}.md")
    } else {
        format!("{folder}/{name}.md")
    };
    {
        let abs = vault.abs_path(&base_candidate)?;
        if !abs.exists() {
            watcher.suppress(base_candidate.clone());
            vault.write_file(&base_candidate, &body)?;
            chosen = Some(base_candidate);
        }
    }
    if chosen.is_none() {
        for n in 1..1000 {
            let candidate = if folder.is_empty() {
                format!("{name}-{n}.md")
            } else {
                format!("{folder}/{name}-{n}.md")
            };
            let abs = vault.abs_path(&candidate)?;
            if !abs.exists() {
                watcher.suppress(candidate.clone());
                vault.write_file(&candidate, &body)?;
                chosen = Some(candidate);
                break;
            }
        }
    }
    let trail_doc_rel = chosen.ok_or_else(|| {
        HikerError::AlreadyExists(format!("ran out of {name}-N candidates"))
    })?;

    // Seed the hidden waypoints dir so subsequent waypoint writes don't
    // need to mkdir on each hop.
    let waypoints_dir = waypoints_dir_for(&trail_id);
    let waypoints_abs = vault.abs_path(&waypoints_dir)?;
    if !waypoints_abs.exists() {
        std::fs::create_dir_all(&waypoints_abs)
            .map_err(|e| HikerError::Io(format!("create waypoint dir: {e}")))?;
    }

    // Re-suppress so the TTL window starts close to the notify event.
    watcher.suppress(trail_doc_rel.clone());

    append_change_best_effort(
        changes,
        ChangeAppend {
            path: &trail_doc_rel,
            op: ChangeOp::Created,
            author: "user",
            content_hash: Some(&hash_str(&body)),
            content: Some(body.as_bytes()),
            rename_from: None,
            metadata: serde_json::json!({"reason": "trails.create_trail"}),
        },
    );

    let _ = jobs
        .send(IndexJob::Upsert {
            rel_path: trail_doc_rel.clone(),
            force: false,
        })
        .await;

    Ok(CreateTrailOutcome {
        trail_doc_rel,
        trail_id,
    })
}

/// Append a waypoint to an existing trail.
///
/// 1. Lazy-stamps `hiker.id` on the source note (per `note-id-stamping`).
/// 2. Reads + parses the trail-doc to learn the trail id and current
///    waypoint count.
/// 3. Mints a waypoint ULID, writes the waypoint-note at
///    `.hiker/trails/<trail-id>/waypoints/<seq>--<source-basename>.md`
///    with empty body (per `trail-empty-waypoint-body`).
/// 4. Appends an entry to the trail-doc's `hiker.waypoints` and
///    rewrites it.
/// 5. Suppresses the watcher around both writes and re-indexes both.
/// 6. Appends one `'created'` and one `'modified'` row to changes.
///
/// status: waypoint-note-shape
/// status: trail-empty-waypoint-body
pub async fn append_waypoint(
    watcher: &Watcher,
    jobs: &IndexJobTx,
    vault: &Vault,
    changes: Option<&Arc<Changes>>,
    store: &mut Store,
    trail_doc_rel: &str,
    source_rel: &str,
    parent_waypoint_id: Option<&str>,
    annotation: Option<&str>,
) -> Result<AppendWaypointOutcome, HikerError> {
    // 1. Lazy-stamp the source. `store` is threaded through so the helper
    // can adopt the indexer's existing `path_ids` ULID rather than minting
    // a fresh one (which would later resolve as a PathConflict orphan in
    // Trails-mode rendering — see
    // `bug-id-stamping-mints-fresh-ulid-instead-of-adopting-path-ids`).
    let source_id =
        crate::ops::ensure_note_id_stamped(watcher, jobs, vault, changes, store, source_rel)
            .await?;

    // 2. Read the trail-doc.
    let trail_src = vault.read_file(trail_doc_rel)?;
    let mut fm = parse_trail_doc_for(trail_doc_rel, &trail_src)
        .map_err(|e| HikerError::Io(format!("parse trail-doc: {e}")))?;
    let trail_id = fm.id.clone();

    // 3. Mint waypoint id; compose the waypoint-note path + body.
    // Filename embeds a 6-char short-id derived from the waypoint ULID
    // so files never need renaming on reorder/re-parent.
    let waypoint_id = new_id();
    let basename = source_rel
        .rsplit('/')
        .next()
        .unwrap_or(source_rel)
        .strip_suffix(".md")
        .unwrap_or(source_rel);
    let waypoints_dir = waypoints_dir_for(&trail_id);

    // Ensure the waypoints dir exists (create_trail seeded it but the
    // user may have deleted it manually).
    let waypoints_abs = vault.abs_path(&waypoints_dir)?;
    if !waypoints_abs.exists() {
        std::fs::create_dir_all(&waypoints_abs)
            .map_err(|e| HikerError::Io(format!("create waypoint dir: {e}")))?;
    }

    // Resolve filename + collision-suffix. The short-id makes collisions
    // vanishingly rare per-trail, but if the same source basename hits
    // an existing file we append `_N` (1..1000) to disambiguate.
    let waypoint_rel = {
        let primary = waypoint_filename(basename, &waypoint_id);
        let primary_rel = format!("{waypoints_dir}/{primary}");
        let primary_abs = vault.abs_path(&primary_rel)?;
        if !primary_abs.exists() {
            primary_rel
        } else {
            let short_id = short_id_of(&waypoint_id);
            let mut chosen: Option<String> = None;
            for n in 2..1000 {
                let candidate =
                    format!("{waypoints_dir}/{basename}_{n}--{short_id}.md");
                let abs = vault.abs_path(&candidate)?;
                if !abs.exists() {
                    chosen = Some(candidate);
                    break;
                }
            }
            chosen.ok_or_else(|| {
                HikerError::AlreadyExists(format!(
                    "ran out of {basename}_N--<short-id>.md candidates"
                ))
            })?
        }
    };
    let source_ref = DoubleLinkRef {
        id: source_id,
        path: source_rel.to_string(),
    };
    let in_trail = DoubleLinkRef {
        id: trail_id.clone(),
        path: trail_doc_rel.to_string(),
    };
    let mut waypoint_body = empty_waypoint_note(&waypoint_id, &source_ref, &in_trail)
        .map_err(|e| HikerError::Io(format!("write waypoint fm: {e}")))?;
    // Honor optional annotation; None or empty → spec-mandated empty body.
    if let Some(ann) = annotation {
        if !ann.is_empty() {
            // Append annotation after the closing FM block. `assemble`
            // already produced a string ending right after `---\n`, so
            // the annotation slots in cleanly.
            waypoint_body.push_str(ann);
        }
    }

    watcher.suppress(waypoint_rel.clone());
    vault.write_file(&waypoint_rel, &waypoint_body)?;
    watcher.suppress(waypoint_rel.clone());

    append_change_best_effort(
        changes,
        ChangeAppend {
            path: &waypoint_rel,
            op: ChangeOp::Created,
            author: "user",
            content_hash: Some(&hash_str(&waypoint_body)),
            content: Some(waypoint_body.as_bytes()),
            rename_from: None,
            metadata: serde_json::json!({"reason": "trails.append_waypoint"}),
        },
    );

    let _ = jobs
        .send(IndexJob::Upsert {
            rel_path: waypoint_rel.clone(),
            force: false,
        })
        .await;

    // 4. Append entry to trail-doc.
    //
    // status: trail-append-cursor
    // Precedence (per `docs/trails.md` §"Append cursor"):
    //   explicit `parent_waypoint_id: Some(id)` > cursor > root-tail.
    //
    // - Explicit-parent (MCP, future explicit-parent callers): use it
    //   verbatim — explicit beats cursor so the typed surface stays
    //   honest.
    // - parent_waypoint_id None + cursor names a live waypoint: use
    //   cursor as the parent.
    // - parent_waypoint_id None + cursor names a stale id (or is None):
    //   root-tail append; warn-log the stale case so a hand-edit
    //   pointing at a deleted id surfaces in the trace.
    let new_entry = WaypointEntry {
        id: waypoint_id.clone(),
        path: waypoint_rel.clone(),
        waypoints: Vec::new(),
    };
    let effective_parent: Option<String> = match parent_waypoint_id {
        Some(id) => Some(id.to_string()),
        None => match fm.append_under.as_deref() {
            Some(cursor_id) => {
                if find_waypoint(&fm.waypoints, cursor_id).is_some() {
                    Some(cursor_id.to_string())
                } else {
                    tracing::warn!(
                        cursor = %cursor_id,
                        trail = %trail_doc_rel,
                        "trail-append-cursor: stale append_under id `{cursor_id}`, falling back to root"
                    );
                    None
                }
            }
            None => None,
        },
    };
    match effective_parent.as_deref() {
        None => fm.waypoints.push(new_entry),
        Some(pid) => {
            let parent = find_waypoint_mut(&mut fm.waypoints, pid).ok_or_else(|| {
                HikerError::NotFound(format!("parent waypoint id: {pid}"))
            })?;
            parent.waypoints.push(new_entry);
        }
    }
    // status: trail-append-cursor
    // The cursor is exclusively user-controlled per spec — appends do
    // NOT move it. Successive appends under the same cursor become
    // siblings (X.1, X.2, X.3); to dig deeper the user explicitly
    // moves the cursor via "Append from here" or by editing
    // `hiker.append_under` directly. Auto-advance was rejected because
    // it produces unintended deepening of the tree.
    let new_trail_src = write_trail_doc_frontmatter(&trail_src, &fm)
        .map_err(|e| HikerError::Io(format!("rewrite trail-doc: {e}")))?;

    watcher.suppress(trail_doc_rel.to_string());
    vault.write_file(trail_doc_rel, &new_trail_src)?;
    watcher.suppress(trail_doc_rel.to_string());

    append_change_best_effort(
        changes,
        ChangeAppend {
            path: trail_doc_rel,
            op: ChangeOp::Modified,
            author: "user",
            content_hash: Some(&hash_str(&new_trail_src)),
            content: Some(new_trail_src.as_bytes()),
            rename_from: None,
            metadata: serde_json::json!({"reason": "trails.append_waypoint"}),
        },
    );

    let _ = jobs
        .send(IndexJob::Upsert {
            rel_path: trail_doc_rel.to_string(),
            force: false,
        })
        .await;

    Ok(AppendWaypointOutcome {
        waypoint_rel,
        waypoint_id,
        trail_id,
    })
}

/// Remove a waypoint from a trail. Routes the waypoint-note delete
/// through `core::ops::delete` (so it lands in trash, restorable) and
/// rewrites the trail-doc to drop the entry.
///
/// status: trails-mode-remove-waypoint-verb
pub async fn remove_waypoint(
    watcher: &Watcher,
    jobs: &IndexJobTx,
    vault: &Vault,
    changes: Option<&Arc<Changes>>,
    _trash: &Trash,
    trail_doc_rel: &str,
    waypoint_id: &str,
) -> Result<RemoveWaypointOutcome, HikerError> {
    // Read trail-doc, find the target anywhere in the tree, and collect
    // every descendant path before mutating so the cascade pass has the
    // full list.
    let trail_src = vault.read_file(trail_doc_rel)?;
    let mut fm = parse_trail_doc_for(trail_doc_rel, &trail_src)
        .map_err(|e| HikerError::Io(format!("parse trail-doc: {e}")))?;

    let target = find_waypoint(&fm.waypoints, waypoint_id)
        .ok_or_else(|| HikerError::NotFound(format!("waypoint id: {waypoint_id}")))?;
    // Collect paths of the target + every descendant. We walk the
    // subtree directly (not via `Store::waypoints_of`) so the answer is
    // correct even if the derived index is stale — frontmatter is the
    // source of truth.
    let mut removed_paths: Vec<String> = Vec::new();
    fn collect_paths(e: &WaypointEntry, out: &mut Vec<String>) {
        out.push(e.path.clone());
        for c in &e.waypoints {
            collect_paths(c, out);
        }
    }
    collect_paths(target, &mut removed_paths);

    // status: trail-append-cursor
    // Cascade-delete safety: if the cursor lives inside the subtree
    // being removed (target itself, or any descendant), reset it to
    // None in the same rewrite. Compute the id-set from the live target
    // before mutating.
    let removed_ids: std::collections::HashSet<String> =
        collect_descendant_ids(target).into_iter().collect();
    let cursor_swept = fm
        .append_under
        .as_deref()
        .map(|c| removed_ids.contains(c))
        .unwrap_or(false);
    if cursor_swept {
        fm.append_under = None;
    }

    // Drop the subtree from frontmatter.
    let _removed_entry =
        remove_waypoint_from_tree(&mut fm.waypoints, waypoint_id).ok_or_else(|| {
            HikerError::NotFound(format!("waypoint id: {waypoint_id}"))
        })?;

    // Cascade-delete every waypoint-note (target + descendants) via
    // `core::ops::delete` so each lands in trash with its own
    // `'deleted'` changes row. Errors are surfaced after the pass — the
    // first failure short-circuits but the caller knows nothing about
    // partial success in v1; revisit if real use surfaces it.
    for rel in &removed_paths {
        let _entry =
            crate::ops::delete(watcher, jobs, vault, changes, rel).await?;
    }

    // Rewrite the trail-doc.
    let new_trail_src = write_trail_doc_frontmatter(&trail_src, &fm)
        .map_err(|e| HikerError::Io(format!("rewrite trail-doc: {e}")))?;
    watcher.suppress(trail_doc_rel.to_string());
    vault.write_file(trail_doc_rel, &new_trail_src)?;
    watcher.suppress(trail_doc_rel.to_string());

    append_change_best_effort(
        changes,
        ChangeAppend {
            path: trail_doc_rel,
            op: ChangeOp::Modified,
            author: "user",
            content_hash: Some(&hash_str(&new_trail_src)),
            content: Some(new_trail_src.as_bytes()),
            rename_from: None,
            metadata: serde_json::json!({"reason": "trails.remove_waypoint"}),
        },
    );

    let _ = jobs
        .send(IndexJob::Upsert {
            rel_path: trail_doc_rel.to_string(),
            force: false,
        })
        .await;

    let removed_count = removed_paths.len() as u32;
    Ok(RemoveWaypointOutcome {
        removed_count,
        removed_paths,
    })
}

/// Pre-compute the cascade size for a `remove_waypoint` of
/// `waypoint_id` without executing anything. UI uses this for the
/// confirm dialog count ("Remove this waypoint and N side-trail
/// waypoints?"). Returns the count *including* the target itself.
///
/// status: trails-mode-remove-waypoint-verb
pub fn descendant_count(
    vault: &Vault,
    trail_doc_rel: &str,
    waypoint_id: &str,
) -> Result<u32, HikerError> {
    let src = vault.read_file(trail_doc_rel)?;
    let fm = parse_trail_doc_for(trail_doc_rel, &src)
        .map_err(|e| HikerError::Io(format!("parse trail-doc: {e}")))?;
    let target = find_waypoint(&fm.waypoints, waypoint_id)
        .ok_or_else(|| HikerError::NotFound(format!("waypoint id: {waypoint_id}")))?;
    Ok(collect_descendant_ids(target).len() as u32)
}

/// Delete a trail. Cascade-deletes the trail-doc *and* its
/// `.hiker/trails/<trail-id>/` waypoint directory by calling
/// `core::ops::delete` on each path.
///
/// V1 trade-off: the trail-doc and the waypoint dir become two separate
/// trash entries. Restoring requires the user to restore both manually.
/// True atomic-pair semantics in `core::trash` is deferred — the simpler
/// shape ships first; revisit if real use shows users routinely
/// re-deleting half-restored trails. Returns the trail-doc's trash
/// entry (the more visible half).
///
/// status: trail-delete-cascade
pub async fn delete_trail(
    watcher: &Watcher,
    jobs: &IndexJobTx,
    vault: &Vault,
    changes: Option<&Arc<Changes>>,
    _trash: &Trash,
    trail_doc_rel: &str,
) -> Result<TrashEntry, HikerError> {
    // Pull the trail id off the trail-doc so we know which waypoint dir
    // to cascade. If the trail-doc can't be parsed (mid-edit, garbage),
    // fall back to deleting just the trail-doc — surface that clearly.
    let trail_id = match vault.read_file(trail_doc_rel) {
        Ok(src) => match parse_trail_doc_for(trail_doc_rel, &src) {
            Ok(fm) => Some(fm.id),
            Err(e) => {
                tracing::warn!(error = %e, path = %trail_doc_rel,
                    "delete_trail: trail-doc unparseable; cascading skipped");
                None
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, path = %trail_doc_rel,
                "delete_trail: read failed; trying delete anyway");
            None
        }
    };

    let entry = crate::ops::delete(watcher, jobs, vault, changes, trail_doc_rel).await?;

    if let Some(tid) = trail_id {
        let waypoint_dir = waypoints_dir_for(&tid);
        // The dir lives at `.hiker/trails/<id>/waypoints` but the spec's
        // delete-cascade scope is the parent `.hiker/trails/<id>/` so a
        // future `manifest/` sibling rides along. Delete the parent.
        let trail_root = format!(".hiker/trails/{tid}");
        let abs = vault.abs_path(&trail_root)?;
        if abs.exists() {
            // TODO(trail-delete-cascade): atomic-pair semantics in trash
            // are deferred — for v1 the trail-doc and the waypoint dir
            // become two separate trash entries; the user restores both.
            if let Err(e) =
                crate::ops::delete(watcher, jobs, vault, changes, &trail_root).await
            {
                tracing::warn!(error = %e, trail_id = %tid,
                    "delete_trail: cascade delete of waypoint dir failed");
            }
        } else {
            // Reference the helper so the dead-code lint stays quiet
            // when the waypoint dir is empty.
            let _ = waypoint_dir;
        }
    }

    Ok(entry)
}

// ---------------------------------------------------------------------------
// Reference resolution
// ---------------------------------------------------------------------------

/// Outcome of resolving a `DoubleLinkRef` against the index.
///
/// status: trail-reference-resolution
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResolutionOutcome {
    /// Both halves agree, or only the ULID is present and it resolves —
    /// either way no rewrite is needed.
    Resolved { rel_path: String, id: String },
    /// ULID resolves but to a different path than the recorded one.
    /// Caller rewrites the rel-path to `canonical_path` and appends a
    /// `core::changes` row tagged `author='user'`.
    SelfHeal {
        canonical_path: String,
        id: String,
        prior_path: String,
    },
    /// Path matches an indexed note but to a different ULID. The caller
    /// surfaces the path-conflict modal (Keep / Repoint / Break).
    ///
    /// status: trail-path-conflict-modal
    PathConflict {
        recorded_id: String,
        current_path_id: String,
        path: String,
    },
    /// Neither half resolves. Render as a greyed orphan card; user
    /// decides delete-or-fix.
    Orphan,
}

/// Resolve a stored double-link reference against the live index. See
/// `docs/trails.md` §"Resolution rule".
///
/// `vault` is accepted (not used in this implementation) so future
/// extensions (e.g. fs-existence fallback when the index hasn't ingested
/// the path yet) have a hook without a signature change.
///
/// status: trail-reference-resolution
pub fn resolve_reference(
    store: &Store,
    _vault: &Vault,
    link: &DoubleLinkRef,
) -> Result<ResolutionOutcome, HikerError> {
    let id_for_path = store
        .id_for_path(&link.path)
        .map_err(|e| HikerError::Io(e.to_string()))?;
    let path_for_id = store
        .path_for_id(&link.id)
        .map_err(|e| HikerError::Io(e.to_string()))?;

    match (path_for_id, id_for_path) {
        // ULID resolves; both agree.
        (Some(p), Some(pid)) if p == link.path && pid == link.id => {
            Ok(ResolutionOutcome::Resolved {
                rel_path: link.path.clone(),
                id: link.id.clone(),
            })
        }
        // ULID resolves to a different path. Whether or not the recorded
        // path itself currently resolves to a different note, the ULID
        // wins: rewrite the path.
        (Some(canonical), _) => Ok(ResolutionOutcome::SelfHeal {
            canonical_path: canonical,
            id: link.id.clone(),
            prior_path: link.path.clone(),
        }),
        // ULID doesn't resolve, but the path does — and to a different
        // ULID. Path-conflict modal territory.
        (None, Some(pid)) if pid != link.id => Ok(ResolutionOutcome::PathConflict {
            recorded_id: link.id.clone(),
            current_path_id: pid,
            path: link.path.clone(),
        }),
        // Neither half resolves.
        (None, None) => Ok(ResolutionOutcome::Orphan),
        // Defensive: ULID missing but the path matches a note whose id
        // happens to equal `link.id` — should be caught by the first arm,
        // but if reached, treat as Resolved.
        (None, Some(pid)) => Ok(ResolutionOutcome::Resolved {
            rel_path: link.path.clone(),
            id: pid,
        }),
    }
}

// ---------------------------------------------------------------------------
// Auto-update on note move (slice 3)
// ---------------------------------------------------------------------------

/// Rewrite trail-doc and waypoint-note path references when a note moves.
///
/// Invoked from the indexer task right after the path remap for an
/// explicit `IndexJob::Move` / `IndexJob::MoveFolder` succeeds, and from
/// the watcher-driven `IndexJob::Rename` branch. The ULID is unchanged —
/// the move is path-only — so the rewrite is a search-and-replace
/// targeting the `path` half of every double-link that points at the
/// moved note.
///
/// Three shapes of move are handled:
///   1. Source-note moved → every waypoint-note whose
///      `hiker.references.path == old_rel` gets that field rewritten to
///      `new_rel`. (The typical user-facing case.)
///   2. Trail-doc moved → every waypoint-note in that trail whose
///      `hiker.in_trail.path == old_rel` gets that field rewritten.
///   3. Waypoint-note moved → the parent trail-doc's
///      `hiker.waypoints[].path` entry is rewritten, plus the derived
///      `trail_waypoints.waypoint_path` column.
///
/// Watcher suppression is applied around each rewrite (when a watcher
/// is attached) so notify can't surface a stale Modified event for the
/// path the indexer is about to re-ingest. Each touched file gets one
/// `core::changes` row (`author='user'`,
/// `metadata.reason='trail-auto-update-on-note-move'`) and an
/// `IndexJob::Upsert` is enqueued so the derived `trail_waypoints` rows
/// re-derive cleanly.
///
/// Errors anywhere inside are logged via `tracing::warn!` but never
/// propagated up — the move's own changelog row already landed and
/// rolling back partial trails work is more complex than v1 needs.
/// Returns the count of files actually rewritten.
///
/// status: trail-auto-update-on-note-move
pub async fn on_note_moved(
    watcher: Option<&Watcher>,
    jobs: Option<&IndexJobTx>,
    vault: &Vault,
    changes: Option<&Arc<Changes>>,
    store: &mut Store,
    old_rel: &str,
    new_rel: &str,
) -> Result<usize, HikerError> {
    if old_rel == new_rel {
        return Ok(0);
    }
    let mut touched: usize = 0;

    // -- Case 1: a source note moved. Find every waypoint-note that
    // references `old_rel` as its source.
    let containing = match store.trails_containing_note(old_rel) {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = %e, path = %old_rel,
                "on_note_moved: trails_containing_note failed");
            Vec::new()
        }
    };
    for hit in &containing {
        if let Err(e) = rewrite_waypoint_source_path(
            watcher, jobs, vault, changes, &hit.waypoint_path, new_rel,
        )
        .await
        {
            tracing::warn!(error = %e, path = %hit.waypoint_path,
                "on_note_moved: source-rewrite of waypoint-note failed");
            continue;
        }
        touched += 1;
    }

    // -- Case 2: the moved note may itself be a trail-doc. Detect via the
    // path_ids → ULID round-trip (the trail-doc's `hiker.id` is the
    // trail_id, so `id_for_path(new_rel)` gives us the trail_id directly
    // when it resolves; the indexer's Move/Rename handler ran path remap
    // before us, so `new_rel` is the live row). For safety we also try
    // `old_rel` as a fallback — `path_ids` retains old paths via
    // `rename_note`'s upsert path.
    let trail_id_candidate = match store.id_for_path(new_rel) {
        Ok(Some(id)) => Some(id),
        _ => match store.id_for_path(old_rel) {
            Ok(Some(id)) => Some(id),
            _ => None,
        },
    };
    if let Some(trail_id) = trail_id_candidate {
        // Cheap check: did this id correspond to a trail-doc? `waypoints_of`
        // returns rows only for trail ids — an empty result is the no-op
        // case (regular note move, not a trail-doc move).
        let waypoints = store.waypoints_of(&trail_id).unwrap_or_default();
        if !waypoints.is_empty() {
            for wp in &waypoints {
                // Each waypoint-note's `hiker.in_trail.path` pointed at
                // the trail-doc's old path. Rewrite to new.
                if let Err(e) = rewrite_waypoint_in_trail_path(
                    watcher,
                    jobs,
                    vault,
                    changes,
                    &wp.waypoint_path,
                    new_rel,
                )
                .await
                {
                    tracing::warn!(error = %e, path = %wp.waypoint_path,
                        "on_note_moved: in_trail-rewrite of waypoint-note failed");
                    continue;
                }
                touched += 1;
            }
        }
    }

    // -- Case 3: the moved note may be a waypoint-note. The derived table
    // is keyed by `waypoint_path`; if `old_rel` matches any row, rewrite
    // its parent trail-doc's `hiker.waypoints[]` entry, then bulk-rename
    // the derived row's `waypoint_path` column.
    if old_rel.starts_with(".hiker/trails/") && old_rel.contains("/waypoints/") {
        // Look up the row's trail_id by walking trails_containing_note
        // won't work (matches source). Use a direct id_for_path:
        // the waypoint-note's own id → in_trail.id is its parent trail.
        // Easier: read the waypoint-note from disk (it's at new_rel now)
        // and parse its in_trail to learn the trail_id, then rewrite the
        // trail-doc.
        if let Ok(src) = vault.read_file(new_rel) {
            if let Ok(fm) = parse_waypoint(&src) {
                let trail_doc_rel = fm.in_trail.path.clone();
                if let Err(e) = rewrite_trail_doc_waypoint_entry(
                    watcher,
                    jobs,
                    vault,
                    changes,
                    &trail_doc_rel,
                    old_rel,
                    new_rel,
                )
                .await
                {
                    tracing::warn!(error = %e, path = %trail_doc_rel,
                        "on_note_moved: trail-doc waypoint-entry rewrite failed");
                } else {
                    touched += 1;
                }
                // Derived-table single-row rename via the prefix helper —
                // exact match acts as a degenerate prefix rewrite.
                if let Err(e) = store.rename_trail_waypoint_paths(old_rel, new_rel) {
                    tracing::warn!(error = %e,
                        "on_note_moved: rename_trail_waypoint_paths failed");
                }
            }
        }
    }

    Ok(touched)
}

/// Read + parse a waypoint-note, rewrite `hiker.references.path`
/// (id unchanged), persist via the standard write path with watcher
/// suppression + changelog append + reindex.
async fn rewrite_waypoint_source_path(
    watcher: Option<&Watcher>,
    jobs: Option<&IndexJobTx>,
    vault: &Vault,
    changes: Option<&Arc<Changes>>,
    waypoint_rel: &str,
    new_source_rel: &str,
) -> Result<(), HikerError> {
    let src = vault.read_file(waypoint_rel)?;
    let mut fm = parse_waypoint(&src)
        .map_err(|e| HikerError::Io(format!("parse waypoint: {e}")))?;
    if fm.references.path == new_source_rel {
        return Ok(()); // already canonical (idempotent re-runs)
    }
    fm.references.path = new_source_rel.to_string();
    let new_src = write_waypoint_frontmatter(&src, &fm)
        .map_err(|e| HikerError::Io(format!("write waypoint: {e}")))?;
    write_with_suppress_and_log(
        watcher, jobs, vault, changes, waypoint_rel, &new_src,
    )
    .await
}

/// Read + parse a waypoint-note, rewrite `hiker.in_trail.path`
/// (id unchanged), persist.
async fn rewrite_waypoint_in_trail_path(
    watcher: Option<&Watcher>,
    jobs: Option<&IndexJobTx>,
    vault: &Vault,
    changes: Option<&Arc<Changes>>,
    waypoint_rel: &str,
    new_trail_doc_rel: &str,
) -> Result<(), HikerError> {
    let src = vault.read_file(waypoint_rel)?;
    let mut fm = parse_waypoint(&src)
        .map_err(|e| HikerError::Io(format!("parse waypoint: {e}")))?;
    if fm.in_trail.path == new_trail_doc_rel {
        return Ok(());
    }
    fm.in_trail.path = new_trail_doc_rel.to_string();
    let new_src = write_waypoint_frontmatter(&src, &fm)
        .map_err(|e| HikerError::Io(format!("write waypoint: {e}")))?;
    write_with_suppress_and_log(
        watcher, jobs, vault, changes, waypoint_rel, &new_src,
    )
    .await
}

/// Read + parse a trail-doc, rewrite the `hiker.waypoints[]` entry
/// whose `path == old_waypoint_rel` to `new_waypoint_rel`, persist.
async fn rewrite_trail_doc_waypoint_entry(
    watcher: Option<&Watcher>,
    jobs: Option<&IndexJobTx>,
    vault: &Vault,
    changes: Option<&Arc<Changes>>,
    trail_doc_rel: &str,
    old_waypoint_rel: &str,
    new_waypoint_rel: &str,
) -> Result<(), HikerError> {
    let src = vault.read_file(trail_doc_rel)?;
    let mut fm = parse_trail_doc_for(trail_doc_rel, &src)
        .map_err(|e| HikerError::Io(format!("parse trail-doc: {e}")))?;
    fn walk_paths(
        entries: &mut Vec<WaypointEntry>,
        old_rel: &str,
        new_rel: &str,
        changed: &mut bool,
    ) {
        for w in entries.iter_mut() {
            if w.path == old_rel {
                w.path = new_rel.to_string();
                *changed = true;
            }
            walk_paths(&mut w.waypoints, old_rel, new_rel, changed);
        }
    }
    let mut changed = false;
    walk_paths(&mut fm.waypoints, old_waypoint_rel, new_waypoint_rel, &mut changed);
    if !changed {
        return Ok(());
    }
    let new_src = write_trail_doc_frontmatter(&src, &fm)
        .map_err(|e| HikerError::Io(format!("write trail-doc: {e}")))?;
    write_with_suppress_and_log(
        watcher, jobs, vault, changes, trail_doc_rel, &new_src,
    )
    .await
}

/// Common: pre-suppress watcher → write file → re-suppress watcher →
/// changelog append → enqueue reindex. Mirrors the suppression pattern
/// used by `append_waypoint` and friends.
async fn write_with_suppress_and_log(
    watcher: Option<&Watcher>,
    jobs: Option<&IndexJobTx>,
    vault: &Vault,
    changes: Option<&Arc<Changes>>,
    rel: &str,
    new_src: &str,
) -> Result<(), HikerError> {
    if let Some(w) = watcher {
        w.suppress(rel.to_string());
    }
    vault.write_file(rel, new_src)?;
    if let Some(w) = watcher {
        w.suppress(rel.to_string());
    }
    append_change_best_effort(
        changes,
        ChangeAppend {
            path: rel,
            op: ChangeOp::Modified,
            author: "user",
            content_hash: Some(&hash_str(new_src)),
            content: Some(new_src.as_bytes()),
            rename_from: None,
            metadata: serde_json::json!({"reason": "trail-auto-update-on-note-move"}),
        },
    );
    if let Some(j) = jobs {
        let _ = j
            .send(IndexJob::Upsert {
                rel_path: rel.to_string(),
                force: false,
            })
            .await;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Listing / detail helpers (slice U1: drives Tauri `trails_list` /
// `trail_get` and the planned MCP `trails_list` / `trail_get` tools).
// Lives in `core` (not the adapter) because both surfaces share the same
// data-shaping policy: classify a vault note as a trail-doc by parsing
// its frontmatter, surface waypoint count + activation timestamp + title.
// ---------------------------------------------------------------------------

/// One row of `list_trails`. Title is the trail-doc's basename without
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

fn basename_no_md(rel: &str) -> String {
    let base = rel.rsplit('/').next().unwrap_or(rel);
    base.strip_suffix(".md").unwrap_or(base).to_string()
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
pub fn list_trails(vault: &Vault, store: &Store) -> Result<Vec<TrailListItem>, HikerError> {
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
            title: basename_no_md(&rel),
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

/// One row of `trails_containing_note_with_paths`. Pairs the
/// derived-table hit's `trail_id` with the trail-doc's vault-relative
/// path so the UI can decide membership for any specific trail without
/// a second round-trip per trail.
///
/// status: trail-add-to-active-from-editor-verb
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrailsContainingNoteHit {
    pub trail_id: String,
    pub trail_doc_rel: String,
}

/// Reverse-lookup: which trails contain `source_rel` as a waypoint at
/// any depth. Resolves each derived-table `trail_id` to its trail-doc
/// rel-path via the same `list_trails` walk the dropdown uses, so the
/// UI gets both halves in one call.
///
/// Drives the per-trail idempotency check used by the
/// "Add to active trail" verbs (tree row + editor pill) — `is the open
/// note already a waypoint of THIS trail?` is a `.some(h.trail_doc_rel
/// === active)` over the result.
///
/// status: trail-add-to-active-from-editor-verb
pub fn trails_containing_note_with_paths(
    vault: &Vault,
    store: &Store,
    source_rel: &str,
) -> Result<Vec<TrailsContainingNoteHit>, HikerError> {
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
    let listing = list_trails(vault, store)?;
    let mut by_id: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for t in &listing {
        by_id.insert(t.trail_id.as_str(), t.rel_path.as_str());
    }
    let mut out = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for h in hits {
        if let Some(rel) = by_id.get(h.trail_id.as_str()) {
            if seen.insert(h.trail_id.clone()) {
                out.push(TrailsContainingNoteHit {
                    trail_id: h.trail_id,
                    trail_doc_rel: (*rel).to_string(),
                });
            }
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

/// Stamp `hiker.last_activated_at = <now>` on a trail-doc and return the
/// updated source. Suppresses the watcher around the write and queues a
/// re-index. Mirrors the shape `append_waypoint` uses for trail-doc
/// rewrites.
///
/// status: trails-mode-active-trail-dropdown
pub async fn stamp_last_activated_at(
    watcher: &Watcher,
    jobs: &IndexJobTx,
    vault: &Vault,
    changes: Option<&Arc<Changes>>,
    trail_doc_rel: &str,
) -> Result<(), HikerError> {
    let src = vault.read_file(trail_doc_rel)?;
    let mut fm = parse_trail_doc_for(trail_doc_rel, &src)
        .map_err(|e| HikerError::Io(format!("parse trail-doc: {e}")))?;
    let now = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| String::new());
    fm.last_activated_at = Some(now);
    let new_src = write_trail_doc_frontmatter(&src, &fm)
        .map_err(|e| HikerError::Io(format!("rewrite trail-doc: {e}")))?;

    watcher.suppress(trail_doc_rel.to_string());
    vault.write_file(trail_doc_rel, &new_src)?;
    watcher.suppress(trail_doc_rel.to_string());

    append_change_best_effort(
        changes,
        ChangeAppend {
            path: trail_doc_rel,
            op: ChangeOp::Modified,
            author: "user",
            content_hash: Some(&hash_str(&new_src)),
            content: Some(new_src.as_bytes()),
            rename_from: None,
            metadata: serde_json::json!({"reason": "trails.set_active"}),
        },
    );

    let _ = jobs
        .send(IndexJob::Upsert {
            rel_path: trail_doc_rel.to_string(),
            force: false,
        })
        .await;
    Ok(())
}

/// Set the trail-doc's append cursor. `waypoint_id: None` resets to
/// root-tail (the default flat-trail behavior). When `Some(id)`, the id
/// MUST resolve to a waypoint anywhere in the trail-doc's tree — we
/// refuse to silently write a stale cursor.
///
/// Same suppress + write + changes-append + reindex pattern every other
/// trails op uses. One `core::changes` row per file write, tagged
/// `metadata.reason = "trail-append-cursor"`.
///
/// status: trail-append-cursor
pub async fn set_append_cursor(
    watcher: &Watcher,
    jobs: &IndexJobTx,
    vault: &Vault,
    changes: Option<&Arc<Changes>>,
    trail_doc_rel: &str,
    waypoint_id: Option<&str>,
) -> Result<(), HikerError> {
    let src = vault.read_file(trail_doc_rel)?;
    let mut fm = parse_trail_doc_for(trail_doc_rel, &src)
        .map_err(|e| HikerError::Io(format!("parse trail-doc: {e}")))?;

    if let Some(id) = waypoint_id {
        if find_waypoint(&fm.waypoints, id).is_none() {
            return Err(HikerError::NotFound(format!(
                "waypoint id: {id}"
            )));
        }
    }
    fm.append_under = waypoint_id.map(|s| s.to_string());

    let new_src = write_trail_doc_frontmatter(&src, &fm)
        .map_err(|e| HikerError::Io(format!("rewrite trail-doc: {e}")))?;

    watcher.suppress(trail_doc_rel.to_string());
    vault.write_file(trail_doc_rel, &new_src)?;
    watcher.suppress(trail_doc_rel.to_string());

    append_change_best_effort(
        changes,
        ChangeAppend {
            path: trail_doc_rel,
            op: ChangeOp::Modified,
            author: "user",
            content_hash: Some(&hash_str(&new_src)),
            content: Some(new_src.as_bytes()),
            rename_from: None,
            metadata: serde_json::json!({"reason": "trail-append-cursor"}),
        },
    );

    let _ = jobs
        .send(IndexJob::Upsert {
            rel_path: trail_doc_rel.to_string(),
            force: false,
        })
        .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dl(id: &str, path: &str) -> DoubleLinkRef {
        DoubleLinkRef {
            id: id.to_string(),
            path: path.to_string(),
        }
    }

    fn we(id: &str, path: &str) -> WaypointEntry {
        WaypointEntry {
            id: id.to_string(),
            path: path.to_string(),
            waypoints: Vec::new(),
        }
    }

    #[test]
    fn waypoints_dir_for_uses_forward_slashes() {
        assert_eq!(
            waypoints_dir_for("01HRX"),
            ".hiker/trails/01HRX/waypoints"
        );
    }

    #[test]
    fn waypoint_filename_uses_short_id_suffix() {
        // 26-char ULID → last 6 chars upper-cased.
        let actual =
            waypoint_filename("raptor-paper", "01ARZ3NDEKTSV4RRFFQ69G5FAV");
        assert_eq!(actual, "raptor-paper--9G5FAV.md");
        // Lower-case input gets upper-cased.
        assert_eq!(short_id_of("aaaaaa01HWPabcdef"), "ABCDEF");
        // Short id falls back to upper-cased full string under 6 chars.
        assert_eq!(waypoint_filename("x", "ab"), "x--AB.md");
    }

    #[test]
    fn parse_trail_doc_round_trip() {
        let src = "---\nhiker:\n  kind: trail\n  id: 01HTRAIL\n  last_activated_at: 2026-05-10T12:00:00Z\n  waypoints:\n    - id: 01HWP1\n      path: .hiker/trails/01HTRAIL/waypoints/a--AAAAAA.md\n    - id: 01HWP2\n      path: .hiker/trails/01HTRAIL/waypoints/b--BBBBBB.md\n---\nbody prose\n";
        let parsed = parse_trail_doc(src).unwrap();
        assert_eq!(parsed.id, "01HTRAIL");
        assert_eq!(parsed.last_activated_at.as_deref(), Some("2026-05-10T12:00:00Z"));
        assert_eq!(parsed.waypoints.len(), 2);
        assert_eq!(
            parsed.waypoints[0],
            we("01HWP1", ".hiker/trails/01HTRAIL/waypoints/a--AAAAAA.md")
        );

        let written = write_trail_doc_frontmatter(src, &parsed).unwrap();
        let reparsed = parse_trail_doc(&written).unwrap();
        assert_eq!(parsed, reparsed);
        assert!(written.ends_with("body prose\n"));
    }

    // status: trail-side-trail-shape
    #[test]
    fn parse_trail_doc_round_trips_nested_tree() {
        let src = "---\nhiker:\n  kind: trail\n  id: 01HTRAIL\n  waypoints:\n    - id: ROOT1\n      path: .hiker/trails/01HTRAIL/waypoints/r1--AAAAAA.md\n      waypoints:\n        - id: CHILD1\n          path: .hiker/trails/01HTRAIL/waypoints/c1--BBBBBB.md\n          waypoints:\n            - id: GRAND1\n              path: .hiker/trails/01HTRAIL/waypoints/g1--CCCCCC.md\n    - id: ROOT2\n      path: .hiker/trails/01HTRAIL/waypoints/r2--DDDDDD.md\n---\nbody\n";
        let parsed = parse_trail_doc(src).unwrap();
        assert_eq!(parsed.waypoints.len(), 2);
        assert_eq!(parsed.waypoints[0].id, "ROOT1");
        assert_eq!(parsed.waypoints[0].waypoints.len(), 1);
        assert_eq!(parsed.waypoints[0].waypoints[0].id, "CHILD1");
        assert_eq!(parsed.waypoints[0].waypoints[0].waypoints.len(), 1);
        assert_eq!(parsed.waypoints[0].waypoints[0].waypoints[0].id, "GRAND1");
        assert_eq!(parsed.waypoints[1].id, "ROOT2");
        assert!(parsed.waypoints[1].waypoints.is_empty());

        let written = write_trail_doc_frontmatter(src, &parsed).unwrap();
        let reparsed = parse_trail_doc(&written).unwrap();
        assert_eq!(parsed, reparsed);
    }

    // status: trail-side-trail-shape
    #[test]
    fn parse_trail_doc_round_trips_empty_tree() {
        let src = "---\nhiker:\n  kind: trail\n  id: 01HTRAIL\n  waypoints: []\n---\nbody\n";
        let parsed = parse_trail_doc(src).unwrap();
        assert!(parsed.waypoints.is_empty());
        let written = write_trail_doc_frontmatter(src, &parsed).unwrap();
        let reparsed = parse_trail_doc(&written).unwrap();
        assert_eq!(parsed, reparsed);
    }

    // status: trail-side-trail-shape
    #[test]
    fn parse_trail_doc_old_flat_format_parses_as_root_tree() {
        // Pre-tree-format yaml: waypoints have no `waypoints:` key.
        let src = "---\nhiker:\n  kind: trail\n  id: 01HTRAIL\n  waypoints:\n    - id: A\n      path: a.md\n    - id: B\n      path: b.md\n---\n";
        let parsed = parse_trail_doc(src).unwrap();
        assert_eq!(parsed.waypoints.len(), 2);
        assert!(parsed.waypoints[0].waypoints.is_empty());
        assert!(parsed.waypoints[1].waypoints.is_empty());
    }

    // status: trail-side-trail-shape
    #[test]
    fn walk_waypoints_yields_depth_first_with_tree_paths() {
        let tree = vec![
            WaypointEntry {
                id: "R1".into(),
                path: "r1.md".into(),
                waypoints: vec![WaypointEntry {
                    id: "C1".into(),
                    path: "c1.md".into(),
                    waypoints: vec![we("G1", "g1.md")],
                }],
            },
            we("R2", "r2.md"),
        ];
        let mut visits: Vec<(Option<String>, String, String)> = Vec::new();
        walk_waypoints_depth_first(&tree, &mut |parent, e, path| {
            visits.push((
                parent.map(str::to_string),
                e.id.clone(),
                path.to_string(),
            ));
        });
        assert_eq!(
            visits,
            vec![
                (None, "R1".into(), "1".into()),
                (Some("R1".into()), "C1".into(), "1.1".into()),
                (Some("C1".into()), "G1".into(), "1.1.1".into()),
                (None, "R2".into(), "2".into()),
            ]
        );
    }

    #[test]
    fn parse_waypoint_round_trip() {
        let src = "---\nhiker:\n  kind: waypoint\n  id: 01HWP\n  references:\n    id: 01HSRC\n    path: research/raptor-paper.md\n  in_trail:\n    id: 01HTRAIL\n    path: trails/my-trail.md\n---\nuser annotation\n";
        let parsed = parse_waypoint(src).unwrap();
        assert_eq!(parsed.id, "01HWP");
        assert_eq!(parsed.references, dl("01HSRC", "research/raptor-paper.md"));
        assert_eq!(parsed.in_trail, dl("01HTRAIL", "trails/my-trail.md"));

        let written = write_waypoint_frontmatter(src, &parsed).unwrap();
        let reparsed = parse_waypoint(&written).unwrap();
        assert_eq!(parsed, reparsed);
        assert!(written.ends_with("user annotation\n"));
    }

    #[test]
    fn parse_trail_doc_for_rejects_non_markdown() {
        let src = "---\nhiker:\n  kind: trail\n  id: 01HTRAIL\n---\n";
        let err = parse_trail_doc_for("trails/my-trail.txt", src).unwrap_err();
        assert!(matches!(err, TrailsError::NotMarkdown(_)));
        assert!(parse_trail_doc_for("trails/my-trail.md", src).is_ok());
    }

    #[test]
    fn parse_trail_doc_rejects_wrong_kind() {
        let src = "---\nhiker:\n  kind: waypoint\n  id: 01HWP\n---\n";
        let err = parse_trail_doc(src).unwrap_err();
        assert!(matches!(err, TrailsError::KindMismatch { expected: "trail", .. }));
    }

    #[test]
    fn write_trail_doc_preserves_unknown_hiker_siblings() {
        // hiker.author and hiker.provenance must round-trip; only the
        // four trail-doc fields get rewritten.
        let src = "---\nhiker:\n  kind: trail\n  id: 01HTRAIL\n  author: user-authored\n  provenance: user\n  waypoints: []\n---\nbody\n";
        let parsed = parse_trail_doc(src).unwrap();
        let written = write_trail_doc_frontmatter(src, &parsed).unwrap();
        assert!(written.contains("author: user-authored"));
        assert!(written.contains("provenance: user"));
    }

    // status: trail-empty-waypoint-body
    #[test]
    fn empty_waypoint_note_has_zero_bytes_after_closing_fm() {
        let src = empty_waypoint_note(
            "01HWP",
            &dl("01HSRC", "research/raptor.md"),
            &dl("01HTRAIL", "trails/my-trail.md"),
        )
        .unwrap();
        // The body must end at the closing `---\n` with no further bytes.
        // (This is the "clean canvas" invariant from the spec.)
        let body_start = src.find("---\n").unwrap();
        // skip first `---\n`
        let after_first = body_start + "---\n".len();
        let close_rel = src[after_first..].find("---\n").unwrap();
        let close_abs = after_first + close_rel + "---\n".len();
        assert_eq!(close_abs, src.len(),
            "expected zero bytes after closing fm; got: {:?}",
            &src[close_abs..]);
    }

    #[test]
    fn write_trail_doc_preserves_top_level_non_hiker_fields() {
        let src = "---\ntitle: My Trail\nhiker:\n  kind: trail\n  id: 01HTRAIL\n  waypoints: []\ntags: [research]\n---\nbody\n";
        let parsed = parse_trail_doc(src).unwrap();
        let written = write_trail_doc_frontmatter(src, &parsed).unwrap();
        assert!(written.contains("title: My Trail"));
        assert!(written.contains("tags:"));
    }

    // -----------------------------------------------------------------
    // Ops tests (slice 2)
    // -----------------------------------------------------------------

    use crate::embed::{EmbedError, Embedder};
    use crate::indexer::{start_indexer, IndexerHandle};
    use crate::store::{NoteUpsert, Store};
    use std::sync::Arc;
    use tempfile::TempDir;

    struct ZeroEmbedder;
    impl Embedder for ZeroEmbedder {
        fn embed_batch(&self, batch: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
            Ok(batch.iter().map(|_| vec![0.0; 384]).collect())
        }
        fn version(&self) -> &str {
            "zero-test"
        }
        fn dim(&self) -> usize {
            384
        }
    }

    fn open_vault(td: &TempDir) -> Vault {
        Vault::open(td.path()).expect("open vault")
    }

    fn start(vault: Vault, store: Store) -> IndexerHandle {
        start_indexer(vault, store, || {
            Ok(Arc::new(ZeroEmbedder) as Arc<dyn Embedder>)
        })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn create_trail_writes_trail_doc_and_seeds_waypoints_dir() {
        let td = TempDir::new().unwrap();
        let vault = open_vault(&td);
        let watcher = Watcher::start(td.path()).unwrap();
        let store = Store::open(td.path()).unwrap();
        let idx = start(vault.clone(), store);

        let cfg = TrailsConfig {
            new_trail_dir: "trails/".into(),
        };
        let outcome =
            create_trail(&watcher, &idx.job_sender(), &vault, None, &cfg, "my-trail")
                .await
                .unwrap();
        assert_eq!(outcome.trail_doc_rel, "trails/my-trail.md");
        assert!(td.path().join(&outcome.trail_doc_rel).exists());
        let waypoints = td
            .path()
            .join(format!(".hiker/trails/{}/waypoints", outcome.trail_id));
        assert!(waypoints.exists() && waypoints.is_dir());

        // Auto-suffix on collision.
        let outcome2 =
            create_trail(&watcher, &idx.job_sender(), &vault, None, &cfg, "my-trail")
                .await
                .unwrap();
        assert_eq!(outcome2.trail_doc_rel, "trails/my-trail-1.md");

        idx.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn append_waypoint_writes_waypoint_and_updates_trail_doc() {
        let td = TempDir::new().unwrap();
        let vault = open_vault(&td);
        let watcher = Watcher::start(td.path()).unwrap();
        let store = Store::open(td.path()).unwrap();
        let idx = start(vault.clone(), store);
        let mut read_store = Store::open(td.path()).unwrap();

        let cfg = TrailsConfig {
            new_trail_dir: "trails/".into(),
        };
        let trail =
            create_trail(&watcher, &idx.job_sender(), &vault, None, &cfg, "t").await.unwrap();

        // Source note.
        std::fs::create_dir_all(td.path().join("research")).unwrap();
        std::fs::write(td.path().join("research/raptor.md"), "body").unwrap();

        let out = append_waypoint(
            &watcher,
            &idx.job_sender(),
            &vault,
            None,
            &mut read_store,
            &trail.trail_doc_rel,
            "research/raptor.md",
            None,
            None,
        )
        .await
        .unwrap();
        assert_eq!(out.trail_id, trail.trail_id);
        let waypoint_abs = td.path().join(&out.waypoint_rel);
        assert!(waypoint_abs.exists(), "waypoint file not written");
        let waypoint_src = std::fs::read_to_string(&waypoint_abs).unwrap();
        // Spec: empty body — zero bytes after the closing FM.
        assert!(waypoint_src.ends_with("---\n"),
            "waypoint body must end at closing fm: {waypoint_src:?}");

        // Trail-doc gained the entry.
        let trail_src = std::fs::read_to_string(td.path().join(&trail.trail_doc_rel))
            .unwrap();
        let fm = parse_trail_doc(&trail_src).unwrap();
        assert_eq!(fm.waypoints.len(), 1);
        assert_eq!(fm.waypoints[0].id, out.waypoint_id);
        assert_eq!(fm.waypoints[0].path, out.waypoint_rel);

        // Source had its `hiker.id` stamped via ensure_note_id_stamped.
        let source_src =
            std::fs::read_to_string(td.path().join("research/raptor.md")).unwrap();
        assert!(source_src.contains("hiker:") && source_src.contains("id:"),
            "expected source to have hiker.id stamped: {source_src:?}");

        idx.shutdown().await;
    }

    // bug-id-stamping-mints-fresh-ulid-instead-of-adopting-path-ids:
    // when the indexer has already minted a `path_ids[source]` ULID for
    // an ingested source note, `append_waypoint` must adopt that ULID
    // when stamping `hiker.id` to the source rather than minting a fresh
    // one. Otherwise the waypoint's `references.id` diverges from
    // `Store::id_for_path(source)`, and `resolve_reference` returns
    // `PathConflict` (renders as a "broken reference" orphan card in the
    // Trails sidebar in v1).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn append_waypoint_adopts_indexer_path_id_for_source() {
        let td = TempDir::new().unwrap();
        let vault = open_vault(&td);
        let watcher = Watcher::start(td.path()).unwrap();
        let store = Store::open(td.path()).unwrap();
        let idx = start(vault.clone(), store);
        let mut prog = idx.subscribe_progress();
        use crate::indexer::ProgressEvent;
        // Drain ModelLoaded.
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            async {
                loop {
                    match prog.recv().await {
                        Ok(ProgressEvent::ModelLoaded) => break,
                        Ok(_) => continue,
                        Err(_) => break,
                    }
                }
            },
        )
        .await;

        // 1. Source note ingested by the indexer first, so `path_ids`
        // has a row keyed by `notes/source.md` with id Y.
        std::fs::create_dir_all(td.path().join("notes")).unwrap();
        std::fs::write(td.path().join("notes/source.md"), "body\n").unwrap();
        idx.index_path("notes/source.md").await.unwrap();
        wait_for_upsert(&mut prog, "notes/source.md").await;

        // Observe Y from a fresh reader (Store is owned by the indexer
        // task; opening a new connection is the per-command read pattern).
        let reader = Store::open(td.path()).unwrap();
        let path_ids_y = reader
            .id_for_path("notes/source.md")
            .unwrap()
            .expect("path_ids should have a row after the upsert drained");

        // 2. Append a waypoint that captures `notes/source.md`.
        let cfg = TrailsConfig {
            new_trail_dir: "trails/".into(),
        };
        let trail = create_trail(
            &watcher,
            &idx.job_sender(),
            &vault,
            None,
            &cfg,
            "t",
        )
        .await
        .unwrap();
        let mut read_store = Store::open(td.path()).unwrap();
        let wp = append_waypoint(
            &watcher,
            &idx.job_sender(),
            &vault,
            None,
            &mut read_store,
            &trail.trail_doc_rel,
            "notes/source.md",
            None,
            None,
        )
        .await
        .unwrap();

        // 3. Read back the waypoint-note's frontmatter and parse it.
        let waypoint_src =
            std::fs::read_to_string(td.path().join(&wp.waypoint_rel)).unwrap();
        let wp_fm = parse_waypoint(&waypoint_src).unwrap();

        // 4. The waypoint's `references.id` MUST match the indexer's
        // `path_ids[notes/source.md]`. This is the assertion that fails
        // pre-fix (the helper minted a fresh ULID, so the values differ).
        assert_eq!(
            wp_fm.references.id, path_ids_y,
            "waypoint references.id must match Store::id_for_path(source) \
             so resolve_reference returns Resolved, not PathConflict"
        );

        // 5. resolve_reference now sees both halves agree.
        let store_for_resolve = Store::open(td.path()).unwrap();
        let outcome =
            resolve_reference(&store_for_resolve, &vault, &wp_fm.references).unwrap();
        match outcome {
            ResolutionOutcome::Resolved { rel_path, id } => {
                assert_eq!(rel_path, "notes/source.md");
                assert_eq!(id, path_ids_y);
            }
            other => panic!(
                "expected Resolved, got {other:?} (this is the orphan/PathConflict \
                 rendering bug — references.id must equal path_ids id)"
            ),
        }

        idx.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remove_waypoint_drops_entry_and_moves_waypoint_to_trash() {
        let td = TempDir::new().unwrap();
        let vault = open_vault(&td);
        let watcher = Watcher::start(td.path()).unwrap();
        let store = Store::open(td.path()).unwrap();
        let idx = start(vault.clone(), store);

        let cfg = TrailsConfig {
            new_trail_dir: "trails/".into(),
        };
        let trail =
            create_trail(&watcher, &idx.job_sender(), &vault, None, &cfg, "t").await.unwrap();
        std::fs::write(td.path().join("a.md"), "body").unwrap();
        let mut read_store = Store::open(td.path()).unwrap();
        let wp = append_waypoint(
            &watcher,
            &idx.job_sender(),
            &vault,
            None,
            &mut read_store,
            &trail.trail_doc_rel,
            "a.md",
            None,
            None,
        )
        .await
        .unwrap();
        assert!(td.path().join(&wp.waypoint_rel).exists());

        let trash = Trash::open(td.path());
        remove_waypoint(
            &watcher,
            &idx.job_sender(),
            &vault,
            None,
            &trash,
            &trail.trail_doc_rel,
            &wp.waypoint_id,
        )
        .await
        .unwrap();

        // Waypoint file gone from its original location.
        assert!(!td.path().join(&wp.waypoint_rel).exists());
        // Trail-doc no longer carries the entry.
        let trail_src = std::fs::read_to_string(td.path().join(&trail.trail_doc_rel))
            .unwrap();
        let fm = parse_trail_doc(&trail_src).unwrap();
        assert!(fm.waypoints.is_empty());

        idx.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delete_trail_cascades_doc_and_waypoint_dir() {
        let td = TempDir::new().unwrap();
        let vault = open_vault(&td);
        let watcher = Watcher::start(td.path()).unwrap();
        let store = Store::open(td.path()).unwrap();
        let idx = start(vault.clone(), store);

        let cfg = TrailsConfig {
            new_trail_dir: "trails/".into(),
        };
        let trail =
            create_trail(&watcher, &idx.job_sender(), &vault, None, &cfg, "t").await.unwrap();
        std::fs::write(td.path().join("a.md"), "body").unwrap();
        let mut read_store = Store::open(td.path()).unwrap();
        let _wp = append_waypoint(
            &watcher,
            &idx.job_sender(),
            &vault,
            None,
            &mut read_store,
            &trail.trail_doc_rel,
            "a.md",
            None,
            None,
        )
        .await
        .unwrap();
        let trail_root = td
            .path()
            .join(format!(".hiker/trails/{}", trail.trail_id));
        assert!(trail_root.exists());

        let trash = Trash::open(td.path());
        let _entry = delete_trail(
            &watcher,
            &idx.job_sender(),
            &vault,
            None,
            &trash,
            &trail.trail_doc_rel,
        )
        .await
        .unwrap();

        // Both halves are gone from their original locations.
        assert!(!td.path().join(&trail.trail_doc_rel).exists());
        assert!(!trail_root.exists(),
            "expected waypoint dir to be cascaded into trash");

        idx.shutdown().await;
    }

    // status: trail-reference-resolution
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resolve_reference_branches() {
        let td = TempDir::new().unwrap();
        let vault = open_vault(&td);
        let mut store = Store::open(td.path()).unwrap();

        // Index two notes manually so we control ids.
        let id_a = new_id();
        let id_b = new_id();
        store
            .upsert_note(NoteUpsert {
                id: &id_a,
                path: "alpha.md",
                content_hash: "h",
                mtime: 0,
                size: 0,
                indexed_at: 0,
                embedder_version: "t",
                chunks: vec![],
            })
            .unwrap();
        store
            .upsert_note(NoteUpsert {
                id: &id_b,
                path: "beta.md",
                content_hash: "h",
                mtime: 0,
                size: 0,
                indexed_at: 0,
                embedder_version: "t",
                chunks: vec![],
            })
            .unwrap();

        // Resolved: both halves agree.
        let r = resolve_reference(
            &store,
            &vault,
            &DoubleLinkRef {
                id: id_a.clone(),
                path: "alpha.md".into(),
            },
        )
        .unwrap();
        assert!(matches!(r, ResolutionOutcome::Resolved { .. }));

        // SelfHeal: id_a resolves to alpha.md, but recorded path is "old.md".
        let r = resolve_reference(
            &store,
            &vault,
            &DoubleLinkRef {
                id: id_a.clone(),
                path: "old.md".into(),
            },
        )
        .unwrap();
        match r {
            ResolutionOutcome::SelfHeal {
                canonical_path,
                id,
                prior_path,
            } => {
                assert_eq!(canonical_path, "alpha.md");
                assert_eq!(id, id_a);
                assert_eq!(prior_path, "old.md");
            }
            other => panic!("expected SelfHeal, got {other:?}"),
        }

        // PathConflict: unknown id, path matches beta.md (id = id_b).
        let r = resolve_reference(
            &store,
            &vault,
            &DoubleLinkRef {
                id: "01UNKNOWN".into(),
                path: "beta.md".into(),
            },
        )
        .unwrap();
        match r {
            ResolutionOutcome::PathConflict {
                recorded_id,
                current_path_id,
                path,
            } => {
                assert_eq!(recorded_id, "01UNKNOWN");
                assert_eq!(current_path_id, id_b);
                assert_eq!(path, "beta.md");
            }
            other => panic!("expected PathConflict, got {other:?}"),
        }

        // Orphan: neither id nor path resolve.
        let r = resolve_reference(
            &store,
            &vault,
            &DoubleLinkRef {
                id: "01NEVER".into(),
                path: "ghost.md".into(),
            },
        )
        .unwrap();
        assert!(matches!(r, ResolutionOutcome::Orphan));
    }

    // -----------------------------------------------------------------
    // Slice 3 tests: trail-auto-update-on-note-move
    // -----------------------------------------------------------------

    /// Wait for an Upsert of `path` to drain through the indexer's
    /// progress stream (Finished, Skipped, or Error all count). Avoids
    /// the test sleeping on indexer readiness in a flaky way.
    async fn wait_for_upsert(
        rx: &mut tokio::sync::broadcast::Receiver<crate::indexer::ProgressEvent>,
        path: &str,
    ) {
        use crate::indexer::ProgressEvent;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let ev = tokio::time::timeout(remaining, rx.recv())
                .await
                .expect("timed out waiting for upsert")
                .expect("progress channel closed");
            match &ev {
                ProgressEvent::Finished { path: p }
                | ProgressEvent::Skipped { path: p, .. }
                | ProgressEvent::Error { path: Some(p), .. }
                    if p == path =>
                {
                    return;
                }
                _ => {}
            }
        }
    }

    // status: trail-auto-update-on-note-move
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn move_note_rewrites_waypoint_source_path() {
        let td = TempDir::new().unwrap();
        let vault = open_vault(&td);
        let watcher = Watcher::start(td.path()).unwrap();
        let store = Store::open(td.path()).unwrap();
        let idx = start(vault.clone(), store);
        let mut prog = idx.subscribe_progress();
        // Drain ModelLoaded so subsequent waits don't see it.
        use crate::indexer::ProgressEvent;
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            async {
                loop {
                    match prog.recv().await {
                        Ok(ProgressEvent::ModelLoaded) => break,
                        Ok(_) => continue,
                        Err(_) => break,
                    }
                }
            },
        )
        .await;

        // Create the source note + a trail with one waypoint.
        std::fs::create_dir_all(td.path().join("notes")).unwrap();
        std::fs::write(td.path().join("notes/a.md"), "body").unwrap();
        let cfg = TrailsConfig {
            new_trail_dir: "trails/".into(),
        };
        let trail =
            create_trail(&watcher, &idx.job_sender(), &vault, None, &cfg, "t").await.unwrap();
        let mut read_store = Store::open(td.path()).unwrap();
        let wp = append_waypoint(
            &watcher,
            &idx.job_sender(),
            &vault,
            None,
            &mut read_store,
            &trail.trail_doc_rel,
            "notes/a.md",
            None,
            None,
        )
        .await
        .unwrap();

        // Drain progress for the trail-doc + waypoint upserts so the
        // derived `trail_waypoints` row exists before the move.
        wait_for_upsert(&mut prog, &trail.trail_doc_rel).await;
        wait_for_upsert(&mut prog, &wp.waypoint_rel).await;

        // Now move the source note.
        crate::ops::move_note(
            &watcher,
            &idx.job_sender(),
            &vault,
            None,
            "notes/a.md",
            "notes/b.md",
        )
        .await
        .unwrap();

        // The waypoint-note's `references.path` should now be "notes/b.md".
        let waypoint_src =
            std::fs::read_to_string(td.path().join(&wp.waypoint_rel)).unwrap();
        let fm = parse_waypoint(&waypoint_src).unwrap();
        assert_eq!(fm.references.path, "notes/b.md",
            "waypoint references.path should track the moved source");
        assert_eq!(fm.references.id, wp_source_id(&wp, &waypoint_src),
            "waypoint references.id must be unchanged by the path-only move");

        // Drain the auto-update reindex of the waypoint-note so the
        // derived `trail_waypoints` row picks up the new source_path.
        wait_for_upsert(&mut prog, &wp.waypoint_rel).await;

        let store2 = Store::open(td.path()).unwrap();
        let containing = store2.trails_containing_note("notes/b.md").unwrap();
        assert_eq!(containing.len(), 1,
            "derived row should now match the new source path");

        idx.shutdown().await;
    }

    /// Pull the source-id from the waypoint-note source for the
    /// "id is unchanged" assertion in the move test; just re-read the
    /// FM and return the references.id (the assertion compares it
    /// against itself, which only fails if the parse failed entirely).
    fn wp_source_id(_wp: &AppendWaypointOutcome, waypoint_src: &str) -> String {
        parse_waypoint(waypoint_src).unwrap().references.id
    }

    // status: trail-auto-update-on-note-move
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn move_folder_rewrites_referencing_waypoints() {
        let td = TempDir::new().unwrap();
        let vault = open_vault(&td);
        let watcher = Watcher::start(td.path()).unwrap();
        let store = Store::open(td.path()).unwrap();
        let idx = start(vault.clone(), store);
        let mut prog = idx.subscribe_progress();
        use crate::indexer::ProgressEvent;
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            async {
                loop {
                    match prog.recv().await {
                        Ok(ProgressEvent::ModelLoaded) => break,
                        Ok(_) => continue,
                        Err(_) => break,
                    }
                }
            },
        )
        .await;

        std::fs::create_dir_all(td.path().join("oldfolder")).unwrap();
        std::fs::write(td.path().join("oldfolder/x.md"), "body").unwrap();
        let cfg = TrailsConfig {
            new_trail_dir: "trails/".into(),
        };
        let trail =
            create_trail(&watcher, &idx.job_sender(), &vault, None, &cfg, "t").await.unwrap();
        let mut read_store = Store::open(td.path()).unwrap();
        let wp = append_waypoint(
            &watcher,
            &idx.job_sender(),
            &vault,
            None,
            &mut read_store,
            &trail.trail_doc_rel,
            "oldfolder/x.md",
            None,
            None,
        )
        .await
        .unwrap();
        wait_for_upsert(&mut prog, &trail.trail_doc_rel).await;
        wait_for_upsert(&mut prog, &wp.waypoint_rel).await;

        crate::ops::move_folder(
            &watcher,
            &idx.job_sender(),
            &vault,
            None,
            "oldfolder",
            "newfolder",
        )
        .await
        .unwrap();

        let waypoint_src =
            std::fs::read_to_string(td.path().join(&wp.waypoint_rel)).unwrap();
        let fm = parse_waypoint(&waypoint_src).unwrap();
        assert_eq!(fm.references.path, "newfolder/x.md");

        idx.shutdown().await;
    }

    // status: trail-auto-update-on-note-move
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn watcher_external_rename_triggers_trails_sweep() {
        let td = TempDir::new().unwrap();
        let vault = open_vault(&td);
        let watcher = Watcher::start(td.path()).unwrap();
        let store = Store::open(td.path()).unwrap();
        let idx = start(vault.clone(), store);
        let mut prog = idx.subscribe_progress();
        use crate::indexer::ProgressEvent;
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            async {
                loop {
                    match prog.recv().await {
                        Ok(ProgressEvent::ModelLoaded) => break,
                        Ok(_) => continue,
                        Err(_) => break,
                    }
                }
            },
        )
        .await;

        std::fs::create_dir_all(td.path().join("notes")).unwrap();
        std::fs::write(td.path().join("notes/src.md"), "body").unwrap();
        let cfg = TrailsConfig {
            new_trail_dir: "trails/".into(),
        };
        let trail =
            create_trail(&watcher, &idx.job_sender(), &vault, None, &cfg, "t").await.unwrap();
        let mut read_store = Store::open(td.path()).unwrap();
        let wp = append_waypoint(
            &watcher,
            &idx.job_sender(),
            &vault,
            None,
            &mut read_store,
            &trail.trail_doc_rel,
            "notes/src.md",
            None,
            None,
        )
        .await
        .unwrap();
        wait_for_upsert(&mut prog, &trail.trail_doc_rel).await;
        wait_for_upsert(&mut prog, &wp.waypoint_rel).await;

        // Simulate an external rename: do the fs rename ourselves, then
        // hand-feed an IndexJob::Rename to the indexer (the watcher
        // bridge would normally do this). Using the tx directly is the
        // closest test surface to the watcher path.
        std::fs::rename(
            td.path().join("notes/src.md"),
            td.path().join("notes/dst.md"),
        )
        .unwrap();
        idx.job_sender()
            .send(crate::indexer::IndexJob::Rename {
                from: "notes/src.md".into(),
                to: "notes/dst.md".into(),
            })
            .await
            .unwrap();

        // The waypoint-note's reference should be rewritten via the
        // Rename-arm trails sweep. Wait for the resulting reindex.
        wait_for_upsert(&mut prog, &wp.waypoint_rel).await;
        let waypoint_src =
            std::fs::read_to_string(td.path().join(&wp.waypoint_rel)).unwrap();
        let fm = parse_waypoint(&waypoint_src).unwrap();
        assert_eq!(fm.references.path, "notes/dst.md");

        idx.shutdown().await;
    }

    // -----------------------------------------------------------------
    // status: trail-append-cursor — cursor field round-trip + behavior
    // -----------------------------------------------------------------

    #[test]
    fn parse_trail_doc_round_trips_append_under_set() {
        let src = "---\nhiker:\n  kind: trail\n  id: 01HTRAIL\n  append_under: 01HWPCURSOR\n  waypoints:\n    - id: 01HWPCURSOR\n      path: .hiker/trails/01HTRAIL/waypoints/a--AAAAAA.md\n---\nbody\n";
        let parsed = parse_trail_doc(src).unwrap();
        assert_eq!(parsed.append_under.as_deref(), Some("01HWPCURSOR"));
        let written = write_trail_doc_frontmatter(src, &parsed).unwrap();
        let reparsed = parse_trail_doc(&written).unwrap();
        assert_eq!(parsed, reparsed);
        assert!(written.contains("append_under") && written.contains("01HWPCURSOR"),
            "expected append_under key + value in written frontmatter: {written:?}");
    }

    #[test]
    fn parse_trail_doc_without_append_under_round_trips_clean() {
        let src = "---\nhiker:\n  kind: trail\n  id: 01HTRAIL\n  waypoints: []\n---\nbody\n";
        let parsed = parse_trail_doc(src).unwrap();
        assert!(parsed.append_under.is_none());
        let written = write_trail_doc_frontmatter(src, &parsed).unwrap();
        assert!(!written.contains("append_under"),
            "expected no append_under key when cursor is None: {written:?}");
        let reparsed = parse_trail_doc(&written).unwrap();
        assert_eq!(parsed, reparsed);
    }

    #[test]
    fn parse_trail_doc_explicit_null_append_under_is_none() {
        let src = "---\nhiker:\n  kind: trail\n  id: 01HTRAIL\n  append_under: null\n  waypoints: []\n---\nbody\n";
        let parsed = parse_trail_doc(src).unwrap();
        assert!(parsed.append_under.is_none());
    }

    #[test]
    fn write_trail_doc_strips_existing_append_under_when_set_to_none() {
        // Pre-existing `append_under` in the YAML; we rewrite with the
        // cursor field set to None — the resulting frontmatter must NOT
        // carry the stale key (cascade-delete-resets-cursor path).
        let src = "---\nhiker:\n  kind: trail\n  id: 01HTRAIL\n  append_under: 01HSTALE\n  waypoints: []\n---\n";
        let mut parsed = parse_trail_doc(src).unwrap();
        parsed.append_under = None;
        let written = write_trail_doc_frontmatter(src, &parsed).unwrap();
        assert!(!written.contains("append_under"),
            "expected stale append_under stripped: {written:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn append_waypoint_consults_cursor_when_no_explicit_parent() {
        let td = TempDir::new().unwrap();
        let vault = open_vault(&td);
        let watcher = Watcher::start(td.path()).unwrap();
        let store = Store::open(td.path()).unwrap();
        let idx = start(vault.clone(), store);
        let mut read_store = Store::open(td.path()).unwrap();

        let cfg = TrailsConfig {
            new_trail_dir: "trails/".into(),
        };
        let trail =
            create_trail(&watcher, &idx.job_sender(), &vault, None, &cfg, "t").await.unwrap();

        std::fs::write(td.path().join("a.md"), "body").unwrap();
        std::fs::write(td.path().join("b.md"), "body").unwrap();
        std::fs::write(td.path().join("c.md"), "body").unwrap();

        // Cursor stays put across appends — A and B both land at root.
        let wp_a = append_waypoint(
            &watcher, &idx.job_sender(), &vault, None, &mut read_store,
            &trail.trail_doc_rel, "a.md", None, None,
        ).await.unwrap();
        let wp_b = append_waypoint(
            &watcher, &idx.job_sender(), &vault, None, &mut read_store,
            &trail.trail_doc_rel, "b.md", None, None,
        ).await.unwrap();

        // Point cursor at A; append C with no explicit parent → should
        // land as a child of A, not of B.
        set_append_cursor(&watcher, &idx.job_sender(), &vault, None,
            &trail.trail_doc_rel, Some(&wp_a.waypoint_id)).await.unwrap();
        let wp_c = append_waypoint(
            &watcher, &idx.job_sender(), &vault, None, &mut read_store,
            &trail.trail_doc_rel, "c.md", None, None,
        ).await.unwrap();

        let trail_src =
            std::fs::read_to_string(td.path().join(&trail.trail_doc_rel)).unwrap();
        let fm = parse_trail_doc(&trail_src).unwrap();
        // Two roots: A and B; C is a child of A.
        assert_eq!(fm.waypoints.len(), 2);
        let a = fm.waypoints.iter().find(|w| w.id == wp_a.waypoint_id).unwrap();
        let b = fm.waypoints.iter().find(|w| w.id == wp_b.waypoint_id).unwrap();
        assert_eq!(a.waypoints.len(), 1, "C should be a child of A");
        assert_eq!(a.waypoints[0].id, wp_c.waypoint_id);
        assert!(b.waypoints.is_empty());
        // Cursor unchanged — still pointing at A across the second append.
        assert_eq!(fm.append_under.as_deref(), Some(wp_a.waypoint_id.as_str()));

        idx.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn append_waypoint_explicit_parent_overrides_cursor() {
        let td = TempDir::new().unwrap();
        let vault = open_vault(&td);
        let watcher = Watcher::start(td.path()).unwrap();
        let store = Store::open(td.path()).unwrap();
        let idx = start(vault.clone(), store);
        let mut read_store = Store::open(td.path()).unwrap();

        let cfg = TrailsConfig {
            new_trail_dir: "trails/".into(),
        };
        let trail =
            create_trail(&watcher, &idx.job_sender(), &vault, None, &cfg, "t").await.unwrap();

        std::fs::write(td.path().join("a.md"), "b").unwrap();
        std::fs::write(td.path().join("b.md"), "b").unwrap();
        std::fs::write(td.path().join("c.md"), "b").unwrap();

        let wp_a = append_waypoint(
            &watcher, &idx.job_sender(), &vault, None, &mut read_store,
            &trail.trail_doc_rel, "a.md", None, None,
        ).await.unwrap();
        let wp_b = append_waypoint(
            &watcher, &idx.job_sender(), &vault, None, &mut read_store,
            &trail.trail_doc_rel, "b.md", None, None,
        ).await.unwrap();

        // Cursor = A, explicit parent = B → child of B; cursor stays at A
        // (appends never move the cursor — exclusively user-controlled).
        set_append_cursor(&watcher, &idx.job_sender(), &vault, None,
            &trail.trail_doc_rel, Some(&wp_a.waypoint_id)).await.unwrap();
        let wp_c = append_waypoint(
            &watcher, &idx.job_sender(), &vault, None, &mut read_store,
            &trail.trail_doc_rel, "c.md", Some(&wp_b.waypoint_id), None,
        ).await.unwrap();

        let trail_src =
            std::fs::read_to_string(td.path().join(&trail.trail_doc_rel)).unwrap();
        let fm = parse_trail_doc(&trail_src).unwrap();
        let b = fm.waypoints.iter().find(|w| w.id == wp_b.waypoint_id).unwrap();
        let a = fm.waypoints.iter().find(|w| w.id == wp_a.waypoint_id).unwrap();
        assert_eq!(b.waypoints.len(), 1, "C must be a child of B (explicit parent wins)");
        assert_eq!(b.waypoints[0].id, wp_c.waypoint_id);
        assert!(a.waypoints.is_empty(), "A should NOT gain C");
        assert_eq!(fm.append_under.as_deref(), Some(wp_a.waypoint_id.as_str()),
            "cursor stays at A — appends never move the cursor");

        idx.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn append_waypoint_does_not_move_cursor() {
        // Cursor is exclusively user-controlled per spec — successive
        // appends under the same cursor become siblings, not a deepening
        // ladder.
        let td = TempDir::new().unwrap();
        let vault = open_vault(&td);
        let watcher = Watcher::start(td.path()).unwrap();
        let store = Store::open(td.path()).unwrap();
        let idx = start(vault.clone(), store);
        let mut read_store = Store::open(td.path()).unwrap();

        let cfg = TrailsConfig {
            new_trail_dir: "trails/".into(),
        };
        let trail =
            create_trail(&watcher, &idx.job_sender(), &vault, None, &cfg, "t").await.unwrap();
        std::fs::write(td.path().join("a.md"), "b").unwrap();
        std::fs::write(td.path().join("b.md"), "b").unwrap();
        std::fs::write(td.path().join("c.md"), "b").unwrap();

        // Three appends with cursor = None → three siblings at root.
        let wp1 = append_waypoint(
            &watcher, &idx.job_sender(), &vault, None, &mut read_store,
            &trail.trail_doc_rel, "a.md", None, None,
        ).await.unwrap();
        let wp2 = append_waypoint(
            &watcher, &idx.job_sender(), &vault, None, &mut read_store,
            &trail.trail_doc_rel, "b.md", None, None,
        ).await.unwrap();

        let fm = parse_trail_doc(
            &std::fs::read_to_string(td.path().join(&trail.trail_doc_rel)).unwrap()
        ).unwrap();
        assert!(fm.append_under.is_none(), "cursor stays None across appends");
        assert_eq!(fm.waypoints.len(), 2, "wp1 and wp2 are siblings at root");
        assert!(fm.waypoints[0].waypoints.is_empty());
        assert!(fm.waypoints[1].waypoints.is_empty());

        // Move cursor to wp1; two appends under it become siblings under wp1.
        set_append_cursor(&watcher, &idx.job_sender(), &vault, None,
            &trail.trail_doc_rel, Some(&wp1.waypoint_id)).await.unwrap();
        let wp3 = append_waypoint(
            &watcher, &idx.job_sender(), &vault, None, &mut read_store,
            &trail.trail_doc_rel, "c.md", None, None,
        ).await.unwrap();

        let fm2 = parse_trail_doc(
            &std::fs::read_to_string(td.path().join(&trail.trail_doc_rel)).unwrap()
        ).unwrap();
        // Cursor still wp1, not wp3.
        assert_eq!(fm2.append_under.as_deref(), Some(wp1.waypoint_id.as_str()));
        let wp1_node = fm2.waypoints.iter().find(|w| w.id == wp1.waypoint_id).unwrap();
        assert_eq!(wp1_node.waypoints.len(), 1);
        assert_eq!(wp1_node.waypoints[0].id, wp3.waypoint_id);
        // wp2 is a sibling of wp1 at root, with no children.
        let wp2_node = fm2.waypoints.iter().find(|w| w.id == wp2.waypoint_id).unwrap();
        assert!(wp2_node.waypoints.is_empty());

        idx.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn append_waypoint_with_stale_cursor_falls_back_to_root() {
        let td = TempDir::new().unwrap();
        let vault = open_vault(&td);
        let watcher = Watcher::start(td.path()).unwrap();
        let store = Store::open(td.path()).unwrap();
        let idx = start(vault.clone(), store);
        let mut read_store = Store::open(td.path()).unwrap();

        let cfg = TrailsConfig {
            new_trail_dir: "trails/".into(),
        };
        let trail =
            create_trail(&watcher, &idx.job_sender(), &vault, None, &cfg, "t").await.unwrap();

        // Hand-set a stale cursor by reading + writing the trail-doc.
        let src = std::fs::read_to_string(td.path().join(&trail.trail_doc_rel)).unwrap();
        let mut fm = parse_trail_doc(&src).unwrap();
        fm.append_under = Some("01HDOESNOTEXIST".into());
        let rewritten = write_trail_doc_frontmatter(&src, &fm).unwrap();
        std::fs::write(td.path().join(&trail.trail_doc_rel), &rewritten).unwrap();

        std::fs::write(td.path().join("a.md"), "b").unwrap();
        let wp = append_waypoint(
            &watcher, &idx.job_sender(), &vault, None, &mut read_store,
            &trail.trail_doc_rel, "a.md", None, None,
        ).await.unwrap();

        let fm = parse_trail_doc(
            &std::fs::read_to_string(td.path().join(&trail.trail_doc_rel)).unwrap()
        ).unwrap();
        // Landed at root tail per the read-only fallback. Cursor stays
        // stale on disk — the spec treats stale `append_under` as null
        // on read with a warn, but doesn't auto-clean it (the next
        // user-driven cursor mutation overwrites it).
        assert_eq!(fm.waypoints.len(), 1);
        assert_eq!(fm.waypoints[0].id, wp.waypoint_id);
        assert_eq!(fm.append_under.as_deref(), Some("01HDOESNOTEXIST"));

        idx.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remove_waypoint_resets_cursor_when_removed() {
        let td = TempDir::new().unwrap();
        let vault = open_vault(&td);
        let watcher = Watcher::start(td.path()).unwrap();
        let store = Store::open(td.path()).unwrap();
        let idx = start(vault.clone(), store);
        let mut read_store = Store::open(td.path()).unwrap();
        let trash = Trash::open(td.path());

        let cfg = TrailsConfig {
            new_trail_dir: "trails/".into(),
        };
        let trail =
            create_trail(&watcher, &idx.job_sender(), &vault, None, &cfg, "t").await.unwrap();
        std::fs::write(td.path().join("a.md"), "b").unwrap();
        let wp = append_waypoint(
            &watcher, &idx.job_sender(), &vault, None, &mut read_store,
            &trail.trail_doc_rel, "a.md", None, None,
        ).await.unwrap();
        // Move cursor onto wp explicitly, then remove wp → cursor must reset.
        set_append_cursor(&watcher, &idx.job_sender(), &vault, None,
            &trail.trail_doc_rel, Some(&wp.waypoint_id)).await.unwrap();
        remove_waypoint(
            &watcher, &idx.job_sender(), &vault, None, &trash,
            &trail.trail_doc_rel, &wp.waypoint_id,
        ).await.unwrap();

        let fm = parse_trail_doc(
            &std::fs::read_to_string(td.path().join(&trail.trail_doc_rel)).unwrap()
        ).unwrap();
        assert!(fm.append_under.is_none(),
            "cursor must reset when its waypoint is removed");

        idx.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remove_waypoint_resets_cursor_when_ancestor_removed() {
        let td = TempDir::new().unwrap();
        let vault = open_vault(&td);
        let watcher = Watcher::start(td.path()).unwrap();
        let store = Store::open(td.path()).unwrap();
        let idx = start(vault.clone(), store);
        let mut read_store = Store::open(td.path()).unwrap();
        let trash = Trash::open(td.path());

        let cfg = TrailsConfig {
            new_trail_dir: "trails/".into(),
        };
        let trail =
            create_trail(&watcher, &idx.job_sender(), &vault, None, &cfg, "t").await.unwrap();
        std::fs::write(td.path().join("a.md"), "b").unwrap();
        std::fs::write(td.path().join("b.md"), "b").unwrap();
        let wp_y = append_waypoint(
            &watcher, &idx.job_sender(), &vault, None, &mut read_store,
            &trail.trail_doc_rel, "a.md", None, None,
        ).await.unwrap();
        // Set cursor on wp_y so the next append lands as a child.
        set_append_cursor(&watcher, &idx.job_sender(), &vault, None,
            &trail.trail_doc_rel, Some(&wp_y.waypoint_id)).await.unwrap();
        let wp_x = append_waypoint(
            &watcher, &idx.job_sender(), &vault, None, &mut read_store,
            &trail.trail_doc_rel, "b.md", None, None,
        ).await.unwrap();
        // Move cursor onto wp_x — the deeper descendant.
        set_append_cursor(&watcher, &idx.job_sender(), &vault, None,
            &trail.trail_doc_rel, Some(&wp_x.waypoint_id)).await.unwrap();

        // Remove the ancestor wp_y → cascades wp_x → cursor must reset.
        remove_waypoint(
            &watcher, &idx.job_sender(), &vault, None, &trash,
            &trail.trail_doc_rel, &wp_y.waypoint_id,
        ).await.unwrap();

        let fm = parse_trail_doc(
            &std::fs::read_to_string(td.path().join(&trail.trail_doc_rel)).unwrap()
        ).unwrap();
        assert!(fm.append_under.is_none(),
            "cursor must reset when an ancestor of the cursor is removed");
        assert!(fm.waypoints.is_empty(), "subtree should be gone");

        idx.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn remove_waypoint_preserves_cursor_when_sibling_removed() {
        let td = TempDir::new().unwrap();
        let vault = open_vault(&td);
        let watcher = Watcher::start(td.path()).unwrap();
        let store = Store::open(td.path()).unwrap();
        let idx = start(vault.clone(), store);
        let mut read_store = Store::open(td.path()).unwrap();
        let trash = Trash::open(td.path());

        let cfg = TrailsConfig {
            new_trail_dir: "trails/".into(),
        };
        let trail =
            create_trail(&watcher, &idx.job_sender(), &vault, None, &cfg, "t").await.unwrap();
        std::fs::write(td.path().join("a.md"), "b").unwrap();
        std::fs::write(td.path().join("b.md"), "b").unwrap();

        let wp_a = append_waypoint(
            &watcher, &idx.job_sender(), &vault, None, &mut read_store,
            &trail.trail_doc_rel, "a.md", None, None,
        ).await.unwrap();
        // Cursor stays None across appends, so wp_b is a root sibling.
        let wp_b = append_waypoint(
            &watcher, &idx.job_sender(), &vault, None, &mut read_store,
            &trail.trail_doc_rel, "b.md", None, None,
        ).await.unwrap();
        // Point cursor at wp_a.
        set_append_cursor(&watcher, &idx.job_sender(), &vault, None,
            &trail.trail_doc_rel, Some(&wp_a.waypoint_id)).await.unwrap();

        // Remove the sibling wp_b → cursor (wp_a) unchanged.
        remove_waypoint(
            &watcher, &idx.job_sender(), &vault, None, &trash,
            &trail.trail_doc_rel, &wp_b.waypoint_id,
        ).await.unwrap();

        let fm = parse_trail_doc(
            &std::fs::read_to_string(td.path().join(&trail.trail_doc_rel)).unwrap()
        ).unwrap();
        assert_eq!(fm.append_under.as_deref(), Some(wp_a.waypoint_id.as_str()),
            "removing a sibling of the cursor must NOT touch the cursor");

        idx.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_append_cursor_round_trip() {
        let td = TempDir::new().unwrap();
        let vault = open_vault(&td);
        let watcher = Watcher::start(td.path()).unwrap();
        let store = Store::open(td.path()).unwrap();
        let idx = start(vault.clone(), store);
        let mut read_store = Store::open(td.path()).unwrap();

        let cfg = TrailsConfig {
            new_trail_dir: "trails/".into(),
        };
        let trail =
            create_trail(&watcher, &idx.job_sender(), &vault, None, &cfg, "t").await.unwrap();
        std::fs::write(td.path().join("a.md"), "b").unwrap();
        let wp = append_waypoint(
            &watcher, &idx.job_sender(), &vault, None, &mut read_store,
            &trail.trail_doc_rel, "a.md", None, None,
        ).await.unwrap();

        // Set to None.
        set_append_cursor(&watcher, &idx.job_sender(), &vault, None,
            &trail.trail_doc_rel, None).await.unwrap();
        let fm = parse_trail_doc(
            &std::fs::read_to_string(td.path().join(&trail.trail_doc_rel)).unwrap()
        ).unwrap();
        assert!(fm.append_under.is_none());

        // Set to wp.
        set_append_cursor(&watcher, &idx.job_sender(), &vault, None,
            &trail.trail_doc_rel, Some(&wp.waypoint_id)).await.unwrap();
        let fm = parse_trail_doc(
            &std::fs::read_to_string(td.path().join(&trail.trail_doc_rel)).unwrap()
        ).unwrap();
        assert_eq!(fm.append_under.as_deref(), Some(wp.waypoint_id.as_str()));

        idx.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_append_cursor_rejects_unknown_id() {
        let td = TempDir::new().unwrap();
        let vault = open_vault(&td);
        let watcher = Watcher::start(td.path()).unwrap();
        let store = Store::open(td.path()).unwrap();
        let idx = start(vault.clone(), store);

        let cfg = TrailsConfig {
            new_trail_dir: "trails/".into(),
        };
        let trail =
            create_trail(&watcher, &idx.job_sender(), &vault, None, &cfg, "t").await.unwrap();

        let err = set_append_cursor(&watcher, &idx.job_sender(), &vault, None,
            &trail.trail_doc_rel, Some("01HBOGUS")).await.unwrap_err();
        assert!(matches!(err, HikerError::NotFound(_)),
            "set_append_cursor must reject a waypoint id that doesn't resolve: got {err:?}");

        idx.shutdown().await;
    }

    // status: trail-auto-update-on-note-move
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn on_note_moved_no_trails_returns_zero() {
        let td = TempDir::new().unwrap();
        let vault = open_vault(&td);
        let mut store = Store::open(td.path()).unwrap();
        // No trails exist; calling on_note_moved should do nothing.
        let touched = on_note_moved(
            None,
            None,
            &vault,
            None,
            &mut store,
            "notes/foo.md",
            "notes/bar.md",
        )
        .await
        .unwrap();
        assert_eq!(touched, 0);
    }
}
