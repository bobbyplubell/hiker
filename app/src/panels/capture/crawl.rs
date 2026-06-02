//! The crawl-job form layout (`crawl-job-form`).
//!
//! Renders the `mode: crawl` capture note's `CrawlParams` as a form over its
//! frontmatter: seed URL(s), mode (list / hub / deep), depth, follow/extract
//! patterns, extract-seed flag, extractor pick, artifact retention, rate limit
//! — plus a Run / Cancel control, live per-page progress, and the
//! captured-page index. Editing any field rewrites the note's frontmatter; Run
//! launches the governed crawl loop off the UI thread.
//
// status: crawl-job-form

use eframe::egui;
use hiker_extract::capture::CrawlMode;

use crate::state::AppState;
use crate::tab::TabId;
use hiker_theme as theme;

use super::{exec, persist, PageRow, RunKind};

/// Render the crawl form for the note at `note_path`.
pub fn show(ui: &mut egui::Ui, app: &mut AppState, tab_id: TabId, note_path: &str) {
    let mut dirty = false;

    egui::ScrollArea::vertical().show(ui, |ui| {
        ui.add_space(6.0);
        ui.heading("Crawl job");
        ui.label(
            egui::RichText::new(
                "Capture a website into child notes. Editing a field saves to the note's \
                 frontmatter; Run launches the crawl.",
            )
            .color(theme::muted())
            .small(),
        );
        ui.add_space(8.0);

        dirty |= fields(ui, app, tab_id);
        ui.add_space(10.0);
        run_controls(ui, app, tab_id, note_path);
        ui.add_space(12.0);
        progress_and_index(ui, app, tab_id);
    });

    if dirty {
        persist(app, tab_id, note_path);
    }
}

/// The editable parameter fields. Returns `true` if any field changed this
/// frame (so the caller persists to frontmatter).
fn fields(ui: &mut egui::Ui, app: &mut AppState, tab_id: TabId) -> bool {
    let mut changed = false;
    let Some(pane) = app.panels.captures.get_mut(&tab_id) else { return false };
    let Some(spec) = pane.spec.as_mut() else { return false };
    let Some(params) = spec.crawl.as_mut() else { return false };

    // Seed URL(s) — multi-line, one per line. Held in a draft string so blank
    // lines mid-edit survive; committed to `seeds` on change.
    ui.label(egui::RichText::new("Seed URL(s) — one per line").strong());
    let draft = pane.seed_draft.get_or_insert_with(|| params.seeds.join("\n"));
    let resp = ui.add(
        egui::TextEdit::multiline(draft)
            .desired_rows(2)
            .desired_width(f32::INFINITY)
            .hint_text("https://example.com/docs"),
    );
    if resp.changed() {
        params.seeds = draft.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect();
        changed = true;
    }
    ui.add_space(8.0);

    egui::Grid::new("crawl-fields")
        .num_columns(2)
        .spacing([12.0, 8.0])
        .show(ui, |ui| {
            ui.label("Mode");
            let mut mode = params.mode;
            egui::ComboBox::from_id_salt("crawl-mode")
                .selected_text(mode_label(mode))
                .show_ui(ui, |ui| {
                    for m in [CrawlMode::List, CrawlMode::Hub, CrawlMode::Deep] {
                        if ui.selectable_value(&mut mode, m, mode_label(m)).clicked() {
                            // A mode switch re-defaults depth + extract-seed,
                            // matching the engine's per-mode defaults.
                            params.depth = m.default_depth();
                            params.extract_seed = m.default_extract_seed();
                        }
                    }
                });
            if mode != params.mode {
                params.mode = mode;
                changed = true;
            }
            ui.end_row();

            ui.label("Depth");
            let mut depth = params.depth as i64;
            if ui.add(egui::DragValue::new(&mut depth).range(0..=20)).changed() {
                params.depth = depth.max(0) as u32;
                changed = true;
            }
            ui.end_row();

            ui.label("Follow pattern");
            changed |= opt_text(ui, "crawl-follow", &mut params.follow_pattern, "/docs/**");
            ui.end_row();

            ui.label("Extract pattern");
            changed |= opt_text(ui, "crawl-extract", &mut params.extract_pattern, "**");
            ui.end_row();

            ui.label("Extract the seed page");
            if ui.checkbox(&mut params.extract_seed, "").changed() {
                changed = true;
            }
            ui.end_row();

            ui.label("Extractor");
            changed |= opt_text(ui, "crawl-extractor", &mut spec.extractor, "(auto)");
            ui.end_row();

            ui.label("Artifact retention");
            changed |= opt_text(ui, "crawl-retention", &mut params.artifact_retention, "latest");
            ui.end_row();

            ui.label("Max pages");
            let mut max_pages = params.max_pages as i64;
            if ui.add(egui::DragValue::new(&mut max_pages).range(1..=100_000)).changed() {
                params.max_pages = max_pages.max(1) as u32;
                changed = true;
            }
            ui.end_row();

            ui.label("Rate limit (ms)");
            let mut rate = params.rate_limit_ms as i64;
            if ui.add(egui::DragValue::new(&mut rate).range(0..=600_000)).changed() {
                params.rate_limit_ms = rate.max(0) as u64;
                changed = true;
            }
            ui.end_row();
        });

    changed
}

/// Run / Cancel button + status line.
fn run_controls(ui: &mut egui::Ui, app: &mut AppState, tab_id: TabId, note_path: &str) {
    let running = app
        .panels
        .captures
        .get(&tab_id)
        .and_then(|p| p.run.as_ref())
        .is_some_and(|r| r.running);

    ui.horizontal(|ui| {
        if running {
            let cancelling = app
                .panels
                .captures
                .get(&tab_id)
                .and_then(|p| p.run.as_ref())
                .is_some_and(exec::RunHandle::is_cancelling);
            let label = if cancelling { "Cancelling…" } else { "Cancel" };
            if ui.add_enabled(!cancelling, egui::Button::new(label)).clicked() {
                exec::cancel(app, tab_id);
            }
            ui.add(egui::Spinner::new());
            ui.label(egui::RichText::new("crawling…").color(theme::muted()));
        } else {
            // Re-run wording once a crawl has landed before.
            let has_run = app
                .panels
                .captures
                .get(&tab_id)
                .is_some_and(|p| p.last_summary.is_some());
            let label = if has_run { "Re-crawl" } else { "Run crawl" };
            if ui.button(label).clicked() {
                RunKind::Crawl.start(app, tab_id, note_path);
            }
        }
    });

    if let Some(summary) = app.panels.captures.get(&tab_id).and_then(|p| p.last_summary.clone()) {
        ui.label(egui::RichText::new(format!("Last run: {summary}")).color(theme::muted()).small());
    }
}

/// The live progress + captured-page index list.
fn progress_and_index(ui: &mut egui::Ui, app: &mut AppState, tab_id: TabId) {
    let Some(pane) = app.panels.captures.get(&tab_id) else { return };
    if pane.pages.is_empty() {
        ui.label(egui::RichText::new("No pages captured yet.").color(theme::muted()).small());
        return;
    }
    let captured = pane.pages.iter().filter(|p| p.path.is_some()).count();
    ui.label(
        egui::RichText::new(format!("Captured pages ({captured} kept / {} touched)", pane.pages.len()))
            .strong(),
    );
    ui.add_space(4.0);
    let rows: Vec<PageRow> = pane.pages.clone();
    egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
        for row in &rows {
            page_row(ui, row);
        }
    });
}

/// One captured-page index row: status glyph + label + note.
fn page_row(ui: &mut egui::Ui, row: &PageRow) {
    ui.horizontal(|ui| {
        let (glyph, color) = if row.path.is_some() {
            ("kept", theme::accent())
        } else {
            ("skip", egui::Color32::from_rgb(170, 130, 80))
        };
        ui.label(egui::RichText::new(glyph).color(color).small().monospace());
        ui.label(egui::RichText::new(&row.label).small());
        ui.label(egui::RichText::new(format!("({})", row.note)).color(theme::muted()).small());
    });
}

/// A `selectable_text`-shaped optional text field: empty input clears the
/// option, non-empty sets it. Returns `true` on change.
fn opt_text(ui: &mut egui::Ui, id: &str, field: &mut Option<String>, hint: &str) -> bool {
    let mut buf = field.clone().unwrap_or_default();
    let resp = ui.add(
        egui::TextEdit::singleline(&mut buf)
            .id_salt(id)
            .desired_width(220.0)
            .hint_text(hint),
    );
    if resp.changed() {
        *field = if buf.trim().is_empty() { None } else { Some(buf.trim().to_string()) };
        return true;
    }
    false
}

/// Human label for a crawl mode.
const fn mode_label(m: CrawlMode) -> &'static str {
    match m {
        CrawlMode::List => "List (extract a known set, follow nothing)",
        CrawlMode::Hub => "Hub (one index page's links)",
        CrawlMode::Deep => "Deep (archive a section)",
    }
}
