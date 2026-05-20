//! Trails ops: mutation verbs (create/append/remove/delete trail and
//! waypoints, cursor + activation stamps) plus the path-remap surface
//! invoked from the indexer on note moves. The split keeps the parent
//! `trails::mod` focused on types + read-only helpers; everything that
//! writes to disk lives here.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::changes::{ChangeAppend, ChangeOp, Changes};
use crate::config::TrailsConfig;
use crate::error::HikerError;
use crate::hash::hash_str;
use crate::indexer::{IndexJob, IndexJobTx};
use crate::store::{new_id, Store};
use crate::trash::{Trash, TrashEntry};
use crate::vault::Vault;
use crate::watcher::Watcher;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use super::{
    append_change_best_effort, collect_descendant_ids, empty_trail_doc, empty_waypoint_note,
    find_waypoint, find_waypoint_mut, parse_trail_doc_for, parse_waypoint, remove_waypoint_from_tree,
    short_id_of, waypoint_filename, waypoints_dir_for, write_trail_doc_frontmatter,
    write_waypoint_frontmatter, DoubleLinkRef, WaypointEntry,
};

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
/// Borrowed bundle of inputs to `append_waypoint`. Bundles the four
/// vault-side handles plus the mutable `store` so the function stays
/// under the `too_many_arguments` threshold without losing the explicit
/// `&mut Store` lifetime that the underlying id-stamping helper needs.
pub struct AppendWaypointArgs<'a> {
    pub watcher: &'a Watcher,
    pub jobs: &'a IndexJobTx,
    pub vault: &'a Vault,
    pub changes: Option<&'a Arc<Changes>>,
    pub store: &'a mut Store,
    pub trail_doc_rel: &'a str,
    pub source_rel: &'a str,
    pub parent_waypoint_id: Option<&'a str>,
    pub annotation: Option<&'a str>,
}

pub async fn append_waypoint(
    args: AppendWaypointArgs<'_>,
) -> Result<AppendWaypointOutcome, HikerError> {
    let AppendWaypointArgs {
        watcher,
        jobs,
        vault,
        changes,
        store,
        trail_doc_rel,
        source_rel,
        parent_waypoint_id,
        annotation,
    } = args;
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
    if let Some(ann) = annotation
        && !ann.is_empty()
    {
        // Append annotation after the closing FM block. `assemble`
        // already produced a string ending right after `---\n`, so
        // the annotation slots in cleanly.
        waypoint_body.push_str(ann);
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
        if let Ok(src) = vault.read_file(new_rel)
            && let Ok(fm) = parse_waypoint(&src)
        {
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
        entries: &mut [WaypointEntry],
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

    if let Some(id) = waypoint_id
        && find_waypoint(&fm.waypoints, id).is_none()
    {
        return Err(HikerError::NotFound(format!(
            "waypoint id: {id}"
        )));
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
