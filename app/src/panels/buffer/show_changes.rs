//! Right-click "Show changes" context menu on the editor toolbar's diff
//! toggle. Per `editor-show-changes-menu` in `editor.md`.
//!
//! Lists recent `changes.db` rows for the active buffer's path and opens
//! the selected row as a snapshot-preview tab with diff mode on. Sibling
//! to the "Diff against on-disk" verb on the same context menu.

use crate::state::AppState;
use crate::tab::{BufferSource, DiffSource, Tab, TabKind};

use eframe::egui;

/// Right-click context menu on the diff toggle. Lists the available diff
/// sources: plain disk diff, plus a "Show changes…" submenu of recent
/// `changes.db` rows.
pub fn show_diff_source_menu(ui: &mut egui::Ui, app: &mut AppState, path: &str) {
    if ui.button("Diff against on-disk").clicked() {
        super::open_diff_vs_disk(app, path);
        ui.close();
    }
    ui.separator();
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
            let label = format_change_row_label(&row);
            if ui.button(label).clicked() {
                open_snapshot_for_change(app, &row);
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

fn open_snapshot_for_change(app: &mut AppState, row: &hiker_core::changes::ChangeRow) {
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
            kind: TabKind::Editor {
                buffer: BufferSource::Snapshot {
                    path: row_path.clone(),
                    change_id: change_id.clone(),
                },
                diff: Some(DiffSource::ChangesDb { change_id, path: row_path }),
            },
            sticky: true,
        });
        app.session.active_tab = Some(id);
    }
}

fn format_change_row_label(row: &hiker_core::changes::ChangeRow) -> String {
    let ts = fmt_ms_short(row.timestamp_ms);
    let op = format!("{:?}", row.op).to_lowercase();
    let author = if row.author.is_empty() {
        "\u{2014}".to_string()
    } else {
        row.author.clone()
    };
    format!("{}  \u{00b7}  {}  \u{00b7}  {}", ts, op, author)
}

fn fmt_ms_short(ms: i64) -> String {
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
}
