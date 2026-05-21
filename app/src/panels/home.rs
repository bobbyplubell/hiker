//! Home tab — vault dashboard. Shows top-level counts, a snapshots
//! browser, and a one-click jump into the unified Changes feed (which
//! now owns the recent-activity surface the home overview used to
//! render inline).
#![allow(clippy::items_after_test_module)]

use eframe::egui;

use crate::editor_pane;
use crate::state::AppState;
use crate::tab::{HomeDetail, Tab, TabKind};
use crate::theme;

pub fn show(ui: &mut egui::Ui, app: &mut AppState) {
    ui.heading("Home");
    ui.add_space(4.0);
    ui.label(
        egui::RichText::new(format!("Vault: {}", app.vault_session.vault_root.display()))
            .color(theme::muted()),
    );
    ui.add_space(16.0);

    // Stats grid: live counts from indexer + a cheap walk for note count.
    let total_notes = note_count(app);
    let (model_ready, queued, indexed) = {
        let s = app.vault_session.services.indexer.status();
        (s.model_ready, s.queued, s.total_notes)
    };
    egui::Grid::new("home-stats")
        .num_columns(2)
        .spacing(egui::vec2(24.0, 6.0))
        .show(ui, |ui| {
            ui.label("Notes on disk:");
            ui.label(format!("{}", total_notes));
            ui.end_row();
            ui.label("Indexed:");
            ui.label(format!("{}", indexed));
            ui.end_row();
            ui.label("Queued for index:");
            ui.label(format!("{}", queued));
            ui.end_row();
            ui.label("Embedder:");
            ui.label(if model_ready { "Ready" } else { "Loading…" });
            ui.end_row();
        });

    ui.add_space(20.0);

    // Recent activity used to render inline here; it's now the
    // dedicated Changes tab (one filterable surface for staged +
    // committed history). The home page links to it instead of
    // duplicating the rows.
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Activity").strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("Open Changes →").clicked() {
                open_singleton(app, TabKind::Changes);
            }
        });
    });
    ui.label(
        egui::RichText::new(
            "Pending agent proposals + committed history live in the Changes tab. Filter by author, source, and op.",
        )
        .color(theme::muted())
        .small(),
    );

    ui.add_space(16.0);
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Snapshots").strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("See all").clicked() {
                open_home_detail(app, HomeDetail::Snapshots);
            }
        });
    });
    render_snapshots(ui, app, 5);
}

fn open_singleton(app: &mut AppState, kind: TabKind) {
    if let Some(existing) = app
        .session
        .tabs
        .iter()
        .find(|t| std::mem::discriminant(&t.kind) == std::mem::discriminant(&kind))
    {
        app.session.active_tab = Some(existing.id);
        return;
    }
    let id = app.next_tab_id();
    app.session.tabs.push(Tab {
        id,
        kind,
        sticky: true,
    });
    app.session.active_tab = Some(id);
}

pub fn show_detail(ui: &mut egui::Ui, app: &mut AppState, which: &HomeDetail) {
    match which {
        HomeDetail::Snapshots => {
            ui.heading("Snapshots");
            ui.add_space(8.0);
            render_snapshots(ui, app, 200);
        }
        HomeDetail::ActivityRow { path } => {
            ui.heading(format!("History · {path}"));
            ui.add_space(8.0);
            render_path_history(ui, app, path);
        }
    }
}

/// Render every changes-log entry touching `path`, newest-first. Each
/// row shows timestamp, author, action, and the content hash so the
/// user can see what changed between versions.
fn render_path_history(ui: &mut egui::Ui, app: &mut AppState, path: &str) {
    let changes = app.vault_session.services.changes.clone();
    let rows = match changes.history_for_path(path, 200) {
        Ok(v) => v,
        Err(err) => {
            ui.label(
                egui::RichText::new(format!("history error: {err}"))
                    .color(egui::Color32::RED)
                    .small(),
            );
            return;
        }
    };
    if rows.is_empty() {
        ui.label(
            egui::RichText::new("(no recorded versions)")
                .color(theme::muted())
                .small(),
        );
        return;
    }
    ui.horizontal(|ui| {
        if ui.button("Open note").clicked() {
            editor_pane::open_file(app, path, /* sticky */ true);
        }
        ui.label(
            egui::RichText::new(format!("{} versions", rows.len()))
                .color(theme::muted())
                .small(),
        );
    });
    ui.add_space(4.0);
    egui::ScrollArea::vertical()
        .id_salt(("home-activity-history", path.to_string()))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for row in &rows {
                egui::Frame::default()
                    .fill(theme::active_bg())
                    .inner_margin(egui::Margin::symmetric(6, 4))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(format_timestamp(row.timestamp_ms))
                                    .small()
                                    .monospace(),
                            );
                            ui.label(
                                egui::RichText::new(&row.author)
                                    .small()
                                    .color(theme::muted()),
                            );
                            ui.label(
                                egui::RichText::new(format!("{:?}", row.op))
                                    .small()
                                    .strong(),
                            );
                            if row.is_current {
                                ui.label(
                                    egui::RichText::new("current")
                                        .small()
                                        .color(theme::accent()),
                                );
                            }
                        });
                        let hash = row
                            .content_hash
                            .as_deref()
                            .unwrap_or("(no hash)");
                        let short = &hash[..hash.len().min(12)];
                        ui.label(
                            egui::RichText::new(short)
                                .small()
                                .monospace()
                                .color(theme::muted()),
                        );
                    });
                ui.add_space(2.0);
            }
        });
}

/// Count vault notes by walking the indexable set. Cheap for thousands of
/// files; for huge vaults this could be cached.
fn note_count(app: &AppState) -> usize {
    app.vault_session.vault.walk_indexable_files("").map(|v| v.len()).unwrap_or(0)
}

fn render_snapshots(ui: &mut egui::Ui, app: &mut AppState, limit: usize) {
    let changes = app.vault_session.services.changes.clone();
    let rows = match changes.recent(limit) {
        Ok(v) => v,
        Err(err) => {
            ui.label(
                egui::RichText::new(format!("changes error: {}", err))
                    .color(egui::Color32::RED)
                    .small(),
            );
            return;
        }
    };
    if rows.is_empty() {
        ui.label(
            egui::RichText::new("(no snapshots yet)")
                .color(theme::muted())
                .small(),
        );
        return;
    }
    egui::ScrollArea::vertical()
        .id_salt("home-snapshots")
        .auto_shrink([false, true])
        .max_height(220.0)
        .show(ui, |ui| {
            for row in rows {
                let ts = format_timestamp(row.timestamp_ms);
                let label = format!("{}  #{}  {}", ts, row.id, row.path);
                if ui
                    .selectable_label(false, label)
                    .on_hover_text("Open snapshot preview")
                    .clicked()
                {
                    open_snapshot(app, &row.path, row.id);
                }
            }
        });
}

fn open_snapshot(app: &mut AppState, path: &str, change_id: i64) {
    use crate::tab::{Tab, TabKind};
    let id = app.next_tab_id();
    app.session.tabs.push(Tab {
        id,
        kind: TabKind::snapshot_preview(path.to_string(), change_id.to_string()),
        sticky: true,
    });
    app.session.active_tab = Some(id);
}

/// Roll a file back to the content of the most recent change before
/// `change_id`. Mirrors the legacy `rollback_change` command: pulls
/// the prior bytes from `changes.previous_content_for_path`, writes them
/// to disk via the drift-aware `write_file_checked`, and appends a new
/// change row stamped with `rolled_back_from`.
pub(crate) fn rollback_change(app: &mut AppState, path: &str, change_id: i64) {
    use crate::state::ToastLevel;
    let changes = app.vault_session.services.changes.clone();
    let prior = match changes.previous_content_for_path(path, change_id) {
        Ok(Some(p)) => p,
        Ok(None) => {
            app.push_toast(
                format!("No earlier version of {} on record", path),
                ToastLevel::Error,
            );
            return;
        }
        Err(err) => {
            app.push_toast(format!("Rollback lookup failed: {err}"), ToastLevel::Error);
            return;
        }
    };
    let prior_content = match String::from_utf8(prior.1) {
        Ok(s) => s,
        Err(err) => {
            app.push_toast(format!("Rollback target not UTF-8: {err}"), ToastLevel::Error);
            return;
        }
    };
    let current_hash = match app.vault_session.vault.read_file(path) {
        Ok(text) => hiker_core::hash_str(&text),
        Err(_) => String::new(),
    };
    let new_hash = match app.vault_session.vault.write_file_checked(path, &current_hash, &prior_content) {
        Ok(h) => h,
        Err(err) => {
            app.push_toast(format!("Rollback write failed: {err}"), ToastLevel::Error);
            return;
        }
    };
    if let Err(err) = changes.append(hiker_core::changes::ChangeAppend {
        path,
        op: hiker_core::changes::ChangeOp::Modified,
        author: "user",
        content_hash: Some(&new_hash),
        content: Some(prior_content.as_bytes()),
        rename_from: None,
        metadata: serde_json::json!({"rolled_back_from": change_id}),
    }) {
        tracing::warn!(error = %err, "rollback: append changes row failed");
    }
    app.push_toast(format!("Rolled back {}", path), ToastLevel::Info);
}

pub(crate) fn open_home_detail(app: &mut AppState, which: HomeDetail) {
    use crate::tab::{Tab, TabKind};
    // De-duplicate: focus an existing detail tab if it matches.
    if let Some(existing) = app.session.tabs.iter().find(|t| {
        matches!(&t.kind, TabKind::HomeDetail { which: w } if std::mem::discriminant(w) == std::mem::discriminant(&which))
    }) {
        app.session.active_tab = Some(existing.id);
        return;
    }
    let id = app.next_tab_id();
    app.session.tabs.push(Tab {
        id,
        kind: TabKind::HomeDetail { which },
        sticky: true,
    });
    app.session.active_tab = Some(id);
}

/// Format a unix-millis timestamp as a compact local-time string.
fn format_timestamp(ms: i64) -> String {
    use time::OffsetDateTime;
    let secs = ms.div_euclid(1000);
    let nanos = (ms.rem_euclid(1000) * 1_000_000) as u32;
    let Ok(dt) = OffsetDateTime::from_unix_timestamp(secs) else {
        return String::from("?");
    };
    let dt = dt.replace_nanosecond(nanos).unwrap_or(dt);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        dt.year(),
        dt.month() as u8,
        dt.day(),
        dt.hour(),
        dt.minute(),
    )
}
