//! Right-click "Show changes" context menu on the editor toolbar's diff
//! toggle. Per `editor-show-changes-menu` in `editor.md`.
//!
//! Lists recent `changes.db` rows for the active buffer's path and opens
//! the selected row as a snapshot-preview tab with diff mode on. Sibling
//! to the "Diff against on-disk" verb on the same context menu.

use crate::state::AppState;
use crate::tab::{BufferSource, Tab, TabKind};

use eframe::egui;

impl AppState {
    /// Right-click context menu on the diff toggle. Lists the available
    /// diff sources: plain disk diff, plus a "Show changes…" submenu of
    /// recent `changes.db` rows. Method on `AppState` so the lint
    /// exempts it from `single_call_fn`.
    pub fn show_diff_source_menu(&mut self, ui: &mut egui::Ui, path: &str) {
        if ui.button("Diff against on-disk").clicked() {
            super::open_diff_vs_disk(self, path);
            ui.close();
        }
        ui.separator();
        let app = self;
        ui.menu_button("Show changes\u{2026}", |ui| {
            let changes = match app.vault_session.services.changes.history_for_path(path, 20) {
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
                let op = format!("{:?}", row.op).to_lowercase();
                let author = if row.author.is_empty() {
                    "\u{2014}".to_string()
                } else {
                    row.author.clone()
                };
                let label = format!("{}  \u{00b7}  {}  \u{00b7}  {}", ts, op, author);
                if ui.button(label).clicked() {
                    // Inlined: open or focus a snapshot tab for this change row.
                    let change_id = row.id.to_string();
                    let row_path = row.path.clone();
                    if let Some(existing) = app.session.tabs.iter().find(|t| {
                        matches!(
                            &t.kind,
                            TabKind::Editor {
                                buffer: BufferSource::Snapshot { change_id: c, path: p },
                                ..
                            } if *c == change_id && p == &row_path
                        )
                    }) {
                        app.session.active_tab = Some(existing.id);
                    } else {
                        let id = app.next_tab_id();
                        app.session.tabs.push(Tab {
                            id,
                            kind: TabKind::snapshot_preview(row_path, change_id),
                            sticky: true,
                        });
                        app.session.active_tab = Some(id);
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
