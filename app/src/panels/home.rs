//! Home tab — vault dashboard. Shows top-level counts. Per-note version
//! history lives in the editor's version dropdown + the "View history for
//! this note" detail page (`HomeDetail::ActivityRow`), both sourced from
//! plain-file snapshots. The old vault-wide cross-note activity/changes
//! feed is retired (the core rework: git-log + per-note snapshot list only).
#![allow(clippy::items_after_test_module)]

use eframe::egui;

use crate::editor_pane;
use crate::state::AppState;
use crate::tab::HomeDetail;
use hiker_theme as theme;

/// Per-tab local state for the Home surface. The vault-wide snapshot-feed
/// cache was retired with the cross-note activity feed; nothing stateful
/// remains, but the type is kept so the panel-state registry wiring is
/// unchanged.
#[derive(Default)]
pub struct State;

impl State {
    /// No-op retained for call-site compatibility: there is no longer a
    /// vault-wide snapshot-feed cache to invalidate.
    pub const fn invalidate_snapshots(&mut self) {}
}

pub fn show(ui: &mut egui::Ui, app: &mut AppState) {
    ui.heading("Home");
    ui.add_space(4.0);
    ui.label(
        egui::RichText::new(format!("Vault: {}", app.vault_session.vault_root.display()))
            .color(theme::muted()),
    );
    ui.add_space(16.0);

    // Stats grid: live counts from indexer + a cheap walk for note count.
    // The walk is cheap for thousands of files; cache for huge vaults.
    let total_notes = app
        .vault_session
        .vault
        .walk_indexable_files("")
        .map(|v| v.len())
        .unwrap_or(0);
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

    ui.label(
        egui::RichText::new(
            "Per-note version history lives in the editor's version dropdown and \
             the right-click \"View history for this note\" page — sourced from \
             plain-file snapshots under .hiker/history/.",
        )
        .color(theme::muted())
        .small(),
    );
}

pub fn show_detail(ui: &mut egui::Ui, app: &mut AppState, which: &HomeDetail) {
    match which {
        HomeDetail::ActivityRow { path } => {
            ui.heading(format!("History · {path}"));
            ui.add_space(8.0);
            app.render_path_history(ui, path);
        }
    }
}

impl AppState {
    /// Render every snapshot version of `path`, newest-first. Each row shows
    /// the snapshot timestamp; clicking opens that version as a read-only
    /// preview diffed against the live file. Sourced from `core::snapshot`.
    fn render_path_history(&mut self, ui: &mut egui::Ui, path: &str) {
    let app = self;
    let log = app.vault_session.services.layered.clone();
    let rows = match hiker_core::ops::op_writes::snapshot_history(log.as_ref(), path, 200) {
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
        // Roll back to the previous snapshot (forward-correct: re-saves the
        // prior content as a new version). Needs at least two snapshots.
        if rows.len() >= 2 && ui.button("Roll back to previous version").clicked() {
            app.rollback_change(path);
        }
        ui.label(
            egui::RichText::new(format!("{} versions", rows.len()))
                .color(theme::muted())
                .small(),
        );
    });
    ui.add_space(4.0);
    let mut open: Option<String> = None;
    egui::ScrollArea::vertical()
        .id_salt(("home-activity-history", path.to_string()))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for (i, row) in rows.iter().enumerate() {
                let badge = if i == 0 { " · current" } else { "" };
                let label = format!("{}{}", format_timestamp(row.timestamp_ms), badge);
                if ui
                    .selectable_label(false, egui::RichText::new(label).small().monospace())
                    .on_hover_text("Open this version (read-only, diffed against the live file)")
                    .clicked()
                {
                    open = Some(row.snapshot_id.clone());
                }
                ui.add_space(2.0);
            }
        });
    if let Some(snapshot_id) = open {
        app.open_version(path, &snapshot_id);
    }
    }
}

impl AppState {
    fn open_version(&mut self, path: &str, snapshot_id: &str) {
        use crate::tab::{Tab, TabKind};
        let id = self.next_tab_id();
        self.session.tabs.push(Tab::new(
            id,
            TabKind::version_preview(path.to_string(), snapshot_id.to_string()),
            true,
        ));
        self.session.active_tab = Some(id);
    }
}

/// Roll a file back to its previous snapshot version. Pulls the prior
/// content from `previous_snapshot_content` and writes it through the op
/// log via `user_save` — a fresh `user` save that becomes the newest
/// version (the prior snapshot stays in the history).
impl AppState {
    pub(crate) fn rollback_change(&mut self, path: &str) {
    use crate::state::ToastLevel;
    let app = self;
    let log = app.vault_session.services.layered.clone();
    let prior =
        match hiker_core::ops::op_writes::previous_snapshot_content(log.as_ref(), path) {
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
    let prior_content = prior.1;
    if let Err(err) = hiker_core::ops::op_writes::user_save(
        log.as_ref(),
        &app.vault_session.vault,
        path,
        &prior_content,
    ) {
        app.push_toast(format!("Rollback write failed: {err}"), ToastLevel::Error);
        return;
    }
    app.push_toast(format!("Rolled back {}", path), ToastLevel::Info);
    // The accepted feed just changed — drop the throttled cache so the
    // snapshots list reflects the rollback on the next frame.
    app.panels.home.invalidate_snapshots();
    }
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
    app.session.tabs.push(Tab::new(id, TabKind::HomeDetail { which }, true));
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
