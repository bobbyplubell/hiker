//! Home tab — vault dashboard. Shows top-level counts, a snapshots
//! browser, and a one-click jump into the unified Changes feed (which
//! now owns the recent-activity surface the home overview used to
//! render inline).
#![allow(clippy::items_after_test_module)]

use std::time::{Duration, Instant};

use eframe::egui;

use hiker_core::activity::ChangeRow;

use crate::editor_pane;
use crate::state::AppState;
use crate::tab::{HomeDetail, Tab, TabKind};
use hiker_theme as theme;

/// How long a cached snapshot-feed read stays fresh. The feed is a SQLite
/// metadata query under the vault-wide op-log lock; running it every frame
/// (the home tab repaints continuously) was a visible lag source. Snapshots
/// only change on accept / commit / rollback, so a sub-second refresh is
/// imperceptible while cutting ~59 of every 60 queries.
const SNAPSHOT_REFRESH: Duration = Duration::from_millis(750);

/// Per-tab local state for the Home surface. Currently just the throttled
/// snapshot-feed cache (see [`SNAPSHOT_REFRESH`]).
#[derive(Default)]
pub struct State {
    snapshots: Option<SnapshotCache>,
}

struct SnapshotCache {
    limit: usize,
    fetched_at: Instant,
    rows: Vec<ChangeRow>,
}

impl State {
    /// Invalidate the cache so the next frame re-reads the feed — call after an
    /// action that changes the snapshot list (e.g. a rollback).
    pub fn invalidate_snapshots(&mut self) {
        self.snapshots = None;
    }
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

    // Recent activity used to render inline here; it's now the
    // dedicated Changes tab (one filterable surface for staged +
    // committed history). The home page links to it instead of
    // duplicating the rows.
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Activity").strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("Open Changes →").clicked() {
                app.open_singleton(TabKind::Changes);
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
        ui.label(egui::RichText::new("Version history").strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("See all").clicked() {
                open_home_detail(app, HomeDetail::VersionHistory);
            }
        });
    });
    render_snapshots(ui, app, 5);
}

impl AppState {
    fn open_singleton(&mut self, kind: TabKind) {
        if let Some(existing) = self
            .session
            .tabs
            .iter()
            .find(|t| std::mem::discriminant(&t.kind) == std::mem::discriminant(&kind))
        {
            self.session.active_tab = Some(existing.id);
            return;
        }
        let id = self.next_tab_id();
        self.session.tabs.push(Tab {
            id,
            kind,
            sticky: true,
        });
        self.session.active_tab = Some(id);
    }
}

pub fn show_detail(ui: &mut egui::Ui, app: &mut AppState, which: &HomeDetail) {
    match which {
        HomeDetail::VersionHistory => {
            ui.heading("Version history");
            ui.add_space(8.0);
            render_snapshots(ui, app, 200);
        }
        HomeDetail::ActivityRow { path } => {
            ui.heading(format!("History · {path}"));
            ui.add_space(8.0);
            app.render_path_history(ui, path);
        }
    }
}

impl AppState {
    /// Render every accepted op touching `path`, newest-first. Each row
    /// shows timestamp, author, and action so the user can see what
    /// changed between versions.
    fn render_path_history(&mut self, ui: &mut egui::Ui, path: &str) {
    let app = self;
    let log = app.vault_session.services.oplog.clone();
    let rows = match hiker_core::ops::op_writes::path_history(log.as_ref(), path, 200) {
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
            for (i, row) in rows.iter().enumerate() {
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
                                egui::RichText::new(row.author.as_wire())
                                    .small()
                                    .color(theme::muted()),
                            );
                            ui.label(
                                egui::RichText::new(&row.op_kind)
                                    .small()
                                    .strong(),
                            );
                            // Newest-first: index 0 is the on-disk version.
                            if i == 0 {
                                ui.label(
                                    egui::RichText::new("current")
                                        .small()
                                        .color(theme::accent()),
                                );
                            }
                        });
                        let short = &row.op_id[..row.op_id.len().min(12)];
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
}

/// Read the snapshot feed through the throttled cache: re-query only when the
/// cache is empty, the requested `limit` changed, or it has gone stale (see
/// [`SNAPSHOT_REFRESH`]). Returns owned rows (a cheap clone of a small list)
/// so the caller can render + dispatch `&mut app` clicks without holding the
/// panel-state borrow.
fn snapshot_rows(app: &mut AppState, limit: usize) -> Result<Vec<ChangeRow>, String> {
    let now = Instant::now();
    let fresh = app.panels.home.snapshots.as_ref().is_some_and(|c| {
        c.limit == limit && now.duration_since(c.fetched_at) < SNAPSHOT_REFRESH
    });
    if !fresh {
        let feed = hiker_core::activity::AcceptedFeed::new(&app.vault_session.services.oplog);
        let rows = feed.recent(limit).map_err(|e| e.to_string())?;
        app.panels.home.snapshots = Some(SnapshotCache { limit, fetched_at: now, rows });
    }
    Ok(app.panels.home.snapshots.as_ref().map(|c| c.rows.clone()).unwrap_or_default())
}

fn render_snapshots(ui: &mut egui::Ui, app: &mut AppState, limit: usize) {
    let rows = match snapshot_rows(app, limit) {
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
            egui::RichText::new("(no versions yet)")
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
                // The op-log projection carries the ulid `op_id` in
                // `metadata` (the `id` field holds `timestamp_ms`).
                let Some(op_id) = row
                    .metadata
                    .get("op_id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                else {
                    continue;
                };
                let ts = format_timestamp(row.timestamp_ms);
                let short = &op_id[..op_id.len().min(8)];
                let label = format!("{}  {}  {}", ts, short, row.path);
                if ui
                    .selectable_label(false, label)
                    .on_hover_text("Open version preview")
                    .clicked()
                {
                    app.open_version(&row.path, &op_id);
                }
            }
        });
}

impl AppState {
    fn open_version(&mut self, path: &str, op_id: &str) {
        use crate::tab::{Tab, TabKind};
        let id = self.next_tab_id();
        self.session.tabs.push(Tab {
            id,
            kind: TabKind::version_preview(path.to_string(), op_id.to_string()),
            sticky: true,
        });
        self.session.active_tab = Some(id);
    }
}

/// Roll a file back to the content of its previous accepted version.
/// Pulls the prior content from `previous_accepted_content` and writes it
/// through the op log via `user_save` — a fresh `user` op that becomes the
/// newest accepted version (the original op stays in the log).
impl AppState {
    pub(crate) fn rollback_change(&mut self, path: &str) {
    use crate::state::ToastLevel;
    let app = self;
    let log = app.vault_session.services.oplog.clone();
    let prior =
        match hiker_core::ops::op_writes::previous_accepted_content(log.as_ref(), path) {
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
