//! Agent-changes feed: the unified activity feed filtered to changes
//! authored by agents (`author LIKE 'agent:%'`). Each row offers an
//! "Open" action (focus the note in the editor) and, when the change
//! row has stored content, a "View diff" action (open the historical
//! snapshot diff tab against the change id).

use eframe::egui;

use hiker_core::activity::{ActivityFilter, ActivityPayload, ActivitySource};

use crate::editor_pane;
use crate::state::AppState;
use crate::tab::{Tab, TabKind};
use crate::theme;

pub fn show(ui: &mut egui::Ui, app: &mut AppState) {
    ui.heading("Agent changes");
    ui.add_space(4.0);

    let activity = app.vault_session.services.activity.clone();

    let filter = ActivityFilter {
        source: ActivitySource::Merged,
        limit: 200,
        author_pattern: Some("agent:%".into()),
        since_ms: None,
    };

    let rows = match activity.list(filter) {
        Ok(r) => r,
        Err(err) => {
            ui.label(
                egui::RichText::new(format!("Failed to load activity: {err}"))
                    .color(egui::Color32::from_rgb(200, 60, 60)),
            );
            return;
        }
    };

    if rows.is_empty() {
        ui.label(
            egui::RichText::new("(no agent changes recorded yet)")
                .color(theme::muted())
                .italics(),
        );
        return;
    }

    // Deferred actions so we don't keep `&app` borrows across the loop.
    enum Action {
        Open(String),
        Diff { path: String, change_id: String },
    }
    let mut pending: Option<Action> = None;

    egui::ScrollArea::vertical()
        .id_salt("agent-changes-scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for row in &rows {
                let ts = format_ts_ms(row.timestamp_ms);
                let summary = match &row.summary {
                    hiker_core::activity::ActivitySummary::Change { op } => {
                        format!("{:?}", op).to_lowercase()
                    }
                    hiker_core::activity::ActivitySummary::Staging { surface, action } => {
                        format!("{surface}/{action}")
                    }
                };
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(ts).color(theme::muted()).small(),
                    );
                    ui.label(
                        egui::RichText::new(&row.author)
                            .color(theme::muted())
                            .small()
                            .monospace(),
                    );
                    ui.label(egui::RichText::new(&row.path).small());
                    ui.label(
                        egui::RichText::new(summary)
                            .color(theme::muted())
                            .small(),
                    );
                    if ui.small_button("Open").clicked() {
                        pending = Some(Action::Open(row.path.clone()));
                    }
                    if let ActivityPayload::Change(c) = &row.payload {
                        if c.content_hash.is_some()
                            && ui.small_button("View diff").clicked()
                        {
                            pending = Some(Action::Diff {
                                path: row.path.clone(),
                                change_id: c.id.to_string(),
                            });
                        }
                    }
                });
            }
        });

    match pending {
        Some(Action::Open(path)) => {
            editor_pane::open_file(app, &path, /* sticky */ true);
        }
        Some(Action::Diff { path, change_id }) => {
            // Reuse an existing SnapshotPreview tab for the same
            // (path, change_id) pair if it's already open.
            if let Some(existing) = app.session.tabs.iter().find(|t| matches!(
                &t.kind,
                TabKind::SnapshotPreview { path: p, change_id: c }
                    if p == &path && c == &change_id
            )) {
                app.session.active_tab = Some(existing.id);
                return;
            }
            let id = app.next_tab_id();
            app.session.tabs.push(Tab {
                id,
                kind: TabKind::SnapshotPreview { path, change_id },
                sticky: true,
            });
            app.session.active_tab = Some(id);
        }
        None => {}
    }
}

fn format_ts_ms(ms: i64) -> String {
    use time::OffsetDateTime;
    use time::macros::format_description;
    let secs = ms / 1000;
    let Ok(t) = OffsetDateTime::from_unix_timestamp(secs) else {
        return String::new();
    };
    let fmt = format_description!("[year]-[month]-[day] [hour]:[minute]");
    t.format(fmt).unwrap_or_default()
}
