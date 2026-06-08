//! Custom window titlebar (frameless mode, the default). A single merged
//! top strip: the first top toolbar's actions on the left, the centered
//! command center, and OS-style window controls (minimize / maximize /
//! close) on the right. Empty areas drag the window. [command-center-topbar]

use eframe::egui::{self, ViewportCommand};

use crate::icons;
use crate::state::AppState;

/// Height of the merged titlebar strip.
const TITLEBAR_HEIGHT: f32 = 34.0;
/// Width reserved on the trailing edge for the three window controls.
const CONTROLS_WIDTH: f32 = 96.0;

impl AppState {
    /// Render the merged custom titlebar. When `show_command_center` is
    /// true the command center is overlaid centered (suppressed in reader
    /// view). The top toolbar is folded in on the left.
    pub fn titlebar(&mut self, ctx: &egui::Context, show_command_center: bool) {
        egui::TopBottomPanel::top("custom-titlebar")
            .exact_height(TITLEBAR_HEIGHT)
            // No frame border AND no panel separator line: the titlebar shares
            // the panel fill with the side bars, so any divider here would cut a
            // dark line across an otherwise continuous surface. egui's panel draws
            // a separator at its inner edge by default — turn it off too so the
            // titlebar flows straight into the side bars below.
            .show_separator_line(false)
            .frame(
                egui::Frame::default()
                    .fill(ctx.style().visuals.panel_fill)
                    .inner_margin(egui::Margin::symmetric(6, 2)),
            )
            .show(ctx, |ui| {
                let full = ui.max_rect();
                let controls_left = full.right() - CONTROLS_WIDTH;

                // Drag the window by the whole strip — sensed FIRST, so it sits
                // UNDER every button/box added below and only catches the pixels
                // none of them claim (the egui custom-window-frame idiom). This
                // replaces the old "carve empty gaps between widgets" model,
                // which left no draggable region — and lost the controls under
                // them — whenever the toolbar grew wide enough to fill the strip
                // (`left_to_right` doesn't wrap). Double-click toggles maximize;
                // pointer-down starts a 1:1 drag.
                let drag = ui.interact(
                    full,
                    ui.id().with("titlebar-drag-bg"),
                    egui::Sense::click_and_drag(),
                );
                if drag.double_clicked() {
                    let max = ctx.input(|i| i.viewport().maximized).unwrap_or(false);
                    ctx.send_viewport_cmd(ViewportCommand::Maximized(!max));
                } else if drag.is_pointer_button_down_on() {
                    ctx.send_viewport_cmd(ViewportCommand::StartDrag);
                }

                // Left: the first top toolbar's actions, packed left. Clip the
                // sublayout to its allotted rect: egui's `left_to_right` doesn't
                // wrap or clip, so a head wide enough to overflow `controls_left`
                // would otherwise paint over (and steal clicks from) the window
                // controls — a cause of the "controls go missing" report.
                let left = egui::Rect::from_min_max(
                    full.min,
                    egui::pos2(controls_left, full.bottom()),
                );
                ui.scope_builder(
                    egui::UiBuilder::new()
                        .max_rect(left)
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                    |ui| {
                        ui.set_clip_rect(left.intersect(ui.clip_rect()));
                        self.render_top_bar_inline(ui)
                    },
                );

                // Center: command center (its own click sense, drawn over the
                // drag strip so a click opens the palette rather than dragging).
                if show_command_center {
                    self.command_center(ui, full);
                }

                // Right: OS-style window controls. Drawn LAST so they paint over
                // (and take input priority over) anything that overlaps the
                // trailing edge — a centered command center on a narrow window,
                // or any toolbar bleed — instead of vanishing beneath it. This is
                // what the "buttons sit above the titlebar" intent requires:
                // same-layer z-order is call order, so they must come last.
                window_controls(ui, ctx, full);
            });
    }
}

/// Draw invisible resize grips along the window edges + corners (frameless
/// windows have no OS resize border). Each grip lives in its OWN tiny
/// foreground `Area` so it blocks lower-layer (panel) input only on its
/// own thin strip — a single area spanning all edges would mask the whole
/// window body (sidebar / editor clicks). Each initiates a compositor
/// resize on pointer-down with the matching cursor. Grips stay below the
/// titlebar so they never overlap its buttons (trade-off: no top-edge /
/// top-corner resize).
pub fn window_resize_handles(ctx: &egui::Context) {
    use egui::{CursorIcon, ResizeDirection, Sense};
    if ctx.input(|i| i.viewport().maximized).unwrap_or(false)
        || ctx.input(|i| i.viewport().fullscreen).unwrap_or(false)
    {
        return;
    }
    let s = ctx.screen_rect();
    const B: f32 = 6.0;
    let (l, r, t, b) = (s.left(), s.right(), s.top(), s.bottom());
    let top = t + TITLEBAR_HEIGHT;
    let rect = egui::Rect::from_min_max;
    let p = egui::pos2;
    let grips: [(egui::Rect, ResizeDirection, CursorIcon); 5] = [
        (rect(p(l, b - B), p(l + B, b)), ResizeDirection::SouthWest, CursorIcon::ResizeSouthWest),
        (rect(p(r - B, b - B), p(r, b)), ResizeDirection::SouthEast, CursorIcon::ResizeSouthEast),
        (rect(p(l + B, b - B), p(r - B, b)), ResizeDirection::South, CursorIcon::ResizeSouth),
        (rect(p(l, top), p(l + B, b - B)), ResizeDirection::West, CursorIcon::ResizeWest),
        (rect(p(r - B, top), p(r, b - B)), ResizeDirection::East, CursorIcon::ResizeEast),
    ];
    for (i, (grip, dir, cursor)) in grips.into_iter().enumerate() {
        egui::Area::new(egui::Id::new(("window-resize-grip", i)))
            .order(egui::Order::Foreground)
            // Don't constrain to the screen: an edge-pinned area would
            // otherwise have its interact rect clipped/shifted inward, so
            // only the left grip (already at x=0) survived.
            .constrain(false)
            .fixed_pos(grip.min)
            .show(ctx, |ui| {
                let resp = ui.allocate_rect(grip, Sense::drag());
                if resp.hovered() {
                    ui.ctx().set_cursor_icon(cursor);
                }
                if resp.is_pointer_button_down_on() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::BeginResize(dir));
                }
            });
    }
}

/// Minimize / maximize / close, pinned to the trailing edge. Rendered via
/// `scope_builder` (egui's custom-window-frame pattern) so the buttons sit
/// above the titlebar and reliably capture their clicks.
fn window_controls(ui: &mut egui::Ui, ctx: &egui::Context, full: egui::Rect) {
    let rect = egui::Rect::from_min_max(
        egui::pos2(full.right() - CONTROLS_WIDTH, full.top()),
        full.right_bottom(),
    );
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
        |ui| {
            if ui
                .add(egui::Button::image(icons::ICONS.image(crate::icons::Icon::WindowClose)).frame(false))
                .on_hover_text("Close")
                .clicked()
            {
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
            if ui
                .add(egui::Button::image(icons::ICONS.image(crate::icons::Icon::WindowMaximize)).frame(false))
                .on_hover_text("Maximize")
                .clicked()
            {
                let max = ctx.input(|i| i.viewport().maximized).unwrap_or(false);
                ctx.send_viewport_cmd(ViewportCommand::Maximized(!max));
            }
            if ui
                .add(egui::Button::image(icons::ICONS.image(crate::icons::Icon::WindowMinimize)).frame(false))
                .on_hover_text("Minimize")
                .clicked()
            {
                ctx.send_viewport_cmd(ViewportCommand::Minimized(true));
            }
        },
    );
}
