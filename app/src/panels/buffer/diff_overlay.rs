//! Unified inline-diff overlay for the editor tab. The single entry point
//! every diff-on-the-live-buffer flow rides on: dirty-buffer diff toggle,
//! pending agent edits (`agent_base`), changes.db history viewer.
//!
//! Inputs are the active buffer plus its tab's optional `DiffSource`.
//! Output is a `Set` ready to push onto the editor's decoration
//! stack, a lightweight hunk-list the file pill uses for counts +
//! next-hunk navigation, a click-map mapping per-hunk Accept/Reject
//! widget ids back to the staging proposals that contributed, and the
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

use editor_core::rope::Rope;
use editor_diff::DiffLayer;
use editor_diff::DiffOwner;
use smol_str::SmolStr;

use crate::buffer::Buffer;
use crate::state::AppState;
use crate::tab::DiffSource;

/// Click-action a hunk overlay-widget dispatches. Variants:
/// - `Accept` / `Reject`: agent-owned hunks — the proposal-id list is the
///   set of hydrated proposals whose footprint overlaps the hunk.
/// - `Restore`: snapshot-owned hunks — write the historical text the hunk
///   represents back to disk for that byte range.
#[derive(Clone, Debug)]
pub enum HunkAction {
    Accept(Vec<String>),
    Reject(Vec<String>),
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
    /// Byte range in the *current* buffer that the hunk covers (Added or
    /// Modified lines). For pure Removed hunks, points at the insertion
    /// site (zero-width range).
    pub byte_start: usize,
    pub byte_end: usize,
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
            DiffOwner::Agent => self.attach_agent_hunk_widgets(&base_decos, &hunks),
            DiffOwner::Snapshot => self.attach_snapshot_hunk_widgets(&base_decos, &hunks),
            _ => (base_decos, HashMap::new()),
        };

        Some(DiffOverlay { decorations, hunks, owner, click_map })
    }

    /// Resolve which "before" rope feeds the diff for this tab plus the
    /// owner (which drives per-hunk verb shape). Priority:
    /// 1. Vault buffer with `agent_base` → `Agent` (hydrated proposals).
    /// 2. Snapshot / StagingProposal / Trash buffer → owner derived from
    ///    `BufferSource`; "before" is the tab's `DiffSource` (typically
    ///    `Disk(path)` for snapshot + staging so the diff reads as "how
    ///    does this preview differ from current disk").
    /// 3. Plain Vault buffer with a `DiffSource` set → owner derived from
    ///    the source (dirty-buffer toggle, history viewer).
    fn resolve_base(&self) -> Option<(String, DiffOwner)> {
        use crate::tab::BufferSource;
        if let Some(base) = &self.buffer.agent_base {
            return Some((base.clone(), DiffOwner::Agent));
        }
        let source_owner = match &self.buffer.source {
            BufferSource::Snapshot { .. } => Some(DiffOwner::Snapshot),
            BufferSource::StagingProposal { .. } => Some(DiffOwner::Staging),
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
                DiffSource::ChangesDb { .. } => DiffOwner::Snapshot,
                DiffSource::StagingProposal { .. } => DiffOwner::Staging,
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
            DiffSource::ChangesDb { change_id, .. } => {
                let id: i64 = change_id.parse().ok()?;
                let bytes = self
                    .app
                    .vault_session
                    .services
                    .changes
                    .content_at(id)
                    .ok()
                    .flatten()?;
                String::from_utf8(bytes).ok()
            }
            DiffSource::StagingProposal { proposal_id } => self
                .app
                .vault_session
                .services
                .staging
                .content(proposal_id)
                .ok(),
            DiffSource::Trash { trash_path } => std::fs::read_to_string(trash_path).ok(),
            DiffSource::Empty => Some(String::new()),
        }
    }

    /// Project raw hunks into byte-range info on the current rope so the
    /// pill can position cursor jumps without recomputing.
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
            out.push(HunkInfo { byte_start, byte_end, kind: h.kind.clone() });
        }
        out
    }

    /// For an Agent-owner overlay, layer per-hunk Accept/Reject buttons on
    /// top of the base diff decorations and return the click-map that
    /// maps each button id back to the proposals it acts on.
    fn attach_agent_hunk_widgets(
        &self,
        base: &Set,
        hunks: &[HunkInfo],
    ) -> (Set, HashMap<u64, HunkAction>) {
        let current = &self.buffer.editor.doc;
        let footprints = &self.buffer.hydration_footprints;
        let mut entries: Vec<(Range<usize>, Decoration)> =
            base.iter_all().map(|(r, d)| (r, d.clone())).collect();
        let mut click_map: HashMap<u64, HunkAction> = HashMap::new();
        let mut next_id = HUNK_WIDGET_BASE;

        for (i, hunk) in hunks.iter().enumerate() {
            if !hunk.is_change() {
                continue;
            }
            // Inlined: collect every hydrated proposal whose recorded
            // footprint overlaps this hunk's current-text byte range.
            let mut proposal_ids: Vec<String> = Vec::new();
            for (pid, range) in footprints {
                let overlap = range.start < hunk.byte_end && hunk.byte_start < range.end;
                if overlap && !proposal_ids.iter().any(|x| x == pid) {
                    proposal_ids.push(pid.clone());
                }
            }
            if proposal_ids.is_empty() {
                // No tracked proposals (e.g. user-typed edits inside a
                // hydrated buffer). Skip the verbs; the change still
                // paints as a diff but bulk Accept-all / Reject-all stay
                // the only affordance for it.
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
            let anchor = Self::anchor_for_hunk_end(current, hunk);
            entries.push((anchor..anchor, Decoration::Block(row)));
        }

        (RangeSet::from_iter(entries), click_map)
    }

    /// Snapshot-owner per-hunk Restore widgets. Each non-context hunk
    /// gets a single Restore button; clicking writes the snapshot's text
    /// for the hunk's current byte range back to disk at the source path.
    fn attach_snapshot_hunk_widgets(
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
        for (i, hunk) in hunks.iter().enumerate() {
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
            let row = BlockDeco {
                side: BlockSide::Below,
                height: 24.0,
                kind: BlockKind::ActionRow {
                    label: SmolStr::new(format!("Hunk {}", i + 1)),
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
            let anchor = Self::anchor_for_hunk_end(current, hunk);
            entries.push((anchor..anchor, Decoration::Block(row)));
        }
        (RangeSet::from_iter(entries), click_map)
    }

    /// Pick the byte position to anchor a per-hunk ActionRow at: just
    /// after the hunk's last line so the buttons sit beneath the change,
    /// not floating inside it. Associated (non-self) — the two widget
    /// attachers above call this from inside their hot loop, so it has
    /// multiple call sites and is exempt from `single_call_fn`.
    fn anchor_for_hunk_end(current: &Rope, hunk: &HunkInfo) -> usize {
        let line_to_byte = |line: usize| -> usize {
            if line >= current.len_lines() {
                current.len_bytes()
            } else {
                current.line_to_byte(line)
            }
        };
        if hunk.byte_end > hunk.byte_start {
            let last_line_end = hunk.byte_end.saturating_sub(1).min(current.len_bytes());
            let line = current.byte_to_line(last_line_end);
            line_to_byte(line)
        } else {
            let pos = hunk.byte_start.min(current.len_bytes());
            let line = current.byte_to_line(pos);
            line_to_byte(line)
        }
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
