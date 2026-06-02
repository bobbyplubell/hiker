//! Custom window titlebar (frameless mode, the default). A single merged
//! top strip: the first top toolbar's actions on the left, the centered
//! command center, and OS-style window controls (minimize / maximize /
//! close) on the right. Empty areas drag the window. [command-center-topbar]

use eframe::egui::{self, ViewportCommand};

use crate::icons;
use crate::state::AppState;
use hiker_theme as theme;

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
            .frame(
                egui::Frame::default()
                    .fill(ctx.style().visuals.panel_fill)
                    .inner_margin(egui::Margin::symmetric(6, 2))
                    .stroke(egui::Stroke::new(1.0, theme::divider())),
            )
            .show(ctx, |ui| {
                let full = ui.max_rect();
                let controls_left = full.right() - CONTROLS_WIDTH;

                // Right: OS-style window controls.
                window_controls(ui, ctx, full);

                // Left: the first top toolbar's actions, packed left.
                let left = egui::Rect::from_min_max(
                    full.min,
                    egui::pos2(controls_left, full.bottom()),
                );
                let toolbar = ui.scope_builder(
                    egui::UiBuilder::new()
                        .max_rect(left)
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                    |ui| self.render_top_bar_inline(ui),
                );
                // Head ends at `head_right`; the tail (sidebar toggles)
                // starts at `tail_left`, right-aligned next to the controls.
                let (head_right, tail_left) =
                    toolbar.inner.unwrap_or((full.left(), controls_left));

                // Center: command center.
                let cc = show_command_center.then(|| crate::command_center::command_center_rect(full));
                if show_command_center {
                    self.command_center(ui, full);
                }

                // Drag zones: only the empty gaps between the head, command
                // center, and tail — never over a button — so no click is
                // swallowed. Pointer-down initiated for 1:1 tracking (the
                // lapce / VSCode no-drag model).
                let pad = 4.0;
                let gaps: [egui::Rect; 2] = match cc {
                    Some(cc) => [
                        egui::Rect::from_min_max(
                            egui::pos2(head_right + pad, full.top()),
                            egui::pos2(cc.left() - pad, full.bottom()),
                        ),
                        egui::Rect::from_min_max(
                            egui::pos2(cc.right() + pad, full.top()),
                            egui::pos2(tail_left - pad, full.bottom()),
                        ),
                    ],
                    None => [
                        egui::Rect::from_min_max(
                            egui::pos2(head_right + pad, full.top()),
                            egui::pos2(tail_left - pad, full.bottom()),
                        ),
                        egui::Rect::NOTHING,
                    ],
                };
                for (i, gap) in gaps.into_iter().enumerate() {
                    if gap.width() < 1.0 {
                        continue;
                    }
                    let resp = ui.interact(
                        gap,
                        ui.id().with(("titlebar-drag", i)),
                        egui::Sense::click_and_drag(),
                    );
                    if resp.double_clicked() {
                        let max = ctx.input(|i| i.viewport().maximized).unwrap_or(false);
                        ctx.send_viewport_cmd(ViewportCommand::Maximized(!max));
                    } else if resp.is_pointer_button_down_on() {
                        ctx.send_viewport_cmd(ViewportCommand::StartDrag);
                    }
                }
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
