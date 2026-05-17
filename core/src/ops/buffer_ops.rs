//! Editor-buffer lifecycle: `open_for_edit` / `commit_buffer` /
//! `resolve_drift`, plus the `BufferToken` family of types that hide
//! hash-as-cursor from adapters.
//!
//! Also hosts `ensure_note_id_stamped` — the user-initiated `hiker.id`
//! stamping path that trail waypoints and the (planned) lazy id-stamping
//! mode ride. It belongs here rather than in `file_ops` because it shares
//! the same `frontmatter`-merge / changelog-row shape as the buffer
//! commit path and reuses the private helpers below.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::changes::{ChangeAppend, ChangeOp, Changes};
use crate::error::HikerError;
use crate::hash::hash_str;
use crate::indexer::{IndexJob, IndexJobTx};
use crate::store::Store;
use crate::vault::Vault;
use crate::watcher::Watcher;

use super::append_change_best_effort;

/// Opaque buffer-identity token issued by `open_for_edit` and rotated by
/// every successful `commit_buffer` / drift resolution. Wraps the path the
/// token is bound to, the content hash that was on disk at issue time, and
/// the load timestamp so callers (UI, MCP, future agents) never have to
/// hold the hash themselves — they round-trip the token verbatim through
/// `commit_buffer` and we re-derive the drift-check inputs from it.
///
/// Fields are private; the type serializes as a JSON object for the Tauri
/// IPC seam (and any future ts-rs export) but the UI must not introspect
/// or reconstruct it. The whole point of this slug is to delete the
/// hash-as-cursor concept from the editor surface.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BufferToken {
    path: String,
    content_hash: String,
    opened_at_ms: i64,
}

impl BufferToken {
    fn new(path: &str, content_hash: &str) -> Self {
        let opened_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        Self {
            path: path.to_string(),
            content_hash: content_hash.to_string(),
            opened_at_ms,
        }
    }

    /// Read accessor used by the (private) commit / resolve paths below.
    /// Never re-exported to adapters — the UI layer holds tokens, not
    /// hashes.
    fn hash(&self) -> &str {
        &self.content_hash
    }

    fn path(&self) -> &str {
        &self.path
    }
}

/// Result of `open_for_edit`. The token is opaque to callers; pair it with
/// `contents` to seed the editor and then round-trip the token unchanged on
/// every `commit_buffer`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenForEditOutcome {
    pub contents: String,
    pub token: BufferToken,
}

/// Outcome of a `commit_buffer` call. `Written` is the success path; the
/// returned `token` replaces the caller's prior token so the next commit
/// drift-checks against this commit's on-disk state. `DriftDetected`
/// surfaces the on-disk state for the caller to render its modal — the
/// caller then dispatches to `resolve_drift` with the user's choice
/// (keep / take / cancel).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommitOutcome {
    Written {
        new_hash: String,
        token: BufferToken,
    },
    DriftDetected {
        current_disk_text: String,
        current_hash: String,
    },
}

/// User's choice when resolving a drift conflict. Modal copy + default
/// focus stay in the UI; this is the typed dispatch surface so MCP / CLI /
/// future agents can drive the same conflict-resolution path.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriftChoice {
    /// Overwrite the on-disk version with the caller's `new_text`.
    KeepMine,
    /// Discard the caller's `new_text`; reload disk into the buffer. The
    /// returned `contents` + `token` reseed the caller.
    TakeTheirs,
    /// No-op. Caller should leave the buffer dirty so the next commit
    /// re-prompts.
    Cancel,
}

/// Result of `resolve_drift`. Mirrors the shapes the UI was juggling
/// inline (overwrite vs reload-from-disk vs no-op) so adapters can
/// dispatch on a typed variant rather than re-implementing the policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DriftResolution {
    Written {
        new_hash: String,
        token: BufferToken,
    },
    TookTheirs {
        contents: String,
        token: BufferToken,
    },
    Cancelled,
}

/// Read `rel` from disk and mint an opaque `BufferToken` capturing its
/// hash + path + load time. The caller seeds the editor with `contents`
/// and round-trips the token verbatim through `commit_buffer` —
/// hash-as-cursor stays inside core.
pub fn open_for_edit(vault: &Vault, rel: &str) -> Result<OpenForEditOutcome, HikerError> {
    let (contents, hash) = vault.read_file_with_hash(rel)?;
    Ok(OpenForEditOutcome {
        contents,
        token: BufferToken::new(rel, &hash),
    })
}

/// Write a buffer's new text using the drift-check encoded in `token`.
///
/// On success, appends a `'modified'` (or `'created'` if the file didn't
/// exist) row to the changelog with `extra_metadata` merged in, and
/// returns `Written { new_hash, token }` — the new token replaces the
/// caller's prior one for the next commit.
///
/// On drift, returns `DriftDetected { current_disk_text, current_hash }`
/// instead of erroring. The adapter renders its modal and dispatches to
/// `resolve_drift` with the user's choice. Other I/O errors propagate as
/// before.
///
/// `extra_metadata` carries one-shot context (e.g.
/// `{ "mutation": "<kind>" }` per `note-mutation-stash-changes-tag`); a
/// non-object value is treated as `{}` to match the existing Tauri
/// command shape.
pub fn commit_buffer(
    vault: &Vault,
    changes: Option<&Arc<Changes>>,
    token: &BufferToken,
    new_text: &str,
    extra_metadata: serde_json::Value,
) -> Result<CommitOutcome, HikerError> {
    let rel = token.path();
    let abs = vault.abs_path(rel)?;
    let existed = abs.exists();

    // Drift inspection: re-read disk and compare its hash to the token's
    // captured hash. On mismatch we surface the on-disk state to the
    // caller via `DriftDetected` rather than erroring; the adapter then
    // dispatches to `resolve_drift`.
    match std::fs::read(&abs) {
        Ok(bytes) => {
            let on_disk = String::from_utf8(bytes)
                .map_err(|e| HikerError::NotUtf8(e.to_string()))?;
            let found = hash_str(&on_disk);
            if found != token.hash() {
                return Ok(CommitOutcome::DriftDetected {
                    current_disk_text: on_disk,
                    current_hash: found,
                });
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if !token.hash().is_empty() {
                return Ok(CommitOutcome::DriftDetected {
                    current_disk_text: String::new(),
                    current_hash: String::new(),
                });
            }
        }
        Err(e) => return Err(e.into()),
    }

    // Baseline-on-first-save: snapshot the pre-write state if no
    // changelog row exists for this path yet, so rollback has somewhere
    // to go. No-op when a row already exists. Read failures fall through
    // silently — better to log a baseline-less write than to refuse it.
    if existed
        && let (Some(c), Ok((pre_text, pre_hash))) = (changes, vault.read_file_with_hash(rel))
        && let Err(e) = c.ensure_baseline(rel, "user", pre_text.as_bytes(), &pre_hash)
    {
        tracing::warn!(error = %e, "changes: ensure_baseline failed (commit_buffer)");
    }

    vault.write_file(rel, new_text)?;
    let new_hash = hash_str(new_text);

    let op = if existed { ChangeOp::Modified } else { ChangeOp::Created };
    let metadata = match extra_metadata {
        serde_json::Value::Object(_) => extra_metadata,
        _ => serde_json::json!({}),
    };
    append_change_best_effort(
        changes,
        ChangeAppend {
            path: rel,
            op,
            author: "user",
            content_hash: Some(&new_hash),
            content: Some(new_text.as_bytes()),
            rename_from: None,
            metadata,
        },
    );

    Ok(CommitOutcome::Written {
        new_hash: new_hash.clone(),
        token: BufferToken::new(rel, &new_hash),
    })
}

/// Dispatch the user's drift-resolution choice. Modal copy + default
/// focus stay in the adapter; this is the typed surface for the action
/// each branch represents.
///
/// - `KeepMine` — unconditional write of `new_text`, append a changelog
///   row, return `Written { new_hash, token }`.
/// - `TakeTheirs` — read disk, return `TookTheirs { contents, token }`.
///   No write, no changelog row. Caller reseeds its buffer.
/// - `Cancel` — no-op. Caller leaves the buffer dirty so the next commit
///   re-prompts.
pub fn resolve_drift(
    vault: &Vault,
    changes: Option<&Arc<Changes>>,
    rel: &str,
    choice: DriftChoice,
    new_text: &str,
    extra_metadata: serde_json::Value,
) -> Result<DriftResolution, HikerError> {
    match choice {
        DriftChoice::KeepMine => {
            let abs = vault.abs_path(rel)?;
            let existed = abs.exists();
            if existed
                && let (Some(c), Ok((pre_text, pre_hash))) = (changes, vault.read_file_with_hash(rel))
                && let Err(e) = c.ensure_baseline(rel, "user", pre_text.as_bytes(), &pre_hash)
            {
                tracing::warn!(error = %e, "changes: ensure_baseline failed (resolve_drift keep_mine)");
            }
            vault.write_file(rel, new_text)?;
            let new_hash = hash_str(new_text);
            let op = if existed { ChangeOp::Modified } else { ChangeOp::Created };
            let metadata = match extra_metadata {
                serde_json::Value::Object(_) => extra_metadata,
                _ => serde_json::json!({}),
            };
            append_change_best_effort(
                changes,
                ChangeAppend {
                    path: rel,
                    op,
                    author: "user",
                    content_hash: Some(&new_hash),
                    content: Some(new_text.as_bytes()),
                    rename_from: None,
                    metadata,
                },
            );
            Ok(DriftResolution::Written {
                new_hash: new_hash.clone(),
                token: BufferToken::new(rel, &new_hash),
            })
        }
        DriftChoice::TakeTheirs => {
            let (contents, hash) = vault.read_file_with_hash(rel)?;
            Ok(DriftResolution::TookTheirs {
                contents,
                token: BufferToken::new(rel, &hash),
            })
        }
        DriftChoice::Cancel => Ok(DriftResolution::Cancelled),
    }
}

/// Ensure the note at `rel` has `hiker.id` set in its frontmatter. If it
/// already has one, return it. Otherwise mint a fresh ULID, write the
/// stamped file through the watcher-suppression + changelog pattern that
/// the agent-frontmatter ops use (author = `"user"` since stamping is
/// triggered by a user-initiated action — adding a waypoint, future
/// wikilink targeting, etc.), and return the new id.
///
/// Caller is responsible for invoking this lazily — i.e. only when a
/// note is about to become a reference target. Per the `lazy` mode in
/// `note-id-stamping`, un-referenced notes stay untouched. The `all`
/// mode's startup-pass that stamps every note proactively isn't wired
/// yet — see TODO in `core::indexer`'s startup scan.
///
/// status: note-id-stamping
///
/// `store` is the indexer's read-side store handle, used to **adopt** the
/// existing `path_ids` ULID for `rel` when the indexer has already minted
/// one for this path. This keeps the two ULID systems in lockstep:
/// `path_ids[rel] == frontmatter_hiker_id == every reference's recorded
/// id`. Without this, freshly-minted ULIDs from `new_id()` would not match
/// what `Store::id_for_path` later returns, so `resolve_reference` would
/// surface every just-stamped note as a `PathConflict` orphan in the
/// Trails sidebar (`bug-id-stamping-mints-fresh-ulid-instead-of-adopting-
/// path-ids`).
///
/// Edge case: if the source already carries a `hiker.id` in frontmatter
/// AND `path_ids` has a different id for the same path, that's a
/// pre-existing inconsistency we don't silently rewrite — the frontmatter
/// id is returned as-is (with a warn log) since clobbering a
/// user-visible value risks data loss. The bug at hand only manifests on
/// fresh stamps; pre-existing mismatches warrant a separate slug if they
/// ever appear in real data.
pub async fn ensure_note_id_stamped(
    watcher: &Watcher,
    jobs: &IndexJobTx,
    vault: &Vault,
    changes: Option<&Arc<Changes>>,
    store: &mut Store,
    rel: &str,
) -> Result<String, HikerError> {
    // Fast path: existing id, no write. If `path_ids` disagrees, log and
    // prefer the frontmatter id (see doc-comment edge-case note above).
    let existing = vault.read_file(rel)?;
    let split = crate::frontmatter::split(&existing);
    if let Some(id) = read_hiker_id(&split.frontmatter) {
        if let Ok(Some(path_id)) = store.id_for_path(rel)
            && path_id != id
        {
            tracing::warn!(
                path = %rel,
                frontmatter_id = %id,
                path_ids_id = %path_id,
                "ensure_note_id_stamped: pre-existing id mismatch; \
                 keeping frontmatter id (resolve_reference may surface \
                 PathConflict until reconciled)",
            );
        }
        return Ok(id);
    }

    // Mint + write. Mirror `agent_set_frontmatter`'s shape: suppress the
    // watcher around the write so notify can't surface a stale event,
    // baseline-snapshot if first touch, append a `'modified'`/`'created'`
    // changelog row, then re-suppress + re-index.
    //
    // Adopt the indexer's existing `path_ids` row when present, so the
    // stamped id matches what `Store::id_for_path` will return later.
    // Only mint a fresh ULID when the path has never been ingested.
    let new_id = match store.id_for_path(rel) {
        Ok(Some(existing)) => existing,
        Ok(None) => crate::store::new_id(),
        Err(e) => {
            tracing::warn!(
                path = %rel,
                error = %e,
                "ensure_note_id_stamped: id_for_path lookup failed; minting fresh",
            );
            crate::store::new_id()
        }
    };
    let patch = serde_json::json!({ "hiker": { "id": new_id.clone() } });
    let merged = merge_user_patch(&existing, patch)?;

    watcher.suppress(rel.to_string());

    if let (Some(c), Ok((pre_text, pre_hash))) = (changes, vault.read_file_with_hash(rel))
        && let Err(e) = c.ensure_baseline(rel, "user", pre_text.as_bytes(), &pre_hash)
    {
        tracing::warn!(error = %e, "changes: ensure_baseline failed (id stamp)");
    }

    let abs = vault.abs_path(rel)?;
    let existed = abs.exists();
    vault.write_file(rel, &merged)?;
    let new_hash = hash_str(&merged);

    watcher.suppress(rel.to_string());

    let op = if existed { ChangeOp::Modified } else { ChangeOp::Created };
    let metadata = serde_json::json!({"reason": "note-id-stamping"});
    append_change_best_effort(
        changes,
        ChangeAppend {
            path: rel,
            op,
            author: "user",
            content_hash: Some(&new_hash),
            content: Some(merged.as_bytes()),
            rename_from: None,
            metadata,
        },
    );

    let _ = jobs
        .send(IndexJob::Upsert {
            rel_path: rel.to_string(),
            force: false,
        })
        .await;

    Ok(new_id)
}

/// Pull `hiker.id` out of an already-split frontmatter block, if present.
fn read_hiker_id(fm: &Option<serde_yml::Value>) -> Option<String> {
    let serde_yml::Value::Mapping(map) = fm.as_ref()? else { return None };
    let serde_yml::Value::Mapping(hiker) = map.get("hiker")? else { return None };
    hiker.get("id")?.as_str().map(|s| s.to_string())
}

/// Same as `frontmatter::merge_agent_patch` but does not stamp
/// `hiker.author = agent-authored` — the id-stamping path is user-
/// initiated, not agent-authored, so we keep author untouched. Other
/// `hiker.*` siblings round-trip unchanged.
fn merge_user_patch(
    source: &str,
    patch: serde_json::Value,
) -> Result<String, HikerError> {
    let split_view = crate::frontmatter::split(source);
    let mut fm = match split_view.frontmatter {
        Some(v) => v,
        None => serde_yml::Value::Mapping(Default::default()),
    };
    if !matches!(fm, serde_yml::Value::Mapping(_)) {
        fm = serde_yml::Value::Mapping(Default::default());
    }
    if let serde_json::Value::Object(_) = patch {
        crate::frontmatter::merge_json_into_yaml(&mut fm, patch);
    }
    crate::frontmatter::assemble(&fm, split_view.body)
        .map_err(|e| HikerError::Io(format!("frontmatter: {e}")))
}
