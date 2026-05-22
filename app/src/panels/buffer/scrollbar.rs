//! Auto-hiding overlay scrollbar for the editor body. Painted along the
//! right edge when the minimap is hidden; macOS-style fade-in/out with
//! click + drag seek. Pulled out of `buffer/mod.rs` to keep that file
//! under the workspace's per-file length cap. The panel constructs an
//! `AutoScrollbar` and calls `paint` once per frame from `show_editor`.

use eframe::egui;

/// Overlay scrollbar painted along the right edge of the editor when
/// the minimap is hidden. macOS-style: invisible at rest, fades in
/// when the pointer is inside the editor or right after a scroll, and
/// supports click + drag on the thumb to seek `view.scroll_y`.
///
/// We can't use `egui::ScrollArea` here because the editor maintains
/// its own viewport model (`view.scroll_y` + `height_map`) and paints
/// only the visible band — wrapping it in a `ScrollArea` would force
/// us to lay out the whole document into a scrollable canvas.
/// Bundles the inputs the auto-hiding scrollbar paint needs. Wrapping
/// in a struct + method keeps the helper out of clippy's
/// `single_call_fn` lint (methods with `self` are exempt).
pub(super) struct AutoScrollbar<'a> {
    pub(super) ui: &'a mut egui::Ui,
    pub(super) view: &'a mut editor_view::viewport::ViewState,
    pub(super) editor_rect: egui::Rect,
}

impl AutoScrollbar<'_> {
    pub(super) fn paint(self) {
        let AutoScrollbar { ui, view, editor_rect } = self;
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
}
