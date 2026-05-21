//! Unified inline-diff overlay for the editor tab. The single entry point
//! every diff-on-the-live-buffer flow rides on: dirty-buffer diff toggle,
//! pending agent edits (`agent_base`), changes.db history viewer.
//!
//! Inputs are the active buffer plus its tab's optional `DiffSource`.
//! Output is a `DecorationSet` ready to push onto the editor's decoration
//! stack, a lightweight hunk-list the file pill uses for counts +
//! next-hunk navigation, a click-map mapping per-hunk Accept/Reject
//! widget ids back to the staging proposals that contributed, and the
//! diff owner (for owner-aware verb rendering).

use std::collections::HashMap;
use std::ops::Range;

use editor_core::diff::{Hunk, HunkKind};
use editor_core::{
    ActionButton, ActionButtonStyle, ActionTone, BlockDeco, BlockKind, BlockSide, Decoration,
    DecorationSet, RangeSet, Rope,
};
use editor_diff::{DiffLayer, DiffOwner};
use smol_str::SmolStr;

use crate::buffer::Buffer;
use crate::state::AppState;
use crate::tab::DiffSource;

/// Click-action a hunk overlay-widget dispatches when the user picks
/// Accept or Reject. The proposal-id list is the set of hydrated
/// proposals whose footprint overlaps the hunk's current byte range.
#[derive(Clone, Debug)]
pub enum HunkAction {
    Accept(Vec<String>),
    Reject(Vec<String>),
}

/// Per-tab inline diff bundle. Decorations go on the editor; `hunks` feeds
/// the pill; `click_map` resolves overlay-widget clicks.
pub struct DiffOverlay {
    pub decorations: DecorationSet,
    pub hunks: Vec<HunkInfo>,
    pub owner: DiffOwner,
    pub click_map: HashMap<u64, HunkAction>,
}

/// Lightweight view of a single hunk for the file pill + navigation.
#[derive(Debug, Clone)]
pub struct HunkInfo {
    /// Byte range in the *current* buffer that the hunk covers (Added or
    /// Modified lines). For pure Removed hunks, points at the insertion
    /// site (zero-width range).
    pub byte_start: usize,
    pub byte_end: usize,
    pub kind: HunkKind,
}

impl HunkInfo {
    pub fn is_change(&self) -> bool {
        !matches!(self.kind, HunkKind::Context)
    }
}

/// Widget-id namespace for per-hunk Accept/Reject buttons. Separate from
/// the fold-widget id range so the editor's click router can keep telling
/// them apart by id alone.
const HUNK_WIDGET_BASE: u64 = 0xFFFF_0002_0000_0000;

/// Compute the diff overlay for `path` in the active tab. Returns `None`
/// when there's nothing to show.
///
/// Agent diff (from `agent_base`) takes precedence over the tab's
/// `DiffSource` toggle so opening a file with pending agent edits never
/// hides them behind whichever diff target the user last picked.
pub fn compute(app: &AppState, path: &str) -> Option<DiffOverlay> {
    let buffer = app.session.buffers.get(path)?;
    let (base_text, owner) = resolve_base(app, buffer, path)?;
    let layer = DiffLayer::from_base_text(base_text, buffer.editor.doc.clone(), owner);
    if layer.is_empty() {
        return None;
    }
    let theme = editor_core::light_default();
    let line_height = buffer.view.line_height.max(14.0);
    let base_decos = layer.decorations(line_height, Some(&theme), buffer.intraline_diff);
    let hunks = layer_hunks(layer.hunks(), &buffer.editor.doc);

    let (decorations, click_map) = match owner {
        DiffOwner::Agent => attach_agent_hunk_widgets(
            base_decos,
            &hunks,
            &buffer.editor.doc,
            &buffer.hydration_footprints,
        ),
        _ => (base_decos, HashMap::new()),
    };

    Some(DiffOverlay { decorations, hunks, owner, click_map })
}

/// Resolve which "before" rope feeds the diff for this tab, in priority
/// order: agent hydration first (per `patch-review-buffer-hydration`),
/// then the tab's `DiffSource` (per `diff-as-mode`).
fn resolve_base(
    app: &AppState,
    buffer: &Buffer,
    path: &str,
) -> Option<(String, DiffOwner)> {
    if let Some(base) = &buffer.agent_base {
        return Some((base.clone(), DiffOwner::Agent));
    }
    let active = app.session.active_tab?;
    let tab = app.tab_by_id(active)?;
    let src = tab.kind.diff_source()?;
    let base = resolve_source_text(app, src, path)?;
    Some((base, diff_owner_for(src)))
}

fn diff_owner_for(src: &DiffSource) -> DiffOwner {
    match src {
        DiffSource::ChangesDb { .. } => DiffOwner::Snapshot,
        DiffSource::StagingProposal { .. } => DiffOwner::Staging,
        DiffSource::Disk { .. }
        | DiffSource::LiveBuffer { .. }
        | DiffSource::Trash { .. }
        | DiffSource::Empty => DiffOwner::Manual,
    }
}

/// Resolve a `DiffSource` to its before-text by going through the relevant
/// service. Failures (missing change row, unreadable file) collapse to
/// `None` — the overlay just doesn't render rather than blocking the
/// editor with an error state.
fn resolve_source_text(app: &AppState, src: &DiffSource, _path: &str) -> Option<String> {
    match src {
        DiffSource::Disk { path } => app.vault_session.vault.read_file(path).ok(),
        DiffSource::LiveBuffer { path } => {
            app.session.buffers.get(path).map(|b| b.editor.doc.to_string())
        }
        DiffSource::ChangesDb { change_id, .. } => {
            let id: i64 = change_id.parse().ok()?;
            let bytes = app.vault_session.services.changes.content_at(id).ok().flatten()?;
            String::from_utf8(bytes).ok()
        }
        DiffSource::StagingProposal { proposal_id } => {
            app.vault_session.services.staging.content(proposal_id).ok()
        }
        DiffSource::Trash { trash_path } => std::fs::read_to_string(trash_path).ok(),
        DiffSource::Empty => Some(String::new()),
    }
}

/// Project raw hunks into byte-range info on the current rope so the pill
/// can position cursor jumps without recomputing.
fn layer_hunks(hunks: &[Hunk], current: &Rope) -> Vec<HunkInfo> {
    let mut out = Vec::new();
    for h in hunks {
        if matches!(h.kind, HunkKind::Context) {
            continue;
        }
        let byte_start = line_to_byte(current, h.right_lines.start);
        let byte_end = line_to_byte(current, h.right_lines.end);
        out.push(HunkInfo { byte_start, byte_end, kind: h.kind.clone() });
    }
    out
}

fn line_to_byte(rope: &Rope, line: usize) -> usize {
    if line >= rope.len_lines() {
        rope.len_bytes()
    } else {
        rope.line_to_byte(line)
    }
}

/// For an Agent-owner overlay, layer per-hunk Accept/Reject buttons on
/// top of the base diff decorations and return the click-map that maps
/// each button id back to the proposals it acts on.
fn attach_agent_hunk_widgets(
    base: DecorationSet,
    hunks: &[HunkInfo],
    current: &Rope,
    footprints: &[(String, Range<usize>)],
) -> (DecorationSet, HashMap<u64, HunkAction>) {
    let mut entries: Vec<(Range<usize>, Decoration)> = base
        .iter_all()
        .map(|(r, d)| (r, d.clone()))
        .collect();
    let mut click_map: HashMap<u64, HunkAction> = HashMap::new();
    let mut next_id = HUNK_WIDGET_BASE;

    for (i, hunk) in hunks.iter().enumerate() {
        if !hunk.is_change() {
            continue;
        }
        let proposal_ids = proposals_for_hunk(footprints, hunk);
        if proposal_ids.is_empty() {
            // No tracked proposals (e.g. user-typed edits inside a
            // hydrated buffer). Skip the verbs; the change still paints
            // as a diff but bulk Accept-all / Reject-all stay the only
            // affordance for it.
            continue;
        }

        let accept_id = next_id;
        let reject_id = next_id + 1;
        next_id += 2;

        click_map.insert(accept_id, HunkAction::Accept(proposal_ids.clone()));
        click_map.insert(reject_id, HunkAction::Reject(proposal_ids));

        let row = BlockDeco {
            side: BlockSide::Below,
            height: 24.0,
            kind: BlockKind::ActionRow {
                label: SmolStr::new(format!("Hunk {}", i + 1)),
                glyph: None,
                tone: ActionTone::Normal,
                buttons: vec![
                    ActionButton {
                        id: accept_id,
                        label: SmolStr::new_static("Accept"),
                        style: ActionButtonStyle::Primary,
                        enabled: true,
                    },
                    ActionButton {
                        id: reject_id,
                        label: SmolStr::new_static("Reject"),
                        style: ActionButtonStyle::Danger,
                        enabled: true,
                    },
                ],
            },
        };
        // Anchor below the hunk's last line. For a zero-width Removed
        // hunk (insertion site only), drop the row right at byte_start.
        let anchor = anchor_for_hunk_end(current, hunk);
        entries.push((anchor..anchor, Decoration::Block(row)));
    }

    (RangeSet::from_iter(entries), click_map)
}

/// Pick the byte position to anchor a per-hunk ActionRow at: just after
/// the hunk's last line so the buttons sit beneath the change, not
/// floating inside it.
fn anchor_for_hunk_end(current: &Rope, hunk: &HunkInfo) -> usize {
    if hunk.byte_end > hunk.byte_start {
        let last_line_end = hunk.byte_end.saturating_sub(1).min(current.len_bytes());
        // Find the start of the line containing byte_end-1; the row will
        // be anchored at that line and rendered Below.
        let line = current.byte_to_line(last_line_end);
        line_to_byte(current, line)
    } else {
        let pos = hunk.byte_start.min(current.len_bytes());
        let line = current.byte_to_line(pos);
        line_to_byte(current, line)
    }
}

/// Find every hydrated proposal whose recorded footprint overlaps a
/// hunk's current-text byte range.
fn proposals_for_hunk(
    footprints: &[(String, Range<usize>)],
    hunk: &HunkInfo,
) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for (pid, range) in footprints {
        let overlap = range.start < hunk.byte_end && hunk.byte_start < range.end;
        if overlap && !ids.iter().any(|x| x == pid) {
            ids.push(pid.clone());
        }
    }
    ids
}
