//! In-memory buffer state mirroring `ui/src/app/state.ts::Buffer`.
//!
//! Per-buffer ownership of the editor's `Editor` + `ViewState` plus
//! the per-document interaction state (fold set, click action sink).
//! Keyed by vault-relative path. Switching tabs to a different buffer
//! just renders that buffer's editor state in the central pane.
//!
//! Dirty-derived rule: `is_dirty = current_text_hash != loaded_hash`. Same
//! single-source-of-truth posture as the TS impl — no separate flag that
//! can desync.

use std::collections::HashSet;
use std::sync::Arc;

use editor_core::decoration::Set;
use editor_core::state::Editor;
use editor_egui::minimap::Cache as MinimapCache;
use editor_egui::widget::PaintCache;
use editor_md::indenter::MarkdownIndent;
use editor_view::viewport::ViewState;
use editor_view::viewport::ClickAction;

/// Compile a double/triple-click selection regex from config. Always returns a
/// valid regex: empty string → `default` (the shared lazy default; lets users
/// clear the field to reset). Invalid pattern → log once and use `default`, so
/// a config typo can never break click selection. status: click-select-pattern
fn compile_click_pattern(
    pattern: &str,
    which: &str,
    default: fn() -> Arc<regex::Regex>,
) -> Arc<regex::Regex> {
    if pattern.is_empty() {
        return default();
    }
    match regex::Regex::new(pattern) {
        Ok(re) => Arc::new(re),
        Err(err) => {
            tracing::warn!(
                target: "ui::editor",
                pattern, which, error = %err,
                "invalid editor click-select pattern; using default",
            );
            default()
        }
    }
}

/// Pattern strings cached on the buffer alongside the compiled regexes, so the
/// per-frame `sync_click_patterns` call can skip recompiling when the user
/// hasn't actually edited the setting. Default-initialized to whatever was
/// passed at construction; `sync_click_patterns` keeps them in lockstep with
/// `view.double_click_re` / `view.triple_click_re`.
#[derive(Default, Clone)]
pub struct ClickPatternCache {
    pub double_src: String,
    pub triple_src: String,
}

pub struct Buffer {
    /// What this buffer is sourcing — vault file, snapshot blob, pending
    /// proposal content, or trash entry. Drives the toolbar's per-source
    /// verb bar (Save vs. Restore vs. Accept/Reject vs. nothing) and the
    /// read-only flag in `view`. For Vault sources, the path here matches
    /// the key in `AppState::buffers`; for the others the storage key is
    /// composite (see `buffer_key_for_source`).
    pub source: crate::tab::BufferSource,
    /// Vault-relative path. Doubles as the key in `AppState::buffers`
    /// for vault sources; for non-vault sources, the path identifies the
    /// underlying note (e.g. the original path of a trashed file, the
    /// target path of a pending proposal, the path the snapshot is *of*).
    pub path: String,
    /// Hash of the contents most recently read from / written to disk.
    pub loaded_hash: String,
    /// Snapshot of the on-disk content as of the last load/save. Used by
    /// the gutter index-diff decoration to mark lines that diverge from
    /// the indexed (= on-disk) version without re-reading the file every
    /// frame. Refreshed in lockstep with `loaded_hash`.
    pub loaded_text: String,
    /// Editor document + selection + history + decoration state.
    pub editor: Editor,
    /// Editor viewport + layout cache.
    pub view: ViewState,
    /// Per-buffer galley cache reused across frames by `Widget`.
    /// Lives on the buffer (rather than inside `ViewState` as it used to)
    /// so the editor-view crate stays free of egui types.
    pub paint_cache: PaintCache,
    /// Per-buffer minimap metrics/classification cache. Same rationale as
    /// `paint_cache`: lives on the buffer so the minimap recomputes its
    /// O(lines) measure+classify pass only when the doc or its decorations
    /// change, not on every scroll frame.
    pub minimap_cache: MinimapCache,
    /// Cached "indexer's stored content hash" for the badge in the
    /// status bar that warns when the buffer is ahead of the index.
    /// Refreshed on a coarse interval — without this the status bar
    /// fired a `Store::note_properties` SQLite query (+ mutex lock)
    /// every single frame just to render the badge state.
    pub index_hash_cache: Option<String>,
    pub index_hash_refreshed_at: Option<std::time::Instant>,
    /// Last observed indexer `is_pending` state for this path. The status
    /// bar forces an out-of-band hash re-read on the pending → done
    /// transition so the "buffer ahead of index" badge clears the instant
    /// indexing completes rather than latching until the next coarse timer
    /// tick. `true` initially so a buffer opened mid-index still picks up
    /// the post-index refresh.
    pub index_pending_last: bool,
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
    /// View toggle: when true, suppress the auto-hiding right-edge
    /// scrollbar that fills in for the minimap when it's hidden. Only
    /// observed while `show_minimap = false` — when the minimap is on it
    /// already serves as the scroll affordance. Vault default lives in
    /// `editor.hide_scrollbar`.
    pub hide_scrollbar: bool,
    /// `materialize_review(session)` — the agent's *proposal*
    /// (`working + pending`) — captured each frame by the buffer panel's
    /// per-frame binding. `None` when the active session has no pending ops on
    /// this document (then `materialize_review == materialize_working` and the
    /// buffer is plain editing). When `Some`, the editable buffer
    /// (`editor.doc == materialize_working`) holds only the user's text and the
    /// inline patch-review surface diffs the buffer against this proposal
    /// (`working` → `review`), rendering the agent's pending ops as a suggestion
    /// overlay — additions as phantom blocks, deletions struck through; per-hunk
    /// accept/reject flips the contributing pending op ids through
    /// `core::ops::op_writes::flip_op_status`. Per `patch-review-buffer-state`
    /// in `patch-review.md`.
    /// Source strings for the compiled `view.double_click_re` / `triple_click_re`,
    /// kept in sync with config by the per-frame `sync_click_patterns` call.
    /// Tracking the strings (not just the compiled regexes) lets the sync skip
    /// recompiling when the setting hasn't changed. status: click-select-pattern
    pub click_patterns: ClickPatternCache,
    /// Per-buffer find-bar UI state (`editor-find-in-note`). The match
    /// engine itself lives in `editor_view::find::SearchState` on the
    /// view; this struct is the host-side UI bits — bar open/closed,
    /// query draft, debounce timestamps, error / wrapped hints, and the
    /// saved selection restored when Esc closes the bar.
    pub find_ui: FindUi,
    /// Per-buffer reader / focus view toggle (`editor-reader-view`).
    /// When true, the buffer panel hides its toolbar + status bar, and
    /// the host hides the window-level chrome (top toolbar, side bars,
    /// activity bar, status bar) around the active editor.
    pub reader_view: bool,
    pub agent_proposal: Option<String>,
    /// Which agent session's pending ops are in scope for the inline review.
    /// `None` selects the whole pending queue (all sessions). The file pill
    /// flips this when the user picks a session row, and the diff overlay /
    /// accept-reject pass it to the op-log seams so the hunks and flips are
    /// scoped to one session at a time. Per `patch-review-multi-session`.
    pub active_session: Option<String>,
}

/// Per-buffer find-bar UI state (`editor-find-in-note`). The match index
/// itself lives in `view.search` (`editor_view::find::SearchState`); this
/// struct is the host-side bits that don't belong in the editor crate.
#[derive(Default)]
pub struct FindUi {
    /// Whether the find bar is visible on the buffer panel.
    pub open: bool,
    /// Most-recent regex parse error, when the regex toggle is on and
    /// the pattern doesn't compile. Cleared on every successful run.
    pub regex_error: Option<String>,
    /// When the user edited the query last; the buffer panel debounces
    /// match-index rebuilds ~150ms off this.
    pub query_dirty_at: Option<std::time::Instant>,
    /// When a wrap happened last; the panel shows a one-shot
    /// `Wrapped to top` / `Wrapped to bottom` hint until ~1.2s elapses.
    pub wrapped_hint_at: Option<std::time::Instant>,
    /// Direction of the most recent wrap (`true` = wrapped past end →
    /// jumped to top, `false` = past start → jumped to bottom). Drives
    /// the hint text.
    pub wrapped_forward: bool,
    /// Selection snapshot captured when the bar was opened; restored on
    /// Esc so closing the bar puts the cursor back where the user was
    /// (per `editor-find-in-note`'s "Esc closes and returns selection to
    /// the active match" — we keep the explicit pre-bar selection so the
    /// user can undo a wandering "next match" walk in one keystroke).
    pub saved_selection: Option<editor_core::selection::Selection>,
    /// Set true on open so the buffer panel can request keyboard focus
    /// on the find input on the next paint.
    pub focus_next_frame: bool,
}

/// Slot for one cached decoration provider output.
#[derive(Clone, Default)]
pub struct CachedDeco {
    pub fingerprint: u64,
    pub result: Set,
}

/// Cache for paint-only decoration providers that walk the whole document
/// (or large viewport ranges) and would otherwise re-run every frame even
/// when none of their inputs changed.
///
/// Each slot is keyed by a u64 fingerprint of (doc.content_id, selection,
/// viewport, folds, theme). On a hit the cached `Set` is cloned
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
    /// Either reuse the cached `Set` (when `fingerprint` matches)
    /// or compute a fresh one via `compute` and store it.
    pub fn get_or_compute<F: FnOnce() -> Set>(
        slot: &mut Option<CachedDeco>,
        fingerprint: u64,
        compute: F,
    ) -> Set {
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

/// Stable storage key for a `BufferSource`. Vault sources key on path
/// (unprefixed, to match every existing `app.session.buffers.get(path)`
/// site); the read-only preview sources prefix to avoid colliding with a
/// vault file whose name happens to look like an id.
pub fn buffer_key_for_source(source: &crate::tab::BufferSource) -> String {
    use crate::tab::BufferSource;
    match source {
        BufferSource::Vault { path } => path.clone(),
        BufferSource::Snapshot { op_id, path } => {
            format!("\0snapshot:{}:{}", op_id, path)
        }
        BufferSource::PendingProposal { proposal_id, .. } => {
            format!("\0pending:{}", proposal_id)
        }
        BufferSource::Trash { trash_path, .. } => {
            format!("\0trash:{}", trash_path)
        }
    }
}

impl Buffer {
    /// Build a `Buffer` and, when `vault` is supplied, register editor
    /// completion sources (currently the wikilink autocomplete) so `[[`
    /// inside the editor opens the vault-path picker.
    pub fn with_config_and_vault(
        path: String,
        contents: &str,
        loaded_hash: String,
        cfg: Option<&hiker_core::config::Config>,
        vault: Option<Arc<hiker_core::vault::Vault>>,
    ) -> Self {
        let loaded_text = contents.to_string();
        let editor = Editor::new(contents);
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
        // Configurable double/triple-click selection: compile the user's
        // `editor.{double,triple}_click_pattern` regexes. Empty = built-in
        // word/line selection; an invalid pattern logs once and falls back to
        // built-in so a typo never breaks selection. status: click-select-pattern
        let double_src = cfg.map(|c| c.editor.double_click_pattern.clone()).unwrap_or_default();
        let triple_src = cfg.map(|c| c.editor.triple_click_pattern.clone()).unwrap_or_default();
        view.double_click_re = compile_click_pattern(
            &double_src,
            "double_click_pattern",
            editor_view::viewport::default_double_click_regex,
        );
        view.triple_click_re = compile_click_pattern(
            &triple_src,
            "triple_click_pattern",
            editor_view::viewport::default_triple_click_regex,
        );
        let click_patterns = ClickPatternCache { double_src, triple_src };
        // Wikilink autocomplete: register a Source so typing `[[`
        // opens a vault-path picker. Only attached when a vault handle is
        // available (always true in normal app flow; preview buffers may
        // pass None to keep the source out of the read-only view).
        if let Some(v) = vault {
            view.completion_sources.push(Arc::new(
                crate::completion_sources::WikilinkSource { vault: v },
            ));
        }
        Self {
            source: crate::tab::BufferSource::Vault { path: path.clone() },
            path,
            loaded_hash,
            loaded_text,
            editor,
            view,
            paint_cache: PaintCache::default(),
            minimap_cache: MinimapCache::default(),
            index_hash_cache: None,
            index_hash_refreshed_at: None,
            index_pending_last: true,
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
            hide_scrollbar: cfg.map(|c| c.editor.hide_scrollbar).unwrap_or(false),
            click_patterns,
            find_ui: FindUi::default(),
            reader_view: false,
            agent_proposal: None,
            active_session: None,
        }
    }

    /// Re-derive `view.double_click_re` / `view.triple_click_re` from the live
    /// config when the user has edited the patterns since this buffer was
    /// constructed. Cheap when nothing changed (two string compares); only
    /// recompiles the regex on an actual edit. Wired into the buffer panel's
    /// per-frame render path so config changes take effect without needing to
    /// close and reopen the file. status: click-select-pattern
    /// Apply pre-detected changes to the click-select patterns. Each arg is
    /// `Some(new_src)` when the live config differs from the cached source and
    /// `None` otherwise — the caller does the equality check under the config
    /// read lock so the no-change frame never allocates. status:
    /// click-select-pattern
    pub fn sync_click_patterns(&mut self, double_src: Option<String>, triple_src: Option<String>) {
        if let Some(src) = double_src {
            self.view.double_click_re = compile_click_pattern(
                &src,
                "double_click_pattern",
                editor_view::viewport::default_double_click_regex,
            );
            self.click_patterns.double_src = src;
        }
        if let Some(src) = triple_src {
            self.view.triple_click_re = compile_click_pattern(
                &src,
                "triple_click_pattern",
                editor_view::viewport::default_triple_click_regex,
            );
            self.click_patterns.triple_src = src;
        }
    }

    /// Swap `editor.doc` to `new_text` and clamp every selection range to a
    /// valid char boundary within it. The reverse half of the editor binding
    /// (per `op-log-editor-binding`) calls this when `materialize_working`
    /// advanced without user typing — an agent op was accepted, or an external
    /// edit landed — so the editable buffer follows. Re-pointing to a shorter
    /// materialization (e.g. an accepted agent delete) would otherwise leave
    /// the cursor past the new end, and the next paint's `byte_to_line(cursor)`
    /// panics with "byte offset out of range".
    ///
    /// status: op-log-editor-binding
    pub fn set_doc_clamping_selection(&mut self, new_text: &str) {
        use editor_core::rope::Rope;
        use editor_core::selection::{SelRange, Selection};
        let new_len = new_text.len();
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
                let mut nr = SelRange::new(clamp(r.anchor.offset()), clamp(r.head.offset()));
                nr.goal_col = r.goal_col;
                nr
            })
            .collect();
        self.editor.doc = Rope::from_str(new_text);
        self.editor.selection = Selection::from_ranges(clamped, main_idx);
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
        use editor_core::rope::Rope;

        use editor_core::selection::SelRange;

        use editor_core::selection::Selection;
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
        hiker_core::hash_string(&self.current_text())
    }

    pub fn is_dirty(&self) -> bool {
        self.current_hash() != self.loaded_hash
    }

    /// Whether the "buffer ahead of index" status badge should be shown:
    /// the indexer has a known content hash for this path (`index_hash_cache`)
    /// *and* it differs from the buffer's live content hash. A `None` cache
    /// (path never indexed) means there's nothing to be ahead of, so the
    /// badge stays hidden; equal hashes — the steady state once a re-index
    /// catches up — also hide it. Recomputed on demand from the cached
    /// stored-hash, never latched, so the badge clears as soon as the cache
    /// reflects the post-index value. The status bar (`panels::buffer`) is
    /// responsible for keeping `index_hash_cache` fresh.
    pub fn is_ahead_of_index(&self) -> bool {
        matches!(self.index_hash_cache.as_deref(), Some(h) if h != self.current_hash())
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

#[cfg(test)]
mod index_badge_tests {
    use super::Buffer;

    fn buf(text: &str, indexed_hash: Option<&str>) -> Buffer {
        let mut b = Buffer::with_config_and_vault(
            "test.md".to_string(),
            text,
            String::new(),
            None,
            None,
        );
        b.index_hash_cache = indexed_hash.map(str::to_string);
        b
    }

    #[test]
    fn hidden_when_path_never_indexed() {
        // No stored hash → nothing to be ahead of.
        assert!(!buf("hello", None).is_ahead_of_index());
    }

    #[test]
    fn shown_when_buffer_diverges_from_index() {
        // A stale stored hash that won't match the live content.
        assert!(buf("hello world", Some("stale-hash")).is_ahead_of_index());
    }

    #[test]
    fn clears_once_index_catches_up() {
        // The latch case: after re-indexing, the stored hash equals the
        // buffer's live hash and the badge must recompute to hidden.
        let mut b = buf("hello world", None);
        let live = b.current_hash();
        b.index_hash_cache = Some(live);
        assert!(!b.is_ahead_of_index());
    }
}
