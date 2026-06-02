//! The per-frame editor binding for an op-log-backed vault buffer: the
//! forward/reverse direction tying the editor's change sets to the document's
//! `working` CRDT layer, plus the overlay refresh that drives inline review.
//!
//! Run once per frame from `panels::buffer::show_editor`, after the widget has
//! applied this frame's input. Per `op-log-editor-binding` in `op-log.md`.
//!
//! **The editable buffer shows `working` (`accepted + the user's uncommitted
//! edits`).** The agent's pending ops are *not* in the buffer text — they
//! render as a suggestion overlay (`materialize_review = working + pending`,
//! diffed against the buffer: additions as phantom blocks, deletions struck
//! through). Because the buffer *is* `working`, the user's edits land on the
//! `working` layer with no coordinate translation; the cursor only ever needs
//! mapping when `working` itself advances out-of-band (an accepted agent op or
//! an external edit), handled in the reverse step. This is the "single generic
//! editor + CRDT-as-overlay" shape (the y-codemirror.next pattern): the editor
//! crate stays CRDT-agnostic; this binding is the only adapter.

use eframe::egui;

use crate::state::AppState;

/// One concrete edit walked out of an editor change set: replace
/// `[byte_start, byte_start + delete_len)` with `insert`.
struct WorkingEdit {
    byte_start: usize,
    delete_len: usize,
    insert: String,
}

/// Walk an editor change set into the (byte_start, delete_len, insert) edits
/// the `working` layer's `apply_working_edit` consumes, applied in sequence.
/// `cursor` tracks the byte offset in the document *as each edit lands* — i.e.
/// it already reflects the preceding edits in this set:
///
/// - `Retain(n)` skips `n` kept bytes — advance the cursor.
/// - `Delete(n)` removes `n` bytes at the cursor; the following text shifts
///   left into the cursor position, so the cursor does *not* advance. A
///   subsequent `Insert` in the same set therefore lands at the deletion site
///   (the replace case), which is what applying delete-then-insert in order
///   produces.
/// - `Insert(s)` inserts `s` at the cursor — advance past the inserted bytes.
///
/// Buffer byte offsets are document text positions directly (both
/// byte-indexed), so no coordinate translation is needed. Per
/// `op-log-editor-binding`.
fn change_set_edits(set: &editor_core::change::Set) -> Vec<WorkingEdit> {
    use editor_core::change::Op;
    let mut edits = Vec::new();
    let mut cursor = 0usize;
    for op in set.ops() {
        match op {
            Op::Retain(n) => cursor += *n as usize,
            Op::Delete(n) => edits.push(WorkingEdit {
                byte_start: cursor,
                delete_len: *n as usize,
                insert: String::new(),
            }),
            Op::Insert(s) => {
                edits.push(WorkingEdit {
                    byte_start: cursor,
                    delete_len: 0,
                    insert: s.to_string(),
                });
                cursor += s.len();
            }
        }
    }
    edits
}

/// Build an editor change [`Set`](editor_core::change::Set) describing
/// `old → new` as a sequence of non-overlapping replacements, so a selection
/// can be carried across the change by mapping it through the set
/// (`Selection::map`) — CodeMirror's `ChangeSet`/`mapPos` discipline — rather
/// than clamping a stale absolute offset (which loses the cursor when an agent
/// edit lands above it). The diff is computed at *character* granularity and
/// byte offsets are accumulated as we walk, so every emitted range lands on a
/// UTF-8 char boundary regardless of multi-byte content. Equal runs become the
/// implicit retains `Set::of` fills between edits.
fn change_set_between(old: &str, new: &str) -> editor_core::change::Set {
    use editor_core::change::Set;
    use similar::{capture_diff_slices, Algorithm, DiffOp};

    let old_chars: Vec<char> = old.chars().collect();
    let new_chars: Vec<char> = new.chars().collect();
    // `old_byte[i]` = byte offset of old char `i` (last entry == old.len()).
    let mut old_byte = Vec::with_capacity(old_chars.len() + 1);
    let mut acc = 0usize;
    old_byte.push(0);
    for c in &old_chars {
        acc += c.len_utf8();
        old_byte.push(acc);
    }
    let take = |lo: usize, hi: usize| new_chars[lo..hi].iter().collect::<String>();
    let mut edits: Vec<(std::ops::Range<usize>, String)> = Vec::new();
    for op in capture_diff_slices(Algorithm::Myers, &old_chars, &new_chars) {
        match op {
            DiffOp::Equal { .. } => {}
            DiffOp::Delete { old_index, old_len, .. } => {
                edits.push((old_byte[old_index]..old_byte[old_index + old_len], String::new()));
            }
            DiffOp::Insert { old_index, new_index, new_len } => {
                edits.push((old_byte[old_index]..old_byte[old_index], take(new_index, new_index + new_len)));
            }
            DiffOp::Replace { old_index, old_len, new_index, new_len } => {
                edits.push((
                    old_byte[old_index]..old_byte[old_index + old_len],
                    take(new_index, new_index + new_len),
                ));
            }
        }
    }
    Set::of(old.len(), edits)
}

/// The editor binding for an op-log-backed vault buffer (per
/// `op-log-editor-binding`). The editable buffer *is* `materialize_working`;
/// the agent's pending ops live in the overlay, not the buffer text. Three
/// steps, run once per frame after the widget applied this frame's input:
///
/// 1. **Forward** — walk each captured change set into (byte_start, delete_len,
///    insert) edits and apply them to the `working` layer directly. The buffer
///    is `working`, so the offsets are already working coordinates — no
///    translation. After this, `working` equals `editor.doc`, so step 2 is a
///    no-op for plain typing.
/// 2. **Reverse** — read `materialize_working`; if it differs from `editor.doc`,
///    pull it into the buffer, mapping the selection through the change so the
///    cursor rides it. This fires when `working` advanced *without* matching
///    user typing — an accepted agent op replayed onto `working`, or an external
///    edit. Host-applied doc edits don't go through the widget's input path, so
///    they never re-enter the forward sink: no echo.
/// 3. **Overlay** — read `materialize_review` (`working + pending`); when it
///    differs from `working`, stash it in `agent_proposal` so the inline review
///    can diff the buffer (`working`) against the proposal and render the
///    pending ops as a suggestion overlay. When equal (no pending),
///    `agent_proposal` clears and the buffer is plain editing.
pub(crate) fn run(
    app: &mut AppState,
    path: &str,
    txns: &[editor_core::transaction::Transaction],
) {
    let log = app.vault_session.services.oplog.clone();
    // Only op-log-backed vault buffers participate; others (snapshot / pending
    // / trash previews, or a path with no doc yet) keep their disk-only flow.
    let Some(buffer) = app.session.buffers.get_mut(path) else { return };
    if !matches!(buffer.source, crate::tab::BufferSource::Vault { .. }) {
        return;
    }
    let Ok(Some(doc_id)) = log.doc_id_for_path(path) else { return };
    apply_binding(&log, buffer, &doc_id, path, txns);
}

/// Resolve and apply a panel-level Ctrl-Z / Ctrl-Shift-Z for the *active*
/// editor tab, returning the inverse change set(s) to seed the forward
/// binding's per-frame sink.
///
/// Undo/redo live here, at the host level, rather than inside the editor
/// widget so the chord fires whenever the tab is active — not only while the
/// widget itself holds egui keyboard focus. Clicking a toolbar button or the
/// minimap moves focus off the widget, which otherwise strands Ctrl-Z. We
/// still defer to a focused host text field (chat / search / command palette)
/// so Ctrl-Z there undoes *that* field, and only the active *vault* tab
/// responds so split panes don't all undo at once. The matched chord is
/// consumed so the widget's own (focus-gated) handler can't double-apply it.
///
/// The history pop is applied to `editor.doc` here; its inverse change set is
/// returned so the caller can seed the sink and the forward step mirrors the
/// undo onto `working`. A sink-less undo would touch only `editor.doc` and be
/// reverted by the reverse step — see
/// `merge_scenarios::undo_reverts_working_and_is_not_clobbered_by_reverse`.
pub(super) fn handle_undo_redo(
    ui: &egui::Ui,
    app: &mut AppState,
    path: &str,
) -> Vec<editor_core::transaction::Transaction> {
    let is_active_vault_tab = app
        .session
        .active_tab
        .and_then(|id| app.session.tabs.iter().find(|t| t.id == id))
        .and_then(|t| t.kind.vault_path())
        == Some(path);
    let focused = ui.memory(egui::Memory::focused);
    let host_text_focused = app.ui.palette_open
        || (focused.is_some()
            && (focused == app.ui.chat_input_id || focused == app.ui.search_input_id));
    if !is_active_vault_tab || host_text_focused {
        return Vec::new();
    }

    let cmd = egui::Modifiers::COMMAND;
    // Redo first: egui's `consume_key` matches modifiers logically, so a bare
    // `cmd+Z` filter would also swallow `cmd+shift+Z`. Drain the shift variant
    // (and the `cmd+Y` alias) before the plain undo.
    let redo = ui.input_mut(|i| {
        i.consume_key(cmd | egui::Modifiers::SHIFT, egui::Key::Z)
            || i.consume_key(cmd, egui::Key::Y)
    });
    let undo = !redo && ui.input_mut(|i| i.consume_key(cmd, egui::Key::Z));

    let Some(buffer) = app.session.buffers.get_mut(path) else { return Vec::new() };
    let stepped = if redo {
        buffer.editor.redo_with_changes()
    } else if undo {
        buffer.editor.undo_with_changes()
    } else {
        None
    };
    match stepped {
        Some((next, tx)) => {
            buffer.editor = next;
            vec![tx]
        }
        None => Vec::new(),
    }
}

/// The forward/reverse/overlay binding for one op-log-backed buffer, factored
/// out of [`run`] so it operates on a plain `(&OpLog, &mut Buffer)` pair — no
/// `AppState` container — which lets the merge scenarios be exercised
/// end-to-end in tests. [`run`] is the thin `AppState` adapter (buffer lookup +
/// `Vault`-source gate + `doc_id` resolution); everything below is the actual
/// binding. `path` is used only for log context.
fn apply_binding(
    log: &hiker_core::oplog::OpLog,
    buffer: &mut crate::buffer::Buffer,
    doc_id: &str,
    path: &str,
    txns: &[editor_core::transaction::Transaction],
) {
    let session = buffer.active_session.clone();

    // Step 1 — forward: mirror this frame's user edits into `working`. The
    // editable buffer *is* `materialize_working` (the agent's pending ops live
    // in the overlay, not the buffer text), so the change-set byte offsets are
    // already working coordinates — applied directly, no translation.
    for txn in txns {
        if txn.changes.is_identity() {
            continue;
        }
        for edit in change_set_edits(&txn.changes) {
            if let Err(e) =
                log.apply_working_edit(doc_id, edit.byte_start, edit.delete_len, &edit.insert)
            {
                tracing::warn!(error = %e, path, "oplog: apply_working_edit failed");
            }
        }
    }

    // Materialize the working layer once per frame and reuse it for both the
    // reverse pull and the overlay base (one lock acquisition, one full-doc
    // read instead of two).
    let working_text = log.materialize_working(doc_id).map(|c| c.text).ok();

    // Step 2 — reverse: the editable buffer tracks `materialize_working`. When
    // it advances without matching user typing — an accepted agent op replayed
    // onto `working`, or an external edit — pull it into the buffer, *mapping*
    // the selection through the old→new change set (CodeMirror's
    // `ChangeSet.mapPos` discipline) so the cursor rides the edit rather than
    // being stranded at a stale absolute offset. For plain typing the forward
    // step already advanced `working` to equal `editor.doc`, so this is inert.
    if let Some(working) = &working_text {
        let current = buffer.editor.doc.to_string();
        if *working != current {
            let changes = change_set_between(&current, working);
            buffer.editor.selection = buffer.editor.selection.clone().map(&changes);
            // Swap in the new doc and clamp the now-mapped selection to a valid
            // char boundary as a safety net.
            buffer.set_doc_clamping_selection(working);
        }
    }

    // Step 3 — overlay: stash the agent's *proposal* (`materialize_review` =
    // `working + pending`) when it differs from the buffer (`working`), so the
    // inline review can diff the buffer against it and render the pending ops as
    // a suggestion overlay — additions as phantom blocks, deletions struck
    // through. Cleared when there are no pending ops (review == working): plain
    // editing. `materialize_review` short-circuits to the working text when no
    // pending op is in scope, so this is cheap on a clean buffer.
    let review_text = log.materialize_review(doc_id, session.as_deref()).map(|c| c.text).ok();
    buffer.agent_proposal = match (&working_text, &review_text) {
        (Some(working), Some(review)) if review != working => Some(review.clone()),
        _ => None,
    };

    // Step 4 — keep `loaded_text`/`loaded_hash` synced to the disk-canonical
    // `accepted` state. `is_dirty` is `hash(editor.doc) != loaded_hash`, and
    // `editor.doc` is `materialize_working`; so dirty should mean "working has
    // uncommitted edits beyond accepted." Without this sync, `loaded_hash` stays
    // pinned to the bytes read at open, so any out-of-band advance of `accepted`
    // — an accepted agent op, an external edit, or even a bootstrap
    // normalization difference — leaves the buffer falsely marked dirty though
    // the user never typed. `accepted` == the on-disk `.md`, so tracking it
    // keeps `loaded_text` equal to disk. When there are no uncommitted user
    // edits, `working == accepted`, so reuse `working_text` and skip a third
    // full materialization on the (common) clean path.
    let accepted_text = if log.has_working_edits(doc_id).unwrap_or(false) {
        log.materialize_accepted(doc_id).map(|c| c.text).ok()
    } else {
        working_text
    };
    if let Some(accepted) = accepted_text
        && accepted != buffer.loaded_text
    {
        buffer.loaded_hash = hiker_core::hash_string(&accepted);
        buffer.loaded_text = accepted;
    }
}

#[cfg(test)]
mod tests {
    use super::{change_set_between, change_set_edits, WorkingEdit};
    use editor_core::change::Set;

    fn one(edits: &[WorkingEdit]) -> (usize, usize, &str) {
        assert_eq!(edits.len(), 1, "expected exactly one edit");
        (edits[0].byte_start, edits[0].delete_len, edits[0].insert.as_str())
    }

    // ── change_set_between ──────────────────────────────────────────────
    //
    // The reverse step maps the cursor through `change_set_between(old, new)`
    // (CodeMirror's `ChangeSet.mapPos`) when `working` advances out-of-band.
    // These pin the position-mapping behaviour the cursor relies on.

    #[test]
    fn change_set_maps_cursor_across_insertion_above() {
        // An insertion above the cursor shifts the cursor forward by its length.
        let old = "alpha\ngamma\n";
        let new = "alpha\nBETA\ngamma\n"; // "BETA\n" inserted at byte 6
        let cs = change_set_between(old, new);
        // The 'g' of gamma is at byte 6 in `old`, byte 11 in `new`.
        assert_eq!(cs.map_pos(6, editor_core::anchor::Bias::Right), 11);
        // A position before the insertion is unchanged.
        assert_eq!(cs.map_pos(3, editor_core::anchor::Bias::Right), 3);
    }

    #[test]
    fn change_set_maps_cursor_across_deletion_above() {
        // A deletion above the cursor pulls the cursor back by its length.
        let old = "head\nXYZ\ntail\n";
        let new = "head\ntail\n"; // "XYZ\n" (bytes 5..9) removed
        let cs = change_set_between(old, new);
        // 't' of tail: byte 9 in `old` → byte 5 in `new`.
        assert_eq!(cs.map_pos(9, editor_core::anchor::Bias::Right), 5);
    }

    #[test]
    fn change_set_is_identity_when_unchanged() {
        let s = "alpha\nbeta\ngamma\n";
        let cs = change_set_between(s, s);
        for off in [0usize, 1, 6, 11, s.len()] {
            assert_eq!(cs.map_pos(off, editor_core::anchor::Bias::Right), off, "offset {off}");
        }
    }

    #[test]
    fn insert_at_offset_tracks_retain_cursor() {
        // "abc" → "abXc": retain 2, insert "X", retain 1.
        let set = Set::of(3, [(2..2, "X".to_string())]);
        let edits = change_set_edits(&set);
        assert_eq!(one(&edits), (2, 0, "X"));
    }

    #[test]
    fn delete_records_span_at_cursor() {
        // "abcd" → "ad": retain 1, delete 2, retain 1.
        let set = Set::of(4, [(1..3, String::new())]);
        let edits = change_set_edits(&set);
        assert_eq!(one(&edits), (1, 2, ""));
    }

    #[test]
    fn replace_emits_delete_then_insert_at_same_offset() {
        // "abcd" → "aXYd": retain 1, delete 2, insert "XY", retain 1.
        let set = Set::of(4, [(1..3, "XY".to_string())]);
        let edits = change_set_edits(&set);
        assert_eq!(edits.len(), 2);
        assert_eq!((edits[0].byte_start, edits[0].delete_len, edits[0].insert.as_str()), (1, 2, ""));
        assert_eq!((edits[1].byte_start, edits[1].delete_len, edits[1].insert.as_str()), (1, 0, "XY"));
    }

    #[test]
    fn identity_change_set_yields_no_edits() {
        let set = Set::empty(5);
        assert!(change_set_edits(&set).is_empty());
    }
}

#[cfg(test)]
mod merge_scenarios {
    //! End-to-end coverage for the binding driven against a *real* `OpLog` +
    //! real `Buffer`: each test simulates the widget applying a keystroke
    //! (mutate `editor.doc`, build the matching change set), runs the actual
    //! [`apply_binding`] forward/reverse/overlay seam, and asserts the working
    //! / review / overlay / disk state — then walks the scenario through to
    //! accept / reject + commit. This is the integration layer the per-frame
    //! `run` drives, minus the `AppState` container; it guards the
    //! user-edits-while-the-agent-edits flows that pure-function tests can't
    //! reach (the seam where the coordinate-mapping bug lived).

    use std::sync::Arc;

    use editor_core::change::Set;
    use editor_core::transaction::Transaction;
    use hiker_core::ops::op_writes::{self, AgentEdit};
    use hiker_core::oplog::OpLog;
    use hiker_core::vault::Vault;
    use tempfile::TempDir;

    use super::apply_binding;
    use crate::buffer::Buffer;

    /// A real op-log-backed vault + editable `Buffer` for `a.md`, seeded from
    /// `initial` on disk exactly as `bootstrap` does at vault open.
    struct Fixture {
        td: TempDir,
        log: Arc<OpLog>,
        vault: Vault,
        buffer: Buffer,
        doc_id: String,
    }

    fn setup(initial: &str) -> Fixture {
        let td = TempDir::new().unwrap();
        std::fs::write(td.path().join("a.md"), initial).unwrap();
        let vault = Vault::open(td.path()).unwrap();
        let log = Arc::new(OpLog::open(td.path()).unwrap());
        op_writes::bootstrap(&vault, &log).unwrap();
        let doc_id = log.doc_id_for_path("a.md").unwrap().unwrap();
        let buffer = Buffer::with_config_and_vault(
            "a.md".to_string(),
            initial,
            hiker_core::hash_string(initial),
            None,
            None,
        );
        Fixture { td, log, vault, buffer, doc_id }
    }

    impl Fixture {
        /// One binding frame with no user input — the agent-staged / accepted /
        /// externally-edited refresh case (reverse + overlay only).
        fn frame(&mut self) {
            apply_binding(&self.log, &mut self.buffer, &self.doc_id, "a.md", &[]);
        }

        /// Simulate the widget applying a single-range edit this frame: replace
        /// `range` in the current buffer (review coords) with `text`, mutate
        /// `editor.doc` as the widget would, then run the binding with the
        /// matching change set — so the forward step maps review→working off the
        /// pre-edit materializations, just like the live per-frame path.
        fn type_edit(&mut self, range: std::ops::Range<usize>, text: &str) {
            let before = self.buffer.editor.doc.to_string();
            let set = Set::of(before.len(), [(range.clone(), text.to_string())]);
            let after = format!("{}{}{}", &before[..range.start], text, &before[range.end..]);
            self.buffer.set_doc_clamping_selection(&after);
            let txns = [Transaction::new(set)];
            apply_binding(&self.log, &mut self.buffer, &self.doc_id, "a.md", &txns);
        }

        fn buffer_text(&self) -> String {
            self.buffer.editor.doc.to_string()
        }
        fn working(&self) -> String {
            self.log.materialize_working(&self.doc_id).unwrap().text
        }
        fn accepted(&self) -> String {
            self.log.materialize_accepted(&self.doc_id).unwrap().text
        }
        fn disk(&self) -> String {
            std::fs::read_to_string(self.td.path().join("a.md")).unwrap()
        }
        fn stage_agent(&self, old: &str, new: &str) -> Vec<String> {
            op_writes::stage_agent_edits(
                &self.log,
                &self.vault,
                "claude-code",
                "mcp-tool-call",
                "a.md",
                &[AgentEdit { old_str: Some(old.to_string()), new_str: new.to_string() }],
            )
            .unwrap()
            .op_ids
        }
        fn accept(&self, op_ids: &[String]) {
            op_writes::flip_op_status(&self.log, "a.md", op_ids, true).unwrap();
        }
        fn reject(&self, op_ids: &[String]) {
            op_writes::flip_op_status(&self.log, "a.md", op_ids, false).unwrap();
        }
        fn commit(&self) {
            assert!(self.log.commit_working(&self.doc_id).unwrap(), "expected a working commit");
        }

        /// Main-cursor head offset (where the next keystroke lands).
        fn cursor(&self) -> usize {
            self.buffer.editor.selection.main().head.offset()
        }
        fn set_cursor(&mut self, pos: usize) {
            self.buffer.editor.selection = editor_core::selection::Selection::single(pos);
        }

        /// Type one character at the current cursor *exactly as the widget
        /// does*: build the insert change set, apply it via `Editor::apply`
        /// (which advances the doc AND maps the selection forward), then run the
        /// binding — which may, via its reverse step, re-point the doc and clamp
        /// the selection. Tracking the cursor across a run of these is what
        /// surfaces the right-to-left / cursor-reset class of bug.
        fn type_char(&mut self, ch: &str) {
            let cursor = self.cursor();
            let set = Set::of(
                self.buffer.editor.doc.len_bytes(),
                [(cursor..cursor, ch.to_string())],
            );
            let txn = Transaction::new(set);
            self.buffer.editor = self.buffer.editor.apply(txn.clone());
            apply_binding(&self.log, &mut self.buffer, &self.doc_id, "a.md", &[txn]);
        }

        /// The agent's proposal overlay (`materialize_review`) the binding
        /// stashed this frame — `None` when there are no pending ops. In Model 1
        /// the agent's edits live here, NOT in the buffer text.
        fn proposal(&self) -> Option<String> {
            self.buffer.agent_proposal.clone()
        }

        /// Undo as the widget does it: pop the editor's history (carrying the
        /// inverse change set), apply it to the editor, and run the binding with
        /// that change set so the `working` layer mirrors the undo. Catches the
        /// "Ctrl+Z does nothing" bug — a tx-less undo would be reverted by the
        /// reverse step.
        fn undo(&mut self) {
            if let Some((next, tx)) = self.buffer.editor.undo_with_changes() {
                self.buffer.editor = next;
                apply_binding(&self.log, &mut self.buffer, &self.doc_id, "a.md", &[tx]);
            }
        }
        fn redo(&mut self) {
            if let Some((next, tx)) = self.buffer.editor.redo_with_changes() {
                self.buffer.editor = next;
                apply_binding(&self.log, &mut self.buffer, &self.doc_id, "a.md", &[tx]);
            }
        }
    }

    #[test]
    fn accepting_agent_edit_above_cursor_carries_the_cursor() {
        // In Model 1 the agent edit is an overlay, not buffer text — so STAGING
        // it must leave the buffer and cursor untouched. ACCEPTING folds it into
        // the buffer (working), and the reverse step must MAP the cursor through
        // that change (not clamp it) so the user keeps typing at the end of
        // "three!" instead of landing inside the agent's new text — the
        // cursor-jump that read as scrambled / reversed typing.
        let mut fx = setup("one\ntwo\nthree\nfour\nfive\n");
        fx.set_cursor(13);
        fx.type_char("!");
        assert_eq!(fx.buffer_text(), "one\ntwo\nthree!\nfour\nfive\n");
        assert_eq!(fx.cursor(), 14, "cursor sits just after \"three!\"");
        // Agent proposes an edit above the cursor: "two" → "TWO-LONGER" (+7).
        let ops = fx.stage_agent("two", "TWO-LONGER");
        fx.frame();
        assert_eq!(fx.buffer_text(), "one\ntwo\nthree!\nfour\nfive\n", "stage leaves the buffer alone");
        assert_eq!(fx.cursor(), 14, "stage leaves the cursor alone");
        assert_eq!(fx.proposal().as_deref(), Some("one\nTWO-LONGER\nthree!\nfour\nfive\n"));
        // Accept folds the edit into working → buffer changes, cursor rides it.
        fx.accept(&ops);
        fx.frame();
        assert_eq!(fx.buffer_text(), "one\nTWO-LONGER\nthree!\nfour\nfive\n");
        let want = fx.buffer_text().find("three!").unwrap() + "three!".len();
        assert_eq!(fx.cursor(), want, "cursor rode the accepted insertion above it (mapped, not clamped)");
    }

    #[test]
    fn typing_with_a_pending_agent_edit_advances_cursor_left_to_right() {
        // Repro for the "typing went right-to-left" report. With a pending agent
        // edit present (as an overlay), the user types on the working buffer.
        // Each char must land to the RIGHT of the last — the forward step
        // applies directly to working with no remap, so the cursor never stalls.
        let mut fx = setup("one\ntwo\nthree\nfour\nfive\n");
        let _a = fx.stage_agent("five", "FIVE");
        fx.frame();
        // Buffer shows working (agent edit is the overlay, not buffer text).
        assert_eq!(fx.buffer_text(), "one\ntwo\nthree\nfour\nfive\n");
        assert_eq!(fx.proposal().as_deref(), Some("one\ntwo\nthree\nfour\nFIVE\n"));
        fx.set_cursor(13);
        fx.type_char("X");
        fx.type_char("Y");
        fx.type_char("Z");
        assert_eq!(fx.buffer_text(), "one\ntwo\nthreeXYZ\nfour\nfive\n", "chars land contiguously L→R");
        assert_eq!(fx.cursor(), 16, "cursor advanced past all three typed chars");
        // The proposal tracks the user's edits too (working + pending).
        assert_eq!(fx.proposal().as_deref(), Some("one\ntwo\nthreeXYZ\nfour\nFIVE\n"));
    }

    #[test]
    fn typing_with_a_pending_agent_edit_above_the_cursor() {
        // The pending agent edit is above the cursor, but it's an OVERLAY — the
        // buffer (working) is unchanged and the cursor is a plain working offset,
        // so typing advances normally with no coordinate translation at all.
        let mut fx = setup("one\ntwo\nthree\nfour\nfive\n");
        let _a = fx.stage_agent("two", "TWO-EDITED");
        fx.frame();
        assert_eq!(fx.buffer_text(), "one\ntwo\nthree\nfour\nfive\n", "agent edit is an overlay, not in the buffer");
        // Cursor at end of "three" in WORKING: "one\n"=4 + "two\n"=4 + "three"=5 = 13.
        fx.set_cursor(13);
        fx.type_char("X");
        fx.type_char("Y");
        fx.type_char("Z");
        assert_eq!(fx.buffer_text(), "one\ntwo\nthreeXYZ\nfour\nfive\n");
        assert_eq!(fx.cursor(), 16);
        assert_eq!(fx.working(), "one\ntwo\nthreeXYZ\nfour\nfive\n");
        assert_eq!(fx.proposal().as_deref(), Some("one\nTWO-EDITED\nthreeXYZ\nfour\nfive\n"));
    }

    #[test]
    fn agent_edits_line_above_an_existing_user_edit_then_user_types() {
        // The exact ordering from the bug report: (1) agent edits a random line,
        // (2) the user edits a line, (3) the agent edits the line ABOVE the one
        // the user just edited, (4) the user keeps typing. In Model 1 the agent
        // edits stay in the overlay (proposal) and never disturb the buffer or
        // the cursor; the user's typing stays L→R throughout.
        let mut fx = setup("one\ntwo\nthree\nfour\nfive\n");
        // (1) agent edits a "random" lower line.
        let _a = fx.stage_agent("five", "FIVE");
        fx.frame();
        // (2) user edits line "three" → append "!" (working offset 13).
        fx.set_cursor(13);
        fx.type_char("!");
        assert_eq!(fx.buffer_text(), "one\ntwo\nthree!\nfour\nfive\n", "buffer = working");
        assert_eq!(fx.cursor(), 14, "cursor just after \"three!\"");
        // (3) agent now proposes an edit on the line ABOVE the user's edit
        //     ("two" → "SECOND"), staged while the user's working edit exists.
        let _b = fx.stage_agent("two", "SECOND");
        fx.frame();
        // The buffer and cursor are untouched — the agent edit is an overlay.
        assert_eq!(fx.buffer_text(), "one\ntwo\nthree!\nfour\nfive\n", "agent edits stay in the overlay");
        assert_eq!(fx.cursor(), 14, "the overlay never moves the cursor");
        assert_eq!(fx.proposal().as_deref(), Some("one\nSECOND\nthree!\nfour\nFIVE\n"), "both agent edits + the user edit in the proposal");
        // (4) user keeps typing — characters append after "three!", L→R.
        fx.type_char("X");
        fx.type_char("Y");
        assert_eq!(fx.buffer_text(), "one\ntwo\nthree!XY\nfour\nfive\n", "typing stays L→R on working");
        assert_eq!(fx.cursor(), 16);
        assert_eq!(fx.proposal().as_deref(), Some("one\nSECOND\nthree!XY\nfour\nFIVE\n"));
    }

    #[test]
    fn accepting_agent_delete_above_cursor_pulls_cursor_back() {
        // Accepting an agent edit that SHRINKS text above the cursor pulls the
        // cursor back by the deleted length (mapped through the reverse step,
        // not clamped to the old offset).
        let mut fx = setup("alpha\nbravo\ncharlie\n");
        // Cursor at start of "charlie": "alpha\n"=6 + "bravo\n"=6 = 12.
        fx.set_cursor(12);
        let ops = fx.stage_agent("bravo", "b"); // line 2 shrinks by 4 bytes
        fx.frame();
        assert_eq!(fx.buffer_text(), "alpha\nbravo\ncharlie\n", "stage doesn't touch the buffer");
        assert_eq!(fx.cursor(), 12, "stage doesn't move the cursor");
        fx.accept(&ops);
        fx.frame();
        assert_eq!(fx.buffer_text(), "alpha\nb\ncharlie\n");
        let want = fx.buffer_text().find("charlie").unwrap();
        assert_eq!(fx.cursor(), want, "cursor pulled back by the accepted deletion above it");
    }

    #[test]
    fn agent_proposal_is_an_overlay_not_in_the_buffer() {
        // In Model 1 a staged agent edit is NOT folded into the buffer text; it
        // surfaces as the proposal overlay on the next frame, leaving the buffer
        // (working) untouched until the user accepts.
        let mut fx = setup("alpha\nbeta\ngamma\n");
        let ops = fx.stage_agent("beta", "BETA");
        assert!(!ops.is_empty());
        assert_eq!(fx.buffer_text(), "alpha\nbeta\ngamma\n");
        fx.frame();
        assert_eq!(fx.buffer_text(), "alpha\nbeta\ngamma\n", "buffer still shows working, not the agent edit");
        assert_eq!(fx.proposal().as_deref(), Some("alpha\nBETA\ngamma\n"), "agent edit lives in the proposal overlay");
        assert_eq!(fx.accepted(), "alpha\nbeta\ngamma\n");
    }

    #[test]
    fn user_edit_below_agent_edit_accept_then_commit_lands_both() {
        // The canonical bug the user hit: edit a line below the one the agent
        // proposed; accepting must keep both edits.
        let mut fx = setup("line one\nline two\nline three\n");
        let ops = fx.stage_agent("line two", "LINE TWO");
        fx.frame();
        assert_eq!(fx.buffer_text(), "line one\nline two\nline three\n", "buffer = working");
        assert_eq!(fx.proposal().as_deref(), Some("line one\nLINE TWO\nline three\n"));
        // Append "X" at the end of line three (working byte 28).
        fx.type_edit(28..28, "X");
        assert_eq!(fx.buffer_text(), "line one\nline two\nline threeX\n");
        assert_eq!(fx.working(), "line one\nline two\nline threeX\n");
        assert_eq!(
            fx.proposal().as_deref(),
            Some("line one\nLINE TWO\nline threeX\n"),
            "proposal = working + pending"
        );
        // Accept the agent op → it folds into accepted AND replays onto working.
        fx.accept(&ops);
        fx.frame();
        assert_eq!(fx.buffer_text(), "line one\nLINE TWO\nline threeX\n", "both edits now in the buffer");
        assert_eq!(fx.accepted(), "line one\nLINE TWO\nline three\n", "disk has agent edit only");
        assert!(fx.proposal().is_none(), "no pending left → overlay cleared");
        // Save folds the user edit onto disk too.
        fx.commit();
        assert_eq!(fx.disk(), "line one\nLINE TWO\nline threeX\n");
    }

    #[test]
    fn user_edit_above_agent_edit_both_survive_accept_and_commit() {
        let mut fx = setup("line one\nline two\nline three\n");
        let ops = fx.stage_agent("line three", "LINE THREE");
        fx.frame();
        assert_eq!(fx.buffer_text(), "line one\nline two\nline three\n", "buffer = working");
        // Replace "one" (bytes 5..8) on line one — above the agent's edit.
        fx.type_edit(5..8, "ONE");
        assert_eq!(fx.buffer_text(), "line ONE\nline two\nline three\n");
        assert_eq!(fx.working(), "line ONE\nline two\nline three\n");
        assert_eq!(fx.proposal().as_deref(), Some("line ONE\nline two\nLINE THREE\n"));
        fx.accept(&ops);
        fx.frame();
        assert_eq!(fx.buffer_text(), "line ONE\nline two\nLINE THREE\n");
        assert_eq!(fx.accepted(), "line one\nline two\nLINE THREE\n");
        fx.commit();
        assert_eq!(fx.disk(), "line ONE\nline two\nLINE THREE\n");
    }

    #[test]
    fn reject_agent_edit_keeps_user_edit() {
        let mut fx = setup("alpha\nbeta\ngamma\n");
        let ops = fx.stage_agent("gamma", "GAMMA");
        fx.frame();
        assert_eq!(fx.buffer_text(), "alpha\nbeta\ngamma\n", "buffer = working");
        // User edits "alpha" (bytes 0..5) — a disjoint region from the agent.
        fx.type_edit(0..5, "ALPHA");
        assert_eq!(fx.buffer_text(), "ALPHA\nbeta\ngamma\n");
        assert_eq!(fx.proposal().as_deref(), Some("ALPHA\nbeta\nGAMMA\n"));
        // Reject the agent op → overlay clears; the user edit stays.
        fx.reject(&ops);
        fx.frame();
        assert_eq!(fx.buffer_text(), "ALPHA\nbeta\ngamma\n", "agent edit gone, user edit kept");
        assert!(fx.proposal().is_none(), "no pending left → no overlay");
        assert_eq!(fx.accepted(), "alpha\nbeta\ngamma\n", "reject never touched accepted/disk");
    }

    #[test]
    fn undo_reverts_working_and_is_not_clobbered_by_reverse() {
        // The reported Ctrl+Z bug at the binding level: typing then undo must
        // leave both the buffer AND the working layer reverted — the undo's
        // change set reaches working, so the reverse step has nothing to revert.
        let mut fx = setup("hello world\n");
        fx.set_cursor(11); // end of "world"
        fx.type_char("!");
        assert_eq!(fx.buffer_text(), "hello world!\n");
        assert_eq!(fx.working(), "hello world!\n", "type lands on working");
        fx.undo();
        assert_eq!(fx.buffer_text(), "hello world\n", "undo reverts the buffer");
        assert_eq!(fx.working(), "hello world\n", "undo reverts the working layer too");
        assert_eq!(fx.cursor(), 11, "cursor back where the char was typed");
    }

    #[test]
    fn redo_reapplies_to_working() {
        let mut fx = setup("hello world\n");
        fx.set_cursor(11);
        fx.type_char("!");
        fx.undo();
        assert_eq!(fx.working(), "hello world\n");
        fx.redo();
        assert_eq!(fx.buffer_text(), "hello world!\n", "redo reapplies to the buffer");
        assert_eq!(fx.working(), "hello world!\n", "redo reapplies to working");
    }

    #[test]
    fn undo_with_a_pending_agent_edit_present_only_reverts_the_user_edit() {
        // Undo while an agent proposal overlay is live: the user's typed char is
        // undone on working; the agent's pending op is untouched (it's not in
        // the buffer or the user's history), and the proposal tracks the new
        // working.
        let mut fx = setup("alpha\nbeta\ngamma\n");
        let _ops = fx.stage_agent("gamma", "GAMMA");
        fx.frame();
        assert_eq!(fx.buffer_text(), "alpha\nbeta\ngamma\n", "buffer = working");
        fx.set_cursor(5); // end of "alpha"
        fx.type_char("!");
        assert_eq!(fx.working(), "alpha!\nbeta\ngamma\n");
        assert_eq!(fx.proposal().as_deref(), Some("alpha!\nbeta\nGAMMA\n"));
        fx.undo();
        assert_eq!(fx.working(), "alpha\nbeta\ngamma\n", "only the user edit undone");
        assert_eq!(fx.proposal().as_deref(), Some("alpha\nbeta\nGAMMA\n"), "agent op still pending");
    }

    #[test]
    fn rejecting_an_agent_edit_does_not_dirty_the_buffer() {
        // Repro for "buffer marked dirty after rejecting an agent suggestion
        // I never edited." The agent edit is an overlay; reject drops the
        // pending op and touches neither `working` nor `accepted`, so the
        // buffer must stay clean.
        let mut fx = setup("alpha\nbeta\ngamma\n");
        fx.frame();
        assert!(!fx.buffer.is_dirty(), "clean on open");
        let ops = fx.stage_agent("beta", "BETA");
        fx.frame();
        assert!(!fx.buffer.is_dirty(), "an agent proposal is an overlay, not a buffer edit");
        fx.reject(&ops);
        fx.frame();
        assert!(!fx.buffer.is_dirty(), "rejecting an agent edit leaves the buffer clean");
    }

    #[test]
    fn accepting_an_agent_edit_does_not_dirty_the_buffer() {
        // Accept advances `accepted` (and disk); the buffer must follow without
        // reading dirty (the user has no uncommitted edits of their own).
        let mut fx = setup("alpha\nbeta\ngamma\n");
        let ops = fx.stage_agent("beta", "BETA");
        fx.frame();
        fx.accept(&ops);
        fx.frame();
        assert_eq!(fx.buffer_text(), "alpha\nBETA\ngamma\n");
        assert!(!fx.buffer.is_dirty(), "accepting an agent edit leaves the buffer clean (it's on disk)");
    }

    #[test]
    fn a_real_user_edit_does_mark_the_buffer_dirty() {
        // The flip side: a genuine uncommitted user edit MUST read dirty until
        // saved (commit_working).
        let mut fx = setup("hello world\n");
        fx.frame();
        assert!(!fx.buffer.is_dirty());
        fx.type_edit(11..11, "!");
        assert!(fx.buffer.is_dirty(), "an unsaved user edit is dirty");
        fx.commit();
        fx.frame();
        assert!(!fx.buffer.is_dirty(), "saving clears dirty");
    }

    #[test]
    fn plain_typing_no_pending_tracks_working_with_no_overlay() {
        // With no agent ops the buffer is plain editing: the keystroke lands 1:1
        // on working and no proposal overlay is set.
        let mut fx = setup("hello world\n");
        fx.type_edit(11..11, "!");
        assert_eq!(fx.buffer_text(), "hello world!\n");
        assert_eq!(fx.working(), "hello world!\n", "working tracks the keystroke 1:1");
        assert!(fx.proposal().is_none(), "no pending → no overlay");
        fx.commit();
        assert_eq!(fx.disk(), "hello world!\n");
    }

    #[test]
    fn two_agent_ops_accept_one_reject_other_around_a_user_edit() {
        // Two disjoint agent ops + a user edit between them: accept the first,
        // reject the second; the accepted one + the user edit survive, the
        // rejected one vanishes.
        let mut fx = setup("alpha\nbeta\ngamma\n");
        let op_alpha = fx.stage_agent("alpha", "ALPHA");
        let op_gamma = fx.stage_agent("gamma", "GAMMA");
        fx.frame();
        assert_eq!(fx.buffer_text(), "alpha\nbeta\ngamma\n", "buffer = working; both edits in the overlay");
        assert_eq!(fx.proposal().as_deref(), Some("ALPHA\nbeta\nGAMMA\n"));
        // User edits the middle line "beta" (bytes 6..10) → "BETA".
        fx.type_edit(6..10, "BETA");
        assert_eq!(fx.buffer_text(), "alpha\nBETA\ngamma\n");
        assert_eq!(fx.proposal().as_deref(), Some("ALPHA\nBETA\nGAMMA\n"));
        fx.accept(&op_alpha);
        fx.reject(&op_gamma);
        fx.frame();
        assert_eq!(fx.buffer_text(), "ALPHA\nBETA\ngamma\n", "alpha accepted, gamma rejected, beta kept");
        assert_eq!(fx.accepted(), "ALPHA\nbeta\ngamma\n", "only the accepted op on disk");
        assert!(fx.proposal().is_none());
        fx.commit();
        assert_eq!(fx.disk(), "ALPHA\nBETA\ngamma\n");
    }
}
