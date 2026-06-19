//! Sprint metrics chart strip (`pm-layered-metrics`): the in-pane render of
//! `core::pm::metrics` — burnup, cycle-time distribution, and velocity —
//! toggled by the Metrics entry in the sprint board's header menu.
//!
//! The tables are computed from the board-doc's snapshot history
//! (zero tracking writes; the history *is* the data) and drawn as
//! in-memory `ChartSpec` + `Table` pairs through the existing hiker-charts
//! path: the `hiker_to_chart_theme` theme bridge, the plotters SVG
//! backend, and the resvg rasterizer — the same pipeline the charts tab
//! and inline chart blocks ride, no new chart machinery. The rendered
//! strip is memoized per board tab by a fingerprint of the metrics' live
//! inputs (`inputs_fingerprint`) — the board-doc's current accepted
//! content plus its cards' estimate meta plus the plan's close stamps — so
//! it invalidates on any input change, including an estimate edit that
//! writes `note_meta` but produces no board snapshot.
//
// status: pm-layered-metrics

use eframe::egui;
use hiker_charts_core::backend::{Backend, Size};
use hiker_charts_core::data::Table;
use hiker_charts_core::dsl::ChartSpec;
use hiker_core::pm::metrics::{Ctx, SprintTables};
use hiker_theme as theme;

use crate::state::AppState;
use crate::tab::TabId;

/// Rendered canvas size per chart, in chart pixels (the strip lays the
/// three charts side by side; logical size divides out the dpr).
const CHART_SIZE: Size = Size { width: 380, height: 230 };

/// The memoized strip: the input fingerprint it was computed at
/// (`inputs_fingerprint`), the rendered chart textures, the history scan's
/// truncation count (snapshots the scan couldn't use), and the loud error
/// of a failed computation. Owned by the board tab's `Pane`.
pub struct Strip {
    key: Option<String>,
    charts: Vec<Chart>,
    /// Snapshots the history scan skipped (unretained + unparseable) — renders the
    /// "history truncated" marker when nonzero.
    skipped_frames: usize,
    /// A failed computation's error — rendered as a distinct loud line, never
    /// conflated with the benign "No metrics yet" empty state.
    error: Option<String>,
}

/// One rendered metric chart: title + uploaded texture + logical size.
struct Chart {
    title: String,
    texture: egui::TextureHandle,
    size: egui::Vec2,
}

/// Render the metrics strip for `board_rel`, recomputing when a fingerprint
/// of the metrics' live inputs (`inputs_fingerprint`) changed since the
/// cached render — scoped per tab. The fingerprint captures the board's
/// current accepted content AND the store-side inputs (card estimates, plan
/// close stamps) the old snapshot-id key missed, so an estimate edit (which
/// writes `note_meta`, not a board snapshot) now correctly refreshes.
pub fn show(ui: &mut egui::Ui, app: &mut AppState, tab_id: TabId, board_rel: &str) {
    let fingerprint = current_fingerprint(app, board_rel);
    let stale = app
        .panels
        .boards
        .get(&tab_id)
        .and_then(|p| p.metrics.as_ref())
        .is_none_or(|s| s.key != fingerprint);
    if stale {
        let strip = build_strip(ui.ctx(), app, board_rel, fingerprint);
        app.panels.boards.entry(tab_id).or_default().metrics = Some(strip);
    }
    let Some(strip) = app.panels.boards.get(&tab_id).and_then(|p| p.metrics.as_ref()) else {
        return;
    };
    if let Some(err) = &strip.error {
        // A failed computation is loud — never the benign empty state.
        ui.colored_label(egui::Color32::RED, error_line(err));
        ui.separator();
        return;
    }
    if let Some(marker) = truncation_marker(strip.skipped_frames) {
        ui.label(egui::RichText::new(marker).small().color(theme::warn()));
    }
    if strip.charts.is_empty() {
        ui.label(
            egui::RichText::new(EMPTY_STATE)
                .small()
                .color(theme::muted()),
        );
    } else {
        egui::ScrollArea::horizontal()
            .id_salt(("board-metrics", tab_id))
            .show(ui, |ui| {
                ui.horizontal_top(|ui| {
                    for chart in &strip.charts {
                        ui.vertical(|ui| {
                            ui.label(
                                egui::RichText::new(&chart.title)
                                    .small()
                                    .color(theme::muted()),
                            );
                            ui.image((chart.texture.id(), chart.size));
                        });
                    }
                });
            });
    }
    ui.separator();
}

/// The benign no-history empty state — only shown when the computation
/// SUCCEEDED and produced nothing.
const EMPTY_STATE: &str = "No metrics yet — they derive from the sprint board's edit history";

/// The loud failed-computation line, distinct from [`EMPTY_STATE`] so a broken
/// computation never masquerades as "no history yet".
fn error_line(err: &str) -> String {
    format!("Metrics failed to compute: {err}")
}

/// The "history truncated" marker for a history scan that skipped snapshots
/// (pre-retention or unparseable) — `None` when nothing was skipped.
fn truncation_marker(skipped: usize) -> Option<String> {
    (skipped > 0).then(|| format!("history truncated: {skipped} frame(s) unavailable"))
}

/// Compute the metric tables and render each non-empty one to a texture.
fn build_strip(
    ctx: &egui::Context,
    app: &AppState,
    board_rel: &str,
    key: Option<String>,
) -> Strip {
    let metrics = match compute_metrics(app, board_rel) {
        Ok(m) => m,
        Err(err) => {
            return Strip {
                key,
                charts: Vec::new(),
                skipped_frames: 0,
                error: Some(err),
            }
        }
    };
    let skipped_frames = metrics.skipped_frames();
    // The theme bridge the inline chart widgets use — charts sit on the
    // editor surface palette. status: pm-layered-metrics
    let chart_theme = crate::charts::hiker_to_chart_theme(&editor_core::theme::light_default());
    let dpr = ctx.pixels_per_point();
    let mut charts = Vec::new();
    for (title, spec_yaml, csv) in chart_blocks(&metrics) {
        match render_pair(&spec_yaml, &csv, &chart_theme, dpr) {
            Some(image) => {
                let size = egui::vec2(
                    image.width() as f32 / dpr,
                    image.height() as f32 / dpr,
                );
                let texture = ctx.load_texture(
                    format!("board-metrics-{board_rel}-{title}"),
                    image,
                    egui::TextureOptions::LINEAR,
                );
                charts.push(Chart { title, texture, size });
            }
            None => {
                tracing::debug!(target: "hiker::pm", %board_rel, %title,
                    "metrics chart failed to render; skipped");
            }
        }
    }
    Strip { key, charts, skipped_frames, error: None }
}

/// The memo key: a fingerprint of the metrics' live inputs. Cheap (no
/// history replay) — reads the board's current accepted content and the
/// relevant store meta. `None` (the store is momentarily locked or the doc
/// is unreadable) just forces a recompute next frame.
fn current_fingerprint(app: &AppState, board_rel: &str) -> Option<String> {
    let store = app.vault_session.services.read_store.lock().ok()?;
    let ctx = Ctx {
        log: &app.vault_session.services.layered,
        store: &store,
        registry: &app.vault_session.services.kinds,
    };
    hiker_core::pm::metrics::inputs_fingerprint(&ctx, board_rel)
}

/// Compute the three metric tables from the board's history. `Err` carries
/// the failure text the strip renders as its loud error line (an
/// unavailable store and a failed computation are both errors — never the
/// benign empty state).
fn compute_metrics(app: &AppState, board_rel: &str) -> Result<SprintTables, String> {
    let store = app
        .vault_session
        .services
        .read_store
        .lock()
        .map_err(|_| "index store unavailable".to_string())?;
    let ctx = Ctx {
        log: &app.vault_session.services.layered,
        store: &store,
        registry: &app.vault_session.services.kinds,
    };
    hiker_core::pm::metrics::sprint_tables(&ctx, board_rel).map_err(|e| {
        tracing::warn!(target: "hiker::pm", %board_rel, error = %e,
            "sprint metrics replay failed");
        e.to_string()
    })
}

/// One in-memory `ChartSpec` + `Table` pair, rendered through the plotters
/// SVG backend and rasterized for upload — the charts-tab pipeline.
fn render_pair(
    spec_yaml: &str,
    csv: &str,
    chart_theme: &hiker_charts_core::theme::Theme,
    dpr: f32,
) -> Option<egui::ColorImage> {
    let spec = ChartSpec::from_yaml(spec_yaml).ok()?;
    let table = Table::from_csv(csv.as_bytes()).ok()?;
    let chart = hiker_charts_core::resolve::resolve(&spec, &table).ok()?;
    let output = hiker_charts_plotters::PlottersSvg
        .render(&chart, chart_theme, CHART_SIZE)
        .ok()?;
    hiker_charts_gui::raster::rasterize(&output.svg, dpr)
}

/// The `(title, spec yaml, csv)` triples for every non-empty metric table.
/// Pure over the rows, so the chart shapes are unit-testable without a
/// renderer. Series values use estimate sums when any card carries an
/// estimate, else plain counts (pm.md charts both; one axis per strip
/// chart keeps the strip legible).
fn chart_blocks(metrics: &SprintTables) -> Vec<(String, String, String)> {
    let mut blocks = Vec::new();
    if let Some(block) = burnup_block(metrics) {
        blocks.push(block);
    }
    if !metrics.cycle.is_empty() {
        let mut csv = String::from("days\n");
        for row in &metrics.cycle {
            csv.push_str(&format!("{:.2}\n", row.days()));
        }
        blocks.push((
            "Cycle time (days)".to_string(),
            "mark: histogram\nx: days\n".to_string(),
            csv,
        ));
    }
    if !metrics.velocity.is_empty() {
        let use_estimate = metrics.velocity.iter().any(|r| r.done_estimate > 0.0);
        let mut csv = String::from("sprint,done\n");
        for row in &metrics.velocity {
            let title = row
                .sprint_rel
                .rsplit('/')
                .next()
                .unwrap_or(&row.sprint_rel)
                .trim_end_matches(".md");
            let value = if use_estimate {
                row.done_estimate
            } else {
                row.done_count as f64
            };
            csv.push_str(&format!("{},{}\n", csv_field(title), value));
        }
        blocks.push((
            "Velocity".to_string(),
            "mark: bar\nx: sprint\ny: done\n".to_string(),
            csv,
        ));
    }
    blocks
}

/// The burnup line chart: done-vs-total series per day, long format with a
/// `series` color column. `None` when the table is empty.
fn burnup_block(metrics: &SprintTables) -> Option<(String, String, String)> {
    if metrics.burnup.is_empty() {
        return None;
    }
    let use_estimate = metrics.burnup.iter().any(|r| r.total_estimate > 0.0);
    let mut csv = String::from("day,series,value\n");
    for row in &metrics.burnup {
        let (done, total) = if use_estimate {
            (row.done_estimate, row.total_estimate)
        } else {
            (row.done_count as f64, row.total_count as f64)
        };
        csv.push_str(&format!("{},done,{done}\n", csv_field(&row.day)));
        csv.push_str(&format!("{},total,{total}\n", csv_field(&row.day)));
    }
    Some((
        "Burnup".to_string(),
        "mark: line\nx: day\ny: value\ncolor: series\n".to_string(),
        csv,
    ))
}

/// Quote a CSV field when it carries a delimiter / quote / newline
/// (sprint titles are user-named); plain fields pass through.
fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use hiker_core::pm::metrics::{BurnupRow, CycleRow, SprintTables, VelocityRow};

    use super::{chart_blocks, csv_field, error_line, truncation_marker, EMPTY_STATE};

    fn metrics() -> SprintTables {
        SprintTables {
            newest_op_id: Some("op-1".to_string()),
            burnup: vec![BurnupRow {
                day: "2026-06-01".to_string(),
                done_count: 1,
                done_estimate: 3.0,
                total_count: 2,
                total_estimate: 5.0,
            }],
            cycle: vec![CycleRow {
                handle: "a.md".to_string(),
                started_ms: 0,
                done_ms: 86_400_000,
            }],
            velocity: vec![VelocityRow {
                sprint_rel: "boards/sprint, one.md".to_string(),
                done_count: 1,
                done_estimate: 3.0,
            }],
            skipped_unretained: 0,
            skipped_unparseable: 0,
        }
    }

    /// Every emitted block is a valid in-memory `ChartSpec` + `Table` pair
    /// — the parse half of the render path the strip rides.
    #[test]
    fn chart_blocks_parse_as_spec_table_pairs() {
        let blocks = chart_blocks(&metrics());
        assert_eq!(blocks.len(), 3, "burnup + cycle + velocity");
        for (title, spec_yaml, csv) in &blocks {
            let spec = hiker_charts_core::dsl::ChartSpec::from_yaml(spec_yaml)
                .unwrap_or_else(|e| panic!("{title}: spec parse: {e}"));
            let table = hiker_charts_core::data::Table::from_csv(csv.as_bytes())
                .unwrap_or_else(|e| panic!("{title}: csv parse: {e}"));
            hiker_charts_core::resolve::resolve(&spec, &table)
                .unwrap_or_else(|d| panic!("{title}: resolve: {d:?}"));
        }
        // Estimates exist, so the burnup series charts estimate sums.
        assert!(blocks[0].2.contains("2026-06-01,done,3"));
        assert!(blocks[0].2.contains("2026-06-01,total,5"));
        // The comma-bearing sprint title is quoted.
        assert!(blocks[2].2.contains("\"sprint, one\""));
    }

    /// Without estimates the series fall back to plain card counts, and
    /// empty tables emit no block at all.
    #[test]
    fn chart_blocks_fall_back_to_counts_and_skip_empty_tables() {
        let mut m = metrics();
        m.burnup[0].done_estimate = 0.0;
        m.burnup[0].total_estimate = 0.0;
        m.cycle.clear();
        m.velocity.clear();
        let blocks = chart_blocks(&m);
        assert_eq!(blocks.len(), 1, "only the burnup table is non-empty");
        assert!(blocks[0].2.contains("2026-06-01,done,1"));
        assert!(blocks[0].2.contains("2026-06-01,total,2"));
    }

    /// The failed-computation line is distinct from the benign empty state (a
    /// broken computation must never read as "no history yet"), and the
    /// truncation marker fires exactly when snapshots were skipped.
    #[test]
    fn error_state_distinct_from_empty_and_marker_fires_on_skips() {
        let line = error_line("replay exploded");
        assert!(line.contains("replay exploded"));
        assert_ne!(line, EMPTY_STATE);

        assert_eq!(truncation_marker(0), None, "clean replay: no marker");
        assert_eq!(
            truncation_marker(3).as_deref(),
            Some("history truncated: 3 frame(s) unavailable"),
        );
    }

    #[test]
    fn csv_field_quotes_only_when_needed() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
    }
}
