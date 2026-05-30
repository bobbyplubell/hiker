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

use super::{
    collect_descendant_paths, drafts_dir, find_waypoint, find_waypoint_mut,
    parse_trail_doc_for, parse_waypoint, random_alphanumeric_6,
    dir_prefix, remove_waypoint_from_tree, trail_root_for, waypoint_filename,
    waypoints_dir_for, write_trail_doc_frontmatter, write_waypoint_frontmatter,
    WaypointEntry, WaypointFrontmatter, WAYPOINTS_DIRNAME,
};

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

/// Create a new trail. Mints a ULID, writes the trail-doc to
/// `<new_trail_dir>/<name>.md` (auto-suffixed on collision), seeds the
/// hidden `.hiker/trails/<trail-id>/waypoints/` directory, and re-indexes
/// the trail-doc.
///
/// `name` is used verbatim as the basename; the function appends
/// `-N.md` (1..1000) only when there is a collision, mirroring
/// `core::ops::create_with_suffix`.
///
/// When `draft` is true the trail-doc lands at
/// `.hiker/trails/drafts/<trail-id>.md` with `hiker.draft: true` stamped
/// in its frontmatter (per `docs/trails.md` §"Draft trails"); the draft
/// path is keyed by the minted ULID so it never collides with another
/// draft and never pollutes the user's `new_trail_dir`. The waypoint dir
/// is identical for drafts and accepted trails. When `draft` is false the
/// behavior is unchanged. [trail-draft-from-agent, trail-draft-review-surface]
///
/// status: trails-default-location
/// status: trail-doc-shape
/// status: trail-draft-from-agent
pub async fn create_trail(
    watcher: &Watcher,
    jobs: &IndexJobTx,
    log: &OpLog,
    vault: &Vault,
    config: &TrailsConfig,
    name: &str,
    draft: bool,
) -> Result<CreateTrailOutcome, HikerError> {
    let folder = config.new_trail_dir.trim_end_matches('/');
    // Auto-create the placement folder so the very first trail in a vault
    // doesn't fail with NotFound on the parent. Drafts always live under
    // the hidden `.hiker/trails/drafts/` carve-out; accepted trails use
    // the configured `new_trail_dir`.
    let placement_dir = if draft {
        drafts_dir()
    } else {
        folder.to_string()
    };
    if !placement_dir.is_empty() {
        let abs = vault.abs_path(&placement_dir)?;
        if !abs.exists() {
            std::fs::create_dir_all(&abs)
                .map_err(|e| HikerError::Io(format!("create trail placement dir: {e}")))?;
        }
    }

    // Minimal valid trail-doc frontmatter — no `hiker.id` per
    // `trail-doc-shape` (the trail's storage key is the op-log's
    // doc_id, read from `doc-index.db` after the write). Draft trails
    // additionally carry `hiker.draft: true` so the listing filter and
    // review surface can distinguish them from accepted trails.
    let body = if draft {
        "---\nhiker:\n  kind: trail\n  draft: true\n  waypoints: []\n---\n".to_string()
    } else {
        "---\nhiker:\n  kind: trail\n  waypoints: []\n---\n".to_string()
    };

    // Resolve the trail-doc path. Drafts use a fresh random 6-char token
    // for the basename so the basename never collides with another draft
    // and the user-facing slot isn't burned on a ULID; accepted trails use
    // the name verbatim with auto-suffix on collision.
    let trail_doc_rel = if draft {
        let candidate = format!("{placement_dir}/{}.md", random_alphanumeric_6());
        watcher.suppress(candidate.clone());
        vault.write_file(&candidate, &body)?;
        candidate
    } else {
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


/// Default-draft policy for an agent-initiated `trail_create`. Per
/// `docs/trails.md` §"Draft sources": when `agent-write-review-mode` is
/// on, agent-created trails default to drafts (matching the existing
/// staging-as-default-for-agent-writes shape); when off, agents create
/// real trails unless they explicitly opt into a draft.
///
/// The MCP `trail_create` wrapper resolves the effective `draft` arg as
/// `explicit.unwrap_or(default_draft_for_review_mode(review_mode))` so an
/// explicit `draft=false` can still override the review-mode default.
///
/// status: trail-draft-from-agent
#[must_use]
pub const fn default_draft_for_review_mode(review_mode_on: bool) -> bool {
    review_mode_on
}

/// Outcome of accepting a draft trail. `trail_doc_rel` is the trail-doc's
/// new (promoted) path; `trail_id` the unchanged ULID.
///
/// status: trail-draft-review-surface
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcceptDraftOutcome {
    pub trail_doc_rel: String,
    pub trail_id: String,
}

/// Accept a draft trail (per `docs/trails.md` §"Review surface").
/// Strips `hiker.draft: true` from the trail-doc frontmatter and moves
/// the doc out of `.hiker/trails/drafts/` to the configured
/// `new_trail_dir`, keeping the waypoints in place. The ULID is unchanged
/// (the move is path-only, via `core::ops::file::move_note`, so the
/// derived `trail_waypoints` rows re-derive against the new path
/// cleanly). The trail joins the dropdown as a normal trail.
///
/// `draft_doc_rel` MUST resolve to a trail-doc currently flagged
/// `hiker.draft: true`; accepting a non-draft is a `NotFound`-shaped
/// no-op error so a double-accept can't silently relocate a real trail.
///
/// status: trail-draft-review-surface
pub async fn accept_draft(
    watcher: &Watcher,
    jobs: &IndexJobTx,
    log: &OpLog,
    vault: &Vault,
    config: &TrailsConfig,
    draft_doc_rel: &str,
) -> Result<AcceptDraftOutcome, HikerError> {
    let src = vault.read_file(draft_doc_rel)?;
    let mut fm = parse_trail_doc_for(draft_doc_rel, &src)
        .map_err(|e| HikerError::Io(format!("parse trail-doc: {e}")))?;
    if !fm.draft {
        return Err(HikerError::NotFound(format!(
            "trail-doc is not a draft: {draft_doc_rel}"
        )));
    }
    // status: store-id-from-oplog
    let trail_id = log
        .doc_id_for_path(draft_doc_rel)
        .map_err(|e| HikerError::Io(e.to_string()))?
        .ok_or_else(|| {
            HikerError::NotFound(format!(
                "op-log doc_id missing for draft trail: {draft_doc_rel}"
            ))
        })?;

    // 1. Clear the draft flag in place, then persist + re-index so the
    //    on-disk doc no longer carries `hiker.draft`.
    fm.draft = false;
    let cleared = write_trail_doc_frontmatter(&src, &fm)
        .map_err(|e| HikerError::Io(format!("rewrite trail-doc: {e}")))?;
    watcher.suppress(draft_doc_rel.to_string());
    vault.write_file(draft_doc_rel, &cleared)?;
    watcher.suppress(draft_doc_rel.to_string());

    // 2. Choose the promoted path under `new_trail_dir` (auto-suffixed on
    //    collision, mirroring `create_trail`). The promoted basename is
    //    the trail's doc_id, so it never collides; the user can rename
    //    later.
    let folder = config.new_trail_dir.trim_end_matches('/');
    if !folder.is_empty() {
        let abs = vault.abs_path(folder)?;
        if !abs.exists() {
            std::fs::create_dir_all(&abs)
                .map_err(|e| HikerError::Io(format!("create new_trail_dir: {e}")))?;
        }
    }
    let dest = promote_destination(vault, folder, &trail_id)?;

    // 3. Relocate via the indexer's path-remap so the doc_id survives.
    crate::ops::file::move_note(watcher, jobs, draft_doc_rel, &dest).await?;

    Ok(AcceptDraftOutcome {
        trail_doc_rel: dest,
        trail_id,
    })
}

/// Resolve a collision-free promoted path `<folder>/<trail_id>.md`
/// (auto-suffixed `-N` on collision; `folder` empty → vault root).
fn promote_destination(
    vault: &Vault,
    folder: &str,
    trail_id: &str,
) -> Result<String, HikerError> {
    let base = if folder.is_empty() {
        format!("{trail_id}.md")
    } else {
        format!("{folder}/{trail_id}.md")
    };
    if !vault.abs_path(&base)?.exists() {
        return Ok(base);
    }
    for n in 1..1000 {
        let candidate = if folder.is_empty() {
            format!("{trail_id}-{n}.md")
        } else {
            format!("{folder}/{trail_id}-{n}.md")
        };
        if !vault.abs_path(&candidate)?.exists() {
            return Ok(candidate);
        }
    }
    Err(HikerError::AlreadyExists(format!(
        "ran out of {trail_id}-N promote candidates"
    )))
}

/// Reject a draft trail (per `docs/trails.md` §"Review surface"). Drafts
/// are pre-acceptance, so rejection is a hard-delete: the trail-doc and
/// the entire `.hiker/trails/<trail-id>/` directory (waypoint-notes
/// included) are removed from disk directly — no trash, no `core::changes`
/// row. Refuses to act on a non-draft trail-doc so a real trail can't be
/// hard-deleted through this path.
///
/// status: trail-draft-review-surface
pub async fn reject_draft(
    watcher: &Watcher,
    jobs: &IndexJobTx,
    log: &OpLog,
    vault: &Vault,
    store: &Store,
    draft_doc_rel: &str,
) -> Result<(), HikerError> {
    let src = vault.read_file(draft_doc_rel)?;
    let fm = parse_trail_doc_for(draft_doc_rel, &src)
        .map_err(|e| HikerError::Io(format!("parse trail-doc: {e}")))?;
    if !fm.draft {
        return Err(HikerError::NotFound(format!(
            "trail-doc is not a draft: {draft_doc_rel}"
        )));
    }
    // status: store-id-from-oplog
    let trail_id = log
        .doc_id_for_path(draft_doc_rel)
        .map_err(|e| HikerError::Io(e.to_string()))?
        .ok_or_else(|| {
            HikerError::NotFound(format!(
                "op-log doc_id missing for draft trail: {draft_doc_rel}"
            ))
        })?;
    drop(src);
    drop(fm);

    // Gather the waypoint-note paths the indexer knows about up front so
    // we can enqueue their index deletes (the !Sync store read happens
    // before any .await fan-out).
    let waypoint_paths: Vec<String> = store
        .waypoints_of(&trail_id)
        .unwrap_or_default()
        .into_iter()
        .map(|w| w.waypoint_path)
        .collect();

    // Hard-delete the trail-doc on disk. Watcher suppressed so notify
    // can't surface a stale Deleted; index Delete enqueued so derived
    // rows clear without waiting for a rescan.
    watcher.suppress(draft_doc_rel.to_string());
    let abs = vault.abs_path(draft_doc_rel)?;
    if abs.exists() {
        std::fs::remove_file(&abs)
            .map_err(|e| HikerError::Io(format!("remove draft trail-doc: {e}")))?;
    }
    let _ = jobs
        .send(IndexJob::Delete {
            rel_path: draft_doc_rel.to_string(),
        })
        .await;

    // Hard-delete the trail's hidden subsystem dir (waypoint-notes), then
    // clear each waypoint-note's index rows.
    let trail_root = trail_root_for(&trail_id);
    let root_abs = vault.abs_path(&trail_root)?;
    if root_abs.exists() {
        std::fs::remove_dir_all(&root_abs)
            .map_err(|e| HikerError::Io(format!("remove draft trail dir: {e}")))?;
    }
    for wp in waypoint_paths {
        watcher.suppress(wp.clone());
        let _ = jobs.send(IndexJob::Delete { rel_path: wp }).await;
    }

    Ok(())
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

    // status: store-id-from-oplog
    // No source-side id stamping — `note-id-stamping` retired with
    // path-as-identity. The source is referenced by its vault path; the
    // op-log keeps the path↔doc_id mapping internally.

    // 1. Read the trail-doc + look up its doc_id (storage key for the
    //    waypoints folder).
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

    // 2. Compose the waypoint-note path + body. Filename embeds a 6-char
    //    random alphanumeric token so two waypoints with the same source
    //    basename don't collide. status: trail-storage-layout
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

    // Resolve filename + collision-suffix. The random 6-char token makes
    // collisions vanishingly rare per-trail, but if a clash hits we
    // append `_N` (1..1000) before re-minting a fresh token.
    let waypoint_rel = {
        let primary_rel = format!("{waypoints_dir}/{}", waypoint_filename(basename));
        let primary_abs = vault.abs_path(&primary_rel)?;
        if !primary_abs.exists() {
            primary_rel
        } else {
            let mut chosen: Option<String> = None;
            for n in 2..1000 {
                let candidate = format!(
                    "{waypoints_dir}/{basename}_{n}--{}.md",
                    random_alphanumeric_6()
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
        let wfm = WaypointFrontmatter {
            references: source_rel.to_string(),
            in_trail: trail_doc_rel.to_string(),
        };
        // Body-source is just the empty string — no body, no extra newlines
        // beyond the closing `---\n` that `assemble` produces. Per spec,
        // `trail-empty-waypoint-body` requires zero bytes after the FM.
        write_waypoint_frontmatter("", &wfm)
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
                if find_waypoint(&fm.waypoints, cursor_path).is_some() {
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
            let parent = find_waypoint_mut(&mut fm.waypoints, pp).ok_or_else(|| {
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

    let target = find_waypoint(&fm.waypoints, waypoint_path)
        .ok_or_else(|| HikerError::NotFound(format!("waypoint path: {waypoint_path}")))?;
    // Collect paths of the target + every descendant.
    let removed_paths: Vec<String> = collect_descendant_paths(target);

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
        remove_waypoint_from_tree(&mut fm.waypoints, waypoint_path).ok_or_else(|| {
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
    let target = find_waypoint(&fm.waypoints, waypoint_path)
        .ok_or_else(|| HikerError::NotFound(format!("waypoint path: {waypoint_path}")))?;
    Ok(collect_descendant_paths(target).len() as u32)
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
    log: &OpLog,
    vault: &Vault,
    _trash: &Trash,
    trail_doc_rel: &str,
) -> Result<Entry, HikerError> {
    // Pull the trail id off the op-log so we know which waypoint dir to
    // cascade. status: store-id-from-oplog
    let trail_id = match log.doc_id_for_path(trail_doc_rel) {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(error = %e, path = %trail_doc_rel,
                "delete_trail: doc_id_for_path failed; cascading skipped");
            None
        }
    };

    let entry = crate::ops::file::delete(watcher, jobs, vault, trail_doc_rel).await?;

    if let Some(tid) = trail_id {
        let waypoint_dir = waypoints_dir_for(&tid);
        // The dir lives at `.hiker/trails/<id>/waypoints` but the spec's
        // delete-cascade scope is the parent `.hiker/trails/<id>/` so a
        // future `manifest/` sibling rides along. Delete the parent.
        let trail_root = trail_root_for(&tid);
        let abs = vault.abs_path(&trail_root)?;
        if abs.exists() {
            // TODO(trail-delete-cascade): atomic-pair semantics in trash
            // are deferred — for v1 the trail-doc and the waypoint dir
            // become two separate trash entries; the user restores both.
            if let Err(e) =
                crate::ops::file::delete(watcher, jobs, vault, &trail_root).await
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
    // status: store-id-from-oplog
    // The trail id of a moved trail-doc is the op-log's doc_id for the
    // doc's path — the same id for old or new path after the rename
    // committed in `doc-index.db`.
    let trail_id_candidate: Option<String> = log.and_then(|l| {
        l.doc_id_for_path(new_rel)
            .ok()
            .flatten()
            .or_else(|| l.doc_id_for_path(old_rel).ok().flatten())
    });
    let waypoints_of_trail = match &trail_id_candidate {
        Some(trail_id) => store.waypoints_of(trail_id).unwrap_or_default(),
        None => Vec::new(),
    };

    let rctx = RewriteCtx { watcher, jobs, vault };
    let mut touched: usize = 0;
    touched += rctx.fan_out_source_moved(&containing, new_rel).await;
    touched += rctx.fan_out_trail_doc_moved(&waypoints_of_trail, new_rel).await;
    touched += rctx.fan_out_waypoint_moved(store, old_rel, new_rel).await;
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
        if !(old_rel.starts_with(&dir_prefix())
            && old_rel.contains(&format!("/{WAYPOINTS_DIRNAME}/")))
        {
            return 0;
        }
        // Look up the row's trail_id by walking trails_containing_note
        // won't work (matches source). Use a direct id_for_path: the
        // waypoint-note's own id → in_trail.id is its parent trail.
        // Easier: read the waypoint-note from disk (it's at new_rel now)
        // and parse its in_trail to learn the trail_id, then rewrite the
        // trail-doc.
        let Ok(src) = self.vault.read_file(new_rel) else { return 0 };
        let Ok(fm) = parse_waypoint(&src) else { return 0 };
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
        let mut fm = parse_waypoint(&src)
            .map_err(|e| HikerError::Io(format!("parse waypoint: {e}")))?;
        if fm.references == new_source_rel {
            return Ok(()); // already canonical (idempotent re-runs)
        }
        fm.references = new_source_rel.to_string();
        let new_src = write_waypoint_frontmatter(&src, &fm)
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
        let mut fm = parse_waypoint(&src)
            .map_err(|e| HikerError::Io(format!("parse waypoint: {e}")))?;
        if fm.in_trail == new_trail_doc_rel {
            return Ok(());
        }
        fm.in_trail = new_trail_doc_rel.to_string();
        let new_src = write_waypoint_frontmatter(&src, &fm)
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

/// Set the trail-doc's append cursor. `waypoint_id: None` resets to
/// root-tail (the default flat-trail behavior). When `Some(id)`, the id
/// MUST resolve to a waypoint anywhere in the trail-doc's tree — we
/// refuse to silently write a stale cursor.
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

    if let Some(p) = waypoint_path
        && find_waypoint(&fm.waypoints, p).is_none()
    {
        return Err(HikerError::NotFound(format!("waypoint path: {p}")));
    }
    fm.append_under = waypoint_path.map(std::string::ToString::to_string);

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
