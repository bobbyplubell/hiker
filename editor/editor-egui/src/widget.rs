//! `EditorWidget`: drop-in egui widget for an EditorState + ViewState pair.

use std::sync::Arc;

use editor_core::{
    BlockDeco, BlockSide, Color, Decoration, EditorState,
    InlineWidget, LineStyle, MarkStyle,
};
use editor_view::command::{self, Action};
use editor_view::view::{DragState, MeasureCache};
use editor_view::{
    ClickAction, ClickRect, ClickZone, InputEvent, Panel, PanelKind, PanelPlacement, ViewState,
};

mod blocks;
use blocks::paint_block_zone;

/// Per-frame snapshot of the inputs that feed the measure pass. Compared
/// against `ViewState::measure_cache` to decide whether geometry needs to be
/// rebuilt.
#[derive(Clone, Copy)]
struct ViewUpdate {
    /// `state.doc.content_id()` before this frame's input handling. Currently
    /// retained for diagnostics / future change-set integration.
    #[allow(dead_code)]
    pre_doc_id: u64,
    doc_id: u64,
    metrics: u64,
    height_decos: u64,
}

/// Per-(buffer-line, vline-index) cached layout result. Lives across frames;
/// owned by the host via `PaintCache` and passed into the widget through
/// `EditorWidget::with_paint_cache`. Invalidated per-entry when any of
/// `(text_hash, doc_id, sel_line, layers_sig, metrics)` changes for that
/// entry. Entries unreferenced for several frames are evicted.
struct CachedRow {
    last_used_frame: u64,
    text_hash: u64,
    doc_id: u64,
    sel_line: u64,
    layers_sig: u64,
    metrics: u64,
    layout: LineLayout,
    measured: LineMeasured,
}

/// Per-widget paint cache. The host stores one of these alongside its
/// `EditorState` + `ViewState` (e.g. on a `Buffer` struct) and passes a
/// `&mut` into the widget via `EditorWidget::with_paint_cache`. When no
/// external cache is provided, `show` falls back to a transient one that
/// lives only for the duration of the call — fine for one-shot renders
/// (tests, previews) but loses cross-frame reuse.
#[derive(Default)]
pub struct PaintCache {
    frame: u64,
    entries: std::collections::HashMap<(usize, usize), CachedRow>,
}

impl PaintCache {
    /// Drop entries that weren't accessed in the last `max_age` frames.
    fn evict_stale(&mut self, max_age: u64) {
        let cutoff = self.frame.saturating_sub(max_age);
        self.entries.retain(|_, e| e.last_used_frame >= cutoff);
    }
}

/// Cheap deterministic hash of a string slice.
fn hash_str(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Stable fingerprint over the metrics that, if changed, invalidate the
/// heightmap or wrap cache (size, font, gutter, wrap settings).
fn compute_metrics_fingerprint(view: &ViewState) -> u64 {
    let bits = [
        view.width.to_bits() as u64,
        view.height.to_bits() as u64,
        view.font_size.to_bits() as u64,
        view.line_height.to_bits() as u64,
        view.gutter_width.to_bits() as u64,
        view.wrap_map.width().to_bits() as u64,
        view.wrap_map.enabled() as u64,
    ];
    let mut acc: u64 = 0xA076_1D64_78BD_642F;
    for b in bits {
        acc ^= b.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        acc = acc.rotate_left(27);
    }
    acc
}
use egui::{
    epaint::text::{LayoutJob, TextFormat},
    Color32, FontFamily, FontId, Pos2, Rect, Sense, Stroke,
};
use smol_str::SmolStr;

pub struct EditorWidget<'a> {
    pub state: &'a mut EditorState,
    pub view: &'a mut ViewState,
    pub clicks_out: Option<&'a mut Vec<ClickAction>>,
    /// Optional host-owned paint cache. When `None`, `show` falls back to
    /// a transient cache that lives only for the duration of the call —
    /// fine for one-shot renders (tests, previews) but loses cross-frame
    /// galley reuse. Persistent hosts (the main buffer panel) should pass
    /// a `&mut PaintCache` via `with_paint_cache`.
    pub paint_cache: Option<&'a mut PaintCache>,
}

impl<'a> EditorWidget<'a> {
    pub fn new(state: &'a mut EditorState, view: &'a mut ViewState) -> Self {
        Self { state, view, clicks_out: None, paint_cache: None }
    }

    /// Configure a sink that receives any `ClickAction`s the widget produced
    /// this frame (e.g. an Expander toggle). The host pops them after `show`
    /// returns and applies them to its fold / region state.
    pub fn with_click_sink(mut self, sink: &'a mut Vec<ClickAction>) -> Self {
        self.clicks_out = Some(sink);
        self
    }

    /// Plug in a host-owned `PaintCache` so per-line galleys survive
    /// across frames. Without this the widget reuses nothing — every
    /// paint rebuilds every line layout.
    pub fn with_paint_cache(mut self, cache: &'a mut PaintCache) -> Self {
        self.paint_cache = Some(cache);
        self
    }

    pub fn show(mut self, ui: &mut egui::Ui) -> egui::Response {
        // Phase 0: layout — claim screen space and compute the text rect.
        let desired = ui.available_size();
        let (rect, response) = ui.allocate_exact_size(desired, Sense::click_and_drag());
        sync_search_panel(self.view);
        let (top_h, bottom_h) = self.view.panels.heights();
        let text_rect = Rect::from_min_max(
            Pos2::new(rect.min.x, rect.min.y + top_h),
            Pos2::new(rect.max.x, rect.max.y - bottom_h),
        );

        // Phase 1: update — pull frame inputs onto ViewState (size, fonts),
        // run input events, and capture what changed.
        let update = self.update(ui, &response, rect, text_rect);

        // Phase 2: measure — heightmap + wrap recompute. Skipped entirely
        // when no input that affects geometry has changed.
        self.measure(update);

        // Phase 3: paint — always runs, but reads cached geometry built in
        // measure.
        self.view.click_zones.clear();
        let has_focus = response.has_focus();
        // Fall back to a transient cache when the host didn't provide
        // one. Persistent panels (the main buffer panel) supply a
        // `&mut PaintCache` via `with_paint_cache`; tests/previews and
        // legacy call sites get a fresh cache per frame.
        let mut fallback_cache = PaintCache::default();
        let cache: &mut PaintCache = match self.paint_cache.as_deref_mut() {
            Some(c) => c,
            None => &mut fallback_cache,
        };
        paint(ui, self.state, self.view, cache, text_rect, has_focus);
        crate::tooltip::paint_tooltips(ui, self.view, self.state, text_rect);
        crate::completion::paint_completion_popup(ui, self.view, self.state, text_rect);
        refresh_search_matches(self.state, self.view);
        crate::panel::paint_panels(ui, self.view, rect, top_h, bottom_h);
        response
    }

    /// Phase 1: sync per-frame screen metrics onto the view, handle input
    /// events, and emit a [`ViewUpdate`] summarising which geometry inputs
    /// changed this frame.
    fn update(
        &mut self,
        ui: &mut egui::Ui,
        response: &egui::Response,
        rect: Rect,
        text_rect: Rect,
    ) -> ViewUpdate {
        self.view.width = text_rect.width();
        self.view.height = text_rect.height();
        let font_id = FontId::new(self.view.font_size, FontFamily::Monospace);
        self.view.line_height = ui.fonts(|f| f.row_height(&font_id));
        let char_width = ui
            .fonts(|f| f.layout_no_wrap("M".into(), font_id.clone(), Color32::WHITE))
            .size()
            .x;
        let text_area_w = (rect.width() - self.view.gutter_width).max(0.0);
        self.view.wrap_map.set_char_width(char_width);
        self.view.wrap_map.set_width(text_area_w);

        // Snapshot the inputs that drove last frame's measure. We compare
        // against the post-input values below.
        let pre_doc_id = self.state.doc.content_id() as u64;

        // Grant focus on any pointer press, not just a completed click —
        // a press that becomes a drag never fires `clicked()`, so without
        // this the widget never takes focus when the user click-drags to
        // select, and arrow keys end up driving egui's focus traversal
        // instead of moving the caret.
        if response.clicked()
            || (response.is_pointer_button_down_on()
                && ui.input(|i| i.pointer.primary_pressed()))
        {
            response.request_focus();
        }
        let has_focus = response.has_focus();
        // While focused, swallow Tab / Arrow / Escape so egui's default
        // focus-traversal doesn't yank focus to a nearby button when the
        // user is just trying to move the caret.
        if has_focus {
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    response.id,
                    egui::EventFilter {
                        tab: true,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: true,
                    },
                )
            });
        }
        // Mouse events MUST flow even when focus hasn't been granted yet —
        // the very first click on an unfocused widget is what grants focus,
        // and dropping that click breaks selection start. Keyboard events
        // remain focus-gated.
        let mods = ui.input(|i| editor_view::Modifiers {
            ctrl: i.modifiers.ctrl,
            alt: i.modifiers.alt,
            shift: i.modifiers.shift,
            meta: i.modifiers.mac_cmd,
        });
        let has_active_drag = !matches!(self.view.drag, DragState::Idle);
        for ev in crate::translate::pointer_mouse_events(ui.ctx(), response, rect, has_active_drag) {
            let action = command::handle_mouse_with_mods(self.state, self.view, &ev, mods);
            self.consume_action(ui, action);
        }
        if has_focus {
            let events: Vec<InputEvent> = ui.input(|i| {
                i.events
                    .iter()
                    .filter_map(crate::translate::translate)
                    .collect()
            });
            for ev in events {
                let action = command::handle(self.state, self.view, &ev);
                self.consume_action(ui, action);
            }
        }
        if response.hovered() || response.has_focus() {
            let scrolled = ui.input(|i| i.smooth_scroll_delta.y);
            if scrolled.abs() > 0.0 {
                let action = command::handle(
                    self.state,
                    self.view,
                    &InputEvent::Scroll { delta_x: 0.0, delta_y: scrolled },
                );
                self.consume_action(ui, action);
            }
        }

        let doc_id = self.state.doc.content_id() as u64;
        let metrics = compute_metrics_fingerprint(self.view);
        let height_decos = self.view.decorations.height_signature;

        ViewUpdate {
            pre_doc_id,
            doc_id,
            metrics,
            height_decos,
        }
    }

    /// Phase 2: rebuild the heightmap + wrap cache only when needed. Reads
    /// `view.measure_cache` to detect a no-op; updates it on a real pass.
    fn measure(&mut self, update: ViewUpdate) {
        let cache = self.view.measure_cache;
        let metrics_changed = cache.metrics != update.metrics;
        let doc_changed = cache.doc_id != update.doc_id;
        let decos_changed = cache.height_decos != update.height_decos;

        // The viewport band is line-quantized; recompute it from the current
        // scroll AFTER any input that may have moved scroll_y.
        let mut viewport = self.view.visible_lines();
        let viewport_changed = (viewport.start, viewport.end) != cache.viewport;

        if !(metrics_changed || doc_changed || decos_changed || viewport_changed) {
            return;
        }

        // Wrap cache: width/char_width/wrap-enabled changes already invalidated
        // it via `set_width` / `set_char_width` / `set_enabled`. Doc changes
        // invalidate per-line entries via the command layer's `invalidate_line`
        // calls. So all we need to do here is ensure capacity and prewrap the
        // visible band.
        self.view.sync_to(self.state);
        self.view.wrap_map.ensure_capacity(self.state.doc.len_lines());
        prewrap_visible(self.view, self.state);
        apply_line_height_decorations(self.view, self.state);

        // Viewport may have shifted as a result of the geometry rebuild
        // (heights changing under us). Re-read for the cache snapshot.
        viewport = self.view.visible_lines();
        self.view.measure_cache = MeasureCache {
            doc_id: update.doc_id,
            height_decos: update.height_decos,
            metrics: update.metrics,
            viewport: (viewport.start, viewport.end),
        };
    }

    fn consume_action(&mut self, ui: &egui::Ui, action: Action) {
        match action {
            Action::Replace(s) => *self.state = s,
            Action::Copy(t) => ui.ctx().copy_text(t),
            Action::Click(c) => {
                if let Some(sink) = self.clicks_out.as_deref_mut() {
                    sink.push(c);
                }
            }
            Action::None => {}
        }
    }
}

/// Synchronize the auto-managed Search panel with `view.search.active`.
/// When the user opens search (Cmd-F), push a bottom-anchored Search panel;
/// when they close it, remove any registered Search panel. The panel uses a
/// reserved id (`SEARCH_PANEL_ID`) so it can be re-found across frames.
fn sync_search_panel(view: &mut ViewState) {
    let has = view
        .panels
        .panels
        .iter()
        .any(|p| matches!(p.kind, PanelKind::Search));
    if view.search.active && !has {
        view.panels.panels.push(Panel {
            id: SEARCH_PANEL_ID,
            placement: PanelPlacement::Bottom,
            height: 36.0,
            kind: PanelKind::Search,
        });
    } else if !view.search.active && has {
        view.panels
            .panels
            .retain(|p| !matches!(p.kind, PanelKind::Search));
    }
}

const SEARCH_PANEL_ID: u64 = 0x5EA8_C400_0000_0001;

/// Re-run the search after panel interaction so the visible match list and
/// decorations stay in sync with `view.search.query` / flags. Cheap when the
/// query is empty (early-return inside `run_search`).
fn refresh_search_matches(state: &EditorState, view: &mut ViewState) {
    if !view.search.active {
        return;
    }
    let matches = editor_view::run_search(state, &view.search.query, view.search.flags);
    if matches != view.search.matches {
        view.search.matches = matches;
        if view.search.matches.is_empty() {
            view.search.current_idx = None;
        } else if view
            .search
            .current_idx
            .map(|i| i >= view.search.matches.len())
            .unwrap_or(true)
        {
            view.search.current_idx = Some(0);
        }
    }
}

fn apply_line_height_decorations(view: &mut ViewState, state: &EditorState) {
    let base = view.line_height;
    let total_lines = state.doc.len_lines();
    // O(K) reset over existing overrides rather than O(N) over every
    // line in the doc. apply runs every scroll frame (viewport change
    // invalidates measure cache), so the loop body cost matters a lot
    // on long files.
    view.height_map.reset_text_heights();
    view.height_map.clear_blocks();
    let has_height_layers = !view.decorations.height_indices.is_empty();
    if !has_height_layers && !view.wrap_map.enabled() {
        // Fast path: nothing to apply; prefix needs to reflect the base-height
        // reset above.
        view.height_map.recompute();
        return;
    }
    let doc_len = state.doc.len_bytes();
    // Only scan layers flagged as height-affecting. The painter still walks
    // every layer separately for marks/replace/widgets.
    for layer in view.decorations.height_layers() {
        for (range, deco) in layer.iter_overlapping(0..doc_len + 1) {
            match deco {
                Decoration::Line(LineStyle { hide: true, .. }) => {
                    let line = state.doc.byte_to_line(range.start.min(doc_len));
                    view.height_map.set_line_height(line, 0.0);
                }
                Decoration::Line(LineStyle { height_scale: Some(scale), .. }) => {
                    let line = state.doc.byte_to_line(range.start);
                    view.height_map.set_line_height(line, base * scale);
                }
                Decoration::Block(BlockDeco { side, height, .. }) => {
                    let line = state.doc.byte_to_line(range.start.min(doc_len));
                    match side {
                        BlockSide::Above => view.height_map.add_block_above(line, *height),
                        BlockSide::Below => view.height_map.add_block_below(line, *height),
                    }
                }
                Decoration::BlockWidget { side, widget } => {
                    let line = state.doc.byte_to_line(range.start.min(doc_len));
                    let h = widget.measure(view.font_size, view.width);
                    match side {
                        BlockSide::Above => view.height_map.add_block_above(line, h),
                        BlockSide::Below => view.height_map.add_block_below(line, h),
                    }
                }
                _ => {}
            }
        }
    }
    // Apply soft-wrap multiplier: a line with N visual rows is N× taller
    // (unless hidden, height==0).
    if view.wrap_map.enabled() {
        for line in 0..total_lines {
            if let Some(w) = view.wrap_map.peek(line) {
                let vc = w.visual_count();
                if vc > 1 {
                    let h = view.height_map.text_height(line);
                    if h > 0.0 {
                        view.height_map.set_line_height(line, h * vc as f32);
                    }
                }
            }
        }
    }
    view.height_map.recompute();
}

struct PaintCtx<'a> {
    ui: &'a mut egui::Ui,
    painter: egui::Painter,
    rect: Rect,
    text_origin_x: f32,
    base_font_id: FontId,
    text_color: Color32,
    selection_color: Color32,
    cursor_color: Color32,
    gutter_color: Color32,
    hatched_default: Color,
    has_focus: bool,
}

fn paint(
    ui: &mut egui::Ui,
    state: &EditorState,
    view: &mut ViewState,
    cache: &mut PaintCache,
    rect: Rect,
    has_focus: bool,
) {
    // Bump the paint-cache frame counter once per paint, so per-row hits
    // refresh `last_used_frame` and we can evict rows that fell off-screen.
    cache.frame = cache.frame.wrapping_add(1);
    // Evict rows untouched for more than ~120 frames (≈2 seconds at 60 Hz).
    cache.evict_stale(120);

    let visuals = ui.visuals().clone();
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, visuals.extreme_bg_color);

    // Placeholder text: when the doc is empty and a placeholder is set, paint
    // it dimmed at the text origin and return early. See SPEC §9.12.
    if state.doc.is_empty() {
        if let Some(placeholder) = view.placeholder.clone() {
            let text_origin_x =
                rect.left() + if view.hide_gutter { 4.0 } else { view.gutter_width };
            let font_id = FontId::new(view.font_size, FontFamily::Monospace);
            let dim = visuals.weak_text_color();
            painter.text(
                Pos2::new(text_origin_x, rect.top()),
                egui::Align2::LEFT_TOP,
                placeholder.as_str(),
                font_id,
                dim,
            );
            if has_focus {
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
            }
            return;
        }
    }

    let hatched_default = if visuals.dark_mode {
        Color::rgba(180, 180, 200, 30)
    } else {
        Color::rgba(120, 120, 140, 45)
    };
    let mut ctx = PaintCtx {
        ui,
        painter,
        rect,
        text_origin_x: rect.left() + if view.hide_gutter { 4.0 } else { view.gutter_width },
        base_font_id: FontId::new(view.font_size, FontFamily::Monospace),
        text_color: visuals.text_color(),
        // egui's `visuals.selection.bg_fill` is opaque and tuned for
        // filling button shapes — used directly here it would obscure
        // the glyphs underneath. We want a translucent tint instead.
        // `linear_multiply(0.5)` was the previous attempt, but Color32
        // is premultiplied, so multiplying scales the alpha too —
        // dropping it to ~50% of an already-dark color on a dark
        // background made the selection visually disappear. Build a
        // fixed-alpha overlay from the theme accent instead.
        selection_color: {
            let s = visuals.selection.bg_fill;
            Color32::from_rgba_unmultiplied(s.r(), s.g(), s.b(), 110)
        },
        cursor_color: visuals.text_color(),
        gutter_color: visuals.weak_text_color(),
        hatched_default,
        has_focus,
    };

    for line_idx in view.visible_lines() {
        if line_idx >= state.doc.len_lines() {
            break;
        }
        paint_visible_line(&mut ctx, state, view, cache, line_idx);
    }

    if let DragState::DraggingSelection { drop_caret } = view.drag {
        paint_drop_caret(&mut ctx, state, view, drop_caret);
    }

    if !view.hide_gutter {
        let sep_x = rect.left() + view.gutter_width - 2.0;
        ctx.painter.line_segment(
            [Pos2::new(sep_x, rect.top()), Pos2::new(sep_x, rect.bottom())],
            Stroke::new(1.0, visuals.weak_text_color().gamma_multiply(0.3)),
        );
    }
    if has_focus {
        ctx.ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
    }
}

/// Paint a thin vertical "drop indicator" caret at `drop_caret` (byte
/// offset). Used during text drag-and-drop to show where a release would
/// insert the dragged text. SPEC §9.19.
fn paint_drop_caret(
    ctx: &mut PaintCtx<'_>,
    state: &EditorState,
    view: &ViewState,
    drop_caret: usize,
) {
    if state.doc.len_lines() == 0 {
        return;
    }
    let clamped = drop_caret.min(state.doc.len_bytes());
    let line = state.doc.byte_to_line(clamped);
    if line >= state.doc.len_lines() {
        return;
    }
    let row_top_y = ctx.rect.top() + view.line_top_y(line);
    let above_h = view.height_map.block_above(line);
    let text_top_y = row_top_y + above_h;
    let row_h = view.height_map.text_height(line).max(view.line_height);

    let line_start = state.doc.line_to_byte(line);
    let line_text = state.doc.line_str(line);
    let local = clamped.saturating_sub(line_start).min(line_text.len());
    let prefix = &line_text[..local];
    let font_id = ctx.base_font_id.clone();
    let galley = ctx
        .ui
        .fonts(|f| f.layout_no_wrap(prefix.to_string(), font_id, Color32::WHITE));
    let x = ctx.text_origin_x + galley.size().x;

    let color = Color32::from_rgb(0, 150, 200);
    let r = Rect::from_min_max(
        Pos2::new(x - 0.75, text_top_y),
        Pos2::new(x + 0.75, text_top_y + row_h),
    );
    ctx.painter.rect_filled(r, 0.0, color);
}

fn paint_visible_line(ctx: &mut PaintCtx<'_>, state: &EditorState, view: &mut ViewState, cache: &mut PaintCache, line_idx: usize) {
    let row_top_y = ctx.rect.top() + view.line_top_y(line_idx);
    let above_h = view.height_map.block_above(line_idx);
    let below_h = view.height_map.block_below(line_idx);
    let line_top_y = row_top_y + above_h;
    let row_height = view.row_height(line_idx);
    let line_text = state.doc.line_str(line_idx);
    let line_byte_start = state.doc.line_to_byte(line_idx);
    let line_byte_end = line_byte_start + line_text.len();
    let is_hidden = view.height_map.text_height(line_idx) <= 0.5;

    if above_h > 0.0 {
        let zone_rect = Rect::from_min_max(
            Pos2::new(ctx.rect.left(), row_top_y),
            Pos2::new(ctx.rect.right(), row_top_y + above_h),
        );
        paint_block_zone(
            ctx.ui, &ctx.painter, &view.decorations.layers,
            line_byte_start, line_byte_end, BlockSide::Above,
            zone_rect, view.font_size, view.line_height,
            ctx.text_origin_x, ctx.hatched_default,
            &mut view.click_zones, ctx.rect,
        );
    }

    if !is_hidden {
        let vlines: Vec<(usize, usize)> = if view.wrap_map.enabled() {
            view.wrap_map
                .peek(line_idx)
                .map(|w| w.vlines.iter().map(|(s, e)| (*s as usize, *e as usize)).collect())
                .unwrap_or_else(|| vec![(0usize, line_text.len())])
        } else {
            vec![(0usize, line_text.len())]
        };
        let vline_count = vlines.len().max(1) as f32;
        // For scaled lines (markdown headings) the heightmap allocates
        // `scale * line_height` vertical space. Each vline gets an equal
        // share so the gutter number + segment text center inside the
        // line's actual extent rather than hugging the top.
        let row_h = (view.height_map.text_height(line_idx) / vline_count)
            .max(view.line_height);
        for (vi, (vs, ve)) in vlines.iter().enumerate() {
            let vline_top_y = line_top_y + (vi as f32) * row_h;
            let vline_byte_start = line_byte_start + *vs;
            let vline_byte_end = line_byte_start + *ve;
            let vline_text = &line_text[*vs..*ve];
            let is_first_vline = vi == 0;
            paint_text_row(
                ctx, state, view, cache, line_idx, vline_text,
                vline_byte_start, vline_byte_end, vline_top_y, row_h, is_first_vline,
            );
        }
    }

    if below_h > 0.0 {
        let zone_top = if is_hidden { line_top_y } else { line_top_y + row_height };
        let zone_rect = Rect::from_min_max(
            Pos2::new(ctx.rect.left(), zone_top),
            Pos2::new(ctx.rect.right(), zone_top + below_h),
        );
        paint_block_zone(
            ctx.ui, &ctx.painter, &view.decorations.layers,
            line_byte_start, line_byte_end, BlockSide::Below,
            zone_rect, view.font_size, view.line_height,
            ctx.text_origin_x, ctx.hatched_default,
            &mut view.click_zones, ctx.rect,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_text_row(
    ctx: &mut PaintCtx<'_>,
    state: &EditorState,
    view: &mut ViewState,
    cache: &mut PaintCache,
    line_idx: usize,
    vline_text: &str,
    vline_byte_start: usize,
    vline_byte_end: usize,
    vline_top_y: f32,
    row_h: f32,
    is_first_vline: bool,
) {
    // Fingerprint inputs that, if any changes, invalidate the cached layout
    // for this row. text_hash catches in-place edits to a same-length line;
    // doc_id catches buffer mutations; sel_line catches cursor-on-line
    // reveal decorations; layers_sig catches changed decoration sets;
    // metrics catches font/size/width changes.
    let text_hash = hash_str(vline_text);
    let doc_id = state.doc.content_id() as u64;
    let sel_line = state
        .doc
        .byte_to_line(state.selection.main().head.offset().min(state.doc.len_bytes()))
        as u64;
    let layers_sig = view.decorations.signature;
    let metrics = compute_metrics_fingerprint(view);
    let key = (line_idx, vline_byte_start);

    // Cache lookup. On a hit we clone the stored layout/measured (cheap —
    // galleys are Arc<Galley>, segments hold SmolStr + Arc<dyn InlineWidget>).
    let frame = cache.frame;
    let hit = cache
        .entries
        .get(&key)
        .filter(|e| {
            e.text_hash == text_hash
                && e.doc_id == doc_id
                && e.sel_line == sel_line
                && e.layers_sig == layers_sig
                && e.metrics == metrics
        })
        .map(|e| (e.layout.clone(), e.measured.clone()));

    let (layout, measured) = if let Some((l, m)) = hit {
        if let Some(entry) = cache.entries.get_mut(&key) {
            entry.last_used_frame = frame;
        }
        (l, m)
    } else {
        // Build fresh.
        let layout = build_line_layout(
            vline_text, vline_byte_start, &view.decorations.layers,
            view.font_size, ctx.text_color,
        );
        let measured = layout.measure(ctx.ui);
        cache.entries.insert(
            key,
            CachedRow {
                last_used_frame: cache.frame,
                text_hash,
                doc_id,
                sel_line,
                layers_sig,
                metrics,
                layout: layout.clone(),
                measured: measured.clone(),
            },
        );
        (layout, measured)
    };

    if is_first_vline {
        paint_line_bgs(ctx, state, view, line_idx, vline_byte_start, vline_byte_end, vline_top_y, row_h);
        if !view.hide_gutter {
            paint_gutter(ctx, view, line_idx, vline_byte_start, vline_byte_end, vline_top_y, row_h);
        }
    } else if let Some(bg) = wrapped_continuation_bg(state, view, line_idx) {
        let r = Rect::from_min_max(
            Pos2::new(ctx.rect.left(), vline_top_y),
            Pos2::new(ctx.rect.right(), vline_top_y + row_h),
        );
        ctx.painter.rect_filled(r, 0.0, bg);
    }
    paint_selections(
        ctx, state, view, line_idx, vline_byte_start, vline_byte_end,
        vline_top_y, row_h, &measured, vline_text.len(),
    );
    paint_segments(ctx, &layout, &measured, vline_top_y, row_h, &mut view.click_zones);
    paint_cursors(ctx, state, vline_byte_start, vline_byte_end, vline_top_y, row_h, &measured);
}

/// If the buffer line has a Line bg, continuation vlines (wrap rows after the
/// first) should also paint that bg so the highlight runs through.
fn wrapped_continuation_bg(state: &EditorState, view: &ViewState, line_idx: usize) -> Option<Color32> {
    let line_byte_start = state.doc.line_to_byte(line_idx);
    for layer in &view.decorations.layers {
        for (range, deco) in layer.iter_overlapping(line_byte_start..line_byte_start + 1) {
            if let Decoration::Line(LineStyle { bg: Some(c), .. }) = deco {
                if state.doc.byte_to_line(range.start) == line_idx {
                    return Some(to_egui_color(*c));
                }
            }
        }
    }
    None
}

fn prewrap_visible(view: &mut ViewState, state: &EditorState) {
    if !view.wrap_map.enabled() {
        return;
    }
    let total = state.doc.len_lines();
    // Two-phase strategy: first pass over ALL lines uses cached vline counts
    // (O(line_count) but O(1) per cached line) to keep `total_visual_lines`
    // and the height map honest; second pass only recomputes wraps for lines
    // intersecting the viewport + margin (which is where stale entries from
    // edits matter, and where character measurement is hot).
    //
    // Initial population (cold cache): walk all lines once so the height map
    // gets a valid total. After that, only the visible band recomputes.
    let cold = view.wrap_map.peek(0).is_none();
    let visible = view.visible_lines();
    let margin = 32usize;
    let scope_start = visible.start.saturating_sub(margin);
    let scope_end = (visible.end + margin).min(total);

    if cold {
        for line in 0..total {
            let text = state.doc.line_str(line);
            view.wrap_map.get_or_compute(line, |_| text.clone());
        }
        return;
    }
    for line in scope_start..scope_end {
        let text = state.doc.line_str(line);
        view.wrap_map.get_or_compute(line, |_| text.clone());
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_line_bgs(
    ctx: &PaintCtx<'_>,
    state: &EditorState,
    view: &ViewState,
    line_idx: usize,
    line_byte_start: usize,
    line_byte_end: usize,
    line_top_y: f32,
    row_height: f32,
) {
    for layer in &view.decorations.layers {
        for (range, deco) in layer.iter_overlapping(line_byte_start..line_byte_end + 1) {
            if let Decoration::Line(LineStyle { bg: Some(c), .. }) = deco {
                if state.doc.byte_to_line(range.start) == line_idx {
                    let r = Rect::from_min_max(
                        Pos2::new(ctx.rect.left(), line_top_y),
                        Pos2::new(ctx.rect.right(), line_top_y + row_height),
                    );
                    ctx.painter.rect_filled(r, 0.0, to_egui_color(*c));
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_selections(
    ctx: &PaintCtx<'_>,
    state: &EditorState,
    view: &ViewState,
    line_idx: usize,
    line_byte_start: usize,
    line_byte_end: usize,
    line_top_y: f32,
    row_height: f32,
    measured: &LineMeasured,
    line_text_len: usize,
) {
    for r in state.selection.ranges() {
        let (s, e) = (r.start(), r.end());
        if e < line_byte_start || s > line_byte_end {
            continue;
        }
        let local_start = s.saturating_sub(line_byte_start);
        let local_end = (e - line_byte_start).min(line_text_len);
        if !r.is_empty() {
            let x_start = measured.x_at_buffer_offset(local_start);
            let x_end = measured.x_at_buffer_offset(local_end);
            let sel = Rect::from_min_max(
                Pos2::new(ctx.text_origin_x + x_start, line_top_y),
                Pos2::new(ctx.text_origin_x + x_end, line_top_y + row_height),
            );
            ctx.painter.rect_filled(sel, 0.0, ctx.selection_color);
        }
        if e > line_byte_end && line_idx + 1 < state.doc.len_lines() {
            let x_end = measured.total_width;
            let extra = Rect::from_min_max(
                Pos2::new(ctx.text_origin_x + x_end, line_top_y),
                Pos2::new(ctx.text_origin_x + x_end + view.font_size * 0.5, line_top_y + row_height),
            );
            ctx.painter.rect_filled(extra, 0.0, ctx.selection_color);
        }
    }
}

fn paint_gutter(
    ctx: &PaintCtx<'_>,
    view: &mut ViewState,
    line_idx: usize,
    line_byte_start: usize,
    line_byte_end: usize,
    line_top_y: f32,
    row_height: f32,
) {
    let num = (line_idx + 1).to_string();
    let num_galley = ctx
        .ui
        .fonts(|f| f.layout_no_wrap(num, ctx.base_font_id.clone(), ctx.gutter_color));
    let num_x = ctx.rect.left() + view.gutter_width - num_galley.size().x - 6.0;
    let num_y = line_top_y + (row_height - num_galley.size().y) * 0.5;
    ctx.painter
        .galley(Pos2::new(num_x, num_y), num_galley, ctx.gutter_color);

    if let Some(ch) = collect_fold_chevron(&view.decorations.layers, line_byte_start, line_byte_end) {
        // Draw chevron as a small filled triangle via Painter::add(Shape::convex_polygon)
        // — Unicode triangle glyphs aren't reliably present in egui's bundled
        // fonts (would render as the missing-glyph box).
        let size = ctx.base_font_id.size * 0.42;
        let cx = ctx.rect.left() + 9.0;
        let cy = line_top_y + row_height * 0.5;
        let points = if ch.collapsed {
            // ▶ — points right
            vec![
                Pos2::new(cx - size * 0.5, cy - size * 0.6),
                Pos2::new(cx - size * 0.5, cy + size * 0.6),
                Pos2::new(cx + size * 0.6, cy),
            ]
        } else {
            // ▼ — points down
            vec![
                Pos2::new(cx - size * 0.6, cy - size * 0.4),
                Pos2::new(cx + size * 0.6, cy - size * 0.4),
                Pos2::new(cx, cy + size * 0.6),
            ]
        };
        ctx.painter.add(egui::Shape::convex_polygon(
            points,
            ctx.gutter_color,
            Stroke::NONE,
        ));
        view.click_zones.push(ClickZone {
            rect: ClickRect {
                x_min: 0.0,
                y_min: line_top_y - ctx.rect.min.y,
                x_max: 18.0,
                y_max: line_top_y - ctx.rect.min.y + row_height,
            },
            action: ClickAction::ToggleFold(ch.id),
        });
    }
}

fn paint_segments(
    ctx: &PaintCtx<'_>,
    layout: &LineLayout,
    measured: &LineMeasured,
    line_top_y: f32,
    row_height: f32,
    click_zones: &mut Vec<ClickZone>,
) {
    for (idx, seg) in layout.segments.iter().enumerate() {
        let g = &measured.galleys[idx];
        let seg_x = ctx.text_origin_x + measured.x_starts[idx];
        let seg_w = measured.seg_widths[idx];
        let seg_y = line_top_y + (row_height - g.size().y) * 0.5;
        let fg = seg.style.fg.map(to_egui_color).unwrap_or(ctx.text_color);

        if let Some(widget) = &seg.widget {
            paint_inline_widget_placeholder(
                ctx, widget, seg_x, line_top_y, seg_w, row_height,
                g, seg_y, click_zones,
            );
            continue;
        }

        if let Some(bg) = seg.style.bg {
            let bg_rect = Rect::from_min_max(
                Pos2::new(seg_x, line_top_y),
                Pos2::new(seg_x + seg_w, line_top_y + row_height),
            );
            ctx.painter.rect_filled(bg_rect, 0.0, to_egui_color(bg));
        }
        ctx.painter.galley(Pos2::new(seg_x, seg_y), g.clone(), fg);
        if seg.style.bold {
            ctx.painter.galley(Pos2::new(seg_x + 0.5, seg_y), g.clone(), fg);
        }
        if seg.style.underline {
            let y = seg_y + g.size().y * 0.92;
            ctx.painter.line_segment(
                [Pos2::new(seg_x, y), Pos2::new(seg_x + seg_w, y)],
                Stroke::new(1.0, fg),
            );
        }
        if seg.style.strikethrough {
            let y = seg_y + g.size().y * 0.5;
            ctx.painter.line_segment(
                [Pos2::new(seg_x, y), Pos2::new(seg_x + seg_w, y)],
                Stroke::new(1.0, fg),
            );
        }
    }
}

/// v1 placeholder render for an inline widget decoration: a styled rect of
/// the widget's measured size plus a tiny "widget" label. Real per-widget
/// painting is deferred to a future trait method.
#[allow(clippy::too_many_arguments)]
fn paint_inline_widget_placeholder(
    ctx: &PaintCtx<'_>,
    widget: &Arc<dyn InlineWidget>,
    seg_x: f32,
    line_top_y: f32,
    seg_w: f32,
    row_height: f32,
    label_galley: &Arc<egui::Galley>,
    label_y: f32,
    click_zones: &mut Vec<ClickZone>,
) {
    let visuals = ctx.ui.style().visuals.clone();
    let bg = if visuals.dark_mode {
        Color32::from_rgba_unmultiplied(70, 80, 110, 80)
    } else {
        Color32::from_rgba_unmultiplied(210, 220, 240, 220)
    };
    let border = visuals.weak_text_color().gamma_multiply(0.5);
    let rect = Rect::from_min_max(
        Pos2::new(seg_x, line_top_y),
        Pos2::new(seg_x + seg_w, line_top_y + row_height),
    );
    ctx.painter.rect_filled(rect, 3.0, bg);
    ctx.painter
        .rect_stroke(rect, 3.0, Stroke::new(0.5, border), egui::StrokeKind::Inside);
    ctx.painter
        .galley(Pos2::new(seg_x + 2.0, label_y), label_galley.clone(), border);

    if widget.handles_click() {
        click_zones.push(ClickZone {
            rect: ClickRect {
                x_min: seg_x - ctx.rect.min.x,
                y_min: line_top_y - ctx.rect.min.y,
                x_max: seg_x + seg_w - ctx.rect.min.x,
                y_max: line_top_y + row_height - ctx.rect.min.y,
            },
            action: ClickAction::WidgetClick(widget.widget_id()),
        });
    }
}

fn paint_cursors(
    ctx: &PaintCtx<'_>,
    state: &EditorState,
    line_byte_start: usize,
    line_byte_end: usize,
    line_top_y: f32,
    row_height: f32,
    measured: &LineMeasured,
) {
    for r in state.selection.ranges() {
        let head = r.head.offset();
        if head < line_byte_start || head > line_byte_end {
            continue;
        }
        let local = head - line_byte_start;
        let x = measured.x_at_buffer_offset(local);
        let cursor_rect = Rect::from_min_max(
            Pos2::new(ctx.text_origin_x + x - 0.5, line_top_y),
            Pos2::new(ctx.text_origin_x + x + 1.0, line_top_y + row_height),
        );
        let color = if ctx.has_focus { ctx.cursor_color } else { ctx.gutter_color };
        ctx.painter.rect_filled(cursor_rect, 0.0, color);
    }
}

/// A single visual line built from buffer text + overlapping decorations.
#[derive(Clone)]
struct LineLayout {
    segments: Vec<Segment>,
    base_font_size: f32,
    base_color: Color32,
}

#[derive(Clone)]
struct Segment {
    display: SmolStr,
    buffer_range: std::ops::Range<usize>,
    style: MarkStyle,
    is_replacement: bool,
    /// When present, this segment renders an inline widget placeholder of the
    /// widget's measured size rather than text. v1 limitation: the egui
    /// adapter does not call into the widget for painting; instead it draws a
    /// styled rect with a small "widget" label.
    widget: Option<Arc<dyn InlineWidget>>,
}

/// Per-segment measured galleys + x positions for one frame.
#[derive(Clone)]
struct LineMeasured {
    galleys: Vec<Arc<egui::Galley>>,
    x_starts: Vec<f32>,
    /// Width used to advance for this segment; equals galley width for text,
    /// or the widget's measured width for inline-widget segments.
    seg_widths: Vec<f32>,
    total_width: f32,
    /// Mirrors `LineLayout::segments[i].buffer_range.start - line_start`.
    seg_buffer_starts: Vec<usize>,
    seg_buffer_ends: Vec<usize>,
    seg_is_replacement: Vec<bool>,
}

impl LineMeasured {
    fn x_at_buffer_offset(&self, line_local_byte: usize) -> f32 {
        for (i, &start) in self.seg_buffer_starts.iter().enumerate() {
            let end = self.seg_buffer_ends[i];
            if line_local_byte < start {
                return self.x_starts[i];
            }
            if line_local_byte <= end {
                let seg_x = self.x_starts[i];
                if self.seg_is_replacement[i] {
                    if line_local_byte == end {
                        return seg_x + self.seg_widths[i];
                    }
                    return seg_x;
                }
                // Walk display chars to find x within the galley.
                let g = &self.galleys[i];
                let display = g.text();
                let local_in_seg = line_local_byte - start;
                let safe = local_in_seg.min(display.len());
                let char_idx = display[..safe].chars().count();
                let ccursor = egui::text::CCursor::new(char_idx);
                return seg_x + g.pos_from_cursor(ccursor).min.x;
            }
        }
        self.total_width
    }
}

impl LineLayout {
    fn measure(&self, ui: &egui::Ui) -> LineMeasured {
        let mut galleys = Vec::with_capacity(self.segments.len());
        let mut x_starts = Vec::with_capacity(self.segments.len());
        let mut seg_widths = Vec::with_capacity(self.segments.len());
        let mut seg_buffer_starts = Vec::with_capacity(self.segments.len());
        let mut seg_buffer_ends = Vec::with_capacity(self.segments.len());
        let mut seg_is_replacement = Vec::with_capacity(self.segments.len());
        let base = self.line_base();
        let mut x = 0.0f32;
        for seg in &self.segments {
            let g = segment_galley(ui, seg, self.base_font_size, self.base_color);
            let w = if let Some(widget) = &seg.widget {
                widget.measure(self.base_font_size).0.max(g.size().x)
            } else {
                g.size().x
            };
            x_starts.push(x);
            seg_widths.push(w);
            x += w;
            galleys.push(g);
            seg_buffer_starts.push(seg.buffer_range.start - base);
            seg_buffer_ends.push(seg.buffer_range.end - base);
            seg_is_replacement.push(seg.is_replacement);
        }
        LineMeasured {
            galleys,
            x_starts,
            seg_widths,
            total_width: x,
            seg_buffer_starts,
            seg_buffer_ends,
            seg_is_replacement,
        }
    }

    fn line_base(&self) -> usize {
        self.segments.first().map(|s| s.buffer_range.start).unwrap_or(0)
    }
}

fn segment_galley(
    ui: &egui::Ui,
    seg: &Segment,
    base_size: f32,
    base_color: Color32,
) -> Arc<egui::Galley> {
    let display = if seg.widget.is_some() {
        // Tiny label rendered inside the placeholder rect. The advance width
        // for the segment uses the widget's `measure()` result, not the label.
        "widget"
    } else if seg.display.is_empty() && seg.is_replacement {
        ""
    } else if seg.display.is_empty() {
        " "
    } else {
        seg.display.as_str()
    };
    let format = format_for(&seg.style, base_size, base_color);
    let mut job = LayoutJob::single_section(display.to_string(), format);
    job.wrap.max_width = f32::INFINITY;
    ui.fonts(|f| f.layout_job(job))
}

fn build_line_layout(
    line_text: &str,
    line_byte_start: usize,
    layers: &[editor_core::DecorationSet],
    base_font_size: f32,
    base_color: Color32,
) -> LineLayout {
    let line_byte_end = line_byte_start + line_text.len();
    let mut events: Vec<DecoEvent> = Vec::new();
    for layer in layers {
        for (range, deco) in layer.iter_overlapping(line_byte_start..line_byte_end + 1) {
            let clipped = range.start.max(line_byte_start)..range.end.min(line_byte_end);
            if clipped.start >= clipped.end {
                continue;
            }
            match deco {
                Decoration::Mark(style) => events.push(DecoEvent::Mark(clipped, style.clone())),
                Decoration::Replace { display } => {
                    events.push(DecoEvent::Replace(clipped, display.clone()))
                }
                Decoration::InlineWidget { widget, .. } => {
                    events.push(DecoEvent::Widget(clipped, widget.clone()))
                }
                Decoration::Line(_) | Decoration::Block(_) | Decoration::BlockWidget { .. } => {}
            }
        }
    }

    // Snap a line-local byte index to the nearest valid char boundary at or
    // before it. Decoration ranges occasionally land mid-codepoint when they
    // outlive a buffer edit (the markdown parse is async) — slicing on those
    // raw indices panics on multi-byte chars like em-dash.
    let snap = |mut b: usize| -> usize {
        if b > line_text.len() {
            b = line_text.len();
        }
        while b > 0 && !line_text.is_char_boundary(b) {
            b -= 1;
        }
        b
    };

    let mut boundaries: Vec<usize> = vec![0, line_text.len()];
    for ev in &events {
        match ev {
            DecoEvent::Mark(r, _)
            | DecoEvent::Replace(r, _)
            | DecoEvent::Widget(r, _) => {
                boundaries.push(snap(r.start.saturating_sub(line_byte_start)));
                boundaries.push(snap(r.end.saturating_sub(line_byte_start)));
            }
        }
    }
    boundaries.sort();
    boundaries.dedup();

    let style_at = |start: usize,
                    end: usize|
     -> (
        MarkStyle,
        Option<Option<SmolStr>>,
        Option<Arc<dyn InlineWidget>>,
    ) {
        let abs_start = line_byte_start + start;
        let abs_end = line_byte_start + end;
        let mut merged = MarkStyle::default();
        let mut replacement: Option<Option<SmolStr>> = None;
        let mut widget: Option<Arc<dyn InlineWidget>> = None;
        for ev in &events {
            match ev {
                DecoEvent::Mark(r, s) if r.start <= abs_start && r.end >= abs_end => {
                    merge_mark(&mut merged, s);
                }
                DecoEvent::Replace(r, disp) if r.start <= abs_start && r.end >= abs_end => {
                    replacement = Some(disp.clone());
                }
                DecoEvent::Widget(r, w) if r.start <= abs_start && r.end >= abs_end => {
                    widget = Some(w.clone());
                }
                _ => {}
            }
        }
        (merged, replacement, widget)
    };

    // Collect line-local Replace AND Widget ranges; each becomes ONE
    // consolidated segment so an interior Mark doesn't subdivide and duplicate
    // either the Replace display or the widget placeholder.
    enum Atomic {
        Replace(Option<SmolStr>),
        Widget(Arc<dyn InlineWidget>),
    }
    let mut atomic_ranges: Vec<(usize, usize, Atomic)> = Vec::new();
    for ev in &events {
        match ev {
            DecoEvent::Replace(r, disp) => atomic_ranges.push((
                snap(r.start.saturating_sub(line_byte_start)),
                snap(r.end.saturating_sub(line_byte_start)),
                Atomic::Replace(disp.clone()),
            )),
            DecoEvent::Widget(r, w) => atomic_ranges.push((
                snap(r.start.saturating_sub(line_byte_start)),
                snap(r.end.saturating_sub(line_byte_start)),
                Atomic::Widget(w.clone()),
            )),
            DecoEvent::Mark(_, _) => {}
        }
    }
    atomic_ranges.sort_by_key(|(s, _, _)| *s);

    // Marks-overlapping-range helper: union of all Mark styles whose range
    // intersects [s, e). Used for both Replace consolidated segments and
    // normal text segments.
    let marks_for = |s: usize, e: usize| -> MarkStyle {
        let abs_s = line_byte_start + s;
        let abs_e = line_byte_start + e;
        let mut merged = MarkStyle::default();
        for ev in &events {
            if let DecoEvent::Mark(r, m) = ev {
                if r.end > abs_s && r.start < abs_e {
                    merge_mark(&mut merged, m);
                }
            }
        }
        merged
    };

    let mut segments = Vec::with_capacity(boundaries.len());
    let mut cursor: usize = 0;
    let line_len = line_text.len();

    while cursor < line_len {
        // 1. If cursor is inside an atomic (Replace or Widget) range, emit ONE
        //    consolidated segment.
        if let Some(idx) = atomic_ranges.iter().position(|(s, e, _)| cursor >= *s && cursor < *e)
        {
            let (rs, re, ref atom) = atomic_ranges[idx];
            let style = marks_for(rs, re);
            match atom {
                Atomic::Replace(disp) => segments.push(Segment {
                    display: disp.clone().unwrap_or_default(),
                    buffer_range: (line_byte_start + rs)..(line_byte_start + re),
                    style,
                    is_replacement: true,
                    widget: None,
                }),
                Atomic::Widget(w) => segments.push(Segment {
                    display: SmolStr::default(),
                    buffer_range: (line_byte_start + rs)..(line_byte_start + re),
                    style,
                    is_replacement: true,
                    widget: Some(w.clone()),
                }),
            }
            cursor = re;
            continue;
        }

        // 2. Find the next break: either the next atomic-range start, the next
        //    Mark boundary, or end of line.
        let mut seg_end = line_len;
        for (rs, _, _) in &atomic_ranges {
            if *rs > cursor && *rs < seg_end {
                seg_end = *rs;
            }
        }
        for b in &boundaries {
            if *b > cursor && *b < seg_end {
                seg_end = *b;
            }
        }
        if seg_end <= cursor {
            cursor += 1;
            continue;
        }
        let (style, _, _) = style_at(cursor, seg_end);
        let slice = &line_text[cursor..seg_end];
        segments.push(Segment {
            display: SmolStr::from(slice),
            buffer_range: (line_byte_start + cursor)..(line_byte_start + seg_end),
            style,
            is_replacement: false,
            widget: None,
        });
        cursor = seg_end;
    }
    if segments.is_empty() {
        segments.push(Segment {
            display: SmolStr::default(),
            buffer_range: line_byte_start..line_byte_start,
            style: MarkStyle::default(),
            is_replacement: false,
            widget: None,
        });
    }
    LineLayout { segments, base_font_size, base_color }
}

enum DecoEvent {
    Mark(std::ops::Range<usize>, MarkStyle),
    Replace(std::ops::Range<usize>, Option<SmolStr>),
    Widget(std::ops::Range<usize>, Arc<dyn InlineWidget>),
}

fn merge_mark(dst: &mut MarkStyle, src: &MarkStyle) {
    if src.bold { dst.bold = true; }
    if src.italic { dst.italic = true; }
    if src.strikethrough { dst.strikethrough = true; }
    if src.underline { dst.underline = true; }
    if src.monospace { dst.monospace = true; }
    if src.fg.is_some() { dst.fg = src.fg; }
    if src.bg.is_some() { dst.bg = src.bg; }
    if src.font_scale.is_some() { dst.font_scale = src.font_scale; }
}

fn format_for(style: &MarkStyle, base_size: f32, base_color: Color32) -> TextFormat {
    let size = base_size * style.font_scale.unwrap_or(1.0);
    // `style.monospace` is the *signal* that this run is code-shaped.
    // Both branches resolve to `Monospace` for now because the wrap
    // calculator below uses monospace `char_width` and mixing
    // proportional runs into a monospace wrap budget produces visible
    // misalignment. Custom font families (per `editor.font_*` settings)
    // are loaded at startup via `egui::Context::set_fonts` and routed
    // through here once that lands.
    // Both branches resolve to Monospace for now (see comment above).
    let _ = style.monospace;
    let family = FontFamily::Monospace;
    let fg = style.fg.map(to_egui_color).unwrap_or(base_color);
    TextFormat {
        font_id: FontId::new(size, family),
        color: fg,
        italics: style.italic,
        // We draw bg/underline/strike manually in the painter so they pick up
        // the segment's measured width, not the glyph rect.
        ..Default::default()
    }
}

fn collect_fold_chevron(
    layers: &[editor_core::DecorationSet],
    line_start: usize,
    line_end: usize,
) -> Option<editor_core::FoldChevron> {
    for layer in layers {
        for (range, deco) in layer.iter_overlapping(line_start..line_end + 1) {
            if let Decoration::Line(ls) = deco {
                if range.start == line_start {
                    if let Some(ch) = ls.fold_chevron {
                        return Some(ch);
                    }
                }
            }
        }
    }
    None
}

fn to_egui_color(c: Color) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r, c.g, c.b, c.a)
}

trait HeightMapExt {
    fn row_height(&self, line: usize) -> f32;
}
impl HeightMapExt for ViewState {
    fn row_height(&self, line: usize) -> f32 {
        self.height_map.text_height(line).max(self.line_height)
    }
}
