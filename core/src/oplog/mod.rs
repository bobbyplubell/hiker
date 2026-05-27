//! The op-log substrate: one Yrs `Doc` per hiker document plus a vault-wide
//! side table of editorial metadata (author, status, surface). Markdown on
//! disk is the canonical materialization of *accepted* operations; pending
//! agent operations are held in a per-document queue until the user accepts.
//!
//! Module discipline mirrors `core::store`:
//! the `yrs` and `rusqlite` dependencies are confined to this module, and the
//! [`OpLog`] public surface returns plain Rust types only — no Yrs `Doc`,
//! `Update`, or rusqlite row ever crosses the boundary. The two-doc model
//! (`accepted` + `pending_view`), the pending queue, the side table, and the
//! on-disk layout all live in the submodules; this root owns the open path
//! and the public verbs.
//
// status: op-log-module
// status: op-log-disk-canonical

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::Connection;
use yrs::{Doc, ReadTxn, StateVector, Transact};

pub mod doc;
pub mod error;
pub mod meta;
pub mod shapes;
pub mod store;
mod history;
mod sync;
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

/// Public, plain-Rust view of `materialize(doc)`. Re-exported so callers can
/// name the return type of [`OpLog::materialize_accepted`] without seeing the
/// Yrs `Doc`.
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

/// Compaction trigger: rewrite `<doc-id>.yrs` when its size exceeds this
/// multiple of the materialized content size. Mirrors `[op-log] compact_threshold`.
const DEFAULT_COMPACT_THRESHOLD: f32 = 4.0;

/// In-memory state for one open document: its `accepted` Doc, the user's
/// uncommitted `working` overlay, and its pending queue.
///
/// `accepted` is the canonical, on-disk CRDT state. `working` is `accepted`
/// plus the user's *uncommitted* edits as `user` ops — `None` when the buffer
/// is clean (it then equals `accepted`), `Some(doc)` once the user has typed.
/// `working` lives in memory only (crash recovery is the autosave sidecar's
/// job, not this layer's) and never contains pending agent ops: the editable
/// buffer is `materialize(accepted + working)`, and pending renders as an
/// overlay on top of that. The pending queue is the deferred-apply buffer
/// that survives restarts via `<doc-id>.pending`.
///
/// status: op-log-working-layer
struct DocState {
    accepted: Doc,
    working: Option<Doc>,
    pending: Vec<PendingOp>,
    /// State vector of `accepted` as last persisted to disk (the `.yrs` base
    /// plus every `.yrslog` delta appended so far). A commit appends only the
    /// ops `accepted` gained beyond this, then advances it — so a save costs
    /// O(edit), not O(doc). Per `op-log-yrs-delta-log`.
    persisted_sv: StateVector,
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


/// The op log for one vault. Holds the side-table + path-index SQLite
/// connections and the lazily-opened per-document state behind one mutex.
/// Cheap to wrap in `Arc<OpLog>`.
///
/// status: op-log-module
pub struct OpLog {
    oplog_dir: PathBuf,
    inner: Mutex<Inner>,
}

struct Inner {
    meta: Connection,
    index: Connection,
    docs: HashMap<String, DocState>,
    compact_threshold: f32,
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
    /// save lands as localized, mergeable Yrs ops rather than a whole-`text`
    /// rewrite.
    FullText(&'a str),
}

impl OpLog {
    /// Open or create the op log under `<vault>/.hiker/oplog/`. Runs the
    /// idempotent schema bootstrap for both SQLite files (fail-loud on a
    /// version mismatch) and checks every already-persisted document for
    /// compaction.
    ///
    /// status: op-log-module
    /// status: op-log-store-layout
    pub fn open(vault_root: &Path) -> Result<Self, Error> {
        Self::open_with_threshold(vault_root, DEFAULT_COMPACT_THRESHOLD)
    }

    /// `open` with an explicit compaction threshold (the `[op-log]
    /// compact_threshold` config value).
    ///
    /// status: op-log-compaction
    pub fn open_with_threshold(vault_root: &Path, compact_threshold: f32) -> Result<Self, Error> {
        let oplog_dir = vault_root.join(".hiker").join("oplog");
        fs::create_dir_all(&oplog_dir)?;
        let meta = meta::open_meta(&oplog_dir)?;
        let index = meta::open_index(&oplog_dir)?;
        let log = Self {
            oplog_dir,
            inner: Mutex::new(Inner {
                meta,
                index,
                docs: HashMap::new(),
                compact_threshold,
            }),
        };
        log.compact_all_on_open()?;
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

    /// On open, rewrite any `<doc-id>.yrs` whose size exceeds
    /// `compact_threshold ×` its materialized size as a fresh snapshot.
    ///
    /// status: op-log-compaction
    fn compact_all_on_open(&self) -> Result<(), Error> {
        let mut doc_ids: Vec<String> = Vec::new();
        for entry in fs::read_dir(&self.oplog_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("yrs")
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            {
                doc_ids.push(stem.to_string());
            }
        }
        let threshold = self.locked(|inner| Ok(inner.compact_threshold))?;
        for doc_id in doc_ids {
            let Some(bytes) = store::load_yrs(&self.oplog_dir, &doc_id)? else {
                continue;
            };
            // Replay the base + any appended deltas to get the live doc.
            let doc = doc::load_doc(&doc_id, &bytes)?;
            for frame in store::load_yrslog(&self.oplog_dir, &doc_id)? {
                let _ = doc::apply_update(&doc, &doc_id, &frame);
            }
            let materialized = doc::materialize(&doc);
            if store::needs_compaction(&self.oplog_dir, &doc_id, materialized.text.len(), threshold)
            {
                // Fold the deltas into a fresh compact base, then clear the log.
                // Base first (atomic rename), log second — a crash between the
                // two leaves the now-redundant log, which replays idempotently.
                store::save_yrs(&self.oplog_dir, &doc_id, &doc::encode_full(&doc))?;
                store::clear_yrslog(&self.oplog_dir, &doc_id)?;
                tracing::info!(doc_id, "oplog: compacted yrs snapshot on open");
            }
        }
        Ok(())
    }

    /// Register a brand-new document (a `Create` op then a content `Replace`
    /// inserting `initial_text`). Mints a path→doc_id row, seeds and persists
    /// the Yrs Doc, writes the side-table rows, and atomically writes the
    /// initial `.md` (which equals `materialize(accepted)` by construction).
    /// Returns the new doc_id. Used by the bootstrap and create paths.
    ///
    /// status: op-log-document-shape
    /// status: op-log-disk-canonical
    pub fn create_document(
        &self,
        path: &str,
        kind: &str,
        initial_text: &str,
        author: &Author,
    ) -> Result<String, Error> {
        let doc_id = ulid::Ulid::new().to_string();
        let accepted = doc::seed_doc(kind, path, initial_text);
        let cid = accepted.client_id();
        let client_id = cid.get() as i64;
        let clock_hi = {
            let txn = accepted.transact();
            txn.state_vector().get(&cid) as i64
        };
        let now = now_ms();
        // Creation is a SINGLE accepted op: the `Create` op carries the seed
        // text's full clock range (0..clock_hi) and owns the first history
        // frame, so `materialize_at(create_op)` reconstructs the note as of
        // creation. A separate content op would have no retained frame and show
        // in the version dropdown as an unloadable "version".
        let create_op_id = ulid::Ulid::new().to_string();
        let snapshot = doc::encode_full(&accepted);
        let materialized = doc::materialize(&accepted);
        let seed_op_id = create_op_id.clone();
        // Index, side-table rows, Yrs state, history frame, doc-cache insert,
        // and the `.md` all land under one lock so a concurrent writer can't
        // observe (or race) a half-registered document.
        self.locked(|inner| {
            store::save_yrs(&self.oplog_dir, &doc_id, &snapshot)?;
            meta::put_doc_id(&inner.index, path, &doc_id)?;
            // The `Create` op spans the seed text's clock range (0..clock_hi)
            // and is the note's first content version.
            meta::insert_metadata(
                &inner.meta,
                &meta::MetadataInsert {
                    doc_id: &doc_id,
                    op_id: &create_op_id,
                    yrs_client_id: client_id,
                    yrs_clock_lo: 0,
                    yrs_clock_hi: clock_hi,
                    author,
                    op_kind: &OpKind::Create,
                    status: OpStatus::Accepted,
                    timestamp_ms: now,
                    content_hash: Some(&content_hash(&materialized.text)),
                    surface: None,
                    session_id: None,
                    batch_id: None,
                    metadata: &serde_json::Value::Null,
                },
            )?;
            // The first history frame is a self-contained keyframe.
            store::append_op(
                &self.oplog_dir,
                &doc_id,
                &store::RetainedOp::keyframe(seed_op_id, &materialized.text, materialized.tombstone, now)?,
            )?;
            // The base `.yrs` snapshot was just written, so the persisted state
            // vector is `accepted`'s current one — no `.yrslog` deltas yet. The
            // keyframe just written anchors the history delta chain.
            let persisted_sv = doc::state_vector(&accepted);
            inner.docs.insert(
                doc_id.clone(),
                DocState {
                    accepted,
                    working: None,
                    pending: Vec::new(),
                    persisted_sv,
                    last_retained_text: Some(materialized.text.clone()),
                    deltas_since_keyframe: 0,
                },
            );
            // The on-disk `.md` equals `materialize(accepted)` by construction.
            write_md_file(&self.oplog_dir, Some(path), &materialized)
        })?;
        Ok(doc_id)
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
    /// op. Diffing — rather than replacing the whole `text` — keeps each save
    /// a minimal, mergeable CRDT edit, so the substrate is sync-correct and
    /// the Yrs history doesn't churn the whole document on every save. A save
    /// that changes nothing is a no-op (`Ok(false)`).
    ///
    /// status: op-log-disk-canonical
    /// status: op-log-yrs-backed
    pub fn apply_user_text(&self, doc_id: &str, new_text: &str) -> Result<bool, Error> {
        self.commit_text_edit(doc_id, EditInput::FullText(new_text), &Author::User, None)
    }

    /// Reconcile an external edit: a `.md` file changed on disk outside
    /// hiker. Diffs `materialize(accepted)` against `disk_text` into minimal
    /// localized spans, applies them to `accepted`'s `text` Y.Text, and writes
    /// a side-table row authored `external`. When `disk_text` already equals
    /// the accepted materialization the diff is empty and the call is a no-op
    /// (a self-write echo) — the safety net behind `watcher-suppress-self-writes`.
    /// Frontmatter and body share the same `text`, so one diff covers both.
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

    /// Save: fold the user's `working` overlay into `accepted`. Returns
    /// `Ok(false)` when the buffer is clean (no `working`). Otherwise reads
    /// `materialize(working).text` and commits it through the shared text-commit
    /// path as a `user` op (the diff against `accepted` yields minimal localized
    /// `user` ops, then persists `.yrs`, the metadata row, the history frame,
    /// and the `.md` atomically), then clears `working`. Returns `Ok(true)`.
    /// Lives here (rather than with the other `working` verbs in `working.rs`)
    /// because it bridges into [`commit_text_edit`](Self::commit_text_edit).
    ///
    /// The non-reentrant lock forces three hops: read the working text under one
    /// `locked`, let `commit_text_edit` take its own lock, then clear `working`.
    ///
    /// status: op-log-working-layer
    /// status: op-log-atomic-write
    pub fn commit_working(&self, doc_id: &str) -> Result<bool, Error> {
        let text = self.locked(|inner| {
            let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
            Ok(state.working.as_ref().map(|w| doc::materialize(w).text))
        })?;
        let Some(text) = text else {
            return Ok(false);
        };
        self.commit_text_edit(doc_id, EditInput::FullText(&text), &Author::User, None)?;
        self.locked(|inner| {
            let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
            state.working = None;
            Ok(())
        })?;
        Ok(true)
    }

    /// Tombstone a document: set `meta.tombstone = true` directly on
    /// `accepted`, write a `Tombstone` side-table row, and persist the Yrs
    /// Doc. The on-disk `.md` is left in place (deletion of the file itself
    /// is the caller's concern; the op log only records the logical delete).
    ///
    /// status: op-log-op-shape
    /// status: op-log-atomic-write
    pub fn tombstone_document(&self, doc_id: &str, author: &Author) -> Result<(), Error> {
        let now = now_ms();
        // Mutate + persist under one lock so a concurrent writer can't
        // interleave between the in-memory tombstone and its disk persistence.
        // The path → doc_id mapping is kept so the history / activity feed can
        // still resolve a deleted note by path; the doc reads as tombstoned,
        // and the on-disk `.md` is left in place (file deletion is the caller's).
        self.locked(|inner| {
            let op_id = ulid::Ulid::new().to_string();
            let (client_id, lo, hi, hash) = {
                let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
                let cid = state.accepted.client_id();
                let lo = doc::state_clock(&state.accepted, cid);
                doc::apply_tombstone(&state.accepted);
                let hi = doc::state_clock(&state.accepted, cid);
                // Persist the Yrs delta before the metadata row that references
                // its clock range, so a crash can't leave a row pointing at
                // unpersisted state.
                Self::persist_accepted(&self.oplog_dir, doc_id, state)?;
                let materialized = doc::materialize(&state.accepted);
                Self::retain_frame(
                    &self.oplog_dir, doc_id, state, op_id.clone(),
                    &materialized.text, materialized.tombstone, now,
                )?;
                (cid.get() as i64, lo, hi, content_hash(&materialized.text))
            };
            meta::insert_metadata(
                &inner.meta,
                &meta::MetadataInsert {
                    doc_id,
                    op_id: &op_id,
                    yrs_client_id: client_id,
                    yrs_clock_lo: lo,
                    yrs_clock_hi: hi,
                    author,
                    op_kind: &OpKind::Tombstone,
                    status: OpStatus::Accepted,
                    timestamp_ms: now,
                    content_hash: Some(&hash),
                    surface: None,
                    session_id: None,
                    batch_id: None,
                    metadata: &serde_json::Value::Null,
                },
            )?;
            Ok(())
        })
    }

    /// Rename a document: update `meta.path` on `accepted`, repoint the
    /// `doc-index.db` mapping, write a `Rename { from }` side-table row, and
    /// persist the Yrs Doc. The caller is responsible for the filesystem
    /// rename of the `.md`; this records the logical rename.
    ///
    /// status: op-log-op-shape
    /// status: op-log-atomic-write
    pub fn rename_document(
        &self,
        doc_id: &str,
        new_path: &str,
        author: &Author,
    ) -> Result<(), Error> {
        let now = now_ms();
        // Mutate `accepted`, repoint the path index, and persist — all under
        // one lock so a concurrent writer can't interleave.
        self.locked(|inner| {
            let op_id = ulid::Ulid::new().to_string();
            let (client_id, lo, hi, from, hash) = {
                let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
                let from = doc::meta_string(&state.accepted, "path").unwrap_or_default();
                let cid = state.accepted.client_id();
                let lo = doc::state_clock(&state.accepted, cid);
                doc::apply_rename(&state.accepted, new_path);
                let hi = doc::state_clock(&state.accepted, cid);
                // Persist the Yrs delta before the metadata row that references
                // its clock range, so a crash can't leave a row pointing at
                // unpersisted state.
                Self::persist_accepted(&self.oplog_dir, doc_id, state)?;
                let materialized = doc::materialize(&state.accepted);
                Self::retain_frame(
                    &self.oplog_dir, doc_id, state, op_id.clone(),
                    &materialized.text, materialized.tombstone, now,
                )?;
                (cid.get() as i64, lo, hi, from, content_hash(&materialized.text))
            };
            // Repoint the path index atomically (drops any stale row for this
            // doc), so a later note created at `from` mints its own doc.
            meta::repoint_doc(&inner.index, doc_id, new_path)?;
            meta::insert_metadata(
                &inner.meta,
                &meta::MetadataInsert {
                    doc_id,
                    op_id: &op_id,
                    yrs_client_id: client_id,
                    yrs_clock_lo: lo,
                    yrs_clock_hi: hi,
                    author,
                    op_kind: &OpKind::Rename { from },
                    status: OpStatus::Accepted,
                    timestamp_ms: now,
                    content_hash: Some(&hash),
                    surface: None,
                    session_id: None,
                    batch_id: None,
                    metadata: &serde_json::Value::Null,
                },
            )?;
            Ok(())
        })
    }

    /// Stage a batch of producer edits as pending ops. Each edit is
    /// translated to a serialized Yrs update against a clone of `accepted`
    /// (the clone is discarded) and queued in `<doc-id>.pending`. No side-
    /// table row is written yet — pending ops have no Yrs clock range until
    /// they land in `accepted` on accept.
    ///
    /// status: op-log-pending-queue
    /// status: op-log-agent-replica
    pub fn stage_pending(
        &self,
        doc_id: &str,
        edits: &[EditSpec],
        ctx: &ProducerCtx,
    ) -> Result<StageOutcome, Error> {
        let batch_id = ulid::Ulid::new().to_string();
        let now = now_ms();
        let mut op_ids = Vec::with_capacity(edits.len());
        self.locked(|inner| {
            let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
            // Produce edits against this session's *pending view* (accepted +
            // the session's own queued ops), not bare `accepted`, so a follow-up
            // edit can anchor on (or diff against) content the agent staged in a
            // prior, not-yet-accepted edit — the `op-log-agent-replica` contract
            // `get_note` already reads through. Each op's update is a delta
            // against this view; `materialize_pending_view` applies the session's
            // ops in order, so they compose. Built once before the loop: within
            // one call every edit resolves against the pre-call view, matching
            // the producer's own per-edit anchor validation.
            let base_doc = doc::clone_doc(&state.accepted);
            for op in &state.pending {
                if op.session_id == ctx.session_id {
                    let _ = doc::apply_update(&base_doc, doc_id, &op.yrs_update);
                }
            }
            for edit in edits {
                let produced = match &edit.old_str {
                    // Prefer resolving the anchor against `accepted` so an
                    // independent edit stays a standalone op (per-hunk
                    // accept/reject keeps working). Fall back to the session's
                    // pending view only when the anchor isn't in `accepted` —
                    // a follow-up edit anchored on the agent's own staged-but-
                    // unaccepted content.
                    Some(old_str) => match doc::produce_replace(&state.accepted, old_str, &edit.new_str) {
                        Ok(produced) => produced,
                        Err(_) => doc::produce_replace(&base_doc, old_str, &edit.new_str)?,
                    },
                    // A whole-document rewrite (`write_note` / `set_frontmatter`
                    // / `apply_tag`): `new_str` is the full new file. Diff it
                    // against the pending view so the op replaces the whole
                    // `text` — never appends after the existing frontmatter
                    // fence (which would duplicate the frontmatter). An
                    // unchanged rewrite produces no op.
                    None => match doc::produce_content_replace(&base_doc, &edit.new_str) {
                        Some(produced) => produced,
                        None => continue,
                    },
                };
                let op_id = ulid::Ulid::new().to_string();
                op_ids.push(op_id.clone());
                state.pending.push(PendingOp {
                    op_id,
                    yrs_update: produced.yrs_update,
                    op_kind: produced.op_kind,
                    author: ctx.author.clone(),
                    session_id: ctx.session_id.clone(),
                    surface: ctx.surface.clone(),
                    batch_id: Some(batch_id.clone()),
                    created_at_ms: now,
                    metadata: serde_json::json!({
                        "new_str": edit.new_str,
                        "old_str": edit.old_str,
                    }),
                });
            }
            store::save_pending(&self.oplog_dir, doc_id, &state.pending)
        })?;
        Ok(StageOutcome { batch_id, op_ids })
    }

    /// Stage a single pending content edit from a *whole new document text*
    /// (the producer already computed the full new file). Diffs against
    /// `materialize(accepted)` and queues one pending op tagged per `ctx`,
    /// sharing a fresh `batch_id`. The op-kind is `SetFrontmatter` when the
    /// change lands inside the frontmatter fence (the cluster-editor tag /
    /// `apply_tag` shape), else `Replace`. A no-op (new text == current) stages
    /// nothing and returns an empty outcome.
    ///
    /// status: op-log-pending-queue
    pub fn stage_pending_content(
        &self,
        doc_id: &str,
        new_text: &str,
        ctx: &ProducerCtx,
    ) -> Result<StageOutcome, Error> {
        let batch_id = ulid::Ulid::new().to_string();
        let now = now_ms();
        let mut op_ids = Vec::new();
        self.locked(|inner| {
            let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
            let Some(produced) = doc::produce_content_replace(&state.accepted, new_text) else {
                return Ok(());
            };
            let op_id = ulid::Ulid::new().to_string();
            op_ids.push(op_id.clone());
            state.pending.push(PendingOp {
                op_id,
                yrs_update: produced.yrs_update,
                op_kind: produced.op_kind,
                author: ctx.author.clone(),
                session_id: ctx.session_id.clone(),
                surface: ctx.surface.clone(),
                batch_id: Some(batch_id.clone()),
                created_at_ms: now,
                metadata: serde_json::json!({ "new_content": new_text }),
            });
            store::save_pending(&self.oplog_dir, doc_id, &state.pending)
        })?;
        Ok(StageOutcome { batch_id, op_ids })
    }

    /// Stage a batch of pending `Rename` ops sharing one cross-document
    /// `batch_id` — the multi-file reorganization seam (`op-log-reorg-batch`).
    /// Each `(doc_id, new_path)` produces one pending `Rename { from }` op on
    /// its document; nothing reaches disk until [`accept_batch`](Self::accept_batch).
    /// The batch is a review/display grouping, *not* a transaction: accept
    /// applies each rename independently and skips failures (partial apply).
    ///
    /// Returns the minted `batch_id` and the per-op ids (across documents).
    /// This is the only place a `batch_id` spans documents — note-edit batches
    /// (`stage_pending`) stay within one document.
    ///
    /// status: op-log-reorg-batch
    /// status: op-log-pending-queue
    pub fn stage_pending_renames(
        &self,
        renames: &[(String, String)],
        ctx: &ProducerCtx,
    ) -> Result<StageOutcome, Error> {
        let batch_id = ulid::Ulid::new().to_string();
        let now = now_ms();
        let mut op_ids = Vec::with_capacity(renames.len());
        for (doc_id, new_path) in renames {
            let op_id = ulid::Ulid::new().to_string();
            self.locked(|inner| {
                let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
                let produced = doc::produce_rename(&state.accepted, new_path);
                state.pending.push(PendingOp {
                    op_id: op_id.clone(),
                    yrs_update: produced.yrs_update,
                    op_kind: produced.op_kind,
                    author: ctx.author.clone(),
                    session_id: ctx.session_id.clone(),
                    surface: ctx.surface.clone(),
                    batch_id: Some(batch_id.clone()),
                    created_at_ms: now,
                    metadata: serde_json::json!({ "new_path": new_path }),
                });
                store::save_pending(&self.oplog_dir, doc_id, &state.pending)
            })?;
            op_ids.push(op_id);
        }
        Ok(StageOutcome { batch_id, op_ids })
    }

    /// The `(doc_id, op_id)` pairs of every pending op across the vault sharing
    /// `batch_id`. The handle the batch accept/reject verbs resolve a reorg
    /// batch through (a `batch_id` may span documents per `op-log-reorg-batch`).
    ///
    /// status: op-log-reorg-batch
    pub fn pending_ops_in_batch(&self, batch_id: &str) -> Result<Vec<(String, String)>, Error> {
        let all = self.all_pending_ops()?;
        Ok(all
            .into_iter()
            .filter(|(_, op)| op.batch_id.as_deref() == Some(batch_id))
            .map(|(doc_id, op)| (doc_id, op.op_id))
            .collect())
    }

    /// Accept an entire reorg batch by `batch_id`: apply each pending op in
    /// the batch independently, skipping any that fail (partial apply per
    /// `op-log-reorg-batch` — a target collision on one move does not block
    /// the rest). Returns the op ids that were successfully accepted.
    ///
    /// status: op-log-reorg-batch
    pub fn accept_batch(&self, batch_id: &str) -> Result<Vec<String>, Error> {
        let batch = self.pending_ops_in_batch(batch_id)?;
        let mut accepted = Vec::new();
        for (doc_id, op_id) in batch {
            match self.accept_pending(&doc_id, &op_id) {
                Ok(()) => accepted.push(op_id),
                Err(e) => {
                    tracing::warn!(
                        batch_id,
                        doc_id,
                        op_id,
                        error = %e,
                        "oplog: reorg batch op failed to apply; skipping (partial apply)"
                    );
                }
            }
        }
        Ok(accepted)
    }

    /// Reject an entire reorg batch by `batch_id`: drop each pending op in the
    /// batch from its document's queue (writing a `rejected` audit row). None
    /// reach `accepted`. Returns the op ids that were rejected.
    ///
    /// status: op-log-reorg-batch
    pub fn reject_batch(&self, batch_id: &str) -> Result<Vec<String>, Error> {
        let batch = self.pending_ops_in_batch(batch_id)?;
        let mut rejected = Vec::new();
        for (doc_id, op_id) in batch {
            self.reject_pending(&doc_id, &op_id)?;
            rejected.push(op_id);
        }
        Ok(rejected)
    }

    /// Accept a pending op: apply its serialized update to `accepted`, write
    /// an `accepted` side-table row, drop it from the queue, persist the
    /// Yrs Doc and the materialized `.md`.
    ///
    /// A pending `Rename` op carries its prior path in `OpKind::Rename { from }`;
    /// applying the update advances `meta.path` to the new location, so on
    /// accept the doc's `.md` is written at the new path, the old `.md` is
    /// removed, and `doc-index.db` is repointed (old path dropped, new path
    /// upserted) — the file moves on disk per `op-log-reorg-batch`.
    ///
    /// status: op-log-status-states
    /// status: op-log-atomic-write
    /// status: op-log-reorg-batch
    pub fn accept_pending(&self, doc_id: &str, op_id: &str) -> Result<(), Error> {
        let now = now_ms();
        // Everything — the rename collision pre-check, applying the op to
        // `accepted`, persisting `.yrs` / the metadata row / the history frame,
        // repointing the path index, and the `.md` write/move — runs under one
        // lock hold, so a concurrent writer can't interleave between the
        // in-memory mutation and its disk persistence (no lost update).
        self.locked(|inner| {
            // Pre-flight: a pending `Rename` whose target is already taken by a
            // *different* document is a collision. Refuse before mutating, so
            // the failed op stays queued and a reorg batch's other moves still
            // apply (partial apply per `op-log-reorg-batch`).
            let rename_target = {
                let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
                let op = state
                    .pending
                    .iter()
                    .find(|p| p.op_id == op_id)
                    .ok_or_else(|| Error::UnknownPendingOp(op_id.to_string()))?;
                match (&op.op_kind, op.metadata.get("new_path").and_then(|v| v.as_str())) {
                    (OpKind::Rename { .. }, Some(new_path)) => Some(new_path.to_string()),
                    _ => None,
                }
            };
            if let Some(new_path) = &rename_target
                && meta::doc_id_for_path(&inner.index, new_path)?
                    .is_some_and(|other| other != doc_id)
            {
                return Err(Error::Anchor(format!(
                    "rename target already occupied: {new_path}"
                )));
            }
            // Remove the op from the queue, apply it to `accepted`, materialize.
            let (materialized, client_id, lo, hi, op, rel_path) = {
                let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
                let idx = state
                    .pending
                    .iter()
                    .position(|p| p.op_id == op_id)
                    .ok_or_else(|| Error::UnknownPendingOp(op_id.to_string()))?;
                let op = state.pending.remove(idx);
                let cid = state.accepted.client_id();
                let lo = doc::state_clock(&state.accepted, cid);
                doc::apply_update(&state.accepted, doc_id, &op.yrs_update)?;
                // Replay the accepted op onto the user's uncommitted overlay
                // too, so `working` stays equal to `accepted + the user's ops`.
                // Without this the editable buffer (`materialize(accepted +
                // working)`) would drop the just-accepted content. Best-effort:
                // a drifted op simply doesn't contribute (the disk `.md` is
                // `materialize(accepted)` regardless — `working` is never on disk).
                if let Some(working) = &state.working {
                    let _ = doc::apply_update(working, doc_id, &op.yrs_update);
                }
                let hi = doc::state_clock(&state.accepted, cid);
                let materialized = doc::materialize(&state.accepted);
                let rel_path = doc::meta_string(&state.accepted, "path");
                store::save_pending(&self.oplog_dir, doc_id, &state.pending)?;
                // Persist the Yrs delta before the metadata row that references
                // its clock range, so a crash can't leave a row pointing at
                // unpersisted state.
                Self::persist_accepted(&self.oplog_dir, doc_id, state)?;
                Self::retain_frame(
                    &self.oplog_dir, doc_id, state, op.op_id.clone(),
                    &materialized.text, materialized.tombstone, now,
                )?;
                (materialized, cid.get() as i64, lo, hi, op, rel_path)
            };
            // A Rename repoints the path index (atomically) so the `.md` move
            // and later path resolution agree.
            if let (OpKind::Rename { .. }, Some(new_path)) = (&op.op_kind, &rename_target) {
                meta::repoint_doc(&inner.index, doc_id, new_path)?;
            }
            meta::insert_metadata(
                &inner.meta,
                &meta::MetadataInsert {
                    doc_id,
                    op_id: &op.op_id,
                    yrs_client_id: client_id,
                    yrs_clock_lo: lo,
                    yrs_clock_hi: hi,
                    author: &op.author,
                    op_kind: &op.op_kind,
                    status: OpStatus::Accepted,
                    timestamp_ms: now,
                    content_hash: Some(&content_hash(&materialized.text)),
                    surface: Some(&op.surface),
                    session_id: op.session_id.as_deref(),
                    batch_id: op.batch_id.as_deref(),
                    metadata: &durable_metadata(&op.metadata),
                },
            )?;
            // Write the `.md` at the doc's (post-accept) path; a Rename also
            // removes the old file once the new one is written.
            write_md_file(&self.oplog_dir, rel_path.as_deref(), &materialized)?;
            if let OpKind::Rename { from } = &op.op_kind
                && rel_path.as_deref() != Some(from.as_str())
            {
                remove_old_md_file(&self.oplog_dir, from)?;
            }
            Ok(())
        })
    }

    /// Reject a pending op: drop it from the queue and write a `rejected`
    /// audit row with the serialized update bytes stashed in the row's
    /// metadata. The op never enters `accepted`.
    ///
    /// status: op-log-status-states
    pub fn reject_pending(&self, doc_id: &str, op_id: &str) -> Result<(), Error> {
        let now = now_ms();
        let op = self.locked(|inner| {
            let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
            let idx = state
                .pending
                .iter()
                .position(|p| p.op_id == op_id)
                .ok_or_else(|| Error::UnknownPendingOp(op_id.to_string()))?;
            let op = state.pending.remove(idx);
            store::save_pending(&self.oplog_dir, doc_id, &state.pending)?;
            Ok(op)
        })?;
        let mut metadata = op.metadata.clone();
        if let serde_json::Value::Object(map) = &mut metadata {
            map.insert(
                "rejected_update".to_string(),
                serde_json::Value::Array(
                    op.yrs_update
                        .iter()
                        .map(|b| serde_json::Value::from(*b))
                        .collect(),
                ),
            );
        }
        self.locked(|inner| {
            meta::insert_metadata(
                &inner.meta,
                &meta::MetadataInsert {
                    doc_id,
                    op_id: &op.op_id,
                    // No Yrs range — the op never landed in `accepted`.
                    yrs_client_id: 0,
                    yrs_clock_lo: 0,
                    yrs_clock_hi: 0,
                    author: &op.author,
                    op_kind: &op.op_kind,
                    status: OpStatus::Rejected,
                    timestamp_ms: now,
                    // Rejected ops never land in `accepted`, so they have no
                    // materialized content to hash.
                    content_hash: None,
                    surface: Some(&op.surface),
                    session_id: op.session_id.as_deref(),
                    batch_id: op.batch_id.as_deref(),
                    metadata: &metadata,
                },
            )
        })
    }

    /// `materialize(accepted)` — the canonical state that equals the on-disk
    /// `.md` by construction.
    ///
    /// status: op-log-materialization
    /// status: op-log-disk-canonical
    pub fn materialize_accepted(&self, doc_id: &str) -> Result<DocContent, Error> {
        self.locked(|inner| {
            let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
            Ok(doc::materialize(&state.accepted).into())
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
        self.locked(|inner| meta::doc_content_hashes(&inner.meta, doc_id))
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
            let view = doc::clone_doc(&state.accepted);
            for op in &state.pending {
                if session.is_some() && op.session_id.as_deref() != session {
                    continue;
                }
                // Best-effort: a drifted op simply doesn't contribute to the
                // view (its apply errors are swallowed here, surfaced via
                // `is_pending_drifted`).
                let _ = doc::apply_update(&view, doc_id, &op.yrs_update);
            }
            Ok(doc::materialize(&view).into())
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
            let view = doc::clone_doc(&state.accepted);
            let _ = doc::apply_update(&view, doc_id, &op.yrs_update);
            Ok(doc::materialize(&view).into())
        })
    }

    /// Query the editorial-metadata side table. Plain-Rust filter and rows.
    ///
    /// status: op-log-side-table
    pub fn query_metadata(&self, filter: &Filter) -> Result<Vec<OpMetadata>, Error> {
        self.locked(|inner| meta::query_metadata(&inner.meta, filter))
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

    /// Every pending op across the whole vault, each paired with its
    /// `doc_id`. Walks the `<doc-id>.pending` files in the oplog dir so the
    /// vault-wide activity feed can surface unreviewed proposals without
    /// knowing which documents have queues. Pending ops carry no Yrs clock
    /// range (they aren't in the side table) — they live only on disk here.
    ///
    /// status: op-log-pending-queue
    /// Every document id in this vault, by scanning the `<doc-id>.yrs` base
    /// files in the oplog dir — the same directory-scan style as
    /// [`all_pending_ops`] (which scans `.pending`) and `compact_all_on_open`
    /// (which scans `.yrs`). Read-only; mints nothing. The sync layer
    /// (`hiker-sync`) calls this to enumerate the vault for manifest building,
    /// then resolves each id to its path / content hash through the existing
    /// plain-typed verbs — yrs stays confined to core.
    ///
    /// status: op-log-multi-device-sync
    pub fn list_doc_ids(&self) -> Result<Vec<String>, Error> {
        let mut doc_ids: Vec<String> = Vec::new();
        for entry in fs::read_dir(&self.oplog_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("yrs")
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            {
                doc_ids.push(stem.to_string());
            }
        }
        Ok(doc_ids)
    }

    pub fn all_pending_ops(&self) -> Result<Vec<(String, PendingOp)>, Error> {
        let mut doc_ids: Vec<String> = Vec::new();
        for entry in fs::read_dir(&self.oplog_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("pending")
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            {
                doc_ids.push(stem.to_string());
            }
        }
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
            let base = state.working.as_ref().unwrap_or(&state.accepted);
            let mut out = Vec::new();
            for op in &state.pending {
                if session.is_some() && op.session_id.as_deref() != session {
                    continue;
                }
                if let Some((op_start, op_end)) =
                    doc::affected_range(base, doc_id, &op.yrs_update)
                {
                    // Half-open overlap test.
                    if op_start < end && start < op_end {
                        out.push(op.op_id.clone());
                    }
                }
            }
            Ok(out)
        })
    }

    /// Whether a pending op has drifted: its anchor (`old_str`) no longer
    /// resolves against the current `accepted`, or its update fails to apply
    /// to a clone of current `accepted`.
    ///
    /// An anchored `Replace` carries the `old_str` it matched on; once
    /// `accepted` advances so that text no longer resolves to exactly one
    /// range, the position the agent's update targets is gone — the op is
    /// drifted regardless of whether Yrs's CRDT can still merge the bytes in
    /// somewhere. Whole-body rewrites and other anchorless ops fall back to
    /// the apply-to-a-clone check (a position-resolution failure is drift).
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
            let op = &state.pending[pos];
            // Check against the view the op was produced against: accepted plus
            // the session's earlier pending ops (queue order). A real drift
            // (the user changed `accepted` under the op) still surfaces because
            // the base starts from current accepted; a follow-up op anchored on
            // the agent's own prior pending edit is NOT falsely flagged.
            let base = doc::clone_doc(&state.accepted);
            for prior in &state.pending[..pos] {
                if prior.session_id == op.session_id {
                    let _ = doc::apply_update(&base, doc_id, &prior.yrs_update);
                }
            }
            if let Some(old_str) = op.metadata.get("old_str").and_then(|v| v.as_str()) {
                // Anchored op: the anchor must still resolve to exactly one
                // range in the base text.
                let current = doc::materialize(&base).text;
                return Ok(doc::resolve_anchor(&current, old_str).is_err());
            }
            // Anchorless op (whole-body rewrite): drift only if the update
            // can't be applied to the base at all.
            Ok(!doc::applies_cleanly(&base, doc_id, &op.yrs_update))
        })
    }

    /// Resolve a vault-relative path to its doc_id via `doc-index.db`.
    ///
    /// status: op-log-store-layout
    pub fn doc_id_for_path(&self, path: &str) -> Result<Option<String>, Error> {
        self.locked(|inner| meta::doc_id_for_path(&inner.index, path))
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

    /// The current vault-relative path for a doc_id. Reads the loaded Doc's
    /// `meta.path` (authoritative — a rename updates it in place), falling
    /// back to the `doc-index.db` mapping when the Doc isn't loadable.
    /// `None` for an unknown doc_id. The changes/activity projection uses
    /// this to resolve a side-table row's `doc_id` back to a path.
    ///
    /// status: op-log-store-layout
    pub fn path_for_doc(&self, doc_id: &str) -> Result<Option<String>, Error> {
        self.locked(|inner| {
            if let Ok(state) = Self::ensure_loaded(&self.oplog_dir, inner, doc_id) {
                return Ok(doc::meta_string(&state.accepted, "path"));
            }
            meta::path_for_doc_id(&inner.index, doc_id)
        })
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
        self.locked(|inner| meta::gc_status(&inner.meta, status, cutoff_ms))
    }

    // ── internals ──────────────────────────────────────────────────────

    /// The single accepted-text commit path. Resolves `input` to minimal
    /// localized spans, applies them to `accepted` in one Yrs transaction (so
    /// the whole edit is one contiguous clock range = one logical op), then
    /// persists in the spec-mandated order — `.yrs`, side-table row, history
    /// frame, `.md` — **all under one lock hold**, so a concurrent writer
    /// (another save, an accept, a future sync receive) can't interleave
    /// between the in-memory mutation and the disk writes. Returns `false`
    /// when the edit is empty (a no-op save / self-write echo).
    ///
    /// status: op-log-atomic-write
    /// status: op-log-disk-canonical
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
            let (materialized, client_id, lo, hi, op_kind, rel_path) = {
                let state = Self::ensure_loaded(&self.oplog_dir, inner, doc_id)?;
                let base_mat = doc::materialize(&state.accepted);
                let base = base_mat.text;
                // A content write to a tombstoned doc resurrects it: this is a
                // re-create at a path whose previous document was deleted (the
                // `path → doc_id` mapping is kept per `tombstone_document`), so
                // the resolved doc still reads as tombstoned. Without clearing
                // it, `write_md_file` would suppress the new file and the save
                // would silently vanish.
                let resurrecting = base_mat.tombstone;
                let spans = match input {
                    EditInput::Spans(spans) => spans.to_vec(),
                    EditInput::FullText(new_text) => doc::multi_span_delta(&base, new_text),
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
                let cid = state.accepted.client_id();
                let lo = doc::state_clock(&state.accepted, cid);
                let before_sv = doc::state_vector(&state.accepted);
                doc::apply_replaces(&state.accepted, &spans);
                if resurrecting {
                    doc::clear_tombstone(&state.accepted);
                }
                let hi = doc::state_clock(&state.accepted, cid);
                // Mirror the just-applied ops onto the working overlay (if any)
                // so the user's uncommitted edits stay layered on top of the
                // new accepted state. Without this an external edit advances
                // `accepted` but not `working`, so the buffer wouldn't show it
                // and the next commit would diff it away. Anchored replay →
                // disjoint regions merge cleanly. (On a user commit, `working`
                // is cleared right after, so the replay there is harmless.)
                if let Some(working) = &state.working {
                    let update = doc::encode_since(&state.accepted, &before_sv);
                    let _ = doc::apply_update(working, doc_id, &update);
                }
                let rel_path = doc::meta_string(&state.accepted, "path");
                let materialized = doc::materialize(&state.accepted);
                // Append the Yrs delta for this commit (O(edit), not O(doc)),
                // then a history frame (keyframe or delta) for the op.
                Self::persist_accepted(&self.oplog_dir, doc_id, state)?;
                Self::retain_frame(
                    &self.oplog_dir, doc_id, state, op_id.clone(),
                    &materialized.text, materialized.tombstone, now,
                )?;
                (materialized, cid.get() as i64, lo, hi, op_kind, rel_path)
            };
            meta::insert_metadata(
                &inner.meta,
                &meta::MetadataInsert {
                    doc_id,
                    op_id: &op_id,
                    yrs_client_id: client_id,
                    yrs_clock_lo: lo,
                    yrs_clock_hi: hi,
                    author,
                    op_kind: &op_kind,
                    status: OpStatus::Accepted,
                    timestamp_ms: now,
                    content_hash: Some(&content_hash(&materialized.text)),
                    surface,
                    session_id: None,
                    batch_id: None,
                    metadata: &serde_json::Value::Null,
                },
            )?;
            write_md_file(&self.oplog_dir, rel_path.as_deref(), &materialized)?;
            Ok(true)
        })
    }

    /// Load a document's `accepted` Doc + pending queue from disk into the
    /// cache if not already present, returning a mutable handle.
    fn ensure_loaded<'a>(
        oplog_dir: &Path,
        inner: &'a mut Inner,
        doc_id: &str,
    ) -> Result<&'a mut DocState, Error> {
        if !inner.docs.contains_key(doc_id) {
            let bytes = store::load_yrs(oplog_dir, doc_id)?
                .ok_or_else(|| Error::UnknownDoc(doc_id.to_string()))?;
            let accepted = doc::load_doc(doc_id, &bytes)?;
            // Replay incremental deltas appended since the `.yrs` base was
            // written. Each is a v2 update; `apply_update` is idempotent, so a
            // delta already folded into the base (e.g. a crash between
            // compaction's base rewrite and log clear) is a harmless no-op.
            for frame in store::load_yrslog(oplog_dir, doc_id)? {
                let _ = doc::apply_update(&accepted, doc_id, &frame);
            }
            let persisted_sv = doc::state_vector(&accepted);
            let pending = store::load_pending(oplog_dir, doc_id)?;
            inner.docs.insert(
                doc_id.to_string(),
                DocState {
                    accepted,
                    working: None,
                    pending,
                    persisted_sv,
                    // `None` forces the first history frame after open to be a
                    // keyframe, re-anchoring the delta chain without reading
                    // `.ops` here.
                    last_retained_text: None,
                    deltas_since_keyframe: 0,
                },
            );
        }
        inner
            .docs
            .get_mut(doc_id)
            .ok_or_else(|| Error::UnknownDoc(doc_id.to_string()))
    }

    /// Persist the ops `accepted` gained since the last save by appending a
    /// single delta frame to `<doc-id>.yrslog`, then advance `persisted_sv`.
    /// This replaces a full `encode_full` + atomic rewrite of `.yrs` on every
    /// commit — the save is now O(edit), and the full base is only rewritten on
    /// compaction. A no-op delta (nothing changed since the last persist) is
    /// skipped so idempotent commits don't grow the log. Per
    /// `op-log-yrs-delta-log`.
    fn persist_accepted(oplog_dir: &Path, doc_id: &str, state: &mut DocState) -> Result<(), Error> {
        let current_sv = doc::state_vector(&state.accepted);
        if current_sv == state.persisted_sv {
            return Ok(());
        }
        let delta = doc::encode_since(&state.accepted, &state.persisted_sv);
        store::append_yrslog(oplog_dir, doc_id, &delta)?;
        state.persisted_sv = current_sv;
        Ok(())
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
