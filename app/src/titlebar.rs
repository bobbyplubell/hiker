//! Custom window titlebar shown when `state.ui.custom_titlebar` is on.
//! Replaces the OS-provided chrome with an in-app row that can be
//! dragged + has minimize / maximize / close buttons.

use eframe::egui::{self, ViewportCommand};

use crate::icons;
use crate::state::AppState;
use crate::theme;

impl AppState {
    pub fn titlebar(&mut self, ctx: &egui::Context) {
    egui::TopBottomPanel::top("custom-titlebar")
        .exact_height(28.0)
        .frame(
            egui::Frame::default()
                .fill(ctx.style().visuals.panel_fill)
                .stroke(egui::Stroke::new(1.0, theme::divider())),
        )
        .show(ctx, |ui| {
            ui.horizontal_centered(|ui| {
                ui.label(egui::RichText::new("hiker").strong());

                // Drag region claims the remaining width, with the right
                // edge reserved for the three window-control buttons.
                ui.with_layout(
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        if ui
                            .add(egui::Button::image(icons::ICONS.image(crate::icons::Icon::WindowClose)))
                            .on_hover_text("Close")
                            .clicked()
                        {
                            ctx.send_viewport_cmd(ViewportCommand::Close);
                        }
                        if ui
                            .add(egui::Button::image(icons::ICONS.image(crate::icons::Icon::WindowMaximize)))
                            .on_hover_text("Maximize")
                            .clicked()
                        {
                            let cur = ctx
                                .input(|i| i.viewport().maximized)
                                .unwrap_or(false);
                            ctx.send_viewport_cmd(
                                ViewportCommand::Maximized(!cur),
                            );
                        }
                        if ui
                            .add(egui::Button::image(icons::ICONS.image(crate::icons::Icon::WindowMinimize)))
                            .on_hover_text("Minimize")
                            .clicked()
                        {
                            ctx.send_viewport_cmd(
                                ViewportCommand::Minimized(true),
                            );
                        }

                        // Everything left of the buttons is drag-to-move.
                        let avail = ui.available_size_before_wrap();
                        let (rect, resp) = ui.allocate_exact_size(
                            avail,
                            egui::Sense::click_and_drag(),
                        );
                        let _ = rect;
                        if resp.drag_started() {
                            ctx.send_viewport_cmd(ViewportCommand::StartDrag);
                        }
                    },
                );
            });
        });
    }
}
