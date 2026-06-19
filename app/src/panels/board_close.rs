//! The sprint close / rollover verb surface (`sprint-rollover`): the
//! Close sprint… destination picker on a sprint board's header — the
//! plan-derived default first (`plan-kind`), then every other board-doc —
//! and the call into `core::pm::close_sprint`, which stages the rollover
//! as one reviewable `auto:sprint-close` batch. Split from
//! `panels::board` so the board pane stays inside the file-length budget.
//
// status: sprint-rollover

use eframe::egui;
use hiker_theme as theme;

use crate::state::{AppState, ToastLevel};

/// The Close sprint picker body: the plan-derived default destination
/// first (pm.md's "next sprint by `start` date, else the plan's backlog
/// board" — `plan-kind`), then every other board-doc in the vault
/// (sprint-kind or plain — a plain board can serve as a backlog). Returns
/// the picked destination's rel path, if any.
///
/// status: sprint-rollover
pub fn render_menu(ui: &mut egui::Ui, app: &AppState, closing_rel: &str) -> Option<String> {
    let (default, candidates) = close_destinations(app, closing_rel);
    let mut picked = None;
    if let Some(rel) = default {
        let title = candidates
            .iter()
            .find(|(r, _)| r == &rel)
            .map_or_else(|| rel.clone(), |(_, t)| t.clone());
        if ui.button(format!("{title} (plan default)")).clicked() {
            picked = Some(rel);
            ui.close();
        }
        ui.separator();
    }
    if candidates.is_empty() {
        ui.label(
            egui::RichText::new("No destination board exists")
                .small()
                .color(theme::muted()),
        );
    }
    for (rel, title) in candidates {
        if ui.button(title).clicked() {
            picked = Some(rel);
            ui.close();
        }
    }
    picked
}

/// Destination data for the Close sprint picker: the plan-derived default
/// (when the sprint belongs to a plan, `plan-kind`) plus every other
/// board-doc in the vault as `(rel_path, title)` rows. Read-only; called
/// on menu open.
fn close_destinations(
    app: &AppState,
    closing_rel: &str,
) -> (Option<String>, Vec<(String, String)>) {
    let Ok(store) = app.vault_session.services.read_store.lock() else {
        return (None, Vec::new());
    };
    let registry = app.vault_session.services.kinds.as_ref();
    let default = hiker_core::pm::default_rollover_destination(&store, registry, closing_rel)
        .ok()
        .flatten();
    let candidates = hiker_core::boards::list(
        &app.vault_session.vault,
        &store,
        &app.vault_session.services.layered,
        Some(registry),
    )
    .unwrap_or_default()
    .into_iter()
    .filter(|b| b.rel_path != closing_rel)
    .map(|b| (b.rel_path, b.title))
    .collect();
    (default, candidates)
}

/// Close this sprint into `destination` via `core::pm::close_sprint`
/// (`sprint-rollover`): one `auto:sprint-close` batch — the rollover moves
/// plus the `closed_at` stamp. Review mode (`[editing] review_required`,
/// the default) stages the batch for the standard staging surfaces, which
/// review it as ONE unit; with review off the batch auto-applies in the
/// same call (the `suggest.rs` precedent), so the close is atomic.
///
/// status: sprint-rollover
pub fn close_sprint(app: &mut AppState, board_rel: &str, destination: &str) {
    let review_required = app
        .vault_session
        .config
        .read()
        .map_or(true, |cfg| cfg.editing.review_required);
    let result = {
        let Ok(store) = app.vault_session.services.read_store.lock() else {
            app.push_toast("Close sprint failed: index store unavailable", ToastLevel::Error);
            return;
        };
        hiker_core::pm::close_sprint(&hiker_core::pm::CloseSprint {
            log: &app.vault_session.services.layered,
            vault: &app.vault_session.vault,
            store: &store,
            registry: &app.vault_session.services.kinds,
            closing_rel: board_rel,
            destination_rel: Some(destination),
            review_required,
        })
    };
    match result {
        Ok(outcome) if outcome.applied => app.push_toast(
            format!(
                "Sprint closed: {} card(s) -> {} \"{}\"",
                outcome.moved, outcome.destination_rel, outcome.destination_column,
            ),
            ToastLevel::Info,
        ),
        Ok(outcome) => app.push_toast(
            format!(
                "Sprint close staged for review: {} card(s) -> {} \"{}\"",
                outcome.moved, outcome.destination_rel, outcome.destination_column,
            ),
            ToastLevel::Info,
        ),
        Err(e) => app.push_toast(format!("Close sprint failed: {e}"), ToastLevel::Error),
    }
}
