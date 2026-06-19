//! The in-memory LAYERED EDITING MODEL — plain-TEXT `accepted` (the canonical
//! `.md` on disk) / `working` (the editor's uncommitted overlay) / `pending`
//! (an agent session's proposed edits) per hiker document. Markdown on disk is
//! the canonical materialization of *accepted* edits; pending agent operations
//! are held in a per-document queue (`.hiker/editing/<path>.pending`) until the
//! user accepts.
//!
//! Local version history is plain-file snapshots under `.hiker/history/`
//! (`core::snapshot`) plus git when integrated — this model does not log
//! history itself. It no longer touches `index.db` at all — that file is
//! opened by `core::store` ALONE (a single writer).
//!
//! `accepted` and `working` are `String`s spliced by the shared text helpers
//! (`merge`/`overlay`) — there is no CRDT and no Yrs dependency. Materialization
//! is the IDENTITY over text, so opening + saving never rewrites a byte the
//! user didn't change. The layered model (`accepted` + `working`), the pending
//! queue, and the on-disk layout all live in the submodules; this root owns the
//! open path and the public verbs. The [`LayeredDoc`] public surface returns plain
//! Rust types only.
//
// status: op-log-module
// status: op-log-disk-canonical
// status: op-log-materialization

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub mod doc;
pub mod error;
pub mod shapes;
pub mod store;
pub mod writes;
mod lifecycle;
pub mod overlay;
mod pending;
mod working;

#[cfg(test)]
mod tests;

// Internal naming convenience so this root can name the public DTOs bare in
// `LayeredDoc`'s method signatures. External consumers reach them through the
// `pub mod`s above (`editing::error::Error`, `editing::shapes::PendingOp`, …),
// matching the repo's `core::trees` layout — no `pub use` re-export farm.
use error::Error;
use shapes::{Author, OpKind, PendingOp};

use doc::Materialized;

/// Public, plain-Rust view of a document's editable state. The return type of
/// [`LayeredDoc::materialize_accepted`] / the working + pending materializations.
///
/// status: op-log-materialization
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocContent {
    pub text: String,
    pub tombstone: bool,
}

impl From<Materialized> for DocContent {
    fn from(m: Materialized) -> Self {
        Self {
            text: m.text,
            tombstone: m.tombstone,
        }
    }
}

/// In-memory state for one open document: its `accepted` TEXT, the user's
/// uncommitted `working` overlay TEXT, and its pending queue.
///
/// `accepted` is the canonical, on-disk content as plain text — materialization
/// is the identity, so it equals the `.md` byte for byte (it is LOADED from the
/// `.md`). `accepted_tombstone` is the delete flag. `working` is `accepted`
/// plus the user's *uncommitted* edits — `None` when the buffer is clean (it
/// then equals `accepted`), `Some(text)` once the user has typed. `working`
/// lives in memory only (crash recovery is the autosave sidecar's job, not this
/// layer's) and never contains pending agent ops: the editable buffer is
/// `accepted + working`, and pending renders as an overlay on top of that. The
/// pending queue is the deferred-apply buffer that survives restarts via
/// `<doc-id>.pending`.
///
/// status: op-log-working-layer
/// status: op-log-materialization
struct DocState {
    accepted: String,
    accepted_tombstone: bool,
    working: Option<String>,
    pending: Vec<PendingOp>,
}

impl DocState {
    /// `materialize(accepted)` — now the identity over the stored text.
    fn accepted(&self) -> Materialized {
        Materialized { text: self.accepted.clone(), tombstone: self.accepted_tombstone }
    }

    /// The editable buffer's text: `working` when the user has uncommitted
    /// edits, else `accepted`.
    fn working_text(&self) -> &str {
        self.working.as_deref().unwrap_or(&self.accepted)
    }
}


/// The layered doc for one vault. Holds the lazily-loaded per-document layered
/// editing state (`accepted`/`working`/`pending`) behind one mutex. Owns NO
/// database connection — `index.db` is opened by `core::store` alone now that
/// the `op_history` engine is gone. Cheap to wrap in `Arc<LayeredDoc>`.
///
/// status: op-log-module
pub struct LayeredDoc {
    editing_dir: PathBuf,
    /// Retention policy for the plain-file snapshots written on every atomic
    /// `.md` write (`core::snapshot`, `plain-file-snapshots`) — the local
    /// version history now that the `.ops` engine is retired. Snapshots are an
    /// ADDITIVE, disposable cache, independent of git. Defaults to the
    /// keep-last-50 / drop-after-30-days policy; the bootstrap path threads the
    /// `[history]` config in via [`LayeredDoc::with_retention`].
    retention: crate::snapshot::RetentionPolicy,
    inner: Mutex<Inner>,
    /// Test-only hook fired inside [`LayeredDoc::commit_working`] after the first
    /// `locked` block (reading `working` text) releases and before
    /// `commit_text_edit` takes its own lock. Lets a test deterministically
    /// interleave a second LayeredDoc call (e.g. `apply_external_edit`) into the gap.
    #[cfg(test)]
    pub(crate) commit_working_test_hook:
        std::sync::Mutex<Option<std::sync::Arc<dyn Fn() + Send + Sync>>>,
}

struct Inner {
    docs: HashMap<String, DocState>,
}

/// Outcome of staging a producer edit batch: the pending op ids minted, all
/// sharing one `batch_id`. Callers (the producer-wiring phase) keep these to
/// later accept/reject individual ops.
///
/// status: op-log-pending-queue
#[derive(Debug, Clone)]
pub struct StageOutcome {
    pub batch_id: String,
    pub op_ids: Vec<String>,
}

/// One edit in a [`LayeredDoc::stage_pending`] call. An anchored replace
/// (`old_str` → `new_str`) or a whole-body rewrite (`old_str = None`).
///
/// status: op-log-pending-queue
#[derive(Debug, Clone)]
pub struct EditSpec {
    /// The text to find-and-replace. `None` means a whole-body rewrite
    /// (everything after the frontmatter fence becomes `new_str`).
    pub old_str: Option<String>,
    pub new_str: String,
}

/// Producer attribution shared across a [`LayeredDoc::stage_pending`] batch.
///
/// status: op-log-author-classes
#[derive(Debug, Clone)]
pub struct ProducerCtx {
    pub author: Author,
    pub surface: String,
    pub session_id: Option<String>,
}

/// How an accepted text edit names its change to the single commit path
/// ([`LayeredDoc::commit_text_edit`]). Holds only borrows, so it's `Copy`.
#[derive(Clone, Copy)]
enum EditInput<'a> {
    /// Pre-resolved positional spans — the editor's real edit ops, or a test.
    /// Already minimal; committed verbatim.
    Spans(&'a [(usize, usize, String)]),
    /// The full new file. The minimal localized spans are diffed against the
    /// current `accepted` *inside the commit lock*, so the diff and the apply
    /// observe one consistent state (no edit can slip between them) and the
    /// save lands as localized, mergeable text spans rather than a whole-text
    /// rewrite.
    FullText(&'a str),
}

impl LayeredDoc {
    /// Open or create the layered-doc store under `<vault>/.hiker/editing/`.
    /// Owns no database — documents load lazily from their canonical `.md` on
    /// disk (`op-log-disk-canonical`) and their `.pending` queue.
    ///
    /// status: op-log-module
    /// status: op-log-store-layout
    pub fn open(vault_root: &Path) -> Result<Self, Error> {
        let editing_dir = vault_root.join(".hiker").join("editing");
        fs::create_dir_all(&editing_dir)?;
        let log = Self {
            editing_dir,
            retention: crate::snapshot::RetentionPolicy::default(),
            inner: Mutex::new(Inner {
                docs: HashMap::new(),
            }),
            #[cfg(test)]
            commit_working_test_hook: std::sync::Mutex::new(None),
        };
        Ok(log)
    }

    /// The vault root this layered doc lives under (`<vault>/.hiker/editing` →
    /// `<vault>`). Lets a vault-scoped sibling locate the vault dir
    /// from the shared `LayeredDoc` handle without threading the path separately.
    pub fn vault_root(&self) -> &Path {
        vault_root_of(&self.editing_dir)
    }

    /// Set the plain-file snapshot [`RetentionPolicy`](crate::snapshot::RetentionPolicy)
    /// (from the `[history]` config) on a freshly-opened log. Builder-style so
    /// the many `LayeredDoc::open` call sites (tests, CLI) keep the default policy
    /// while the app bootstrap threads the configured one. Additive — snapshots
    /// are written either way; this only tunes pruning.
    ///
    /// status: plain-file-snapshots
    #[must_use]
    pub const fn with_retention(mut self, retention: crate::snapshot::RetentionPolicy) -> Self {
        self.retention = retention;
        self
    }

    /// Run `f` with the inner state lock held, releasing it when `f` returns.
    /// Every public verb takes the lock through here. The lock is **never**
    /// re-entered: a method that needs to call another locking method (e.g.
    /// `apply_user_edit` → `write_md`) closes its `locked` block first, so a
    /// second `locked` runs after the first releases — `std::sync::Mutex` is
    /// not re-entrant and nesting would deadlock.
    fn locked<R>(&self, f: impl FnOnce(&mut Inner) -> Result<R, Error>) -> Result<R, Error> {
        let mut inner = self.inner.lock().map_err(|_| Error::Poisoned)?;
        f(&mut inner)
    }

    /// Apply a positional user edit directly to `accepted` — author `user`,
    /// status `accepted` — committed verbatim through the shared text-commit
    /// path; persistence and the `.md` rewrite happen atomically under one lock
    /// (see [`commit_text_edit`](Self::commit_text_edit)).
    ///
    /// **Write-path discipline.** This and [`apply_user_text`](Self::apply_user_text)
    /// land *straight on `accepted`* (and disk), bypassing the `working` layer
    /// and therefore the agent-review overlay. They are for *non-interactive*
    /// producers — rollback, programmatic/whole-file saves, migration — where
    /// there is no live editor buffer to reconcile. The interactive editor MUST
    /// instead route through [`apply_working_edit`](Self::apply_working_edit)
    /// (uncommitted) + [`commit_working`](Self::commit_working) (Save), so the
    /// user's edits coexist with pending agent ops in one coordinate space.
    /// Using a direct path for editor-origin edits would clobber the overlay and
    /// any unsaved `working` state.
    ///
    /// status: op-log-atomic-write
    /// status: op-log-disk-canonical
    pub fn apply_user_edit(
        &self,
        doc_id: &str,
        byte_start: usize,
        byte_len: usize,
        new_text: &str,
    ) -> Result<(), Error> {
        let spans = [(byte_start, byte_len, new_text.to_string())];
        self.commit_text_edit(doc_id, EditInput::Spans(&spans), &Author::User, None)?;
        Ok(())
    }

    /// Apply a whole-buffer user save: the editor hands the layered doc the full
    /// file, which is diffed against the current `accepted` into minimal
    /// localized spans (inside the commit lock) and committed as one `user`
    /// op. Diffing — rather than replacing the whole text — keeps each save a
    /// minimal, mergeable text edit, so the substrate is sync-correct and the
    /// `.ops` history doesn't churn the whole document on every save. A save
    /// that changes nothing is a no-op (`Ok(false)`).
    ///
    /// status: op-log-disk-canonical
    /// status: op-log-materialization
    pub fn apply_user_text(&self, doc_id: &str, new_text: &str) -> Result<bool, Error> {
        self.commit_text_edit(doc_id, EditInput::FullText(new_text), &Author::User, None)
    }

    /// Reconcile an external edit: a `.md` file changed on disk outside
    /// hiker. Diffs `accepted` against `disk_text` into minimal localized
    /// spans, splices them into the `accepted` text, and writes a side-table
    /// row authored `external`. When `disk_text` already equals the accepted
    /// text the diff is empty and the call is a no-op (a self-write echo) — the
    /// safety net behind `watcher-suppress-self-writes`. Frontmatter and body
    /// are one text blob, so one diff covers both.
    ///
    /// Returns `true` when a delta was applied, `false` on the no-op echo.
    ///
    /// status: op-log-external-edit-sync
    pub fn apply_external_edit(&self, doc_id: &str, disk_text: &str) -> Result<bool, Error> {
        self.commit_text_edit(
            doc_id,
            EditInput::FullText(disk_text),
            &Author::External,
            Some("external-edit-sync"),
        )
    }

    // NOTE: `reextract_replace` (the in-process re-extraction `Replace` that
    // spliced a freshly-extracted body onto `accepted` authored `extractor:<id>`)
    // was removed under the manifest-only ingest decision
    // (`hiker-core-rework-plan.md` WS6). Hiker performs no in-process content
    // extraction; re-importing a changed source is an import-path concern driven
    // by an external producer's manifest, not a re-extraction seam. The
    // `Author::Extractor` shapes variant is retained so an `extractor:<id>`
    // author still round-trips. status: manifest-only-ingest

    /// Save: fold the user's `working` overlay into `accepted`. Returns
    /// `Ok(false)` when the buffer is clean (no `working`). Otherwise reads the
    /// `working` text and commits it through the shared text-commit path as a
    /// `user` op (the diff against `accepted` yields minimal localized `user`
    /// ops, then persists the `.ops` history frame, the metadata row, and the
    /// `.md` atomically), then clears `working`. Returns `Ok(true)`.
    /// Lives here (rather than with the other `working` verbs in `working.rs`)
    /// because it bridges into [`commit_text_edit`](Self::commit_text_edit).
    ///
    /// The non-reentrant lock forces three hops: read the working text under one
    /// `locked`, let `commit_text_edit` take its own lock, then clear `working`.
    ///
    /// status: op-log-working-layer
    /// status: op-log-atomic-write
    pub fn commit_working(&self, doc_id: &str) -> Result<bool, Error> {
        let captured = self.locked(|inner| {
            let state = Self::ensure_loaded(&self.editing_dir, inner, doc_id)?;
            Ok(state
                .working
                .as_ref()
                .map(|w| (w.clone(), state.accepted.clone())))
        })?;
        let Some((working_text, base_accepted_text)) = captured else {
            return Ok(false);
        };
        #[cfg(test)]
        {
            let hook = self
                .commit_working_test_hook
                .lock()
                .ok()
                .and_then(|g| g.clone());
            if let Some(hook) = hook {
                hook();
            }
        }
        // Re-check accepted under the commit lock: if a remote/external edit
        // advanced accepted between the two locks, three-way merge the user's
        // working delta over the now-advanced accepted so the peer's bytes
        // aren't diffed away as a user deletion.
        let merged = self.locked(|inner| {
            let state = Self::ensure_loaded(&self.editing_dir, inner, doc_id)?;
            let current_accepted = state.accepted.clone();
            if current_accepted == base_accepted_text {
                Ok(working_text.clone())
            } else {
                Ok(crate::merge::three_way_merge(
                    &base_accepted_text,
                    &working_text,
                    &current_accepted,
                ))
            }
        })?;
        self.commit_text_edit(doc_id, EditInput::FullText(&merged), &Author::User, None)?;
        self.locked(|inner| {
            let state = Self::ensure_loaded(&self.editing_dir, inner, doc_id)?;
            state.working = None;
            Ok(())
        })?;
        Ok(true)
    }

    /// `materialize(accepted)` — the canonical state that equals the on-disk
    /// `.md` by construction.
    ///
    /// status: op-log-materialization
    /// status: op-log-disk-canonical
    pub fn materialize_accepted(&self, doc_id: &str) -> Result<DocContent, Error> {
        self.locked(|inner| {
            let state = Self::ensure_loaded(&self.editing_dir, inner, doc_id)?;
            Ok(state.accepted().into())
        })
    }

    /// Re-materialize `accepted` to disk IF the `.md`/`.canvas` is missing there,
    /// returning whether a write happened. The layered doc's `accepted` is
    /// canonical (`op-log-disk-canonical`), so on a save whose accepted content
    /// is unchanged
    /// (`commit_working` was a no-op) but whose file has since vanished from disk
    /// (deleted/moved out-of-band after an autosave), this restores the file the
    /// user expects their save to produce. A no-op when the file is present (the
    /// canonical content already round-tripped through `commit_text_edit`), or
    /// when the doc is tombstoned (a real delete must not be resurrected here).
    ///
    /// status: op-log-disk-canonical
    pub fn ensure_on_disk(&self, doc_id: &str) -> Result<bool, Error> {
        self.locked(|inner| {
            let state = Self::ensure_loaded(&self.editing_dir, inner, doc_id)?;
            let materialized = state.accepted();
            if materialized.tombstone {
                return Ok(false);
            }
            let abs = vault_root_of(&self.editing_dir).join(doc_id);
            if abs.exists() {
                return Ok(false);
            }
            write_md_file(&self.editing_dir, Some(doc_id), &materialized, self.retention)?;
            Ok(true)
        })
    }

    /// `materialize(pending_view(session))` — a clone of `accepted` with the
    /// session's queued pending updates applied on top. Drifted ops (whose
    /// update no longer applies cleanly) are skipped, matching the buffer's
    /// "show what would land" semantics. `session = None` applies the whole
    /// queue.
    ///
    /// status: op-log-two-doc-model
    pub fn materialize_pending_view(
        &self,
        doc_id: &str,
        session: Option<&str>,
    ) -> Result<DocContent, Error> {
        self.locked(|inner| {
            let state = Self::ensure_loaded(&self.editing_dir, inner, doc_id)?;
            let base = state.accepted();
            // Best-effort: a drifted op simply doesn't contribute to the view
            // (its `op_spans` is `None`, surfaced via `is_pending_drifted`).
            let text = overlay::fold_session_text(&base.text, &state.pending, session, |_| false);
            Ok(DocContent {
                text,
                tombstone: base.tombstone,
            })
        })
    }

    /// `materialize(accepted + just this one pending op)` — a clone of
    /// `accepted` with a *single* pending op's update applied on top. The
    /// whole-file review surface previews one proposal in isolation (the user
    /// picked a specific op from the version dropdown / banner), so it can't
    /// use the session-wide `materialize_pending_view`. Errors if the op id
    /// isn't in the pending queue; a drifted op still materializes best-effort
    /// (its apply error is swallowed, matching the pending-view semantics).
    ///
    /// status: write-note-review-surface
    pub fn materialize_with_pending_op(
        &self,
        doc_id: &str,
        op_id: &str,
    ) -> Result<DocContent, Error> {
        self.locked(|inner| {
            let state = Self::ensure_loaded(&self.editing_dir, inner, doc_id)?;
            let op = state
                .pending
                .iter()
                .find(|p| p.op_id == op_id)
                .ok_or_else(|| Error::UnknownPendingOp(op_id.to_string()))?;
            let base = state.accepted();
            let spans = overlay::op_spans(&base.text, op).unwrap_or_default();
            let text = overlay::apply_spans_str(&base.text, &spans);
            Ok(DocContent {
                text,
                tombstone: base.tombstone,
            })
        })
    }

    /// The current pending queue for a document, reconstituted from disk if
    /// the document isn't loaded yet.
    ///
    /// status: op-log-pending-queue
    pub fn pending_ops(&self, doc_id: &str) -> Result<Vec<PendingOp>, Error> {
        self.locked(|inner| {
            let state = Self::ensure_loaded(&self.editing_dir, inner, doc_id)?;
            Ok(state.pending.clone())
        })
    }

    /// Every pending op across the whole vault, each paired with its `doc_id`.
    /// Walks the `<doc-id>.pending` files in the layered dir so the vault-wide
    /// activity feed can surface unreviewed proposals without knowing which
    /// documents have queues. Pending ops aren't in the side table — they live
    /// only on disk here.
    ///
    /// status: op-log-pending-queue
    pub fn all_pending_ops(&self) -> Result<Vec<(String, PendingOp)>, Error> {
        let doc_ids = store::scan_doc_ids(&self.editing_dir, "pending")?;
        let mut out: Vec<(String, PendingOp)> = Vec::new();
        for doc_id in doc_ids {
            let pending = self.locked(|inner| {
                let state = Self::ensure_loaded(&self.editing_dir, inner, &doc_id)?;
                Ok(state.pending.clone())
            })?;
            for op in pending {
                out.push((doc_id.clone(), op));
            }
        }
        Ok(out)
    }

    /// The op ids of pending ops (optionally scoped to `session`) whose
    /// edit overlaps the byte range `[start, end)` in the editable buffer's
    /// coordinate space (`working` when the user has uncommitted edits, else
    /// `accepted`) — the resolution per-hunk accept/reject rides on.
    /// Per `op-log-per-hunk-accept-reject`: each pending update is applied to
    /// a clone of that base and its affected position range checked for
    /// overlap with the hunk's range. Drifted ops (update no longer applies)
    /// contribute no range and are skipped.
    ///
    /// status: op-log-per-hunk-accept-reject
    pub fn ops_in_range(
        &self,
        doc_id: &str,
        session: Option<&str>,
        start: usize,
        end: usize,
    ) -> Result<Vec<String>, Error> {
        self.locked(|inner| {
            let state = Self::ensure_loaded(&self.editing_dir, inner, doc_id)?;
            // Pending ops render on top of `working` in the review overlay
            // (`materialize(working + pending)`), so resolve their affected
            // range against `working` when the user has uncommitted edits;
            // this keeps the result in the same coordinate space as the hunk
            // ranges the overlay passes in. Clean buffer → `working` is None →
            // falls back to `accepted` (unchanged behaviour).
            let base_text = state.working_text().to_string();
            let mut out = Vec::new();
            for op in &state.pending {
                if session.is_some() && op.session_id.as_deref() != session {
                    continue;
                }
                // Drifted / rename ops contribute no range.
                let Some(spans) = overlay::op_spans(&base_text, op) else {
                    continue;
                };
                if spans.is_empty() {
                    continue;
                }
                let op_start = spans.first().unwrap().0;
                let last = spans.last().unwrap();
                let mut op_end = last.0 + last.1;
                // Widen a pure insertion to a single position so an overlap
                // test against a hunk covering the insertion point still
                // matches (the old `changed_span` behavior).
                if op_end <= op_start {
                    op_end = (op_start + 1).min(base_text.len().max(op_start));
                }
                // Half-open overlap test.
                if op_start < end && start < op_end {
                    out.push(op.op_id.clone());
                }
            }
            Ok(out)
        })
    }

    /// Whether a pending op has drifted: its anchor (`old_str`) no longer
    /// resolves against the current `accepted` (plus the op's earlier
    /// same-session pending edits).
    ///
    /// An anchored `Replace` carries the `old_str` it matched on; once
    /// `accepted` advances so that text no longer resolves to exactly one
    /// range, the position the agent's edit targets is gone — the op is
    /// drifted. Whole-body rewrites and renames carry no anchor, so they never
    /// drift (a whole-doc replace re-diffs against whatever the base now is).
    ///
    /// status: op-log-pending-queue
    pub fn is_pending_drifted(&self, doc_id: &str, op_id: &str) -> Result<bool, Error> {
        self.locked(|inner| {
            let state = Self::ensure_loaded(&self.editing_dir, inner, doc_id)?;
            let pos = state
                .pending
                .iter()
                .position(|p| p.op_id == op_id)
                .ok_or_else(|| Error::UnknownPendingOp(op_id.to_string()))?;
            Ok(Self::op_drifted(state, doc_id, pos))
        })
    }

    /// Whether `state.pending[pos]` has drifted: its anchor no longer resolves
    /// (anchored op; anchorless whole-body / rename ops never drift), checked
    /// against `accepted` plus the op's earlier same-session pending ops (queue
    /// order). A real drift (the user, or a sync edit, changed `accepted` under
    /// the op) surfaces because the base starts from current accepted; a
    /// follow-up op anchored on the agent's own prior pending edit is NOT
    /// falsely flagged.
    ///
    /// Lock-free (operates on an already-loaded `state`) so both
    /// [`is_pending_drifted`](Self::is_pending_drifted) and `materialize_review`
    /// — which skips drifted ops rather than rendering their best-effort
    /// positional merge as a clean inline proposal — can call it.
    fn op_drifted(state: &DocState, _doc_id: &str, pos: usize) -> bool {
        let op = &state.pending[pos];
        // Base = accepted folded with this op's earlier same-session pending
        // ops (queue order), so a follow-up op anchored on the agent's own
        // prior staged edit isn't falsely flagged; only a real change to
        // `accepted` under the op surfaces as drift.
        let base_text = overlay::fold_session_text(
            &state.accepted,
            &state.pending[..pos],
            op.session_id.as_deref(),
            |_| false,
        );
        if let Some(old_str) = op.metadata.get("old_str").and_then(|v| v.as_str()) {
            // Anchored op: the anchor must still resolve to exactly one range.
            return doc::resolve_anchor(&base_text, old_str).is_err();
        }
        // Whole-body / rename ops never drift by anchor.
        false
    }

    /// Whether a document exists at `path`: the path IS the id under
    /// path-identity (`op-log-path-identity`), so a doc exists iff its canonical
    /// `.md` is on disk OR it is currently loaded in the cache (a freshly
    /// created or tombstoned doc whose file is gone but whose lifecycle state is
    /// still live this session).
    fn doc_exists(&self, path: &str) -> bool {
        if vault_root_of(&self.editing_dir).join(path).exists() {
            return true;
        }
        self.inner
            .lock()
            .map(|inner| inner.docs.contains_key(path))
            .unwrap_or(false)
    }

    /// Whether the document at `path` is already loaded in this layered doc's
    /// in-memory cache (registered this session). The bootstrap seed uses this
    /// as its idempotency key: there is no persistent presence check (no durable
    /// per-doc store), so "already seeded" means "already loaded" — a second bootstrap
    /// pass within one session skips the loaded docs, and a fresh open re-seeds
    /// (a cheap verify-against-disk, never an mtime-churning rewrite).
    ///
    /// status: op-log-doc-id-bootstrap
    pub fn is_loaded(&self, path: &str) -> bool {
        self.inner
            .lock()
            .map(|inner| inner.docs.contains_key(path))
            .unwrap_or(false)
    }

    /// Resolve a vault-relative path to its doc_id. Under path-as-identity the
    /// id IS the path (`op-log-path-identity`): this returns `Some(path)` iff a
    /// document exists at `path` (its canonical `.md` is on disk, or it is
    /// loaded), else `None`. A thin existence shim kept so the path→id callers
    /// compile unchanged.
    ///
    /// status: op-log-path-identity
    pub fn doc_id_for_path(&self, path: &str) -> Result<Option<String>, Error> {
        if self.doc_exists(path) {
            Ok(Some(path.to_string()))
        } else {
            Ok(None)
        }
    }

    /// The current vault-relative path for a doc_id. Under path-as-identity the
    /// id IS the path (`op-log-path-identity`): returns `Some(doc_id)` iff a
    /// document exists at that path (canonical `.md` on disk, or loaded), so a
    /// caller can resolve a doc_id back to a path (which is itself). A thin
    /// identity shim.
    ///
    /// status: op-log-path-identity
    pub fn path_for_doc(&self, doc_id: &str) -> Result<Option<String>, Error> {
        if self.doc_exists(doc_id) {
            Ok(Some(doc_id.to_string()))
        } else {
            Ok(None)
        }
    }

    /// Reject every pending op on `doc_id` that has drifted (its anchor no
    /// longer resolves, or its update no longer applies cleanly against the
    /// current `accepted`). Per `op-log.md`'s drift section, this is the
    /// `auto_reject_on_drift = true` policy: rather than surfacing drifted
    /// ops in the file pill, flip them to `rejected` immediately. Returns the
    /// op ids that were auto-rejected.
    ///
    /// status: op-log-status-states
    pub fn auto_reject_drifted(&self, doc_id: &str) -> Result<Vec<String>, Error> {
        // Snapshot the pending ids first (don't hold the lock across the
        // per-op drift check + reject, each of which re-locks).
        let op_ids: Vec<String> = self.locked(|inner| {
            let state = Self::ensure_loaded(&self.editing_dir, inner, doc_id)?;
            Ok(state.pending.iter().map(|p| p.op_id.clone()).collect())
        })?;
        let mut rejected = Vec::new();
        for op_id in op_ids {
            if self.is_pending_drifted(doc_id, &op_id)? {
                self.reject_pending(doc_id, &op_id)?;
                rejected.push(op_id);
            }
        }
        Ok(rejected)
    }

    // ── internals ──────────────────────────────────────────────────────

    /// The single accepted-text commit path. Resolves `input` to minimal
    /// localized spans and splices them into the `accepted` String
    /// (high-offset-first), then writes the `.md` atomically (+ a plain-file
    /// snapshot, `core::snapshot`) — **all under one lock hold**, so a
    /// concurrent writer (another save, an accept) can't interleave between the
    /// in-memory mutation and the disk write. The atomic `.md` write IS the
    /// durable persistence (`op-log-disk-canonical`). Returns `false` when the
    /// edit is empty (a no-op save / self-write echo). `author`/`surface` named
    /// the change on the deleted history frame; they are vestigial now (the
    /// callers' signatures are unchanged to avoid churn).
    ///
    /// status: op-log-atomic-write
    /// status: op-log-disk-canonical
    /// status: op-log-materialization
    fn commit_text_edit(
        &self,
        doc_id: &str,
        input: EditInput<'_>,
        author: &Author,
        surface: Option<&str>,
    ) -> Result<bool, Error> {
        let _ = (author, surface); // rode the deleted history frame
        self.locked(|inner| {
            let (materialized, rel_path) = {
                let state = Self::ensure_loaded(&self.editing_dir, inner, doc_id)?;
                let base = state.accepted.clone();
                // A content write to a tombstoned doc resurrects it: this is a
                // re-create at a path whose previous document was deleted (the
                // `path → doc_id` mapping is kept per `tombstone_document`), so
                // the resolved doc still reads as tombstoned. Without clearing
                // it, `write_md_file` would suppress the new file and the save
                // would silently vanish.
                let resurrecting = state.accepted_tombstone;
                // For a full-text fold the caller hands us the exact target the
                // materialized doc must equal; keep it so we can assert the
                // span round-trip reproduced it (below). A `Spans` (agent
                // producer) edit has no single target — its anchors are
                // validated where they're resolved.
                let (spans, full_text_target) = match input {
                    EditInput::Spans(spans) => (spans.to_vec(), None),
                    EditInput::FullText(new_text) => {
                        (crate::merge::multi_span_delta(&base, new_text), Some(new_text))
                    }
                };
                // A clean doc with no change is a no-op save / self-write echo.
                // A tombstoned doc must still commit (to clear the tombstone and
                // write the file) even when the text is byte-identical.
                if spans.is_empty() && !resurrecting {
                    return Ok(false);
                }
                let op_kind = if shapes::spans_in_frontmatter(&base, &spans) {
                    OpKind::SetFrontmatter
                } else {
                    OpKind::Replace { anchor: None }
                };
                // Splice the resolved spans into the accepted text
                // high-offset-first (the `apply_spans_str` discipline) so an
                // earlier edit never shifts a later span's coordinates.
                state.accepted = overlay::apply_spans_str(&base, &spans);
                // Invariant guard for every fold-in path: applying the computed
                // delta must reproduce the intended full text exactly. If it
                // doesn't (a bug in `multi_span_delta`/`apply_spans_str`, e.g.
                // the silent char-boundary skip in the latter), refuse to
                // persist — emitting drifted bytes here is precisely the
                // op-log-diverges-from-disk class of bug we never want.
                if let Some(target) = full_text_target
                    && state.accepted != target
                {
                    return Err(Error::FoldRoundTrip { path: doc_id.to_string() });
                }
                if resurrecting {
                    state.accepted_tombstone = false;
                }
                // Mirror the just-applied change onto the working overlay (if
                // any) so the user's uncommitted edits stay layered on top of
                // the new accepted state. Without this an external/remote edit
                // advances `accepted` but not `working`, so the buffer wouldn't
                // show it and the next commit would diff it away. A TEXT 3-way
                // merge: `working` holds locally-authored uncommitted edits the
                // accepted-level merge can't see, so an incoming edit that
                // DUPLICATES one of them is deduped by the merge's twin-skip and
                // a genuine disjoint one is shifted to match `accepted`. (On a
                // user commit, `working` is cleared right after, so the
                // reconcile there is harmless.)
                if let Some(old_working) = state.working.clone() {
                    let merged =
                        crate::merge::three_way_merge(&base, &old_working, &state.accepted);
                    if merged != old_working {
                        state.working = Some(merged);
                    }
                }
                // The doc id IS the path (path-identity), so the `.md` lands at
                // the doc_id. The atomic `.md` write below IS the durable
                // persistence (`op-log-disk-canonical`) — no history frame is
                // appended; the version history is the plain-file snapshot
                // `write_md_file` writes alongside it (`core::snapshot`).
                let _ = op_kind; // op-kind labelling rode the deleted frame
                let rel_path = doc_id.to_string();
                let materialized = state.accepted();
                (materialized, rel_path)
            };
            write_md_file(&self.editing_dir, Some(&rel_path), &materialized, self.retention)?;
            Ok(true)
        })
    }

    /// Load a document's `accepted` text + tombstone (from the canonical `.md`
    /// on disk) and pending queue into the cache if not already present,
    /// returning a mutable handle. The on-disk `.md` IS the current accepted
    /// content (`op-log-disk-canonical`); a doc whose `.md` is absent (or
    /// non-UTF-8) is unknown.
    fn ensure_loaded<'a>(
        editing_dir: &Path,
        inner: &'a mut Inner,
        doc_id: &str,
    ) -> Result<&'a mut DocState, Error> {
        Self::ensure_loaded_in(editing_dir, &mut inner.docs, doc_id)
    }

    /// The doc-cache half of [`ensure_loaded`], operating on just the `docs`
    /// map so a caller that also needs another `inner` field can hold both
    /// borrows at once.
    fn ensure_loaded_in<'a>(
        editing_dir: &Path,
        docs: &'a mut HashMap<String, DocState>,
        doc_id: &str,
    ) -> Result<&'a mut DocState, Error> {
        if !docs.contains_key(doc_id) {
            let (accepted, accepted_tombstone) = store::load_accepted(editing_dir, doc_id)?
                .ok_or_else(|| Error::UnknownDoc(doc_id.to_string()))?;
            let pending = store::load_pending(editing_dir, doc_id)?;
            docs.insert(
                doc_id.to_string(),
                DocState {
                    accepted,
                    accepted_tombstone,
                    working: None,
                    pending,
                },
            );
        }
        docs.get_mut(doc_id)
            .ok_or_else(|| Error::UnknownDoc(doc_id.to_string()))
    }

}

/// The vault root for an editing dir (`<vault>/.hiker/editing` → `<vault>`).
fn vault_root_of(editing_dir: &Path) -> &Path {
    editing_dir
        .parent()
        .and_then(|p| p.parent())
        .unwrap_or(editing_dir)
}

/// Atomically write `materialized` to `rel_path` under the vault root. A
/// tombstoned doc or a path-less doc (`rel_path = None`) writes nothing. Pure
/// filesystem I/O — no lock — so the commit path can call it inside its lock
/// hold and the lifecycle ops can resolve the path first and call it after.
///
/// status: op-log-atomic-write
/// status: op-log-disk-canonical
fn write_md_file(
    editing_dir: &Path,
    rel_path: Option<&str>,
    materialized: &Materialized,
    retention: crate::snapshot::RetentionPolicy,
) -> Result<(), Error> {
    if materialized.tombstone {
        return Ok(());
    }
    let Some(rel_path) = rel_path else {
        return Ok(());
    };
    let vault_root = vault_root_of(editing_dir);
    let abs = vault_root.join(rel_path);
    if let Some(parent) = abs.parent() {
        fs::create_dir_all(parent)?;
    }
    store::write_atomic(&abs, materialized.text.as_bytes())?;

    // ADDITIVE plain-file snapshot, written AFTER the atomic `.md` write — the
    // local version history now that the `.ops` engine is retired
    // (`plain-file-snapshots`). A disposable cache: a snapshot failure must
    // never fail the canonical save, so we log and continue rather than
    // propagate. Independent of git.
    if let Err(e) =
        crate::snapshot::snapshot(vault_root, rel_path, &materialized.text, retention)
    {
        tracing::warn!(rel_path, error = %e, "snapshot write failed (non-fatal; canonical save succeeded)");
    }
    Ok(())
}

/// How [`LayeredDoc::register_document`](crate::editing::LayeredDoc) reconciles a
/// newly-registered document with the `.md` on disk. Lives here beside
/// [`write_md_file`] / [`verify_md_matches`] (the two disk actions it selects
/// between) so the lifecycle split file stays a pure `impl LayeredDoc` continuation.
/// status: op-log-disk-canonical
#[derive(Clone, Copy)]
pub(super) enum SeedDisk {
    /// The file may not exist yet (a genuine create, a sync copy-in) — write
    /// `accepted` to disk atomically.
    Write,
    /// The file already exists on disk with exactly `accepted`'s bytes
    /// (bootstrap / first-open seed) — verify the hash matches and write
    /// nothing, so the file's mtime is never churned.
    VerifyExisting,
}

/// Seed-time counterpart to [`write_md_file`]: the document is being registered
/// from a file that ALREADY exists on disk with exactly these bytes, so there
/// is nothing to write — rewriting it would only churn the file's mtime (and
/// inode) for no benefit. This was the cause of a whole vault being re-stamped
/// on its first open: bootstrap seeds every untracked file, and the old create
/// path wrote each one's own bytes back over itself. Instead of writing, hash
/// what we *would* write and assert it matches what's on disk; a mismatch means
/// the seed bytes diverged from the file, so we refuse rather than silently
/// overwrite. A tombstoned or path-less doc verifies nothing.
///
/// status: op-log-disk-canonical
fn verify_md_matches(
    editing_dir: &Path,
    rel_path: Option<&str>,
    materialized: &Materialized,
) -> Result<(), Error> {
    if materialized.tombstone {
        return Ok(());
    }
    let Some(rel_path) = rel_path else {
        return Ok(());
    };
    let abs = vault_root_of(editing_dir).join(rel_path);
    let on_disk = fs::read(&abs)?;
    if blake3::hash(&on_disk) != blake3::hash(materialized.text.as_bytes()) {
        return Err(Error::SeedMismatch { path: rel_path.to_string() });
    }
    Ok(())
}

/// Remove the `.md` at `rel_path` under the vault root. A missing file is not
/// an error (the move target may equal the source, or an external tool may
/// have already moved it).
///
/// status: op-log-reorg-batch
fn remove_old_md_file(editing_dir: &Path, rel_path: &str) -> Result<(), Error> {
    let abs = vault_root_of(editing_dir).join(rel_path);
    if abs.exists() {
        fs::remove_file(&abs)?;
    }
    Ok(())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
