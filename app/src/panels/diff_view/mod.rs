//! Shared read-only diff preview surface used by the staging, snapshot, and
//! trash preview tabs.
//!
//! Builds an `EditorState` + `ViewState` over the "after" text, applies
//! unified-diff decorations against the "before" text, and renders through
//! the standard `EditorWidget`. Toggling diff off reveals just the after-
//! side text without decorations.

use editor_core::diff::diff_lines;
use editor_core::{light_default, EditorState};
use editor_diff::{unified_decorations, unified_decorations_opts};
use editor_egui::{EditorWidget, PaintCache};
use editor_md::{
    callout_decorations, fold_decorations, footnote_decorations, frontmatter_fold,
    markdown_decorations, math_decorations, mermaid_decorations, transclusion_decorations,
    wikilink_decorations,
};
use editor_view::view::ViewState;
use std::collections::HashSet;
use std::sync::Arc;

use eframe::egui;

use editor_md::MarkdownIndent;

/// Per-preview cached editor/view pair. Recreated when the underlying text
/// changes (which for previews is rare — staging proposals are immutable
/// once written, snapshots never change, trash entries are read-only).
pub struct PreviewBuffer {
    /// Identity key (proposal_id / change_id / trashed_name). Used to
    /// invalidate the cache when a different preview lands in the tab.
    pub key: String,
    /// "After" side rope shown in the editor.
    pub editor: EditorState,
    pub view: ViewState,
    /// Per-preview galley cache reused across frames by `EditorWidget`.
    pub paint_cache: PaintCache,
    pub folds: HashSet<u64>,
    /// "Before" side text — kept so the diff toggle can rebuild
    /// decorations without re-reading from disk.
    pub before_text: String,
    /// "After" side text snapshot, mirrored from `editor.doc` once on
    /// build so the diff hunks don't recompute when the user just
    /// scrolls (read-only — the doc never changes after build).
    pub after_text: String,
    /// True when diff decorations are layered on top of markdown decos.
    pub diff_active: bool,
}

impl PreviewBuffer {
    pub fn new(key: String, before_text: String, after_text: String, diff_active: bool) -> Self {
        let editor = EditorState::new(&after_text);
        let mut view = ViewState {
            font_size: 15.0,
            indent_provider: Some(Arc::new(MarkdownIndent)),
            scroll_past_end: 0.3,
            read_only: true,
            ..ViewState::default()
        };
        view.wrap_map.set_enabled(true);
        Self {
            key,
            editor,
            view,
            paint_cache: PaintCache::default(),
            folds: HashSet::new(),
            before_text,
            after_text,
            diff_active,
        }
    }
}

/// Render the preview buffer body filling the remaining vertical space.
/// Backwards-compatible entry; uses the default intraline-on behaviour.
pub fn show(ui: &mut egui::Ui, buf: &mut PreviewBuffer) {
    show_with(ui, buf, /*intraline=*/ true);
}

pub fn show_with(ui: &mut egui::Ui, buf: &mut PreviewBuffer, intraline: bool) {
    let theme_owned = light_default();
    let theme = Some(&theme_owned);

    buf.view.decorations.clear();
    buf.view
        .decorations
        .push(markdown_decorations(&buf.editor, theme));
    buf.view
        .decorations
        .push(fold_decorations(&buf.editor, &buf.folds));
    buf.view
        .decorations
        .push(wikilink_decorations(&buf.editor, theme, None));
    buf.view
        .decorations
        .push(callout_decorations(&buf.editor, theme, None));
    buf.view
        .decorations
        .push(frontmatter_fold(&buf.editor, &buf.folds, theme));
    buf.view
        .decorations
        .push(transclusion_decorations(&buf.editor, theme, None));
    buf.view
        .decorations
        .push(footnote_decorations(&buf.editor, theme, None));
    buf.view
        .decorations
        .push(math_decorations(&buf.editor, theme, None));
    buf.view
        .decorations
        .push(mermaid_decorations(&buf.editor, theme, None));

    if buf.diff_active {
        let hunks = diff_lines(&buf.before_text, &buf.after_text);
        let line_height = buf.view.line_height.max(18.0);
        buf.view.decorations.push(unified_decorations_opts(
            &buf.editor.doc,
            &buf.before_text,
            &hunks,
            line_height,
            theme,
            intraline,
        ));
    }

    let _ = unified_decorations; // kept re-exported for back-compat callers

    EditorWidget::new(&mut buf.editor, &mut buf.view)
        .with_paint_cache(&mut buf.paint_cache)
        .show(ui);
}
