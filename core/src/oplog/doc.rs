//! The Yrs Doc layout and every operation expressed against it. All `yrs`
//! API usage in the op log is confined here and in this module's siblings;
//! the [`OpLog`](super::OpLog) public surface returns plain Rust types only.
//!
//! Layout per `op-log-document-shape`:
//!
//! ```text
//! Y.Doc
//! ├── text: Y.Text   # the entire .md file, frontmatter fence + body, verbatim
//! └── meta: Y.Map    # { kind, path, tombstone } — never written into the file
//! ```
//!
//! Text positions use byte offsets (`OffsetKind::Bytes`) so a producer's
//! `old_str` byte range maps straight onto Y.Text positions with no UTF-16
//! conversion. Materialization is the identity over `text`, so the on-disk
//! `.md` equals `materialize(accepted)` byte-for-byte.
//
// status: op-log-document-shape
// status: op-log-materialization
// status: op-log-two-doc-model
// status: op-log-agent-replica
// status: op-log-pending-queue

use std::time::Duration;

use similar::{Algorithm, DiffTag, TextDiff};
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{
    Any, ClientID, Doc, GetString, Map, MapRef, Options, Out, ReadTxn, StateVector, Text, TextRef,
    Transact, Update,
};

/// Upper bound on the char-level diff a single save may spend computing. A
/// pathologically large rewrite degrades to a coarser (still correct) span
/// rather than stalling the save; the common small-edit case finishes far
/// inside this budget.
const DIFF_TIMEOUT: Duration = Duration::from_secs(1);

use super::error::Error;
use super::shapes::{is_frontmatter_range, AnchorHint, OpKind};

/// Pure read over a Yrs Doc: `text` is the file's bytes verbatim (no
/// parse/re-emit), `tombstone` is the `meta` flag. Drives every diff
/// render, save-to-disk, and accept dry-run.
///
/// status: op-log-materialization
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Materialized {
    pub text: String,
    pub tombstone: bool,
}

/// Build a fresh, empty Yrs Doc with the op-log root layout and byte-offset
/// text positions. Both roots are inserted so `get_text`/`get_map` reads
/// never miss.
pub(super) fn new_doc() -> Doc {
    let opts = Options {
        offset_kind: yrs::OffsetKind::Bytes,
        ..Options::default()
    };
    let doc = Doc::with_options(opts);
    doc.get_or_insert_text("text");
    doc.get_or_insert_map("meta");
    doc
}

/// The `text` root, resolved from a live transaction. `new_doc` always
/// inserts both roots, so this never misses on a Doc the op log produced.
///
/// Resolving the root *through the transaction* (rather than via
/// `Doc::get_or_insert_text`, which opens its own internal `transact_mut`)
/// is what keeps the op log deadlock-free: yrs's per-Doc store lock is not
/// re-entrant, so acquiring a second transaction while one is live blocks
/// forever. Every read/write below first opens its transaction, then pulls
/// the root off that same transaction.
fn text_in<T: ReadTxn>(txn: &T) -> TextRef {
    txn.get_text("text").expect("op-log Doc always has `text` root")
}

fn meta_in<T: ReadTxn>(txn: &T) -> MapRef {
    txn.get_map("meta").expect("op-log Doc always has `meta` root")
}

/// `materialize(doc)` per the spec — pure, no I/O.
///
/// status: op-log-materialization
pub(super) fn materialize(doc: &Doc) -> Materialized {
    let txn = doc.transact();
    let text = text_in(&txn).get_string(&txn);
    let tombstone = matches!(
        meta_in(&txn).get(&txn, "tombstone"),
        Some(Out::Any(Any::Bool(true)))
    );
    Materialized { text, tombstone }
}

/// Read a string field off `meta` (e.g. `kind`, `path`).
pub(super) fn meta_string(doc: &Doc, key: &str) -> Option<String> {
    let txn = doc.transact();
    match meta_in(&txn).get(&txn, key) {
        Some(Out::Any(Any::String(s))) => Some(s.to_string()),
        _ => None,
    }
}

/// Serialize the Doc's full state as a v2 update — the on-disk `.yrs`
/// representation and the compaction snapshot.
///
/// status: op-log-store-layout
/// status: op-log-compaction
pub(super) fn encode_full(doc: &Doc) -> Vec<u8> {
    let txn = doc.transact();
    txn.encode_state_as_update_v2(&StateVector::default())
}

/// The current clock for `cid` in `doc`'s state vector — the watermark a
/// write captures before and after a transaction to record the op's
/// `(yrs_clock_lo, yrs_clock_hi)` range in the side table. Opens its own
/// read transaction, so callers must not already hold one on `doc`.
pub(super) fn state_clock(doc: &Doc, cid: ClientID) -> i64 {
    let txn = doc.transact();
    txn.state_vector().get(&cid) as i64
}

/// The full state vector of `doc`, captured before a mutation so the ops it
/// then gains can be encoded with [`encode_since`].
pub(super) fn state_vector(doc: &Doc) -> StateVector {
    let txn = doc.transact();
    txn.state_vector()
}

/// The dominant `(client_id, clock_lo, clock_hi)` range gained between
/// `before_sv` and `after_sv` — used by the sync receive path
/// ([`super::OpLog::apply_remote_update`], `accept_pending`,
/// `adopt_lineage_theirs`) to record a side-table row that actually describes
/// the ops the update introduced.
///
/// The bug this fixes: those paths used to capture `cid =
/// accepted.client_id()` and bracket the apply with `state_clock(accepted,
/// cid)` pre/post — but the incoming Yrs update authors ops under the *peer's*
/// (or pending-session's) client id, so the local cid's clock never advances
/// and the recorded range is a zero-width `(local_cid, c, c)` describing
/// nothing real. Diffing state vectors per-client gives the actually-gained
/// ranges.
///
/// In the common case exactly one client id advances per `apply_update`. When
/// the update batches ops from several remote clients, we pick the cid with the
/// widest advance and record that one range — the side-table schema is one
/// `(client_id, lo, hi)` triple per op, so a lossy single-row pick is the
/// minimum-impact correct fix. Returns `None` when no client id advanced (the
/// update was a no-op against `before_sv`).
pub(super) fn dominant_advance(
    before_sv: &StateVector,
    after_sv: &StateVector,
) -> Option<(i64, i64, i64)> {
    let mut best: Option<(ClientID, u32, u32)> = None;
    for (cid, after_clock) in after_sv.iter() {
        let before_clock = before_sv.get(cid);
        if *after_clock > before_clock {
            let delta = *after_clock - before_clock;
            let widest = best.map_or(0, |(_, lo, hi)| hi - lo);
            if delta > widest {
                best = Some((*cid, before_clock, *after_clock));
            }
        }
    }
    best.map(|(cid, lo, hi)| (cid.get() as i64, lo as i64, hi as i64))
}

/// The update carrying every op `doc` gained since `before` was captured.
/// Replaying it onto another Doc that shares `doc`'s prior history merges
/// those ops in by anchor — used to mirror an `accepted` edit onto the
/// `working` overlay so the user's uncommitted edits stay layered on top.
pub(super) fn encode_since(doc: &Doc, before: &StateVector) -> Vec<u8> {
    let txn = doc.transact();
    txn.encode_state_as_update_v2(before)
}

/// The doc's current state vector encoded as v2 bytes — the watermark a peer
/// ships so this device can compute "ops since you last saw" (`export_since`).
/// Keeps the yrs `StateVector` type inside this module: only `Vec<u8>` crosses
/// the [`OpLog`](super::OpLog) boundary.
///
/// status: op-log-multi-device-sync
pub(super) fn state_vector_v2(doc: &Doc) -> Vec<u8> {
    let txn = doc.transact();
    txn.state_vector().encode_v2()
}

/// The update carrying every op `doc` holds beyond the *peer's* v2-encoded
/// state vector — the inbound side of `export_since`. Decodes the peer SV bytes
/// (so the yrs `StateVector` type never leaves this module) and returns
/// `encode_state_as_update_v2(&peer_sv)`. A malformed SV is a decode error.
///
/// status: op-log-multi-device-sync
pub(super) fn encode_since_sv_bytes(
    doc: &Doc,
    doc_id: &str,
    peer_state_vector: &[u8],
) -> Result<Vec<u8>, Error> {
    let sv = StateVector::decode_v2(peer_state_vector).map_err(|e| Error::YrsUpdate {
        doc_id: doc_id.to_string(),
        message: e.to_string(),
    })?;
    let txn = doc.transact();
    Ok(txn.encode_state_as_update_v2(&sv))
}

/// Decode a v2 update and apply it to `doc`. Used to load a Doc from its
/// `.yrs` bytes and to merge pending updates onto a clone.
pub(super) fn apply_update(doc: &Doc, doc_id: &str, update: &[u8]) -> Result<(), Error> {
    let update = Update::decode_v2(update).map_err(|e| Error::YrsUpdate {
        doc_id: doc_id.to_string(),
        message: e.to_string(),
    })?;
    let mut txn = doc.transact_mut();
    txn.apply_update(update).map_err(|e| Error::YrsUpdate {
        doc_id: doc_id.to_string(),
        message: e.to_string(),
    })?;
    Ok(())
}

/// Rebuild a Doc from its serialized `.yrs` bytes.
pub(super) fn load_doc(doc_id: &str, bytes: &[u8]) -> Result<Doc, Error> {
    let doc = new_doc();
    apply_update(&doc, doc_id, bytes)?;
    Ok(doc)
}

/// Clone the accepted Doc by round-tripping its full state into a fresh
/// Doc. Used for `pending_view` (apply pending on top) and for the
/// producer/drift dry-runs, which must never touch `accepted`.
///
/// status: op-log-two-doc-model
pub(super) fn clone_doc(doc: &Doc) -> Doc {
    let bytes = encode_full(doc);
    let clone = new_doc();
    // The bytes came straight from `encode_full`; decode/apply cannot fail
    // on well-formed self-produced state, but the error path is preserved
    // rather than unwrapped so a future malformed input surfaces loudly.
    let _ = apply_update(&clone, "<clone>", &bytes);
    clone
}

/// Seed a brand-new document: set `meta.kind` / `meta.path`, then insert
/// the initial bytes as the `text` content. Returns the Doc. The op-kind
/// pairing (`Create` then a content `Replace`) is recorded by the caller in
/// the side table; here we only establish the Yrs state.
///
/// status: op-log-document-shape
pub(super) fn seed_doc(kind: &str, path: &str, initial_text: &str) -> Doc {
    let doc = new_doc();
    {
        let mut txn = doc.transact_mut();
        let meta = meta_in(&txn);
        meta.insert(&mut txn, "kind", kind);
        meta.insert(&mut txn, "path", path);
        meta.insert(&mut txn, "tombstone", false);
        if !initial_text.is_empty() {
            text_in(&txn).insert(&mut txn, 0, initial_text);
        }
    }
    doc
}

/// Apply a byte-range replace directly to `doc`'s `text` (delete the old
/// range, insert the new content). The single primitive every text edit
/// rides on — user typing, accepted agent ops, external-edit reconciliation.
///
/// status: op-log-materialization
pub(super) fn apply_replace(doc: &Doc, byte_start: usize, byte_len: usize, new_text: &str) {
    let mut txn = doc.transact_mut();
    let text = text_in(&txn);
    if byte_len > 0 {
        text.remove_range(&mut txn, byte_start as u32, byte_len as u32);
    }
    if !new_text.is_empty() {
        text.insert(&mut txn, byte_start as u32, new_text);
    }
}

/// Byte offset of every char start in `s`, plus a trailing `s.len()`
/// sentinel — so a char index from a char-level diff maps straight to a byte
/// offset (the `text` Y.Text uses byte positions).
fn char_byte_bounds(s: &str) -> Vec<usize> {
    let mut bounds: Vec<usize> = s.char_indices().map(|(i, _)| i).collect();
    bounds.push(s.len());
    bounds
}

/// The minimal set of localized edit spans turning `before` into `after`,
/// each `(byte_start, removed_len, inserted)` in `before`'s coordinate space,
/// ascending and non-overlapping. A char-level Myers diff keeps every span as
/// small as the actual change: a save that touches two distant regions yields
/// two small ops, never one delete-and-reinsert of everything between them.
/// That locality is what keeps the CRDT mergeable — untouched bytes (including
/// a concurrent remote edit elsewhere in the file) are never rewritten, so a
/// remote op against them still merges cleanly.
///
/// `similar` orders its ops by the *new* sequence, so a pure-insert op's
/// `old_range()` anchor is **not** reliably monotonic: an insert that lands
/// after a kept character reports an old anchor *before* the preceding op's
/// old end (a `Delete .. Equal .. Insert` run anchors the insert inside the
/// delete). Mapping each op straight to an `old_range().start` span therefore
/// produces overlapping spans, which [`apply_replaces`] (high-offset-first
/// remove-then-insert) silently corrupts. We instead track our own `before`
/// cursor, advanced only by ops that consume `before` (`Equal`/`Delete`/
/// `Replace`), and coalesce each maximal run of non-`Equal` ops into one span
/// anchored at that cursor — guaranteeing the ascending, disjoint spans
/// `apply_replaces` requires.
///
/// status: op-log-yrs-backed
pub(super) fn multi_span_delta(before: &str, after: &str) -> Vec<(usize, usize, String)> {
    if before == after {
        return Vec::new();
    }
    let before_bounds = char_byte_bounds(before);
    let after_bounds = char_byte_bounds(after);
    let diff = TextDiff::configure()
        .algorithm(Algorithm::Myers)
        .timeout(DIFF_TIMEOUT)
        .diff_chars(before, after);

    let mut spans = Vec::new();
    // The open run, in `before` char indices: `[run_start, run_end)` is the
    // region it removes, `run_inserted` its replacement. `cursor` is how far
    // into `before` we've consumed — the anchor a fresh run starts at.
    let mut run_start: Option<usize> = None;
    let mut run_end = 0usize;
    let mut run_inserted = String::new();
    let mut cursor = 0usize;

    for op in diff.ops() {
        let old = op.old_range();
        if op.tag() == DiffTag::Equal {
            if let Some(start) = run_start.take() {
                let start_b = before_bounds[start];
                spans.push((start_b, before_bounds[run_end] - start_b, std::mem::take(&mut run_inserted)));
            }
            cursor = old.end;
            continue;
        }
        let new = op.new_range();
        if run_start.is_none() {
            run_start = Some(cursor);
            run_end = cursor;
        }
        // Only `Delete`/`Replace` consume `before`; a pure insert is
        // zero-width and leaves `run_end` (and the cursor) where they are.
        if old.end > run_end {
            run_end = old.end;
        }
        run_inserted.push_str(&after[after_bounds[new.start]..after_bounds[new.end]]);
        cursor = run_end;
    }
    if let Some(start) = run_start {
        let start_b = before_bounds[start];
        spans.push((start_b, before_bounds[run_end] - start_b, run_inserted));
    }
    spans
}

/// Three-way text merge for lineage adoption (`sync-lineage-adoption`): combine
/// our divergence (`ours`, the adopting device's local text) and the peer's
/// divergence (`theirs`, the canonical text) over their common ancestor
/// (`base`, the pre-divergence seed) into one merged text.
///
/// Two independently-seeded Yrs Docs can't CRDT-merge (identical bytes
/// interleave because the lineages share no history), so the merge happens at
/// the text level over the known common base, then lands as `user` ops on the
/// adopted canonical lineage. The merge replays the peer's text (`theirs` is
/// the new base on disk) and re-applies *our* localized divergence spans
/// (`base → ours`) on top, shifting each span's offset by the net byte delta
/// the peer's earlier edits introduced — so disjoint edits both survive. A span
/// that would land inside a region the peer also changed is dropped (the peer's
/// content wins there), keeping the merge deterministic rather than interleaving
/// a genuine overlap.
///
/// status: op-log-multi-device-sync
pub(super) fn three_way_merge(base: &str, ours: &str, theirs: &str) -> String {
    // Our localized divergence, in `base` coordinates (ascending, disjoint).
    let our_spans = multi_span_delta(base, ours);
    // The peer's changed regions, in `base` coordinates, so we can both shift
    // our spans past the peer's net length change and detect overlaps.
    let their_spans = multi_span_delta(base, theirs);
    let mut merged = theirs.to_string();
    // Apply our spans high-offset-first so an earlier application never shifts a
    // later span's coordinates (the `apply_replaces` discipline).
    for (start, removed_len, inserted) in our_spans.iter().rev() {
        let our_end = start + removed_len;
        // Drop a span overlapping any region the peer also edited — the peer's
        // content is canonical there (no silent interleave of a real conflict).
        let overlaps_peer = their_spans
            .iter()
            .any(|(ts, tl, _)| *start < ts + tl && *ts < our_end);
        if overlaps_peer {
            continue;
        }
        // Shift by the peer's net byte change strictly *before* this span, so
        // the offset lands in `theirs`/`merged` coordinates.
        let shift: isize = their_spans
            .iter()
            .filter(|(ts, tl, _)| ts + tl <= *start)
            .map(|(_, tl, ins)| ins.len() as isize - *tl as isize)
            .sum();
        let m_start = (*start as isize + shift).clamp(0, merged.len() as isize) as usize;
        let m_end = (m_start + removed_len).min(merged.len());
        if !merged.is_char_boundary(m_start) || !merged.is_char_boundary(m_end) {
            continue;
        }
        merged.replace_range(m_start..m_end, inserted);
    }
    merged
}

/// Apply a set of `(byte_start, removed_len, inserted)` spans — the shape
/// [`multi_span_delta`] returns (ascending, non-overlapping) — to `doc`'s
/// `text` in one transaction, so the whole edit lands as a single contiguous
/// Yrs clock range (one logical op, one side-table row). Spans apply
/// high-offset-first so an earlier edit never shifts a later span's
/// coordinates.
///
/// status: op-log-materialization
pub(super) fn apply_replaces(doc: &Doc, spans: &[(usize, usize, String)]) {
    let mut txn = doc.transact_mut();
    let text = text_in(&txn);
    for (start, removed_len, inserted) in spans.iter().rev() {
        if *removed_len > 0 {
            text.remove_range(&mut txn, *start as u32, *removed_len as u32);
        }
        if !inserted.is_empty() {
            text.insert(&mut txn, *start as u32, inserted);
        }
    }
}

/// Set `meta.tombstone = true`.
///
/// status: op-log-document-shape
pub(super) fn apply_tombstone(doc: &Doc) {
    let mut txn = doc.transact_mut();
    meta_in(&txn).insert(&mut txn, "tombstone", true);
}

/// Set `meta.tombstone = false` — resurrect a logically-deleted document.
/// A content write that lands on a tombstoned doc (a re-create at a path
/// whose previous document was deleted — `tombstone_document` keeps the
/// `path → doc_id` mapping) flips the doc live again, so the materialized
/// `.md` is written instead of suppressed by [`write_md_file`].
///
/// status: op-log-document-shape
pub(super) fn clear_tombstone(doc: &Doc) {
    let mut txn = doc.transact_mut();
    meta_in(&txn).insert(&mut txn, "tombstone", false);
}

/// Set `meta.path` to a new vault-relative path (a rename).
///
/// status: op-log-document-shape
pub(super) fn apply_rename(doc: &Doc, new_path: &str) {
    let mut txn = doc.transact_mut();
    meta_in(&txn).insert(&mut txn, "path", new_path);
}

/// Resolve `old_str` to exactly one byte range in `materialized`. Mirrors
/// the staging anchor contract: zero matches or (without `replace_all`)
/// multiple matches are an anchor conflict. With `replace_all` every
/// occurrence is one logical replace — but the op log models each as its
/// own update, so this returns the first range and the caller iterates.
pub(super) fn resolve_anchor(materialized: &str, old_str: &str) -> Result<usize, Error> {
    let mut matches = materialized.match_indices(old_str);
    let first = matches
        .next()
        .ok_or_else(|| Error::Anchor(format!("no match for old_str ({} bytes)", old_str.len())))?;
    if matches.next().is_some() {
        return Err(Error::Anchor(
            "old_str matched multiple times without replace_all".to_string(),
        ));
    }
    Ok(first.0)
}

/// Result of translating a producer edit into a serialized pending update.
pub(super) struct ProducedOp {
    pub yrs_update: Vec<u8>,
    pub op_kind: OpKind,
}

/// The producer step (per `op-log-pending-queue`): read `materialize(accepted)`,
/// resolve `old_str` to a byte range, translate to Y.Text positions on a
/// *clone* of accepted, apply the replace, and serialize the resulting
/// update as a diff against accepted's state vector. The clone is discarded;
/// only the update bytes survive. The op-kind is `SetFrontmatter` when the
/// byte range lands inside the frontmatter fence, else `Replace`.
///
/// status: op-log-pending-queue
/// status: op-log-agent-replica
pub(super) fn produce_replace(
    accepted: &Doc,
    old_str: &str,
    new_str: &str,
) -> Result<ProducedOp, Error> {
    let base = materialize(accepted);
    let start = resolve_anchor(&base.text, old_str)?;
    let len = old_str.len();
    let before_sv = {
        let txn = accepted.transact();
        txn.state_vector()
    };
    let clone = clone_doc(accepted);
    apply_replace(&clone, start, len, new_str);
    let yrs_update = {
        let txn = clone.transact();
        txn.encode_state_as_update_v2(&before_sv)
    };
    let op_kind = if is_frontmatter_range(&base.text, start, start + len) {
        OpKind::SetFrontmatter
    } else {
        OpKind::Replace {
            anchor: Some(AnchorHint::from_old_str(old_str)),
        }
    };
    Ok(ProducedOp {
        yrs_update,
        op_kind,
    })
}

/// Produce a pending content edit from a *whole new document text*: diff the
/// new text against `materialize(accepted)`, apply the minimal single-span
/// change to a clone, and serialize the update. The op-kind is `SetFrontmatter`
/// when the changed span lands inside the frontmatter fence (the
/// `apply_tag` / `set_frontmatter` producer shape), else an anchorless
/// `Replace`. Used by producers that already computed a full new file (e.g.
/// the cluster-editor tag path's frontmatter re-emit) rather than an anchored
/// find-replace. Returns `None` when `new_text` equals the current text.
///
/// status: op-log-pending-queue
/// status: op-log-op-shape
pub(super) fn produce_content_replace(accepted: &Doc, new_text: &str) -> Option<ProducedOp> {
    let base = materialize(accepted);
    if base.text == new_text {
        return None;
    }
    let (start, removed_len, inserted) = text_delta(&base.text, new_text);
    let before_sv = {
        let txn = accepted.transact();
        txn.state_vector()
    };
    let clone = clone_doc(accepted);
    apply_replace(&clone, start, removed_len, &inserted);
    let yrs_update = {
        let txn = clone.transact();
        txn.encode_state_as_update_v2(&before_sv)
    };
    let op_kind = if is_frontmatter_range(&base.text, start, start + removed_len) {
        OpKind::SetFrontmatter
    } else {
        OpKind::Replace { anchor: None }
    };
    Some(ProducedOp {
        yrs_update,
        op_kind,
    })
}

/// Produce a pending `Rename`: a serialized Yrs update that sets `meta.path`
/// to `new_path`, computed against a *clone* of `accepted` so the canonical
/// Doc is untouched until accept. The op-kind carries the prior path as
/// `from` (read off the clone's current `meta.path`). The clone is discarded;
/// only the update bytes survive, the same producer discipline as
/// [`produce_replace`]. Accept replays the bytes onto `accepted`, repointing
/// `meta.path` there.
///
/// status: op-log-reorg-batch
/// status: op-log-pending-queue
pub(super) fn produce_rename(accepted: &Doc, new_path: &str) -> ProducedOp {
    let from = meta_string(accepted, "path").unwrap_or_default();
    let before_sv = {
        let txn = accepted.transact();
        txn.state_vector()
    };
    let clone = clone_doc(accepted);
    apply_rename(&clone, new_path);
    let yrs_update = {
        let txn = clone.transact();
        txn.encode_state_as_update_v2(&before_sv)
    };
    ProducedOp {
        yrs_update,
        op_kind: OpKind::Rename { from },
    }
}

/// Whether a pending op's update still applies cleanly against `accepted`'s
/// *current* state. Per `op-log-pending-queue` drift detection: apply the
/// update to a clone of current accepted; a position-resolution / decode
/// failure means the op is drifted. Returns `true` when the update still
/// applies. Used for anchorless ops (whole-body rewrites); anchored ops use
/// the stronger `old_str`-resolves check in [`OpLog::is_pending_drifted`].
///
/// status: op-log-pending-queue
pub(super) fn applies_cleanly(accepted: &Doc, doc_id: &str, update: &[u8]) -> bool {
    let clone = clone_doc(accepted);
    apply_update(&clone, doc_id, update).is_ok()
}

/// The byte range a single pending update affects, expressed in the
/// coordinate space of `materialize(base)`. Applies the update to a clone of
/// `base` and diffs the two materializations: the changed span runs from the
/// first differing byte to the last differing byte (in the *base* text), which
/// is the range the agent's edit touches. Returns `None` when the update fails
/// to apply (drifted) or produces no change.
///
/// `base` is the doc the pending op sits on top of in the view being queried:
/// `accepted` when the buffer is clean, or `working` when the user has
/// uncommitted edits (so the result lands in the same coordinate space as the
/// review overlay, `materialize(working + pending)`).
///
/// Per `op-log-per-hunk-accept-reject`, this is the "apply to a clone and
/// check the affected position range" resolution the hunk-overlap query
/// rides on.
///
/// status: op-log-per-hunk-accept-reject
pub(super) fn affected_range(base: &Doc, doc_id: &str, update: &[u8]) -> Option<(usize, usize)> {
    let before = materialize(base).text;
    let clone = clone_doc(base);
    if apply_update(&clone, doc_id, update).is_err() {
        return None;
    }
    let after = materialize(&clone).text;
    changed_span(&before, &after)
}

/// The minimal single-span edit turning `before` into `after`, as
/// `(byte_start, removed_len, inserted_text)`. Trims the common prefix and
/// suffix and treats the differing middle as one replace — the shape
/// [`apply_replace`] consumes. For identical inputs returns a zero-length
/// no-op at offset 0. Used by external-edit reconciliation to apply a disk
/// change as one Y.Text edit (frontmatter + body share the same `text`, so
/// one delta covers both).
///
/// The split honours UTF-8 char boundaries: the trimmed prefix/suffix are
/// backed off to the nearest boundary so the inserted slice is valid UTF-8.
///
/// status: op-log-external-edit-sync
pub(super) fn text_delta(before: &str, after: &str) -> (usize, usize, String) {
    let b = before.as_bytes();
    let a = after.as_bytes();
    if b == a {
        return (0, 0, String::new());
    }
    let max_prefix = b.len().min(a.len());
    let mut start = 0;
    while start < max_prefix && b[start] == a[start] {
        start += 1;
    }
    // Back the prefix off to a char boundary in `before` (== same boundary
    // in `after`, since the bytes matched up to `start`).
    while start > 0 && !before.is_char_boundary(start) {
        start -= 1;
    }
    let mut suffix = 0;
    while suffix < (b.len() - start).min(a.len() - start)
        && b[b.len() - 1 - suffix] == a[a.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let before_end = b.len() - suffix;
    let after_end = a.len() - suffix;
    // Back the suffix split off to char boundaries on both sides.
    let mut before_end = before_end;
    while before_end < b.len() && !before.is_char_boundary(before_end) {
        before_end += 1;
    }
    let mut after_end = after_end;
    while after_end < a.len() && !after.is_char_boundary(after_end) {
        after_end += 1;
    }
    let removed_len = before_end.saturating_sub(start);
    let inserted = after[start..after_end.max(start)].to_string();
    (start, removed_len, inserted)
}

/// First and last differing byte offsets between `before` and `after`,
/// returned as a half-open `[start, end)` range over `before`'s bytes.
/// `None` when the two are identical. The end is clamped so a pure-insertion
/// at a point still yields a non-empty single-position range
/// (`[p, p+1)` clamped to `before.len()`), so overlap tests against a hunk
/// that contains the insertion point still match.
fn changed_span(before: &str, after: &str) -> Option<(usize, usize)> {
    let b = before.as_bytes();
    let a = after.as_bytes();
    if b == a {
        return None;
    }
    let max_prefix = b.len().min(a.len());
    let mut start = 0;
    while start < max_prefix && b[start] == a[start] {
        start += 1;
    }
    // Common suffix length, not crossing into the common prefix already counted.
    let mut suffix = 0;
    while suffix < (b.len() - start).min(a.len() - start)
        && b[b.len() - 1 - suffix] == a[a.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let end_before = b.len() - suffix;
    // For a pure insertion the differing region in `before` is empty
    // (`start == end_before`); widen to a single position so overlap tests
    // against a hunk covering the insertion point still match.
    let end = if end_before <= start {
        (start + 1).min(b.len().max(start))
    } else {
        end_before
    };
    Some((start, end))
}

#[cfg(test)]
mod delta_tests {
    use super::*;

    /// Apply spans to a String with `apply_replaces`'s high-offset-first
    /// discipline, using plain string ops — a Yrs-free oracle for the spans.
    fn apply_spans_str(before: &str, spans: &[(usize, usize, String)]) -> String {
        let mut s = before.to_string();
        for (start, removed_len, inserted) in spans.iter().rev() {
            s.replace_range(*start..(*start + *removed_len), inserted);
        }
        s
    }

    /// Spans must be ascending and non-overlapping in `before` coordinates,
    /// and applying them must reproduce `after`. Regression for the
    /// `similar` new-ordered-ops anchoring bug, where a `Delete .. Equal ..
    /// Insert` run anchored the insert *inside* the delete, yielding
    /// overlapping spans that `apply_replaces` corrupted (chars relocated,
    /// same total length) — e.g. a cluster-tree's `nodes: []` growing into a
    /// `nodes:\n  - id: ...` block dropped the newline after `nodes:`.
    fn assert_delta_sound(before: &str, after: &str) {
        let spans = multi_span_delta(before, after);
        let mut prev_end = 0usize;
        for (start, removed, _) in &spans {
            assert!(*start >= prev_end, "overlapping spans: start {start} < prev_end {prev_end}\n{spans:?}");
            prev_end = start + removed;
        }
        assert_eq!(apply_spans_str(before, &spans), after, "spans do not reproduce `after`");
    }

    #[test]
    fn yaml_seq_growth_anchors_insert_correctly() {
        // The exact shape that broke a cluster-tree confirm: a `nodes: []`
        // frontmatter grows into a populated block sequence. At this size +
        // structure `similar`'s new-ordered ops anchor the insert inside the
        // `Delete " []"` op (a `Delete .. Equal .. Insert` run), the case the
        // old per-op mapping turned into overlapping spans.
        let head = "---\nhiker:\n  kind: cluster-tree\n  id: '01KSG5MBTAEEW98M6BY06TYSFX'\n  name: Semantic\n  source: review:confirm\n  state: draft\n  created_at_ms: 1779732983626\n  nodes:";
        let body = "\n---\n<!-- Cluster tree. The structure lives in the `hiker:` frontmatter above. -->\n";
        let before = format!("{head} []{body}");
        let mut after = String::from(head);
        after.push_str("\n  - id: root\n    kind: cluster\n    name: 'Vault: notes'\n    confidence: 1.0");
        for i in 0..80 {
            after.push_str(&format!(
                "\n  - id: n{i}\n    parent: root\n    kind: leaf\n    note:\n      id: note-{i}\n      path: note-{i}\n    name: 'Cluster #{i}: misc - ascii stuff and things'\n    confidence: 1.0"
            ));
        }
        after.push_str(body);

        assert_delta_sound(&before, &after);
        // End to end through the Yrs apply path: the hot symptom was a dropped
        // newline after `nodes:` (`nodes:- id`) that made the frontmatter
        // unparseable on reload.
        let doc = seed_doc("markdown", "x.md", "");
        apply_replaces(&doc, &multi_span_delta("", &before));
        apply_replaces(&doc, &multi_span_delta(&before, &after));
        assert_eq!(materialize(&doc).text, after);
        assert!(!materialize(&doc).text.contains("nodes:- "));
    }

    #[test]
    fn assorted_transitions_are_sound() {
        let cases = [
            ("", "hello world"),
            ("hello world", ""),
            ("abc", "aXbYc"),
            ("a: []\nb: 1\n", "a:\n  - x\n  - y\nb: 1\n"),
            ("line\nline\nline\n", "line\nCHANGED\nline\n"),
            ("keep [drop] keep", "keep [\n  insert\n] keep"),
        ];
        for (before, after) in cases {
            assert_delta_sound(before, after);
        }
    }
}
