//! Plugins host. Reads `<vault>/.hiker/plugins.json` and offers
//! Enable/Disable/Remove actions that round-trip back to the manifest.
//! A future host runtime will pick up the `enabled` flag and load/
//! unload plugins; today the manifest is the source of truth and the
//! UI just edits it.

use std::path::PathBuf;

use eframe::egui;
use serde::{Deserialize, Serialize};

use crate::state::{AppState, ToastLevel};
use crate::theme;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PluginsFile {
    #[serde(default)]
    plugins: Vec<PluginEntry>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PluginEntry {
    id: String,
    name: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    description: String,
}

fn manifest_path(app: &AppState) -> PathBuf {
    app.vault_session.vault_root.join(".hiker/plugins.json")
}

fn load(app: &AppState) -> PluginsFile {
    std::fs::read(manifest_path(app))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save(app: &AppState, file: &PluginsFile) -> std::io::Result<()> {
    let path = manifest_path(app);
    if let Some(parent) = path.parent()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent)?;
    }
    let bytes =
        serde_json::to_vec_pretty(file).unwrap_or_else(|_| b"{\"plugins\":[]}".to_vec());
    std::fs::write(path, bytes)
}

pub fn show(ui: &mut egui::Ui, app: &mut AppState) {
    ui.heading("Plugins");
    ui.add_space(4.0);

    let mut file = load(app);
    let manifest = manifest_path(app);
    ui.label(
        egui::RichText::new(format!("Manifest: {}", manifest.display()))
            .color(theme::muted())
            .small(),
    );
    ui.add_space(6.0);

    if file.plugins.is_empty() {
        ui.add_space(20.0);
        ui.vertical_centered(|ui| {
            ui.label(
                egui::RichText::new("No plugins installed.").color(theme::muted()),
            );
            ui.add_space(8.0);
            if ui.button("Create empty manifest").clicked()
                && let Err(err) = save(app, &PluginsFile::default())
            {
                app.push_toast(
                    format!("Failed to create manifest: {err}"),
                    ToastLevel::Error,
                );
            }
        });
        return;
    }

    let mut to_remove: Option<usize> = None;
    let mut to_toggle: Option<usize> = None;
    let mut to_reload: Option<usize> = None;

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for (i, p) in file.plugins.iter().enumerate() {
                egui::Frame::default()
                    .fill(theme::active_bg())
                    .inner_margin(egui::Margin::symmetric(8, 6))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let mut enabled = p.enabled;
                            if ui.checkbox(&mut enabled, "").changed() {
                                to_toggle = Some(i);
                            }
                            ui.vertical(|ui| {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "{}  v{}",
                                        p.name, p.version
                                    ))
                                    .strong(),
                                );
                                if !p.description.is_empty() {
                                    ui.label(
                                        egui::RichText::new(&p.description)
                                            .color(theme::muted())
                                            .small(),
                                    );
                                }
                                ui.label(
                                    egui::RichText::new(&p.id)
                                        .color(theme::muted())
                                        .small()
                                        .monospace(),
                                );
                            });
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui.button("Remove").clicked() {
                                        to_remove = Some(i);
                                    }
                                    let toggle_label =
                                        if p.enabled { "Disable" } else { "Enable" };
                                    if ui.button(toggle_label).clicked() {
                                        to_toggle = Some(i);
                                    }
                                    if ui.button("Reload").clicked() {
                                        to_reload = Some(i);
                                    }
                                },
                            );
                        });
                    });
                ui.add_space(4.0);
            }
        });

    if let Some(i) = to_toggle {
        if let Some(p) = file.plugins.get_mut(i) {
            p.enabled = !p.enabled;
            let new_state = if p.enabled { "enabled" } else { "disabled" };
            let name = p.name.clone();
            if let Err(err) = save(app, &file) {
                app.push_toast(
                    format!("Manifest save failed: {err}"),
                    ToastLevel::Error,
                );
            } else {
                app.push_toast(
                    format!("{} {}", name, new_state),
                    ToastLevel::Info,
                );
            }
        }
    } else if let Some(i) = to_remove {
        if i < file.plugins.len() {
            let removed = file.plugins.remove(i);
            if let Err(err) = save(app, &file) {
                app.push_toast(
                    format!("Manifest save failed: {err}"),
                    ToastLevel::Error,
                );
            } else {
                app.push_toast(
                    format!("Removed {}", removed.name),
                    ToastLevel::Info,
                );
            }
        }
    } else if let Some(i) = to_reload {
        // "Reload" in v1 just re-toggles enabled, signalling the future
        // host that this plugin should re-init. With no host the signal
        // is a no-op apart from a manifest re-save and a toast.
        if let Some(p) = file.plugins.get(i) {
            let name = p.name.clone();
            if let Err(err) = save(app, &file) {
                app.push_toast(
                    format!("Manifest save failed: {err}"),
                    ToastLevel::Error,
                );
            } else {
                app.push_toast(
                    format!("Reloaded {} (host runtime: not yet wired)", name),
                    ToastLevel::Info,
                );
            }
        }
    }
}
