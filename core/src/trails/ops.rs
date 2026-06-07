//! Trails ops: mutation verbs (create/append/remove/delete trail and
//! waypoints, cursor + activation stamps) plus the path-remap surface
//! invoked from the indexer on note moves. The split keeps the parent
//! `trails::mod` focused on types + read-only helpers; everything that
//! writes to disk lives here.

use serde::{Deserialize, Serialize};

use crate::config::sections::TrailsConfig;
use crate::errors::HikerError;
use crate::indexer::{IndexJob, IndexJobTx};
use crate::oplog::OpLog;
use crate::store::Store;
use crate::trash::{Trash, Entry};
use crate::vault::Vault;
use crate::watcher::Watcher;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

// Only the most heavily-used parent items are imported; the rest are
// reached via explicit `super::` paths at their use sites so this file
// doesn't lean on a wide slice of its parent's namespace (per
// `check-splits` super-reach). The migration helpers near the bottom of
// the file are the main consumers of the rarer parent items.
use super::{dir, parse_trail_doc_for, write_trail_doc_frontmatter, WaypointEntry};

// ---------------------------------------------------------------------------
// Ops (slice 2): create_trail, append_waypoint, remove_waypoint, delete_trail
// ---------------------------------------------------------------------------

/// Outcome of a successful `create_trail` call. `trail_doc_rel` is the
/// vault-relative path of the just-written trail-doc; `trail_id` is the
/// op-log `doc_id` for that path (read after the write, since op-log
/// minted it during ingest of the new file).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateTrailOutcome {
    pub trail_doc_rel: String,
    pub trail_id: String,
}

/// Outcome of a successful `append_waypoint` call. The waypoint is
/// addressed by its vault-relative path; the optional op-log `doc_id`
/// for the waypoint-note is surfaced for callers that still need the
/// internal id (e.g. trail-graph viewers).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppendWaypointOutcome {
    pub waypoint_rel: String,
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

/// Create a new trail. Writes the trail-doc to `<new_trail_dir>/<name>.md`
/// (auto-suffixed on collision) and re-indexes it. The waypoint companion
/// folder is created lazily on the first `append_waypoint`
/// (`note-companion-folder`), so a fresh trail has no folder until it gets
/// a waypoint.
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
    log: &OpLog,
    vault: &Vault,
    config: &TrailsConfig,
    name: &str,
) -> Result<CreateTrailOutcome, HikerError> {
    let folder = config.new_trail_dir.trim_end_matches('/');
    // Auto-create the placement folder so the very first trail in a vault
    // doesn't fail with NotFound on the parent.
    if !folder.is_empty() {
        let abs = vault.abs_path(folder)?;
        if !abs.exists() {
            std::fs::create_dir_all(&abs)
                .map_err(|e| HikerError::Io(format!("create trail placement dir: {e}")))?;
        }
    }

    // Minimal valid trail-doc frontmatter — no `hiker.id` per
    // `trail-doc-shape` (the trail's storage key is the op-log's
    // doc_id, read from `doc-index.db` after the write).
    let body = "---\nhiker:\n  kind: trail\n  waypoints: []\n---\n".to_string();

    // Resolve the trail-doc path: the name verbatim with auto-suffix on
    // collision.
    let trail_doc_rel = {
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
        chosen.ok_or_else(|| {
            HikerError::AlreadyExists(format!("ran out of {name}-N candidates"))
        })?
    };

    // Read the op-log's doc_id for the trail-doc path; this is the
    // trail's storage key for the waypoint folder under `.hiker/trails/`.
    // op-log mints it on first ingest (`op-log-doc-id-bootstrap`); the
    // file we just wrote exists on disk, but the bootstrap pass may not
    // have run yet on this path — call into `commit_working` /
    // `external_edit`-equivalent semantics via the standard read after
    // the file is on disk. The `OpLog::doc_id_for_path` only returns
    // Some once seeded; if it isn't, seed by reading the just-written
    // file as a fresh document via the bootstrap routine.
    let trail_id =
        crate::ops::op_writes::doc_id_or_seed(log, vault, &trail_doc_rel, &body)?;

    // status: note-companion-folder
    // The companion folder is created lazily on the first `append_waypoint`,
    // not at trail creation — a trail with zero waypoints has no folder.

    // Re-suppress so the TTL window starts close to the notify event.
    watcher.suppress(trail_doc_rel.clone());

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
/// 1. Reads + parses the trail-doc to learn the trail id and current
///    waypoint count.
/// 2. Writes the waypoint-note into the trail-doc's companion folder
///    (`<dir>/<trail>/<source-basename>--<rand6>.md`, per
///    `note-companion-folder`), creating the folder lazily on this first
///    write, with empty body (per `trail-empty-waypoint-body`).
/// 3. Appends an entry to the trail-doc's `hiker.waypoints` and
///    rewrites it.
/// 4. Suppresses the watcher around both writes and re-indexes both.
///
/// status: waypoint-note-shape
/// status: trail-empty-waypoint-body
/// Borrowed bundle of inputs to `append_waypoint`. Bundles the three
/// vault-side handles plus the mutable `store` so the function stays
/// under the `too_many_arguments` threshold without losing the explicit
/// `&mut Store` lifetime that the underlying id-stamping helper needs.
pub struct AppendWaypointArgs<'a> {
    pub watcher: &'a Watcher,
    pub jobs: &'a IndexJobTx,
    pub log: &'a OpLog,
    pub vault: &'a Vault,
    pub trail_doc_rel: &'a str,
    pub source_rel: &'a str,
    /// Vault-relative waypoint-note path of the parent waypoint when this
    /// append should land as a child (a side-trail entry); `None` means
    /// "use the trail-doc's append cursor, or root-tail when the cursor
    /// is unset". status: trail-append-cursor
    pub parent_waypoint_path: Option<&'a str>,
    pub annotation: Option<&'a str>,
}

pub async fn append_waypoint(
    args: AppendWaypointArgs<'_>,
) -> Result<AppendWaypointOutcome, HikerError> {
    let AppendWaypointArgs {
        watcher,
        jobs,
        log,
        vault,
        trail_doc_rel,
        source_rel,
        parent_waypoint_path,
        annotation,
    } = args;

    // status: store-path-is-identity
    // No source-side id stamping — `note-id-stamping` retired with
    // path-as-identity. The source is referenced by its vault path; the
    // op-log keeps the path↔doc_id mapping internally.

    // 1. Read the trail-doc + look up its doc_id (the trail's identity).
    let trail_src = vault.read_file(trail_doc_rel)?;
    let mut fm = parse_trail_doc_for(trail_doc_rel, &trail_src)
        .map_err(|e| HikerError::Io(format!("parse trail-doc: {e}")))?;
    let trail_id = log
        .doc_id_for_path(trail_doc_rel)
        .map_err(|e| HikerError::Io(e.to_string()))?
        .ok_or_else(|| {
            HikerError::NotFound(format!(
                "op-log doc_id missing for trail-doc: {trail_doc_rel}"
            ))
        })?;

    // 2. Compose the waypoint-note path + body. The waypoints live in the
    //    trail-doc's companion folder; the filename embeds a 6-char random
    //    alphanumeric token so two waypoints with the same source basename
    //    don't collide. status: trail-storage-layout
    let basename = source_rel
        .rsplit('/')
        .next()
        .unwrap_or(source_rel)
        .strip_suffix(".md")
        .unwrap_or(source_rel);
    let waypoints_dir = super::waypoints_dir_for_doc(trail_doc_rel).ok_or_else(|| {
        HikerError::Io(format!("trail-doc path is not .md: {trail_doc_rel}"))
    })?;

    // status: note-companion-folder
    // Lazy creation: the companion folder is created here on the first
    // waypoint write, not at trail creation.
    let waypoints_abs = vault.abs_path(&waypoints_dir)?;
    if !waypoints_abs.exists() {
        std::fs::create_dir_all(&waypoints_abs)
            .map_err(|e| HikerError::Io(format!("create waypoint dir: {e}")))?;
    }

    // Resolve filename + collision-suffix. The random 6-char token makes
    // collisions vanishingly rare per-trail, but if a clash hits we
    // append `_N` (1..1000) before re-minting a fresh token.
    let waypoint_rel = {
        let primary_rel = format!("{waypoints_dir}/{}", super::waypoint_filename(basename));
        let primary_abs = vault.abs_path(&primary_rel)?;
        if !primary_abs.exists() {
            primary_rel
        } else {
            let mut chosen: Option<String> = None;
            for n in 2..1000 {
                let candidate = format!(
                    "{waypoints_dir}/{basename}_{n}--{}.md",
                    super::random_alphanumeric_6()
                );
                let abs = vault.abs_path(&candidate)?;
                if !abs.exists() {
                    chosen = Some(candidate);
                    break;
                }
            }
            chosen.ok_or_else(|| {
                HikerError::AlreadyExists(format!(
                    "ran out of {basename}_N--<rand6>.md candidates"
                ))
            })?
        }
    };
    let mut waypoint_body = {
        let wfm = super::WaypointFrontmatter {
            references: source_rel.to_string(),
            in_trail: trail_doc_rel.to_string(),
        };
        // Body-source is just the empty string — no body, no extra newlines
        // beyond the closing `---\n` that `assemble` produces. Per spec,
        // `trail-empty-waypoint-body` requires zero bytes after the FM.
        super::write_waypoint_frontmatter("", &wfm)
            .map_err(|e| HikerError::Io(format!("write waypoint fm: {e}")))?
    };
    // Honor optional annotation; None or empty → spec-mandated empty body.
    if let Some(ann) = annotation
        && !ann.is_empty()
    {
        waypoint_body.push_str(ann);
    }

    watcher.suppress(waypoint_rel.clone());
    vault.write_file(&waypoint_rel, &waypoint_body)?;
    watcher.suppress(waypoint_rel.clone());

    let _ = jobs
        .send(IndexJob::Upsert {
            rel_path: waypoint_rel.clone(),
            force: false,
        })
        .await;

    // 3. Append entry to trail-doc.
    //
    // status: trail-append-cursor
    // Precedence (per `docs/trails.md` §"Append cursor"):
    //   explicit `parent_waypoint_path: Some(p)` > cursor > root-tail.
    let new_entry = WaypointEntry {
        path: waypoint_rel.clone(),
        waypoints: Vec::new(),
    };
    let effective_parent: Option<String> = match parent_waypoint_path {
        Some(p) => Some(p.to_string()),
        None => match fm.append_under.as_deref() {
            Some(cursor_path) => {
                if super::find_waypoint(&fm.waypoints, cursor_path).is_some() {
                    Some(cursor_path.to_string())
                } else {
                    tracing::warn!(
                        cursor = %cursor_path,
                        trail = %trail_doc_rel,
                        "trail-append-cursor: stale append_under path `{cursor_path}`, falling back to root"
                    );
                    None
                }
            }
            None => None,
        },
    };
    match effective_parent.as_deref() {
        None => fm.waypoints.push(new_entry),
        Some(pp) => {
            let parent = super::find_waypoint_mut(&mut fm.waypoints, pp).ok_or_else(|| {
                HikerError::NotFound(format!("parent waypoint path: {pp}"))
            })?;
            parent.waypoints.push(new_entry);
        }
    }
    let new_trail_src = write_trail_doc_frontmatter(&trail_src, &fm)
        .map_err(|e| HikerError::Io(format!("rewrite trail-doc: {e}")))?;

    watcher.suppress(trail_doc_rel.to_string());
    vault.write_file(trail_doc_rel, &new_trail_src)?;
    watcher.suppress(trail_doc_rel.to_string());

    let _ = jobs
        .send(IndexJob::Upsert {
            rel_path: trail_doc_rel.to_string(),
            force: false,
        })
        .await;

    Ok(AppendWaypointOutcome {
        waypoint_rel,
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
    _trash: &Trash,
    trail_doc_rel: &str,
    waypoint_path: &str,
) -> Result<RemoveWaypointOutcome, HikerError> {
    // Read trail-doc, find the target anywhere in the tree, and collect
    // every descendant path before mutating so the cascade pass has the
    // full list.
    let trail_src = vault.read_file(trail_doc_rel)?;
    let mut fm = parse_trail_doc_for(trail_doc_rel, &trail_src)
        .map_err(|e| HikerError::Io(format!("parse trail-doc: {e}")))?;

    let target = super::find_waypoint(&fm.waypoints, waypoint_path)
        .ok_or_else(|| HikerError::NotFound(format!("waypoint path: {waypoint_path}")))?;
    // Collect paths of the target + every descendant.
    let removed_paths: Vec<String> = super::collect_descendant_paths(target);

    // status: trail-append-cursor
    // Cascade-delete safety: if the cursor lives inside the subtree
    // being removed, reset it to None in the same rewrite.
    let removed_set: std::collections::HashSet<&str> =
        removed_paths.iter().map(String::as_str).collect();
    let cursor_swept = fm
        .append_under
        .as_deref()
        .map(|c| removed_set.contains(c))
        .unwrap_or(false);
    if cursor_swept {
        fm.append_under = None;
    }

    // Drop the subtree from frontmatter.
    let _removed_entry =
        super::remove_waypoint_from_tree(&mut fm.waypoints, waypoint_path).ok_or_else(|| {
            HikerError::NotFound(format!("waypoint path: {waypoint_path}"))
        })?;

    // Cascade-delete every waypoint-note (target + descendants) via
    // `core::ops::delete` so each lands in trash. Errors are surfaced after
    // the pass — the first failure short-circuits but the caller knows
    // nothing about partial success in v1; revisit if real use surfaces it.
    for rel in &removed_paths {
        let _entry =
            crate::ops::file::delete(watcher, jobs, vault, rel).await?;
    }

    // Rewrite the trail-doc.
    let new_trail_src = write_trail_doc_frontmatter(&trail_src, &fm)
        .map_err(|e| HikerError::Io(format!("rewrite trail-doc: {e}")))?;
    watcher.suppress(trail_doc_rel.to_string());
    vault.write_file(trail_doc_rel, &new_trail_src)?;
    watcher.suppress(trail_doc_rel.to_string());

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
    waypoint_path: &str,
) -> Result<u32, HikerError> {
    let src = vault.read_file(trail_doc_rel)?;
    let fm = parse_trail_doc_for(trail_doc_rel, &src)
        .map_err(|e| HikerError::Io(format!("parse trail-doc: {e}")))?;
    let target = super::find_waypoint(&fm.waypoints, waypoint_path)
        .ok_or_else(|| HikerError::NotFound(format!("waypoint path: {waypoint_path}")))?;
    Ok(super::collect_descendant_paths(target).len() as u32)
}

/// Delete a trail. Cascade-deletes the trail-doc *and* its companion
/// folder (`<dir>/<trail>/`, holding the waypoint-notes) by calling
/// `core::ops::delete` on each path.
///
/// V1 trade-off: the trail-doc and the companion folder become two
/// separate trash entries. Restoring requires the user to restore both
/// manually. True atomic-pair semantics in `core::trash` is deferred — the
/// simpler shape ships first; revisit if real use shows users routinely
/// re-deleting half-restored trails. Returns the trail-doc's trash entry
/// (the more visible half).
///
/// `log` is retained in the signature for callers that pass it, but the
/// cascade scope is now derived from the trail-doc *path* (its companion
/// folder), not the op-log doc_id.
///
/// status: trail-delete-cascade
/// status: note-companion-folder
pub async fn delete_trail(
    watcher: &Watcher,
    jobs: &IndexJobTx,
    _log: &OpLog,
    vault: &Vault,
    _trash: &Trash,
    trail_doc_rel: &str,
) -> Result<Entry, HikerError> {
    // status: note-companion-folder
    // The cascade scope is the trail-doc's companion folder, computed from
    // its path. Capture it before the doc delete (the path string is
    // unaffected by removing the doc itself).
    let companion = super::waypoints_dir_for_doc(trail_doc_rel);

    let entry = crate::ops::file::delete(watcher, jobs, vault, trail_doc_rel).await?;

    if let Some(companion) = companion {
        let abs = vault.abs_path(&companion)?;
        if abs.exists() {
            // TODO(trail-delete-cascade): atomic-pair semantics in trash
            // are deferred — for v1 the trail-doc and the companion folder
            // become two separate trash entries; the user restores both.
            if let Err(e) =
                crate::ops::file::delete(watcher, jobs, vault, &companion).await
            {
                tracing::warn!(error = %e, companion = %companion,
                    "delete_trail: cascade delete of waypoint dir failed");
            }
        }
    }

    Ok(entry)
}

// ---------------------------------------------------------------------------
// Reference resolution
// ---------------------------------------------------------------------------

/// Outcome of resolving a path reference against the live index.
///
/// Under path-as-identity (`trail-path-references`) the reference IS a
/// vault path, so resolution collapses to a two-branch yes/no: either
/// the path lives in the index (`Resolved`) or it doesn't (`Orphan`).
/// The legacy `SelfHeal` and `PathConflict` branches retire — there's
/// no id half left to disagree with the path.
///
/// status: trail-reference-resolution
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResolutionOutcome {
    /// The path resolves to an indexed note.
    Resolved { rel_path: String },
    /// The path doesn't resolve. Render as a greyed orphan card; user
    /// decides delete-or-fix.
    Orphan,
}

/// Resolve a path reference against the live index. A note marked as
/// `skipped` in the index still counts as resolved — the user-visible
/// file exists at that path; the indexer just didn't ingest its body.
///
/// `vault` is accepted (not used in this implementation) so future
/// extensions (e.g. fs-existence fallback when the index hasn't ingested
/// the path yet) have a hook without a signature change.
///
/// status: trail-reference-resolution
pub fn resolve_reference(
    store: &Store,
    _vault: &Vault,
    rel_path: &str,
) -> Result<ResolutionOutcome, HikerError> {
    let exists = store
        .note_exists(rel_path)
        .map_err(|e| HikerError::Io(e.to_string()))?;
    if exists {
        Ok(ResolutionOutcome::Resolved {
            rel_path: rel_path.to_string(),
        })
    } else {
        Ok(ResolutionOutcome::Orphan)
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
/// path the indexer is about to re-ingest. Each touched file enqueues an
/// `IndexJob::Upsert` so the derived `trail_waypoints` rows re-derive
/// cleanly.
///
/// Errors anywhere inside are logged via `tracing::warn!` but never
/// propagated up — rolling back partial trails work is more complex than
/// v1 needs. Returns the count of files actually rewritten.
///
/// status: trail-auto-update-on-note-move
pub async fn on_note_moved(
    watcher: Option<&Watcher>,
    jobs: Option<&IndexJobTx>,
    log: Option<&OpLog>,
    vault: &Vault,
    store: &mut Store,
    old_rel: &str,
    new_rel: &str,
) -> Result<usize, HikerError> {
    if old_rel == new_rel {
        return Ok(0);
    }
    // Gather all store reads up front so the async fan-out doesn't hold
    // a `&Store` (rusqlite is !Sync, which would make the resulting
    // future !Send under tokio's multi-thread scheduler).
    let containing = match store.trails_containing_note(old_rel) {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = %e, path = %old_rel,
                "on_note_moved: trails_containing_note failed");
            Vec::new()
        }
    };
    // status: store-path-is-identity
    // The trail id IS the trail-doc's path (`op-log-path-identity`). The
    // derived `trail_waypoints` rows are keyed by the path the trail-doc had at
    // its last ingest — the OLD path, since this rewrite runs before the
    // re-ingest at the new path. So look up the waypoints under `old_rel`; a
    // trail-doc that was never ingested (no rows) falls back to the new path.
    let _ = log;
    let waypoints_of_trail = {
        let by_old = store.waypoints_of(old_rel).unwrap_or_default();
        if by_old.is_empty() {
            store.waypoints_of(new_rel).unwrap_or_default()
        } else {
            by_old
        }
    };

    let rctx = RewriteCtx { watcher, jobs, vault };
    let mut touched: usize = 0;
    touched += rctx.fan_out_source_moved(&containing, new_rel).await;
    touched += rctx.fan_out_trail_doc_moved(&waypoints_of_trail, new_rel).await;
    touched += rctx.fan_out_waypoint_moved(store, old_rel, new_rel).await;
    // status: note-companion-folder
    // A trail-doc rename moves its companion folder in the same
    // `move_note` op, so the moved trail-doc's own `hiker.waypoints[].path`
    // entries point into the *old* companion folder. Rewrite them by prefix
    // (old companion → new companion) when the moved note is a trail-doc.
    touched += rctx
        .rewrite_own_waypoint_paths_on_trail_doc_move(old_rel, new_rel)
        .await;
    Ok(touched)
}

/// Borrow-bundle for the three path-rewrite helpers in `on_note_moved`.
/// Methods on this struct stay exempt from `single_call_fn` and share the
/// suppression plumbing without repeating three-arg signatures.
struct RewriteCtx<'a> {
    watcher: Option<&'a Watcher>,
    jobs: Option<&'a IndexJobTx>,
    vault: &'a Vault,
}

impl<'a> RewriteCtx<'a> {
    /// Case 1: a source note moved. Rewrite every waypoint-note in
    /// `containing` to point at `new_rel`. Returns the count rewritten.
    async fn fan_out_source_moved(
        &self,
        containing: &[crate::store::dto::TrailContainingHit],
        new_rel: &str,
    ) -> usize {
        let mut touched = 0;
        for hit in containing {
            if let Err(e) = self
                .rewrite_waypoint_source_path(&hit.waypoint_path, new_rel)
                .await
            {
                tracing::warn!(error = %e, path = %hit.waypoint_path,
                    "on_note_moved: source-rewrite of waypoint-note failed");
                continue;
            }
            touched += 1;
        }
        touched
    }

    /// Case 2: the moved note may itself be a trail-doc. Rewrite the
    /// `hiker.in_trail.path` of every waypoint in `waypoints_of_trail`.
    async fn fan_out_trail_doc_moved(
        &self,
        waypoints_of_trail: &[crate::store::dto::WaypointRow],
        new_rel: &str,
    ) -> usize {
        let mut touched = 0;
        for wp in waypoints_of_trail {
            // Each waypoint-note's `hiker.in_trail.path` pointed at the
            // trail-doc's old path. Rewrite to new.
            if let Err(e) = self
                .rewrite_waypoint_in_trail_path(&wp.waypoint_path, new_rel)
                .await
            {
                tracing::warn!(error = %e, path = %wp.waypoint_path,
                    "on_note_moved: in_trail-rewrite of waypoint-note failed");
                continue;
            }
            touched += 1;
        }
        touched
    }

    /// Case 3: the moved note may be a waypoint-note. The derived table is
    /// keyed by `waypoint_path`; if `old_rel` matches any row, rewrite its
    /// parent trail-doc's `hiker.waypoints[]` entry, then bulk-rename the
    /// derived row's `waypoint_path` column.
    async fn fan_out_waypoint_moved(
        &self,
        store: &mut Store,
        old_rel: &str,
        new_rel: &str,
    ) -> usize {
        // status: note-companion-folder
        // Waypoints now live in the trail-doc's *visible* companion folder,
        // so a path-prefix gate no longer identifies them. Instead read the
        // moved note at its new path and parse it: only a note carrying
        // `hiker.kind: waypoint` parses, so a non-waypoint move short-
        // circuits here. The waypoint's `hiker.in_trail` names its parent
        // trail-doc, whose `hiker.waypoints[]` entry we then rewrite.
        if !new_rel.ends_with(".md") {
            return 0;
        }
        let Ok(src) = self.vault.read_file(new_rel) else { return 0 };
        let Ok(fm) = super::parse_waypoint(&src) else { return 0 };
        let trail_doc_rel = fm.in_trail.clone();
        // Drop `src` / `fm` borrows before the .await so no Store-derived
        // value lives across the suspension point.
        drop(src);
        drop(fm);
        let mut touched = 0;
        if let Err(e) = self
            .rewrite_trail_doc_waypoint_entry(&trail_doc_rel, old_rel, new_rel)
            .await
        {
            tracing::warn!(error = %e, path = %trail_doc_rel,
                "on_note_moved: trail-doc waypoint-entry rewrite failed");
        } else {
            touched += 1;
        }
        // Derived-table single-row rename via the prefix helper — exact
        // match acts as a degenerate prefix rewrite.
        if let Err(e) = store.rename_trail_waypoint_paths(old_rel, new_rel) {
            tracing::warn!(error = %e,
                "on_note_moved: rename_trail_waypoint_paths failed");
        }
        touched
    }

    /// Read + parse a waypoint-note, rewrite `hiker.references.path`
    /// (id unchanged), persist via the standard write path with watcher
    /// suppression + changelog append + reindex.
    async fn rewrite_waypoint_source_path(
        &self,
        waypoint_rel: &str,
        new_source_rel: &str,
    ) -> Result<(), HikerError> {
        let src = self.vault.read_file(waypoint_rel)?;
        let mut fm = super::parse_waypoint(&src)
            .map_err(|e| HikerError::Io(format!("parse waypoint: {e}")))?;
        if fm.references == new_source_rel {
            return Ok(()); // already canonical (idempotent re-runs)
        }
        fm.references = new_source_rel.to_string();
        let new_src = super::write_waypoint_frontmatter(&src, &fm)
            .map_err(|e| HikerError::Io(format!("write waypoint: {e}")))?;
        write_with_suppress_and_reindex(
            self.watcher, self.jobs, self.vault, waypoint_rel, &new_src,
        )
        .await
    }

    /// Read + parse a waypoint-note, rewrite `hiker.in_trail.path`,
    /// persist.
    async fn rewrite_waypoint_in_trail_path(
        &self,
        waypoint_rel: &str,
        new_trail_doc_rel: &str,
    ) -> Result<(), HikerError> {
        let src = self.vault.read_file(waypoint_rel)?;
        let mut fm = super::parse_waypoint(&src)
            .map_err(|e| HikerError::Io(format!("parse waypoint: {e}")))?;
        if fm.in_trail == new_trail_doc_rel {
            return Ok(());
        }
        fm.in_trail = new_trail_doc_rel.to_string();
        let new_src = super::write_waypoint_frontmatter(&src, &fm)
            .map_err(|e| HikerError::Io(format!("write waypoint: {e}")))?;
        write_with_suppress_and_reindex(
            self.watcher, self.jobs, self.vault, waypoint_rel, &new_src,
        )
        .await
    }

    /// Read + parse a trail-doc, rewrite the `hiker.waypoints[]` entry
    /// whose `path == old_waypoint_rel` to `new_waypoint_rel`, persist.
    async fn rewrite_trail_doc_waypoint_entry(
        &self,
        trail_doc_rel: &str,
        old_waypoint_rel: &str,
        new_waypoint_rel: &str,
    ) -> Result<(), HikerError> {
        let vault = self.vault;
        let watcher = self.watcher;
        let jobs = self.jobs;
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
        write_with_suppress_and_reindex(
            watcher, jobs, vault, trail_doc_rel, &new_src,
        )
        .await
    }

    /// Companion-folder case: when a *trail-doc* itself moves, its
    /// companion folder of waypoint-notes moves with it (`move_note`
    /// pairing), so the trail-doc's own `hiker.waypoints[].path` entries
    /// still point into the old companion folder. Rewrite each entry whose
    /// path lives under the old companion folder, swapping the prefix for
    /// the new companion folder. No-op when `new_rel` isn't a trail-doc or
    /// nothing matches. Returns 1 when the trail-doc was rewritten.
    ///
    /// status: note-companion-folder
    async fn rewrite_own_waypoint_paths_on_trail_doc_move(
        &self,
        old_rel: &str,
        new_rel: &str,
    ) -> usize {
        let (Some(old_companion), Some(new_companion)) = (
            crate::vault::companion_folder_for(old_rel),
            crate::vault::companion_folder_for(new_rel),
        ) else {
            return 0;
        };
        let old_prefix = format!("{old_companion}/");
        let new_prefix = format!("{new_companion}/");
        let Ok(src) = self.vault.read_file(new_rel) else { return 0 };
        // Only a trail-doc has waypoint entries to rewrite.
        let Ok(mut fm) = parse_trail_doc_for(new_rel, &src) else { return 0 };
        fn walk(entries: &mut [WaypointEntry], old: &str, new: &str, changed: &mut bool) {
            for w in entries.iter_mut() {
                if let Some(suffix) = w.path.strip_prefix(old) {
                    w.path = format!("{new}{suffix}");
                    *changed = true;
                }
                walk(&mut w.waypoints, old, new, changed);
            }
        }
        let mut changed = false;
        walk(&mut fm.waypoints, &old_prefix, &new_prefix, &mut changed);
        if !changed {
            return 0;
        }
        let Ok(new_src) = write_trail_doc_frontmatter(&src, &fm) else { return 0 };
        if let Err(e) = write_with_suppress_and_reindex(
            self.watcher, self.jobs, self.vault, new_rel, &new_src,
        )
        .await
        {
            tracing::warn!(error = %e, path = %new_rel,
                "on_note_moved: trail-doc own-waypoint-path rewrite failed");
            return 0;
        }
        1
    }
}

/// Shared rename-rewrite re-export of the suppress-write-reindex sequence
/// below. Called from `core::links_rename` so the wikilink-body rewriter
/// rides the exact same watcher / indexer plumbing the trail and board
/// rewriters use, instead of reimplementing it. Pure re-export; the wrapper
/// is the seam that keeps the helper itself private to this module.
///
/// status: wikilink-rename-rewrite
pub(crate) async fn write_with_suppress_and_reindex_for_links(
    watcher: Option<&Watcher>,
    jobs: Option<&IndexJobTx>,
    vault: &Vault,
    rel: &str,
    new_src: &str,
) -> Result<(), HikerError> {
    write_with_suppress_and_reindex(watcher, jobs, vault, rel, new_src).await
}

/// Common: pre-suppress watcher → write file → re-suppress watcher →
/// enqueue reindex. Mirrors the suppression pattern used by
/// `append_waypoint` and friends.
async fn write_with_suppress_and_reindex(
    watcher: Option<&Watcher>,
    jobs: Option<&IndexJobTx>,
    vault: &Vault,
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

    let _ = jobs
        .send(IndexJob::Upsert {
            rel_path: trail_doc_rel.to_string(),
            force: false,
        })
        .await;
    Ok(())
}

/// Set the trail-doc's append cursor. `waypoint_path: None` resets to
/// root-tail (the default flat-trail behavior). When `Some(path)`, the
/// path must resolve to a waypoint anywhere in the trail-doc's tree; a
/// stale path that doesn't resolve (concurrent edit, hand-edited
/// frontmatter pointing at a deleted waypoint) is treated as `None` with
/// a `tracing::warn!` rather than written through — same self-healing
/// posture as orphan waypoint refs (per `docs/trails.md` §"Append cursor",
/// the cascade-delete safety paragraph).
///
/// Same suppress + write + reindex pattern every other trails op uses.
///
/// status: trail-append-cursor
pub async fn set_append_cursor(
    watcher: &Watcher,
    jobs: &IndexJobTx,
    vault: &Vault,
    trail_doc_rel: &str,
    waypoint_path: Option<&str>,
) -> Result<(), HikerError> {
    let src = vault.read_file(trail_doc_rel)?;
    let mut fm = parse_trail_doc_for(trail_doc_rel, &src)
        .map_err(|e| HikerError::Io(format!("parse trail-doc: {e}")))?;

    // Validate: a `Some(path)` that doesn't resolve to a waypoint in the
    // tree falls back to `None` (cursor cleared) + a warn, never a stale
    // write.
    let resolved: Option<String> = match waypoint_path {
        Some(p) if super::find_waypoint(&fm.waypoints, p).is_some() => Some(p.to_string()),
        Some(p) => {
            tracing::warn!(
                cursor = %p,
                trail = %trail_doc_rel,
                "trail-append-cursor: set_append_cursor target `{p}` not in trail, resetting cursor to root"
            );
            None
        }
        None => None,
    };
    fm.append_under = resolved;

    let new_src = write_trail_doc_frontmatter(&src, &fm)
        .map_err(|e| HikerError::Io(format!("rewrite trail-doc: {e}")))?;

    watcher.suppress(trail_doc_rel.to_string());
    vault.write_file(trail_doc_rel, &new_src)?;
    watcher.suppress(trail_doc_rel.to_string());

    let _ = jobs
        .send(IndexJob::Upsert {
            rel_path: trail_doc_rel.to_string(),
            force: false,
        })
        .await;
    Ok(())
}

// ---------------------------------------------------------------------------
// One-time storage-layout migration: hidden `.hiker/trails/<id>/waypoints/`
// → the trail-doc's visible companion folder.
// ---------------------------------------------------------------------------

/// Relocate any legacy hidden waypoint directories
/// (`.hiker/trails/<trail-id>/waypoints/`) to the trail-doc's visible
/// companion folder (`<dir>/<trail>/`, per `note-companion-folder`), and
/// rewrite the trail-doc's `hiker.waypoints[].path` entries to match. Runs
/// at vault open, off disk + the op-log path mapping; the derived
/// `trail_waypoints` index re-derives on the next ingest, so this never
/// touches the store.
///
/// **Idempotent.** A vault already on the new layout has no
/// `.hiker/trails/<id>/waypoints/` dirs (only `.hiker/trails/drafts/`
/// survives, which is skipped), so a second open is a cheap directory
/// listing that moves nothing. A trail whose companion folder already
/// exists at the destination is skipped (the move already happened, or a
/// name collision the user must resolve by hand).
///
/// Drafts (`.hiker/trails/drafts/<id>.md` + their `<id>/` companion folder)
/// stay hidden — pre-acceptance machinery per `trail-draft-review-surface`
/// — so the `drafts` subdir is never migrated.
///
/// Returns the number of trail companion folders relocated. Per-trail
/// errors are logged and skipped so one broken trail can't block the rest
/// (or the vault opening).
///
/// status: trail-storage-layout
/// status: note-companion-folder
pub fn migrate_waypoints_to_companion_folders(
    vault: &Vault,
    log: &OpLog,
) -> Result<usize, HikerError> {
    let trails_root_abs = match vault.abs_path(&dir()) {
        Ok(p) => p,
        Err(_) => return Ok(0),
    };
    if !trails_root_abs.is_dir() {
        return Ok(0);
    }
    let mut migrated = 0usize;
    let entries = match std::fs::read_dir(&trails_root_abs) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(error = %e, "trails migration: cannot read .hiker/trails");
            return Ok(0);
        }
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else { continue };
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        // Drafts stay hidden — never migrate them.
        if name == super::DRAFTS_DIRNAME {
            continue;
        }
        // A legacy trail dir holds a `waypoints/` subdir; new vaults won't.
        let legacy_waypoints_rel = format!("{}/{name}/waypoints", dir());
        let legacy_abs = match vault.abs_path(&legacy_waypoints_rel) {
            Ok(p) => p,
            Err(_) => continue,
        };
        if !legacy_abs.is_dir() {
            continue;
        }
        match migrate_one_trail(vault, log, &name, &legacy_waypoints_rel) {
            Ok(true) => migrated += 1,
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(error = %e, trail_id = %name,
                    "trails migration: failed to relocate one trail; skipping");
            }
        }
    }
    Ok(migrated)
}

/// Migrate a single legacy trail's waypoints. `trail_id` is the
/// `.hiker/trails/<trail_id>/` dir name (the op-log doc_id of the
/// trail-doc); `legacy_waypoints_rel` is the `.../waypoints` dir to move.
/// Returns `Ok(true)` when the relocation happened, `Ok(false)` when it was
/// skipped (no resolvable trail-doc, draft, or destination already present).
fn migrate_one_trail(
    vault: &Vault,
    log: &OpLog,
    trail_id: &str,
    legacy_waypoints_rel: &str,
) -> Result<bool, HikerError> {
    // Resolve the trail-doc path. Under path-as-identity the hidden dir name is
    // no longer the trail-doc's id, so read it from a legacy waypoint's
    // `hiker.in_trail` instead (every waypoint points back at its trail-doc).
    let members =
        crate::ops::op_writes::walk_hidden_md_subtree(vault, legacy_waypoints_rel)
            .unwrap_or_default();
    let trail_doc_rel = match members.iter().find_map(|m| {
        let src = vault.read_file(m).ok()?;
        super::parse_waypoint(&src).ok().map(|fm| fm.in_trail)
    }) {
        Some(p) => p,
        None => {
            tracing::warn!(trail_id = %trail_id,
                "trails migration: no waypoint resolves a trail-doc for hidden dir; leaving in place");
            return Ok(false);
        }
    };
    // A trail-doc still living under `.hiker/trails/` is a draft (or an
    // un-promoted artifact); drafts keep their hidden companion folder.
    if trail_doc_rel.starts_with(&format!("{}/", dir())) {
        return Ok(false);
    }
    let Some(companion_rel) = super::waypoints_dir_for_doc(&trail_doc_rel) else {
        return Ok(false);
    };
    let companion_abs = vault.abs_path(&companion_rel)?;
    if companion_abs.exists() {
        // Destination already present — already migrated, or a collision
        // the user must resolve by hand. Don't clobber.
        return Ok(false);
    }

    // Enumerate the waypoint files before the move so we can rewrite their
    // op-log path mappings + the trail-doc entries afterward. The legacy
    // dir lives under `.hiker/trails/`, which the watcher-ignore rule now
    // prunes (no per-subsystem carve-out), so `walk_indexable_files` can't
    // reach it — walk the hidden subtree directly instead.
    let legacy_prefix = format!("{legacy_waypoints_rel}/");
    let pairs: Vec<(String, String)> = members
        .iter()
        .map(|m| {
            let suffix = m.strip_prefix(&legacy_prefix).unwrap_or(m);
            (m.clone(), format!("{companion_rel}/{suffix}"))
        })
        .collect();

    // Ensure the companion folder's parent exists, then fs-rename the
    // legacy `waypoints/` dir onto the companion path.
    if let Some(parent) = companion_abs.parent()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| HikerError::Io(format!("create companion parent: {e}")))?;
    }
    let legacy_abs = vault.abs_path(legacy_waypoints_rel)?;
    std::fs::rename(&legacy_abs, &companion_abs)
        .map_err(|e| HikerError::Io(format!("move waypoints dir: {e}")))?;

    // Remove the now-empty `.hiker/trails/<id>/` parent shell (best-effort).
    let legacy_root_rel = format!("{}/{trail_id}", dir());
    if let Ok(legacy_root_abs) = vault.abs_path(&legacy_root_rel) {
        let _ = std::fs::remove_dir(&legacy_root_abs);
    }

    // Repoint each waypoint-note's op-log path mapping (best-effort: a
    // never-seeded waypoint simply has no mapping to update).
    for (old, new) in &pairs {
        if let Ok(Some(doc_id)) = log.doc_id_for_path(old)
            && let Err(e) = log.rename_document(&doc_id, new, &crate::oplog::shapes::Author::User)
        {
            tracing::warn!(error = %e, old = %old, new = %new,
                "trails migration: op-log rename of waypoint failed");
        }
    }

    // Rewrite the trail-doc's `hiker.waypoints[].path` entries from the old
    // hidden prefix to the new companion prefix.
    rewrite_trail_doc_waypoint_paths(vault, &trail_doc_rel, &legacy_prefix, &companion_rel)?;

    tracing::info!(trail_id = %trail_id, trail = %trail_doc_rel,
        "trails migration: relocated waypoints to companion folder");
    Ok(true)
}

/// Rewrite every `hiker.waypoints[].path` in the trail-doc at
/// `trail_doc_rel` whose value starts with `old_prefix`, swapping that
/// prefix for `new_dir`. Writes the trail-doc directly (no watcher/indexer
/// plumbing — the migration runs before the watcher/indexer are wired and
/// the next ingest re-derives the table). No-op when nothing matches.
fn rewrite_trail_doc_waypoint_paths(
    vault: &Vault,
    trail_doc_rel: &str,
    old_prefix: &str,
    new_dir: &str,
) -> Result<(), HikerError> {
    let src = vault.read_file(trail_doc_rel)?;
    let mut fm = parse_trail_doc_for(trail_doc_rel, &src)
        .map_err(|e| HikerError::Io(format!("parse trail-doc: {e}")))?;
    fn walk(entries: &mut [WaypointEntry], old_prefix: &str, new_dir: &str, changed: &mut bool) {
        for w in entries.iter_mut() {
            if let Some(suffix) = w.path.strip_prefix(old_prefix) {
                w.path = format!("{new_dir}/{suffix}");
                *changed = true;
            }
            walk(&mut w.waypoints, old_prefix, new_dir, changed);
        }
    }
    let mut changed = false;
    walk(&mut fm.waypoints, old_prefix, new_dir, &mut changed);
    if !changed {
        return Ok(());
    }
    let new_src = write_trail_doc_frontmatter(&src, &fm)
        .map_err(|e| HikerError::Io(format!("rewrite trail-doc: {e}")))?;
    vault.write_file(trail_doc_rel, &new_src)?;
    Ok(())
}
