//! Diff view helpers: turn a `Vec<Hunk>` (from `editor_core::diff::lines as diff_lines`)
//! into `DecorationSet`s ready to feed into a view's `decorations` layer.
//!
//! The widget itself doesn't know about diff — it just renders Block, Mark,
//! and Line decorations. This crate is the "consumer" that produces them.

pub mod view;

use editor_core::diff::lines as diff_lines;
use editor_core::diff::Hunk;
use editor_core::decoration::Set as DecorationSet;
use editor_core::rope::Rope;
use editor_core::theme::Theme;
/// Who owns the diff, which drives per-hunk verb affordances rendered by the
/// host (accept/reject for `Agent` / `Pending`, restore for `HistoryVersion`,
/// nothing for `Index` / `Manual`). Rendering of the underlying hunks is
/// identical across owners — only the host-side overlay widget content
/// differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffOwner {
    /// Gutter-only diff (e.g. "changed since open"). No inline decorations.
    Index,
    /// Pending op-log proposal (whole-file). Per-hunk accept / reject.
    Pending,
    /// Pending agent edits hydrated into the live buffer. Per-hunk
    /// accept / reject.
    Agent,
    /// A historical version of the note materialized from the op log. Per-hunk restore.
    HistoryVersion,
    /// User-initiated diff (e.g. between two buffers). No host verbs.
    Manual,
}

/// A single diff between two ropes plus the host's intent (owner). The
/// primitive every diff surface uses: history-version review, dirty-buffer diff,
/// pending agent edits, history browser.
///
/// Hunks are computed once via [`DiffLayer::from_ropes`] (or cheaply on
/// construction); the decoration set is emitted via
/// [`DiffLayer::decorations`].
pub struct DiffLayer {
    pub base: Rope,
    pub current: Rope,
    pub owner: DiffOwner,
    hunks: Vec<Hunk>,
    base_text: String,
}

impl DiffLayer {
    /// Build a layer from two ropes. Computes hunks once.
    pub fn from_ropes(base: Rope, current: Rope, owner: DiffOwner) -> Self {
        let base_text = base.to_string();
        let current_text = current.to_string();
        let hunks = diff_lines(&base_text, &current_text);
        Self { base, current, owner, hunks, base_text }
    }

    /// Build from already-known base text + current rope. Skips one rope→string conversion.
    pub fn from_base_text(base_text: String, current: Rope, owner: DiffOwner) -> Self {
        let base = Rope::from_str(&base_text);
        let current_text = current.to_string();
        let hunks = diff_lines(&base_text, &current_text);
        Self { base, current, owner, hunks, base_text }
    }

    pub const fn owner(&self) -> DiffOwner {
        self.owner
    }

    pub fn hunks(&self) -> &[Hunk] {
        &self.hunks
    }

    pub fn is_empty(&self) -> bool {
        self.hunks.iter().all(|h| matches!(h.kind, editor_core::diff::HunkKind::Context))
    }

    /// Produce a `DecorationSet` ready to be pushed onto the consumer's view.
    /// `intraline` matches the View-menu toggle that controls char-level marks.
    pub fn decorations(
        &self,
        line_height: f32,
        theme: Option<&Theme>,
        intraline: bool,
    ) -> DecorationSet {
        view::unified_decorations_opts(
            &self.current,
            &self.base_text,
            &self.hunks,
            line_height,
            theme,
            intraline,
        )
    }
}
