//! Rules panel: every registered vault rule — name, trigger, enabled
//! state — expanding to its failed firings from the engine's in-memory
//! diagnostics ring. Read-only in v1 — the `[rules.<name>]` TOML is the
//! editing surface.
//!
//! NOTE: the *applied*-firing log used to project off op-log frames
//! authored `auto:rule:<name>`. With the op-log history engine + per-write
//! attribution retired (the core rework), there is no committed-history
//! feed to read, so this panel now surfaces only the failed-firings ring
//! plus rule metadata. TODO(K3c): if an applied-firing log is still wanted,
//! re-home it onto the engine (an in-memory ring like `failures`) or git
//! trailers — never the op-log history engine.
//!
//! See `docs/rules.md` §"The rules panel".
//
// status: rule-firings-panel

use eframe::egui;

use hiker_core::rules::FiringFailure;

use crate::state::AppState;
use hiker_theme as theme;

/// Firings shown per expanded rule.
const FIRINGS_PER_RULE: usize = 20;

/// Failure-row red (the changes tab's Deleted hue; the theme exports no
/// error color).
const FAILURE_RED: egui::Color32 = egui::Color32::from_rgb(0xb9, 0x3a, 0x3a);

/// A row action picked this frame, applied after the scroll closure
/// releases its borrows (the boards-index / changes pattern).
enum RowAction {
    Open(String),
}

/// One rule's panel projection, gathered before rendering so the UI pass
/// holds no service borrows.
struct RuleView {
    name: String,
    trigger: String,
    enabled: bool,
    /// Failed firings from the diagnostics ring, newest first.
    failures: Vec<FiringFailure>,
}

/// Render the Rules panel. status: rule-firings-panel
pub fn show(ui: &mut egui::Ui, app: &mut AppState) {
    ui.heading("Rules");
    ui.add_space(2.0);
    ui.label(
        egui::RichText::new(
            "Vault automation rules ([rules.<name>] in the vault config TOML — the \
             editing surface; this panel is read-only). Failed firings are listed \
             below; the applied-firing log was retired with the op-log history engine.",
        )
        .color(theme::muted())
        .small(),
    );
    ui.add_space(6.0);

    let views = gather_rules(app);
    if views.is_empty() {
        ui.label(
            egui::RichText::new(
                "No rules registered. Declare [rules.<name>] entries in \
                 .hiker/config.toml — see docs/rules.md.",
            )
            .color(theme::muted())
            .italics(),
        );
        return;
    }

    let mut action: Option<RowAction> = None;
    egui::ScrollArea::vertical()
        .id_salt("rules-scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for view in &views {
                render_rule(ui, view, &mut action);
            }
        });

    match action {
        Some(RowAction::Open(path)) => {
            // status: click-opens
            crate::editor_pane::open_file(app, &path, /* sticky */ true);
        }
        None => {}
    }
}

/// Snapshot every registered rule plus its failed firings from the
/// diagnostics ring.
fn gather_rules(app: &AppState) -> Vec<RuleView> {
    let services = &app.vault_session.services;
    let failures = services.rules.failures();
    services
        .rules
        .rules()
        .map(|rule| {
            let mine: Vec<FiringFailure> = failures
                .iter()
                .filter(|f| f.rule == rule.name)
                .take(FIRINGS_PER_RULE)
                .cloned()
                .collect();
            RuleView {
                name: rule.name.clone(),
                trigger: rule.trigger.label(),
                enabled: rule.enabled,
                failures: mine,
            }
        })
        .collect()
}

/// One rule: the header row (name, trigger, enabled state, last firing)
/// expanding to its recent firings + failure rows.
fn render_rule(ui: &mut egui::Ui, view: &RuleView, action: &mut Option<RowAction>) {
    let state = if view.enabled { "" } else { " · disabled" };
    let header = format!("{} · {}{}", view.name, view.trigger, state);
    egui::CollapsingHeader::new(egui::RichText::new(header).strong())
        .id_salt(format!("rule-{}", view.name))
        .show(ui, |ui| {
            if view.failures.is_empty() {
                ui.label(
                    egui::RichText::new("(no failed firings)")
                        .color(theme::muted())
                        .italics(),
                );
                return;
            }
            for failure in &view.failures {
                render_failure_row(ui, failure, action);
            }
        });
    ui.add_space(2.0);
}

/// One failed firing from the diagnostics ring — nothing was committed, so
/// this is the only trace (`obs-log-ring-buffer` posture).
fn render_failure_row(ui: &mut egui::Ui, failure: &FiringFailure, action: &mut Option<RowAction>) {
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(format_ts(failure.at_ms))
                .color(theme::muted())
                .small(),
        );
        ui.label(egui::RichText::new("failed").color(FAILURE_RED).small().strong());
        if failure.note_path.is_empty() {
            ui.label(egui::RichText::new(&failure.message).color(FAILURE_RED).small());
            return;
        }
        let resp = ui.link(egui::RichText::new(&failure.note_path).small());
        if resp.clicked() {
            *action = Some(RowAction::Open(failure.note_path.clone()));
        }
        crate::widgets::preview::register_note_hover(ui, resp.rect, &failure.note_path);
        ui.label(egui::RichText::new(&failure.message).color(FAILURE_RED).small());
    });
}

/// `YYYY-MM-DD HH:MM` for a unix-millisecond stamp.
fn format_ts(ms: i64) -> String {
    use time::macros::format_description;
    use time::OffsetDateTime;
    match OffsetDateTime::from_unix_timestamp(ms / 1000) {
        Ok(t) => {
            let fmt = format_description!("[year]-[month]-[day] [hour]:[minute]");
            t.format(fmt).unwrap_or_default()
        }
        Err(_) => String::new(),
    }
}
