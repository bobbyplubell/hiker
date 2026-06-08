//! Buffer tab body: editor toolbar strip, the editor widget itself, then
//! the status bar. Buffer-only chrome stays here (hidden for all
//! non-buffer kinds).
#![allow(clippy::items_after_test_module)]

pub mod breadcrumb;
pub mod clipboard_menu;
pub mod decorations;
// Interactive mermaid diagram links: click dispatch + hover tooltips.
// status: widget-mermaid-links
pub mod diagram_nav;
pub mod diff_overlay;
pub mod find;
pub(crate) mod editor_binding;
mod format;
// App-side editor widgets: LaTeX render → RGBA helper + the decoration
// provider + the floating edit-preview overlay. `pub(crate)` because the
// overlay's render-cache type lives on `AppState` (`PanelStates::edit_preview`).
// status: widget-render-providers
pub mod widgets;
pub mod minimap_opts;
pub mod patch_review;
pub mod patch_review_pill;
pub mod scrollbar;
pub mod show_changes;
mod toolbar;
pub mod toolbar_menus;
pub mod wikilink_nav;

use std::sync::Arc;

use eframe::egui;

use decorations::DecoRebuildCtx;

use editor_core::theme::light_default;
use editor_egui::widget::Widget as EditorWidget;
use editor_egui::minimap::Options as MinimapOptions;
use editor_egui::minimap::Widget as MinimapWidget;
use minimap_opts::MinimapOptionsExt;
use editor_view::viewport::ClickAction;

use crate::editor_pane;
use crate::icons;
use crate::state::{AppState, ToastLevel};
use hiker_theme as theme;


/// Context bundle for the buffer panel: UI, app, and the active buffer
/// path. Most helpers in this file share these three arguments — making
/// them methods on `&mut self` keeps them factored without tripping
/// `clippy::single_call_fn` (the lint exempts methods with a `self`
/// receiver).
pub(super) struct BufCtx<'a> {
    pub(super) ui: &'a mut egui::Ui,
    pub(super) app: &'a mut AppState,
    pub(super) path: &'a str,
}

use breadcrumb::HeadingBreadcrumb;

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
        // Reader / focus mode (`view-reader-mode`): hide the buffer panel's
        // status bar + pending-rewrite banner so the editor canvas dominates.
        // The toolbar shows by default and hides only when the user opts into
        // `view-reader-hide-toolbar` (see `reader_hides_view_toolbar`). Window-
        // level chrome (top toolbar, side bars, status bar) is hidden by the
        // workbench — see `main::update`. Reader mode is the single workbench/
        // session-level flag, not per-buffer.
        let reader = self.app.workbench.reader_mode();

        // Esc exits reader mode. Consume so it doesn't reach the editor
        // (would otherwise clear selection).
        if reader
            && self.ui.input_mut(|i| {
                i.consume_key(eframe::egui::Modifiers::NONE, eframe::egui::Key::Escape)
            })
        {
            self.app.workbench.set_reader_mode(false);
            return;
        }

        // Esc dismisses the floating edit-preview popup (mermaid / wavedrom /
        // math) when one is showing: hide the popup but keep editing — caret
        // and selection unchanged. The key is consumed ONLY when a popup is
        // actually up, so otherwise Esc still reaches the editor (clears
        // selection). status: widget-edit-popup-dismiss
        if !reader && self.ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            let dismiss_anchor = self.app.session.buffers.get(self.path).and_then(|b| {
                let is_md = match self
                    .path
                    .rsplit_once('.')
                    .map(|(_, e)| e.to_ascii_lowercase())
                    .as_deref()
                {
                    Some("md") => true,
                    Some("txt") => b.render_txt_as_markdown,
                    _ => false,
                };
                let gated = b.render_widgets && b.live_edit_preview && is_md;
                widgets::edit_preview::dismissible_anchor(
                    &self.app.panels.edit_preview,
                    &b.editor,
                    &b.view,
                    gated,
                )
            });
            if let Some(anchor) = dismiss_anchor {
                self.ui.input_mut(|i| {
                    i.consume_key(eframe::egui::Modifiers::NONE, eframe::egui::Key::Escape)
                });
                // Record the dismissal and fall through: the popup is suppressed
                // later this same frame in `show_edit_preview` (it reads the
                // anchor), so the editor still draws — no one-frame skip.
                self.app.panels.edit_preview.dismiss(anchor);
            }
        }

        // Find bar pinned to the top of the buffer panel (above the
        // toolbar) — `editor-find-in-note`. The bar is hidden by default;
        // open via Mod-F.
        find::render_bar(self.ui, self.app, self.path);
        find::tick_rebuild(self.app, self.path);

        // Toolbar across the top of the buffer tab body. [view-reader-hide-toolbar]
        if !self.app.reader_hides_view_toolbar() {
            self.toolbar();
        }

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
        if is_vault && !reader {
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
            let active_session = self
                .app
                .session
                .buffers
                .get(self.path)
                .and_then(|b| b.active_session.clone());
            // Drift + multi-session metadata for the active document, read
            // off the op log. The drifted count rides the pill's `(M
            // drifted)` suffix; the session list backs per-session rows.
            let pill_meta =
                Self::pill_meta(self.app, self.path, active_session.as_deref());
            let pill_action = patch_review_pill::Pill { ui: self.ui }
                .show(&ov.hunks, cursor_byte, &pill_meta);
            if let Some(sel) = &pill_action.select_session
                && let Some(buffer) = self.app.session.buffers.get_mut(self.path)
            {
                // Switch the active session; the per-frame editor binding's
                // overlay step recomputes `agent_proposal` from
                // `materialize_review(new_session)` on the next frame.
                buffer.active_session = sel.clone();
            }
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

    // Resolve+apply a panel-level Ctrl-Z / Ctrl-Shift-Z for the active editor
    // tab BEFORE the buffer is re-borrowed and the widget renders, so the
    // widget paints the reverted state this frame. Any inverse change set this
    // produced seeds `txns` below so the editor binding mirrors it into the
    // `working` layer. See `editor_binding::handle_undo_redo`.
    let undo_txns = editor_binding::handle_undo_redo(ui, app, path);

    // Read live editor settings that the view mirrors per-frame up front under
    // a single short-lived config read lock, so the mutable buffer borrow below
    // doesn't collide with the immutable config borrow. The click patterns are
    // cloned only when they actually differ from the buffer's cached source —
    // string compare under the read lock keeps the steady-state path
    // allocation-free. status: click-select-pattern
    let (scroll_speed, click_patch) = {
        let buffer_view = app.session.buffers.get(path);
        app.vault_session
            .config
            .read()
            .map(|c| {
                let speed = c.editor.scroll_speed.max(0.0);
                let patch = buffer_view.map(|b| {
                    let dbl = (c.editor.double_click_pattern != b.click_patterns.double_src)
                        .then(|| c.editor.double_click_pattern.clone());
                    let trp = (c.editor.triple_click_pattern != b.click_patterns.triple_src)
                        .then(|| c.editor.triple_click_pattern.clone());
                    (dbl, trp)
                });
                (speed, patch)
            })
            .unwrap_or((1.0, None))
    };

    // Reader / focus mode is the workbench-level flag (the single source of
    // truth). Captured before the mutable buffer borrow below so the
    // in-editor chrome (minimap + gutter) can follow it. [view-reader-mode]
    let reader = app.workbench.reader_mode();

    // Persisted diagram-cache context (`widget-render-disk-cache`), built
    // before the mutable buffer borrow so the rebuild closure can carry it
    // owned without borrowing `app`. Honors `[render] cache_diagrams`.
    let diagram_cache = diagram_cache_ctx(app);

    let Some(buffer) = app.session.buffers.get_mut(path) else {
        ui.label(format!("buffer {} not loaded", path));
        return;
    };
    buffer.view.scroll_speed = scroll_speed;
    if let Some((dbl, trp)) = click_patch {
        // Recompiles only the regex(es) the user actually edited; takes effect
        // this frame so close-and-reopen isn't needed.
        buffer.sync_click_patterns(dbl, trp);
    }

    // Decoration layers are rebuilt through the widget's
    // `with_decoration_rebuild` hook below, so they describe the doc state
    // AFTER this frame's keystroke is applied (the widget applies input inside
    // `show`). Building them inline here instead would leave them one edit
    // behind the painted text — the live-preview "flash per keystroke" bug.
    // Most decoration providers take an Option<&Theme> so they fall back to a
    // built-in palette when the host hasn't supplied one.
    let theme_owned = light_default();
    let theme = Some(&theme_owned);

    // Device pixel ratio + markdown-ness captured before the widget borrows
    // `ui`. `dpr` feeds the math raster (physical px = points × dpr) and its
    // cache key (`widget-render-cache`); `is_markdown` gates the widget
    // provider (`widget-render-gating`).
    let dpr = ui.ctx().pixels_per_point();
    let is_markdown = {
        let ext = path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
        match ext.as_deref() {
            Some("md") => true,
            Some("txt") => buffer.render_txt_as_markdown,
            _ => false,
        }
    };

    // Render the editor (left) and the structural minimap (right). The
    // minimap reads the same `ViewState.decorations` the editor paints
    // from, so heading/code/quote classification follows whatever syntax
    // pipeline the host has wired up.
    // Resolve minimap options from the live config snapshot. Cheap each
    // frame — a few field copies + 9 hex parses. Hex parses default back
    // to the built-in palette if the user typed something invalid.
    let mini_opts: Option<MinimapOptions> = if buffer.show_minimap && !reader {
        app.vault_session.config
            .read()
            .ok()
            .map(|c| c.editor.minimap.to_minimap_options())
    } else {
        None
    };
    // Reader view also hides the gutter (line numbers / fold chevrons /
    // diff markers) so only the prose canvas remains. Save the prior
    // hide-gutter state so we can restore it when reader view exits.
    let prev_hide_gutter = buffer.view.hide_gutter;
    if reader {
        buffer.view.hide_gutter = true;
    }

    // Wikilink live-title resolver, built off an Arc clone so it borrows
    // neither `app` nor `buffer`. Runs only inside the cached wikilink layer's
    // rebuild, so the store lock isn't taken per frame.
    let resolve_title =
        wikilink_nav::title_resolver(app.vault_session.services.read_store.clone());

    let click_buffer = &mut buffer.click_buffer;
    let paint_cache = &mut buffer.paint_cache;
    let body = ui.available_rect_before_wrap();
    let minimap_w: f32 = mini_opts.as_ref().map(|o| o.width).unwrap_or(0.0);
    let split_x = (body.right() - minimap_w).max(body.left());
    let editor_rect = egui::Rect::from_min_max(body.min, egui::pos2(split_x, body.max.y));
    // Forward half of the editor binding: a fresh sink that collects, in
    // application order, the change set behind every doc-mutating edit the
    // widget applies from user input this frame (host-applied doc edits — the
    // reverse step below — never re-enter this sink, so there is no echo).
    // Seed the sink with any undo/redo change set resolved above so the
    // editor binding mirrors it into `working` alongside this frame's typing.
    let mut txns: Vec<editor_core::transaction::Transaction> = undo_txns;
    // Set by the editor's right-click "Open in chart editor" menu item; executed
    // after the editor block so `open_block` can take `&mut app`. status: chart-open-in-builder
    let mut chart_open: Option<widgets::chart::EditTarget> = None;
    {
        crate::profile_scope!("Widget::show");
        let mut editor_ui = ui.new_child(egui::UiBuilder::new().max_rect(editor_rect));
        // Disjoint field borrows for the decoration-rebuild hook. Bound as
        // locals BEFORE the widget so the borrow checker sees them as separate
        // from `&mut buffer.editor` / `&mut buffer.view`, which the widget
        // (and the hook) take instead. Scoped inside this `{ }` block so the
        // closure and its captures drop before the minimap reborrows below.
        let mut deco_ctx = DecoRebuildCtx {
            cache: &mut buffer.decoration_cache,
            folds: &buffer.folds,
            loaded_text: &buffer.loaded_text,
            theme,
            live_preview: buffer.live_preview,
            render_widgets: buffer.render_widgets,
            is_markdown,
            dpr,
            font_px: buffer.view.font_size,
            chunk_boundaries: buffer.chunk_boundaries,
            show_whitespace: buffer.show_whitespace,
            highlight_trailing_whitespace: buffer.highlight_trailing_whitespace,
            diff,
            resolve_title: Some(&resolve_title),
            diagram_cache,
            // Bind the chart data resolver to this note's directory so an inline
            // ```chart block's `data: x.csv` resolves note-relative under the
            // vault sandbox (the same machinery wikilinks use). status: widget-chart-render
            chart_resolver: Some(crate::charts::VaultDataResolver::new(
                app.vault_session.vault.as_ref().clone(),
                path,
            )),
        };
        let mut rebuild =
            |editor: &editor_core::state::Editor,
             view: &mut editor_view::viewport::ViewState| {
                decorations::rebuild_editor_layers(editor, view, &mut deco_ctx);
            };
        let editor_resp = EditorWidget::new(&mut buffer.editor, &mut buffer.view)
            .with_click_sink(click_buffer)
            .with_paint_cache(paint_cache)
            .with_transactions_sink(&mut txns)
            .with_decoration_rebuild(&mut rebuild)
            .show(&mut editor_ui);
        // Right-click → "Open in chart editor" (a LEFT click reveals the chart's
        // source like other block widgets, via `edit_targets`). status: chart-open-in-builder
        let chart_targets = widgets::chart::edit_targets(&buffer.editor, theme, None, dpr);
        let chart_menu_target = chart_under_right_click(
            editor_ui.ctx(),
            editor_rect,
            &buffer.view.click_zones,
            &chart_targets,
            egui::Id::new(("chart-ctx-menu", path)),
        );
        clipboard_menu::attach(&editor_resp, chart_menu_target.as_ref(), &mut chart_open);
    }
    if let Some(opts) = mini_opts {
        crate::profile_scope!("Widget::show");
        let minimap_rect =
            egui::Rect::from_min_max(egui::pos2(split_x, body.min.y), body.max);
        let mut mini_ui = ui.new_child(egui::UiBuilder::new().max_rect(minimap_rect));
        MinimapWidget::new(&buffer.editor, &mut buffer.view)
            .with_options(opts)
            .with_cache(&mut buffer.minimap_cache)
            .show(&mut mini_ui);
    } else if !buffer.hide_scrollbar {
        // No minimap → draw a thin auto-hiding scrollbar overlay along
        // the right edge of the editor body. Same affordance role the
        // file tree gets from `ScrollArea::vertical`, just adapted to
        // the editor's hand-rolled scroll model (`view.scroll_y`).
        scrollbar::AutoScrollbar { ui, view: &mut buffer.view, editor_rect }.paint();
    }

    // Snapshot the wikilink-tagged click zones now while the buffer
    // borrow is still live — `wikilink_nav::track_hover` reads them
    // below after the borrow ends, and it needs widget-local rects to
    // hit-test the pointer. Tiny copy: at most a few dozen pills are
    // in the viewport at once. [wikilink-hover-preview]
    let wikilink_zones: Vec<editor_view::viewport::ClickZone> = buffer
        .view
        .click_zones
        .iter()
        .filter(|z| matches!(
            z.action,
            ClickAction::WidgetClick(id)
                if id & editor_md::links::WIKILINK_WIDGET_TAG != 0,
        ))
        .cloned()
        .collect();

    // Diagram-region interaction (`widget-mermaid-links`): rebuild this frame's
    // id → {link, tooltip} registry for the on-screen mermaid widgets, and
    // snapshot their click zones for the hover hit-test. Plus the click-to-edit
    // target map keyed by each on-screen block widget's whole-widget id
    // (`widget-block-click-to-edit`). See [`build_diagram_interaction`].
    let (diagram_registry, diagram_zones, edit_targets) =
        build_diagram_interaction(buffer, theme, is_markdown, dpr);

    // Pull WidgetClicks for patch-review buttons out of the click buffer
    // BEFORE fold-toggle handling so the click_map mapping is consumed
    // here. Other WidgetClick consumers (none today) would chain here too.
    let all_widget_clicks: Vec<u64> = buffer
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
    // Sort WidgetClicks by consumer via `classify_widget_click`. Membership-keyed
    // consumers (diagram regions, block-widget edit targets) are checked BEFORE
    // the wikilink BIT test, because a block widget's whole-widget body-click id
    // is a bare `content_hash` that can coincidentally set ANY reserved tag bit —
    // including `WIKILINK_WIDGET_TAG` — so a bit-first test silently misrouted a
    // mermaid / display-math body click to the wikilink handler (which then does
    // nothing). Region ids and edit-target ids are minted with the wikilink bit
    // clear, so a genuine wikilink pill never lands in either map. Everything
    // unclaimed falls to the diff-overlay hunk consumer.
    let WidgetClickBuckets {
        wikilink: wikilink_clicks,
        diagram: diagram_clicks,
        edit: edit_clicks,
        other: widget_clicks,
    } = classify_widget_clicks(&all_widget_clicks, &diagram_registry, &edit_targets);
    let mod_click = ui.input(|i| i.modifiers.command || i.modifiers.ctrl);

    // Apply fold toggles from this frame's clicks.
    buffer.drain_fold_clicks();

    // Restore the gutter-hidden flag if reader view temporarily
    // overrode it (so flipping reader view off this same frame puts
    // the gutter back without losing the user's persisted preference).
    if reader {
        buffer.view.hide_gutter = prev_hide_gutter;
    }

    // Run the editor binding for op-log-backed vault buffers: forward this
    // frame's captured change sets into the `working` layer, pull
    // `materialize_working` back into the editable buffer, and refresh the
    // agent suggestion overlay (`agent_proposal`). The `buffer` borrow above
    // has ended (last use was `drain_fold_clicks`), so the binding can take
    // `&mut app` freely. Plain disk-only buffers (no op-log doc) fall through.
    editor_binding::run(app, path, &txns);

    // Wikilink click dispatch: resolve each clicked pill's target and open it.
    wikilink_nav::handle_clicks(app, ui.ctx(), path, &wikilink_clicks, mod_click);

    // Wikilink hover-preview: when the pointer rests on a resolved pill,
    // register a hover on the shared note-preview mechanism (the same one
    // the file-tree / canvas sidebars use). Reads the painter's per-frame
    // zones snapshotted above. [wikilink-hover-preview]
    wikilink_nav::track_hover(app, ui, path, editor_rect, &wikilink_zones);

    // Floating live edit-preview overlay: when the main caret reveals a math /
    // mermaid source span, float a non-interactive rendered preview near it.
    // status: widget-edit-popup-preview
    show_edit_preview(app, ui.ctx(), path, editor_rect, theme, dpr, is_markdown);

    // Interactive mermaid diagram regions (`widget-mermaid-links`): dispatch
    // this frame's region clicks through the shared `dispatch_link` mapping,
    // then surface a hover tooltip for any region carrying one
    // (`widget-diagram-hover-tooltip`). Both read the per-frame registry +
    // zones snapshotted above.
    diagram_nav::handle_clicks(app, ui.ctx(), &diagram_clicks, &diagram_registry, mod_click);
    diagram_nav::track_hover(app, ui.ctx(), editor_rect, &diagram_zones, &diagram_registry);

    // Click-to-edit (`widget-block-click-to-edit`): a body click on a rendered
    // block widget (mermaid diagram / display math) places the caret inside its
    // source span, which triggers the existing reveal (source shows + the
    // edit-preview popup) so there's a way into the otherwise-hidden source.
    place_caret_for_block_click(app, ui.ctx(), path, &edit_clicks, &edit_targets);

    // Open-in-builder (`chart-open-in-builder`): the editor's right-click menu set
    // `chart_open` to the chart block under the pointer; open it in the builder.
    if let Some(t) = chart_open {
        crate::panels::charts_tab::open_block(app, path, &t.inner, t.inner_range);
    }

    // Per-hunk overlay-widget click dispatch. The diff overlay maps each
    // button id to the pending op id(s) it covers; we flip them through
    // `op_writes::flip_op_status` and re-materialize the pending-view so the
    // remaining hunks shift / disappear. Restore writes back to disk directly.
    if let Some(ov) = diff
        && !widget_clicks.is_empty()
    {
        for id in widget_clicks {
            let Some(action) = ov.click_map.get(&id) else { continue };
            match action.clone() {
                diff_overlay::HunkAction::Accept(ids) => app.apply_hunk_accept(path, &ids),
                diff_overlay::HunkAction::Reject(ids) => app.apply_hunk_reject(path, &ids),
                // Conflict resolutions (op-log-merge-conflict): keep-mine
                // rejects, keep-both accepts, keep-theirs reverts then accepts.
                diff_overlay::HunkAction::KeepMine(ids) => app.apply_hunk_reject(path, &ids),
                diff_overlay::HunkAction::KeepBoth(ids) => app.apply_hunk_accept(path, &ids),
                diff_overlay::HunkAction::KeepTheirs { op_ids, revert } => {
                    app.apply_hunk_keep_theirs(path, &op_ids, &revert);
                }
                diff_overlay::HunkAction::Restore { path: target, byte_start, byte_end } => {
                    app.apply_hunk_restore(path, &target, byte_start, byte_end);
                }
            }
        }
        // A flip mutated the op log; repaint so the next frame's editor
        // binding re-materializes the buffer / overlay immediately.
        ui.ctx().request_repaint();
    }
    }
}

/// Rebuild this frame's diagram-region interaction data for `buffer`: the
/// id → {link, tooltip} registry for the on-screen mermaid widgets, a snapshot
/// of their click zones (for the hover hit-test), and the click-to-edit target
/// map keyed by every on-screen block widget's whole-widget id
/// (`widget-block-click-to-edit`).
///
/// All three are raster-free (parse + layout only, no resvg blit), so
/// recomputing them each frame is cheap — and keeps them correct even on frames
/// where the decoration cache serves the widget layer from memory. Gated by the
/// same `render_widgets && is_markdown` flag as the widget layer; when off, no
/// diagrams are painted so all three are empty. status: widget-mermaid-links
fn build_diagram_interaction(
    buffer: &crate::buffer::Buffer,
    theme: Option<&editor_core::theme::Theme>,
    is_markdown: bool,
    dpr: f32,
) -> (
    widgets::DiagramRegionRegistry,
    Vec<editor_view::viewport::ClickZone>,
    widgets::WidgetEditTargets,
) {
    if !(buffer.render_widgets && is_markdown) {
        return (
            widgets::DiagramRegionRegistry::new(),
            Vec::new(),
            widgets::WidgetEditTargets::new(),
        );
    }
    let visible = buffer.view.visible_lines();
    let last_line = buffer.editor.doc.len_lines().saturating_sub(1);
    let vis_start = buffer.editor.doc.line_to_byte(visible.start.min(last_line));
    let vis_end = if visible.end.min(last_line) + 1 < buffer.editor.doc.len_lines() {
        buffer.editor.doc.line_to_byte(visible.end.min(last_line) + 1)
    } else {
        buffer.editor.doc.len_bytes()
    };
    let viewport = vis_start..vis_end;
    let registry = widgets::mermaid_link_registry(
        &buffer.editor,
        theme,
        Some(&viewport),
        buffer.view.font_size,
        dpr,
    );
    let edit_targets = widgets::widget_edit_targets(
        &buffer.editor,
        theme,
        Some(&viewport),
        buffer.view.font_size,
        dpr,
    );
    let zones: Vec<editor_view::viewport::ClickZone> = buffer
        .view
        .click_zones
        .iter()
        .filter(|z| matches!(
            z.action,
            ClickAction::WidgetClick(id)
                if id & widgets::MERMAID_REGION_TAG != 0,
        ))
        .cloned()
        .collect();
    (registry, zones, edit_targets)
}

/// The inline ```` ```chart ```` widget the editor's right-click menu should
/// target, if any. On a secondary click, hit-test the pointer against the chart
/// click zones and stash the result (or `None`) in egui temp memory keyed by
/// `menu_id` — so the choice persists while the menu is open and self-corrects on
/// each right-click; then return the currently-stashed target. Reads the global
/// pointer (not the editor response's secondary-click sense) so it's independent
/// of the widget's `Sense`. status: chart-open-in-builder
fn chart_under_right_click(
    ctx: &egui::Context,
    editor_rect: egui::Rect,
    zones: &[editor_view::viewport::ClickZone],
    chart_targets: &widgets::chart::EditTargets,
    menu_id: egui::Id,
) -> Option<widgets::chart::EditTarget> {
    let (secondary, pos) =
        ctx.input(|i| (i.pointer.secondary_clicked(), i.pointer.interact_pos()));
    if secondary {
        let hit = pos.filter(|p| editor_rect.contains(*p)).and_then(|p| {
            let (lx, ly) = (p.x - editor_rect.min.x, p.y - editor_rect.min.y);
            zones.iter().find_map(|z| match z.action {
                ClickAction::WidgetClick(id) if z.rect.contains(lx, ly) => {
                    chart_targets.get(&id).cloned()
                }
                _ => None,
            })
        });
        ctx.data_mut(|d| d.insert_temp(menu_id, hit));
    }
    ctx.data(|d| d.get_temp::<Option<widgets::chart::EditTarget>>(menu_id)).flatten()
}

/// This frame's `WidgetClick` ids sorted by consumer (`widget-block-click-to-edit`).
struct WidgetClickBuckets {
    wikilink: Vec<u64>,
    diagram: Vec<u64>,
    edit: Vec<u64>,
    other: Vec<u64>,
}

/// Sort this frame's `WidgetClick` ids into consumer buckets via
/// [`widgets::classify_widget_click`] (membership-keyed consumers before the
/// wikilink bit test — see that function's note). Pulled out of `show_editor`
/// so the per-bucket routing + the click-drain trace live in one place.
fn classify_widget_clicks(
    all: &[u64],
    diagram_registry: &widgets::DiagramRegionRegistry,
    edit_targets: &widgets::WidgetEditTargets,
) -> WidgetClickBuckets {
    let mut b = WidgetClickBuckets {
        wikilink: Vec::new(),
        diagram: Vec::new(),
        edit: Vec::new(),
        other: Vec::new(),
    };
    for &id in all {
        match widgets::classify_widget_click(id, diagram_registry, edit_targets) {
            widgets::WidgetClickBucket::Diagram => b.diagram.push(id),
            widgets::WidgetClickBucket::Edit => b.edit.push(id),
            widgets::WidgetClickBucket::Wikilink => b.wikilink.push(id),
            widgets::WidgetClickBucket::Other => b.other.push(id),
        }
    }
    if !all.is_empty() {
        tracing::debug!(
            target: "hiker::widget_click",
            ids = ?all,
            wikilink = b.wikilink.len(),
            diagram = b.diagram.len(),
            edit = b.edit.len(),
            other = b.other.len(),
            "widget-click drain",
        );
    }
    b
}

/// Paint the floating live edit-preview overlay for the buffer at `path`: when
/// the main caret reveals a math / mermaid source span, float a
/// non-interactive rendered preview near it, anchored scroll-correctly via
/// `view.line_top_y` + `editor_rect`. Reads the buffer immutably and
/// `app.panels.edit_preview` mutably (disjoint `AppState` fields). The popup is
/// gated on `render_widgets && live_edit_preview && is_markdown` — the
/// `Live edit preview` toggle turns it off independently of in-place widget
/// rendering. status: widget-edit-popup-preview
fn show_edit_preview(
    app: &mut AppState,
    ctx: &egui::Context,
    path: &str,
    editor_rect: egui::Rect,
    theme: Option<&editor_core::theme::Theme>,
    dpr: f32,
    is_markdown: bool,
) {
    // Owned disk-cache context built before the `buffer` borrow so the popup
    // render reuses the persisted diagram cache (`widget-render-disk-cache`).
    let cache = diagram_cache_ctx(app);
    let Some(buffer) = app.session.buffers.get(path) else {
        return;
    };
    let inputs = widgets::edit_preview::PreviewInputs {
        state: &buffer.editor,
        view: &buffer.view,
        editor_rect,
        theme,
        font_px: buffer.view.font_size,
        dpr,
        gated: buffer.render_widgets && buffer.live_edit_preview && is_markdown,
        cache: cache.as_ref(),
    };
    widgets::edit_preview::show(&mut app.panels.edit_preview, ctx, &inputs);
}

/// Build the persisted-diagram-cache context for the current vault, honoring
/// the `[render] cache_diagrams` toggle (default on). `None` when the toggle is
/// off — the render path then uses only the in-memory caches
/// (`widget-render-disk-cache`, `render-cache-diagrams-toggle`).
pub(crate) fn diagram_cache_ctx(app: &AppState) -> Option<widgets::disk_cache::DiagramCacheCtx> {
    let enabled = app
        .vault_session
        .config
        .read()
        .map(|c| c.render.cache_diagrams)
        .unwrap_or(true);
    widgets::disk_cache::DiagramCacheCtx::new(&app.vault_session.vault_root, enabled)
}

/// Route this frame's block-widget body clicks (`widget-block-click-to-edit`):
/// place the caret inside the clicked widget's source span (the offset the
/// edit-target map resolves the id to), which triggers the existing reveal so
/// the hidden source shows for editing. At most one body click is acted on per
/// frame. status: widget-block-click-to-edit
fn place_caret_for_block_click(
    app: &mut AppState,
    ctx: &egui::Context,
    path: &str,
    edit_clicks: &[u64],
    edit_targets: &widgets::WidgetEditTargets,
) {
    if let Some(&id) = edit_clicks.first()
        && let Some(&target) = edit_targets.get(&id)
        && let Some(b) = app.session.buffers.get_mut(path)
    {
        b.editor.selection = editor_core::selection::Selection::single(target);
        // TEMP diagnostic (widget-block-click-to-edit): confirms the caret was
        // placed into the span + the offset. Remove once confirmed.
        tracing::debug!(
            target: "hiker::widget_click",
            id, offset = target, "placed caret into block-widget source span",
        );
        ctx.request_repaint();
    }
}

/// Hunk-mutation methods on `AppState`. Lives here next to the buffer
/// panel because the verbs are entirely UI-driven (the panel surfaces
/// these via per-hunk Accept / Reject / Restore overlay widgets). Methods
/// with `&mut self` receivers are exempt from `clippy::single_call_fn`.
/// (The Agent-owned Accept / Reject verbs live in `patch_review.rs`.)
impl AppState {
    /// Per-hunk Restore: write the snapshot buffer's text for
    /// `[byte_start, byte_end)` back to disk at `target_path`, splicing
    /// it into the current on-disk content. Routes through the op log so
    /// the restore is itself an accepted op the history surfaces show.
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
        let _ = disk_hash;
        let mut new_text = String::with_capacity(disk_text.len() + snippet.len());
        let safe_start = byte_start.min(disk_text.len());
        let safe_end = byte_end.min(disk_text.len()).max(safe_start);
        new_text.push_str(&disk_text[..safe_start]);
        new_text.push_str(&snippet);
        new_text.push_str(&disk_text[safe_end..]);
        match hiker_core::ops::op_writes::user_save(
            self.vault_session.services.oplog.as_ref(),
            &self.vault_session.vault,
            target_path,
            &new_text,
        ) {
            Ok(()) => {
                self.push_toast(
                    format!("Restored hunk to {}", target_path),
                    ToastLevel::Info,
                );
            }
            Err(err) => self.push_toast(format!("Restore failed: {}", err), ToastLevel::Error),
        }
    }
}

/// Snapshot / staging-proposal verbs, surfaced from the read-only
/// source-toolbar. Methods on `AppState` so they're exempt from
/// `clippy::single_call_fn`.
impl AppState {
    pub(super) fn restore_snapshot_to_disk(&mut self, path: &str, op_id: &str) {
        let log = self.vault_session.services.oplog.clone();
        let snapshot_text =
            match hiker_core::ops::op_writes::content_at_op(log.as_ref(), path, op_id) {
                Ok(Some(t)) => t,
                _ => return,
            };
        // Restore writes the version content back through the op log: a fresh
        // `user` op against `accepted` that atomically rewrites the `.md`, so
        // the restore is itself an accepted op the history surfaces show.
        match hiker_core::ops::op_writes::user_save(
            log.as_ref(),
            &self.vault_session.vault,
            path,
            &snapshot_text,
        ) {
            Ok(()) => {
                self.push_toast(format!("Restored snapshot of {}", path), ToastLevel::Info);
            }
            Err(err) => self.push_toast(format!("Restore failed: {}", err), ToastLevel::Error),
        }
    }

    /// Accept a pending whole-file proposal: flip the op to `accepted` via the
    /// op log (`op_writes::flip_op_status` → `OpLog::accept_pending`), which
    /// applies its text edit to `accepted` and atomically rewrites the `.md`.
    /// `proposal_id` is the pending op id; `target_path` the note it targets.
    /// On success, navigate to the target as a preview tab per
    /// `staging-accept-navigates-to-preview`.
    ///
    /// status: write-note-review-surface
    pub(super) fn accept_staging_proposal(&mut self, proposal_id: &str, target_path: &str) {
        let log = self.vault_session.services.oplog.clone();
        match hiker_core::ops::op_writes::flip_op_status(
            log.as_ref(),
            target_path,
            std::slice::from_ref(&proposal_id.to_string()),
            /* accept */ true,
        ) {
            Ok(_) => {
                self.push_toast(format!("Accepted proposal for {}", target_path), ToastLevel::Info);
                editor_pane::open_file(self, target_path, /* sticky */ true);
            }
            Err(err) => self.push_toast(format!("Accept failed: {}", err), ToastLevel::Error),
        }
    }

    /// Reject a pending whole-file proposal: flip the op to `rejected` via the
    /// op log (`op_writes::flip_op_status` → `OpLog::reject_pending`), writing
    /// a rejected audit row and dropping the op from the queue. Disk content is
    /// untouched.
    ///
    /// status: write-note-review-surface
    pub(super) fn reject_staging_proposal(&mut self, proposal_id: &str, target_path: &str) {
        let log = self.vault_session.services.oplog.clone();
        match hiker_core::ops::op_writes::flip_op_status(
            log.as_ref(),
            target_path,
            std::slice::from_ref(&proposal_id.to_string()),
            /* accept */ false,
        ) {
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
    // Pill only surfaces when there's an explicitly-active trail-doc
    // (`vault.active_trail` config).
    let Some(trail_rel) = active_trail_rel(app) else {
        return;
    };
    let trail_name = trail_title(&trail_rel);
    // Idempotency: a note already a waypoint of THIS trail disables the
    // pill (per `trail-add-to-active-from-editor-verb`).
    let already = trail_contains_path(app, &trail_rel, path);

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
        match append_waypoint_to_active(app, &trail_rel, path) {
            Ok(()) => app.push_toast(format!("Added to '{}'", trail_name), ToastLevel::Info),
            Err(e) => app.push_toast(format!("Add to trail failed: {e}"), ToastLevel::Error),
        }
    }
    }
}

/// Vault-relative path of the active trail-doc, from `vault.active_trail`
/// config, filtered to a trail that still exists in the vault listing.
fn active_trail_rel(app: &AppState) -> Option<String> {
    let rel = app.vault_session.config.read().ok()?.vault.active_trail.clone()?;
    let store = app.vault_session.services.read_store.lock().ok()?;
    let exists = hiker_core::trails::list(
        &app.vault_session.vault,
        &store,
        &app.vault_session.services.oplog,
    )
    .unwrap_or_default()
    .into_iter()
    .any(|t| t.rel_path == rel);
    exists.then_some(rel)
}

/// Trail-doc title (basename without `.md`).
fn trail_title(rel: &str) -> String {
    let base = rel.rsplit('/').next().unwrap_or(rel);
    base.strip_suffix(".md").unwrap_or(base).to_string()
}

/// Whether `note_rel` is already a waypoint of the trail at `trail_rel`,
/// via the derived `trail_waypoints` reverse lookup.
fn trail_contains_path(app: &AppState, trail_rel: &str, note_rel: &str) -> bool {
    let Ok(store) = app.vault_session.services.read_store.lock() else {
        return false;
    };
    hiker_core::trails::containing_note_with_paths(
        &app.vault_session.vault,
        &store,
        &app.vault_session.services.oplog,
        note_rel,
    )
    .unwrap_or_default()
    .iter()
    .any(|h| h.trail_doc_rel == trail_rel)
}

/// Append `note_rel` as a waypoint of `trail_rel` (parent `None` ⇒ append
/// cursor) via the core verb on the frame's tokio runtime.
fn append_waypoint_to_active(
    app: &AppState,
    trail_rel: &str,
    note_rel: &str,
) -> Result<(), hiker_core::errors::HikerError> {
    let watcher = app.vault_session.services.watcher.clone();
    let jobs = app.vault_session.services.indexer.job_sender();
    let log = app.vault_session.services.oplog.clone();
    let vault = app.vault_session.vault.clone();
    let (trail_rel, note_rel) = (trail_rel.to_string(), note_rel.to_string());
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => handle.block_on(async {
            hiker_core::trails::ops::append_waypoint(hiker_core::trails::ops::AppendWaypointArgs {
                watcher: &watcher,
                jobs: &jobs,
                log: &log,
                vault: &vault,
                trail_doc_rel: &trail_rel,
                source_rel: &note_rel,
                parent_waypoint_path: None,
                annotation: None,
            })
            .await
            .map(|_| ())
        }),
        Err(_) => Err(hiker_core::errors::HikerError::Io("no tokio runtime".into())),
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
                // Left: version dropdown — unifies the live buffer, changelog
                // snapshots, and any pending agent proposals so the user can
                // flip between them (including back to "Live") without leaving
                // the tab. The dropdown is keyed on the NOTE path (the buffer's
                // source path); `path` here is the buffer-map KEY, which is the
                // note path for a live buffer but a composite key for a snapshot
                // / proposal preview — so derive the note path from the source.
                let note_path = app
                    .session
                    .buffers
                    .get(path)
                    .map_or_else(|| path.to_string(), |b| b.source.path().to_string());
                let basename = note_path.rsplit('/').next().unwrap_or(&note_path);
                let label = basename.to_string();
                if app.session.buffers.get(path).map(super::super::buffer::Buffer::is_dirty).unwrap_or(false) {
                    ui.add(icons::ICONS.current_dot());
                }
                toolbar_menus::Menus { ui, app, path: &note_path }.version_dropdown(&label);

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
                    //
                    // The badge state is *recomputed*, never latched. The
                    // cached stored-hash is refreshed on a coarse 2s timer
                    // (a per-frame SQLite query + std-mutex lock against the
                    // shared read store costs measurable scroll latency, and
                    // the hash only flips when the indexer commits a
                    // re-index). The timer alone, however, would leave the
                    // badge stuck on for up to one window after the index
                    // catches up — so a refresh is *also* forced the moment
                    // the indexer reports this path is no longer pending
                    // (the index just finished). That re-reads the now-equal
                    // stored hash immediately and clears the badge, instead
                    // of waiting for / latching on the timer.
                    let path_owned = path.to_string();
                    let now = std::time::Instant::now();
                    let indexing_pending = app
                        .vault_session
                        .services
                        .indexer
                        .is_pending(&path_owned);
                    // Record the latest pending state and report whether
                    // indexing just transitioned pending → done for this
                    // buffer (the latch-breaking edge).
                    let just_finished_indexing = app
                        .session.buffers
                        .get_mut(&path_owned)
                        .map(|b| {
                            let edge = b.index_pending_last && !indexing_pending;
                            b.index_pending_last = indexing_pending;
                            edge
                        })
                        .unwrap_or(false);
                    let needs_refresh = just_finished_indexing
                        || app
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
                        && buffer.is_ahead_of_index()
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
