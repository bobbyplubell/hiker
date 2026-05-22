//! Indexer detail tab: model id, live status, reindex, progress log.
//!
//! Renders state pulled from `AppState::indexer` (the live `Handle`)
//! plus the per-session ring buffer of progress events that
//! `main::drain_indexer_events` feeds from the indexer's broadcast channel.

use std::sync::Arc;

use eframe::egui;

use crate::state::{AppState, ToastLevel};
use crate::theme;

pub fn show(
    ui: &mut egui::Ui,
    app: &mut AppState,
    rt: &Arc<tokio::runtime::Runtime>,
) {
    ui.heading("Index");
    ui.add_space(4.0);

    let model_id = app
        .vault_session.config
        .read()
        .map(|c| c.indexing.model.clone())
        .unwrap_or_else(|_| "(unknown)".to_string());

    egui::Grid::new("indexer-detail-grid")
        .num_columns(2)
        .spacing([12.0, 4.0])
        .show(ui, |ui| {
            ui.label(egui::RichText::new("Model").color(theme::muted()));
            ui.label(model_id);
            ui.end_row();

            {
                let s = app.vault_session.services.indexer.status();
                ui.label(egui::RichText::new("Model ready").color(theme::muted()));
                ui.label(if s.model_ready { "yes" } else { "loading…" });
                ui.end_row();

                ui.label(egui::RichText::new("Pending").color(theme::muted()));
                ui.label(
                    app.vault_session
                        .services
                        .indexer
                        .pending_count()
                        .to_string(),
                );
                ui.end_row();

                ui.label(egui::RichText::new("Total notes").color(theme::muted()));
                ui.label(s.total_notes.to_string());
                ui.end_row();

                if let Some(err) = &s.last_error {
                    ui.label(egui::RichText::new("Last error").color(theme::muted()));
                    ui.label(
                        egui::RichText::new(err)
                            .color(egui::Color32::from_rgb(200, 60, 60)),
                    );
                    ui.end_row();
                }
            }
        });

    ui.add_space(8.0);

    // Reindex button — `Handle::full_scan()` is the public method
    // for kicking off a vault-wide re-embed pass.
    let mut reindex_clicked = false;
    if ui
        .add(egui::Button::new("Reindex everything"))
        .clicked()
    {
        reindex_clicked = true;
    }
    if reindex_clicked {
        let idx = app.vault_session.services.indexer.clone();
        rt.spawn(async move {
            if let Err(err) = idx.full_scan(true).await {
                tracing::warn!(error = %err, "full_scan submit failed");
            }
        });
        app.push_toast("Reindex requested", ToastLevel::Info);
    }

    ui.add_space(12.0);
    ui.separator();
    ui.label(egui::RichText::new("Recent progress").color(theme::muted()));

    // Filter pills (`queue-detail-filter-index`): narrow the event log to
    // a substring match (embed / skip / error / write etc.). State is held
    // in egui memory so it survives across renders without leaking into
    // AppState.
    let mem_id = egui::Id::new("indexer-events-filter");
    let mut filter: String = ui
        .ctx()
        .data(|d| d.get_temp::<String>(mem_id))
        .unwrap_or_default();
    ui.horizontal(|ui| {
        let all_sel = filter.is_empty();
        if ui.selectable_label(all_sel, "All").clicked() {
            filter.clear();
        }
        for pill in ["embed", "skip", "error", "write", "remove"] {
            let sel = filter == pill;
            if ui.selectable_label(sel, pill).clicked() {
                filter = if sel { String::new() } else { pill.to_string() };
            }
        }
    });
    ui.ctx().data_mut(|d| d.insert_temp(mem_id, filter.clone()));

    egui::ScrollArea::vertical()
        .id_salt("indexer-events-scroll")
        .max_height(ui.available_height())
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let n = app.vault_session.events.indexer_events.len();
            let start = n.saturating_sub(200);
            let mut shown = 0usize;
            for line in app.vault_session.events.indexer_events.iter().skip(start) {
                if !filter.is_empty() && !line.to_ascii_lowercase().contains(&filter) {
                    continue;
                }
                ui.label(egui::RichText::new(line).monospace().small());
                shown += 1;
                if shown >= 50 {
                    break;
                }
            }
            if n == 0 {
                ui.label(
                    egui::RichText::new("(no events yet)")
                        .color(theme::muted())
                        .italics()
                        .small(),
                );
            } else if shown == 0 {
                ui.label(
                    egui::RichText::new(format!("(no events match '{}')", filter))
                        .color(theme::muted())
                        .italics()
                        .small(),
                );
            }
        });
}
