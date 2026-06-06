//! A small `+` split-button: one rounded button whose right edge is a built-in
//! caret segment that opens a dropdown menu. One reusable control for the
//! Files-panel header (`sidebar-new-item-button`) and the canvas create toolbar
//! (`canvas-node-create`) — the wide primary half runs the common action, the
//! narrow caret half exposes the rest as a visible dropdown. The two halves
//! share one outer rounded outline with a hairline seam between them, so it
//! reads as a single button with a dropdown built into its side.
//! status: split-add-button

use eframe::egui;

use crate::icons::{Icon, ICONS};

/// Shared corner radius for the small toolbar buttons (this split-button and the
/// canvas toolbar's icon buttons), so they round consistently with every other
/// button. Re-exported from the theme, which owns the token. egui's
/// `ImageButton` defaults to square corners, so the icon buttons opt in via
/// `.corner_radius(BUTTON_CORNER_RADIUS)`.
pub const BUTTON_CORNER_RADIUS: u8 = hiker_theme::BUTTON_CORNER_RADIUS;

/// The outcome of one frame of a [`split_add_button`].
pub struct SplitAddResponse {
    /// The primary (wide) half was clicked this frame.
    pub primary_clicked: bool,
    /// The combined response over both halves, for anchoring a follow-up popup
    /// (e.g. the canvas link prompt) beneath the whole control.
    pub response: egui::Response,
}

/// Render the `+` split-button: a wide primary `+` half butted against a narrow
/// caret half that opens `menu` as a dropdown, both inside one rounded outline.
/// `hover` is the primary half's tooltip; `menu` builds the dropdown body (use
/// `ui.close()` to dismiss it after a pick). status: split-add-button
pub fn split_add_button(
    ui: &mut egui::Ui,
    hover: &str,
    menu: impl FnOnce(&mut egui::Ui),
) -> SplitAddResponse {
    // egui's `ImageButton` pads the image by `button_padding.x` on BOTH axes
    // (see its `ui`), so match that here so this control is the same height as a
    // neighbouring icon button — using `button_padding.y` (default 1px) made it
    // noticeably shorter. But cap the height to the available cross-axis space:
    // a tight host strip (the 26px side-bar section header) is shorter than a
    // full icon button, and without the cap the control overflowed into the
    // rows below. The padding the height ends up using is fed back into the
    // widths so the control stays proportional when clamped.
    let icon = 14.0_f32;
    let caret_icon = 11.0_f32;
    let pad_x = ui.spacing().button_padding.x;
    let avail = ui.available_height();
    let h = (icon + pad_x * 2.0).min((avail - 2.0).max(icon + 6.0));
    let pad = ((h - icon) * 0.5).max(1.0);
    let primary_w = icon + pad * 2.0;
    let caret_w = caret_icon + pad * 1.4;

    // Reserve a stable id (for the two sub-rect interactions + the popup) and
    // the layout space for the whole control.
    let id = ui.next_auto_id();
    ui.skip_ahead_auto_ids(1);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(primary_w + caret_w, h), egui::Sense::hover());

    let split_x = rect.left() + primary_w;
    let primary_rect = egui::Rect::from_min_max(rect.left_top(), egui::pos2(split_x, rect.bottom()));
    let caret_rect = egui::Rect::from_min_max(egui::pos2(split_x, rect.top()), rect.right_bottom());

    let primary = ui
        .interact(primary_rect, id.with("primary"), egui::Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(hover);
    let caret = ui
        .interact(caret_rect, id.with("caret"), egui::Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text("More\u{2026}");

    // Per-corner rounding so each half keeps the outer rounded corners on its
    // outer edge and squares off at the shared seam. `r` matches the app's
    // other small buttons.
    let r = BUTTON_CORNER_RADIUS;
    let outer = egui::CornerRadius::same(r);
    let left = egui::CornerRadius { nw: r, sw: r, ne: 0, se: 0 };
    let right = egui::CornerRadius { nw: 0, sw: 0, ne: r, se: r };

    let hovered = ui.visuals().widgets.hovered;
    let active = ui.visuals().widgets.active;
    let seg_fill = |resp: &egui::Response| {
        if resp.is_pointer_button_down_on() {
            Some(active.weak_bg_fill)
        } else if resp.hovered() {
            Some(hovered.weak_bg_fill)
        } else {
            None
        }
    };
    let painter = ui.painter().clone();
    // Ghost button (status: style-ghost-button): nothing at rest. Once a half is
    // engaged, fill that half, draw the seam dividing the two, and wrap the whole
    // control in one border — so it reads as a single button with a built-in
    // dropdown only while in use.
    if let Some(c) = seg_fill(&primary) {
        painter.rect_filled(primary_rect, left, c);
    }
    if let Some(c) = seg_fill(&caret) {
        painter.rect_filled(caret_rect, right, c);
    }
    let engaged = primary.hovered()
        || caret.hovered()
        || primary.is_pointer_button_down_on()
        || caret.is_pointer_button_down_on();
    if engaged {
        painter.vline(
            split_x,
            egui::Rangef::new(rect.top() + 3.0, rect.bottom() - 3.0),
            egui::Stroke::new(1.0, hiker_theme::divider()),
        );
        painter.rect_stroke(rect, outer, hovered.bg_stroke, egui::StrokeKind::Inside);
    }

    ICONS
        .image(Icon::Plus)
        .paint_at(ui, egui::Rect::from_center_size(primary_rect.center(), egui::Vec2::splat(icon)));
    ICONS.image(Icon::ChevronDown).paint_at(
        ui,
        egui::Rect::from_center_size(caret_rect.center(), egui::Vec2::splat(caret_icon)),
    );

    egui::Popup::menu(&caret).show(menu);

    SplitAddResponse {
        primary_clicked: primary.clicked(),
        response: primary.union(caret),
    }
}
