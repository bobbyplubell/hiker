//! `Widget`: drop-in egui widget for an EditorState + ViewState pair.

use std::sync::Arc;

use editor_core::decoration::BlockDeco;

use editor_core::decoration::BlockSide;

use editor_core::decoration::Color;

use editor_core::decoration::Decoration;

use editor_core::state::Editor as EditorState;
use editor_core::transaction::Transaction;
use editor_core::decoration::InlineWidget;

use editor_core::decoration::LineStyle;

use editor_view::command;
use editor_view::command::Action;
use editor_view::viewport::DragState;
use editor_view::viewport::MeasureCache;
use editor_view::viewport::ClickAction;
use editor_view::viewport::ClickRect;
use editor_view::viewport::ClickZone;
use editor_view::events::InputEvent;
use editor_view::viewport::ViewState;
use editor_view::wrapping::VisualSpan;
mod blocks;
pub(crate) mod layout;
mod search_panel;
use blocks::{BlockPaint, BlockZone};
use layout::{LineLayout, LineLayoutBuilder, LineMeasured};

/// Per-frame snapshot of the inputs that feed the measure pass. Compared
/// against `ViewState::measure_cache` to decide whether geometry needs to be
/// rebuilt.
#[derive(Clone, Copy)]
struct ViewUpdate {
    /// `state.doc.content_id()` before this frame's input handling. When it
    /// differs from `doc_id`, an edit landed this frame and the host's
    /// decoration layers (built pre-edit) are stale — see
    /// `Widget::decorations_stale`.
    pre_doc_id: u64,
    doc_id: u64,
    metrics: u64,
    height_decos: u64,
}

/// Per-(buffer-line, vline-index) cached layout result. Lives across frames;
/// owned by the host via `PaintCache` and passed into the widget through
/// `Widget::with_paint_cache`. Invalidated per-entry when any of
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
/// `&mut` into the widget via `Widget::with_paint_cache`. When no
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
use egui::{Color32, FontFamily, FontId, Pos2, Rect, Sense, Stroke};

pub struct Widget<'a> {
    pub state: &'a mut EditorState,
    pub view: &'a mut ViewState,
    pub clicks_out: Option<&'a mut Vec<ClickAction>>,
    /// Optional sink that receives, in application order, the change set
    /// (transaction) behind every doc-mutating edit the widget applied from
    /// user input this frame — the forward half of the editor binding. The
    /// host drains it after `show` returns to mirror those edits into a
    /// higher layer (e.g. a CRDT working layer). Selection-only edits, scroll,
    /// clicks, and no-op (identity) transactions push nothing. When `None`,
    /// transactions are simply not collected and behavior is unchanged.
    pub transactions_out: Option<&'a mut Vec<Transaction>>,
    /// Optional host-owned paint cache. When `None`, `show` falls back to
    /// a transient cache that lives only for the duration of the call —
    /// fine for one-shot renders (tests, previews) but loses cross-frame
    /// galley reuse. Persistent hosts (the main buffer panel) should pass
    /// a `&mut PaintCache` via `with_paint_cache`.
    pub paint_cache: Option<&'a mut PaintCache>,
    /// Optional host hook, run *after* this frame's input is applied to the
    /// doc but *before* heights are measured and the body is painted. The host
    /// uses it to rebuild its decoration layers against the post-edit doc, so
    /// the keystroke that landed this frame is reflected in the decorations the
    /// same frame. Without it, the host's layers describe the pre-edit doc on
    /// every edit frame, so marker-hiding / block decorations are offset by the
    /// just-typed text for one frame — live preview visibly drops out and
    /// blocks blink. When this hook is set, the widget treats decorations as
    /// current (no stale-frame height deferral).
    pub decoration_rebuild: Option<DecorationRebuild<'a>>,
}

/// Host hook invoked between input and paint to rebuild decoration layers
/// against the post-edit doc — see [`Widget::decoration_rebuild`].
pub type DecorationRebuild<'a> = &'a mut dyn FnMut(&EditorState, &mut ViewState);

impl<'a> Widget<'a> {
    pub const fn new(state: &'a mut EditorState, view: &'a mut ViewState) -> Self {
        Self {
            state,
            view,
            clicks_out: None,
            transactions_out: None,
            paint_cache: None,
            decoration_rebuild: None,
        }
    }

    /// Set the post-input decoration-rebuild hook (see
    /// [`Widget::decoration_rebuild`]).
    pub fn with_decoration_rebuild(
        mut self,
        rebuild: DecorationRebuild<'a>,
    ) -> Self {
        self.decoration_rebuild = Some(rebuild);
        self
    }

    /// Configure a sink that receives any `ClickAction`s the widget produced
    /// this frame (e.g. an Expander toggle). The host pops them after `show`
    /// returns and applies them to its fold / region state.
    pub const fn with_click_sink(mut self, sink: &'a mut Vec<ClickAction>) -> Self {
        self.clicks_out = Some(sink);
        self
    }

    /// Configure a sink that receives the change set (transaction) behind
    /// every doc-mutating edit the widget applied from user input this frame,
    /// in application order. The host drains them after `show` returns to
    /// mirror the edits into a higher layer. Mirrors `with_click_sink`.
    pub const fn with_transactions_sink(mut self, sink: &'a mut Vec<Transaction>) -> Self {
        self.transactions_out = Some(sink);
        self
    }

    /// Plug in a host-owned `PaintCache` so per-line galleys survive
    /// across frames. Without this the widget reuses nothing — every
    /// paint rebuilds every line layout.
    pub const fn with_paint_cache(mut self, cache: &'a mut PaintCache) -> Self {
        self.paint_cache = Some(cache);
        self
    }

    pub fn show(mut self, ui: &mut egui::Ui) -> egui::Response {
        // Phase 0: layout — claim screen space and compute the text rect.
        let desired = ui.available_size();
        let (rect, response) = ui.allocate_exact_size(desired, Sense::click_and_drag());
        self.sync_search_panel();
        let (top_h, bottom_h) = self.view.panels.heights();
        let text_rect = Rect::from_min_max(
            Pos2::new(rect.min.x, rect.min.y + top_h),
            Pos2::new(rect.max.x, rect.max.y - bottom_h),
        );

        // Phase 1: update — pull frame inputs onto ViewState (size, fonts),
        // run input events, and capture what changed.
        let mut update = self.update(ui, &response, rect, text_rect);

        // Phase 1.5: let the host rebuild its decoration layers against the
        // doc as it stands AFTER this frame's input. This is what keeps live
        // preview from dropping out for a frame on each keystroke: the layers
        // now describe the post-edit doc, so marker-hiding / block decorations
        // line up with the text being painted. Re-read the height fingerprint
        // afterward (the rebuild changed the layers) and mark decorations
        // fresh so `measure` derives heights this frame instead of deferring.
        let decorations_refreshed = if let Some(rebuild) = self.decoration_rebuild.take() {
            rebuild(self.state, self.view);
            update.height_decos = self.view.decorations.height_signature;
            true
        } else {
            false
        };

        // Phase 2: measure — heightmap + wrap recompute. Skipped entirely
        // when no input that affects geometry has changed. Returns true when
        // it had to defer the height re-derivation because the host's
        // decoration layers were built against the pre-edit doc (an edit
        // landed inside this frame's input handling); in that case we force an
        // immediate repaint so the host rebuilds decorations against the new
        // doc and the geometry settles on the very next frame rather than
        // waiting on the idle 500 ms repaint (which would read as a flicker).
        let needs_relayout = self.measure(update, decorations_refreshed);

        // An edit this frame asked the caret be scrolled back into view. Apply
        // it now that the height map reflects the post-edit doc — but only when
        // measure didn't defer (`needs_relayout`); on a deferred frame the
        // geometry is still stale, so keep the flag and let the forced repaint
        // below settle it next frame.
        if self.view.scroll_caret_into_view && !needs_relayout {
            command::scroll_caret_into_view(self.state, self.view);
            self.view.scroll_caret_into_view = false;
        }

        // Phase 3: paint — always runs, but reads cached geometry built in
        // measure.
        self.view.click_zones.clear();
        let has_focus = response.has_focus();
        // Fall back to a transient cache when the host didn't provide
        // one. Persistent panels (the main buffer panel) supply a
        // `&mut PaintCache` via `with_paint_cache`; tests/previews and
        // legacy call sites get a fresh cache per frame.
        let mut fallback_cache = PaintCache::default();
        // Temporarily detach the host cache so we can call `self.paint(...)`
        // without holding two mutable borrows of `self` (the cache field
        // and the method receiver).
        let mut host_cache = self.paint_cache.take();
        let cache: &mut PaintCache = match host_cache.as_deref_mut() {
            Some(c) => c,
            None => &mut fallback_cache,
        };
        self.paint(ui, cache, text_rect, has_focus);
        self.paint_cache = host_cache;
        if needs_relayout {
            ui.ctx().request_repaint();
        }
        crate::tooltip::paint_tooltips(ui, self.view, self.state, text_rect);
        crate::completion::paint_completion_popup(ui, self.view, self.state, text_rect);
        self.refresh_search_matches();
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
        let text_area_w = (rect.width() - self.view.content_origin_x()).max(0.0);
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
        let mods = ui.input(|i| editor_view::events::Modifiers {
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
        // Wheel scroll is gated on hover ALONE, never on focus. egui's
        // `smooth_scroll_delta` is a single global per-frame value, not a
        // per-widget signal — so if a non-hovered editor consumed it too,
        // every visible editor group would scroll in lockstep when the user
        // spins the wheel over just one of them. With split editor panes,
        // exactly one pane is hovered but a different pane may still hold
        // keyboard focus; gating on `has_focus()` made that focused-but-not-
        // hovered pane scroll alongside the hovered one. Keyboard scrolling
        // (PageUp/PageDown) flows through the focus-gated key path above, not
        // through `smooth_scroll_delta`, so dropping focus here loses nothing.
        if response.hovered() {
            let scrolled = ui.input(|i| i.smooth_scroll_delta.y);
            if scrolled.abs() > 0.0 {
                let speed = if self.view.scroll_speed > 0.0 {
                    self.view.scroll_speed
                } else {
                    1.0
                };
                let action = command::handle(
                    self.state,
                    self.view,
                    &InputEvent::Scroll { delta_x: 0.0, delta_y: scrolled * speed },
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
    ///
    /// Returns `true` when it deferred the height re-derivation because the
    /// host's decoration layers are stale — see [`Self::decorations_stale`].
    fn measure(&mut self, update: ViewUpdate, decorations_refreshed: bool) -> bool {
        let cache = self.view.measure_cache;
        let metrics_changed = cache.metrics != update.metrics;
        let doc_changed = cache.doc_id != update.doc_id;
        let decos_changed = cache.height_decos != update.height_decos;

        // The viewport band is line-quantized; recompute it from the current
        // scroll AFTER any input that may have moved scroll_y.
        let mut viewport = self.view.visible_lines();
        let viewport_changed = (viewport.start, viewport.end) != cache.viewport;

        if !(metrics_changed || doc_changed || decos_changed || viewport_changed) {
            return false;
        }

        // The host rebuilds its decoration layers ONCE per frame, before the
        // widget runs — i.e. against the doc as it stood at the END of the
        // previous frame. But an edit keystroke is applied INSIDE this frame's
        // input handling (Phase 1), so on an edit frame the height-affecting
        // layers describe the pre-edit doc while `state.doc` is already the
        // post-edit doc. Their byte ranges no longer line up: feeding them to
        // `apply_line_height_decorations` would map (say) a heading's stale
        // byte_start to the wrong line in the new doc — applying the heading's
        // tall row to a line above its real position and collapsing the real
        // heading line. That mismatch is the per-keystroke reflow of lines
        // below the cursor. Detect it via `pre_doc_id`: when an edit landed
        // this frame, keep the previous frame's (consistent) height overrides
        // and force a repaint so the host re-derives decorations against the
        // new doc next frame.
        // When the host rebuilt decorations against the post-edit doc this
        // frame (the `decoration_rebuild` hook), the layers are current even
        // though an edit landed — so there's nothing to defer.
        let stale = !decorations_refreshed && self.decorations_stale(update);

        // Wrap geometry depends only on line text, width, and per-line font
        // scale — all of which are current even on an edit frame. Keep it in
        // sync EVERY frame, before the stale-path early return below, so paint
        // (Phase 3, which always runs) never slices the post-edit line text
        // with a pre-edit vline range. `get_or_compute` short-circuits in O(1)
        // per line on a matching text hash, so this is cheap; it also recomputes
        // lines whose cached entry shifted onto the wrong index after a mid-doc
        // insertion (`ensure_capacity`'s resize only appends at the end). Width/
        // char-width/wrap-enabled changes are invalidated eagerly by `set_width`
        // / `set_char_width` / `set_enabled`.
        //
        // Skipping this on the stale path used to leave the wrap cache holding
        // vline ranges computed against the longer pre-edit text; pressing Enter
        // in a list (which empties/shifts a visible line) then sliced the now-
        // empty line with the old range and panicked.
        self.view.wrap_map.ensure_capacity(self.state.doc.len_lines());
        self.prewrap_visible();

        if stale {
            // Heights derive from the host's decoration layers, which still
            // describe the pre-edit doc this frame. Don't wipe overrides via a
            // line-count `sync_to` (which clears them on any line-count change),
            // and don't apply the stale ranges. Keep last frame's heights and
            // just reconcile the line count non-destructively so reads stay
            // in-bounds.
            self.view
                .height_map
                .set_line_count(self.state.doc.len_lines(), self.view.line_height);
            // Deliberately leave `measure_cache` unchanged so the next frame
            // (fresh decorations) still sees doc_id / height_decos as changed
            // and performs the real re-derivation.
            return true;
        }

        // Decorations are current: derive heights from them.
        self.view.sync_to(self.state);
        self.apply_line_height_decorations();

        // Viewport may have shifted as a result of the geometry rebuild
        // (heights changing under us). Re-read for the cache snapshot.
        viewport = self.view.visible_lines();
        self.view.measure_cache = MeasureCache {
            doc_id: update.doc_id,
            height_decos: update.height_decos,
            metrics: update.metrics,
            viewport: (viewport.start, viewport.end),
        };
        false
    }

    /// True when an edit was applied during this frame's input handling, so
    /// the host's decoration layers (built before the widget ran, against the
    /// pre-edit doc) no longer align with `state.doc`. Height re-derivation
    /// from these layers would mis-place per-line heights for one frame.
    const fn decorations_stale(&self, update: ViewUpdate) -> bool {
        update.pre_doc_id != update.doc_id
    }

    fn consume_action(&mut self, ui: &egui::Ui, action: Action) {
        match action {
            Action::Replace { state, tx } => {
                // Forward the applied change set to the host's sink before
                // swapping in the new state — but only when the edit actually
                // changed the doc. Selection-only / history-navigation replaces
                // carry `tx: None`, and a tx whose change set is the identity
                // (all-retain) is a no-op the host shouldn't mirror.
                if let Some(sink) = self.transactions_out.as_deref_mut() {
                    if let Some(tx) = tx {
                        if !tx.changes.is_identity() {
                            sink.push(tx);
                        }
                    }
                }
                *self.state = state;
            }
            Action::Copy(t) => ui.ctx().copy_text(t),
            Action::Cut { text, state, tx } => {
                ui.ctx().copy_text(text);
                // Mirror the deletion to the host sink (same as `Replace`) so a
                // CRDT `working` layer sees the cut; otherwise the reverse pass
                // reverts it. Skip an identity change set (empty selection +
                // empty line is conceivable).
                if let Some(sink) = self.transactions_out.as_deref_mut() {
                    if !tx.changes.is_identity() {
                        sink.push(tx);
                    }
                }
                *self.state = state;
            }
            Action::Click(c) => {
                if let Some(sink) = self.clicks_out.as_deref_mut() {
                    sink.push(c);
                }
            }
            Action::None => {}
        }
    }
}

impl<'a> Widget<'a> {
fn apply_line_height_decorations(&mut self) {
    let view = &mut *self.view;
    let state = &*self.state;
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
}

/// Geometry + byte extent of one painted row (a buffer line, or one
/// visual row of a wrapped line). Threaded through the per-row paint
/// helpers so they share a single cohesive descriptor instead of four
/// loose positional args. `Copy` so it passes by value cheaply.
#[derive(Clone, Copy)]
struct RowSpan {
    line_idx: usize,
    byte_start: usize,
    byte_end: usize,
    top_y: f32,
    height: f32,
}

/// Geometry of one inline segment's placeholder box: its horizontal
/// extent (`x`, `width`), the row band it sits in (`top_y`, `height`),
/// and the baseline `label_y` for any text drawn inside. `Copy`.
#[derive(Clone, Copy)]
struct SegSpan {
    x: f32,
    width: f32,
    top_y: f32,
    height: f32,
    label_y: f32,
}

struct PaintCtx<'a> {
    ui: &'a mut egui::Ui,
    state: &'a EditorState,
    view: &'a mut ViewState,
    cache: &'a mut PaintCache,
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

impl<'a> Widget<'a> {
fn paint(
    &mut self,
    ui: &mut egui::Ui,
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
    if self.state.doc.is_empty() {
        if let Some(placeholder) = self.view.placeholder.clone() {
            let text_origin_x = rect.left() + self.view.content_origin_x();
            let font_id = FontId::new(self.view.font_size, FontFamily::Monospace);
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
    let text_origin_x = rect.left() + self.view.content_origin_x();
    let base_font_id = FontId::new(self.view.font_size, FontFamily::Monospace);
    let selection_color = {
        let s = visuals.selection.bg_fill;
        Color32::from_rgba_unmultiplied(s.r(), s.g(), s.b(), 110)
    };
    let text_color = visuals.text_color();
    let cursor_color = visuals.text_color();
    let gutter_color = visuals.weak_text_color();
    let mut ctx = PaintCtx {
        ui,
        state: &*self.state,
        view: &mut *self.view,
        cache,
        painter,
        rect,
        text_origin_x,
        base_font_id,
        // egui's `visuals.selection.bg_fill` is opaque and tuned for
        // filling button shapes — used directly here it would obscure
        // the glyphs underneath. We want a translucent tint instead.
        // `linear_multiply(0.5)` was the previous attempt, but Color32
        // is premultiplied, so multiplying scales the alpha too —
        // dropping it to ~50% of an already-dark color on a dark
        // background made the selection visually disappear. Build a
        // fixed-alpha overlay from the theme accent instead.
        text_color,
        selection_color,
        cursor_color,
        gutter_color,
        hatched_default,
        has_focus,
    };

    ctx.paint_lines();

    if !ctx.view.hide_gutter {
        let sep_x = rect.left() + ctx.view.gutter_width - 2.0;
        ctx.painter.line_segment(
            [Pos2::new(sep_x, rect.top()), Pos2::new(sep_x, rect.bottom())],
            Stroke::new(1.0, visuals.weak_text_color().gamma_multiply(0.3)),
        );
    }
    if has_focus {
        // Schedule a repaint precisely when the next blink transition occurs,
        // so the caret animates without burning CPU on per-frame repaints.
        // Half-period = 530 ms, full cycle = 1060 ms.
        const HALF_PERIOD_MS: u64 = 530;
        const FULL_CYCLE_MS: u64 = HALF_PERIOD_MS * 2;
        let elapsed_ms = ctx.view.last_interaction.elapsed().as_millis() as u64;
        let until_next_ms = if elapsed_ms < HALF_PERIOD_MS {
            // Still in the initial solid phase; schedule at the end of it.
            HALF_PERIOD_MS - elapsed_ms
        } else {
            // In the repeating cycle: compute remaining time in current phase.
            let phase = elapsed_ms % FULL_CYCLE_MS;
            if phase < HALF_PERIOD_MS {
                HALF_PERIOD_MS - phase
            } else {
                FULL_CYCLE_MS - phase
            }
        };
        // Clamp to at least 1 ms to avoid scheduling an immediate repaint.
        let until_next_ms = until_next_ms.max(1);
        ctx.ui.ctx().request_repaint_after(std::time::Duration::from_millis(until_next_ms));
    }
}
}

impl<'a> PaintCtx<'a> {
    fn paint_lines(&mut self) {
        for line_idx in self.view.visible_lines() {
            if line_idx >= self.state.doc.len_lines() {
                break;
            }
            self.paint_visible_line(line_idx);
        }
        if let DragState::DraggingSelection { drop_caret } = self.view.drag {
            self.paint_drop_caret(drop_caret);
        }
    }

    /// Paint a thin vertical "drop indicator" caret at `drop_caret` (byte
    /// offset). Used during text drag-and-drop to show where a release would
    /// insert the dragged text. SPEC §9.19.
    fn paint_drop_caret(&mut self, drop_caret: usize) {
        if self.state.doc.len_lines() == 0 {
            return;
        }
        let clamped = drop_caret.min(self.state.doc.len_bytes());
        let line = self.state.doc.byte_to_line(clamped);
        if line >= self.state.doc.len_lines() {
            return;
        }
        let row_top_y = self.rect.top() + self.view.line_top_y(line);
        let above_h = self.view.height_map.block_above(line);
        let text_top_y = row_top_y + above_h;
        let row_h = self.view.height_map.text_height(line).max(self.view.line_height);

        let line_start = self.state.doc.line_to_byte(line);
        let line_text = self.state.doc.line_str(line);
        let local = clamped.saturating_sub(line_start).min(line_text.len());
        let prefix = &line_text[..local];
        let font_id = self.base_font_id.clone();
        let galley = self
            .ui
            .fonts(|f| f.layout_no_wrap(prefix.to_string(), font_id, Color32::WHITE));
        let x = self.text_origin_x + galley.size().x;

        let color = Color32::from_rgb(0, 150, 200);
        let r = Rect::from_min_max(
            Pos2::new(x - 0.75, text_top_y),
            Pos2::new(x + 0.75, text_top_y + row_h),
        );
        self.painter.rect_filled(r, 0.0, color);
    }

    fn paint_visible_line(&mut self, line_idx: usize) {
        let row_top_y = self.rect.top() + self.view.line_top_y(line_idx);
        let above_h = self.view.height_map.block_above(line_idx);
        let below_h = self.view.height_map.block_below(line_idx);
        let line_top_y = row_top_y + above_h;
        let row_height = self.view.row_height(line_idx);
        let line_text = self.state.doc.line_str(line_idx);
        let line_byte_start = self.state.doc.line_to_byte(line_idx);
        let line_byte_end = line_byte_start + line_text.len();
        let is_hidden = self.view.height_map.text_height(line_idx) <= 0.5;

        if above_h > 0.0 {
            let zone_rect = Rect::from_min_max(
                Pos2::new(self.rect.left(), row_top_y),
                Pos2::new(self.rect.right(), row_top_y + above_h),
            );
            BlockPaint {
                ui: &*self.ui,
                painter: &self.painter,
                font_size: self.view.font_size,
                line_height: self.view.line_height,
                text_origin_x: self.text_origin_x,
                hatched_default: self.hatched_default,
                click_zones: &mut self.view.click_zones,
                widget_rect: self.rect,
            }
            .paint_zone(&BlockZone {
                layers: &self.view.decorations.layers,
                line_byte_start,
                line_byte_end,
                side: BlockSide::Above,
                rect: zone_rect,
            });
        }

        if !is_hidden {
            let vlines: Vec<(usize, usize)> = if self.view.wrap_map.enabled() {
                self.view.wrap_map
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
            let row_h = (self.view.height_map.text_height(line_idx) / vline_count)
                .max(self.view.line_height);
            for (vi, (vs, ve)) in vlines.iter().enumerate() {
                let vline_top_y = line_top_y + (vi as f32) * row_h;
                let vline_byte_start = line_byte_start + *vs;
                let vline_byte_end = line_byte_start + *ve;
                let vline_text = line_text[*vs..*ve].to_string();
                let is_first_vline = vi == 0;
                self.paint_text_row(
                    RowSpan {
                        line_idx,
                        byte_start: vline_byte_start,
                        byte_end: vline_byte_end,
                        top_y: vline_top_y,
                        height: row_h,
                    },
                    &vline_text,
                    is_first_vline,
                );
            }
        }

        if below_h > 0.0 {
            let zone_top = if is_hidden { line_top_y } else { line_top_y + row_height };
            let zone_rect = Rect::from_min_max(
                Pos2::new(self.rect.left(), zone_top),
                Pos2::new(self.rect.right(), zone_top + below_h),
            );
            BlockPaint {
                ui: &*self.ui,
                painter: &self.painter,
                font_size: self.view.font_size,
                line_height: self.view.line_height,
                text_origin_x: self.text_origin_x,
                hatched_default: self.hatched_default,
                click_zones: &mut self.view.click_zones,
                widget_rect: self.rect,
            }
            .paint_zone(&BlockZone {
                layers: &self.view.decorations.layers,
                line_byte_start,
                line_byte_end,
                side: BlockSide::Below,
                rect: zone_rect,
            });
        }
    }

    fn paint_text_row(
        &mut self,
        span: RowSpan,
        vline_text: &str,
        is_first_vline: bool,
    ) {
        let RowSpan {
            line_idx,
            byte_start: vline_byte_start,
            top_y: vline_top_y,
            height: row_h,
            ..
        } = span;
        // Fingerprint inputs that, if any changes, invalidate the cached layout
        // for this row. text_hash catches in-place edits to a same-length line;
        // doc_id catches buffer mutations; sel_line catches cursor-on-line
        // reveal decorations; layers_sig catches changed decoration sets;
        // metrics catches font/size/width changes.
        let text_hash = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            vline_text.hash(&mut h);
            h.finish()
        };
        let doc_id = self.state.doc.content_id() as u64;
        let sel_line = self.state
            .doc
            .byte_to_line(self.state.selection.main().head.offset().min(self.state.doc.len_bytes()))
            as u64;
        let layers_sig = self.view.decorations.signature;
        let metrics = compute_metrics_fingerprint(self.view);
        let key = (line_idx, vline_byte_start);

        // Cache lookup. On a hit we clone the stored layout/measured (cheap —
        // galleys are Arc<Galley>, segments hold SmolStr + Arc<dyn InlineWidget>).
        let frame = self.cache.frame;
        let hit = self.cache
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
            if let Some(entry) = self.cache.entries.get_mut(&key) {
                entry.last_used_frame = frame;
            }
            (l, m)
        } else {
            // Build fresh.
            let layout = LineLayoutBuilder {
                line_text: vline_text,
                line_byte_start: vline_byte_start,
                line_byte_end: vline_byte_start + vline_text.len(),
                events: Vec::new(),
                trailing_widgets: Vec::new(),
                base_font_size: self.view.font_size,
                base_color: self.text_color,
            }
            .build(&self.view.decorations.layers);
            let measured = layout.measure(self.ui);
            self.cache.entries.insert(
                key,
                CachedRow {
                    last_used_frame: self.cache.frame,
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
            self.paint_line_bgs(span);
            if !self.view.hide_gutter {
                self.paint_gutter(span);
            }
        } else if let Some(bg) = self.wrapped_continuation_bg(line_idx) {
            let r = Rect::from_min_max(
                Pos2::new(self.rect.left(), vline_top_y),
                Pos2::new(self.rect.right(), vline_top_y + row_h),
            );
            self.painter.rect_filled(r, 0.0, bg);
        }
        self.paint_selections(span, &measured, vline_text.len());
        self.paint_segments(&layout, &measured, vline_top_y, row_h);
        self.paint_cursors(span, &measured);
    }

    /// If the buffer line has a Line bg, continuation vlines (wrap rows after the
    /// first) should also paint that bg so the highlight runs through.
    fn wrapped_continuation_bg(&self, line_idx: usize) -> Option<Color32> {
        let line_byte_start = self.state.doc.line_to_byte(line_idx);
        for layer in &self.view.decorations.layers {
            for (range, deco) in layer.iter_overlapping(line_byte_start..line_byte_start + 1) {
                if let Decoration::Line(LineStyle { bg: Some(c), .. }) = deco {
                    if self.state.doc.byte_to_line(range.start) == line_idx {
                        return Some(to_egui_color(*c));
                    }
                }
            }
        }
        None
    }
}

impl<'a> Widget<'a> {
fn prewrap_visible(&mut self) {
    let view = &mut *self.view;
    let state = &*self.state;
    if !view.wrap_map.enabled() {
        return;
    }
    let total = state.doc.len_lines();
    // Decide the line range to rescan. On a pure scroll (full-doc geometry
    // inputs unchanged), only lines whose viewport-scoped decoration coverage
    // could have shifted need work — the union of last frame's and this
    // frame's visible band. Lines outside that union were covered by the same
    // layers in both frames, so their cached wraps remain valid. On any
    // geometry change (doc edit, full-doc layer churn, width/char/enabled
    // change, line-count change) we fall back to walking the whole document.
    //
    // The "always walk all lines" approach we kept here for a while masked an
    // off-screen-stale bug after edits by paying O(N) per scroll frame; the
    // partition between geometry-affecting and viewport-scoped layers
    // (`DecorationLayers::geometry_epoch` vs `signature`) gives us the same
    // correctness without the per-line cost.
    //
    // Per-line `font_scale` (heading promotion etc.) and visual spans are
    // folded into the wrap calc so a heading whose decorated text is e.g.
    // 1.6× the base monospace cell wraps at the right column, and a hidden
    // marker (`<span …>` tag, wikilink target) doesn't eat wrap budget it
    // never paints. Probes one byte past EOL so decorations anchored at the
    // line break still register (range queries are half-open).
    let geo_key = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        state.doc.content_id().hash(&mut h);
        view.decorations.geometry_epoch.hash(&mut h);
        view.wrap_map.width().to_bits().hash(&mut h);
        view.wrap_map.char_width().to_bits().hash(&mut h);
        view.wrap_map.enabled().hash(&mut h);
        h.finish()
    };
    let vp = view.visible_lines();
    let walk = view.wrap_map.walk_range(geo_key, total, (vp.start, vp.end));
    let mut spans: Vec<VisualSpan> = Vec::new();
    for line in walk {
        let start = state.doc.line_to_byte(line);
        let line_text = state.doc.line_str(line);
        let line_len = line_text.len();
        let probe_end = (start + line_len).max(start + 1);
        let snap = |b: usize| -> usize {
            let mut b = b.min(line_len);
            while b > 0 && !line_text.is_char_boundary(b) {
                b -= 1;
            }
            b
        };
        let mut max_scale: f32 = 1.0;
        spans.clear();
        for layer in &view.decorations.layers {
            for (range, deco) in layer.iter_overlapping(start..probe_end) {
                match deco {
                    Decoration::Mark(ms) => {
                        if let Some(s) = ms.font_scale
                            && s > max_scale
                        {
                            max_scale = s;
                        }
                    }
                    Decoration::Replace { display } => {
                        let s = snap(range.start.saturating_sub(start));
                        let e = snap(range.end.saturating_sub(start));
                        if e > s {
                            let cols =
                                display.as_ref().map_or(0, |d| d.chars().count()) as u32;
                            spans.push(VisualSpan { start: s as u32, end: e as u32, cols });
                        }
                    }
                    _ => {}
                }
            }
        }
        spans.sort_by_key(|s| s.start);
        view.wrap_map.get_or_compute(line, &line_text, max_scale, &spans);
    }
}
}

impl<'a> PaintCtx<'a> {
    fn paint_line_bgs(&self, span: RowSpan) {
        let RowSpan { line_idx, byte_start: line_byte_start, byte_end: line_byte_end, top_y: line_top_y, height: row_height } = span;
        for layer in &self.view.decorations.layers {
            for (range, deco) in layer.iter_overlapping(line_byte_start..line_byte_end + 1) {
                if let Decoration::Line(LineStyle { bg: Some(c), .. }) = deco {
                    if self.state.doc.byte_to_line(range.start) == line_idx {
                        let r = Rect::from_min_max(
                            Pos2::new(self.rect.left(), line_top_y),
                            Pos2::new(self.rect.right(), line_top_y + row_height),
                        );
                        self.painter.rect_filled(r, 0.0, to_egui_color(*c));
                    }
                }
            }
        }
    }

    fn paint_selections(
        &self,
        span: RowSpan,
        measured: &LineMeasured,
        line_text_len: usize,
    ) {
        let RowSpan { line_idx, byte_start: line_byte_start, byte_end: line_byte_end, top_y: line_top_y, height: row_height } = span;
        for r in self.state.selection.ranges() {
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
                    Pos2::new(self.text_origin_x + x_start, line_top_y),
                    Pos2::new(self.text_origin_x + x_end, line_top_y + row_height),
                );
                self.painter.rect_filled(sel, 0.0, self.selection_color);
            }
            if e > line_byte_end && line_idx + 1 < self.state.doc.len_lines() {
                let x_end = measured.total_width;
                let extra = Rect::from_min_max(
                    Pos2::new(self.text_origin_x + x_end, line_top_y),
                    Pos2::new(self.text_origin_x + x_end + self.view.font_size * 0.5, line_top_y + row_height),
                );
                self.painter.rect_filled(extra, 0.0, self.selection_color);
            }
        }
    }

    fn paint_gutter(&mut self, span: RowSpan) {
        let RowSpan { line_idx, byte_start: line_byte_start, byte_end: line_byte_end, top_y: line_top_y, height: row_height } = span;
        let num = (line_idx + 1).to_string();
        let num_galley = self
            .ui
            .fonts(|f| f.layout_no_wrap(num, self.base_font_id.clone(), self.gutter_color));
        let num_x = self.rect.left() + self.view.gutter_width - num_galley.size().x - 6.0;
        let num_y = line_top_y + (row_height - num_galley.size().y) * 0.5;
        self.painter
            .galley(Pos2::new(num_x, num_y), num_galley, self.gutter_color);

        if let Some(ch) = self.collect_fold_chevron(line_byte_start, line_byte_end) {
            // Draw chevron as a small filled triangle via Painter::add(Shape::convex_polygon)
            // — Unicode triangle glyphs aren't reliably present in egui's bundled
            // fonts (would render as the missing-glyph box).
            let size = self.base_font_id.size * 0.42;
            let cx = self.rect.left() + 9.0;
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
            self.painter.add(egui::Shape::convex_polygon(
                points,
                self.gutter_color,
                Stroke::NONE,
            ));
            self.view.click_zones.push(ClickZone {
                rect: ClickRect {
                    x_min: 0.0,
                    y_min: line_top_y - self.rect.min.y,
                    x_max: 18.0,
                    y_max: line_top_y - self.rect.min.y + row_height,
                },
                action: ClickAction::ToggleFold(ch.id),
            });
        }
    }

    fn paint_segments(
        &mut self,
        layout: &LineLayout,
        measured: &LineMeasured,
        line_top_y: f32,
        row_height: f32,
    ) {
        for (idx, seg) in layout.segments.iter().enumerate() {
            let g = &measured.galleys[idx];
            let seg_x = self.text_origin_x + measured.x_starts[idx];
            let seg_w = measured.seg_widths[idx];
            let seg_y = line_top_y + (row_height - g.size().y) * 0.5;
            let fg = seg.style.fg.map(to_egui_color).unwrap_or(self.text_color);

            if let Some(widget) = &seg.widget {
                let widget = widget.clone();
                let g_clone = g.clone();
                self.paint_inline_widget_placeholder(
                    &widget,
                    SegSpan {
                        x: seg_x,
                        width: seg_w,
                        top_y: line_top_y,
                        height: row_height,
                        label_y: seg_y,
                    },
                    &g_clone,
                );
                continue;
            }

            if let Some(bg) = seg.style.bg {
                let bg_rect = Rect::from_min_max(
                    Pos2::new(seg_x, line_top_y),
                    Pos2::new(seg_x + seg_w, line_top_y + row_height),
                );
                self.painter.rect_filled(bg_rect, 0.0, to_egui_color(bg));
            }
            self.painter.galley(Pos2::new(seg_x, seg_y), g.clone(), fg);
            if seg.style.bold {
                self.painter.galley(Pos2::new(seg_x + 0.5, seg_y), g.clone(), fg);
            }
            if seg.style.underline {
                let y = seg_y + g.size().y * 0.92;
                self.painter.line_segment(
                    [Pos2::new(seg_x, y), Pos2::new(seg_x + seg_w, y)],
                    Stroke::new(1.0, fg),
                );
            }
            if seg.style.strikethrough {
                let y = seg_y + g.size().y * 0.5;
                self.painter.line_segment(
                    [Pos2::new(seg_x, y), Pos2::new(seg_x + seg_w, y)],
                    Stroke::new(1.0, fg),
                );
            }
        }
    }

    /// v1 placeholder render for an inline widget decoration: a styled rect of
    /// the widget's measured size plus a tiny "widget" label. Real per-widget
    /// painting is deferred to a future trait method.
    fn paint_inline_widget_placeholder(
        &mut self,
        widget: &Arc<dyn InlineWidget>,
        span: SegSpan,
        label_galley: &Arc<egui::Galley>,
    ) {
        let SegSpan { x: seg_x, width: seg_w, top_y: line_top_y, height: row_height, label_y } = span;
        let visuals = self.ui.style().visuals.clone();
        let rect = Rect::from_min_max(
            Pos2::new(seg_x, line_top_y),
            Pos2::new(seg_x + seg_w, line_top_y + row_height),
        );

        if let Some(display) = widget.display() {
            // Textual variant — host wants this widget to read as ordinary
            // inline text with a colored background (patch-review intraline
            // insertion, future inline diagnostics, etc.). Skip the bordered
            // placeholder entirely and just paint a bg fill + the galley.
            if let Some(bg) = display.bg {
                self.painter.rect_filled(rect, 0.0, to_egui_color(bg));
            }
            let fg = display
                .fg
                .map(to_egui_color)
                .unwrap_or_else(|| visuals.text_color());
            self.painter
                .galley(Pos2::new(seg_x, label_y), label_galley.clone(), fg);
            if display.strikethrough {
                let mid_y = line_top_y + row_height * 0.5;
                self.painter.line_segment(
                    [Pos2::new(seg_x, mid_y), Pos2::new(seg_x + seg_w, mid_y)],
                    Stroke::new(1.0, fg),
                );
            }
        } else {
            let bg = if visuals.dark_mode {
                Color32::from_rgba_unmultiplied(70, 80, 110, 80)
            } else {
                Color32::from_rgba_unmultiplied(210, 220, 240, 220)
            };
            let border = visuals.weak_text_color().gamma_multiply(0.5);
            self.painter.rect_filled(rect, 3.0, bg);
            self.painter
                .rect_stroke(rect, 3.0, Stroke::new(0.5, border), egui::StrokeKind::Inside);
            self.painter
                .galley(Pos2::new(seg_x + 2.0, label_y), label_galley.clone(), border);
        }

        if widget.handles_click() {
            self.view.click_zones.push(ClickZone {
                rect: ClickRect {
                    x_min: seg_x - self.rect.min.x,
                    y_min: line_top_y - self.rect.min.y,
                    x_max: seg_x + seg_w - self.rect.min.x,
                    y_max: line_top_y + row_height - self.rect.min.y,
                },
                action: ClickAction::WidgetClick(widget.widget_id()),
            });
        }
    }

    fn paint_cursors(&self, span: RowSpan, measured: &LineMeasured) {
        let RowSpan { byte_start: line_byte_start, byte_end: line_byte_end, top_y: line_top_y, height: row_height, .. } = span;
        // Only paint the caret while the widget holds keyboard focus. An
        // unfocused editor takes neither typing nor Ctrl-Z, so a visible
        // caret there reads as misleadingly active — hide it entirely.
        if !self.has_focus {
            return;
        }
        // Blink logic: the caret is solid for the first half-period after any
        // interaction (typing, motion, click), then toggles every half-period.
        // Half-period = 530 ms, full blink cycle = 1060 ms.
        const HALF_PERIOD_MS: u128 = 530;
        const FULL_CYCLE_MS: u128 = HALF_PERIOD_MS * 2;
        let elapsed_ms = self.view.last_interaction.elapsed().as_millis();
        let caret_visible = if elapsed_ms < HALF_PERIOD_MS {
            // Within the first half-period: always solid (caret shows right
            // after the user acts).
            true
        } else {
            // After the first half-period, toggle each half-period within the
            // repeating blink cycle. Phase 0..HALF_PERIOD_MS → visible,
            // phase HALF_PERIOD_MS..FULL_CYCLE_MS → hidden.
            let phase = elapsed_ms % FULL_CYCLE_MS;
            phase < HALF_PERIOD_MS
        };
        if !caret_visible {
            return;
        }
        for r in self.state.selection.ranges() {
            let head = r.head.offset();
            if head < line_byte_start || head > line_byte_end {
                continue;
            }
            let local = head - line_byte_start;
            let x = measured.x_at_buffer_offset(local);
            let cursor_rect = Rect::from_min_max(
                Pos2::new(self.text_origin_x + x - 0.5, line_top_y),
                Pos2::new(self.text_origin_x + x + 1.0, line_top_y + row_height),
            );
            self.painter.rect_filled(cursor_rect, 0.0, self.cursor_color);
        }
    }
}

impl<'a> PaintCtx<'a> {
    fn collect_fold_chevron(
        &self,
        line_start: usize,
        line_end: usize,
    ) -> Option<editor_core::decoration::FoldChevron> {
        for layer in &self.view.decorations.layers {
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
