//! Unified activity / changes feed: every committed `changes.db` row plus
//! every pending `staging.db` proposal, in one tab with author / source /
//! op filter chips. Supersedes the old `agent_changes` tab (which was a
//! strict subset filtered to `author LIKE 'agent:%'`) and absorbs the
//! home-page "recent activity" widget so there's one canonical view.

use eframe::egui;

use hiker_core::activity::{
    Filter, Payload, Source, Summary,
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
    const fn pattern(self) -> Option<&'static str> {
        match self {
            AuthorFilter::All => None,
            AuthorFilter::User => Some("user"),
            AuthorFilter::Agent => Some("agent:%"),
            AuthorFilter::Auto => Some("auto:%"),
        }
    }
    const fn label(self) -> &'static str {
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
    const fn to_activity(self) -> Source {
        match self {
            SourceFilter::All => Source::Merged,
            SourceFilter::Committed => Source::ChangesOnly,
            SourceFilter::Pending => Source::StagingOnly,
        }
    }
    const fn label(self) -> &'static str {
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
    const fn label(self) -> &'static str {
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
pub struct FilterState {
    pub author: AuthorFilter,
    pub source: SourceFilter,
    pub op: OpFilter,
}

impl Default for FilterState {
    fn default() -> Self {
        Self {
            author: AuthorFilter::All,
            source: SourceFilter::All,
            op: OpFilter::All,
        }
    }
}

/// A row action the user picked this frame, applied after the scroll
/// closure releases its borrows. Kept at module scope so the per-row
/// renderer and the dispatcher (both `AppState` methods below) can share it.
enum Action {
    Open(String),
    Diff { path: String, change_id: String },
    Inspect(String),
    ViewHistory(String),
    Rollback { path: String, change_id: i64 },
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
    let mut filter_state: FilterState = ui
        .ctx()
        .data(|d| d.get_temp(mem_id))
        .unwrap_or_default();

    filter_state.render_chips(ui);
    ui.add_space(6.0);

    let activity = app.vault_session.services.activity.clone();
    let filter = Filter {
        source: filter_state.source.to_activity(),
        limit: 500,
        author_pattern: filter_state.author.pattern().map(str::to_string),
        since_ms: None,
    };

    let rows = match activity.list(&filter) {
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

    // Op filter runs in Rust since `Filter` doesn't model it.
    // Cheap — list is bounded at 500 above.
    let op_filtered: Vec<_> = rows
        .into_iter()
        .filter(|row| match &row.summary {
            Summary::Change { op } => filter_state.op.matches(*op),
            // Staging rows don't have a `ChangeOp` — they show only when
            // the op filter is "All". Any narrower op picker excludes
            // them (you're asking for committed-op-X by definition).
            Summary::Staging { .. } => matches!(filter_state.op, OpFilter::All),
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

    let mut pending: Option<Action> = None;

    egui::ScrollArea::vertical()
        .id_salt("changes-scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for row in &op_filtered {
                if let Some(action) = app.render_activity_row(ui, row) {
                    pending = Some(action);
                }
            }
        });

    if let Some(action) = pending {
        app.apply_changes_action(action);
    }

    ui.ctx().data_mut(|d| d.insert_temp(mem_id, filter_state));
}

impl AppState {
    /// Render one activity-feed row and return the action (if any) the
    /// user picked. Pure UI + click detection — no state mutation; the
    /// caller defers `apply_changes_action` until borrows are released.
    fn render_activity_row(
        &self,
        ui: &mut egui::Ui,
        row: &hiker_core::activity::Item,
    ) -> Option<Action> {
        let mut action = None;
        let ts = {
            use time::OffsetDateTime;
            use time::macros::format_description;
            let secs = row.timestamp_ms / 1000;
            match OffsetDateTime::from_unix_timestamp(secs) {
                Ok(t) => {
                    let fmt = format_description!(
                        "[year]-[month]-[day] [hour]:[minute]"
                    );
                    t.format(fmt).unwrap_or_default()
                }
                Err(_) => String::new(),
            }
        };
        let (summary_text, summary_color) = match &row.summary {
            Summary::Change { op } => (
                format!("{:?}", op).to_lowercase(),
                match op {
                    ChangeOp::Created => egui::Color32::from_rgb(0x2f, 0x8f, 0x4d),
                    ChangeOp::Modified => egui::Color32::from_rgb(0x2f, 0x6f, 0xb9),
                    ChangeOp::Deleted => egui::Color32::from_rgb(0xb9, 0x3a, 0x3a),
                    ChangeOp::Renamed => egui::Color32::from_rgb(0x9a, 0x5f, 0x1f),
                },
            ),
            Summary::Staging { surface, action } => (
                format!("staged · {surface}/{action}"),
                theme::warn(),
            ),
        };
        let author_color = if row.author.starts_with("agent:") {
            egui::Color32::from_rgb(0x6a, 0x4f, 0x8f)
        } else if row.author.starts_with("auto:") {
            egui::Color32::from_rgb(0x5f, 0x7f, 0x5f)
        } else {
            theme::muted()
        };
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(ts).color(theme::muted()).small(),
            );
            ui.label(
                egui::RichText::new(&row.author)
                    .color(author_color)
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
                action = Some(Action::Open(row.path.clone()));
            }
            match &row.payload {
                Payload::Change(c) => {
                    if c.content_hash.is_some()
                        && ui.small_button("View diff").clicked()
                    {
                        action = Some(Action::Diff {
                            path: row.path.clone(),
                            change_id: c.id.to_string(),
                        });
                    }
                }
                Payload::Staging(s) => {
                    if ui.small_button("Review").clicked() {
                        action = Some(Action::Inspect(s.id.clone()));
                    }
                }
            }
            let row_path = row.path.clone();
            let rollback_target: Option<i64> = match &row.payload {
                Payload::Change(c) => Some(c.id),
                _ => None,
            };
            open_resp.context_menu(|ui| {
                if ui.button("View history for this note").clicked() {
                    action = Some(Action::ViewHistory(row_path.clone()));
                    ui.close();
                }
                if let Some(change_id) = rollback_target
                    && ui.button("Roll back to previous version").clicked()
                {
                    action = Some(Action::Rollback {
                        path: row_path.clone(),
                        change_id,
                    });
                    ui.close();
                }
            });
        });
        action
    }

    /// Apply a deferred row action: open/diff/inspect a note, jump to its
    /// history, or roll a change back.
    fn apply_changes_action(&mut self, action: Action) {
        match action {
            Action::Open(path) => {
                editor_pane::open_file(self, &path, /* sticky */ true);
            }
            Action::Diff { path, change_id } => {
                if let Some(existing) = self.session.tabs.iter().find(|t| matches!(
                    &t.kind,
                    TabKind::Editor {
                        buffer: crate::tab::BufferSource::Snapshot { path: p, change_id: c },
                        ..
                    } if p == &path && c == &change_id
                )) {
                    self.session.active_tab = Some(existing.id);
                } else {
                    let id = self.next_tab_id();
                    self.session.tabs.push(Tab {
                        id,
                        kind: TabKind::snapshot_preview(path, change_id),
                        sticky: true,
                    });
                    self.session.active_tab = Some(id);
                }
            }
            Action::ViewHistory(path) => {
                crate::panels::home::open_home_detail(
                    self,
                    crate::tab::HomeDetail::ActivityRow { path },
                );
            }
            Action::Rollback { path, change_id } => {
                self.rollback_change(&path, change_id);
            }
            Action::Inspect(proposal_id) => {
                // Walk the staging service so we can resolve the target
                // path — staging IDs are opaque elsewhere in the UI.
                let staging = self.vault_session.services.staging.clone();
                if let Ok(list) = staging.list(&Default::default())
                    && let Some(p) = list.into_iter().find(|p| p.id == proposal_id)
                {
                    let pid = p.id.clone();
                    let target = p.target_path.clone();
                    let pid_for_build = pid.clone();
                    self.find_or_open_tab(
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
        }
    }
}

impl FilterState {
    fn render_chips(&mut self, ui: &mut egui::Ui) {
    ui.horizontal_wrapped(|ui| {
        chip_group(
            ui,
            "Source",
            &mut self.source,
            &[
                SourceFilter::All,
                SourceFilter::Committed,
                SourceFilter::Pending,
            ],
            |s: SourceFilter| s.label(),
        );
        ui.add_space(12.0);
        chip_group(
            ui,
            "Author",
            &mut self.author,
            &[
                AuthorFilter::All,
                AuthorFilter::User,
                AuthorFilter::Agent,
                AuthorFilter::Auto,
            ],
            |a: AuthorFilter| a.label(),
        );
        ui.add_space(12.0);
        chip_group(
            ui,
            "Op",
            &mut self.op,
            &[
                OpFilter::All,
                OpFilter::Modified,
                OpFilter::Created,
                OpFilter::Deleted,
                OpFilter::Renamed,
            ],
            |o: OpFilter| o.label(),
        );
    });
    }
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

