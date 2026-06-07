//! Unified inline-diff overlay for the editor tab. The single entry point
//! every diff-on-the-live-buffer flow rides on: dirty-buffer diff toggle,
//! pending agent edits (`agent_proposal` suggestion overlay), op-log history viewer.
//!
//! Inputs are the active buffer plus its tab's optional `DiffSource`.
//! Output is a `Set` ready to push onto the editor's decoration
//! stack, a lightweight hunk-list the file pill uses for counts +
//! next-hunk navigation, a click-map mapping per-hunk Accept/Reject
//! widget ids back to the pending op ids that contributed, and the
//! diff owner (for owner-aware verb rendering).

use std::collections::HashMap;
use std::ops::Range;

use editor_core::diff::Hunk;

use editor_core::diff::HunkKind;
use editor_core::decoration::ActionButton;
use editor_core::decoration::ActionButtonStyle;
use editor_core::decoration::ActionTone;
use editor_core::decoration::BlockDeco;
use editor_core::decoration::BlockKind;
use editor_core::decoration::BlockSide;
use editor_core::decoration::Decoration;
use editor_core::decoration::Set;
use editor_core::rangeset::RangeSet;

use editor_diff::DiffLayer;
use editor_diff::DiffOwner;
use smol_str::SmolStr;

use editor_diff::conflict;
use crate::buffer::Buffer;
use crate::state::AppState;
use crate::tab::DiffSource;

/// Click-action a hunk overlay-widget dispatches. Variants:
/// - `Accept` / `Reject`: agent-owned hunks — the id list is the set of
///   pending op ids whose edit overlaps the hunk's current-text range
///   (resolved via `op_writes::ops_in_hunk`).
/// - `KeepMine` / `KeepTheirs` / `KeepBoth`: conflict hunks (an agent op whose
///   region the user also edited in `working`, per `op-log-merge-conflict`).
///   Keep-mine rejects the agent op; keep-both accepts it (the merge keeps
///   both edits); keep-theirs first reverts the user's overlapping
///   `working` edit to the accepted text (`revert` carries the precomputed
///   `apply_working_edit` args) then accepts.
/// - `Restore`: history-version-owned hunks — write the historical text the hunk
///   represents back to disk for that byte range.
#[derive(Clone, Debug)]
pub enum HunkAction {
    Accept(Vec<String>),
    Reject(Vec<String>),
    KeepMine(Vec<String>),
    KeepBoth(Vec<String>),
    KeepTheirs {
        op_ids: Vec<String>,
        /// `apply_working_edit` args: `(byte_start, byte_len, accepted_text)`
        /// for the user's conflicting `working` region.
        revert: (usize, usize, String),
    },
    Restore { path: String, byte_start: usize, byte_end: usize },
}

/// Per-tab inline diff bundle. Decorations go on the editor; `hunks` feeds
/// the pill; `click_map` resolves overlay-widget clicks.
pub struct DiffOverlay {
    pub decorations: Set,
    pub hunks: Vec<HunkInfo>,
    pub owner: DiffOwner,
    pub click_map: HashMap<u64, HunkAction>,
}

/// Lightweight view of a single hunk for the file pill + navigation.
#[derive(Debug, Clone)]
pub struct HunkInfo {
    /// Byte range in the editable buffer (`editor.doc`) the hunk sits at —
    /// used for anchoring the per-hunk widgets, cursor navigation, and (for the
    /// agent overlay) conflict detection against the user's `working` edits.
    /// For the agent overlay the buffer is `materialize_working`, so this is the
    /// hunk's *working* (left/old side) span; a pure agent insertion is a
    /// zero-width range at the insertion site. For snapshot/manual diffs the
    /// buffer is the new side, so this is the right-side span.
    pub byte_start: usize,
    pub byte_end: usize,
    /// Byte range in the agent's *proposal* (`materialize_review`, the diff's
    /// right/new side) — the space pending-op affected ranges live in, where an
    /// insertion has width. Used to resolve which pending ops a hunk covers
    /// (`ops_in_hunk`). For non-agent diffs this mirrors `byte_*`.
    pub op_start: usize,
    pub op_end: usize,
    pub kind: HunkKind,
}

impl HunkInfo {
    pub const fn is_change(&self) -> bool {
        !matches!(self.kind, HunkKind::Context)
    }
}

/// Widget-id namespace for per-hunk Accept/Reject buttons. Separate from
/// the fold-widget id range so the editor's click router can keep telling
/// them apart by id alone.
const HUNK_WIDGET_BASE: u64 = 0xFFFF_0002_0000_0000;

/// Compute context — bundles the inputs the helpers share so they can be
/// methods on `self`. Methods with `&self` receivers are exempt from
/// `clippy::single_call_fn`, which lets us keep each step factored without
/// running afoul of the lint.
struct Compute<'a> {
    app: &'a AppState,
    buffer: &'a Buffer,
    path: &'a str,
}

impl<'a> Compute<'a> {
    /// Compute the diff overlay for `path`. Returns `None` when there's
    /// nothing to show.
    ///
    /// Agent diff (from `agent_base`) takes precedence over the tab's
    /// `DiffSource` toggle so opening a file with pending agent edits
    /// never hides them behind whichever diff target the user last
    /// picked.
    fn run(&self) -> Option<DiffOverlay> {
        // Agent overlay: the editable buffer is `working`; the agent's pending
        // ops are a *proposal* (`materialize_review`) rendered as a suggestion
        // overlay on top. Takes precedence over the tab's DiffSource so pending
        // edits are never hidden behind whichever diff target the user picked.
        if let Some(proposal) = &self.buffer.agent_proposal {
            return self.agent_overlay(proposal);
        }

        // Other surfaces (snapshot / history / dirty-buffer toggle): a standard
        // diff where the editable buffer is the new side.
        let (base_text, owner) = self.resolve_base()?;
        let layer = DiffLayer::from_base_text(
            base_text,
            self.buffer.editor.doc.clone(),
            owner,
        );
        if layer.is_empty() {
            return None;
        }
        let theme = editor_core::theme::light_default();
        let line_height = self.buffer.view.line_height.max(14.0);
        let base_decos =
            layer.decorations(line_height, Some(&theme), self.buffer.intraline_diff);
        let hunks = self.layer_hunks(layer.hunks());

        let (decorations, click_map) = match owner {
            DiffOwner::HistoryVersion => self.attach_history_version_hunk_widgets(&base_decos, &hunks),
            _ => (base_decos, HashMap::new()),
        };

        Some(DiffOverlay { decorations, hunks, owner, click_map })
    }

    /// Build the agent suggestion overlay: diff the editable buffer (`working`)
    /// against the agent's `proposal` (`materialize_review`) and render the
    /// pending ops as a suggestion overlay — additions as phantom blocks,
    /// deletions struck through (`proposal_decorations`) — then layer per-hunk
    /// Accept/Reject widgets on top. Per `patch-review-buffer-state`.
    fn agent_overlay(&self, proposal: &str) -> Option<DiffOverlay> {
        let working = self.buffer.editor.doc.to_string();
        let raw = editor_core::diff::lines(&working, proposal);
        if raw.iter().all(|h| matches!(h.kind, HunkKind::Context)) {
            return None;
        }
        let theme = editor_core::theme::light_default();
        let line_height = self.buffer.view.line_height.max(14.0);
        let base_decos = editor_diff::view::proposal_decorations(
            &self.buffer.editor.doc,
            proposal,
            &raw,
            line_height,
            Some(&theme),
            self.buffer.intraline_diff,
        );
        let hunks = self.agent_hunks(&raw, proposal);
        let (decorations, click_map) = self.attach_agent_hunk_widgets(&base_decos, &hunks);
        Some(DiffOverlay { decorations, hunks, owner: DiffOwner::Agent, click_map })
    }

    /// Resolve which "before" rope feeds a *non-agent* diff for this tab plus
    /// the owner (which drives per-hunk verb shape). The agent overlay is
    /// handled separately in [`Self::agent_overlay`] before this is reached.
    /// Priority:
    /// 1. HistoryVersion / PendingProposal / Trash buffer → owner derived from
    ///    `BufferSource`; "before" is the tab's `DiffSource` (typically
    ///    `Disk(path)` for snapshot + pending so the diff reads as "how
    ///    does this preview differ from current disk").
    /// 2. Plain Vault buffer with a `DiffSource` set → owner derived from
    ///    the source (dirty-buffer toggle, history viewer).
    fn resolve_base(&self) -> Option<(String, DiffOwner)> {
        use crate::tab::BufferSource;
        let source_owner = match &self.buffer.source {
            BufferSource::HistoryVersion { .. } => Some(DiffOwner::HistoryVersion),
            BufferSource::PendingProposal { .. } => Some(DiffOwner::Pending),
            BufferSource::Trash { .. } => Some(DiffOwner::Manual),
            BufferSource::Vault { .. } => None,
        };
        let active = self.app.session.active_tab?;
        let tab = self.app.tab_by_id(active)?;
        let src = tab.kind.diff_source()?;
        let base = self.resolve_source_text(src)?;
        let owner = source_owner.unwrap_or_else(|| {
            // Inlined `diff_owner_for`: owner derived from the DiffSource
            // discriminant when no BufferSource carries one.
            match src {
                DiffSource::HistoryVersion { .. } => DiffOwner::HistoryVersion,
                DiffSource::PendingProposal { .. } => DiffOwner::Pending,
                DiffSource::Disk { .. }
                | DiffSource::LiveBuffer { .. }
                | DiffSource::Trash { .. }
                | DiffSource::Empty => DiffOwner::Manual,
            }
        });
        Some((base, owner))
    }

    /// Resolve a `DiffSource` to its before-text by going through the
    /// relevant service. Failures (missing change row, unreadable file)
    /// collapse to `None` — the overlay just doesn't render rather than
    /// blocking the editor with an error state.
    fn resolve_source_text(&self, src: &DiffSource) -> Option<String> {
        match src {
            DiffSource::Disk { path } => self.app.vault_session.vault.read_file(path).ok(),
            DiffSource::LiveBuffer { path } => {
                self.app.session.buffers.get(path).map(|b| b.editor.doc.to_string())
            }
            DiffSource::HistoryVersion { op_id, path } => {
                let log = self.app.vault_session.services.oplog.as_ref();
                hiker_core::ops::op_writes::content_at_op(log, path, op_id)
                    .ok()
                    .flatten()
            }
            DiffSource::PendingProposal { proposal_id } => {
                // The op id's proposed content via the op log. The target
                // path comes from the preview buffer's source (a
                // `PendingProposal` buffer fronts a whole-file op for one
                // path). No legacy pending-store read on this surface.
                let target = match &self.buffer.source {
                    crate::tab::BufferSource::PendingProposal { target_path, .. } => {
                        target_path.as_str()
                    }
                    _ => return None,
                };
                let log = self.app.vault_session.services.oplog.as_ref();
                hiker_core::ops::op_writes::proposal_materializations(
                    log,
                    target,
                    proposal_id,
                )
                .ok()
                .flatten()
                .map(|(_accepted, proposed)| proposed)
            }
            DiffSource::Trash { trash_path } => std::fs::read_to_string(trash_path).ok(),
            DiffSource::Empty => Some(String::new()),
        }
    }

    /// Project raw hunks into byte-range info (non-agent diffs) so the pill can
    /// position cursor jumps without recomputing. The editable buffer is the
    /// new (right) side, so `byte_*` is the right-side span over the current
    /// rope; `op_*` mirrors it (pending-op resolution only matters for the
    /// agent overlay, which uses [`Self::agent_hunks`]).
    fn layer_hunks(&self, hunks: &[Hunk]) -> Vec<HunkInfo> {
        let current = &self.buffer.editor.doc;
        let line_to_byte = |line: usize| -> usize {
            if line >= current.len_lines() {
                current.len_bytes()
            } else {
                current.line_to_byte(line)
            }
        };
        let mut out = Vec::new();
        for h in hunks {
            if matches!(h.kind, HunkKind::Context) {
                continue;
            }
            let byte_start = line_to_byte(h.right_lines.start);
            let byte_end = line_to_byte(h.right_lines.end);
            out.push(HunkInfo {
                byte_start,
                byte_end,
                op_start: byte_start,
                op_end: byte_end,
                kind: h.kind.clone(),
            });
        }
        out
    }

    /// Project agent-overlay hunks (`diff(working, proposal)`) into `HunkInfo`.
    /// `byte_*` is the buffer (= `working`, the diff's left side) span where the
    /// hunk attaches — a pure agent insertion is a zero-width range at its site,
    /// which is exactly where the Accept/Reject widget should anchor. `op_*` is
    /// the proposal (right side) span where the pending op's content lives, used
    /// to resolve which ops the hunk covers (an insertion has width there).
    fn agent_hunks(&self, hunks: &[Hunk], proposal: &str) -> Vec<HunkInfo> {
        editor_diff::overlay::agent_hunks(&self.buffer.editor.doc, proposal, hunks)
            .into_iter()
            .map(|g| HunkInfo {
                byte_start: g.byte_start,
                byte_end: g.byte_end,
                op_start: g.op_start,
                op_end: g.op_end,
                kind: g.kind,
            })
            .collect()
    }

    /// For an Agent-owner overlay, layer per-hunk Accept/Reject buttons on
    /// top of the base diff decorations and return the click-map that maps
    /// each button id back to the pending op ids it acts on.
    ///
    /// Each hunk's `current_range` is resolved to the contributing pending
    /// op ids via `op_writes::ops_in_hunk` (scoped to the buffer's active
    /// session) per `op-log-per-hunk-accept-reject`. Accept is greyed when
    /// any contributing op has drifted (`is_pending_drifted`), per
    /// `patch-review-conflicted-accept-disabled`; Reject stays active.
    fn attach_agent_hunk_widgets(
        &self,
        base: &Set,
        hunks: &[HunkInfo],
    ) -> (Set, HashMap<u64, HunkAction>) {
        let current = &self.buffer.editor.doc;
        let log = self.app.vault_session.services.oplog.as_ref();
        let session = self.buffer.active_session.as_deref();
        let doc_id = log.doc_id_for_path(self.path).ok().flatten();
        // User-edit ranges in `working` coords: the changed regions of
        // diff(accepted, working). A hunk overlapping one of these is a
        // conflict (the user and the agent both touched that region) per
        // `op-log-merge-conflict`. Read both materializations off the op log
        // by doc_id; absent doc → no user edits → every hunk is "normal".
        let (accepted_text, user_edits) = doc_id
            .as_deref()
            .map(|d| self.user_edits_for_doc(d))
            .unwrap_or_default();
        let mut entries: Vec<(Range<usize>, Decoration)> =
            base.iter_all().map(|(r, d)| (r, d.clone())).collect();
        let mut click_map: HashMap<u64, HunkAction> = HashMap::new();
        let mut next_id = HUNK_WIDGET_BASE;

        for hunk in hunks {
            if !hunk.is_change() {
                continue;
            }
            // Resolve the hunk to its contributing pending op ids (op-log seam,
            // session-scoped). `op_*` is the hunk's span in the *proposal*
            // (review) coordinates where pending-op footprints live and an
            // insertion has width; `ops_in_range` matches against affected
            // ranges in that space. The buffer-side `byte_*` range (working
            // coords, zero-width for a pure insertion) is for conflict + anchor.
            let op_ids: Vec<String> = hiker_core::ops::op_writes::ops_in_hunk(
                log,
                self.path,
                session,
                hunk.op_start,
                hunk.op_end,
            )
            .unwrap_or_default();
            if op_ids.is_empty() {
                // No pending ops cover this hunk (e.g. user-typed edits, or
                // the change is contributed only by drifted ops not in the
                // view). Skip the verbs; the change still paints as a diff
                // but Reject-all stays the affordance for it.
                continue;
            }
            // Accept is disabled when any contributing op has drifted.
            let any_drifted = doc_id.as_deref().is_some_and(|d| {
                op_ids
                    .iter()
                    .any(|op| log.is_pending_drifted(d, op).unwrap_or(false))
            });
            let hunk_working = hunk.byte_start..hunk.byte_end;
            let mut row = if conflict::hunk_overlaps_user_edit(&hunk_working, &user_edits) {
                Self::conflict_row(
                    &mut next_id,
                    &mut click_map,
                    &op_ids,
                    &accepted_text,
                    &hunk_working,
                    &user_edits,
                )
            } else {
                Self::accept_reject_row(&mut next_id, &mut click_map, &op_ids, any_drifted)
            };
            let (anchor, side) =
                editor_diff::overlay::anchor_and_side(current, hunk.byte_start, hunk.byte_end);
            row.side = side;
            entries.push((anchor..anchor, Decoration::Block(row)));
        }

        (RangeSet::from_iter(entries), click_map)
    }

    /// `(accepted_text, user_edit_ranges)` for `doc_id`: the canonical text
    /// plus the regions the user changed in `working` (in `working` coords).
    /// Both materializations come straight off the op-log handle per
    /// `op-log.md`'s module placement. A materialization error collapses to
    /// "no user edits", so the overlay degrades to plain Accept/Reject rather
    /// than blocking.
    fn user_edits_for_doc(&self, doc_id: &str) -> (String, Vec<conflict::UserEdit>) {
        let log = self.app.vault_session.services.oplog.as_ref();
        let (Ok(accepted), Ok(working)) =
            (log.materialize_accepted(doc_id), log.materialize_working(doc_id))
        else {
            return (String::new(), Vec::new());
        };
        let edits = conflict::user_edit_ranges(&accepted.text, &working.text);
        (accepted.text, edits)
    }

    /// Build the plain Accept/Reject `ActionRow` for a non-conflict agent
    /// hunk and register its button ids in `click_map`. Drift greys Accept;
    /// Reject stays active.
    fn accept_reject_row(
        next_id: &mut u64,
        click_map: &mut HashMap<u64, HunkAction>,
        op_ids: &[String],
        any_drifted: bool,
    ) -> BlockDeco {
        let accept_id = *next_id;
        let reject_id = *next_id + 1;
        *next_id += 2;
        click_map.insert(accept_id, HunkAction::Accept(op_ids.to_vec()));
        click_map.insert(reject_id, HunkAction::Reject(op_ids.to_vec()));
        BlockDeco {
            side: BlockSide::Below,
            height: 24.0,
            kind: BlockKind::ActionRow {
                // No hunk number — the Accept / Reject buttons are the
                // affordance; the index just added noise and a line. Keep
                // a "drifted" marker, since that's what explains the
                // disabled Accept.
                label: if any_drifted {
                    SmolStr::new_static("drifted")
                } else {
                    SmolStr::new_static("")
                },
                glyph: None,
                tone: ActionTone::Normal,
                buttons: vec![
                    ActionButton {
                        id: accept_id,
                        label: SmolStr::new_static("Accept"),
                        style: ActionButtonStyle::Primary,
                        // Drift disables Accept; Reject stays active.
                        enabled: !any_drifted,
                    },
                    ActionButton {
                        id: reject_id,
                        label: SmolStr::new_static("Reject"),
                        style: ActionButtonStyle::Danger,
                        enabled: true,
                    },
                ],
            },
        }
    }

    /// Build the three-button conflict `ActionRow` (Keep mine / Keep theirs /
    /// Keep both) for an agent hunk whose region the user also edited, and
    /// register the three button ids in `click_map` per `op-log-merge-conflict`.
    /// Keep-theirs precomputes its `apply_working_edit` revert args from the
    /// accepted text + the overlapping user edit; if the overlap can't be
    /// resolved to revert args (shouldn't happen for a detected conflict) the
    /// button is dropped, leaving keep-mine / keep-both.
    fn conflict_row(
        next_id: &mut u64,
        click_map: &mut HashMap<u64, HunkAction>,
        op_ids: &[String],
        accepted_text: &str,
        hunk_working: &Range<usize>,
        user_edits: &[conflict::UserEdit],
    ) -> BlockDeco {
        let mine_id = *next_id;
        let theirs_id = *next_id + 1;
        let both_id = *next_id + 2;
        *next_id += 3;
        click_map.insert(mine_id, HunkAction::KeepMine(op_ids.to_vec()));
        click_map.insert(both_id, HunkAction::KeepBoth(op_ids.to_vec()));
        let mut buttons = vec![ActionButton {
            id: mine_id,
            label: SmolStr::new_static("Keep mine"),
            style: ActionButtonStyle::Primary,
            enabled: true,
        }];
        if let Some(revert) =
            conflict::keep_theirs_edit(accepted_text, hunk_working, user_edits)
        {
            click_map.insert(theirs_id, HunkAction::KeepTheirs { op_ids: op_ids.to_vec(), revert });
            buttons.push(ActionButton {
                id: theirs_id,
                label: SmolStr::new_static("Keep theirs"),
                style: ActionButtonStyle::Danger,
                enabled: true,
            });
        }
        buttons.push(ActionButton {
            id: both_id,
            label: SmolStr::new_static("Keep both"),
            style: ActionButtonStyle::Neutral,
            enabled: true,
        });
        BlockDeco {
            side: BlockSide::Below,
            height: 24.0,
            kind: BlockKind::ActionRow {
                label: SmolStr::new_static("conflict"),
                glyph: None,
                tone: ActionTone::Conflicted,
                buttons,
            },
        }
    }

    /// History-version-owner per-hunk Restore widgets. Each non-context hunk
    /// gets a single Restore button; clicking writes the historical text
    /// for the hunk's current byte range back to disk at the source path.
    fn attach_history_version_hunk_widgets(
        &self,
        base: &Set,
        hunks: &[HunkInfo],
    ) -> (Set, HashMap<u64, HunkAction>) {
        let current = &self.buffer.editor.doc;
        let path = self.buffer.path.as_str();
        let _ = self.path; // path arg retained for symmetry with the agent variant
        let mut entries: Vec<(Range<usize>, Decoration)> =
            base.iter_all().map(|(r, d)| (r, d.clone())).collect();
        let mut click_map: HashMap<u64, HunkAction> = HashMap::new();
        let mut next_id = HUNK_WIDGET_BASE + 0x1_0000_0000;
        for hunk in hunks {
            if !hunk.is_change() {
                continue;
            }
            let restore_id = next_id;
            next_id += 1;
            click_map.insert(
                restore_id,
                HunkAction::Restore {
                    path: path.to_string(),
                    byte_start: hunk.byte_start,
                    byte_end: hunk.byte_end,
                },
            );
            let mut row = BlockDeco {
                side: BlockSide::Below,
                height: 24.0,
                kind: BlockKind::ActionRow {
                    // No hunk number — the Restore button is the affordance.
                    label: SmolStr::new_static(""),
                    glyph: None,
                    tone: ActionTone::Normal,
                    buttons: vec![ActionButton {
                        id: restore_id,
                        label: SmolStr::new_static("Restore"),
                        style: ActionButtonStyle::Primary,
                        enabled: true,
                    }],
                },
            };
            let (anchor, side) =
                editor_diff::overlay::anchor_and_side(current, hunk.byte_start, hunk.byte_end);
            row.side = side;
            entries.push((anchor..anchor, Decoration::Block(row)));
        }
        (RangeSet::from_iter(entries), click_map)
    }

}

impl AppState {
    /// Compute the diff overlay for `path` in the active tab. Returns
    /// `None` when there's nothing to show. Lives on `AppState` so the
    /// caller can dot-call into it as a method — single-call free
    /// functions get flagged by clippy, methods don't.
    pub fn diff_overlay_for(&self, path: &str) -> Option<DiffOverlay> {
        let buffer = self.session.buffers.get(path)?;
        Compute { app: self, buffer, path }.run()
    }
}
