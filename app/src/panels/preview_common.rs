//! Shared chrome for read-only preview tabs (snapshot, trash, staging).
//!
//! These three panels all render an identical "banner" header (title +
//! subtitle on the left, action buttons on the right) and share a tiny
//! `close_active` helper. Kept here to avoid drift between the three.

use eframe::egui;

use crate::state::AppState;
use crate::theme;

pub fn banner(
    ui: &mut egui::Ui,
    title: &str,
    subtitle: &str,
    actions: impl FnOnce(&mut egui::Ui),
) {
    egui::Frame::default()
        .fill(theme::active_bg())
        .inner_margin(egui::Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(
                        egui::RichText::new(title)
                            .strong()
                            .color(theme::accent()),
                    );
                    ui.label(
                        egui::RichText::new(subtitle)
                            .color(theme::muted())
                            .small()
                            .monospace(),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    actions(ui);
                });
            });
        });
}

pub fn close_active(app: &mut AppState) {
    if let Some(id) = app.session.active_tab {
        crate::editor_pane::close_tab(app, id);
    }
}
