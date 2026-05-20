//! Raw-TOML fallback view. Shows the per-scope file on disk so users can
//! see what's there (and copy/paste hand edits out). Not editable in-place
//! — the section forms are the canonical write path.

use eframe::egui;

use hiker_core::config::{ConfigPaths, SettingsScope};

use crate::state::AppState;
use crate::theme;

pub fn show(ui: &mut egui::Ui, app: &mut AppState, scope: SettingsScope) {
    egui::CollapsingHeader::new("Raw TOML (read-only)")
        .default_open(false)
        .show(ui, |ui| {
            let paths = ConfigPaths::resolve(&app.vault_session.vault_root);
            let path = match scope {
                SettingsScope::User => paths.user,
                SettingsScope::Vault => Some(paths.vault),
            };
            let Some(path) = path else {
                ui.label("(no platform config dir resolved)");
                return;
            };
            ui.label(
                egui::RichText::new(path.display().to_string())
                    .color(theme::muted())
                    .small()
                    .monospace(),
            );
            let body = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| format!("(read error: {e})"));
            let mut owned = body;
            ui.add(
                egui::TextEdit::multiline(&mut owned)
                    .desired_rows(20)
                    .desired_width(f32::INFINITY)
                    .code_editor()
                    .interactive(false),
            );
        });
}
