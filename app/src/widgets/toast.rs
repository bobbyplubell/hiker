//! Bottom-right toast stack. Auto-dismiss after a short window; an Undo
//! button reactivates the optional `UndoSpec::action`.

use std::time::{Duration, Instant};

use eframe::egui;

use crate::state::{AppState, ToastLevel};

const TOAST_TTL: Duration = Duration::from_secs(5);

impl AppState {
    pub fn toast_overlay(&mut self, ctx: &egui::Context) {
    let app = self;
    let now = Instant::now();
    app.toasts
        .retain(|t| now.saturating_duration_since(t.created_at) < TOAST_TTL);

    if app.toasts.is_empty() {
        return;
    }

    // Drain toasts that the user clicks Undo on (collected by index after
    // rendering since we can't mutate during the immutable iter).
    let mut undo_idx: Option<usize> = None;

    egui::Area::new(egui::Id::new("toast-area"))
        .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-16.0, -16.0))
        .order(egui::Order::Foreground)
        .show(ctx, |ui| {
            ui.vertical(|ui| {
                for (i, toast) in app.toasts.iter().enumerate() {
                    let (bg, fg) = match toast.level {
                        ToastLevel::Info => (
                            egui::Color32::from_rgb(0xe7, 0xf0, 0xff),
                            egui::Color32::from_rgb(0x1c, 0x33, 0x59),
                        ),
                        ToastLevel::Warn => (
                            egui::Color32::from_rgb(0xff, 0xf4, 0xd6),
                            egui::Color32::from_rgb(0x6e, 0x4a, 0x07),
                        ),
                        ToastLevel::Error => (
                            egui::Color32::from_rgb(0xff, 0xe4, 0xe4),
                            egui::Color32::from_rgb(0x80, 0x21, 0x21),
                        ),
                    };
                    egui::Frame::default()
                        .fill(bg)
                        .stroke(egui::Stroke::new(1.0, crate::theme::divider()))
                        .corner_radius(4.0)
                        .inner_margin(egui::Margin::symmetric(10, 6))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(&toast.message).color(fg));
                                if let Some(undo) = &toast.undo
                                    && ui.button(&undo.label).clicked()
                                {
                                    undo_idx = Some(i);
                                }
                            });
                        });
                    ui.add_space(4.0);
                }
            });
        });

    if let Some(i) = undo_idx
        && i < app.toasts.len()
    {
        let toast = app.toasts.remove(i);
        if let Some(undo) = toast.undo {
            (undo.action)(app);
        }
    }
    }
}
