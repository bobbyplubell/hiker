//! Inline git-conflict resolver overlay for the editor buffer panel — the UI
//! half of [[spec:git-conflict-inline-markers]].
//!
//! When a buffer's text carries `git merge` conflict markers (the git
//! transport's conflict surface; see [`super::gitmerge`]), this builds
//! the per-frame decoration layer that:
//! - tints the ours / base / theirs sections of each region distinctly so the
//!   markers read as a structured conflict rather than raw text, and
//! - attaches an inline action row above each region with **Accept Current**,
//!   **Accept Incoming**, **Accept Both** buttons.
//!
//! It mirrors the inline-widget pattern of [`super::diff_overlay`] /
//! [`super::patch_review`]: a `Set` of decorations plus a `click_map` from each
//! button's widget id to the [`ConflictAction`] it dispatches. Clicking a button
//! rewrites just that region in the buffer (markers removed, chosen side kept)
//! via the same Transaction path typing uses, so the buffer goes dirty and saves
//! — and the already-built git sync round finalizes the merge once the saved file
//! has no markers left. The user can equally hand-edit the markers; that's just
//! text, no special handling.
//!
//! The buffer is already forced to render as source (not markdown live-preview)
//! while it has markers — that suppression lives in
//! [`super::decorations::rebuild_editor_layers`] and keys off the same predicate
//! this module parses against, so the markers stay visible + editable underneath
//! these decorations.
//!
//! status: git-conflict-inline-markers

use std::collections::HashMap;
use std::ops::Range;

use editor_core::decoration::{
    ActionButton, ActionButtonStyle, ActionTone, BlockDeco, BlockKind, BlockSide, Color,
    Decoration, LineStyle, Set,
};
use editor_core::rangeset::RangeSet;
use smol_str::SmolStr;

use super::gitmerge::{has_conflict_markers, parse_conflicts, resolve_region, Choice, ConflictRegion};
use crate::state::{AppState, ToastLevel};

/// Widget-id namespace for conflict-resolver buttons. A high, distinct base (vs
/// the diff-overlay hunk-widget range) so the panel's `WidgetClick` router keeps
/// telling conflict buttons apart from every other inline-widget id by id alone.
const CONFLICT_WIDGET_BASE: u64 = 0xFFFF_0003_0000_0000;

/// Faint line tints for the three sides — chosen to read against both light and
/// dark themes (low alpha) and to echo VS Code's green-ours / red-theirs accent.
const OURS_BG: Color = Color::rgba(0x33, 0xAA, 0x55, 0x22);
const BASE_BG: Color = Color::rgba(0x88, 0x88, 0x88, 0x18);
const THEIRS_BG: Color = Color::rgba(0x33, 0x77, 0xCC, 0x22);

/// A resolved choice for one conflict region: the byte range of the whole region
/// in the buffer at overlay-build time, plus which side to keep. The panel
/// re-parses the live buffer before applying (the region's bytes may have shifted
/// from hand-edits since this overlay was built), matching it by the marker text
/// rather than trusting the stale range blindly.
#[derive(Debug, Clone)]
pub struct ConflictAction {
    /// The whole-region byte range as parsed when the overlay was built.
    pub region: Range<usize>,
    /// Which side the clicked button keeps.
    pub choice: Choice,
}

/// The per-frame conflict-resolver overlay: decorations to push onto the editor
/// stack plus the click-map resolving each button id to its [`ConflictAction`].
pub struct ConflictOverlay {
    pub decorations: Set,
    pub click_map: HashMap<u64, ConflictAction>,
}

impl AppState {
    /// Build the conflict-resolver overlay for the buffer at `path`, or `None`
    /// when the buffer is absent or carries no complete conflict region.
    ///
    /// status: git-conflict-inline-markers
    #[must_use]
    pub fn conflict_overlay_for(&self, path: &str) -> Option<ConflictOverlay> {
        let buffer = self.session.buffers.get(path)?;
        let text = buffer.editor.doc.to_string();
        // Fast reject before the full parse: most buffers carry no markers.
        if !has_conflict_markers(&text) {
            return None;
        }
        let regions = parse_conflicts(&text);
        if regions.is_empty() {
            return None;
        }
        Some(build_overlay(&buffer.editor.doc, &text, &regions))
    }
}

/// Assemble the decoration `Set` + click-map for the parsed `regions`.
fn build_overlay(doc: &editor_core::rope::Rope, text: &str, regions: &[ConflictRegion]) -> ConflictOverlay {
    let mut entries: Vec<(Range<usize>, Decoration)> = Vec::new();
    let mut click_map: HashMap<u64, ConflictAction> = HashMap::new();
    let mut next_id = CONFLICT_WIDGET_BASE;

    for region in regions {
        tint_side(&mut entries, doc, text, &region.ours, OURS_BG);
        if let Some(base) = &region.base {
            tint_side(&mut entries, doc, text, base, BASE_BG);
        }
        tint_side(&mut entries, doc, text, &region.theirs, THEIRS_BG);
        attach_action_row(&mut entries, doc, &mut click_map, &mut next_id, region);
    }

    ConflictOverlay { decorations: RangeSet::from_iter(entries), click_map }
}

/// Push a whole-line background tint over every line the byte range `side`
/// touches. An empty side (a delete-vs-edit half with no content) contributes no
/// tint. Line-oriented so the tint reads as a band, like the diff overlay's
/// added/removed line backgrounds.
fn tint_side(
    entries: &mut Vec<(Range<usize>, Decoration)>,
    doc: &editor_core::rope::Rope,
    text: &str,
    side: &Range<usize>,
    bg: Color,
) {
    if side.start >= side.end {
        return;
    }
    let first_line = doc.byte_to_line(side.start.min(text.len()));
    // The content range ends just before the next marker line's start, i.e. at a
    // line boundary; step back one byte so we land on the last content line, not
    // the marker line below it.
    let last_byte = side.end.saturating_sub(1).min(text.len());
    let last_line = doc.byte_to_line(last_byte);
    for line in first_line..=last_line {
        let (ls, le) = line_bytes(doc, line);
        entries.push((
            ls..le,
            Decoration::Line(LineStyle { bg: Some(bg), ..LineStyle::default() }),
        ));
    }
}

/// The `[start, end)` byte range of physical line `line`, end clamped to the
/// document length.
fn line_bytes(doc: &editor_core::rope::Rope, line: usize) -> (usize, usize) {
    let start = doc.line_to_byte(line);
    let end = if line + 1 < doc.len_lines() {
        doc.line_to_byte(line + 1)
    } else {
        doc.len_bytes()
    };
    (start, end)
}

/// Attach the Accept Current / Incoming / Both action row above the region's
/// head (the `<<<<<<<` line) and register the three button ids in `click_map`.
fn attach_action_row(
    entries: &mut Vec<(Range<usize>, Decoration)>,
    doc: &editor_core::rope::Rope,
    click_map: &mut HashMap<u64, ConflictAction>,
    next_id: &mut u64,
    region: &ConflictRegion,
) {
    let current_id = *next_id;
    let incoming_id = *next_id + 1;
    let both_id = *next_id + 2;
    *next_id += 3;
    let mk = |choice: Choice| ConflictAction { region: region.region.clone(), choice };
    click_map.insert(current_id, mk(Choice::Current));
    click_map.insert(incoming_id, mk(Choice::Incoming));
    click_map.insert(both_id, mk(Choice::Both));

    let row = BlockDeco {
        side: BlockSide::Above,
        height: 24.0,
        kind: BlockKind::ActionRow {
            label: SmolStr::new_static("merge conflict"),
            glyph: None,
            tone: ActionTone::Conflicted,
            buttons: vec![
                ActionButton {
                    id: current_id,
                    label: SmolStr::new_static("Accept Current"),
                    style: ActionButtonStyle::Primary,
                    enabled: true,
                },
                ActionButton {
                    id: incoming_id,
                    label: SmolStr::new_static("Accept Incoming"),
                    style: ActionButtonStyle::Danger,
                    enabled: true,
                },
                ActionButton {
                    id: both_id,
                    label: SmolStr::new_static("Accept Both"),
                    style: ActionButtonStyle::Neutral,
                    enabled: true,
                },
            ],
        },
    };
    // Anchor the row at the start of the region's first line (the `<<<<<<<`
    // line), `Above` it, so the buttons sit directly over the conflict head.
    let head_line = doc.byte_to_line(region.region.start.min(doc.len_bytes()));
    let anchor = doc.line_to_byte(head_line.min(doc.len_lines().saturating_sub(1)));
    entries.push((anchor..anchor, Decoration::Block(row)));
}

/// Route this frame's conflict-resolver button clicks (`git-conflict-inline-
/// markers`): each Accept Current/Incoming/Both button maps to a region + chosen
/// side; `resolve_conflict_region` rewrites just that region in the buffer via a
/// Transaction (markers removed) and mirrors it into the `working` layer like any
/// other edit, so the buffer goes dirty and saves through the normal path. Once
/// the saved file has no markers left, the already-built git sync round finalizes
/// the merge. At most one region is resolved per frame: applying a resolution
/// shifts every later region's byte offsets, so the remaining buttons' stale
/// ranges are re-derived from the freshly-parsed buffer next frame.
pub(super) fn dispatch_conflict_clicks(
    app: &mut AppState,
    ctx: &egui::Context,
    path: &str,
    conflict: Option<&ConflictOverlay>,
    widget_clicks: &[u64],
) {
    let Some(cv) = conflict else { return };
    for id in widget_clicks {
        let Some(action) = cv.click_map.get(id) else { continue };
        app.resolve_conflict_region(path, action);
        ctx.request_repaint();
        break;
    }
}

/// Inline git-conflict resolver verb on `AppState` (`git-conflict-inline-
/// markers`). [`dispatch_conflict_clicks`] dispatches an Accept Current/Incoming/
/// Both button click here.
impl AppState {
    /// Resolve one conflict region in the buffer at `path` by `action.choice`,
    /// rewriting just that region (markers removed, chosen side kept) and
    /// mirroring the rewrite into the `working` layer so the buffer goes dirty
    /// and saves through the normal path.
    ///
    /// The live buffer is re-parsed first — hand-edits since the overlay was
    /// built may have shifted offsets — and the region is matched to the
    /// action's recorded start. If no region matches that start (a hand-edit
    /// removed or moved it), the resolve safely no-ops with an informative toast
    /// rather than splicing a stale range.
    ///
    /// The byte offsets of the splice are derived from `editor.doc` (the editable
    /// buffer), but the same offsets are also replayed onto the `working` layer
    /// so the editor binding's reverse step doesn't revert the change. That is
    /// only sound while `working` is byte-identical to `editor.doc`. Before
    /// touching `working` we VERIFY that invariant (`materialize_working ==
    /// editor.doc`); if they have diverged — e.g. a pending review overlay or an
    /// out-of-band advance shifted `working` — the offsets would index *different*
    /// bytes there and silently corrupt the saved file, so we abort safely
    /// (reverting the editor-doc splice) and tell the user instead.
    ///
    /// status: git-conflict-inline-markers
    pub(super) fn resolve_conflict_region(&mut self, path: &str, action: &ConflictAction) {
        let Some(buffer) = self.session.buffers.get_mut(path) else { return };
        let text = buffer.editor.doc.to_string();
        // Re-parse the live buffer and match the region to the action's recorded
        // start; if a hand-edit changed it, fall back to the recorded range.
        let region = parse_conflicts(&text)
            .into_iter()
            .find(|r| r.region.start == action.region.start);
        let Some(region) = region else {
            self.push_toast(
                "Resolve conflict: region no longer found (edited?)".to_string(),
                ToastLevel::Info,
            );
            return;
        };
        // `resolve_region` yields the whole resolved buffer; the kept text it
        // spliced over the region is the slice from the region start to the same
        // distance-from-end the unresolved suffix had. Deriving the edit from the
        // canonical whole-buffer resolution (rather than re-implementing the
        // splice here) keeps the editor doc and the `working` mirror in lockstep
        // with the tested resolution.
        let resolved_full = resolve_region(&text, &region, action.choice);
        let r = region.region.clone();
        let kept_end = resolved_full.len().saturating_sub(text.len() - r.end);
        let kept = resolved_full.get(r.start..kept_end).unwrap_or("").to_string();
        // The splice offsets `r` index `text` (== `editor.doc`). They are also
        // replayed onto the `working` layer below so the editor binding doesn't
        // revert the change — but that replay is only sound while `working` is
        // byte-identical to `editor.doc`. Verify that BEFORE mutating anything so
        // a divergent splice (which would corrupt the saved file by indexing
        // different bytes in `working`) is refused, not applied-then-reverted.
        // A buffer with no layered doc (disk-only) has no `working` layer to
        // diverge — its editor doc is the source of truth — so it skips the check.
        let log = self.vault_session.services.layered.clone();
        let doc_id = match log.doc_id_for_path(path) {
            Ok(id) => id,
            Err(e) => {
                self.push_toast(format!("Resolve conflict failed: {e}"), ToastLevel::Error);
                return;
            }
        };
        if let Some(doc_id) = &doc_id {
            let working = log.materialize_working(doc_id).map(|c| c.text).ok();
            if !working_matches_editor(working.as_deref(), &text) {
                self.push_toast(
                    "Resolve conflict: the buffer changed underneath this action — \
                     reopen the conflict and try again"
                        .to_string(),
                    ToastLevel::Info,
                );
                return;
            }
        }
        // Apply the splice as a single-range transaction on the editor doc so the
        // change shows this frame.
        let changes = editor_core::change::Set::of(
            buffer.editor.doc.len_bytes(),
            std::iter::once((r.clone(), kept.clone())),
        );
        let txn = editor_core::transaction::Transaction::new(changes)
            .with_edit_type(editor_core::transaction::EditType::Other);
        buffer.editor = buffer.editor.apply(txn);
        // Mirror the same splice into `working` so the editor binding's reverse
        // step (next frame) sees `working == editor.doc` and doesn't revert it.
        // Verified equal to `editor.doc` above, so the offsets index the same
        // bytes here.
        if let Some(doc_id) = &doc_id {
            let delete_len = r.end - r.start;
            if let Err(e) = log.apply_working_edit(doc_id, r.start, delete_len, &kept) {
                self.push_toast(format!("Resolve conflict failed: {e}"), ToastLevel::Error);
                return;
            }
        }
        let label = match action.choice {
            Choice::Current => "Accepted current",
            Choice::Incoming => "Accepted incoming",
            Choice::Both => "Accepted both",
        };
        self.push_toast(label.to_string(), ToastLevel::Info);
    }
}

/// Whether the conflict-resolve splice (whose byte offsets index `editor_text`)
/// is safe to replay onto the `working` layer: it is exactly when the working
/// layer's text is byte-identical to the editor doc. `None` means the working
/// text couldn't be read (treated as a mismatch — don't apply blindly).
fn working_matches_editor(working: Option<&str>, editor_text: &str) -> bool {
    working == Some(editor_text)
}

#[cfg(test)]
mod splice_safety_tests {
    use super::working_matches_editor;

    /// The common case: the working layer mirrors the editor doc, so the splice
    /// offsets index the same bytes in both — safe to apply.
    #[test]
    fn matches_when_working_equals_editor() {
        let text = "<<<<<<< ours\na\n=======\nb\n>>>>>>> theirs\n";
        assert!(working_matches_editor(Some(text), text));
    }

    /// Divergence (e.g. a pending review overlay shifted `working`): the same
    /// offsets would index different bytes there, so the resolve must NOT apply.
    #[test]
    fn rejects_when_working_diverged() {
        let editor = "<<<<<<< ours\na\n=======\nb\n>>>>>>> theirs\n";
        let drifted = "PREFIX\n<<<<<<< ours\na\n=======\nb\n>>>>>>> theirs\n";
        assert!(!working_matches_editor(Some(drifted), editor));
    }

    /// Unreadable working text is treated as a mismatch — never splice blindly.
    #[test]
    fn rejects_when_working_unreadable() {
        assert!(!working_matches_editor(None, "anything"));
    }
}
