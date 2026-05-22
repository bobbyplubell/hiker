//! Buffer tab body: editor toolbar strip, the editor widget itself, then
//! the status bar. Buffer-only chrome stays here (hidden for all
//! non-buffer kinds).
#![allow(clippy::items_after_test_module)]

pub mod decorations;
pub mod diff_overlay;
pub mod patch_review_pill;
pub mod scrollbar;
pub mod show_changes;
pub mod toolbar_menus;

use std::sync::Arc;

use eframe::egui;

use decorations::EditorDecorations;

use editor_core::theme::light_default;
use editor_egui::widget::Widget as EditorWidget;
use editor_egui::minimap::Options as MinimapOptions;
use editor_egui::minimap::Widget as MinimapWidget;
use editor_md::admonitions::callout_decorations;
use editor_md::folds::fold_decorations;
use editor_md::notes::footnote_decorations;
use editor_md::meta::frontmatter_fold;
use editor_md::styling::markdown_decorations;
use editor_md::equations::math_decorations;
use editor_md::diagrams::mermaid_decorations;
use editor_md::embeds::transclusion_decorations;
use editor_md::links::wikilink_decorations;
use editor_view::brackets::DEFAULT_BRACKETS;
use editor_view::highlight::occurrence_decorations;
use editor_view::brackets::bracket_match_decorations;

use editor_view::highlights::active_line_decorations;

use editor_view::highlights::trailing_whitespace_decorations;
use editor_view::whitespace::special_chars_decorations;

use editor_view::whitespace::SpecialCharsFlags;
use editor_view::viewport::ClickAction;

use crate::buffer::DecorationCache;
use crate::editor_pane;
use crate::icons;
use crate::state::{AppState, ToastLevel};
use crate::theme;


/// Combine multiple u64 values into a single fingerprint via splitmix-style
/// hashing. Order-dependent.
const fn mix(seed: u64, x: u64) -> u64 {
    let mut z = seed.wrapping_add(x).wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Context bundle for the buffer panel: UI, app, and the active buffer
/// path. Most helpers in this file share these three arguments — making
/// them methods on `&mut self` keeps them factored without tripping
/// `clippy::single_call_fn` (the lint exempts methods with a `self`
/// receiver).
struct BufCtx<'a> {
    ui: &'a mut egui::Ui,
    app: &'a mut AppState,
    path: &'a str,
}

/// Heading-breadcrumb lookup on a buffer. Standalone trait so the tests
/// can call it without going through the panel context.
trait HeadingBreadcrumb {
    /// Walk the document from the start up through the cursor's line
    /// and return a `>`-joined breadcrumb of the active heading stack.
    fn heading_breadcrumb(&self) -> String;
}

pub fn show(
    ui: &mut egui::Ui,
    app: &mut AppState,
    path: &str,
    _rt: &Arc<tokio::runtime::Runtime>,
) {
    BufCtx { ui, app, path }.show();
}

impl<'a> BufCtx<'a> {
    /// Top-level buffer-panel body: toolbar, optional pending-rewrite
    /// banner, inline diff overlay, then the editor itself.
    fn show(&mut self) {
        // Toolbar across the top of the buffer tab body.
        self.toolbar();

        // Pending-rewrite banner: thin row that surfaces a write-shaped
        // proposal targeting this note. Only meaningful for vault
        // buffers — a snapshot / staging / trash tab doesn't surface
        // side proposals.
        let is_vault = self
            .app
            .session
            .buffers
            .get(self.path)
            .map(|b| matches!(&b.source, crate::tab::BufferSource::Vault { .. }))
            .unwrap_or(false);
        if is_vault {
            self.pending_rewrite_banner();
        }

        // Build the inline diff overlay once. Drives both the file pill
        // (counts + Next-hunk + bulk verbs above the editor) and the
        // in-buffer decorations pushed by `show_editor`. Owner-aware:
        // Agent for hydrated proposals, Manual / Snapshot / Staging for
        // the dirty-buffer diff toggle / history viewer / staging-
        // proposal review.
        let overlay = self.app.diff_overlay_for(self.path);
        if let Some(ov) = &overlay
            && matches!(ov.owner, editor_diff::DiffOwner::Agent)
        {
            let cursor_byte = self.cursor_byte();
            let pill_action = patch_review_pill::Pill { ui: self.ui }
                .show(&ov.hunks, cursor_byte);
            self.apply_pill_action(&pill_action);
        }

        self.ui.add_space(4.0);

        // Hoist captures so the egui closure sees disjoint `&mut`s on
        // `ui` vs `app`.
        let Self { ui, app, path } = self;
        let path: &str = *path;
        let overlay_ref = overlay.as_ref();
        egui::Frame::default().show(*ui, |ui| {
            let body_height = ui.available_height().max(80.0);
            let (rect, _resp) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), body_height),
                egui::Sense::hover(),
            );
            app.session.nav.swipe_skip_rects.push(rect);
            let mut body_ui = ui.new_child(egui::UiBuilder::new().max_rect(rect));
            BufCtx { ui: &mut body_ui, app, path }.show_editor(overlay_ref);
        });
    }

    /// Cursor offset in the buffer keyed by `self.path`, or 0 if the
    /// buffer isn't loaded.
    fn cursor_byte(&self) -> usize {
        self.app
            .session
            .buffers
            .get(self.path)
            .map(|b| b.editor.selection.main().head.offset())
            .unwrap_or(0)
    }

    /// Resolve a pill bulk action against the hydrated agent proposals on
    /// this buffer. Accept-all iterates the buffer's `hydrated_proposals`
    /// list and calls `staging.accept` on each (which writes the
    /// changes-db audit row and removes the proposal); Reject-all calls
    /// `staging.reject` on each. After either, the buffer is re-read
    /// from disk and re-hydrated to reflect the post-action state.
    fn apply_pill_action(&mut self, action: &patch_review_pill::PillAction) {
        let app = &mut *self.app;
        let path: &str = self.path;
        let proposal_ids: Vec<String> = app
            .session
            .buffers
            .get(path)
            .map(|b| b.hydrated_proposals.clone())
            .unwrap_or_default();
        if action.accept_all && !proposal_ids.is_empty() {
            let staging = app.vault_session.services.staging.clone();
            let changes = app.vault_session.services.changes.clone();
            let (mut ok, mut err) = (0usize, 0usize);
            for id in &proposal_ids {
                match staging.accept(id, &app.vault_session.vault, Some(changes.as_ref())) {
                    Ok(_) => ok += 1,
                    Err(_) => err += 1,
                }
            }
            Self::reload_and_rehydrate(app, path);
            app.push_toast(
                if err == 0 {
                    format!("Accepted {} hunk{}", ok, if ok == 1 { "" } else { "s" })
                } else {
                    format!("Accepted {}, {} failed", ok, err)
                },
                if err == 0 { ToastLevel::Info } else { ToastLevel::Error },
            );
        }
        if action.reject_all && !proposal_ids.is_empty() {
            let staging = app.vault_session.services.staging.clone();
            let mut n = 0usize;
            for id in &proposal_ids {
                if staging.reject(id).is_ok() {
                    n += 1;
                }
            }
            Self::reload_and_rehydrate(app, path);
            app.push_toast(
                format!("Rejected {} hunk{}", n, if n == 1 { "" } else { "s" }),
                ToastLevel::Info,
            );
        }
        if let Some(byte) = action.scroll_to_byte
            && let Some(buffer) = app.session.buffers.get_mut(path)
        {
            let line = buffer.editor.doc.byte_to_line(byte);
            let target_y = buffer.view.height_map.y_at_row_top(line) - 24.0;
            buffer.view.scroll_y = target_y.max(0.0);
        }
    }

    /// Re-read disk into the buffer and re-apply the buffer-hydration
    /// step so the inline diff layer reflects whatever proposals remain
    /// in `staging.db` after a bulk accept/reject. Called from multiple
    /// methods on `BufCtx` (apply_pill_action, handle_hunk_accept,
    /// handle_hunk_reject), so it's not single-call.
    fn reload_and_rehydrate(app: &mut AppState, path: &str) {
        let _ = editor_pane::reload_from_disk(app, path);
        let staging = app.vault_session.services.staging.clone();
        if let Some(buffer) = app.session.buffers.get_mut(path) {
            buffer.agent_base = None;
            buffer.hydrated_proposals.clear();
            buffer.hydrate_pending_proposals(staging.as_ref());
        }
    }

    fn show_editor(&mut self, diff: Option<&diff_overlay::DiffOverlay>) {
        let ui = &mut *self.ui;
        let app = &mut *self.app;
        let path: &str = self.path;
    crate::profile_function!();
    // Cmd-S / Ctrl-S save shortcut at the buffer level. We intercept
    // before the editor consumes the event since the editor doesn't bind
    // it (save is a host concern).
    let save_pressed = ui.input(|i| {
        i.key_pressed(egui::Key::S) && i.modifiers.command_only()
    });
    if save_pressed
        && let Err(err) = editor_pane::save_buffer(app, path)
    {
        app.push_toast(format!("Save failed: {}", err), ToastLevel::Error);
    }

    // Read scroll speed up-front so the immutable config borrow doesn't
    // collide with the mutable buffer borrow below. The view also reads
    // this each frame so changing the setting takes effect immediately.
    let scroll_speed = app
        .vault_session
        .config
        .read()
        .map(|c| c.editor.scroll_speed)
        .unwrap_or(1.0)
        .max(0.0);

    let Some(buffer) = app.session.buffers.get_mut(path) else {
        ui.label(format!("buffer {} not loaded", path));
        return;
    };
    buffer.view.scroll_speed = scroll_speed;

    // Rebuild decoration layers from current state. Most decoration
    // providers take an Option<&Theme> so they can fall back to a
    // built-in palette when the host hasn't supplied one.
    let theme_owned = light_default();
    let theme = Some(&theme_owned);
    // Compute the visible byte range up-front so we can scope paint-only
    // providers to the viewport.
    let visible = buffer.view.visible_lines();
    let last_line = buffer.editor.doc.len_lines().saturating_sub(1);
    let visible_start = buffer
        .editor
        .doc
        .line_to_byte(visible.start.min(last_line));
    let visible_end_line = visible.end.min(last_line);
    let visible_end = if visible_end_line + 1 < buffer.editor.doc.len_lines() {
        buffer.editor.doc.line_to_byte(visible_end_line + 1)
    } else {
        buffer.editor.doc.len_bytes()
    };
    let visible_range = visible_start..visible_end;

    // Fingerprint inputs for memoized providers. `content_id` is an Arc
    // pointer into the rope tree — changes only on doc edits, so idle / pure
    // scroll frames hit the cache.
    let doc_id = buffer.editor.doc.content_id() as u64;
    let sel = buffer.editor.selection.main().head.offset() as u64;
    // Layers whose only cursor dependence is "is the cursor on this line?"
    // (markdown reveal, wikilink reveal) key on the line index instead of
    // the byte offset — otherwise a selection drag busts the cache on every
    // byte and reparses the whole doc per frame.
    let cursor_line = buffer.editor.doc.byte_to_line(sel as usize) as u64;
    // Inlined `folds_hash`: XOR-mix the fold ids in an order-independent
    // way. Cheap and stable for memoization keys (HashSet iteration order
    // isn't deterministic).
    let folds_id: u64 = {
        let mut h: u64 = 0;
        for &id in &buffer.folds {
            h ^= id.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
        h
    };
    let vp_lo = visible_start as u64;
    let vp_hi = visible_end as u64;
    let vp_fp = mix(vp_lo, vp_hi);
    let cache: &mut DecorationCache = &mut buffer.decoration_cache;

    crate::profile_scope!("rebuild decorations");
    buffer.view.decorations.clear();

    // Per-layer caching follows the same shape everywhere: gate on a flag,
    // mix a fingerprint, either reuse the cached `Set` or rebuild
    // it via the supplied closure, then push (optionally with heights for
    // layers that emit Line decorations the heightmap needs to see).
    //
    // `cached!(slot, fp, build, heights?)` keeps the per-layer code to a
    // single line each. `heights` is the optional fourth arg — when present,
    // the layer goes through `push_with_heights`; otherwise plain `push`.
    macro_rules! cached {
        ($slot:ident, $fp:expr, $build:expr) => {{
            let v = DecorationCache::get_or_compute(&mut cache.$slot, $fp, $build);
            buffer.view.decorations.push(v);
        }};
        ($slot:ident, $fp:expr, $build:expr, heights) => {{
            let v = DecorationCache::get_or_compute(&mut cache.$slot, $fp, $build);
            buffer.view.decorations.push_with_heights(v);
        }};
    }

    // active_line is cheap to BUILD, but it's not cheap to *not cache* —
    // each fresh `RangeSet::from_iter` call hands back a new `Arc`, which
    // flips `view.decorations.signature` (mix of every layer's
    // `content_id`). That signature is part of the per-line galley cache
    // key, so an unstable signature invalidates every visible line's
    // layout every frame. Cache on `(doc_id, sel)` — the inputs the
    // provider actually depends on — so idle / pure-scroll frames return
    // the same Arc and the galley cache holds.
    cached!(active_line, mix(doc_id, sel), || {
        active_line_decorations(&buffer.editor)
    });

    // Paint-only, viewport-scoped, doc-only-dependent. Gated on its
    // own View-menu toggle (`view-highlight-trailing-whitespace-toggle`).
    if buffer.highlight_trailing_whitespace {
        cached!(trailing_ws, mix(doc_id, vp_fp), || {
            trailing_whitespace_decorations(&buffer.editor, Some(&visible_range))
        });
    }

    // Index-diff gutter (`compute_diff` parity). Cached on (doc content
    // id, loaded-text length + ptr hash) — `loaded_text` is only swapped
    // on disk reads/writes, so its address + length together act as a
    // cheap identity fingerprint that survives across paints. Without
    // this cache, every paint runs a full line-level `diff::compute`
    // over the buffer + on-disk snapshot, which is the dominant scroll
    // cost on non-trivial files.
    let loaded_fp = mix(
        buffer.loaded_text.as_ptr() as u64,
        buffer.loaded_text.len() as u64,
    );
    cached!(index_diff, mix(doc_id, loaded_fp), || {
        buffer.editor.index_diff_decorations(&buffer.loaded_text)
    });

    // markdown / fold / fold emit Line decorations with
    // `hide: true` or `height_scale`, so they go through `push_with_heights`
    // to reach the heightmap driver. markdown depends on cursor line
    // (code blocks reveal on cursor-on-line); fold/frontmatter on the fold
    // set. Live-preview layers stay gated on `buffer.live_preview`; the
    // structural fold layer is unconditional so manual folds keep working
    // when previews are off.
    if buffer.live_preview {
        cached!(markdown, mix(mix(doc_id, cursor_line), folds_id),
            || markdown_decorations(&buffer.editor, theme), heights);
    }
    cached!(fold, mix(doc_id, folds_id),
        || fold_decorations(&buffer.editor, &buffer.folds), heights);

    if buffer.live_preview {
        // wikilink reveals when the cursor isn't on the same line —
        // selection-dependent on top of doc + viewport.
        cached!(wikilink, mix(mix(doc_id, cursor_line), vp_fp),
            || wikilink_decorations(&buffer.editor, theme, Some(&visible_range)));
        cached!(callout, mix(doc_id, vp_fp),
            || callout_decorations(&buffer.editor, theme, Some(&visible_range)));
    }

    cached!(frontmatter, mix(doc_id, folds_id),
        || frontmatter_fold(&buffer.editor, &buffer.folds, theme), heights);

    if buffer.live_preview {
        cached!(transclusion, mix(doc_id, vp_fp),
            || transclusion_decorations(&buffer.editor, theme, Some(&visible_range)));
        cached!(footnote, mix(doc_id, vp_fp),
            || footnote_decorations(&buffer.editor, theme, Some(&visible_range)));
        cached!(math, mix(doc_id, vp_fp),
            || math_decorations(&buffer.editor, theme, Some(&visible_range)));
        cached!(mermaid, mix(doc_id, vp_fp),
            || mermaid_decorations(&buffer.editor, theme, Some(&visible_range)));
    }

    // Chunk-boundary visualisation: a gutter marker + faint background at
    // every chunk start, so the user can see how the indexer slices this
    // note (`view-show-chunk-boundaries`).
    if buffer.chunk_boundaries {
        cached!(chunk_boundaries, doc_id, || {
            buffer.editor.chunk_boundary_decorations()
        });
    }

    // Whitespace overlay (view-menu toggle). Doc-dependent only; cache
    // on doc_id so the layer's Arc stays stable across scroll frames and
    // doesn't flip `layers_sig`.
    if buffer.show_whitespace {
        cached!(special_chars, doc_id, || {
            let flags = SpecialCharsFlags {
                tabs: true,
                spaces: true,
                nbsp: true,
                zero_width: true,
                crlf: true,
            };
            special_chars_decorations(&buffer.editor, flags)
        });
    }

    // Diff overlay: view zones for removed lines + line backgrounds for
    // added/modified ranges, computed once at the top of `show`. Pushed
    // last so the diff stacks above other decoration layers; goes through
    // `push_with_heights` because the Block entries reserve space in the
    // line-height map.
    if let Some(ov) = diff {
        buffer
            .view
            .decorations
            .push_with_heights(ov.decorations.clone());
    }

    // Viewport-scoped layers (occurrence highlight, bracket match). Both
    // are cheap to build, but constructing a fresh `RangeSet` every frame
    // flips `view.decorations.signature` (Arc-pointer-based content_id)
    // and forces the per-line galley cache to rebuild every visible row.
    // Cache them on the inputs the provider actually depends on so the
    // signature stays stable on idle/scroll frames.
    cached!(occurrence, mix(mix(doc_id, sel), vp_fp), || {
        occurrence_decorations(&buffer.editor, visible_range.clone())
    });
    cached!(bracket_match, mix(doc_id, sel), || {
        bracket_match_decorations(&buffer.editor, DEFAULT_BRACKETS, 5000)
    });

    // Render the editor (left) and the structural minimap (right). The
    // minimap reads the same `ViewState.decorations` the editor paints
    // from, so heading/code/quote classification follows whatever syntax
    // pipeline the host has wired up.
    // Resolve minimap options from the live config snapshot. Cheap each
    // frame — a few field copies + 9 hex parses. Hex parses default back
    // to the built-in palette if the user typed something invalid.
    let mini_opts: Option<MinimapOptions> = if buffer.show_minimap {
        app.vault_session.config
            .read()
            .ok()
            .map(|c| c.editor.minimap.to_minimap_options())
    } else {
        None
    };

    let click_buffer = &mut buffer.click_buffer;
    let paint_cache = &mut buffer.paint_cache;
    let body = ui.available_rect_before_wrap();
    let minimap_w: f32 = mini_opts.as_ref().map(|o| o.width).unwrap_or(0.0);
    let split_x = (body.right() - minimap_w).max(body.left());
    let editor_rect = egui::Rect::from_min_max(body.min, egui::pos2(split_x, body.max.y));
    {
        crate::profile_scope!("Widget::show");
        let mut editor_ui = ui.new_child(egui::UiBuilder::new().max_rect(editor_rect));
        EditorWidget::new(&mut buffer.editor, &mut buffer.view)
            .with_click_sink(click_buffer)
            .with_paint_cache(paint_cache)
            .show(&mut editor_ui);
    }
    if let Some(opts) = mini_opts {
        crate::profile_scope!("Widget::show");
        let minimap_rect =
            egui::Rect::from_min_max(egui::pos2(split_x, body.min.y), body.max);
        let mut mini_ui = ui.new_child(egui::UiBuilder::new().max_rect(minimap_rect));
        MinimapWidget::new(&buffer.editor, &mut buffer.view)
            .with_options(opts)
            .show(&mut mini_ui);
    } else if !buffer.hide_scrollbar {
        // No minimap → draw a thin auto-hiding scrollbar overlay along
        // the right edge of the editor body. Same affordance role the
        // file tree gets from `ScrollArea::vertical`, just adapted to
        // the editor's hand-rolled scroll model (`view.scroll_y`).
        scrollbar::AutoScrollbar { ui, view: &mut buffer.view, editor_rect }.paint();
    }

    // Pull WidgetClicks for patch-review buttons out of the click buffer
    // BEFORE fold-toggle handling so the click_map mapping is consumed
    // here. Other WidgetClick consumers (none today) would chain here too.
    let widget_clicks: Vec<u64> = buffer
        .click_buffer
        .iter()
        .filter_map(|c| match c {
            ClickAction::WidgetClick(id) => Some(*id),
            _ => None,
        })
        .collect();
    buffer
        .click_buffer
        .retain(|c| !matches!(c, ClickAction::WidgetClick(_)));

    // Apply fold toggles from this frame's clicks.
    buffer.drain_fold_clicks();

    // Per-hunk overlay-widget click dispatch. The diff overlay maps each
    // Accept / Reject button id to the proposal(s) it covers; we route
    // the action through the staging service and then re-hydrate so the
    // remaining proposals' hunks shift / disappear cleanly.
    if let Some(ov) = diff
        && !widget_clicks.is_empty()
    {
        for id in widget_clicks {
            let Some(action) = ov.click_map.get(&id) else { continue };
            match action.clone() {
                diff_overlay::HunkAction::Accept(ids) => app.apply_hunk_accept(path, &ids),
                diff_overlay::HunkAction::Reject(ids) => app.apply_hunk_reject(path, &ids),
                diff_overlay::HunkAction::Restore { path: target, byte_start, byte_end } => {
                    app.apply_hunk_restore(path, &target, byte_start, byte_end);
                }
            }
        }
    }
    }
}

/// Hunk-mutation methods on `AppState`. Lives here next to the buffer
/// panel because the verbs are entirely UI-driven (the panel surfaces
/// these via per-hunk Accept / Reject / Restore overlay widgets). Methods
/// with `&mut self` receivers are exempt from `clippy::single_call_fn`.
impl AppState {
    /// Per-hunk Accept: dispatch `staging.accept` on every proposal whose
    /// footprint overlapped the hunk, then reload + re-hydrate so the
    /// diff layer reflects whatever remains.
    pub(super) fn apply_hunk_accept(&mut self, path: &str, proposal_ids: &[String]) {
        let staging = self.vault_session.services.staging.clone();
        let changes = self.vault_session.services.changes.clone();
        let (mut ok, mut err) = (0usize, 0usize);
        for id in proposal_ids {
            match staging.accept(id, &self.vault_session.vault, Some(changes.as_ref())) {
                Ok(_) => ok += 1,
                Err(_) => err += 1,
            }
        }
        BufCtx::reload_and_rehydrate(self, path);
        self.push_toast(
            if err == 0 {
                format!("Accepted {} hunk{}", ok, if ok == 1 { "" } else { "s" })
            } else {
                format!("Accepted {}, {} failed", ok, err)
            },
            if err == 0 { ToastLevel::Info } else { ToastLevel::Error },
        );
    }

    /// Per-hunk Restore: write the snapshot buffer's text for
    /// `[byte_start, byte_end)` back to disk at `target_path`, splicing
    /// it into the current on-disk content. Appends a `Modified` row to
    /// `changes.db` so the restore is itself an auditable change.
    pub(super) fn apply_hunk_restore(
        &mut self,
        buffer_key: &str,
        target_path: &str,
        byte_start: usize,
        byte_end: usize,
    ) {
        let snapshot_text = match self.session.buffers.get(buffer_key) {
            Some(b) => b.editor.doc.to_string(),
            None => return,
        };
        let snippet = snapshot_text.get(byte_start..byte_end).unwrap_or("").to_string();
        let (disk_text, disk_hash) = match self
            .vault_session
            .vault
            .read_file_with_hash(target_path)
        {
            Ok(v) => v,
            Err(err) => {
                self.push_toast(
                    format!("Restore failed (read disk): {}", err),
                    ToastLevel::Error,
                );
                return;
            }
        };
        let mut new_text = String::with_capacity(disk_text.len() + snippet.len());
        let safe_start = byte_start.min(disk_text.len());
        let safe_end = byte_end.min(disk_text.len()).max(safe_start);
        new_text.push_str(&disk_text[..safe_start]);
        new_text.push_str(&snippet);
        new_text.push_str(&disk_text[safe_end..]);
        let new_hash = hiker_core::hash_string(&new_text);
        match self
            .vault_session
            .vault
            .write_file_checked(target_path, &disk_hash, &new_text)
        {
            Ok(_) => {
                let _ = self.vault_session.services.changes.append(
                    hiker_core::changes::ChangeAppend {
                        path: target_path,
                        op: hiker_core::changes::ChangeOp::Modified,
                        author: "user",
                        content_hash: Some(&new_hash),
                        content: Some(new_text.as_bytes()),
                        rename_from: None,
                        metadata: serde_json::json!({ "restored_hunk_range": [byte_start, byte_end] }),
                    },
                );
                self.push_toast(
                    format!("Restored hunk to {}", target_path),
                    ToastLevel::Info,
                );
            }
            Err(err) => self.push_toast(format!("Restore failed: {}", err), ToastLevel::Error),
        }
    }

    /// Per-hunk Reject: dispatch `staging.reject` on every covered
    /// proposal, then reload + re-hydrate. Reject doesn't append a
    /// `changes.db` row; the rejected text simply isn't re-applied on
    /// the next hydration.
    pub(super) fn apply_hunk_reject(&mut self, path: &str, proposal_ids: &[String]) {
        let staging = self.vault_session.services.staging.clone();
        let mut n = 0usize;
        for id in proposal_ids {
            if staging.reject(id).is_ok() {
                n += 1;
            }
        }
        BufCtx::reload_and_rehydrate(self, path);
        self.push_toast(
            format!("Rejected {} hunk{}", n, if n == 1 { "" } else { "s" }),
            ToastLevel::Info,
        );
    }
}

/// Surface a thin banner whenever there's a pending write-shaped
/// proposal targeting the open buffer. Spec mandates a single-line strip
/// just under the toolbar (`patch-review.md:138-148`) with Accept,
/// Reject, and View-diff actions — *not* the larger half-page banner the
/// old TS UI used.
impl<'a> BufCtx<'a> {
fn pending_rewrite_banner(&mut self) {
    let ui = &mut *self.ui;
    let app = &mut *self.app;
    let path: &str = self.path;
    let staging = app.vault_session.services.staging.clone();
    // Reads the per-frame cache populated in `main::refresh_staging_snapshot`.
    let Some(prop) = app
        .ui_cache.staging_snapshot
        .iter()
        .find(|p| p.target_path == path && p.action == "write_note")
        .cloned()
    else {
        return;
    };
    let mut accept = false;
    let mut reject = false;
    let mut view = false;
    egui::Frame::default()
        .fill(egui::Color32::from_rgb(0xff, 0xf3, 0xc4))
        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(0xd9, 0xb8, 0x4e)))
        .inner_margin(egui::Margin::symmetric(8, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add(crate::icons::ICONS.image(crate::icons::Icon::Robot));
                ui.label(
                    egui::RichText::new("Agent proposed a full-note rewrite")
                        .small()
                        .strong(),
                );
                ui.label(
                    egui::RichText::new(format!("({})", &prop.id[..prop.id.len().min(8)]))
                        .color(theme::muted())
                        .monospace()
                        .small(),
                );
                ui.with_layout(
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        if ui.small_button("Accept").clicked() {
                            accept = true;
                        }
                        if ui.small_button("Reject").clicked() {
                            reject = true;
                        }
                        if ui.small_button("View diff").clicked() {
                            view = true;
                        }
                    },
                );
            });
        });
    if accept {
        let changes = app.vault_session.services.changes.clone();
        match staging.accept(&prop.id, &app.vault_session.vault, Some(changes.as_ref())) {
            Ok(o) => app.push_toast(
                format!("Accepted proposal for {}", o.target_path),
                ToastLevel::Info,
            ),
            Err(err) => app.push_toast(format!("Accept failed: {err}"), ToastLevel::Error),
        }
    }
    if reject {
        match staging.reject(&prop.id) {
            Ok(()) => app.push_toast(
                "Proposal rejected".to_string(),
                ToastLevel::Info,
            ),
            Err(err) => app.push_toast(format!("Reject failed: {err}"), ToastLevel::Error),
        }
    }
    if view {
        use crate::tab::TabKind;
        let pid = prop.id.clone();
        let target = prop.target_path.clone();
        let pid_for_build = pid.clone();
        app.find_or_open_tab(
            |k| matches!(
                k,
                TabKind::Editor {
                    buffer: crate::tab::BufferSource::StagingProposal { proposal_id, .. },
                    ..
                } if *proposal_id == pid
            ),
            || TabKind::staging_preview(pid_for_build, target),
        );
    }
}

fn toolbar(&mut self) {
    let ui = &mut *self.ui;
    let app = &mut *self.app;
    let path: &str = self.path;
    let source = app.session.buffers.get(path).map(|b| b.source.clone());
    let is_vault = matches!(&source, Some(crate::tab::BufferSource::Vault { .. }));
    egui::Frame::default()
        .inner_margin(egui::Margin::symmetric(4, 2))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                if !is_vault {
                    // Reconstruct a `BufCtx` inside this closure — the
                    // outer `&mut self` is split into `ui`/`app`/`path`
                    // locals so we can call the read-only sibling as a
                    // method via a fresh borrow.
                    BufCtx { ui, app, path }.render_readonly_source_toolbar(source.as_ref());
                    return;
                }
                if ui
                    .add(egui::Button::image(icons::ICONS.image(crate::icons::Icon::Check)))
                    .on_hover_text("Save (Mod-S)")
                    .clicked()
                {
                    if let Err(err) = editor_pane::save_buffer(app, path) {
                        app.push_toast(format!("Save failed: {}", err), ToastLevel::Error);
                    }
                }
                let dirty = app.session.buffers.get(path).map(super::super::buffer::Buffer::is_dirty).unwrap_or(false);
                if dirty {
                    ui.add(icons::ICONS.current_dot());
                }
                ui.separator();
                let diff_resp = ui
                    .add(egui::Button::image(icons::ICONS.image(crate::icons::Icon::Diff)))
                    .on_hover_text("Diff vs disk — right-click to show changes\u{2026}");
                if diff_resp.clicked() {
                    open_diff_vs_disk(app, path);
                }
                diff_resp.context_menu(|ui| {
                    app.show_diff_source_menu(ui, path);
                });
                // Agent-diff toggle: jump to the staging-preview tab when
                // a write-shaped proposal is in flight against this note.
                // Mutually-exclusive with the user-diff button above per
                // `patch-review.md:17-27` — both toggle the same buffer
                // tab strip into a single diff mode at a time.
                let has_agent_proposal = app.ui_cache.staging_snapshot.iter().any(|p| {
                    p.target_path == path
                        && (p.action == "write_note" || p.action == "edit_note")
                });
                ui.add_enabled_ui(has_agent_proposal, |ui| {
                    if ui
                        .add(egui::Button::image(crate::icons::ICONS.image(crate::icons::Icon::Robot)))
                        .on_hover_text(if has_agent_proposal {
                            "Agent diff (pending proposal)"
                        } else {
                            "No pending agent proposal for this note"
                        })
                        .clicked()
                    {
                        // Open the staging preview for the first matching
                        // proposal. Done via singleton tab semantics so
                        // repeated clicks just focus the existing tab.
                        if let Some(p) = app.ui_cache.staging_snapshot.iter().find(|p| {
                            p.target_path == path
                                && (p.action == "write_note" || p.action == "edit_note")
                        }) {
                            use crate::tab::TabKind;
                            let pid = p.id.clone();
                            let tpath = p.target_path.clone();
                            let pid_for_build = pid.clone();
                            app.find_or_open_tab(
                                |k| matches!(
                                    k,
                                    TabKind::Editor {
                                        buffer: crate::tab::BufferSource::StagingProposal { proposal_id, .. },
                                        ..
                                    } if *proposal_id == pid
                                ),
                                || TabKind::staging_preview(pid_for_build, tpath),
                            );
                        }
                    }
                });
                toolbar_menus::Menus { ui, app, path }.view_options_menu();
                toolbar_menus::Menus { ui, app, path }.mutations_menu();

                // "Add to trail" pill — legacy `addToTrailPill.ts`,
                // `trail-add-to-active-from-editor-verb`. Hidden when
                // no active trail or when the buffer path isn't a
                // regular indexable extension. Disabled (with tooltip)
                // when the path is already a waypoint at any depth.
                BufCtx { ui: &mut *ui, app: &mut *app, path }.add_to_trail_pill();

                // Centered mode-controls slot — empty in plain editing mode.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |_ui| {
                    // (right side reserved for future view-mode badges)
                });
            });
        });
}

/// Toolbar for the read-only source kinds — snapshot blob, staging
/// proposal, trash entry. Each renders a source-specific verb pair
/// (Restore / Accept-Reject / nothing) plus the diff toggle when a
/// `DiffSource` is in play. No Save, no Mutations, no dirty marker —
/// these buffers are read-only.
fn render_readonly_source_toolbar(&mut self, source: Option<&crate::tab::BufferSource>) {
    let ui = &mut *self.ui;
    let app = &mut *self.app;
    let key: &str = self.path;
    use crate::tab::BufferSource;
    let active_id = app.session.active_tab;
    let diff_active = active_id
        .and_then(|id| app.tab_by_id(id))
        .and_then(|t| t.kind.diff_source())
        .is_some();
    match source {
        Some(BufferSource::Snapshot { path, change_id }) => {
            let path = path.clone();
            let cid = change_id.clone();
            if ui
                .add(
                    egui::Button::image_and_text(
                        icons::ICONS.primary_restore(),
                        egui::RichText::new("Restore").color(egui::Color32::WHITE),
                    )
                    .fill(egui::Color32::from_rgb(0x2f, 0x6f, 0xed)),
                )
                .on_hover_text("Write this snapshot back to disk")
                .clicked()
            {
                app.restore_snapshot_to_disk(&path, &cid);
            }
            BufCtx { ui: &mut *ui, app: &mut *app, path: key }.render_diff_toggle_button(key, diff_active);
        }
        Some(BufferSource::StagingProposal { proposal_id, target_path }) => {
            let pid = proposal_id.clone();
            let target = target_path.clone();
            if ui
                .add(
                    egui::Button::new(
                        egui::RichText::new("Accept").color(egui::Color32::WHITE),
                    )
                    .fill(egui::Color32::from_rgb(0x2f, 0x8f, 0x4d)),
                )
                .on_hover_text("Write this proposal to disk")
                .clicked()
            {
                app.accept_staging_proposal(&pid, &target);
            }
            if ui
                .add(
                    egui::Button::new(
                        egui::RichText::new("Reject").color(egui::Color32::WHITE),
                    )
                    .fill(egui::Color32::from_rgb(0xb9, 0x3a, 0x3a)),
                )
                .on_hover_text("Discard this proposal")
                .clicked()
            {
                app.reject_staging_proposal(&pid);
            }
            BufCtx { ui: &mut *ui, app: &mut *app, path: key }.render_diff_toggle_button(key, diff_active);
        }
        Some(BufferSource::Trash { .. }) => {
            ui.label(egui::RichText::new("In trash · read-only").color(theme::muted()));
        }
        _ => {}
    }
}

fn render_diff_toggle_button(&mut self, _key: &str, diff_active: bool) {
    let ui = &mut *self.ui;
    let app = &mut *self.app;
    let label = if diff_active { "Hide diff" } else { "Show diff" };
    if ui.button(label).clicked() {
        // Flip the active tab's diff field between None and Disk(path).
        let Some(active_id) = app.session.active_tab else { return };
        let Some(tab) = app.tab_by_id_mut(active_id) else { return };
        if let crate::tab::TabKind::Editor { buffer, diff } = &mut tab.kind {
            *diff = match diff {
                Some(_) => None,
                None => Some(crate::tab::DiffSource::Disk { path: buffer.path().to_string() }),
            };
        }
    }
    }
}

/// Snapshot / staging-proposal verbs, surfaced from the read-only
/// source-toolbar. Methods on `AppState` so they're exempt from
/// `clippy::single_call_fn`.
impl AppState {
    pub(super) fn restore_snapshot_to_disk(&mut self, path: &str, change_id: &str) {
        let id: i64 = match change_id.parse() {
            Ok(i) => i,
            Err(_) => return,
        };
        let bytes = match self.vault_session.services.changes.content_at(id) {
            Ok(Some(b)) => b,
            _ => return,
        };
        let snapshot_text = String::from_utf8_lossy(&bytes).into_owned();
        let snapshot_hash = hiker_core::hash_string(&snapshot_text);
        let (_disk_text, disk_hash) = self
            .vault_session
            .vault
            .read_file_with_hash(path)
            .unwrap_or_else(|_| (String::new(), String::new()));
        match self
            .vault_session
            .vault
            .write_file_checked(path, &disk_hash, &snapshot_text)
        {
            Ok(_) => {
                let _ = self.vault_session.services.changes.append(
                    hiker_core::changes::ChangeAppend {
                        path,
                        op: hiker_core::changes::ChangeOp::Modified,
                        author: "user",
                        content_hash: Some(&snapshot_hash),
                        content: Some(snapshot_text.as_bytes()),
                        rename_from: None,
                        metadata: serde_json::json!({ "restored_from_change_id": id }),
                    },
                );
                self.push_toast(format!("Restored snapshot of {}", path), ToastLevel::Info);
            }
            Err(err) => self.push_toast(format!("Restore failed: {}", err), ToastLevel::Error),
        }
    }

    pub(super) fn accept_staging_proposal(&mut self, proposal_id: &str, target_path: &str) {
        let staging = self.vault_session.services.staging.clone();
        let changes = self.vault_session.services.changes.clone();
        match staging.accept(proposal_id, &self.vault_session.vault, Some(changes.as_ref())) {
            Ok(_) => {
                self.push_toast(format!("Accepted proposal for {}", target_path), ToastLevel::Info);
                editor_pane::open_file(self, target_path, /* sticky */ true);
            }
            Err(err) => self.push_toast(format!("Accept failed: {}", err), ToastLevel::Error),
        }
    }

    pub(super) fn reject_staging_proposal(&mut self, proposal_id: &str) {
        let staging = self.vault_session.services.staging.clone();
        match staging.reject(proposal_id) {
            Ok(()) => self.push_toast("Proposal rejected".to_string(), ToastLevel::Info),
            Err(err) => self.push_toast(format!("Reject failed: {}", err), ToastLevel::Error),
        }
    }
}

impl<'a> BufCtx<'a> {
    /// "Add to trail" pill in the editor toolbar. Legacy parity:
    /// `ui/src/trails/addToTrailPill.ts`. Hidden unless an indexable
    /// buffer is open AND there is an active (or fallback Recent) trail
    /// to append to. Disabled when the path is already a waypoint at
    /// any depth (idempotency, same as legacy membership cache).
    fn add_to_trail_pill(&mut self) {
    let ui = &mut *self.ui;
    let app = &mut *self.app;
    let path: &str = self.path;
    let lower = path.to_lowercase();
    if !lower.ends_with(".md") && !lower.ends_with(".txt") {
        return;
    }
    // Pill only surfaces when there's an explicitly-active trail.
    let Some(trail_id) = app
        .session.active_trail
        .clone()
        .filter(|id| app.session.trails.iter().any(|t| &t.id == id))
    else {
        return;
    };
    let trail = match app.session.trails.iter().find(|t| t.id == trail_id) {
        Some(t) => t,
        None => return,
    };
    let trail_name = trail.name.clone();
    let already = trail_contains_path(&trail.waypoints, path);

    ui.separator();
    let label = format!("+ {}", trail_name);
    let tooltip = if already {
        format!("Already in '{}'", trail_name)
    } else {
        format!("Add to trail '{}'", trail_name)
    };
    let resp = ui.add_enabled(
        !already,
        egui::Button::image_and_text(crate::icons::ICONS.trail(), label),
    );
    let resp = resp.on_hover_text(tooltip);
    if resp.clicked() {
        crate::state::trail_append_waypoint(app, path);
        let _ = crate::bootstrap::save_trails(&app.vault_session.vault_root, &app.session.trails);
        app.push_toast(
            format!("Added to '{}'", trail_name),
            ToastLevel::Info,
        );
    }
    }
}

fn trail_contains_path(waypoints: &[crate::state::Waypoint], path: &str) -> bool {
    for w in waypoints {
        if w.path == path {
            return true;
        }
        if trail_contains_path(&w.children, path) {
            return true;
        }
    }
    false
}


/// Persist a `editor.*` view toggle into vault-scoped settings + swap the
/// merged copy in `AppState::config` so subsequent buffer opens pick up
/// the change. Mirrors the `set_setting` helper used by the settings tab.
/// Parse `#RRGGBB` / `#RRGGBBAA` into an egui `Color32`. Falls back to
/// fully-opaque magenta on a malformed value so a bad config entry is
/// visually obvious instead of silently transparent.
fn parse_hex_color(s: &str) -> egui::Color32 {
    let bytes = s.as_bytes();
    if !matches!(bytes.first(), Some(b'#')) {
        return egui::Color32::from_rgb(0xff, 0x00, 0xff);
    }
    let hex = &s[1..];
    let hex_byte = |i: usize| -> Option<u8> {
        u8::from_str_radix(hex.get(i..i + 2)?, 16).ok()
    };
    match hex.len() {
        6 => {
            let (Some(r), Some(g), Some(b)) = (hex_byte(0), hex_byte(2), hex_byte(4)) else {
                return egui::Color32::from_rgb(0xff, 0x00, 0xff);
            };
            egui::Color32::from_rgb(r, g, b)
        }
        8 => {
            let (Some(r), Some(g), Some(b), Some(a)) =
                (hex_byte(0), hex_byte(2), hex_byte(4), hex_byte(6))
            else {
                return egui::Color32::from_rgb(0xff, 0x00, 0xff);
            };
            egui::Color32::from_rgba_unmultiplied(r, g, b, a)
        }
        _ => egui::Color32::from_rgb(0xff, 0x00, 0xff),
    }
}

/// Extension trait to read a `MinimapConfig` directly into the egui-side
/// `MinimapOptions`. Trait methods on `&self` are exempt from
/// `clippy::single_call_fn` even when only one caller materializes them.
trait MinimapOptionsExt {
    fn to_minimap_options(&self) -> MinimapOptions;
}

impl MinimapOptionsExt for hiker_core::config::sections::MinimapConfig {
    fn to_minimap_options(&self) -> MinimapOptions {
        MinimapOptions {
            width: self.width as f32,
            bar_padding_left: self.bar_padding_left as f32,
            bar_padding_right: self.bar_padding_right as f32,
            bar_corner_radius: self.bar_corner_radius as f32,
            min_bar_width: self.min_bar_width as f32,
            bar_gap: (self.bar_gap_tenths as f32) / 10.0,
            colored: self.colored,
            show_section_rules: self.show_section_rules,
            show_viewport: self.show_viewport,
            show_left_edge: self.show_left_edge,
            color_heading: parse_hex_color(&self.color_heading),
            color_code: parse_hex_color(&self.color_code),
            color_emphasis: parse_hex_color(&self.color_emphasis),
            color_quote: parse_hex_color(&self.color_quote),
            color_plain: parse_hex_color(&self.color_plain),
            color_background: parse_hex_color(&self.color_background),
            color_section_rule: parse_hex_color(&self.color_section_rule),
            color_viewport: parse_hex_color(&self.color_viewport),
            color_viewport_hover: parse_hex_color(&self.color_viewport_hover),
        }
    }
}

fn persist_view_setting(app: &mut AppState, key: &str, value: &serde_json::Value) {
    let label = format!("Save {key} failed");
    app.set_setting(
        hiker_core::config::SettingsScope::Vault,
        key,
        value,
        &label,
    );
}

/// "Diff vs disk" — open (or focus) a `BufferDiff` tab that shows a
/// read-only side-by-side diff between the buffer text and the on-disk
/// version. The preview tab updates as the user keeps typing.
/// Toggle the diff-against-disk overlay on the *active* editor tab —
/// diff is a mode of the same tab (per `diff-as-mode`), not a separate
/// tab kind. Press once to layer the disk diff on top of the live buffer;
/// press again to clear it. The buffer's cursor / selection / scroll
/// survive both transitions because the buffer text is untouched.
pub(super) fn open_diff_vs_disk(app: &mut AppState, path: &str) {
    use crate::tab::{BufferSource, DiffSource, TabKind};
    let Some(active_id) = app.session.active_tab else { return };
    let Some(tab) = app.tab_by_id_mut(active_id) else { return };
    if let TabKind::Editor { buffer: BufferSource::Vault { path: tab_path }, diff } = &mut tab.kind
        && tab_path == path
    {
        *diff = match diff {
            Some(_) => None,
            None => Some(DiffSource::Disk { path: path.to_string() }),
        };
    }
}

impl HeadingBreadcrumb for crate::buffer::Buffer {
    fn heading_breadcrumb(&self) -> String {
        let cursor_line = self
            .editor
            .doc
            .byte_to_line(self.editor.selection.main().head.byte as usize);
        let mut stack: Vec<(u8, String)> = Vec::new();
        let total_lines = self.editor.doc.len_lines();
        for line_idx in 0..=cursor_line {
            let start = self.editor.doc.line_to_byte(line_idx);
            let end = if line_idx + 1 < total_lines {
                self.editor.doc.line_to_byte(line_idx + 1)
            } else {
                self.editor.doc.len_bytes()
            };
            let line: String = self.editor.doc.slice(start..end).to_string();
            let trimmed = line.trim_start();
            if let Some(rest) = trimmed.strip_prefix('#') {
                let mut depth: u8 = 1;
                let mut chars = rest.chars();
                for c in chars.by_ref() {
                    if c == '#' && depth < 6 {
                        depth += 1;
                    } else if c == ' ' || c == '\t' {
                        break;
                    } else {
                        depth = 0;
                        break;
                    }
                }
                if depth == 0 {
                    continue;
                }
                let title = chars.as_str().trim_end_matches(['\n', '\r']).trim();
                stack.retain(|(d, _)| *d < depth);
                stack.push((depth, title.to_string()));
            }
        }
        stack.into_iter().map(|(_, t)| t).collect::<Vec<_>>().join(" /")
    }
}

#[cfg(test)]
mod breadcrumb_tests {
    use super::*;
    use crate::buffer::Buffer;

    fn make(text: &str, cursor_byte: usize) -> Buffer {
        let mut buf = Buffer::with_config_and_vault(
            "test.md".to_string(),
            text,
            String::new(),
            None,
            None,
        );
        buf.editor.selection = editor_core::selection::Selection::single(cursor_byte);
        buf
    }

    #[test]
    fn empty_when_no_headings() {
        let buf = make("just some text\nno heads here\n", 0);
        assert_eq!(buf.heading_breadcrumb(), "");
    }

    #[test]
    fn picks_up_h1() {
        let buf = make("# Title\nbody\n", 9); // cursor on `body`
        assert_eq!(buf.heading_breadcrumb(), "Title");
    }

    #[test]
    fn stacks_deeper_headings() {
        let text = "# A\n## B\n### C\nbody\n";
        let byte = text.find("body").unwrap();
        let buf = make(text, byte);
        assert_eq!(buf.heading_breadcrumb(), "A /B /C");
    }

    #[test]
    fn higher_heading_resets_deeper_stack() {
        let text = "# A\n## B\n### C\n## D\nbody\n";
        let byte = text.find("body").unwrap();
        let buf = make(text, byte);
        assert_eq!(buf.heading_breadcrumb(), "A /D");
    }
}

impl AppState {
    /// Status bar for the active buffer panel. Public entry point — the
    /// workbench host pushes this into the chrome strip at the bottom of
    /// the editor pane.
    pub(crate) fn render_buffer_status_bar(&mut self, ui: &mut egui::Ui, path: &str) {
        BufCtx { ui, app: self, path }.status_bar();
    }
}

impl<'a> BufCtx<'a> {
    /// Status bar row at the bottom of the buffer panel: version
    /// dropdown on the left, indexer status + heading breadcrumb in the
    /// middle, position + counts on the right.
    fn status_bar(&mut self) {
    let ui = &mut *self.ui;
    let app = &mut *self.app;
    let path: &str = self.path;
    egui::Frame::default()
        .inner_margin(egui::Margin::symmetric(6, 2))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                // Left: version dropdown — unifies the active buffer,
                // changelog snapshots, and any pending agent proposals so
                // the user can flip between them without leaving the
                // editor. Spec: status-bar version dropdown spanning the
                // unified activity feed.
                let basename = path.rsplit('/').next().unwrap_or(path);
                let label = basename.to_string();
                if app.session.buffers.get(path).map(super::super::buffer::Buffer::is_dirty).unwrap_or(false) {
                    ui.add(icons::ICONS.current_dot());
                }
                toolbar_menus::Menus { ui, app, path }.version_dropdown(&label);

                // Center: index status from the indexer, optionally
                // followed by the heading breadcrumb when the per-buffer
                // toggle is on.
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.add_space(20.0);
                    let label = {
                        let idx = app.vault_session.services.indexer.as_ref();
                        let s = idx.status();
                        if let Some(err) = &s.last_error {
                            format!("Index error: {}", err)
                        } else if !s.model_ready {
                            "Model loading…".to_string()
                        } else if s.queued > 0 {
                            format!("Indexing ({} queued)", s.queued)
                        } else {
                            format!("Indexed ({} notes)", s.total_notes)
                        }
                    };
                    ui.label(
                        egui::RichText::new(label)
                            .color(theme::muted())
                            .small(),
                    );
                    // Index-diff awareness (`compute_diff` legacy hook):
                    // when the indexer's stored content hash for this path
                    // diverges from the buffer's current hash, the user
                    // sees a "buffer ahead of index" badge so they know
                    // search hits may be stale.
                    // Coarse refresh of the indexer's stored hash for
                    // this buffer. The status bar runs every frame; a
                    // per-frame SQLite query + std-mutex lock against
                    // the shared read store contributes measurably to
                    // scroll latency. The hash only flips when the
                    // indexer commits a re-index, so a 2s refresh
                    // window is plenty responsive.
                    let path_owned = path.to_string();
                    let now = std::time::Instant::now();
                    let needs_refresh = app
                        .session.buffers
                        .get(&path_owned)
                        .map(|b| {
                            b.index_hash_refreshed_at
                                .map(|t| {
                                    now.duration_since(t)
                                        > std::time::Duration::from_secs(2)
                                })
                                .unwrap_or(true)
                        })
                        .unwrap_or(false);
                    if needs_refresh
                        && let Ok(store) = app.vault_session.services.read_store.lock()
                    {
                        let h = store
                            .note_properties(&path_owned)
                            .ok()
                            .flatten()
                            .and_then(|p| p.content_hash);
                        drop(store);
                        if let Some(buf) = app.session.buffers.get_mut(&path_owned) {
                            buf.index_hash_cache = h;
                            buf.index_hash_refreshed_at = Some(now);
                        }
                    }
                    if let Some(buffer) = app.session.buffers.get(&path_owned)
                        && let Some(idx_hash) = buffer.index_hash_cache.as_deref()
                        && idx_hash != buffer.current_hash()
                    {
                        ui.add_space(12.0);
                        ui.add(crate::icons::ICONS.warn());
                        ui.label(
                            egui::RichText::new("buffer ahead of index")
                                .color(theme::warn())
                                .small(),
                        )
                        .on_hover_text(
                            "Live text differs from the most recently indexed version — \
                             save the buffer to refresh.",
                        );
                    }
                    if let Some(buffer) = app.session.buffers.get(path)
                        && buffer.heading_breadcrumb
                    {
                        let crumbs = buffer.heading_breadcrumb();
                        if !crumbs.is_empty() {
                            ui.add_space(12.0);
                            ui.label(
                                egui::RichText::new(format!(": {}", crumbs))
                                    .color(theme::muted())
                                    .small(),
                            );
                        }
                    }
                });

                // Right: line:col + word count + extension badge.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let ext = path.rsplit('.').next().unwrap_or("");
                    ui.label(
                        egui::RichText::new(ext)
                            .color(theme::muted())
                            .small()
                            .monospace(),
                    );
                    if let Some(buffer) = app.session.buffers.get(path) {
                        let wc = buffer.current_text().split_whitespace().count();
                        ui.label(
                            egui::RichText::new(format!("{} words", wc))
                                .color(theme::muted())
                                .small(),
                        );
                        // Line:col — derived from main selection. Click to
                        // open a goto-line popover (per spec).
                        let main = buffer.editor.selection.main();
                        let line = buffer.editor.doc.byte_to_line(main.head.byte as usize) + 1;
                        let line_start = buffer.editor.doc.line_to_byte(line - 1);
                        let col = (main.head.byte as usize).saturating_sub(line_start) + 1;
                        let total_lines = buffer.editor.doc.len_lines().max(1);
                        let resp = ui.add(
                            egui::Label::new(
                                egui::RichText::new(format!("Ln {}, Col {}", line, col))
                                    .color(theme::muted())
                                    .small(),
                            )
                            .sense(egui::Sense::click()),
                        );
                        let mut goto_target: Option<usize> = None;
                        egui::Popup::menu(&resp).show(|ui| {
                            ui.label(
                                egui::RichText::new("Go to line")
                                    .small()
                                    .strong(),
                            );
                            let mut draft = ui.ctx().data_mut(|d| {
                                d.get_persisted_mut_or_default::<String>(
                                    egui::Id::new(("goto-line", path.to_string())),
                                )
                                .clone()
                            });
                            let edit = ui.add(
                                egui::TextEdit::singleline(&mut draft)
                                    .hint_text(format!("1–{total_lines}"))
                                    .desired_width(80.0),
                            );
                            edit.request_focus();
                            ui.ctx().data_mut(|d| {
                                d.insert_persisted(
                                    egui::Id::new(("goto-line", path.to_string())),
                                    draft.clone(),
                                );
                            });
                            let go = edit.lost_focus()
                                && ui.input(|i| i.key_pressed(egui::Key::Enter));
                            if go
                                && let Ok(n) = draft.trim().parse::<usize>()
                                && n >= 1
                                && n <= total_lines
                            {
                                goto_target = Some(n - 1);
                                ui.close();
                            }
                        });
                        if let Some(zero_idx) = goto_target
                            && let Some(b) = app.session.buffers.get_mut(path)
                        {
                            let target_byte = b.editor.doc.line_to_byte(zero_idx);
                            b.editor.selection = editor_core::selection::Selection::single(target_byte);
                        }
                    }
                });
            });
        });
    }
}
