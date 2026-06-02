//! The RSS subscription form layout (the GUI half of
//! `rss-subscription-lifecycle`).
//!
//! Renders the `mode: feed` capture note's `FeedParams` as a form over its
//! frontmatter: feed URL, poll interval, full-text toggle, item retention.
//! The subscription status reads as an ongoing state — "subscribed · polling
//! every N · last checked …" — with Pause / Resume (the `paused` flag) and a
//! Poll-now button, plus the captured-entries index. Editing a field rewrites
//! the note's frontmatter; Poll-now runs the feed poll off the UI thread.
//
// status: rss-subscription-lifecycle

use eframe::egui;

use crate::state::AppState;
use crate::tab::TabId;
use hiker_theme as theme;

use super::{persist, PageRow, RunKind};

/// Render the feed subscription form for the note at `note_path`.
pub fn show(ui: &mut egui::Ui, app: &mut AppState, tab_id: TabId, note_path: &str) {
    let mut dirty = false;

    egui::ScrollArea::vertical().show(ui, |ui| {
        ui.add_space(6.0);
        ui.heading("Feed subscription");
        status_line(ui, app, tab_id);
        ui.add_space(8.0);

        dirty |= fields(ui, app, tab_id);
        ui.add_space(10.0);
        controls(ui, app, tab_id, note_path);
        ui.add_space(12.0);
        index(ui, app, tab_id);
    });

    if dirty {
        persist(app, tab_id, note_path);
    }
}

/// The ongoing "subscribed · polling every N · last checked …" status line.
fn status_line(ui: &mut egui::Ui, app: &AppState, tab_id: TabId) {
    let Some(pane) = app.panels.captures.get(&tab_id) else { return };
    let Some(params) = pane.spec.as_ref().and_then(|s| s.feed.as_ref()) else { return };

    let state = if params.paused { "paused" } else { "subscribed" };
    let cadence = match params.poll_interval.as_deref() {
        Some(i) if !i.is_empty() => format!("polling every {i}"),
        _ => "manual poll only".to_string(),
    };
    let checked = match params.last_checked.as_deref() {
        Some(t) if !t.is_empty() => format!("last checked {t}"),
        _ => "never checked".to_string(),
    };
    ui.label(
        egui::RichText::new(format!("{state} · {cadence} · {checked}"))
            .color(theme::muted()),
    );
}

/// The editable feed parameter fields. Returns `true` if any changed.
fn fields(ui: &mut egui::Ui, app: &mut AppState, tab_id: TabId) -> bool {
    let mut changed = false;
    let Some(pane) = app.panels.captures.get_mut(&tab_id) else { return false };
    let Some(spec) = pane.spec.as_mut() else { return false };
    let Some(params) = spec.feed.as_mut() else { return false };

    egui::Grid::new("feed-fields")
        .num_columns(2)
        .spacing([12.0, 8.0])
        .show(ui, |ui| {
            ui.label("Feed URL");
            let resp = ui.add(
                egui::TextEdit::singleline(&mut params.url)
                    .desired_width(320.0)
                    .hint_text("https://example.com/feed.xml"),
            );
            if resp.changed() {
                // `capture.source` mirrors the feed URL for the discriminator
                // path; keep them in sync.
                spec.source = Some(params.url.clone());
                changed = true;
            }
            ui.end_row();

            ui.label("Poll interval");
            let mut interval = params.poll_interval.clone().unwrap_or_default();
            let resp = ui.add(
                egui::TextEdit::singleline(&mut interval)
                    .id_salt("feed-interval")
                    .desired_width(120.0)
                    .hint_text("30m / 6h / blank = manual"),
            );
            if resp.changed() {
                params.poll_interval =
                    if interval.trim().is_empty() { None } else { Some(interval.trim().to_string()) };
                changed = true;
            }
            ui.end_row();

            ui.label("Full-text articles");
            if ui
                .checkbox(&mut params.full_text, "follow each link, extract the full article")
                .changed()
            {
                changed = true;
            }
            ui.end_row();

            ui.label("Item retention");
            let mut retention = params.item_retention.clone().unwrap_or_default();
            let resp = ui.add(
                egui::TextEdit::singleline(&mut retention)
                    .id_salt("feed-retention")
                    .desired_width(120.0)
                    .hint_text("keep:50 / forever"),
            );
            if resp.changed() {
                params.item_retention =
                    if retention.trim().is_empty() { None } else { Some(retention.trim().to_string()) };
                changed = true;
            }
            ui.end_row();
        });

    changed
}

/// Pause / Resume + Poll-now controls.
fn controls(ui: &mut egui::Ui, app: &mut AppState, tab_id: TabId, note_path: &str) {
    let running = app
        .panels
        .captures
        .get(&tab_id)
        .and_then(|p| p.run.as_ref())
        .is_some_and(|r| r.running);
    let paused = app
        .panels
        .captures
        .get(&tab_id)
        .and_then(|p| p.spec.as_ref())
        .and_then(|s| s.feed.as_ref())
        .is_some_and(|f| f.paused);

    let mut toggle_pause = false;
    ui.horizontal(|ui| {
        if running {
            ui.add(egui::Spinner::new());
            ui.label(egui::RichText::new("polling…").color(theme::muted()));
        } else if ui.button("Poll now").clicked() {
            RunKind::Feed.start(app, tab_id, note_path);
        }

        let pause_label = if paused { "Resume" } else { "Pause" };
        if ui.button(pause_label).clicked() {
            toggle_pause = true;
        }
    });

    if toggle_pause {
        if let Some(params) = app
            .panels
            .captures
            .get_mut(&tab_id)
            .and_then(|p| p.spec.as_mut())
            .and_then(|s| s.feed.as_mut())
        {
            params.paused = !params.paused;
        }
        persist(app, tab_id, note_path);
    }

    if let Some(summary) = app.panels.captures.get(&tab_id).and_then(|p| p.last_summary.clone()) {
        ui.label(egui::RichText::new(format!("Last poll: {summary}")).color(theme::muted()).small());
    }
}

/// The captured-entries index list.
fn index(ui: &mut egui::Ui, app: &mut AppState, tab_id: TabId) {
    let Some(pane) = app.panels.captures.get(&tab_id) else { return };
    if pane.pages.is_empty() {
        ui.label(egui::RichText::new("No new entries from the last poll.").color(theme::muted()).small());
        return;
    }
    ui.label(egui::RichText::new(format!("Captured entries ({})", pane.pages.len())).strong());
    ui.add_space(4.0);
    let rows: Vec<PageRow> = pane.pages.clone();
    egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
        for row in &rows {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("•").color(theme::muted()));
                ui.label(egui::RichText::new(&row.label).small());
                ui.label(egui::RichText::new(format!("({})", row.note)).color(theme::muted()).small());
            });
        }
    });
}
