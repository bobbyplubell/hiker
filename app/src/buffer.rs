//! In-memory buffer state mirroring `ui/src/app/state.ts::Buffer`.
//!
//! Per-buffer ownership of the editor's `EditorState` + `ViewState` plus
//! the per-document interaction state (fold set, click action sink).
//! Keyed by vault-relative path. Switching tabs to a different buffer
//! just renders that buffer's editor state in the central pane.
//!
//! Dirty-derived rule: `is_dirty = current_text_hash != loaded_hash`. Same
//! single-source-of-truth posture as the TS impl — no separate flag that
//! can desync.

use std::collections::HashSet;
use std::sync::Arc;

use editor_core::{DecorationSet, EditorState};
use editor_egui::PaintCache;
use editor_md::MarkdownIndent;
use editor_view::view::ViewState;
use editor_view::ClickAction;

pub struct Buffer {
    /// Vault-relative path. Doubles as the key in `AppState::buffers`.
    pub path: String,
    /// Hash of the contents most recently read from / written to disk.
    pub loaded_hash: String,
    /// Snapshot of the on-disk content as of the last load/save. Used by
    /// the gutter index-diff decoration to mark lines that diverge from
    /// the indexed (= on-disk) version without re-reading the file every
    /// frame. Refreshed in lockstep with `loaded_hash`.
    pub loaded_text: String,
    /// Editor document + selection + history + decoration state.
    pub editor: EditorState,
    /// Editor viewport + layout cache.
    pub view: ViewState,
    /// Per-buffer galley cache reused across frames by `EditorWidget`.
    /// Lives on the buffer (rather than inside `ViewState` as it used to)
    /// so the editor-view crate stays free of egui types.
    pub paint_cache: PaintCache,
    /// Cached "indexer's stored content hash" for the badge in the
    /// status bar that warns when the buffer is ahead of the index.
    /// Refreshed on a coarse interval — without this the status bar
    /// fired a `Store::note_properties` SQLite query (+ mutex lock)
    /// every single frame just to render the badge state.
    pub index_hash_cache: Option<String>,
    pub index_hash_refreshed_at: Option<std::time::Instant>,
    /// Set of collapsed fold ids. Updated by `ClickAction::ToggleFold`.
    pub folds: HashSet<u64>,
    /// Per-frame click action sink consumed by `drain_clicks`.
    pub click_buffer: Vec<ClickAction>,
    /// Per-provider memo of last frame's decoration output, keyed by a
    /// fingerprint of the provider's inputs (doc identity, selection,
    /// viewport, folds, etc.). Hits avoid re-running the provider; misses
    /// recompute and replace the slot.
    pub decoration_cache: DecorationCache,
    /// View toggle: when true, render whitespace characters explicitly
    /// (spaces as · and tabs as → via a special-chars decoration layer).
    pub show_whitespace: bool,
    /// View toggle: when true, paint a faint red background over trailing
    /// whitespace at the end of every line. Independent of
    /// `show_whitespace` — that one is a glyph overlay across the whole
    /// doc; this one is a background highlight scoped to trailing runs.
    /// (`view-highlight-trailing-whitespace-toggle`)
    pub highlight_trailing_whitespace: bool,
    /// View toggle: when true, the frontmatter block at the top of the
    /// buffer is folded.
    pub hide_frontmatter: bool,
    /// View toggle: when true (default), the markdown live-preview layer
    /// applies — wikilink reveal, callouts, math, etc. When false, the
    /// buffer renders as plain monospace text. Per `view-live-preview-toggle`
    /// this is a per-buffer flip, not vault-wide.
    pub live_preview: bool,
    /// View toggle: when true, render `.txt` files with the markdown
    /// decoration stack. Default off — plain text stays plain.
    /// (`view-render-txt-as-markdown-toggle`)
    pub render_txt_as_markdown: bool,
    /// View toggle: when true, the status bar renders a breadcrumb of
    /// the heading path the cursor currently sits inside.
    /// (`view-heading-breadcrumb-toggle`)
    pub heading_breadcrumb: bool,
    /// View toggle: when true, the buffer paints the indexer's chunk
    /// boundaries as line decorations + gutter markers.
    /// (`view-show-chunk-boundaries`)
    pub chunk_boundaries: bool,
    /// View toggle: when true, the dirty-buffer diff view colors changes
    /// at character granularity. (`view-intraline-diff-toggle`)
    pub intraline_diff: bool,
    /// View toggle: when true (default), the structural minimap renders
    /// alongside the editor body. Per-buffer flip; vault default lives in
    /// `editor.show_minimap`.
    pub show_minimap: bool,
}

/// Slot for one cached decoration provider output.
#[derive(Clone, Default)]
pub struct CachedDeco {
    pub fingerprint: u64,
    pub result: DecorationSet,
}

/// Cache for paint-only decoration providers that walk the whole document
/// (or large viewport ranges) and would otherwise re-run every frame even
/// when none of their inputs changed.
///
/// Each slot is keyed by a u64 fingerprint of (doc.content_id, selection,
/// viewport, folds, theme). On a hit the cached `DecorationSet` is cloned
/// (cheap — Arc-shared SumTree); on a miss the slot is recomputed via the
/// caller-supplied closure.
#[derive(Default)]
pub struct DecorationCache {
    pub trailing_ws: Option<CachedDeco>,
    pub markdown: Option<CachedDeco>,
    pub fold: Option<CachedDeco>,
    pub wikilink: Option<CachedDeco>,
    pub callout: Option<CachedDeco>,
    pub frontmatter: Option<CachedDeco>,
    pub transclusion: Option<CachedDeco>,
    pub footnote: Option<CachedDeco>,
    pub math: Option<CachedDeco>,
    pub mermaid: Option<CachedDeco>,
    pub index_diff: Option<CachedDeco>,
    pub active_line: Option<CachedDeco>,
    pub occurrence: Option<CachedDeco>,
    pub bracket_match: Option<CachedDeco>,
    pub special_chars: Option<CachedDeco>,
    pub chunk_boundaries: Option<CachedDeco>,
}

impl DecorationCache {
    /// Either reuse the cached `DecorationSet` (when `fingerprint` matches)
    /// or compute a fresh one via `compute` and store it.
    pub fn get_or_compute<F: FnOnce() -> DecorationSet>(
        slot: &mut Option<CachedDeco>,
        fingerprint: u64,
        compute: F,
    ) -> DecorationSet {
        if let Some(cached) = slot.as_ref()
            && cached.fingerprint == fingerprint
        {
            return cached.result.clone();
        }
        let result = compute();
        *slot = Some(CachedDeco { fingerprint, result: result.clone() });
        result
    }
}

impl Buffer {
    pub fn from_disk(path: String, contents: String, loaded_hash: String) -> Self {
        Self::with_config(path, contents, loaded_hash, None)
    }

    /// Build a `Buffer`, initializing the view toggles from `cfg` when one
    /// is supplied. Falls back to the same defaults `from_disk` used.
    pub fn with_config(
        path: String,
        contents: String,
        loaded_hash: String,
        cfg: Option<&hiker_core::config::Config>,
    ) -> Self {
        Self::with_config_and_vault(path, contents, loaded_hash, cfg, None)
    }

    /// Build a `Buffer` and, when `vault` is supplied, register editor
    /// completion sources (currently the wikilink autocomplete) so `[[`
    /// inside the editor opens the vault-path picker.
    pub fn with_config_and_vault(
        path: String,
        contents: String,
        loaded_hash: String,
        cfg: Option<&hiker_core::config::Config>,
        vault: Option<Arc<hiker_core::vault::Vault>>,
    ) -> Self {
        let loaded_text = contents.clone();
        let editor = EditorState::new(&contents);
        let (wrap, show_ln, show_ws, highlight_trailing_ws, hide_fm) = match cfg {
            Some(c) => (
                c.editor.word_wrap,
                c.editor.show_line_numbers,
                c.editor.show_whitespace,
                c.editor.highlight_trailing_whitespace,
                c.editor.hide_frontmatter,
            ),
            None => (true, true, false, false, false),
        };
        // The "render this view as markdown" toggle is the union of
        // file-extension default (`.md` ⇒ on, anything else ⇒ off) and the
        // global `render_txt_as_markdown` flag for `.txt` files. The
        // per-buffer flag below lets the user override either direction.
        let is_md = path.rsplit_once('.')
            .map(|(_, ext)| ext.eq_ignore_ascii_case("md"))
            .unwrap_or(false);
        let is_txt = path.rsplit_once('.')
            .map(|(_, ext)| ext.eq_ignore_ascii_case("txt"))
            .unwrap_or(false);
        let render_txt_as_markdown = cfg
            .map(|c| c.editor.render_txt_as_markdown)
            .unwrap_or(false);
        let live_preview_default = cfg.map(|c| c.editor.live_preview).unwrap_or(true);
        let live_preview = live_preview_default
            && (is_md || (is_txt && render_txt_as_markdown));
        let mut view = ViewState {
            font_size: 15.0,
            indent_provider: Some(Arc::new(MarkdownIndent)),
            placeholder: Some("Start typing markdown…".into()),
            scroll_past_end: 0.3,
            ..ViewState::default()
        };
        view.wrap_map.set_enabled(wrap);
        view.hide_gutter = !show_ln;
        // Wikilink autocomplete: register a CompletionSource so typing `[[`
        // opens a vault-path picker. Only attached when a vault handle is
        // available (always true in normal app flow; preview buffers may
        // pass None to keep the source out of the read-only view).
        if let Some(v) = vault {
            view.completion_sources.push(Arc::new(
                crate::completion_sources::WikilinkSource { vault: v },
            ));
        }
        Self {
            path,
            loaded_hash,
            loaded_text,
            editor,
            view,
            paint_cache: PaintCache::default(),
            index_hash_cache: None,
            index_hash_refreshed_at: None,
            folds: HashSet::new(),
            click_buffer: Vec::new(),
            decoration_cache: DecorationCache::default(),
            show_whitespace: show_ws,
            highlight_trailing_whitespace: highlight_trailing_ws,
            hide_frontmatter: hide_fm,
            live_preview,
            render_txt_as_markdown,
            // Heading breadcrumb isn't yet a config-level toggle — drives
            // the status bar add-on; default off.
            heading_breadcrumb: false,
            chunk_boundaries: cfg
                .map(|c| c.editor.show_chunk_boundaries)
                .unwrap_or(false),
            intraline_diff: cfg
                .map(|c| c.editor.intraline_diff)
                .unwrap_or(false),
            show_minimap: cfg.map(|c| c.editor.show_minimap).unwrap_or(true),
        }
    }

    /// Replace the buffer's text contents in place while preserving
    /// scroll position, folds, decoration caches, paint cache, and the
    /// rest of the per-buffer UI state. Used for external-edit reloads
    /// (`maybe_reload_clean_buffer`) and wand-mutation applies — both
    /// previously rebuilt the whole `Buffer`, snapping the user's scroll
    /// to the top.
    ///
    /// Multi-cursor anchors are clamped to the new text length and
    /// snapped back to the nearest UTF-8 char boundary so cursors never
    /// dangle past EOF or land inside a multi-byte codepoint. History
    /// is intentionally not touched — the replace is silent, not an
    /// undoable edit (v0; legacy CM6 also reloaded out-of-band).
    pub fn replace_text(&mut self, new_text: String, new_loaded_hash: String) {
        use editor_core::{Rope, SelRange, Selection};

        let new_len = new_text.len();

        // Clamp every cursor anchor independently to a valid byte
        // boundary in the new text.
        let clamp = |byte: usize| -> usize {
            let mut b = byte.min(new_len);
            while b > 0 && !new_text.is_char_boundary(b) {
                b -= 1;
            }
            b
        };

        let old_sel = self.editor.selection.clone();
        let main_idx = old_sel.main_index();
        let clamped: Vec<SelRange> = old_sel
            .ranges()
            .iter()
            .map(|r| {
                let a = clamp(r.anchor.offset());
                let h = clamp(r.head.offset());
                let mut nr = SelRange::new(a, h);
                nr.goal_col = r.goal_col;
                nr
            })
            .collect();

        // Swap in the new doc + hashes.
        self.editor.doc = Rope::from_str(&new_text);
        self.editor.selection = Selection::from_ranges(clamped, main_idx);
        self.loaded_text = new_text;
        self.loaded_hash = new_loaded_hash;

        // Decoration caches are keyed off doc content_id / hash and will
        // self-invalidate on the next frame; clearing eagerly here would
        // just trade one frame of stale paint for a guaranteed full
        // recompute, so leave them alone. Same for `paint_cache`,
        // `folds`, `view` (scroll), and view toggles.
    }

    pub fn current_text(&self) -> String {
        self.editor.doc.to_string()
    }

    pub fn current_hash(&self) -> String {
        hiker_core::hash_str(&self.current_text())
    }

    pub fn is_dirty(&self) -> bool {
        self.current_hash() != self.loaded_hash
    }

    /// Drain the per-frame click buffer, applying fold toggles back into
    /// `self.folds`. Other click action kinds (`WidgetClick` etc.) are
    /// handled by the caller before this is invoked.
    pub fn drain_fold_clicks(&mut self) {
        self.click_buffer.retain(|action| {
            if let ClickAction::ToggleFold(id) = action {
                if !self.folds.remove(id) {
                    self.folds.insert(*id);
                }
                false
            } else {
                true
            }
        });
    }
}
