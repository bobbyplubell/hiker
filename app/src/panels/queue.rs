//! Task queue panel: surface the shared `core::tasks::Queue` (note
//! mutations, agent calls, structured-output tasks) alongside the
//! indexer's own pending-paths queue. Mirrors `ui/src/app/taskQueueTile.ts`
//! plus the old detail pane.

use eframe::egui;

use hiker_core::tasks::types::{TaskKind, TaskRecord, TaskState};

use crate::editor_pane;
use crate::state::AppState;
use crate::theme;

pub fn show(ui: &mut egui::Ui, app: &mut AppState) {
    ui.heading("Task queue");
    ui.add_space(8.0);

    // Worker configuration row: drives where new tasks dispatch (the
    // in-process direct-LLM worker, an external rmcp client, or `Auto`
    // which prefers external when one is attached). Spec: `tasks_md`
    // `task-queue-row-pulsing-leased` + `[tasks]` settings.
    View { ui, app }.worker_controls();
    ui.add_space(8.0);

    // ----- Background task queue (mutations, agent tools, etc.) -----
    // Read the per-frame snapshot cache populated in
    // `main::refresh_task_snapshot`; avoids blocking the UI thread here.
    let task_snapshot: Vec<TaskRecord> = app.ui_cache.task_snapshot.clone();
    let (mut queued, mut leased, mut completed, mut failed, mut cancelled) =
        (0usize, 0, 0, 0, 0);
    for r in &task_snapshot {
        match r.state {
            TaskState::Queued => queued += 1,
            TaskState::Leased => leased += 1,
            TaskState::Completed => completed += 1,
            TaskState::Failed => failed += 1,
            TaskState::Cancelled => cancelled += 1,
        }
    }
    egui::Grid::new("task-queue-stats")
        .num_columns(2)
        .spacing([16.0, 4.0])
        .show(ui, |ui| {
            ui.label("Tasks queued");
            ui.label(format!("{queued}"));
            ui.end_row();
            ui.label("Running");
            ui.label(format!("{leased}"));
            ui.end_row();
            ui.label("Completed");
            ui.label(format!("{completed}"));
            ui.end_row();
            if failed > 0 {
                ui.label(egui::RichText::new("Failed").color(egui::Color32::RED));
                ui.label(egui::RichText::new(format!("{failed}")).color(egui::Color32::RED));
                ui.end_row();
            }
            if cancelled > 0 {
                ui.label("Cancelled");
                ui.label(format!("{cancelled}"));
                ui.end_row();
            }
        });
    if !task_snapshot.is_empty() {
        ui.add_space(6.0);
        // State filter pills (`queue-detail-filter-tasks`). The default
        // "All" shows every state; clicking one of the labelled pills
        // narrows the list. Counts live next to each label so users can
        // see what's available without flipping the toggle.
        let memid = egui::Id::new("queue-state-filter");
        let mut filter: Option<TaskState> = ui
            .ctx()
            .data(|d| d.get_temp::<Option<TaskState>>(memid))
            .unwrap_or(None);
        ui.horizontal(|ui| {
            let all_sel = filter.is_none();
            if ui
                .selectable_label(all_sel, format!("All ({})", task_snapshot.len()))
                .clicked()
            {
                filter = None;
            }
            for (label, val, n) in [
                ("Running", TaskState::Leased, leased),
                ("Queued", TaskState::Queued, queued),
                ("Failed", TaskState::Failed, failed),
                ("Done", TaskState::Completed, completed),
                ("Cancelled", TaskState::Cancelled, cancelled),
            ] {
                if n == 0 {
                    continue;
                }
                let sel = filter == Some(val);
                if ui
                    .selectable_label(sel, format!("{label} ({n})"))
                    .clicked()
                {
                    filter = if sel { None } else { Some(val) };
                }
            }
        });
        ui.ctx().data_mut(|d| d.insert_temp(memid, filter));

        egui::ScrollArea::vertical()
            .id_salt("task-queue-rows")
            .max_height(220.0)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let mut rows = task_snapshot.clone();
                rows.sort_by(|a, b| state_rank(a.state).cmp(&state_rank(b.state)).then(b.submitted_at_ms.cmp(&a.submitted_at_ms)));
                let mut any_leased = false;
                for r in &rows {
                    if let Some(want) = filter
                        && r.state != want
                    {
                        continue;
                    }
                    if matches!(r.state, TaskState::Leased) {
                        any_leased = true;
                    }
                    View { ui, app }.task_row(r);
                }
                // Leased-row pulse animation (`task-queue-row-pulsing-leased`):
                // request a steady repaint while any task is in flight so the
                // running-state coloring can throb.
                if any_leased {
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(120));
                }
            });
    } else {
        ui.label(
            egui::RichText::new("(no background tasks)")
                .color(theme::muted())
                .small(),
        );
    }

    ui.add_space(12.0);
    ui.separator();
    ui.add_space(8.0);
    ui.label(egui::RichText::new("Indexer").strong());
    ui.add_space(4.0);

    let indexer = app.vault_session.services.indexer.clone();

    let status = indexer.status();
    egui::Grid::new("queue-stats")
        .num_columns(2)
        .spacing([16.0, 4.0])
        .show(ui, |ui| {
            ui.label("Model");
            ui.label(if status.model_ready {
                "Ready"
            } else {
                "Loading…"
            });
            ui.end_row();

            ui.label("Queued");
            ui.label(format!("{}", status.queued));
            ui.end_row();

            ui.label("Indexed notes");
            ui.label(format!("{}", status.total_notes));
            ui.end_row();

            if let Some(err) = &status.last_error {
                ui.label(egui::RichText::new("Last error").color(egui::Color32::RED));
                ui.label(egui::RichText::new(err).color(egui::Color32::RED));
                ui.end_row();
            }
        });

    ui.add_space(12.0);
    ui.separator();
    ui.add_space(8.0);

    let pending = indexer.pending_paths();
    if pending.is_empty() {
        ui.label(
            egui::RichText::new("Nothing queued right now.")
                .color(theme::muted()),
        );
    } else {
        ui.label(
            egui::RichText::new(format!("Pending ({})", pending.len())).strong(),
        );
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for rel in &pending {
                    ui.horizontal(|ui| {
                        if ui
                            .small_button("Open")
                            .on_hover_text("Open this file in a buffer tab")
                            .clicked()
                        {
                            editor_pane::open_file(app, rel, /* sticky */ false);
                        }
                        ui.label(
                            egui::RichText::new(rel.clone())
                                .color(theme::muted())
                                .monospace()
                                .small(),
                        );
                    });
                }
            });
    }
}

/// Order rows by liveness: in-flight first, queued next, then terminal.
const fn state_rank(s: TaskState) -> u8 {
    match s {
        TaskState::Leased => 0,
        TaskState::Queued => 1,
        TaskState::Failed => 2,
        TaskState::Completed => 3,
        TaskState::Cancelled => 4,
    }
}

/// Per-frame render context for the task-queue panel. Bundling `ui` +
/// `app` lets the per-row / worker-control render steps be inherent
/// methods rather than single-use free functions.
struct View<'a> {
    ui: &'a mut egui::Ui,
    app: &'a mut AppState,
}

impl View<'_> {
    fn task_row(&mut self, r: &TaskRecord) {
    let ui = &mut *self.ui;
    let app = &mut *self.app;
    let (label, color) = match r.state {
        TaskState::Queued => ("queued", theme::muted()),
        TaskState::Leased => ("running", egui::Color32::from_rgb(0x1f, 0x70, 0x4c)),
        TaskState::Completed => ("done", theme::muted()),
        TaskState::Failed => ("failed", egui::Color32::RED),
        TaskState::Cancelled => ("cancelled", theme::muted()),
    };
    // Leased rows pulse so the user can see motion. Modulate alpha on a
    // ~1.5s sine via egui's time. Non-leased rows stay flat.
    let display_color = if matches!(r.state, TaskState::Leased) {
        let t = ui.ctx().input(|i| i.time);
        let phase = (t * std::f64::consts::TAU / 1.5).sin() * 0.5 + 0.5;
        let alpha = (180.0 + 75.0 * phase) as u8;
        egui::Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha)
    } else {
        color
    };
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(label)
                .small()
                .monospace()
                .color(display_color),
        );
        let kind_text = match &r.kind {
            TaskKind::NoteMutation { mutation, source_path } => {
                format!("mutation · {mutation} · {source_path}")
            }
            other => format!("{other:?}"),
        };
        ui.add(egui::Label::new(egui::RichText::new(kind_text).small()).truncate());
        if let TaskKind::NoteMutation { source_path, .. } = &r.kind {
            let path = source_path.clone();
            if ui
                .small_button("Open")
                .on_hover_text("Open the source note")
                .clicked()
            {
                editor_pane::open_file(app, &path, /* sticky */ false);
            }
        }
        if matches!(r.state, TaskState::Queued | TaskState::Leased)
            && ui.small_button("Cancel").clicked()
        {
            let queue = app.vault_session.services.tasks.clone();
            let id = r.id.clone();
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    queue.cancel(&id).await;
                });
            }
        }
    });
    }

    /// Worker-preference + direct-worker.enabled toggles. Spec calls these
    /// out per `task-queue.md` — letting users opt out of the in-app direct
    /// worker (so background work only fires when an external rmcp client is
    /// draining) or force-prefer one side of the routing.
    fn worker_controls(&mut self) {
    let ui = &mut *self.ui;
    let app = &mut *self.app;
    use hiker_core::config::sections::WorkerPreferenceCfg;
    let cfg_snap = app
        .vault_session.config
        .read()
        .map(|c| c.tasks.clone())
        .unwrap_or_default();
    let mut pref = cfg_snap.worker_preference;
    let mut enabled = cfg_snap.direct_worker.enabled;
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("Worker")
                .small()
                .color(theme::muted()),
        );
        let mut changed_pref = false;
        for (lbl, val, hover) in [
            ("Auto", WorkerPreferenceCfg::Auto, "Prefer external when one is attached, else direct"),
            ("Direct", WorkerPreferenceCfg::Internal, "Always run via the in-app direct worker"),
            ("External", WorkerPreferenceCfg::External, "Wait for an external rmcp client to drain"),
        ] {
            if ui
                .radio_value(&mut pref, val, lbl)
                .on_hover_text(hover)
                .changed()
            {
                changed_pref = true;
            }
        }
        if changed_pref {
            let bias_str = match pref {
                WorkerPreferenceCfg::Auto => "auto",
                WorkerPreferenceCfg::Internal => "internal",
                WorkerPreferenceCfg::External => "external",
            };
            persist_tasks_setting(
                app,
                "tasks.worker_preference",
                &serde_json::json!(bias_str),
            );
        }
    });
    ui.horizontal(|ui| {
        if ui
            .checkbox(&mut enabled, "Run in-app direct worker")
            .on_hover_text(
                "When off, hiker queues background work but doesn't drain it until an external rmcp client attaches.",
            )
            .changed()
        {
            persist_tasks_setting(
                app,
                "tasks.direct_worker.enabled",
                &serde_json::json!(enabled),
            );
        }
    });
    }
}

fn persist_tasks_setting(
    app: &AppState,
    key: &str,
    value: &serde_json::Value,
) {
    crate::state::set_setting_quiet(
        app,
        hiker_core::config::SettingsScope::Vault,
        key,
        value,
        "tasks",
    );
}

pub fn show_detail(ui: &mut egui::Ui, _app: &mut AppState, task_id: &str) {
    // The indexer's pending set is identified by path; "task_id" here
    // doubles as the vault-relative path. A richer detail view (logs,
    // progress events) would live behind a subscription to
    // `indexer.subscribe_progress()` — left for follow-up.
    ui.heading(format!("Task · {}", task_id));
    ui.label(
        egui::RichText::new(
            "Detailed per-task progress isn't wired yet. The Queue panel \
             shows the snapshot of currently-pending paths.",
        )
        .color(theme::muted()),
    );
}
