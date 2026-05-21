//! Buffer tab body: editor toolbar strip, the editor widget itself, then
//! the status bar. Buffer-only chrome stays here (hidden for all
//! non-buffer kinds).
#![allow(clippy::items_after_test_module)]

pub mod diff_overlay;
pub mod patch_review_pill;
pub mod show_changes;
pub mod toolbar_menus;

use std::sync::Arc;

use eframe::egui;

use editor_core::light_default;
use editor_egui::{EditorWidget, MinimapOptions, MinimapWidget};
use editor_md::{
    callout_decorations, fold_decorations, footnote_decorations, frontmatter_fold,
    markdown_decorations, math_decorations, mermaid_decorations, transclusion_decorations,
    wikilink_decorations,
};
use editor_view::{
    active_line_decorations, bracket_match_decorations, brackets::DEFAULT_BRACKETS,
    occurrence::occurrence_decorations, special_chars_decorations, trailing_whitespace_decorations,
    ClickAction, SpecialCharsFlags,
};

use crate::buffer::DecorationCache;
use crate::editor_pane;
use crate::icons;
use crate::state::{AppState, ToastLevel};
use crate::theme;


/// XOR-mix the fold ids in an order-independent way. Cheap and stable for
/// memoization keys (HashSet iteration order isn't deterministic).
fn folds_hash(folds: &std::collections::HashSet<u64>) -> u64 {
    let mut h: u64 = 0;
    for &id in folds {
        h ^= id.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
    h
}

/// Combine multiple u64 values into a single fingerprint via splitmix-style
/// hashing. Order-dependent.
fn mix(seed: u64, x: u64) -> u64 {
    let mut z = seed.wrapping_add(x).wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

pub fn show(
    ui: &mut egui::Ui,
    app: &mut AppState,
    path: &str,
    _rt: &Arc<tokio::runtime::Runtime>,
) {
    // Toolbar across the top of the buffer tab body.
    toolbar(ui, app, path);

    // Pending-rewrite banner: thin row that surfaces a write-shaped
    // proposal targeting this note.
    pending_rewrite_banner(ui, app, path);

    // Build the inline diff overlay once. Drives both the file pill
    // (counts + Next-hunk + bulk verbs above the editor) and the in-buffer
    // decorations pushed by `show_editor`. Owner-aware: Agent for
    // hydrated proposals, Manual / Snapshot / Staging for the dirty-buffer
    // diff toggle / history viewer / staging-proposal review.
    let overlay = diff_overlay::compute(app, path);
    if let Some(ov) = &overlay
        && matches!(ov.owner, editor_diff::DiffOwner::Agent)
    {
        let cursor_byte = current_cursor_byte(app, path);
        let pill_action = patch_review_pill::show(ui, app, &ov.hunks, cursor_byte);
        apply_pill_action(app, path, pill_action);
    }

    ui.add_space(4.0);

    egui::Frame::default().show(ui, |ui| {
        let body_height = ui.available_height().max(80.0);
        let (rect, _resp) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), body_height),
            egui::Sense::hover(),
        );
        app.session.nav.swipe_skip_rects.push(rect);
        let mut body_ui = ui.new_child(egui::UiBuilder::new().max_rect(rect));
        show_editor(&mut body_ui, app, path, overlay);
    });
}

/// Snapshot pending patch-review state for `path`. Reads the buffer text
/// and queries the staging service for *all* proposals targeting this path
/// (applyable + conflicted) so the inline UI can paint conflicted hunks
fn current_cursor_byte(app: &AppState, path: &str) -> usize {
    app.session
        .buffers
        .get(path)
        .map(|b| b.editor.selection.main().head.offset())
        .unwrap_or(0)
}

/// Resolve a pill bulk action against the hydrated agent proposals on this
/// buffer. Accept-all iterates the buffer's `hydrated_proposals` list and
/// calls `staging.accept` on each (which writes the changes-db audit row
/// and removes the proposal); Reject-all calls `staging.reject` on each.
/// After either, the buffer is re-read from disk and re-hydrated to reflect
/// the post-action state.
fn apply_pill_action(
    app: &mut AppState,
    path: &str,
    action: patch_review_pill::PillAction,
) {
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
        reload_and_rehydrate(app, path);
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
        reload_and_rehydrate(app, path);
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

/// Re-read disk into the buffer and re-apply the buffer-hydration step so
/// the inline diff layer reflects whatever proposals remain in
/// `staging.db` after a bulk accept/reject.
fn reload_and_rehydrate(app: &mut AppState, path: &str) {
    let _ = editor_pane::reload_from_disk(app, path);
    let staging = app.vault_session.services.staging.clone();
    if let Some(buffer) = app.session.buffers.get_mut(path) {
        buffer.agent_base = None;
        buffer.hydrated_proposals.clear();
        buffer.hydrate_pending_proposals(staging.as_ref());
    }
}

fn show_editor(
    ui: &mut egui::Ui,
    app: &mut AppState,
    path: &str,
    diff: Option<diff_overlay::DiffOverlay>,
) {
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
    let folds_id = folds_hash(&buffer.folds);
    let vp_lo = visible_start as u64;
    let vp_hi = visible_end as u64;
    let vp_fp = mix(vp_lo, vp_hi);
    let cache: &mut DecorationCache = &mut buffer.decoration_cache;

    crate::profile_scope!("rebuild decorations");
    buffer.view.decorations.clear();

    // Per-layer caching follows the same shape everywhere: gate on a flag,
    // mix a fingerprint, either reuse the cached `DecorationSet` or rebuild
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
            trailing_whitespace_decorations(&buffer.editor, Some(visible_range.clone()))
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
        index_diff_decorations(&buffer.loaded_text, &buffer.editor)
    });

    // markdown / fold / frontmatter_fold emit Line decorations with
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
            || wikilink_decorations(&buffer.editor, theme, Some(visible_range.clone())));
        cached!(callout, mix(doc_id, vp_fp),
            || callout_decorations(&buffer.editor, theme, Some(visible_range.clone())));
    }

    cached!(frontmatter, mix(doc_id, folds_id),
        || frontmatter_fold(&buffer.editor, &buffer.folds, theme), heights);

    if buffer.live_preview {
        cached!(transclusion, mix(doc_id, vp_fp),
            || transclusion_decorations(&buffer.editor, theme, Some(visible_range.clone())));
        cached!(footnote, mix(doc_id, vp_fp),
            || footnote_decorations(&buffer.editor, theme, Some(visible_range.clone())));
        cached!(math, mix(doc_id, vp_fp),
            || math_decorations(&buffer.editor, theme, Some(visible_range.clone())));
        cached!(mermaid, mix(doc_id, vp_fp),
            || mermaid_decorations(&buffer.editor, theme, Some(visible_range.clone())));
    }

    // Chunk-boundary visualisation: a gutter marker + faint background at
    // every chunk start, so the user can see how the indexer slices this
    // note (`view-show-chunk-boundaries`).
    if buffer.chunk_boundaries {
        cached!(chunk_boundaries, doc_id, || {
            chunk_boundary_decorations(&buffer.editor)
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
    if let Some(ov) = &diff {
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
            .map(|c| minimap_options_from_config(&c.editor.minimap))
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
        crate::profile_scope!("EditorWidget::show");
        let mut editor_ui = ui.new_child(egui::UiBuilder::new().max_rect(editor_rect));
        EditorWidget::new(&mut buffer.editor, &mut buffer.view)
            .with_click_sink(click_buffer)
            .with_paint_cache(paint_cache)
            .show(&mut editor_ui);
    }
    if let Some(opts) = mini_opts {
        crate::profile_scope!("MinimapWidget::show");
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
        auto_hide_scrollbar(ui, &mut buffer.view, editor_rect);
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
    if let Some(ov) = &diff
        && !widget_clicks.is_empty()
    {
        for id in widget_clicks {
            let Some(action) = ov.click_map.get(&id) else { continue };
            match action.clone() {
                diff_overlay::HunkAction::Accept(ids) => handle_hunk_accept(app, path, &ids),
                diff_overlay::HunkAction::Reject(ids) => handle_hunk_reject(app, path, &ids),
            }
        }
    }
}

/// Per-hunk Accept: dispatch `staging.accept` on every proposal whose
/// footprint overlapped the hunk, then reload + re-hydrate so the diff
/// layer reflects whatever remains.
fn handle_hunk_accept(app: &mut AppState, path: &str, proposal_ids: &[String]) {
    let staging = app.vault_session.services.staging.clone();
    let changes = app.vault_session.services.changes.clone();
    let (mut ok, mut err) = (0usize, 0usize);
    for id in proposal_ids {
        match staging.accept(id, &app.vault_session.vault, Some(changes.as_ref())) {
            Ok(_) => ok += 1,
            Err(_) => err += 1,
        }
    }
    reload_and_rehydrate(app, path);
    app.push_toast(
        if err == 0 {
            format!("Accepted {} hunk{}", ok, if ok == 1 { "" } else { "s" })
        } else {
            format!("Accepted {}, {} failed", ok, err)
        },
        if err == 0 { ToastLevel::Info } else { ToastLevel::Error },
    );
}

/// Per-hunk Reject: dispatch `staging.reject` on every covered proposal,
/// then reload + re-hydrate. Reject doesn't append a `changes.db` row;
/// the rejected text simply isn't re-applied on the next hydration.
fn handle_hunk_reject(app: &mut AppState, path: &str, proposal_ids: &[String]) {
    let staging = app.vault_session.services.staging.clone();
    let mut n = 0usize;
    for id in proposal_ids {
        if staging.reject(id).is_ok() {
            n += 1;
        }
    }
    reload_and_rehydrate(app, path);
    app.push_toast(
        format!("Rejected {} hunk{}", n, if n == 1 { "" } else { "s" }),
        ToastLevel::Info,
    );
}

/// Overlay scrollbar painted along the right edge of the editor when
/// the minimap is hidden. macOS-style: invisible at rest, fades in
/// when the pointer is inside the editor or right after a scroll, and
/// supports click + drag on the thumb to seek `view.scroll_y`.
///
/// We can't use `egui::ScrollArea` here because the editor maintains
/// its own viewport model (`view.scroll_y` + `height_map`) and paints
/// only the visible band — wrapping it in a `ScrollArea` would force
/// us to lay out the whole document into a scrollable canvas.
fn auto_hide_scrollbar(
    ui: &mut egui::Ui,
    view: &mut editor_view::view::ViewState,
    editor_rect: egui::Rect,
) {
    let total_h = view.height_map.total_height();
    let viewport_h = view.height.max(1.0);
    let max_scroll = (total_h - viewport_h).max(0.0);
    if max_scroll <= 0.5 {
        return;
    }

    let track_w = 10.0;
    let track_rect = egui::Rect::from_min_max(
        egui::pos2(editor_rect.right() - track_w, editor_rect.top()),
        egui::pos2(editor_rect.right(), editor_rect.bottom()),
    );

    let id = ui.id().with("editor::auto_scrollbar");
    let response = ui.interact(track_rect, id, egui::Sense::click_and_drag());

    // Wake the bar on any pointer activity in the editor body (so the
    // user gets a visual hint while reading) plus the usual scrollbar
    // interactions and scroll-wheel input.
    let now = ui.ctx().input(|i| i.time);
    let pointer_pos = ui.ctx().input(|i| i.pointer.hover_pos());
    let scroll_just_happened = ui.ctx().input(|i| i.smooth_scroll_delta.y.abs() > 0.0);
    let pointer_in_editor = pointer_pos.map(|p| editor_rect.contains(p)).unwrap_or(false);

    let activity_id = id.with("last_active");
    let mut last_active: f64 = ui
        .ctx()
        .data(|d| d.get_temp::<f64>(activity_id))
        .unwrap_or(0.0);
    if response.hovered()
        || response.dragged()
        || pointer_in_editor
        || scroll_just_happened
    {
        last_active = now;
        ui.ctx()
            .data_mut(|d| d.insert_temp(activity_id, last_active));
    }

    // Fade window: solid for `hold`, lerp out over `fade`, then idle.
    let elapsed = (now - last_active).max(0.0);
    let hold = 0.8_f64;
    let fade = 0.6_f64;
    let alpha = if elapsed < hold {
        1.0
    } else if elapsed < hold + fade {
        1.0 - ((elapsed - hold) / fade) as f32
    } else {
        0.0
    };
    if alpha <= 0.0 {
        return;
    }
    // Schedule a repaint during the fade so the bar actually animates
    // away instead of getting stuck at full opacity until the next
    // input event.
    if elapsed < hold + fade {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(33));
    }

    // Drag interaction: convert pointer delta in track space back to
    // content space (scale by total_h/track_h) so a full track sweep
    // covers the full scrollable range.
    let track_h = track_rect.height();
    let thumb_min_h = 24.0_f32;
    let thumb_h = ((viewport_h / total_h) * track_h).max(thumb_min_h);
    let scroll_range = (track_h - thumb_h).max(1.0);
    let frac = (view.scroll_y / max_scroll).clamp(0.0, 1.0);
    let thumb_top = track_rect.top() + frac * scroll_range;
    let thumb_rect = egui::Rect::from_min_max(
        egui::pos2(track_rect.left() + 2.0, thumb_top),
        egui::pos2(track_rect.right() - 2.0, thumb_top + thumb_h),
    );

    if response.dragged() {
        let dy = response.drag_delta().y;
        if dy.abs() > 0.0 {
            view.scroll_y = (view.scroll_y + dy * (max_scroll / scroll_range))
                .clamp(0.0, max_scroll);
        }
    } else if response.clicked() {
        // Click on the track outside the thumb → page jump in that
        // direction. Click on the thumb itself is a no-op (drag handles it).
        if let Some(p) = pointer_pos
            && !thumb_rect.contains(p)
        {
            let dir = if p.y < thumb_rect.top() { -1.0 } else { 1.0 };
            view.scroll_y = (view.scroll_y + dir * viewport_h * 0.9).clamp(0.0, max_scroll);
        }
    }

    // Paint. Solid grey thumb tinted by hover; the track itself stays
    // transparent so the editor text underneath shows through when the
    // bar is partially faded.
    let hovered = response.hovered() || response.dragged();
    let base_alpha = if hovered { 220.0 } else { 140.0 };
    let thumb_alpha = (base_alpha * alpha).round().clamp(0.0, 255.0) as u8;
    let thumb_color = egui::Color32::from_rgba_unmultiplied(96, 102, 110, thumb_alpha);
    ui.painter().rect_filled(
        thumb_rect.shrink2(egui::vec2(0.0, 0.0)),
        egui::CornerRadius::same(3),
        thumb_color,
    );
}

/// Surface a thin banner whenever there's a pending write-shaped
/// proposal targeting the open buffer. Spec mandates a single-line strip
/// just under the toolbar (`patch-review.md:138-148`) with Accept,
/// Reject, and View-diff actions — *not* the larger half-page banner the
/// old TS UI used.
fn pending_rewrite_banner(ui: &mut egui::Ui, app: &mut AppState, path: &str) {
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
                ui.add(crate::icons::robot());
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

fn toolbar(ui: &mut egui::Ui, app: &mut AppState, path: &str) {
    egui::Frame::default()
        .inner_margin(egui::Margin::symmetric(4, 2))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui
                    .add(egui::Button::image(icons::check()))
                    .on_hover_text("Save (Mod-S)")
                    .clicked()
                {
                    if let Err(err) = editor_pane::save_buffer(app, path) {
                        app.push_toast(format!("Save failed: {}", err), ToastLevel::Error);
                    }
                }
                let dirty = app.session.buffers.get(path).map(|b| b.is_dirty()).unwrap_or(false);
                if dirty {
                    ui.add(icons::current_dot());
                }
                ui.separator();
                let diff_resp = ui
                    .add(egui::Button::image(icons::diff()))
                    .on_hover_text("Diff vs disk — right-click to show changes\u{2026}");
                if diff_resp.clicked() {
                    open_diff_vs_disk(app, path);
                }
                diff_resp.context_menu(|ui| {
                    show_changes::show_diff_source_menu(ui, app, path);
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
                        .add(egui::Button::image(crate::icons::robot()))
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
                toolbar_menus::view_options_menu(ui, app, path);
                toolbar_menus::mutations_menu(ui, app, path);

                // "Add to trail" pill — legacy `addToTrailPill.ts`,
                // `trail-add-to-active-from-editor-verb`. Hidden when
                // no active trail or when the buffer path isn't a
                // regular indexable extension. Disabled (with tooltip)
                // when the path is already a waypoint at any depth.
                add_to_trail_pill(ui, app, path);

                // Centered mode-controls slot — empty in plain editing mode.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |_ui| {
                    // (right side reserved for future view-mode badges)
                });
            });
        });
}

/// "Add to trail" pill in the editor toolbar. Legacy parity:
/// `ui/src/trails/addToTrailPill.ts`. Hidden unless an indexable buffer
/// is open AND there is an active (or fallback Recent) trail to append
/// to. Disabled when the path is already a waypoint at any depth
/// (idempotency, same as legacy membership cache).
fn add_to_trail_pill(ui: &mut egui::Ui, app: &mut AppState, path: &str) {
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
        egui::Button::image_and_text(crate::icons::trail(), label),
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

fn minimap_options_from_config(
    cfg: &hiker_core::config::MinimapConfig,
) -> MinimapOptions {
    MinimapOptions {
        width: cfg.width as f32,
        bar_padding_left: cfg.bar_padding_left as f32,
        bar_padding_right: cfg.bar_padding_right as f32,
        bar_corner_radius: cfg.bar_corner_radius as f32,
        min_bar_width: cfg.min_bar_width as f32,
        bar_gap: (cfg.bar_gap_tenths as f32) / 10.0,
        colored: cfg.colored,
        show_section_rules: cfg.show_section_rules,
        show_viewport: cfg.show_viewport,
        show_left_edge: cfg.show_left_edge,
        color_heading: parse_hex_color(&cfg.color_heading),
        color_code: parse_hex_color(&cfg.color_code),
        color_emphasis: parse_hex_color(&cfg.color_emphasis),
        color_quote: parse_hex_color(&cfg.color_quote),
        color_plain: parse_hex_color(&cfg.color_plain),
        color_background: parse_hex_color(&cfg.color_background),
        color_section_rule: parse_hex_color(&cfg.color_section_rule),
        color_viewport: parse_hex_color(&cfg.color_viewport),
        color_viewport_hover: parse_hex_color(&cfg.color_viewport_hover),
    }
}

fn persist_view_setting(app: &mut AppState, key: &str, value: serde_json::Value) {
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

// Status-bar version dropdown lives in the sibling `versions` submodule
// to keep this file under the project's file-length cap. The dropdown
// surfaces the live buffer, every changelog entry for the path (newest
// first, capped at 20), and every pending staging proposal targeting
// the path.
mod versions;
use versions::version_dropdown;

/// Build a decoration layer that paints a subtle line tint plus a gutter
/// marker on every chunk-start line, matching the indexer's heading-aware
/// chunk boundaries (`view-show-chunk-boundaries`).
fn chunk_boundary_decorations(
    editor: &editor_core::EditorState,
) -> editor_core::DecorationSet {
    use editor_core::decoration::{
        Color, Decoration, GutterMarker, LineStyle,
    };
    let text = editor.doc.to_string();
    let chunks = hiker_core::chunker::chunk_markdown(&text);
    let mut set = editor_core::DecorationSet::empty();
    // Faint stripe color (light blue) — picked to be visible against
    // both light and dark themes.
    let stripe = Color::rgba(0x66, 0x99, 0xff, 0x18);
    for (idx, chunk) in chunks.iter().enumerate() {
        if idx == 0 {
            continue; // The first chunk starts at the doc head — skip.
        }
        let byte = chunk.byte_start;
        if byte >= text.len() {
            continue;
        }
        let line = editor.doc.byte_to_line(byte);
        let line_start = editor.doc.line_to_byte(line);
        let line_end = if line + 1 < editor.doc.len_lines() {
            editor.doc.line_to_byte(line + 1)
        } else {
            editor.doc.len_bytes()
        };
        let style = LineStyle {
            bg: Some(stripe),
            gutter_marker: Some(GutterMarker::Custom(
                smol_str::SmolStr::new("S"),
            )),
            ..LineStyle::default()
        };
        set = set.insert(line_start..line_end, Decoration::Line(style));
    }
    set
}

#[cfg(test)]
mod breadcrumb_tests {
    use super::*;
    use crate::buffer::Buffer;

    fn make(text: &str, cursor_byte: usize) -> Buffer {
        let mut buf = Buffer::from_disk(
            "test.md".to_string(),
            text.to_string(),
            String::new(),
        );
        buf.editor.selection = editor_core::Selection::single(cursor_byte);
        buf
    }

    #[test]
    fn empty_when_no_headings() {
        let buf = make("just some text\nno heads here\n", 0);
        assert_eq!(heading_breadcrumb_for(&buf), "");
    }

    #[test]
    fn picks_up_h1() {
        let buf = make("# Title\nbody\n", 9); // cursor on `body`
        assert_eq!(heading_breadcrumb_for(&buf), "Title");
    }

    #[test]
    fn stacks_deeper_headings() {
        let text = "# A\n## B\n### C\nbody\n";
        // cursor on `body` line — should see A /B /C
        let byte = text.find("body").unwrap();
        let buf = make(text, byte);
        assert_eq!(heading_breadcrumb_for(&buf), "A /B /C");
    }

    #[test]
    fn higher_heading_resets_deeper_stack() {
        let text = "# A\n## B\n### C\n## D\nbody\n";
        // After `## D`, deeper headings get popped; expect A /D
        let byte = text.find("body").unwrap();
        let buf = make(text, byte);
        assert_eq!(heading_breadcrumb_for(&buf), "A /D");
    }
}

/// Walk the document from the start up through the cursor's line and
/// return a `>`-joined breadcrumb of the active heading stack. Higher
/// levels override deeper ones, so an H2 resets any prior H3/H4 entries.
/// Returns an empty string when no heading precedes the cursor.
fn heading_breadcrumb_for(buffer: &crate::buffer::Buffer) -> String {
    let cursor_line =
        buffer.editor.doc.byte_to_line(buffer.editor.selection.main().head.byte as usize);
    let mut stack: Vec<(u8, String)> = Vec::new();
    let total_lines = buffer.editor.doc.len_lines();
    for line_idx in 0..=cursor_line {
        let start = buffer.editor.doc.line_to_byte(line_idx);
        let end = if line_idx + 1 < total_lines {
            buffer.editor.doc.line_to_byte(line_idx + 1)
        } else {
            buffer.editor.doc.len_bytes()
        };
        let line: String = buffer.editor.doc.slice(start..end).to_string();
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
                    // not a real heading (# followed by non-space)
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
    stack
        .into_iter()
        .map(|(_, t)| t)
        .collect::<Vec<_>>()
        .join(" /")
}

/// Compute a `DecorationSet` that places `GutterMarker::DiffAdded`,
/// `DiffRemoved`, or `DiffModified` on every line of the live buffer that
/// diverges from `loaded_text` (the most recent disk read / write).
///
/// Strategy: line-level diff via `hiker_core::diff::compute`. Each Insert
/// in the diff is a line in `after` that has no exact counterpart in
/// `before`. We emit `DiffModified` when a Delete on the same after-line
/// preceded the Insert (i.e. a replace), otherwise `DiffAdded`. Pure
/// Deletes don't have a corresponding `after` line to mark, so we collapse
/// adjacent Delete-only runs onto the nearest following surviving line as
/// `DiffRemoved` (matches the legacy gutter behavior).
fn index_diff_decorations(
    loaded_text: &str,
    state: &editor_core::EditorState,
) -> editor_core::DecorationSet {
    use editor_core::{Decoration, GutterMarker, LineStyle, RangeSet};
    use hiker_core::diff::DiffOp;
    let live = state.doc.to_string();
    if loaded_text == live {
        return RangeSet::empty();
    }
    let diff = hiker_core::diff::compute(loaded_text, &live);
    // Per-after-line op summary. We mark each Insert line in the live
    // buffer; pure Deletes get pushed onto the next surviving line.
    let mut per_after_line: std::collections::BTreeMap<u32, GutterMarker> =
        std::collections::BTreeMap::new();
    let mut pending_delete = false;
    let mut last_after_seen: u32 = 0;
    for hunk in &diff.hunks {
        for line in &hunk.lines {
            match line.op {
                DiffOp::Equal => {
                    if let Some(an) = line.after_line_no {
                        last_after_seen = an;
                        if pending_delete {
                            per_after_line.entry(an).or_insert(GutterMarker::DiffRemoved);
                            pending_delete = false;
                        }
                    }
                }
                DiffOp::Insert => {
                    if let Some(an) = line.after_line_no {
                        let marker = if pending_delete {
                            pending_delete = false;
                            GutterMarker::DiffModified
                        } else {
                            GutterMarker::DiffAdded
                        };
                        per_after_line.insert(an, marker);
                        last_after_seen = an;
                    }
                }
                DiffOp::Delete => {
                    pending_delete = true;
                }
            }
        }
    }
    if pending_delete {
        // Trailing deletes at EOF — pin to the last seen line so the
        // marker is at least visible somewhere. Bias by 1 since lines are
        // 1-indexed but the rope is 0-indexed below.
        if last_after_seen > 0 {
            per_after_line
                .entry(last_after_seen)
                .or_insert(GutterMarker::DiffRemoved);
        }
    }

    let doc = &state.doc;
    let total_bytes = doc.len_bytes();
    let total_lines = doc.len_lines();
    let mut entries: Vec<(std::ops::Range<usize>, Decoration)> =
        Vec::with_capacity(per_after_line.len());
    for (line1, marker) in per_after_line {
        let line0 = line1.saturating_sub(1) as usize;
        if line0 >= total_lines {
            continue;
        }
        let line_start = doc.line_to_byte(line0);
        let line_end = if line0 + 1 < total_lines {
            doc.line_to_byte(line0 + 1)
        } else {
            total_bytes
        };
        let range = if line_start == line_end {
            line_start..line_start + 1
        } else {
            line_start..line_end
        };
        entries.push((
            range,
            Decoration::Line(LineStyle {
                gutter_marker: Some(marker),
                ..LineStyle::default()
            }),
        ));
    }
    RangeSet::from_iter(entries)
}

pub(crate) fn status_bar(ui: &mut egui::Ui, app: &mut AppState, path: &str) {
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
                if app.session.buffers.get(path).map(|b| b.is_dirty()).unwrap_or(false) {
                    ui.add(icons::current_dot());
                }
                version_dropdown(ui, app, path, &label);

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
                        ui.add(crate::icons::warn());
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
                        let crumbs = heading_breadcrumb_for(buffer);
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
                            b.editor.selection = editor_core::Selection::single(target_byte);
                        }
                    }
                });
            });
        });
}
