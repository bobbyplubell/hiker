//! Editor-buffer lifecycle: `open_for_edit` / `commit` /
//! `resolve_drift`, plus the `Token` family of types that hide
//! hash-as-cursor from adapters.
//!
//! Also hosts `ensure_note_id_stamped` — the user-initiated `hiker.id`
//! stamping path that trail waypoints and the (planned) lazy id-stamping
//! mode ride. It belongs here rather than in `file` because it shares
//! the same `frontmatter`-merge shape as the buffer commit path and
//! reuses the private helpers below.

use serde::{Deserialize, Serialize};

use crate::errors::HikerError;
use crate::hash_string;
use crate::indexer::{IndexJob, IndexJobTx};
use crate::store::Store;
use crate::vault::Vault;
use crate::watcher::Watcher;

/// Opaque buffer-identity token issued by `open_for_edit` and rotated by
/// every successful `commit` / drift resolution. Wraps the path the
/// token is bound to, the content hash that was on disk at issue time, and
/// the load timestamp so callers (UI, MCP, future agents) never have to
/// hold the hash themselves — they round-trip the token verbatim through
/// `commit` and we re-derive the drift-check inputs from it.
///
/// Fields are private; the type serializes as a JSON object for the
/// IPC seam but the UI must not introspect or reconstruct it. The whole
/// point of this slug is to delete the hash-as-cursor concept from the
/// editor surface.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Token {
    path: String,
    content_hash: String,
    opened_at_ms: i64,
}

impl Token {
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
/// every `commit`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenForEditOutcome {
    pub contents: String,
    pub token: Token,
}

/// Outcome of a `commit` call. `Written` is the success path; the
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
        token: Token,
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
        token: Token,
    },
    TookTheirs {
        contents: String,
        token: Token,
    },
    Cancelled,
}

/// Read `rel` from disk and mint an opaque `Token` capturing its
/// hash + path + load time. The caller seeds the editor with `contents`
/// and round-trips the token verbatim through `commit` —
/// hash-as-cursor stays inside core.
pub fn open_for_edit(vault: &Vault, rel: &str) -> Result<OpenForEditOutcome, HikerError> {
    let (contents, hash) = vault.read_file_with_hash(rel)?;
    Ok(OpenForEditOutcome {
        contents,
        token: Token::new(rel, &hash),
    })
}

/// Write a buffer's new text using the drift-check encoded in `token`.
///
/// On success returns `Written { new_hash, token }` — the new token
/// replaces the caller's prior one for the next commit.
///
/// On drift, returns `DriftDetected { current_disk_text, current_hash }`
/// instead of erroring. The adapter renders its modal and dispatches to
/// `resolve_drift` with the user's choice. Other I/O errors propagate as
/// before.
pub fn commit(
    vault: &Vault,
    token: &Token,
    new_text: &str,
) -> Result<CommitOutcome, HikerError> {
    let rel = token.path();
    let abs = vault.abs_path(rel)?;

    // Drift inspection: re-read disk and compare its hash to the token's
    // captured hash. On mismatch we surface the on-disk state to the
    // caller via `DriftDetected` rather than erroring; the adapter then
    // dispatches to `resolve_drift`.
    match std::fs::read(&abs) {
        Ok(bytes) => {
            let on_disk = String::from_utf8(bytes)
                .map_err(|e| HikerError::NotUtf8(e.to_string()))?;
            let found = hash_string(&on_disk);
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

    vault.write_file(rel, new_text)?;
    let new_hash = hash_string(new_text);

    Ok(CommitOutcome::Written {
        new_hash: new_hash.clone(),
        token: Token::new(rel, &new_hash),
    })
}

/// Dispatch the user's drift-resolution choice. Modal copy + default
/// focus stay in the adapter; this is the typed surface for the action
/// each branch represents.
///
/// - `KeepMine` — unconditional write of `new_text`, return
///   `Written { new_hash, token }`.
/// - `TakeTheirs` — read disk, return `TookTheirs { contents, token }`.
///   No write. Caller reseeds its buffer.
/// - `Cancel` — no-op. Caller leaves the buffer dirty so the next commit
///   re-prompts.
pub fn resolve_drift(
    vault: &Vault,
    rel: &str,
    choice: DriftChoice,
    new_text: &str,
) -> Result<DriftResolution, HikerError> {
    match choice {
        DriftChoice::KeepMine => {
            vault.write_file(rel, new_text)?;
            let new_hash = hash_string(new_text);
            Ok(DriftResolution::Written {
                new_hash: new_hash.clone(),
                token: Token::new(rel, &new_hash),
            })
        }
        DriftChoice::TakeTheirs => {
            let (contents, hash) = vault.read_file_with_hash(rel)?;
            Ok(DriftResolution::TookTheirs {
                contents,
                token: Token::new(rel, &hash),
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
    store: &mut Store,
    rel: &str,
) -> Result<String, HikerError> {
    IdStamper {
        watcher,
        jobs,
        vault,
        store,
        rel,
    }
    .run()
    .await
}

/// One-shot stamping context for [`ensure_note_id_stamped`]. Holds the
/// borrows the operation needs and chops the work into `&mut self`
/// methods so the public entry point stays under the
/// cognitive-complexity budget without a fan-out of free helpers.
struct IdStamper<'a> {
    watcher: &'a Watcher,
    jobs: &'a IndexJobTx,
    vault: &'a Vault,
    store: &'a mut Store,
    rel: &'a str,
}

impl<'a> IdStamper<'a> {
    async fn run(&mut self) -> Result<String, HikerError> {
        let existing = self.vault.read_file(self.rel)?;
        if let Some(id) = self.fast_path_existing_id(&existing) {
            return Ok(id);
        }
        let new_id = self.mint_or_adopt_id();
        let merged = self.merge_id_into_frontmatter(&existing, &new_id)?;
        self.write_and_record(&merged).await?;
        Ok(new_id)
    }

    /// Fast path: existing id in frontmatter → return it (no write).
    /// If `path_ids` disagrees, log and prefer the frontmatter id (see
    /// the doc-comment edge-case note on `ensure_note_id_stamped`).
    fn fast_path_existing_id(&self, existing: &str) -> Option<String> {
        let split = crate::frontmatter::split(existing);
        let read_id = split.frontmatter.as_ref().and_then(|fm| {
            let serde_yml::Value::Mapping(map) = fm else { return None };
            let serde_yml::Value::Mapping(hiker) = map.get("hiker")? else { return None };
            hiker.get("id")?.as_str().map(std::string::ToString::to_string)
        });
        let id = read_id?;
        if let Ok(Some(path_id)) = self.store.id_for_path(self.rel)
            && path_id != id
        {
            tracing::warn!(
                path = %self.rel,
                frontmatter_id = %id,
                path_ids_id = %path_id,
                "ensure_note_id_stamped: pre-existing id mismatch; \
                 keeping frontmatter id (resolve_reference may surface \
                 PathConflict until reconciled)",
            );
        }
        Some(id)
    }

    /// Adopt the indexer's existing `path_ids` row when present so the
    /// stamped id matches what `Store::id_for_path` will return later.
    /// Only mint a fresh ULID when the path has never been ingested.
    fn mint_or_adopt_id(&mut self) -> String {
        match self.store.id_for_path(self.rel) {
            Ok(Some(existing)) => existing,
            Ok(None) => crate::store::dto::new_id(),
            Err(e) => {
                tracing::warn!(
                    path = %self.rel,
                    error = %e,
                    "ensure_note_id_stamped: id_for_path lookup failed; minting fresh",
                );
                crate::store::dto::new_id()
            }
        }
    }

    /// Inline of `merge_user_patch`: mirrors
    /// `frontmatter::merge_agent_patch` but does not stamp
    /// `hiker.author = agent-authored` — id-stamping is user-initiated,
    /// not agent-authored.
    fn merge_id_into_frontmatter(
        &self,
        existing: &str,
        new_id: &str,
    ) -> Result<String, HikerError> {
        let patch = serde_json::json!({ "hiker": { "id": new_id } });
        let split_view = crate::frontmatter::split(existing);
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

    /// Mirror `set_frontmatter`'s shape: suppress the watcher around the
    /// write so notify can't surface a stale event, then re-suppress +
    /// re-index.
    async fn write_and_record(&self, merged: &str) -> Result<(), HikerError> {
        self.watcher.suppress(self.rel.to_string());
        self.vault.write_file(self.rel, merged)?;
        self.watcher.suppress(self.rel.to_string());
        let _ = self
            .jobs
            .send(IndexJob::Upsert {
                rel_path: self.rel.to_string(),
                force: false,
            })
            .await;
        Ok(())
    }
}

/// Normalize hand-typed / external wikilinks in `text` to the durable id form.
///
/// Every `[[Name]]` / `[[Name|alias]]` whose target is a name (not already a
/// ULID) is resolved by unique title match (`wikilink::resolve_name`); on a
/// unique hit the target note's `hiker.id` is stamped via
/// [`ensure_note_id_stamped`] (the lazy `note-id-stamping` trigger) and the
/// link is rewritten to `[[<ulid>|<display>]]`. Ambiguous or unmatched names
/// are left untouched so they stay unresolved rather than being guessed.
///
/// Returns the rewritten text (identical to the input when nothing resolved).
/// Self-links — a name resolving to `rel` itself — are skipped so stamping
/// doesn't race the in-flight save of the same file. status: wikilink-name-normalize
pub async fn normalize_wikilinks(
    watcher: &Watcher,
    jobs: &IndexJobTx,
    vault: &Vault,
    store: &mut Store,
    rel: &str,
    text: &str,
) -> Result<String, HikerError> {
    let pending: Vec<crate::wikilink::ParsedLink> = crate::wikilink::parse_links(text)
        .into_iter()
        .filter(|l| !l.is_id_form() && !l.target.is_empty())
        .collect();
    if pending.is_empty() {
        return Ok(text.to_string());
    }

    let paths = vault.walk_indexable_files("")?;
    // Cache name→ulid so multiple links to one note stamp it only once.
    let mut stamped: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    // (span, replacement) collected then applied right-to-left so byte offsets
    // stay valid as we splice.
    let mut edits: Vec<(std::ops::Range<usize>, String)> = Vec::new();

    for link in pending {
        let crate::wikilink::NameResolution::Unique(target_path) =
            crate::wikilink::resolve_name(&paths, &link.target)
        else {
            continue;
        };
        if target_path == rel {
            continue; // self-link; don't stamp the file we're saving
        }
        let ulid = match stamped.get(&target_path) {
            Some(id) => id.clone(),
            None => {
                let id = ensure_note_id_stamped(watcher, jobs, vault, store, &target_path).await?;
                stamped.insert(target_path.clone(), id.clone());
                id
            }
        };
        let display = link.display.clone().unwrap_or_else(|| link.target.clone());
        edits.push((link.span.clone(), format!("[[{ulid}|{display}]]")));
    }

    if edits.is_empty() {
        return Ok(text.to_string());
    }
    edits.sort_by(|a, b| b.0.start.cmp(&a.0.start));
    let mut out = text.to_string();
    for (span, replacement) in edits {
        out.replace_range(span, &replacement);
    }
    Ok(out)
}

