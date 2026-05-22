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
use editor_egui::widget::PaintCache;
use editor_md::indenter::MarkdownIndent;
use editor_view::viewport::ViewState;
use editor_view::viewport::ClickAction;

pub struct Buffer {
    /// What this buffer is sourcing — vault file, snapshot blob, staging
    /// proposal content, or trash entry. Drives the toolbar's per-source
    /// verb bar (Save vs. Restore vs. Accept/Reject vs. nothing) and the
    /// read-only flag in `view`. For Vault sources, the path here matches
    /// the key in `AppState::buffers`; for the others the storage key is
    /// composite (see `buffer_key_for_source`).
    pub source: crate::tab::BufferSource,
    /// Vault-relative path. Doubles as the key in `AppState::buffers`
    /// for vault sources; for non-vault sources, the path identifies the
    /// underlying note (e.g. the original path of a trashed file, the
    /// target path of a staging proposal, the path the snapshot is *of*).
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
    /// View toggle: when true, suppress the auto-hiding right-edge
    /// scrollbar that fills in for the minimap when it's hidden. Only
    /// observed while `show_minimap = false` — when the minimap is on it
    /// already serves as the scroll affordance. Vault default lives in
    /// `editor.hide_scrollbar`.
    pub hide_scrollbar: bool,
    /// Snapshot of the disk text at the moment any pending `edit_note`
    /// proposals were hydrated into the live buffer. `None` when no
    /// proposals applied — the buffer is just plain editing. When `Some`,
    /// the inline patch-review surface renders `DiffLayer(agent_base,
    /// current, Agent)` and per-hunk accept/reject mutates these two
    /// ropes plus removes the contributing proposals from `staging.db`.
    /// Per `patch-review-buffer-hydration` in `patch-review.md`.
    pub agent_base: Option<String>,
    /// IDs of the proposals that were applied to `current` at hydration
    /// time. Saving the buffer is refused while this is non-empty — the
    /// user must individually accept or reject every hunk so each accepted
    /// proposal writes its `changes.db` audit row before its content
    /// reaches disk. Per `patch-review-hydrate-dehydrate`.
    pub hydrated_proposals: Vec<String>,
    /// Byte ranges in `current` that each hydrated proposal's `new_str`
    /// occupies, recorded as proposals applied (left-to-right) and shifted
    /// forward as later proposals lengthen / shorten earlier byte
    /// positions. Used by the per-hunk Accept/Reject widgets to map a
    /// hunk's byte range back to the proposal(s) that contributed to it.
    /// One proposal can produce multiple entries when `replace_all=true`.
    pub hydration_footprints: Vec<(String, std::ops::Range<usize>)>,
    /// Hash of `editor.doc` right after the last hydration pass. Lets the
    /// per-frame re-hydration check tell "the user has typed since
    /// hydration" (current hash drifted) apart from "staging changed
    /// underneath us" (current hash still matches but pending proposal
    /// IDs differ). Re-hydration is skipped in the former case so we
    /// never clobber user edits.
    pub post_hydration_hash: Option<String>,
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
        BufferSource::Snapshot { change_id, path } => {
            format!("\0snapshot:{}:{}", change_id, path)
        }
        BufferSource::StagingProposal { proposal_id, .. } => {
            format!("\0staging:{}", proposal_id)
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
            hide_scrollbar: cfg.map(|c| c.editor.hide_scrollbar).unwrap_or(false),
            agent_base: None,
            hydrated_proposals: Vec::new(),
            hydration_footprints: Vec::new(),
            post_hydration_hash: None,
        }
    }

    /// Apply pending `edit_note` proposals targeting this buffer's path,
    /// snapshotting the pre-apply text as `agent_base`. After this returns,
    /// `current` reflects "disk + applied proposals" and `hydrated_proposals`
    /// carries the ids that contributed. Per `patch-review-buffer-hydration`.
    ///
    /// Conflicted proposals (where the edit can't apply against the
    /// partially-applied text) are surfaced separately via the staging
    /// service's eager-recheck path; this routine just skips them and
    /// records the successes.
    pub fn hydrate_pending_proposals(
        &mut self,
        staging: &hiker_core::staging::Staging,
    ) {
        let filter = hiker_core::staging::types::Filter {
            path: Some(self.path.clone()),
            ..Default::default()
        };
        let proposals = match staging.list(&filter) {
            Ok(p) => p,
            Err(_) => return,
        };

        let mut running = self.loaded_text.clone();
        let mut applied: Vec<String> = Vec::new();
        let mut footprints: Vec<(String, std::ops::Range<usize>)> = Vec::new();

        for p in &proposals {
            if p.action != "edit_note" {
                continue;
            }
            let Some(edit) = p.edit.as_ref() else { continue };
            let matches = self.find_all(&running, &edit.old_str);
            if matches.is_empty()
                || (matches.len() > 1 && !edit.replace_all)
            {
                continue;
            }
            let old_len = edit.old_str.len();
            let new_len = edit.new_str.len();
            let delta = new_len as isize - old_len as isize;

            // Build the post-apply text in one pass so we can record each
            // replacement's new byte position before the next match.
            let mut next = String::with_capacity(
                (running.len() as isize + delta * matches.len() as isize).max(0) as usize,
            );
            let mut cursor = 0usize;
            let mut new_positions: Vec<std::ops::Range<usize>> = Vec::with_capacity(matches.len());
            for m_start in &matches {
                next.push_str(&running[cursor..*m_start]);
                let new_pos = next.len();
                next.push_str(&edit.new_str);
                new_positions.push(new_pos..new_pos + new_len);
                cursor = m_start + old_len;
            }
            next.push_str(&running[cursor..]);

            // Shift earlier footprints forward to reflect the bytes added
            // (or removed) by every match that lies strictly before them.
            for (_pid, fp) in footprints.iter_mut() {
                let shift_start = shift_for_position(fp.start, &matches, old_len, delta);
                let shift_end = shift_for_position(fp.end, &matches, old_len, delta);
                fp.start = (fp.start as isize + shift_start) as usize;
                fp.end = (fp.end as isize + shift_end) as usize;
            }

            for pos in new_positions {
                footprints.push((p.id.clone(), pos));
            }
            running = next;
            applied.push(p.id.clone());
        }

        // Always reseat hydration state — callers ensure this is only
        // called when `editor.doc` matches the previously-recorded
        // post-hydration text (or it's the first hydration pass right
        // after a disk read), so reseating never clobbers user edits.
        self.editor.doc = editor_core::rope::Rope::from_str(&running);
        self.post_hydration_hash = Some(hiker_core::hash_string(&running));
        if applied.is_empty() {
            self.agent_base = None;
            self.hydrated_proposals.clear();
            self.hydration_footprints.clear();
        } else {
            self.agent_base = Some(self.loaded_text.clone());
            self.hydrated_proposals = applied;
            self.hydration_footprints = footprints;
        }
        // loaded_text / loaded_hash deliberately stay as the disk values
        // so `is_dirty()` flips true while hydrated content is live in
        // the buffer.
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

impl Buffer {
    /// All start positions where `needle` occurs in `haystack`. Mirrors the
    /// staging service's internal `find_all_matches` so hydration's
    /// footprint tracking sees the same positions as `apply_edit`. A method
    /// (its sole caller is `hydrate_pending_proposals`) so it doesn't trip
    /// `single_call_fn`.
    fn find_all(&self, haystack: &str, needle: &str) -> Vec<usize> {
        if needle.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut from = 0;
        while let Some(rel) = haystack[from..].find(needle) {
            let pos = from + rel;
            out.push(pos);
            from = pos + needle.len();
        }
        out
    }
}

/// Compute the byte offset to add to an earlier footprint position,
/// given the list of new-replacement positions in *running* text from
/// the current proposal. Each match whose end lies at or before `pos`
/// shifts `pos` by `delta` bytes.
fn shift_for_position(pos: usize, matches: &[usize], old_len: usize, delta: isize) -> isize {
    let mut shift = 0isize;
    for m_start in matches {
        if *m_start + old_len <= pos {
            shift += delta;
        }
    }
    shift
}
