//! The producer-facing seam over the op-log substrate. Every write path
//! routes through this module: user saves apply to `accepted`
//! and ride the op-log atomic-write path, agent edits queue as pending ops,
//! and per-op accept/reject flips a pending op's status. Producers (the app
//! buffer-save command, the MCP write tools, the cluster/triage automations)
//! call these helpers and never reach into the substrate themselves — keeping
//! the Yrs / rusqlite dependency confined to the substrate crate and the
//! orchestration policy (path → doc_id resolution, author class, surface
//! naming) here.
//!
//! Module placement follows `op-log.md`'s "Module placement": `core::ops`
//! wraps the substrate with the higher-level write paths; the substrate owns
//! the CRDT and side table. Helpers return plain [`HikerError`] so adapters
//! match per-variant the same way they do for every other `core::ops` verb.
//
// status: op-log-ops-producer-helpers
// status: op-log-doc-id-bootstrap

use std::sync::Arc;

use crate::errors::HikerError;
use crate::oplog::{shapes::Author, error::Error as SubstrateError, EditSpec, OpLog, ProducerCtx, StageOutcome};
use crate::vault::Vault;

/// Translate a substrate error into the vault-wide [`HikerError`] so
/// producers never see the substrate's error type. The anchor / unknown-doc
/// cases map to `NotFound`; everything else is an I/O shaped failure as far
/// as the caller is concerned.
fn map_err(e: SubstrateError) -> HikerError {
    use SubstrateError as E;
    match e {
        E::UnknownDoc(d) => HikerError::NotFound(format!("op-log doc {d}")),
        E::UnknownPath(p) => HikerError::NotFound(format!("op-log path {p}")),
        E::UnknownPendingOp(op) => HikerError::NotFound(format!("op-log pending op {op}")),
        E::Anchor(msg) => HikerError::NotFound(format!("op-log anchor: {msg}")),
        other => HikerError::Io(other.to_string()),
    }
}

/// The op-log document `kind` for a vault-relative path. Native vault
/// markdown is `"markdown"`; a `*.<ext>.md` next to a non-md source is a
/// `"sidecar"` per `design.md`'s storage-mode table; a `.canvas` file is a
/// `"canvas"` JSON Canvas document. The string is recorded in the Yrs Doc's
/// `meta.kind` for the re-extraction / lifecycle surfaces that read it later;
/// bootstrap and create both stamp it.
//
// status: canvas-doc-kind
fn kind_for(rel: &str) -> &'static str {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    // A `.canvas` file is a first-class JSON Canvas op-log document — its
    // JSON text rides op-log exactly like a note, under the `canvas` kind.
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

/// Seed the op log from the on-disk vault. For every existing indexable
/// document (`.md` notes and sidecars) with no `doc-index.db` entry yet,
/// mint a doc_id, seed its Yrs Doc from the file's current bytes authored as
/// `user`, set `meta.kind` / `meta.path`, and write the `path → doc_id` row.
/// Returns the number of documents freshly seeded.
///
/// Idempotent: a path already mapped in `doc-index.db` is skipped, so a
/// second open is a no-op walk. The on-disk `.md` already equals
/// `materialize(accepted)` by construction, so [`OpLog::create_document`]
/// performs no rewrite of the user's file.
///
/// status: op-log-doc-id-bootstrap
pub fn bootstrap(vault: &Vault, log: &OpLog) -> Result<usize, HikerError> {
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
    // via sync (or a fresh open against an existing vault) need op-log
    // `doc_id`s exactly like vault-root notes so trail integrity holds
    // without waiting for an individual ingest event. status: op-log-doc-id-bootstrap
    for rel in walk_hidden_md_subtree(vault, &crate::trails::dir())? {
        seeded += seed_one(vault, log, &rel)? as usize;
    }
    // TODO: `.hiker/trees/` cluster-tree docs (per `cluster-editor.md` and
    // `op-log.md`'s sync section) deserve the same second-pass coverage
    // here. Currently per-tree `.md` files are managed by `core::trees`
    // and lazily seeded on first save via `op_writes::user_save` →
    // `doc_id_or_seed`; a vault arriving with pre-existing tree files
    // (sync, manual import) would not have op-log mappings until something
    // touches them. Add a `walk_hidden_md_subtree(vault, ".hiker/trees")`
    // pass once the tree-doc location stabilizes (or a watcher carve-out
    // is added for `.hiker/trees/` matching the trails carve-out).
    Ok(seeded)
}

/// Seed one path into the op-log if it isn't already mapped. Returns `true`
/// when a new doc was created, `false` when the path was skipped (already
/// mapped, or marked unreadable on a prior run, or unreadable now). Read
/// failures log and persist a skip marker but never abort the caller —
/// matching the original bootstrap loop's posture.
fn seed_one(vault: &Vault, log: &OpLog, rel: &str) -> Result<bool, HikerError> {
    if log.doc_id_for_path(rel).map_err(map_err)?.is_some() {
        return Ok(false);
    }
    if log.is_bootstrap_skipped(rel).map_err(map_err)? {
        return Ok(false);
    }
    let text = match vault.read_file(rel) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(path = %rel, error = %e, "op-log bootstrap: skipping unreadable note");
            if let Err(mark_err) = log.mark_bootstrap_skipped(rel, &e.to_string()) {
                tracing::debug!(path = %rel, error = %mark_err, "op-log bootstrap: could not persist skip marker");
            }
            return Ok(false);
        }
    };
    log.create_document(rel, kind_for(rel), &text, &Author::User)
        .map_err(map_err)?;
    Ok(true)
}

/// Walk a hidden vault subtree (e.g. `.hiker/trails`) returning every `.md`
/// file as a vault-relative path. Used by [`bootstrap`] (and the trails
/// storage-layout migration) to reach files the main
/// [`Vault::walk_indexable_files`] pass prunes at `.hiker/`. Symlinks are not
/// followed, mirroring the main walker's policy.
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
    log: &OpLog,
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

/// Ensure an op-log document exists for `rel`, seeding one from the file's
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
pub fn ensure_doc(log: &OpLog, vault: &Vault, rel: &str) -> Result<String, HikerError> {
    doc_id_or_seed(log, vault, rel, "")
}

/// Route a user save through the op log: resolve `rel` to its doc_id (seeding
/// one if necessary), then commit the buffer's full text as a `user` edit on
/// `accepted`. The op log diffs `new_text` against the current accepted state
/// into minimal localized spans, so a save lands as mergeable Yrs ops over
/// only the bytes that actually changed — never a whole-document rewrite. It
/// persists the Yrs Doc and atomically writes the materialized `.md` (the
/// `op-log-atomic-write` / `op-log-disk-canonical` path), so the caller does
/// **not** also write the file itself. A save that changes nothing is a no-op.
///
/// status: op-log-ops-producer-helpers
pub fn user_save(log: &OpLog, vault: &Vault, rel: &str, new_text: &str) -> Result<(), HikerError> {
    let doc_id = doc_id_or_seed(log, vault, rel, "")?;
    log.apply_user_text(&doc_id, new_text).map_err(map_err)?;
    Ok(())
}

/// The outcome of a re-extraction routed through [`reextract`]: which policy
/// fired and whether a new version landed. The host surfaces this to decide
/// whether to re-index the sidecar / report "no change".
///
/// status: op-log-reextract-replace
/// status: op-log-reextract-skip
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReextractOutcome {
    /// A previously-LINKED sidecar: the new body was applied as an `extractor`
    /// op on `accepted` and a new version landed.
    Replaced,
    /// A previously-LINKED sidecar whose re-extraction produced *identical*
    /// content — no op, no version (the no-op-on-identical contract).
    Unchanged,
    /// A previously-UNLINKED sidecar (the user unlinked to hand-edit): the
    /// extractor did not overwrite the body (`op-log-reextract-skip`).
    Skipped,
}

/// Route a re-extraction's new body onto an existing LINKED sidecar as an
/// `extractor`-authored op (`op-log-reextract-replace`), or skip it when the
/// sidecar is UNLINKED (`op-log-reextract-skip`). The policy is selected from
/// the sidecar's *current* on-disk link state: a `fill_body: false` /
/// `link_state: unlinked` sidecar means the user took the body over by hand, so
/// re-extraction must not clobber it; anything else (the linked default) lands
/// the new body in place, leaving prior bodies in op-log history rather than a
/// blind overwrite.
///
/// `rel` is the sidecar's vault-relative path; `new_body` is the freshly
/// extracted body (the `Extracted.markdown` the leaf crate produced);
/// `extractor_id` is the producing extractor's name (the `extractor:<id>`
/// author identity). The doc must already exist (a first-time extraction of a
/// brand-new sidecar uses the direct write path, not this) — an unmapped path
/// resolves no policy and is reported `Skipped`.
///
/// This is the seam between `hiker-extract`'s output and `core::oplog`: the
/// leaf crate produces the body; the host calls here to apply it as an op so
/// the sidecar's version history, diff, per-hunk restore, and the status-bar
/// version dropdown all come from the existing op-log / `core::changes`
/// surfaces — no bespoke version store.
///
/// status: op-log-reextract-replace
/// status: op-log-reextract-skip
/// status: extract-version-oplog
pub fn reextract(
    log: &OpLog,
    vault: &Vault,
    rel: &str,
    new_body: &str,
    extractor_id: &str,
) -> Result<ReextractOutcome, HikerError> {
    let Some(doc_id) = log.doc_id_for_path(rel).map_err(map_err)? else {
        // No op-log document: not a previously-extracted linked sidecar. The
        // first-time-extraction direct write path owns this case.
        return Ok(ReextractOutcome::Skipped);
    };
    if sidecar_is_unlinked(vault, rel) {
        return Ok(ReextractOutcome::Skipped);
    }
    if log.reextract_replace(&doc_id, new_body, extractor_id).map_err(map_err)? {
        Ok(ReextractOutcome::Replaced)
    } else {
        Ok(ReextractOutcome::Unchanged)
    }
}

/// Whether the sidecar at `rel` is UNLINKED — the user-unlinked-to-hand-edit
/// escape hatch (`capture-fill-body-toggle` / `extract-sidecar-linked-state`).
/// Reads the on-disk frontmatter: a sidecar is unlinked when `hiker.link_state`
/// is `unlinked` *or* `hiker.fill_body` is `false`. Anything else (linked
/// default, missing fields, unreadable file) is treated as LINKED so the
/// re-extraction replaces in place — the conservative default that keeps
/// extraction working for the source-type.
fn sidecar_is_unlinked(vault: &Vault, rel: &str) -> bool {
    let Ok(source) = vault.read_file(rel) else {
        return false;
    };
    let Some(fm) = crate::frontmatter::split(&source).frontmatter else {
        return false;
    };
    let Some(hiker) = fm.get("hiker") else {
        return false;
    };
    if hiker
        .get("link_state")
        .and_then(|v| v.as_str())
        .is_some_and(|s| s.eq_ignore_ascii_case("unlinked"))
    {
        return true;
    }
    // `fill_body: false` is the capture-spec note's body-link switch — an
    // explicit "don't fill the body" is the same as unlinked for re-extraction.
    hiker
        .get("fill_body")
        .and_then(serde_yml::Value::as_bool)
        == Some(false)
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
/// [`OpLog::stage_pending`] tagged `agent:<client_id>`. The ops do not reach
/// disk until accepted; the returned op ids let the caller surface them for
/// review and later flip each via [`flip_op_status`].
///
/// status: op-log-ops-producer-helpers
pub fn stage_agent_edits(
    log: &OpLog,
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
    log: &OpLog,
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
/// text*, tagged `auto:<producer>`. The op-log diffs the new text against the
/// current accepted state and queues one pending op (labeled `SetFrontmatter`
/// when the change lands in the frontmatter fence — the cluster-editor tag
/// shape — else `Replace`). Returns the minted batch id + op ids; the batch
/// id flips through [`flip_batch_status`] or the per-op [`flip_op_status`].
///
/// status: op-log-reorg-batch
pub fn stage_auto_content(
    log: &OpLog,
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

/// Accept or reject every pending op in a reorg `batch_id` (`op-log-reorg-batch`).
/// Accept applies each contributing `Rename` independently, skipping any that
/// fail (a target collision on one move does not block the rest — partial
/// apply); reject drops the whole batch from the queue. `accept = true`
/// accepts; `false` rejects. Returns the op ids that were applied / rejected.
///
/// status: op-log-reorg-batch
pub fn flip_batch_status(
    log: &OpLog,
    batch_id: &str,
    accept: bool,
) -> Result<Vec<String>, HikerError> {
    if accept {
        log.accept_batch(batch_id).map_err(map_err)
    } else {
        log.reject_batch(batch_id).map_err(map_err)
    }
}

/// Accept or reject pending ops by id. The single per-op primitive both the
/// per-hunk verbs and the patch-review bulk actions ride on: accept applies
/// the op's Yrs update to `accepted` (and atomically rewrites the `.md`),
/// reject writes a rejected audit row and drops the op. `accept = true`
/// accepts; `false` rejects. Resolves the doc_id from `rel`.
///
/// status: op-log-ops-producer-helpers
pub fn flip_op_status(
    log: &OpLog,
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
    log: &OpLog,
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
/// module placement the app may also read these straight off the `OpLog`
/// handle; this seam keeps the path→doc_id resolution in `core::ops` for
/// callers that only hold a path.
///
/// status: op-log-hunk-view
pub fn review_materializations(
    log: &OpLog,
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
    /// The op log document id the op lives on.
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
const fn whole_file_action(op_kind: &crate::oplog::shapes::OpKind) -> Option<&'static str> {
    use crate::oplog::shapes::OpKind;
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
/// target path. Walks `OpLog::all_pending_ops`, keeps only the whole-file
/// shapes (per [`whole_file_action`]), and resolves each op's `doc_id` to a
/// vault-relative path. Feeds the buffer review surface (version dropdown,
/// pending-rewrite banner, agent-diff toggle). Sorted newest-first so the
/// review surface opens the most recent proposal for a path by default
/// (`note-open-routes-to-pending-review`).
///
/// status: write-note-review-surface
pub fn list_whole_file_proposals(log: &OpLog) -> Result<Vec<WholeFileProposal>, HikerError> {
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
}

/// Coarse action label for a pending op's kind, used in the cross-vault
/// review tab's per-row header. Distinct from [`whole_file_action`], which
/// filters to *only* the whole-file shapes; this names every kind.
const fn action_label(op_kind: &crate::oplog::shapes::OpKind) -> &'static str {
    use crate::oplog::shapes::OpKind;
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
/// `OpLog::all_pending_ops`, resolves each op's `doc_id` to a vault-relative
/// path (`path_for_doc`), and checks drift (`is_pending_drifted`). Sorted
/// newest-first so the tab opens with the most recent proposals on top. Ops
/// whose document has no path mapping are skipped (they can't surface a row).
///
/// status: write-note-review-surface
pub fn list_pending_proposals(log: &OpLog) -> Result<Vec<PendingProposal>, HikerError> {
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
        });
    }
    out.sort_by_key(|b| std::cmp::Reverse(b.created_at_ms));
    Ok(out)
}

/// The number of pending ops across the whole vault — the pending count the
/// toolbar / status-bar / Patch-review badge displays. Counts the
/// `<doc-id>.pending` queue contents directly, so a zero count means the
/// review surfaces have nothing to show.
///
/// status: write-note-review-surface
pub fn pending_op_count(log: &OpLog) -> Result<usize, HikerError> {
    Ok(log.all_pending_ops().map_err(map_err)?.len())
}

/// The proposed content of a single pending whole-file op at `rel`, plus the
/// current accepted (= on-disk) content — the two ropes the whole-file review
/// surface's `DiffLayer` compares. Returns `(accepted_text, proposed_text)`.
/// `proposed_text` is `materialize(accepted + just this op)`; `accepted_text`
/// is `materialize(accepted)`. `Ok(None)` when the path has no doc. The op-log
/// preview path reads through here so the buffer never materializes Yrs state
/// itself.
///
/// status: write-note-review-surface
pub fn proposal_materializations(
    log: &OpLog,
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

/// A path's accepted-op history, newest-first — the version list behind the
/// snapshot dropdown, per-file history, and recent activity. Resolves
/// `rel` → doc_id; empty `Vec` when the path has no doc.
///
/// status: op-log-history-materialization
pub fn path_history(
    log: &OpLog,
    rel: &str,
    limit: usize,
) -> Result<Vec<crate::oplog::meta::OpMetadata>, HikerError> {
    let Some(doc_id) = log.doc_id_for_path(rel).map_err(map_err)? else {
        return Ok(Vec::new());
    };
    log.doc_history(&doc_id, limit).map_err(map_err)
}

/// The accepted content of `rel` as of accepted op `op_id` — the content
/// behind a version-dropdown preview / snapshot diff. `Ok(None)` when the path
/// has no doc or no retained history frame matches the op (pre-retention /
/// lifecycle marker). Reconstructed from the per-op snapshot, never the live doc.
///
/// status: op-log-history-materialization
pub fn content_at_op(log: &OpLog, rel: &str, op_id: &str) -> Result<Option<String>, HikerError> {
    let Some(doc_id) = log.doc_id_for_path(rel).map_err(map_err)? else {
        return Ok(None);
    };
    Ok(log
        .materialize_at(&doc_id, op_id)
        .map_err(map_err)?
        .map(|c| c.text))
}

/// The accepted content of `rel` as of the op immediately *before* its newest
/// accepted op — the "restore previous" rollback source. Returns
/// `(prior_op_id, content)`, or `None` when the path has no doc, no prior
/// version, or that version predates retention.
///
/// status: op-log-history-materialization
pub fn previous_accepted_content(
    log: &OpLog,
    rel: &str,
) -> Result<Option<(String, String)>, HikerError> {
    let Some(doc_id) = log.doc_id_for_path(rel).map_err(map_err)? else {
        return Ok(None);
    };
    let hist = log.doc_history(&doc_id, 2).map_err(map_err)?;
    let Some(prev) = hist.get(1) else {
        return Ok(None);
    };
    match log.materialize_at(&doc_id, &prev.op_id).map_err(map_err)? {
        Some(content) => Ok(Some((prev.op_id.clone(), content.text))),
        None => Ok(None),
    }
}

/// Reconcile an external `.md` edit into the op log. The watcher reports a
/// change hiker didn't initiate (after `watcher-suppress-self-writes` has
/// already dropped self-write echoes); this reads the new disk bytes and
/// hands them to the substrate, which compares against
/// `materialize(accepted)`: equal → ignored as a self-write echo (the safety
/// net); different → the text delta is applied to `accepted`'s `text` Y.Text
/// tagged `author=external`. Producers / the watcher bridge never touch
/// `OpLog` directly — this is the seam.
///
/// No-op (`Ok(false)`) when the path has no doc yet (the bootstrap / create
/// path adopts it instead) or when disk already equals accepted. Returns
/// `Ok(true)` when a delta was applied.
///
/// status: op-log-external-edit-sync
pub fn external_edit(log: &OpLog, vault: &Vault, rel: &str) -> Result<bool, HikerError> {
    let Some(doc_id) = log.doc_id_for_path(rel).map_err(map_err)? else {
        return Ok(false);
    };
    // Read the current disk bytes. A vanished/unreadable file is left for
    // the delete path to handle; reconciliation only covers content edits.
    let disk_text = match vault.read_file(rel) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(path = %rel, error = %e, "external-edit-sync: unreadable, skipping");
            return Ok(false);
        }
    };
    log.apply_external_edit(&doc_id, &disk_text).map_err(map_err)
}

/// Auto-reject any drifted pending ops on the document at `rel` when the
/// `[op-log] auto_reject_on_drift` flag is set. A no-op when the flag is
/// `false` or the path has no doc. Returns the op ids that were rejected.
/// The caller passes the resolved config flag so this stays free of config
/// plumbing. Per `op-log.md`'s drift section, this flips drifted ops to
/// `rejected` immediately rather than surfacing them in the file pill.
///
/// status: op-log-status-states
pub fn auto_reject_drifted(
    log: &OpLog,
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

/// Run the on-open retention GC: drop accepted-op metadata rows older than
/// `metadata_retention_days` and rejected-op rows older than
/// `rejected_retention_days`. Called once at vault open (compaction of the
/// `.yrs` snapshots already runs inside [`OpLog::open`] per
/// `compact_threshold`). Returns `(accepted_dropped, rejected_dropped)`.
///
/// A `retention_days` of `0` means "keep nothing older than now" — to avoid
/// surprising data loss it is treated as "no GC".
///
/// status: op-log-retention
pub fn run_retention_gc(
    log: &OpLog,
    metadata_retention_days: u32,
    rejected_retention_days: u32,
) -> Result<(usize, usize), HikerError> {
    use crate::oplog::meta::OpStatus;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let day_ms: i64 = 86_400_000;
    let mut accepted_dropped = 0;
    let mut rejected_dropped = 0;
    if metadata_retention_days > 0 {
        let cutoff = now_ms - i64::from(metadata_retention_days) * day_ms;
        accepted_dropped = log
            .gc_metadata(OpStatus::Accepted, cutoff)
            .map_err(map_err)?;
    }
    if rejected_retention_days > 0 {
        let cutoff = now_ms - i64::from(rejected_retention_days) * day_ms;
        rejected_dropped = log
            .gc_metadata(OpStatus::Rejected, cutoff)
            .map_err(map_err)?;
    }
    Ok((accepted_dropped, rejected_dropped))
}

/// Convenience alias for the `Arc<OpLog>` handle producers thread around. The
/// app holds one per vault session; the agent write paths take
/// `Option<&OpLogHandle>` so call sites without an op log open (early CLI,
/// some tests) skip op-log staging.
pub type OpLogHandle = Arc<OpLog>;
