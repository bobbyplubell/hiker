//! Right-click "Show changes" context menu on the editor toolbar's diff
//! toggle. Per `editor-show-changes-menu` in `editor.md`.
//!
//! Lists recent accepted ops for the active buffer's path and opens the
//! selected version as a snapshot-preview tab with diff mode on. Sibling
//! to the "Diff against on-disk" verb on the same context menu.

use crate::state::AppState;
use crate::tab::TabKind;

use eframe::egui;

/// Verbs from the diff-source context menu (status: ctxmenu-diff-source).
enum DiffSourceVerb {
    /// Open the user diff vs the on-disk file.
    DiffVsDisk,
    /// Load a specific recent version (op id) into the active tab.
    ShowVersion(String),
    /// Open the full history browser for this path.
    BrowseAll,
}

/// One recent-op row in the "Show changes…" submenu: a display label and the
/// op id it loads. Gathered when the menu opens so the submenu is plain data.
struct HistoryRow {
    label: String,
    op_id: String,
}

/// Build the diff-source menu for `path` (status: ctxmenu-diff-source). The
/// "Diff against on-disk" verb sits beside a dynamic "Show changes…" submenu of
/// recent accepted ops (plus "Browse all…"). `history` is gathered at open time
/// so the submenu is pure data; an empty/erroring history renders a disabled note.
fn build_diff_source_menu(
    history: Result<Vec<HistoryRow>, ()>,
) -> egui_workbench::menu::Menu<DiffSourceVerb> {
    let mut submenu = egui_workbench::menu::Menu::new();
    match history {
        Ok(rows) if !rows.is_empty() => {
            for row in rows {
                submenu = submenu.action(row.label, DiffSourceVerb::ShowVersion(row.op_id));
            }
            submenu = submenu
                .section()
                .action("Browse all\u{2026}", DiffSourceVerb::BrowseAll);
        }
        // No history (or load error): the prior code returned early, showing
        // just the "(no history for this file)" note with no Browse-all row.
        _ => {
            submenu = submenu.action_with(
                egui_workbench::menu::Action::new("(no history for this file)", DiffSourceVerb::BrowseAll)
                    .enabled(egui_workbench::menu::Enabled::No(std::borrow::Cow::Borrowed(""))),
            );
        }
    }
    egui_workbench::menu::Menu::new()
        .action("Diff against on-disk", DiffSourceVerb::DiffVsDisk)
        .section()
        .submenu("Show changes\u{2026}", submenu)
}

/// A short "Xs/m/h/d ago" relative timestamp for a recent op row.
fn relative_ts(ms: i64) -> String {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(ms);
    let secs = (now_ms - ms).max(0) / 1000;
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

impl AppState {
    /// Right-click context menu on the diff toggle. Lists the available
    /// diff sources: plain disk diff, plus a "Show changes…" submenu of
    /// recent accepted ops. Method on `AppState` so the lint exempts it
    /// from `single_call_fn`.
    pub fn show_diff_source_menu(&mut self, ui: &mut egui::Ui, path: &str) {
        // Gather recent ops at open time so the submenu is plain data.
        let history: Result<Vec<HistoryRow>, ()> = {
            let log = self.vault_session.services.oplog.as_ref();
            hiker_core::ops::op_writes::path_history(log, path, 20)
                .map_err(|_| ())
                .map(|rows| {
                    rows.into_iter()
                        .map(|row| {
                            let author = {
                                let wire = row.author.as_wire();
                                if wire.is_empty() {
                                    "\u{2014}".to_string()
                                } else {
                                    wire
                                }
                            };
                            HistoryRow {
                                label: format!(
                                    "{}  \u{00b7}  {}  \u{00b7}  {}",
                                    relative_ts(row.timestamp_ms),
                                    row.op_kind,
                                    author
                                ),
                                op_id: row.op_id.clone(),
                            }
                        })
                        .collect()
                })
        };
        // `show` already runs inside the `context_menu` closure passed by the
        // caller, so render straight into `ui`.
        match egui_workbench::menu::show(ui, build_diff_source_menu(history)) {
            Some(DiffSourceVerb::DiffVsDisk) => super::open_diff_vs_disk(self, path),
            Some(DiffSourceVerb::ShowVersion(op_id)) => {
                // Load this version IN THIS tab (same as the version dropdown)
                // rather than spawning a new one.
                if let Some(active) = self.session.active_tab
                    && let Some(tab) = self.session.tabs.iter_mut().find(|t| t.id == active)
                {
                    tab.kind = TabKind::version_preview(path.to_string(), op_id);
                }
            }
            Some(DiffSourceVerb::BrowseAll) => {
                crate::panels::home::open_home_detail(
                    self,
                    crate::tab::HomeDetail::ActivityRow { path: path.to_string() },
                );
            }
            None => {}
        }
    }
}
