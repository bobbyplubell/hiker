//! Unified activity / changes feed: every committed `changes.db` row plus
//! every pending `staging.db` proposal, in one tab with author / source /
//! op filter chips. Supersedes the old `agent_changes` tab (which was a
//! strict subset filtered to `author LIKE 'agent:%'`) and absorbs the
//! home-page "recent activity" widget so there's one canonical view.

use eframe::egui;

use hiker_core::activity::{
    ActivityFilter, ActivityPayload, ActivitySource, ActivitySummary,
};
use hiker_core::changes::ChangeOp;

use crate::editor_pane;
use crate::state::AppState;
use crate::tab::{Tab, TabKind};
use crate::theme;

/// Author-side filter applied on top of the activity feed. `All` skips
/// the predicate; the others narrow by the `author` column on change
/// rows (staging rows match only when `Source::Pending` is in scope —
/// they don't have a meaningful "author" beyond the producer surface).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthorFilter {
    All,
    User,
    Agent,
    Auto,
}

impl AuthorFilter {
    fn pattern(self) -> Option<&'static str> {
        match self {
            AuthorFilter::All => None,
            AuthorFilter::User => Some("user"),
            AuthorFilter::Agent => Some("agent:%"),
            AuthorFilter::Auto => Some("auto:%"),
        }
    }
    fn label(self) -> &'static str {
        match self {
            AuthorFilter::All => "All",
            AuthorFilter::User => "User",
            AuthorFilter::Agent => "Agent",
            AuthorFilter::Auto => "Auto",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceFilter {
    All,
    Committed,
    Pending,
}

impl SourceFilter {
    fn to_activity(self) -> ActivitySource {
        match self {
            SourceFilter::All => ActivitySource::Merged,
            SourceFilter::Committed => ActivitySource::ChangesOnly,
            SourceFilter::Pending => ActivitySource::StagingOnly,
        }
    }
    fn label(self) -> &'static str {
        match self {
            SourceFilter::All => "All",
            SourceFilter::Committed => "Committed",
            SourceFilter::Pending => "Pending",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpFilter {
    All,
    Modified,
    Created,
    Deleted,
    Renamed,
}

impl OpFilter {
    fn matches(self, op: ChangeOp) -> bool {
        match self {
            OpFilter::All => true,
            OpFilter::Modified => op == ChangeOp::Modified,
            OpFilter::Created => op == ChangeOp::Created,
            OpFilter::Deleted => op == ChangeOp::Deleted,
            OpFilter::Renamed => op == ChangeOp::Renamed,
        }
    }
    fn label(self) -> &'static str {
        match self {
            OpFilter::All => "All",
            OpFilter::Modified => "Modified",
            OpFilter::Created => "Created",
            OpFilter::Deleted => "Deleted",
            OpFilter::Renamed => "Renamed",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ChangesFilterState {
    pub author: AuthorFilter,
    pub source: SourceFilter,
    pub op: OpFilter,
}

impl Default for ChangesFilterState {
    fn default() -> Self {
        Self {
            author: AuthorFilter::All,
            source: SourceFilter::All,
            op: OpFilter::All,
        }
    }
}

pub fn show(ui: &mut egui::Ui, app: &mut AppState) {
    ui.heading("Changes");
    ui.add_space(2.0);
    ui.label(
        egui::RichText::new(
            "Every committed edit plus every pending agent proposal. Filter by who, what, and whether it's landed yet.",
        )
        .color(theme::muted())
        .small(),
    );
    ui.add_space(6.0);

    // Pull the persistent filter state out of egui memory and write it
    // back at the end of the frame. Using ctx-data instead of AppState
    // session keeps the tab self-contained (no new field on the giant
    // SessionState struct for a transient UI knob).
    let mem_id = egui::Id::new("changes-tab-filter");
    let mut filter_state: ChangesFilterState = ui
        .ctx()
        .data(|d| d.get_temp(mem_id))
        .unwrap_or_default();

    render_filter_chips(ui, &mut filter_state);
    ui.add_space(6.0);

    let activity = app.vault_session.services.activity.clone();
    let filter = ActivityFilter {
        source: filter_state.source.to_activity(),
        limit: 500,
        author_pattern: filter_state.author.pattern().map(str::to_string),
        since_ms: None,
    };

    let rows = match activity.list(filter) {
        Ok(r) => r,
        Err(err) => {
            ui.colored_label(
                egui::Color32::RED,
                format!("Failed to load activity: {err}"),
            );
            ui.ctx().data_mut(|d| d.insert_temp(mem_id, filter_state));
            return;
        }
    };

    // Op filter runs in Rust since `ActivityFilter` doesn't model it.
    // Cheap — list is bounded at 500 above.
    let op_filtered: Vec<_> = rows
        .into_iter()
        .filter(|row| match &row.summary {
            ActivitySummary::Change { op } => filter_state.op.matches(*op),
            // Staging rows don't have a `ChangeOp` — they show only when
            // the op filter is "All". Any narrower op picker excludes
            // them (you're asking for committed-op-X by definition).
            ActivitySummary::Staging { .. } => matches!(filter_state.op, OpFilter::All),
        })
        .collect();

    if op_filtered.is_empty() {
        ui.label(
            egui::RichText::new("(no activity matches these filters)")
                .color(theme::muted())
                .italics(),
        );
        ui.ctx().data_mut(|d| d.insert_temp(mem_id, filter_state));
        return;
    }

    enum Action {
        Open(String),
        Diff { path: String, change_id: String },
        Inspect(String),
        ViewHistory(String),
        Rollback { path: String, change_id: i64 },
    }
    let mut pending: Option<Action> = None;

    egui::ScrollArea::vertical()
        .id_salt("changes-scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for row in &op_filtered {
                let ts = format_ts_ms(row.timestamp_ms);
                let (summary_text, summary_color) = match &row.summary {
                    ActivitySummary::Change { op } => (
                        format!("{:?}", op).to_lowercase(),
                        op_color(*op),
                    ),
                    ActivitySummary::Staging { surface, action } => (
                        format!("staged · {surface}/{action}"),
                        theme::warn(),
                    ),
                };
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(ts).color(theme::muted()).small(),
                    );
                    ui.label(
                        egui::RichText::new(&row.author)
                            .color(author_color(&row.author))
                            .small()
                            .monospace(),
                    );
                    ui.label(egui::RichText::new(&row.path).small());
                    ui.label(
                        egui::RichText::new(summary_text)
                            .color(summary_color)
                            .small()
                            .strong(),
                    );
                    let open_resp = ui.small_button("Open");
                    if open_resp.clicked() {
                        pending = Some(Action::Open(row.path.clone()));
                    }
                    match &row.payload {
                        ActivityPayload::Change(c) => {
                            if c.content_hash.is_some()
                                && ui.small_button("View diff").clicked()
                            {
                                pending = Some(Action::Diff {
                                    path: row.path.clone(),
                                    change_id: c.id.to_string(),
                                });
                            }
                        }
                        ActivityPayload::Staging(s) => {
                            if ui.small_button("Review").clicked() {
                                pending = Some(Action::Inspect(s.id.clone()));
                            }
                        }
                    }
                    let row_path = row.path.clone();
                    let rollback_target: Option<i64> = match &row.payload {
                        ActivityPayload::Change(c) => Some(c.id),
                        _ => None,
                    };
                    open_resp.context_menu(|ui| {
                        if ui.button("View history for this note").clicked() {
                            pending = Some(Action::ViewHistory(row_path.clone()));
                            ui.close();
                        }
                        if let Some(change_id) = rollback_target
                            && ui.button("Roll back to previous version").clicked()
                        {
                            pending = Some(Action::Rollback {
                                path: row_path.clone(),
                                change_id,
                            });
                            ui.close();
                        }
                    });
                });
            }
        });

    match pending {
        Some(Action::Open(path)) => {
            editor_pane::open_file(app, &path, /* sticky */ true);
        }
        Some(Action::Diff { path, change_id }) => {
            if let Some(existing) = app.session.tabs.iter().find(|t| matches!(
                &t.kind,
                TabKind::Editor {
                    buffer: crate::tab::BufferSource::Snapshot { path: p, change_id: c },
                    ..
                } if p == &path && c == &change_id
            )) {
                app.session.active_tab = Some(existing.id);
            } else {
                let id = app.next_tab_id();
                app.session.tabs.push(Tab {
                    id,
                    kind: TabKind::snapshot_preview(path, change_id),
                    sticky: true,
                });
                app.session.active_tab = Some(id);
            }
        }
        Some(Action::ViewHistory(path)) => {
            crate::panels::home::open_home_detail(
                app,
                crate::tab::HomeDetail::ActivityRow { path },
            );
        }
        Some(Action::Rollback { path, change_id }) => {
            crate::panels::home::rollback_change(app, &path, change_id);
        }
        Some(Action::Inspect(proposal_id)) => {
            // Walk the staging service so we can resolve the target
            // path — staging IDs are opaque elsewhere in the UI.
            let staging = app.vault_session.services.staging.clone();
            if let Ok(list) = staging.list(&Default::default())
                && let Some(p) = list.into_iter().find(|p| p.id == proposal_id)
            {
                let pid = p.id.clone();
                let target = p.target_path.clone();
                let pid_for_build = pid.clone();
                app.find_or_open_tab(
                    |k| matches!(
                        k,
                        TabKind::Editor {
                            buffer: crate::tab::BufferSource::StagingProposal { proposal_id: q, .. },
                            ..
                        } if *q == pid
                    ),
                    || TabKind::staging_preview(pid_for_build, target),
                );
            }
        }
        None => {}
    }

    ui.ctx().data_mut(|d| d.insert_temp(mem_id, filter_state));
}

fn render_filter_chips(ui: &mut egui::Ui, state: &mut ChangesFilterState) {
    ui.horizontal_wrapped(|ui| {
        chip_group(
            ui,
            "Source",
            &mut state.source,
            &[
                SourceFilter::All,
                SourceFilter::Committed,
                SourceFilter::Pending,
            ],
            |s| s.label(),
        );
        ui.add_space(12.0);
        chip_group(
            ui,
            "Author",
            &mut state.author,
            &[
                AuthorFilter::All,
                AuthorFilter::User,
                AuthorFilter::Agent,
                AuthorFilter::Auto,
            ],
            |a| a.label(),
        );
        ui.add_space(12.0);
        chip_group(
            ui,
            "Op",
            &mut state.op,
            &[
                OpFilter::All,
                OpFilter::Modified,
                OpFilter::Created,
                OpFilter::Deleted,
                OpFilter::Renamed,
            ],
            |o| o.label(),
        );
    });
}

fn chip_group<T: Copy + PartialEq>(
    ui: &mut egui::Ui,
    label: &str,
    current: &mut T,
    options: &[T],
    text_of: impl Fn(T) -> &'static str,
) {
    ui.label(
        egui::RichText::new(label)
            .small()
            .color(theme::muted()),
    );
    for &opt in options {
        let selected = *current == opt;
        if ui.selectable_label(selected, text_of(opt)).clicked() {
            *current = opt;
        }
    }
}

fn op_color(op: ChangeOp) -> egui::Color32 {
    match op {
        ChangeOp::Created => egui::Color32::from_rgb(0x2f, 0x8f, 0x4d),
        ChangeOp::Modified => egui::Color32::from_rgb(0x2f, 0x6f, 0xb9),
        ChangeOp::Deleted => egui::Color32::from_rgb(0xb9, 0x3a, 0x3a),
        ChangeOp::Renamed => egui::Color32::from_rgb(0x9a, 0x5f, 0x1f),
    }
}

fn author_color(author: &str) -> egui::Color32 {
    if author.starts_with("agent:") {
        egui::Color32::from_rgb(0x6a, 0x4f, 0x8f)
    } else if author.starts_with("auto:") {
        egui::Color32::from_rgb(0x5f, 0x7f, 0x5f)
    } else {
        theme::muted()
    }
}

fn format_ts_ms(ms: i64) -> String {
    use time::macros::format_description;
    use time::OffsetDateTime;
    let secs = ms / 1000;
    let Ok(t) = OffsetDateTime::from_unix_timestamp(secs) else {
        return String::new();
    };
    let fmt = format_description!("[year]-[month]-[day] [hour]:[minute]");
    t.format(fmt).unwrap_or_default()
}
