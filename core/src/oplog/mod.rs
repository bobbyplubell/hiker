//! The op-log substrate: plain-TEXT `accepted`/`working` per hiker document.
//! The editorial metadata (author, op-kind, surface, session/batch ids,
//! durable metadata) rides on each self-describing `.ops` history frame; a
//! REGENERABLE `op_history` query-index over those frames lives in the vault's
//! sole `index.db` (`op-log-no-oplog-db` — there is no `oplog_meta.db`).
//! Markdown on disk is the canonical materialization of *accepted* operations;
//! pending agent operations are held in a per-document queue until the user
//! accepts.
//!
//! Module discipline mirrors `core::store`: the `rusqlite` dependency is
//! confined to `meta`, and the [`OpLog`] public surface returns plain Rust
//! types only. `accepted` and `working` are `String`s spliced by the shared
//! text helpers (`merge`/`overlay`) — there is no CRDT and no Yrs dependency.
//! Materialization is the IDENTITY over text, so opening + saving never
//! rewrites a byte the user didn't change. The layered model (`accepted` +
//! `working`), the pending queue, the regenerable query-index, and the on-disk
//! layout all live in the submodules; this root owns the open path and the
//! public verbs.
//
// status: op-log-module
// status: op-log-disk-canonical
// status: op-log-materialization

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::Connection;

pub mod doc;
pub mod error;
pub mod meta;
pub mod shapes;
pub mod store;
pub mod writes;
mod history;
mod lifecycle;
pub mod overlay;
mod pending;
pub mod sync;
mod working;

#[cfg(test)]
mod tests;

// Internal naming convenience so this root can name the public DTOs bare in
// `OpLog`'s method signatures. External consumers reach them through the
// `pub mod`s above (`oplog::error::Error`, `oplog::shapes::PendingOp`,
// `oplog::meta::OpMetadata`, …), matching the repo's `core::trees` layout —
// no `pub use` re-export farm.
use error::Error;
use meta::{Filter, OpMetadata, OpStatus};
use shapes::{Author, OpKind, PendingOp};

use doc::Materialized;

/// Public, plain-Rust view of a document's editable state. The return type of
/// [`OpLog::materialize_accepted`] / the working + pending materializations.
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
/// is the identity, so it equals the `.md` (and the newest `.ops` frame) byte
/// for byte. `accepted_tombstone` is the delete flag. `working` is `accepted`
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
    /// Materialized text of the most recent `.ops` history frame, kept so the
    /// next frame can be stored as a zstd-dictionary delta against it.
    /// `None` after a (re)open — the first write then forces a keyframe, which
    /// re-establishes the exact text the following deltas decode against (no
    /// `.ops` read needed at load). Per `op-log-accepted-op-retention`.
    last_retained_text: Option<String>,
    /// Delta frames appended since the last keyframe; a keyframe is forced once
    /// it reaches [`KEYFRAME_INTERVAL`], bounding history reconstruction cost.
    deltas_since_keyframe: usize,
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


/// The op log for one vault. Holds the side-table + path-index SQLite
/// connections and the lazily-opened per-document state behind one mutex.
/// Cheap to wrap in `Arc<OpLog>`.
///
/// status: op-log-module
pub struct OpLog {
    oplog_dir: PathBuf,
    inner: Mutex<Inner>,
    /// Test-only hook fired inside [`OpLog::commit_working`] after the first
    /// `locked` block (reading `working` text) releases and before
    /// `commit_text_edit` takes its own lock. Lets a test deterministically
    /// interleave a second OpLog call (e.g. `apply_external_edit`) into the
    /// gap, exposing `bug-sync-commit-working-races-remote-apply`.
    #[cfg(test)]
    pub(crate) commit_working_test_hook:
        std::sync::Mutex<Option<std::sync::Arc<dyn Fn() + Send + Sync>>>,
}

struct Inner {
    /// The op log's own connection to the vault's sole `index.db`
    /// (`op-log-no-oplog-db`): the regenerable `op_history` query-index +
    /// the durable `bootstrap_skipped` marker. The search store owns its own
    /// connection to the same file; they coordinate at the SQLite WAL level.
    index: Connection,
    docs: HashMap<String, DocState>,
}

impl Inner {
    /// Load `doc_id` if needed and return the index connection alongside its
    /// mutable [`DocState`] as a SPLIT borrow — the `retain_frame` path needs
    /// both `&Connection` (to append the `op_history` row) and `&mut DocState`
    /// (to advance the delta chain) at once, which a single
    /// `OpLog::ensure_loaded(&mut Inner)` borrow can't hand out. Borrowing the
    /// two fields separately keeps the borrow checker satisfied.
    fn index_and_state(
        &mut self,
        oplog_dir: &Path,
        doc_id: &str,
    ) -> Result<(&Connection, &mut DocState), Error> {
        OpLog::ensure_loaded_in(oplog_dir, &mut self.docs, doc_id)?;
        let state = self
            .docs
            .get_mut(doc_id)
            .ok_or_else(|| Error::UnknownDoc(doc_id.to_string()))?;
        Ok((&self.index, state))
    }
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

/// One edit in a [`OpLog::stage_pending`] call. An anchored replace
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

/// Producer attribution shared across a [`OpLog::stage_pending`] batch.
///
/// status: op-log-author-classes
#[derive(Debug, Clone)]
pub struct ProducerCtx {
    pub author: Author,
    pub surface: String,
    pub session_id: Option<String>,
}

/// How an accepted text edit names its change to the single commit path
/// ([`OpLog::commit_text_edit`]). Holds only borrows, so it's `Copy`.
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

impl OpLog {
    /// Open or create the op log under `<vault>/.hiker/oplog/`. Opens the op
    /// log's handle to the vault `index.db` and REBUILDS the regenerable
    /// `op_history` query-index by replaying every doc's `.ops` frames
    /// (`changes-query-api`). Documents load lazily from their `.ops` history.
    ///
    /// status: op-log-module
    /// status: op-log-store-layout
    /// status: op-log-no-oplog-db
    pub fn open(vault_root: &Path) -> Result<Self, Error> {
        Self::open_with_threshold(vault_root, 0.0)
    }

    /// The vault root this op-log lives under (`<vault>/.hiker/oplog` →
    /// `<vault>`). Lets a vault-scoped sibling (e.g. the sync node's persisted
    /// blocked-conflict store under `<vault>/.hiker/sync/`) locate the vault dir
    /// from the shared `OpLog` handle without threading the path separately.
    pub fn vault_root(&self) -> &Path {
        vault_root_of(&self.oplog_dir)
    }

    /// `open` with a vestigial `_threshold` arg. Compaction is gone — the
    /// `.ops` history log is the document's sole durable representation now that
    /// `accepted` is plain text (the log stays linear via the keyframe/delta
    /// machinery), so there is no separate snapshot to fold. The arg is kept so
    /// the `[op-log] compact_threshold` config plumbing and the tests that pass
    /// it don't churn.
    ///
    /// status: op-log-accepted-op-retention
    pub fn open_with_threshold(vault_root: &Path, _threshold: f32) -> Result<Self, Error> {
        let oplog_dir = vault_root.join(".hiker").join("oplog");
        fs::create_dir_all(&oplog_dir)?;
        // Open the op log's own handle to the vault's `index.db` and rebuild the
        // regenerable `op_history` query-index from every doc's `.ops` frames
        // (`changes-query-api`). The rebuild is idempotent and keeps the index in
        // lock-step with the durable history even after an `rm index.db`.
        let index = meta::open_index(vault_root)?;
        meta::rebuild_from_ops(&index, &oplog_dir)?;
        let log = Self {
            oplog_dir,
            inner: Mutex::new(Inner {
                index,
                docs: HashMap::new(),
            }),
            #[cfg(test)]
            commit_working_test_hook: std::sync::Mutex::new(None),
        };
        Ok(log)
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

    /// Apply a whole-buffer user save: the editor hands the op log the full
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

    /// Re-extraction `Replace`: apply a single text edit to `accepted` that
    /// replaces the document's **body region** (everything after the leading
    /// frontmatter fence) with `new_body`, authored `extractor:<id>`. The
    /// frontmatter fence is left byte-for-byte untouched — only the body bytes
    /// that actually differ become text ops (the commit path diffs the assembled
    /// `frontmatter + new_body` against the current `accepted` into minimal
    /// localized spans), so a concurrent user edit elsewhere merges via the
    /// 3-way text merge and the frontmatter is never rewritten.
    ///
    /// An **identical** re-extraction (the resulting body equals the current
    /// body) produces an empty diff and is a **no-op**: no op, no metadata
    /// row, no history frame, no version. Returns `Ok(true)` when a new version
    /// landed, `Ok(false)` on the identical-content no-op. This is the default
    /// re-extraction policy for a previously-LINKED sidecar (`fill_body: true`).
    /// Per `op-log.md`'s "Re-extraction" table (`Replace`) and `extract.md`'s
    /// "Versioning and retention".
    ///
    /// status: op-log-reextract-replace
    /// status: extract-version-oplog
    pub fn reextract_replace(
        &self,
        doc_id: &str,
        new_body: &str,
        extractor_id: &str,
    ) -> Result<bool, Error> {
        // Read the current accepted text, keep its leading frontmatter fence
        // verbatim, and splice the new body after it. The full assembled text
        // is committed through the shared text-commit path, which diffs it into
        // minimal localized spans — so untouched frontmatter bytes carry no op
        // and an unchanged body produces no op at all.
        let current = self.materialize_accepted(doc_id)?.text;
        let fence_end = shapes::frontmatter_fence_end(&current).unwrap_or(0);
        let mut assembled = String::with_capacity(fence_end + new_body.len());
        assembled.push_str(&current[..fence_end]);
        assembled.push_str(new_body);
        self.commit_text_edit(
            doc_id,
            EditInput::FullText(&assembled),
            &Author::Extractor(extractor_id.to_string()),
            Some("extractor"),
        )
    }

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
            let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
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
            let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
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
            let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
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
            let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
            Ok(state.accepted().into())
        })
    }

    /// Re-materialize `accepted` to disk IF the `.md`/`.canvas` is missing there,
    /// returning whether a write happened. The op-log's `accepted` is canonical
    /// (`op-log-disk-canonical`), so on a save whose op-log content is unchanged
    /// (`commit_working` was a no-op) but whose file has since vanished from disk
    /// (deleted/moved out-of-band after an autosave), this restores the file the
    /// user expects their save to produce. A no-op when the file is present (the
    /// canonical content already round-tripped through `commit_text_edit`), or
    /// when the doc is tombstoned (a real delete must not be resurrected here).
    ///
    /// status: op-log-disk-canonical
    pub fn ensure_on_disk(&self, doc_id: &str) -> Result<bool, Error> {
        self.locked(|inner| {
            let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
            let materialized = state.accepted();
            if materialized.tombstone {
                return Ok(false);
            }
            let abs = vault_root_of(&self.oplog_dir).join(doc_id);
            if abs.exists() {
                return Ok(false);
            }
            write_md_file(&self.oplog_dir, Some(doc_id), &materialized)?;
            Ok(true)
        })
    }

    /// A document's accepted-op history, newest-first (the version list behind
    /// the snapshot dropdown / per-file history). Thin projection over
    /// [`query_metadata`] restricted to `status = Accepted`.
    ///
    /// status: op-log-history-materialization
    pub fn doc_history(&self, doc_id: &str, limit: usize) -> Result<Vec<OpMetadata>, Error> {
        self.query_metadata(&Filter {
            doc_id: Some(doc_id.to_string()),
            status: Some(OpStatus::Accepted),
            limit: Some(limit),
            ..Filter::default()
        })
    }

    /// The set of `content_hash` values across this doc's accepted ops — every
    /// content state the document has ever materialized to (a revert recreates
    /// an old hash, so this is "was once exactly this", not strict ancestry).
    /// The sync enrollment classification (`sync-enrollment-hash-classification`)
    /// tests a peer's current hash against this set to decide bind / fast-forward
    /// / Blocked. Per `sync-content-hash-column`.
    ///
    /// status: op-log-multi-device-sync
    pub fn doc_history_hashes(
        &self,
        doc_id: &str,
    ) -> Result<std::collections::HashSet<String>, Error> {
        self.locked(|inner| meta::doc_content_hashes(&inner.index, doc_id))
    }

    /// Ordered, bounded recent-history-hash window for a doc: distinct accepted
    /// `content_hash`es, `timestamp_ms DESC, rowid DESC`, capped at `limit`.
    /// The sync manifest's `recent_history_hashes` uses this so the carried
    /// window is the *most-recent* N rather than an arbitrary HashSet-iteration
    /// subset (`bug-sync-history-hashset-truncation-nondet`).
    ///
    /// status: op-log-multi-device-sync
    pub fn recent_doc_history_hashes(
        &self,
        doc_id: &str,
        limit: usize,
    ) -> Result<Vec<String>, Error> {
        self.locked(|inner| meta::doc_recent_content_hashes(&inner.index, doc_id, limit))
    }

    /// Vault-wide accepted-op history, newest-first (the recent-activity feed).
    ///
    /// status: op-log-history-materialization
    pub fn vault_history(&self, limit: usize) -> Result<Vec<OpMetadata>, Error> {
        self.query_metadata(&Filter {
            status: Some(OpStatus::Accepted),
            limit: Some(limit),
            ..Filter::default()
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
            let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
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
            let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
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

    /// Query the editorial-metadata side table. Plain-Rust filter and rows.
    ///
    /// status: op-log-side-table
    pub fn query_metadata(&self, filter: &Filter) -> Result<Vec<OpMetadata>, Error> {
        self.locked(|inner| meta::query_metadata(&inner.index, filter))
    }

    /// The current pending queue for a document, reconstituted from disk if
    /// the document isn't loaded yet.
    ///
    /// status: op-log-pending-queue
    pub fn pending_ops(&self, doc_id: &str) -> Result<Vec<PendingOp>, Error> {
        self.locked(|inner| {
            let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
            Ok(state.pending.clone())
        })
    }

    /// Every document id in this vault, by scanning the `<doc-id>.ops` history
    /// files in the oplog dir — the same directory-scan style as
    /// [`all_pending_ops`] (which scans `.pending`). Read-only; mints nothing.
    /// The sync layer (`hiker-sync`) calls this to enumerate the vault for
    /// manifest building, then resolves each id to its path / content hash
    /// through the existing plain-typed verbs.
    ///
    /// status: op-log-multi-device-sync
    pub fn list_doc_ids(&self) -> Result<Vec<String>, Error> {
        store::scan_doc_ids(&self.oplog_dir, "ops")
    }

    /// Every pending op across the whole vault, each paired with its `doc_id`.
    /// Walks the `<doc-id>.pending` files in the oplog dir so the vault-wide
    /// activity feed can surface unreviewed proposals without knowing which
    /// documents have queues. Pending ops aren't in the side table — they live
    /// only on disk here.
    ///
    /// status: op-log-pending-queue
    pub fn all_pending_ops(&self) -> Result<Vec<(String, PendingOp)>, Error> {
        let doc_ids = store::scan_doc_ids(&self.oplog_dir, "pending")?;
        let mut out: Vec<(String, PendingOp)> = Vec::new();
        for doc_id in doc_ids {
            let pending = self.locked(|inner| {
                let state = Self::ensure_loaded(&self.oplog_dir, inner, &doc_id)?;
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
            let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
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
            let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
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

    /// Resolve a vault-relative path to its doc_id. Under path-as-identity the
    /// id IS the path (`op-log-path-identity`): this returns `Some(path)` iff a
    /// document exists at `path` (its persisted `.ops` history is present), else
    /// `None`. A thin existence shim kept so the path→id callers compile
    /// unchanged.
    ///
    /// status: op-log-path-identity
    pub fn doc_id_for_path(&self, path: &str) -> Result<Option<String>, Error> {
        if store::ops_path(&self.oplog_dir, path).exists() {
            Ok(Some(path.to_string()))
        } else {
            Ok(None)
        }
    }

    /// Record a path that bootstrap could not seed (e.g. non-UTF-8 bytes).
    /// Idempotent upsert into `bootstrap_skipped` so subsequent bootstrap
    /// runs skip it silently without re-reading the file.
    pub fn mark_bootstrap_skipped(&self, path: &str, reason: &str) -> Result<(), Error> {
        self.locked(|inner| meta::put_bootstrap_skip(&inner.index, path, reason))
    }

    /// Returns `true` when `path` has a persistent bootstrap-skip marker.
    pub fn is_bootstrap_skipped(&self, path: &str) -> Result<bool, Error> {
        self.locked(|inner| meta::is_bootstrap_skipped(&inner.index, path))
    }

    /// The current vault-relative path for a doc_id. Under path-as-identity the
    /// id IS the path (`op-log-path-identity`): returns `Some(doc_id)` iff a
    /// document exists at that path (its persisted `.ops` history is present), so
    /// the changes/activity projection can resolve a side-table row's `doc_id`
    /// back to a path (which is itself). A thin identity shim.
    ///
    /// status: op-log-path-identity
    pub fn path_for_doc(&self, doc_id: &str) -> Result<Option<String>, Error> {
        if store::ops_path(&self.oplog_dir, doc_id).exists() {
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
            let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
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

    /// GC accepted/rejected side-table rows older than the per-status cutoff.
    /// `now_ms - retention_days*86_400_000` is the caller's cutoff. Pending
    /// ops are never GC'd (they aren't in this table).
    ///
    /// status: op-log-status-states
    pub fn gc_metadata(&self, status: OpStatus, cutoff_ms: i64) -> Result<usize, Error> {
        self.locked(|inner| meta::gc_status(&inner.index, status, cutoff_ms))
    }

    // ── internals ──────────────────────────────────────────────────────

    /// The single accepted-text commit path. Resolves `input` to minimal
    /// localized spans and splices them into the `accepted` String
    /// (high-offset-first), then persists in the spec-mandated order — history
    /// frame, side-table row, `.md` — **all under one lock hold**, so a
    /// concurrent writer (another save, an accept, a future sync receive) can't
    /// interleave between the in-memory mutation and the disk writes. The
    /// `.ops` frame IS the durable persistence (there is no separate serialized
    /// snapshot). Returns `false` when the edit is empty (a no-op save /
    /// self-write echo).
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
        let now = now_ms();
        self.locked(|inner| {
            let op_id = ulid::Ulid::new().to_string();
            let (materialized, rel_path) = {
                let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
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
                // the doc_id.
                let rel_path = doc_id.to_string();
                let materialized = state.accepted();
                // The `.ops` history frame is the durable persistence; the
                // self-describing metadata rides on it (author/op-kind/surface),
                // and `retain_frame` appends the matching regenerable index row.
                let author_wire = author.as_wire();
                let (index, state) = inner.index_and_state(&self.oplog_dir, doc_id)?;
                Self::retain_frame(
                    &self.oplog_dir, index, doc_id, state,
                    &store::FrameSpec {
                        op_id: &op_id,
                        text: &materialized.text,
                        tombstone: materialized.tombstone,
                        timestamp_ms: now,
                        meta: &store::FrameMeta {
                            author: &author_wire,
                            op_kind: op_kind.as_str(),
                            rename_from: None,
                            surface,
                            session_id: None,
                            batch_id: None,
                            metadata: &serde_json::Value::Null,
                        },
                    },
                )?;
                (materialized, rel_path)
            };
            write_md_file(&self.oplog_dir, Some(&rel_path), &materialized)?;
            Ok(true)
        })
    }

    /// Load a document's `accepted` text + tombstone (from its `.ops` history)
    /// and pending queue from disk into the cache if not already present,
    /// returning a mutable handle. The NEWEST `.ops` frame's materialized text
    /// IS the current accepted content (`op-log-materialization`); a doc with no
    /// frames yet is unknown.
    fn ensure_loaded<'a>(
        oplog_dir: &Path,
        inner: &'a mut Inner,
        doc_id: &str,
    ) -> Result<&'a mut DocState, Error> {
        Self::ensure_loaded_in(oplog_dir, &mut inner.docs, doc_id)
    }

    /// The doc-cache half of [`ensure_loaded`], operating on just the `docs`
    /// map so callers that also need `inner.index` (the `retain_frame` path via
    /// [`Inner::index_and_state`]) can hold both borrows at once.
    fn ensure_loaded_in<'a>(
        oplog_dir: &Path,
        docs: &'a mut HashMap<String, DocState>,
        doc_id: &str,
    ) -> Result<&'a mut DocState, Error> {
        if !docs.contains_key(doc_id) {
            let (accepted, accepted_tombstone) = store::load_accepted(oplog_dir, doc_id)?
                .ok_or_else(|| Error::UnknownDoc(doc_id.to_string()))?;
            let pending = store::load_pending(oplog_dir, doc_id)?;
            docs.insert(
                doc_id.to_string(),
                DocState {
                    // The loaded accepted text IS the newest retained frame, so
                    // seed `last_retained_text` with it: the next edit then
                    // stores as a delta against it (no forced keyframe, no `.ops`
                    // re-read), and the delta chain stays anchored.
                    last_retained_text: Some(accepted.clone()),
                    accepted,
                    accepted_tombstone,
                    working: None,
                    pending,
                    deltas_since_keyframe: 0,
                },
            );
        }
        docs.get_mut(doc_id)
            .ok_or_else(|| Error::UnknownDoc(doc_id.to_string()))
    }

}

/// The producer metadata to keep on an *accepted* side-table row, with the
/// bulky pending edit text dropped. Drift detection and the `edit_note`
/// preview need the full `old_str` / `new_str` / `new_content` *while the op
/// is pending* (they live in `<doc-id>.pending`), but once the op is accepted
/// the content is in the document itself — so the durable record keeps only
/// the compact fields (`new_path`, `trail_id`, …) and the typed `AnchorHint`
/// on `op_kind`, never a second copy of the matched/inserted text.
///
/// status: op-log-op-shape
fn durable_metadata(metadata: &serde_json::Value) -> serde_json::Value {
    let mut out = metadata.clone();
    if let serde_json::Value::Object(map) = &mut out {
        for key in ["old_str", "new_str", "new_content"] {
            map.remove(key);
        }
    }
    out
}

/// The vault root for an oplog dir (`<vault>/.hiker/oplog` → `<vault>`).
fn vault_root_of(oplog_dir: &Path) -> &Path {
    oplog_dir
        .parent()
        .and_then(|p| p.parent())
        .unwrap_or(oplog_dir)
}

/// Atomically write `materialized` to `rel_path` under the vault root. A
/// tombstoned doc or a path-less doc (`rel_path = None`) writes nothing. Pure
/// filesystem I/O — no lock — so the commit path can call it inside its lock
/// hold and the lifecycle ops can resolve the path first and call it after.
///
/// status: op-log-atomic-write
/// status: op-log-disk-canonical
fn write_md_file(
    oplog_dir: &Path,
    rel_path: Option<&str>,
    materialized: &Materialized,
) -> Result<(), Error> {
    if materialized.tombstone {
        return Ok(());
    }
    let Some(rel_path) = rel_path else {
        return Ok(());
    };
    let abs = vault_root_of(oplog_dir).join(rel_path);
    if let Some(parent) = abs.parent() {
        fs::create_dir_all(parent)?;
    }
    store::write_atomic(&abs, materialized.text.as_bytes())
}

/// How [`OpLog::register_document`](crate::oplog::OpLog) reconciles a
/// newly-registered document with the `.md` on disk. Lives here beside
/// [`write_md_file`] / [`verify_md_matches`] (the two disk actions it selects
/// between) so the lifecycle split file stays a pure `impl OpLog` continuation.
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
    oplog_dir: &Path,
    rel_path: Option<&str>,
    materialized: &Materialized,
) -> Result<(), Error> {
    if materialized.tombstone {
        return Ok(());
    }
    let Some(rel_path) = rel_path else {
        return Ok(());
    };
    let abs = vault_root_of(oplog_dir).join(rel_path);
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
fn remove_old_md_file(oplog_dir: &Path, rel_path: &str) -> Result<(), Error> {
    let abs = vault_root_of(oplog_dir).join(rel_path);
    if abs.exists() {
        fs::remove_file(&abs)?;
    }
    Ok(())
}

/// blake3 hex of a materialized note's bytes — the `content_hash` stamped on
/// every accepted-op metadata row, so the sync enrollment classification can
/// ask "was this doc ever exactly this content" with one indexed query
/// (`sync-content-hash-column`).
fn content_hash(text: &str) -> String {
    blake3::hash(text.as_bytes()).to_hex().to_string()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
