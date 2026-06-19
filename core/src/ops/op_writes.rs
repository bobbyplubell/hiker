//! The producer-facing seam over the layered editing model. Every write path
//! routes through this module: user saves apply to `accepted`
//! and ride the atomic-write path, agent edits queue as pending ops,
//! and per-op accept/reject flips a pending op's status. Producers (the app
//! buffer-save command, the MCP write tools, the cluster/triage automations)
//! call these helpers and never reach into the substrate themselves — keeping
//! the rusqlite dependency confined to the substrate crate and the
//! orchestration policy (path → doc_id resolution, author class, surface
//! naming) here.
//!
//! Module placement follows `op-log.md`'s "Module placement": `core::ops`
//! wraps the substrate with the higher-level write paths; the substrate owns
//! the text store and side table. Helpers return plain [`HikerError`] so adapters
//! match per-variant the same way they do for every other `core::ops` verb.
//
// status: op-log-ops-producer-helpers
// status: op-log-doc-id-bootstrap

use std::sync::Arc;

use crate::errors::HikerError;
use crate::editing::{shapes::Author, error::Error as SubstrateError, EditSpec, LayeredDoc, ProducerCtx, StageOutcome};
use crate::vault::Vault;

/// Translate a substrate error into the vault-wide [`HikerError`] so
/// producers never see the substrate's error type. The anchor / unknown-doc
/// cases map to `NotFound`; everything else is an I/O shaped failure as far
/// as the caller is concerned.
fn map_err(e: SubstrateError) -> HikerError {
    use SubstrateError as E;
    match e {
        E::UnknownDoc(d) => HikerError::NotFound(format!("layered doc {d}")),
        E::UnknownPath(p) => HikerError::NotFound(format!("layered-doc path {p}")),
        E::UnknownPendingOp(op) => HikerError::NotFound(format!("layered-doc pending op {op}")),
        E::Anchor(msg) => HikerError::NotFound(format!("layered-doc anchor: {msg}")),
        other => HikerError::Io(other.to_string()),
    }
}

/// The layered document `kind` for a vault-relative path. Native vault
/// markdown is `"markdown"`; a `*.<ext>.md` next to a non-md source is a
/// `"sidecar"` per `design.md`'s storage-mode table; a `.canvas` file is a
/// `"canvas"` JSON Canvas document. Under path-identity the kind is derived
/// from the path extension for the re-extraction / lifecycle surfaces that
/// read it later; bootstrap and create both resolve it the same way.
//
// status: canvas-doc-kind
fn kind_for(rel: &str) -> &'static str {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    // A `.canvas` file is a first-class JSON Canvas layered document — its
    // JSON text rides the layered editing model exactly like a note, under the `canvas` kind.
    if name.ends_with(".canvas") {
        return "canvas";
    }
    // A sidecar is `<full-source-filename>.md` — i.e. a `.md` whose stem
    // still carries a source extension (`diagram.png.md`, `contract.pdf.md`).
    let stem = name.strip_suffix(".md").unwrap_or(name);
    if stem.contains('.') {
        "sidecar"
    } else {
        "markdown"
    }
}

/// Seed the layered editing model from the on-disk vault. For every existing
/// indexable document (`.md` notes and sidecars) not already mapped into the
/// model this session, seed the document from the file's current bytes
/// authored as `user` (the path IS the doc id under path-identity).
/// Returns the number of documents freshly seeded.
///
/// Idempotent: a path already mapped this session is skipped, so a
/// second open is a no-op walk. The on-disk `.md` already equals
/// `materialize(accepted)` by construction, so seeding goes through
/// [`LayeredDoc::seed_document`], which verifies the bytes against disk instead of
/// rewriting the user's file — a first open never touches any note's mtime.
///
/// status: op-log-doc-id-bootstrap
pub fn bootstrap(vault: &Vault, log: &LayeredDoc) -> Result<usize, HikerError> {
    let mut seeded = 0usize;
    // Main pass: the user-visible vault. `walk_indexable_files` prunes at
    // `.hiker/` (the watcher-ignore rule applies in filter_entry), so the
    // hidden carve-outs are picked up in the second pass below.
    for rel in vault.walk_indexable_files("")? {
        seeded += seed_one(vault, log, &rel)? as usize;
    }
    // Second pass: `.hiker/trails/` carve-out — trail-docs at
    // `.hiker/trails/drafts/` and waypoint-notes at
    // `.hiker/trails/<id>/waypoints/`. Pre-existing waypoint files arriving
    // against an existing vault need layered-doc
    // `doc_id`s exactly like vault-root notes so trail integrity holds
    // without waiting for an individual ingest event. status: op-log-doc-id-bootstrap
    for rel in walk_hidden_md_subtree(vault, &crate::trails::dir())? {
        seeded += seed_one(vault, log, &rel)? as usize;
    }
    // Cluster-tree docs live at a *visible* vault path
    // (`cluster-tree-visible-note`, default `cluster-trees/`), so the main
    // `walk_indexable_files` pass above already seeds them like any other
    // note — no separate cluster-tree pass is needed.
    Ok(seeded)
}

/// Seed one path into the layered editing model if it isn't already mapped.
/// Returns `true` when a new doc was created, `false` when the path was
/// skipped (already mapped this session, or unreadable now). Read
/// failures log but never abort the caller —
/// matching the original bootstrap loop's posture.
fn seed_one(vault: &Vault, log: &LayeredDoc, rel: &str) -> Result<bool, HikerError> {
    // Idempotency key is in-memory registration, not on-disk presence: under
    // the disk-canonical model every existing `.md` "exists" as a doc, so the
    // skip condition is "already loaded this session" (`op-log-doc-id-bootstrap`).
    if log.is_loaded(rel) {
        return Ok(false);
    }
    let text = match vault.read_file(rel) {
        Ok(t) => t,
        Err(e) => {
            // An unreadable note (non-UTF-8, permission error) is skipped for
            // this open; the bootstrap-skip marker rode the deleted `op_history`
            // index, so a future open simply re-reads and re-skips (cheap, rare).
            tracing::warn!(path = %rel, error = %e, "layered-doc bootstrap: skipping unreadable note");
            return Ok(false);
        }
    };
    // Seed, don't write: the file already exists on disk holding exactly
    // `text`, so we register the doc and verify the bytes match rather than
    // rewriting the file over itself (which would churn every note's mtime on
    // first open). status: op-log-disk-canonical
    log.seed_document(rel, kind_for(rel), &text, &Author::User)
        .map_err(map_err)?;
    Ok(true)
}

/// Walk a hidden vault subtree (e.g. `.hiker/trails`) returning every `.md`
/// file as a vault-relative path. Used by [`bootstrap`] to reach files the
/// main [`Vault::walk_indexable_files`] pass prunes at `.hiker/`. Symlinks
/// are not followed, mirroring the main walker's policy.
pub(crate) fn walk_hidden_md_subtree(
    vault: &Vault,
    rel_subtree: &str,
) -> Result<Vec<String>, HikerError> {
    let abs = vault.root().join(rel_subtree);
    if !abs.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(&abs).follow_links(false) {
        let entry = entry.map_err(|e| HikerError::Io(e.to_string()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let Ok(rel_to_vault) = path.strip_prefix(vault.root()) else {
            continue;
        };
        let rel_str = rel_to_vault.to_string_lossy().replace('\\', "/");
        if !rel_str.ends_with(".md") {
            continue;
        }
        out.push(rel_str);
    }
    Ok(out)
}

/// Resolve a vault-relative path to its doc_id, seeding a fresh document from
/// the path's current bytes (or `initial_text` when the file does not yet
/// exist on disk) if no mapping exists. Used by the write paths so a note
/// that was created after the bootstrap walk — or never seen by it — still
/// gets a doc before its first op is recorded.
pub(crate) fn doc_id_or_seed(
    log: &LayeredDoc,
    vault: &Vault,
    rel: &str,
    initial_text: &str,
) -> Result<String, HikerError> {
    if let Some(id) = log.doc_id_for_path(rel).map_err(map_err)? {
        return Ok(id);
    }
    let seed = vault.read_file(rel).unwrap_or_else(|_| initial_text.to_string());
    log.create_document(rel, kind_for(rel), &seed, &Author::User)
        .map_err(map_err)
}

/// Ensure a layered document exists for `rel`, seeding one from the file's
/// current bytes when none is registered yet — a note created after the
/// bootstrap walk (the New Note button, the tree's new-file verb, the
/// wikilink "create missing note" jump). Returns the doc_id. Idempotent: a
/// no-op returning the existing id when the path already has a document.
///
/// Callers seed at *open* time so the live editor binding engages from the
/// first keystroke (it bails on a doc-less buffer), and so the layered save
/// (`commit_working`) has a doc to commit onto. Seeding at save time alone is
/// too late — the user's typing would never have reached the `working` layer.
///
/// status: op-log-ops-producer-helpers
pub fn ensure_doc(log: &LayeredDoc, vault: &Vault, rel: &str) -> Result<String, HikerError> {
    doc_id_or_seed(log, vault, rel, "")
}

/// Route a user save through the layered editing model: resolve `rel` to its
/// doc_id (seeding one if necessary), then commit the buffer's full text as a
/// `user` edit on `accepted`. The model diffs `new_text` against the current
/// accepted state into minimal localized spans, so a save lands as a text edit
/// over only the bytes that actually changed — never a whole-document rewrite.
/// It atomically writes the materialized `.md` (the
/// `op-log-atomic-write` / `op-log-disk-canonical` path), so the caller does
/// **not** also write the file itself. A save that changes nothing is a no-op.
///
/// status: op-log-ops-producer-helpers
pub fn user_save(log: &LayeredDoc, vault: &Vault, rel: &str, new_text: &str) -> Result<(), HikerError> {
    let doc_id = doc_id_or_seed(log, vault, rel, "")?;
    log.apply_user_text(&doc_id, new_text).map_err(map_err)?;
    Ok(())
}

/// The outcome of a re-extraction routed through [`reextract`].
///
/// In-process extraction has been removed: hiker does **zero** content
/// extraction. All extraction/crawl/retrieval lives in a separate producer tool
/// (working name *trailblazer*) that emits a manifest hiker imports (see
/// `docs/import.md`). [`reextract`] is therefore a no-op stub that always
/// reports `Skipped`; the enum is retained only so any residual caller still
/// compiles. status: manifest-only-ingest
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReextractOutcome {
    /// A re-extraction landed a new version. No longer produced — hiker does no
    /// in-process extraction — retained only for the enum's stable shape.
    Replaced,
    /// A re-extraction produced identical content. No longer produced.
    Unchanged,
    /// Re-extraction did nothing. The only outcome [`reextract`] ever returns:
    /// hiker performs no in-process extraction, so there is nothing to re-run.
    Skipped,
}

/// No-op re-extraction stub. In-process extraction was removed under the
/// manifest-only ingest decision (`hiker-core-rework-plan.md` WS6): hiker does
/// no content extraction at all — an external producer (*trailblazer*) emits a
/// manifest hiker imports, and re-importing a changed source is an import-path
/// concern, not an in-process re-extraction. This always returns
/// [`ReextractOutcome::Skipped`] and touches nothing.
///
/// status: manifest-only-ingest
#[allow(clippy::unnecessary_wraps)]
pub const fn reextract(
    _log: &LayeredDoc,
    _vault: &Vault,
    _rel: &str,
    _new_body: &str,
    _extractor_id: &str,
) -> Result<ReextractOutcome, HikerError> {
    Ok(ReextractOutcome::Skipped)
}

/// One anchored or whole-body edit handed to [`stage_agent_edits`]. Mirrors
/// the MCP `edit_note` shape: `old_str = Some` is an anchored replace,
/// `old_str = None` is a whole-body rewrite (`write_note`).
#[derive(Debug, Clone)]
pub struct AgentEdit {
    pub old_str: Option<String>,
    pub new_str: String,
}

/// Stage a batch of agent edits as pending ops against the document at `rel`.
/// Resolves (or seeds) the doc_id, then queues each edit via
/// [`LayeredDoc::stage_pending`] tagged `agent:<client_id>`. The ops do not reach
/// disk until accepted; the returned op ids let the caller surface them for
/// review and later flip each via [`flip_op_status`].
///
/// status: op-log-ops-producer-helpers
pub fn stage_agent_edits(
    log: &LayeredDoc,
    vault: &Vault,
    client_id: &str,
    surface: &str,
    rel: &str,
    edits: &[AgentEdit],
) -> Result<StageOutcome, HikerError> {
    let doc_id = doc_id_or_seed(log, vault, rel, "")?;
    let specs: Vec<EditSpec> = edits
        .iter()
        .map(|e| EditSpec {
            old_str: e.old_str.clone(),
            new_str: e.new_str.clone(),
        })
        .collect();
    let ctx = ProducerCtx {
        author: Author::Agent(client_id.to_string()),
        surface: surface.to_string(),
        session_id: Some(client_id.to_string()),
    };
    log.stage_pending(&doc_id, &specs, &ctx).map_err(map_err)
}

/// One move in a [`stage_reorg_batch`] call: the note's current vault-
/// relative path and the path it should move to. The basename-and-folder
/// math (computing `to` from a target folder) stays the producer's concern;
/// this seam takes the resolved destination path.
#[derive(Debug, Clone)]
pub struct ReorgMove {
    pub from: String,
    pub to: String,
}

/// Stage a multi-file reorganization as a batch of pending `Rename` ops
/// sharing one cross-document `batch_id` (`op-log-reorg-batch`). Each
/// [`ReorgMove`] resolves (or seeds) its source path's doc_id and queues one
/// pending `Rename { from }` op tagged `auto:<producer>` (e.g. `auto:cluster`
/// for the cluster-apply flow, `auto:triage` for the saved-tree classifier).
/// Nothing reaches disk until the batch is accepted via [`flip_batch_status`];
/// the batch is a review/display grouping, not a transaction (partial apply
/// allowed). No-op moves (`from == to`) and unmapped sources are skipped.
///
/// status: op-log-reorg-batch
pub fn stage_reorg_batch(
    log: &LayeredDoc,
    vault: &Vault,
    producer: &str,
    surface: &str,
    moves: &[ReorgMove],
) -> Result<StageOutcome, HikerError> {
    let mut renames: Vec<(String, String)> = Vec::with_capacity(moves.len());
    for mv in moves {
        if mv.from == mv.to {
            continue;
        }
        let doc_id = doc_id_or_seed(log, vault, &mv.from, "")?;
        renames.push((doc_id, mv.to.clone()));
    }
    let ctx = ProducerCtx {
        author: Author::Auto(producer.to_string()),
        surface: surface.to_string(),
        session_id: None,
    };
    log.stage_pending_renames(&renames, &ctx).map_err(map_err)
}

/// Stage a single pending content edit at `rel` from a *whole new document
/// text*, tagged `auto:<producer>`. The layered editing model diffs the new text against the
/// current accepted state and queues one pending op (labeled `SetFrontmatter`
/// when the change lands in the frontmatter fence — the cluster-editor tag
/// shape — else `Replace`). Returns the minted batch id + op ids; the batch
/// id flips through [`flip_batch_status`] or the per-op [`flip_op_status`].
///
/// status: op-log-reorg-batch
pub fn stage_auto_content(
    log: &LayeredDoc,
    vault: &Vault,
    producer: &str,
    surface: &str,
    rel: &str,
    new_text: &str,
) -> Result<StageOutcome, HikerError> {
    let doc_id = doc_id_or_seed(log, vault, rel, "")?;
    let ctx = ProducerCtx {
        author: Author::Auto(producer.to_string()),
        surface: surface.to_string(),
        session_id: None,
    };
    log.stage_pending_content(&doc_id, new_text, &ctx)
        .map_err(map_err)
}

/// One whole-document text in a [`stage_auto_content_batch`] call: the
/// vault-relative path and the full new file the producer computed.
#[derive(Debug, Clone)]
pub struct ContentStage {
    pub rel: String,
    pub new_text: String,
}

/// One automation firing's in-flight overlay of computed whole-document
/// texts. A multi-step producer (a rules firing applying several actions)
/// writes each step's output here instead of committing per step: later
/// steps read earlier steps' output through [`Draft::read`], and the
/// producer stages the collected texts as ONE cross-document batch via
/// [`stage_auto_content_batch`] — the one-batch-per-firing property. The
/// board write ops land into this overlay in their `AutoStaged` mode
/// (`boards::ops::BoardWriteMode`), so automation and the direct user
/// path share one mutation body and only the landing differs.
///
/// status: rule-attribution
/// status: rule-closed-verbs
#[derive(Default)]
pub struct Draft {
    texts: std::collections::BTreeMap<String, String>,
    /// First-touch order, so the staged batch is deterministic.
    order: Vec<String>,
}

impl Draft {
    /// Read `rel` through the overlay: the draft's computed text when one
    /// was already produced this firing, else the current disk bytes.
    pub fn read(&self, vault: &Vault, rel: &str) -> Result<String, HikerError> {
        if let Some(text) = self.texts.get(rel) {
            return Ok(text.clone());
        }
        vault.read_file(rel)
    }

    /// Record `rel`'s new whole-document text. The first touch fixes the
    /// path's position in the staged-batch order.
    pub fn put(&mut self, rel: &str, text: String) {
        if !self.texts.contains_key(rel) {
            self.order.push(rel.to_string());
        }
        self.texts.insert(rel.to_string(), text);
    }

    /// Whether the draft already holds a text for `rel`.
    #[must_use]
    pub fn contains(&self, rel: &str) -> bool {
        self.texts.contains_key(rel)
    }

    /// The drafted paths in first-touch order.
    #[must_use]
    pub fn paths(&self) -> &[String] {
        &self.order
    }

    /// The drafted texts as [`ContentStage`] items, in first-touch order —
    /// the exact shape [`stage_auto_content_batch`] takes.
    #[must_use]
    pub fn stages(&self) -> Vec<ContentStage> {
        self.order
            .iter()
            .map(|rel| ContentStage {
                rel: rel.clone(),
                new_text: self.texts[rel].clone(),
            })
            .collect()
    }
}

/// Stage several whole-document texts as pending content ops sharing ONE
/// cross-document batch id — the multi-doc sibling of [`stage_auto_content`]
/// (which mints a batch per call), in the `op-log-reorg-batch` shape. Built
/// for `sprint-rollover`'s close batch: N board-doc rewrites authored
/// `auto:<producer>` (`auto:sprint-close`), reviewed and flipped as one unit
/// through [`flip_batch_status`] and the standard staging surfaces, with the
/// usual per-item partial-apply semantics on accept. Docs whose new text
/// equals the current accepted state stage nothing.
///
/// status: sprint-rollover
/// status: op-log-reorg-batch
pub fn stage_auto_content_batch(
    log: &LayeredDoc,
    vault: &Vault,
    producer: &str,
    surface: &str,
    items: &[ContentStage],
) -> Result<StageOutcome, HikerError> {
    let mut resolved: Vec<(String, String)> = Vec::with_capacity(items.len());
    for item in items {
        let doc_id = doc_id_or_seed(log, vault, &item.rel, "")?;
        resolved.push((doc_id, item.new_text.clone()));
    }
    let ctx = ProducerCtx {
        author: Author::Auto(producer.to_string()),
        surface: surface.to_string(),
        session_id: None,
    };
    log.stage_pending_contents(&resolved, &ctx).map_err(map_err)
}

/// Accept or reject every pending op in a reorg `batch_id` (`op-log-reorg-batch`).
/// Accept applies each contributing `Rename` independently, skipping any that
/// fail (a target collision on one move does not block the rest — partial
/// apply); reject drops the whole batch from the queue. `accept = true`
/// accepts; `false` rejects. Returns the op ids that were applied / rejected.
///
/// status: op-log-reorg-batch
pub fn flip_batch_status(
    log: &LayeredDoc,
    batch_id: &str,
    accept: bool,
) -> Result<Vec<String>, HikerError> {
    if accept {
        log.accept_batch(batch_id).map_err(map_err)
    } else {
        log.reject_batch(batch_id).map_err(map_err)
    }
}

/// Registry + store + vault handles for the apply-time invariant re-check
/// at the flip seam. Layering decided with `derived-status-rule`'s
/// apply-time fix: the layered editing model stays pure — it knows nothing of
/// kinds, boards, or membership — so the check lives HERE in the ops
/// layer, where producers already hold these handles, as a wrapper over
/// the raw flip primitives. Surfaces that can accept a board-doc content
/// op (the review tabs, the in-buffer hunk verbs, the rules / sprint-close
/// auto-flips) flip through [`flip_op_status_checked`] /
/// [`flip_batch_status_checked`]; producers whose ops can never add a
/// board card (triage renames, agent note edits without a store handle)
/// stay on the raw primitives.
pub struct FlipCtx<'a> {
    pub vault: &'a Vault,
    pub store: &'a crate::store::Store,
    pub kinds: &'a crate::kinds::Registry,
}

/// [`flip_batch_status`] with the apply-time one-sprint re-check
/// (`derived-status-rule`): before accepting, every op in the batch that
/// adds a note card to a sprint-kind board is re-verified against the
/// accepted state at THIS moment (`pm::verify_flip_single_sprint`) — the
/// stage-time check can be hours stale under review mode. A violation
/// refuses the whole flip with the typed [`HikerError::SprintConflict`],
/// leaving the batch pending. Rejects are never checked.
///
/// status: derived-status-rule
pub fn flip_batch_status_checked(
    log: &LayeredDoc,
    ctx: &FlipCtx<'_>,
    batch_id: &str,
    accept: bool,
) -> Result<Vec<String>, HikerError> {
    if accept {
        let ops = log.pending_ops_in_batch(batch_id).map_err(map_err)?;
        crate::pm::verify_flip_single_sprint(log, ctx.vault, ctx.store, ctx.kinds, &ops)?;
    }
    flip_batch_status(log, batch_id, accept)
}

/// [`flip_op_status`] with the apply-time one-sprint re-check — the per-op
/// sibling of [`flip_batch_status_checked`]. Each op is verified alone
/// (accepted state + just that op), so accepting half a multi-doc batch
/// (the destination board of a sprint close without its closing half) is
/// refused exactly when it would land a note on two sprints.
///
/// status: derived-status-rule
pub fn flip_op_status_checked(
    log: &LayeredDoc,
    ctx: &FlipCtx<'_>,
    rel: &str,
    op_ids: &[String],
    accept: bool,
) -> Result<(), HikerError> {
    if accept {
        let doc_id = log
            .doc_id_for_path(rel)
            .map_err(map_err)?
            .ok_or_else(|| HikerError::NotFound(format!("op-log path {rel}")))?;
        let ops: Vec<(String, String)> = op_ids
            .iter()
            .map(|op_id| (doc_id.clone(), op_id.clone()))
            .collect();
        crate::pm::verify_flip_single_sprint(log, ctx.vault, ctx.store, ctx.kinds, &ops)?;
    }
    flip_op_status(log, rel, op_ids, accept)
}

/// Accept or reject pending ops by id. The single per-op primitive both the
/// per-hunk verbs and the patch-review bulk actions ride on: accept applies
/// the op's text edit to `accepted` (and atomically rewrites the `.md`),
/// reject writes a rejected audit row and drops the op. `accept = true`
/// accepts; `false` rejects. Resolves the doc_id from `rel`.
///
/// status: op-log-ops-producer-helpers
pub fn flip_op_status(
    log: &LayeredDoc,
    rel: &str,
    op_ids: &[String],
    accept: bool,
) -> Result<(), HikerError> {
    let doc_id = log
        .doc_id_for_path(rel)
        .map_err(map_err)?
        .ok_or_else(|| HikerError::NotFound(format!("op-log path {rel}")))?;
    for op_id in op_ids {
        if accept {
            log.accept_pending(&doc_id, op_id).map_err(map_err)?;
        } else {
            log.reject_pending(&doc_id, op_id).map_err(map_err)?;
        }
    }
    Ok(())
}

/// Resolve a patch-review hunk to the pending op ids contributing to it.
/// The hunk's `current_range` (a byte range in the pending-view
/// materialization the buffer renders) is matched against each pending op's
/// affected range per `op-log-per-hunk-accept-reject`. The caller then flips
/// the returned ids via [`flip_op_status`]. Scoped to `session` when set
/// (the active agent session in the file pill). Empty when the path has no
/// doc or no overlapping ops.
///
/// status: op-log-per-hunk-accept-reject
pub fn ops_in_hunk(
    log: &LayeredDoc,
    rel: &str,
    session: Option<&str>,
    start: usize,
    end: usize,
) -> Result<Vec<String>, HikerError> {
    let Some(doc_id) = log.doc_id_for_path(rel).map_err(map_err)? else {
        return Ok(Vec::new());
    };
    log.ops_in_range(&doc_id, session, start, end)
        .map_err(map_err)
}

/// Materialize the document at `rel` in its accepted state (= what's on disk)
/// and its pending-view state (accepted + the session's queued pending ops).
/// Returns `(accepted_text, pending_view_text)` — the two ropes the inline
/// patch-review `DiffLayer` diffs (`op-log-hunk-view`). Per `op-log.md`'s
/// module placement the app may also read these straight off the `LayeredDoc`
/// handle; this seam keeps the path→doc_id resolution in `core::ops` for
/// callers that only hold a path.
///
/// status: op-log-hunk-view
pub fn review_materializations(
    log: &LayeredDoc,
    rel: &str,
    session: Option<&str>,
) -> Result<Option<(String, String)>, HikerError> {
    let Some(doc_id) = log.doc_id_for_path(rel).map_err(map_err)? else {
        return Ok(None);
    };
    let accepted = log.materialize_accepted(&doc_id).map_err(map_err)?;
    let pending = log
        .materialize_pending_view(&doc_id, session)
        .map_err(map_err)?;
    Ok(Some((accepted.text, pending.text)))
}

/// One pending whole-file proposal surfaced to the buffer review surface
/// (`patch-review.md` `write-note-review-surface`). A whole-file proposal is
/// a pending op whose shape replaces the entire document body or creates a
/// new note — the `write_note` MCP-call shape — as opposed to the anchored
/// `edit_note` `Replace` ops that compose into the inline per-hunk view.
///
/// status: write-note-review-surface
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WholeFileProposal {
    /// The pending op id — the handle Accept / Reject flip through
    /// [`flip_op_status`], and the key the preview tab is keyed on.
    pub op_id: String,
    /// The layered document id the op lives on.
    pub doc_id: String,
    /// Vault-relative path the proposal targets.
    pub target_path: String,
    /// Coarse action label for the dropdown / banner row: `"write_note"`
    /// for a whole-body rewrite, `"create"` for a new-note proposal.
    pub action: &'static str,
    /// Producer surface (`"mcp-tool-call"`, …) — shown in the listing.
    pub surface: String,
    /// Submit time in unix milliseconds; the review surface opens the most
    /// recent op when a path has several.
    pub created_at_ms: i64,
    /// Whether the op has drifted against the current `accepted` — Accept is
    /// disabled for drifted proposals per `write-note-review-conflicted-display`.
    pub drifted: bool,
}

/// Whether a pending op is a *whole-file* proposal — the shape the whole-file
/// review surface owns, as opposed to anchored `edit_note` hunks. A whole-body
/// rewrite is an anchorless `Replace`; a new-note proposal is a `Create`.
/// Frontmatter patches (`SetFrontmatter`), anchored `Replace`, `Rename`, and
/// `Tombstone` are *not* whole-file — they ride the inline / confirm surfaces.
const fn whole_file_action(op_kind: &crate::editing::shapes::OpKind) -> Option<&'static str> {
    use crate::editing::shapes::OpKind;
    match op_kind {
        OpKind::Replace { anchor: None } => Some("write_note"),
        OpKind::Create => Some("create"),
        OpKind::Replace { anchor: Some(_) }
        | OpKind::SetFrontmatter
        | OpKind::Rename { .. }
        | OpKind::Tombstone => None,
    }
}

/// List every pending whole-file proposal across the vault, resolved to its
/// target path. Walks `LayeredDoc::all_pending_ops`, keeps only the whole-file
/// shapes (per [`whole_file_action`]), and resolves each op's `doc_id` to a
/// vault-relative path. Feeds the buffer review surface (version dropdown,
/// pending-rewrite banner, agent-diff toggle). Sorted newest-first so the
/// review surface opens the most recent proposal for a path by default
/// (`note-open-routes-to-pending-review`).
///
/// status: write-note-review-surface
pub fn list_whole_file_proposals(log: &LayeredDoc) -> Result<Vec<WholeFileProposal>, HikerError> {
    let pending = log.all_pending_ops().map_err(map_err)?;
    let mut out = Vec::new();
    for (doc_id, op) in pending {
        let Some(action) = whole_file_action(&op.op_kind) else {
            continue;
        };
        // Resolve doc_id → path; skip ops whose document has no path mapping
        // (a never-pathed doc can't surface a review tab anyway).
        let Some(target_path) = log.path_for_doc(&doc_id).map_err(map_err)? else {
            continue;
        };
        let drifted = log
            .is_pending_drifted(&doc_id, &op.op_id)
            .map_err(map_err)?;
        out.push(WholeFileProposal {
            op_id: op.op_id,
            doc_id,
            target_path,
            action,
            surface: op.surface,
            created_at_ms: op.created_at_ms,
            drifted,
        });
    }
    out.sort_by_key(|b| std::cmp::Reverse(b.created_at_ms));
    Ok(out)
}

/// One pending agent note-edit op surfaced to the cross-vault review tab
/// (`patch-review.md`'s `PatchReview` tab) and counted for the toolbar /
/// status-bar pending badge. Unlike [`WholeFileProposal`] (whole-file shapes
/// only) this covers *every* pending op — anchored `edit_note` `Replace`s,
/// `SetFrontmatter` patches, whole-body rewrites, renames, creates, tombstones
/// — so the bulk surface lists the full pending queue.
///
/// status: write-note-review-surface
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingProposal {
    /// The pending op id — the handle Accept / Reject flip through
    /// [`flip_op_status`].
    pub op_id: String,
    /// Vault-relative path the op targets (resolved via `path_for_doc`).
    pub target_path: String,
    /// Producer surface (`"mcp-tool-call"`, `"triage"`, …).
    pub surface: String,
    /// Coarse op-kind label for the row (`"edit_note"`, `"write_note"`,
    /// `"set_frontmatter"`, `"create"`, `"rename"`, `"delete"`).
    pub action: &'static str,
    /// Submit time in unix milliseconds (drives the row's created stamp).
    pub created_at_ms: i64,
    /// Whether the op has drifted against current `accepted` — Accept is
    /// disabled for drifted proposals per `patch-review-conflicted-accept-disabled`.
    pub drifted: bool,
    /// The op's staging batch id. Ops sharing a batch id across documents
    /// (the `op-log-reorg-batch` / sprint-close shape) review as ONE unit:
    /// the review tab groups them into a single row flipped through
    /// [`flip_batch_status`], never per-doc.
    pub batch_id: Option<String>,
}

/// Coarse action label for a pending op's kind, used in the cross-vault
/// review tab's per-row header. Distinct from [`whole_file_action`], which
/// filters to *only* the whole-file shapes; this names every kind.
const fn action_label(op_kind: &crate::editing::shapes::OpKind) -> &'static str {
    use crate::editing::shapes::OpKind;
    match op_kind {
        OpKind::Replace { anchor: Some(_) } => "edit_note",
        OpKind::Replace { anchor: None } => "write_note",
        OpKind::SetFrontmatter => "set_frontmatter",
        OpKind::Rename { .. } => "rename",
        OpKind::Create => "create",
        OpKind::Tombstone => "delete",
    }
}

/// List every pending op across the vault, resolved to its target path and
/// drift status — the cross-vault `PatchReview` tab's feed. Walks
/// `LayeredDoc::all_pending_ops`, resolves each op's `doc_id` to a vault-relative
/// path (`path_for_doc`), and checks drift (`is_pending_drifted`). Sorted
/// newest-first so the tab opens with the most recent proposals on top. Ops
/// whose document has no path mapping are skipped (they can't surface a row).
///
/// status: write-note-review-surface
pub fn list_pending_proposals(log: &LayeredDoc) -> Result<Vec<PendingProposal>, HikerError> {
    let pending = log.all_pending_ops().map_err(map_err)?;
    let mut out = Vec::new();
    for (doc_id, op) in pending {
        let Some(target_path) = log.path_for_doc(&doc_id).map_err(map_err)? else {
            continue;
        };
        let drifted = log.is_pending_drifted(&doc_id, &op.op_id).map_err(map_err)?;
        out.push(PendingProposal {
            op_id: op.op_id,
            target_path,
            surface: op.surface,
            action: action_label(&op.op_kind),
            created_at_ms: op.created_at_ms,
            drifted,
            batch_id: op.batch_id,
        });
    }
    out.sort_by_key(|b| std::cmp::Reverse(b.created_at_ms));
    Ok(out)
}

/// The target paths of the OTHER still-pending ops sharing `op_id`'s batch
/// — the per-doc-accept guard's read: a nonempty result means accepting
/// just this op splits a multi-doc batch (the sprint-close shape), so the
/// caller warns and names what's left pending. Empty when the op has no
/// batch, the batch has no other pending members, or the op isn't pending.
///
/// status: op-log-reorg-batch
pub fn pending_batch_siblings(log: &LayeredDoc, op_id: &str) -> Result<Vec<String>, HikerError> {
    let pending = log.all_pending_ops().map_err(map_err)?;
    let Some(batch_id) = pending
        .iter()
        .find(|(_, op)| op.op_id == op_id)
        .and_then(|(_, op)| op.batch_id.clone())
    else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for (doc_id, op) in pending {
        if op.op_id != op_id
            && op.batch_id.as_deref() == Some(batch_id.as_str())
            && let Some(path) = log.path_for_doc(&doc_id).map_err(map_err)?
        {
            out.push(path);
        }
    }
    Ok(out)
}

/// The number of pending ops across the whole vault — the pending count the
/// toolbar / status-bar / Patch-review badge displays. Counts the
/// `<doc-id>.pending` queue contents directly, so a zero count means the
/// review surfaces have nothing to show.
///
/// status: write-note-review-surface
pub fn pending_op_count(log: &LayeredDoc) -> Result<usize, HikerError> {
    Ok(log.all_pending_ops().map_err(map_err)?.len())
}

/// The proposed content of a single pending whole-file op at `rel`, plus the
/// current accepted (= on-disk) content — the two ropes the whole-file review
/// surface's `DiffLayer` compares. Returns `(accepted_text, proposed_text)`.
/// `proposed_text` is `materialize(accepted + just this op)`; `accepted_text`
/// is `materialize(accepted)`. `Ok(None)` when the path has no doc. The layered-doc
/// preview path reads through here so the buffer never reaches into the
/// substrate's internal state itself.
///
/// status: write-note-review-surface
pub fn proposal_materializations(
    log: &LayeredDoc,
    rel: &str,
    op_id: &str,
) -> Result<Option<(String, String)>, HikerError> {
    let Some(doc_id) = log.doc_id_for_path(rel).map_err(map_err)? else {
        return Ok(None);
    };
    let accepted = log.materialize_accepted(&doc_id).map_err(map_err)?;
    let proposed = log
        .materialize_with_pending_op(&doc_id, op_id)
        .map_err(map_err)?;
    Ok(Some((accepted.text, proposed.text)))
}

/// One version in a note's plain-file snapshot history (`core::snapshot`).
/// The `snapshot_id` is the snapshot's millisecond timestamp rendered as a
/// string — a stable, restart-safe handle the version dropdown / diff
/// surfaces carry (it replaces the retired op-log `op_id`). `timestamp_ms`
/// is the same instant as an `i64` for display ordering.
///
/// status: plain-file-snapshots
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotVersion {
    pub snapshot_id: String,
    pub timestamp_ms: i64,
}

/// A path's version history, newest-first — the list behind the version
/// dropdown, per-file history, and the home version-browser. Sourced from
/// the plain-file snapshot tree under `.hiker/history/<rel>/` (the op-log
/// history engine is retired). Empty `Vec` when the note has no snapshots.
///
/// status: plain-file-snapshots
pub fn snapshot_history(
    log: &LayeredDoc,
    rel: &str,
    limit: usize,
) -> Result<Vec<SnapshotVersion>, HikerError> {
    let snaps = crate::snapshot::list_snapshots(log.vault_root(), rel)
        .map_err(|e| HikerError::Io(e.to_string()))?;
    Ok(snaps
        .into_iter()
        .take(limit)
        .map(|s| SnapshotVersion {
            snapshot_id: s.timestamp_ms.to_string(),
            timestamp_ms: s.timestamp_ms as i64,
        })
        .collect())
}

/// The content of `rel` at snapshot `snapshot_id` (the snapshot's
/// millisecond timestamp as a string) — the content behind a version-
/// dropdown preview / diff. `Ok(None)` when no snapshot with that id exists.
/// Read straight off the plain `.md` snapshot file, never the live doc.
///
/// status: plain-file-snapshots
pub fn content_at_snapshot(
    log: &LayeredDoc,
    rel: &str,
    snapshot_id: &str,
) -> Result<Option<String>, HikerError> {
    let snaps = crate::snapshot::list_snapshots(log.vault_root(), rel)
        .map_err(|e| HikerError::Io(e.to_string()))?;
    let Some(snap) = snaps
        .into_iter()
        .find(|s| s.timestamp_ms.to_string() == snapshot_id)
    else {
        return Ok(None);
    };
    crate::snapshot::read(&snap.path)
        .map(Some)
        .map_err(|e| HikerError::Io(e.to_string()))
}

/// The content of `rel` at the snapshot immediately *before* its newest one
/// — the "restore previous" rollback source. Returns `(prior_snapshot_id,
/// content)`, or `None` when the note has fewer than two snapshots. The
/// newest snapshot mirrors the current on-disk content, so index 1 is the
/// prior version.
///
/// status: plain-file-snapshots
pub fn previous_snapshot_content(
    log: &LayeredDoc,
    rel: &str,
) -> Result<Option<(String, String)>, HikerError> {
    let snaps = crate::snapshot::list_snapshots(log.vault_root(), rel)
        .map_err(|e| HikerError::Io(e.to_string()))?;
    let Some(prev) = snaps.get(1) else {
        return Ok(None);
    };
    crate::snapshot::read(&prev.path)
        .map(|content| Some((prev.timestamp_ms.to_string(), content)))
        .map_err(|e| HikerError::Io(e.to_string()))
}

/// Auto-reject any drifted pending ops on the document at `rel` when the
/// `[editing] auto_reject_on_drift` flag is set. A no-op when the flag is
/// `false` or the path has no doc. Returns the op ids that were rejected.
/// The caller passes the resolved config flag so this stays free of config
/// plumbing. Per `op-log.md`'s drift section, this flips drifted ops to
/// `rejected` immediately rather than surfacing them in the file pill.
///
/// status: op-log-status-states
pub fn auto_reject_drifted(
    log: &LayeredDoc,
    rel: &str,
    enabled: bool,
) -> Result<Vec<String>, HikerError> {
    if !enabled {
        return Ok(Vec::new());
    }
    let Some(doc_id) = log.doc_id_for_path(rel).map_err(map_err)? else {
        return Ok(Vec::new());
    };
    log.auto_reject_drifted(&doc_id).map_err(map_err)
}

/// Convenience alias for the `Arc<LayeredDoc>` handle producers thread around. The
/// app holds one per vault session; the agent write paths take
/// `Option<&LayeredDocHandle>` so call sites without a layered doc open (early
/// CLI, some tests) skip pending staging.
pub type LayeredDocHandle = Arc<LayeredDoc>;
