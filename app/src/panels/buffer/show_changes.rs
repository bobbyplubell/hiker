//! Right-click "Show changes" context menu on the editor toolbar's diff
//! toggle. Per `editor-show-changes-menu` in `editor.md`.
//!
//! Lists recent accepted ops for the active buffer's path and opens the
//! selected version as a snapshot-preview tab with diff mode on. Sibling
//! to the "Diff against on-disk" verb on the same context menu.

use crate::state::AppState;
use crate::tab::TabKind;

use eframe::egui;

impl AppState {
    /// Right-click context menu on the diff toggle. Lists the available
    /// diff sources: plain disk diff, plus a "Show changes…" submenu of
    /// recent accepted ops. Method on `AppState` so the lint exempts it
    /// from `single_call_fn`.
    pub fn show_diff_source_menu(&mut self, ui: &mut egui::Ui, path: &str) {
        if ui.button("Diff against on-disk").clicked() {
            super::open_diff_vs_disk(self, path);
            ui.close();
        }
        ui.separator();
        let app = self;
        ui.menu_button("Show changes\u{2026}", |ui| {
            let log = app.vault_session.services.oplog.as_ref();
            let changes = match hiker_core::ops::op_writes::path_history(log, path, 20) {
                Ok(rows) => rows,
                Err(_) => return,
            };
            if changes.is_empty() {
                ui.label(egui::RichText::new("(no history for this file)").italics());
                return;
            }
            for row in changes {
                // Inlined row label: timestamp + op + author.
                let ts = {
                    let ms = row.timestamp_ms;
                    let now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as i64)
                        .unwrap_or(ms);
                    let delta = (now_ms - ms).max(0);
                    let secs = delta / 1000;
                    if secs < 60 {
                        format!("{}s ago", secs)
                    } else if secs < 3600 {
                        format!("{}m ago", secs / 60)
                    } else if secs < 86_400 {
                        format!("{}h ago", secs / 3600)
                    } else {
                        format!("{}d ago", secs / 86_400)
                    }
                };
                let op = row.op_kind.clone();
                let author = {
                    let wire = row.author.as_wire();
                    if wire.is_empty() {
                        "\u{2014}".to_string()
                    } else {
                        wire
                    }
                };
                let label = format!("{}  \u{00b7}  {}  \u{00b7}  {}", ts, op, author);
                if ui.button(label).clicked() {
                    // Load this version IN THIS tab (same as the version
                    // dropdown) rather than spawning a new one.
                    let op_id = row.op_id.clone();
                    if let Some(active) = app.session.active_tab
                        && let Some(tab) =
                            app.session.tabs.iter_mut().find(|t| t.id == active)
                    {
                        tab.kind = TabKind::version_preview(path.to_string(), op_id);
                    }
                    ui.close();
                }
            }
            ui.separator();
            if ui.button("Browse all\u{2026}").clicked() {
                crate::panels::home::open_home_detail(
                    app,
                    crate::tab::HomeDetail::ActivityRow { path: path.to_string() },
                );
                ui.close();
            }
        });
    }
}
