//! Headless performance profiler for the markdown editor's decoration rebuild.
//!
//! This tool quantifies a known perf bug: when the caret moves in a markdown
//! note that contains rendered diagrams (```mermaid / ```wavedrom / ```chart /
//! `$$` math), the per-frame decoration rebuild re-emits the whole-document
//! diagram-widget layers (`mermaid_widget`, `wavedrom_widget`, `chart_widget`,
//! `math_widget`, `table_widget`). Those layers are cache-keyed on the exact
//! selection fingerprint (`sel_fp`, see `app/src/panels/buffer/decorations.rs`),
//! so EVERY caret move busts them and rebuilds — and each cache-hit on the
//! underlying rendered bitmap is served by a deep `.cloned()` of the RGBA pixel
//! buffer. The cost therefore scales with the diagram count even though nothing
//! the user can see has changed.
//!
//! FIDELITY: the profiler drives the REAL
//! [`hiker_app::panels::buffer::decorations::rebuild_editor_layers`] against
//! a real [`editor_core::state::Editor`], real [`hiker_app::buffer::DecorationCache`],
//! real [`hiker_app::panels::buffer::decorations::DecoRebuildCtx`], a real theme,
//! and a real disk-backed [`DiagramCacheCtx`] rooted in a tempdir. It is NOT a
//! reimplementation — the whole point is to measure the genuine code path,
//! including the process-wide in-memory render cache that backs steady-state
//! cache hits.
//!
//! The rebuild is driven through the real [`editor_egui::widget::Widget`]'s
//! `with_decoration_rebuild` hook (the same seam the app uses), so the viewport,
//! visible-line range, and call ordering match production. We time ONLY the
//! `rebuild_editor_layers` call inside that hook — that is the cost the
//! caret-move bug is about, isolated from the widget's layout/paint.
//!
//! Method per level: build a synthetic doc with N diagrams interspersed with
//! prose, warm the caches with one rebuild (so steady-state measurements reflect
//! cache-hit behavior — what the user experiences), then move the caret to a
//! sweep of byte offsets (prose AND inside diagram fences, which flips reveal
//! state) and time the rebuild for each move.
//!
//! Usage:
//!   cargo run --release -p profile-buffer -- [OPTIONS]
//!     --levels a,b,c       diagram counts to measure (default 0,5,20,50)
//!     --moves N            caret moves timed per level (default 60)
//!     --width W            editor width px (default 900)
//!     --height H           editor height px (default 700)
//!     --kind mermaid|math  which diagram family to generate (default mermaid)
//!
//! Output is `tracing` info-level plus a labeled summary table at the end.

mod stats;
mod synth;

use std::time::Instant;

use anyhow::Result;
use editor_core::state::Editor;
use editor_core::theme::{Theme, light_default};
use editor_egui::widget::{PaintCache, Widget as EditorWidget};
use editor_view::viewport::ViewState;

use hiker_app::buffer::DecorationCache;
use hiker_app::panels::buffer::decorations::{DecoRebuildCtx, rebuild_editor_layers};
use hiker_app::panels::buffer::widgets::disk_cache::DiagramCacheCtx;

use crate::stats::Stats;
use crate::synth::DiagramKind;

/// An always-empty per-table overflow map for the profiling rebuild (no overflow
/// toggle / in-place cell edit here), borrowed `'static` so it isn't allocated
/// per frame.
static EMPTY_TABLE_OVERFLOW: std::sync::LazyLock<
    hiker_app::panels::buffer::widgets::tables::TableViewMap,
> = std::sync::LazyLock::new(hiker_app::panels::buffer::widgets::tables::TableViewMap::new);

/// Parsed command-line configuration.
struct Args {
    levels: Vec<usize>,
    moves: usize,
    width: f32,
    height: f32,
    kind: DiagramKind,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            levels: vec![0, 5, 20, 50],
            moves: 60,
            width: 900.0,
            height: 700.0,
            kind: DiagramKind::Mermaid,
        }
    }
}

fn parse_args() -> Result<Args> {
    let mut a = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = || {
            it.next()
                .ok_or_else(|| anyhow::anyhow!("missing value for {arg}"))
        };
        match arg.as_str() {
            "--levels" => a.levels = parse_levels(&value()?)?,
            "--moves" => a.moves = value()?.parse()?,
            "--width" => a.width = value()?.parse()?,
            "--height" => a.height = value()?.parse()?,
            "--kind" => a.kind = DiagramKind::parse(&value()?)?,
            "-h" | "--help" => {
                println!("see source comment for flags");
                std::process::exit(0);
            }
            other => anyhow::bail!("unknown arg: {other}"),
        }
    }
    Ok(a)
}

fn parse_levels(s: &str) -> Result<Vec<usize>> {
    s.split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(|t| t.parse::<usize>().map_err(anyhow::Error::from))
        .collect()
}

/// Everything the timed sweep owns for one level. The editor + view are driven
/// by the widget; the rest feeds the real `DecoRebuildCtx` the rebuild hook
/// builds each frame.
struct Level {
    editor: Editor,
    view: ViewState,
    cache: DecorationCache,
    paint_cache: PaintCache,
    folds: std::collections::HashSet<u64>,
    theme: Theme,
    diagram_cache: DiagramCacheCtx,
    loaded_text: String,
}

/// The summary numbers for one measured level.
struct LevelResult {
    diagrams: usize,
    bytes: usize,
    stats: Stats,
}

/// Drive one frame through the real editor widget, calling the real decoration
/// rebuild from its `with_decoration_rebuild` hook. Returns the time spent
/// INSIDE `rebuild_editor_layers` only — the cost the caret-move bug is
/// about, isolated from the widget's layout + paint.
///
/// The widget-render gates in the ctx (`render_widgets`, `is_markdown`,
/// `live_preview`) are all ON — that's the configuration where the diagram-widget
/// layers run and the bug manifests. `chart_resolver` is `None` (external-CSV
/// charts aren't part of this measure; inline diagrams still render), so the
/// profiler never touches `app/src/charts.rs`. `dpr` is fixed at 1.0.
fn timed_rebuild(lvl: &mut Level, ctx: &egui::Context, screen: egui::Rect) -> std::time::Duration {
    let raw = egui::RawInput {
        screen_rect: Some(screen),
        ..Default::default()
    };
    let editor_rect =
        egui::Rect::from_min_size(screen.min, egui::vec2(screen.width(), screen.height()));

    // Borrows split so the widget can take `&mut editor` / `&mut view` while the
    // rebuild closure captures the disjoint ctx inputs.
    let Level {
        editor,
        view,
        cache,
        paint_cache,
        folds,
        theme,
        diagram_cache,
        loaded_text,
    } = lvl;
    let font_px = view.font_size;

    let mut elapsed = std::time::Duration::ZERO;
    let mut rebuild = |ed: &Editor, vw: &mut ViewState| {
        let mut deco_ctx = DecoRebuildCtx {
            cache: &mut *cache,
            conflict: None,
            folds,
            loaded_text,
            // No git dirty-diff gutter in the profiling harness. status: git-dirty-diff-gutter
            git_head_text: None,
            theme: Some(theme),
            live_preview: true,
            render_widgets: true,
            is_markdown: true,
            code_language: None,
            dpr: 1.0,
            font_px,
            chunk_boundaries: false,
            show_whitespace: false,
            highlight_trailing_whitespace: false,
            diff: None,
            resolve_title: None,
            diagram_cache: Some(diagram_cache.clone()),
            chart_resolver: None,
            image_resolver: None,
            table_overflow: &EMPTY_TABLE_OVERFLOW,
            editing_table: None,
        };
        let t0 = Instant::now();
        rebuild_editor_layers(ed, vw, &mut deco_ctx);
        elapsed += t0.elapsed();
    };

    let _ = ctx.run(raw, |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            let mut editor_ui = ui.new_child(egui::UiBuilder::new().max_rect(editor_rect));
            let mut clicks = Vec::new();
            let mut txns = Vec::new();
            EditorWidget::new(editor, view)
                .with_click_sink(&mut clicks)
                .with_transactions_sink(&mut txns)
                .with_paint_cache(paint_cache)
                .with_decoration_rebuild(&mut rebuild)
                .show(&mut editor_ui);
        });
    });
    elapsed
}

fn main() -> Result<()> {
    tracing_subscriber::fmt().init();
    let args = parse_args()?;
    run(&args)
}

fn run(args: &Args) -> Result<()> {
    let tmp = std::env::temp_dir().join(format!("profile-buffer-{}", std::process::id()));
    std::fs::create_dir_all(&tmp)?;
    let diagram_cache = DiagramCacheCtx::new(&tmp, /* enabled */ true)
        .ok_or_else(|| anyhow::anyhow!("diagram cache ctx"))?;
    let theme = light_default();
    let ctx = egui::Context::default();
    let screen = egui::Rect::from_min_size(
        egui::pos2(0.0, 0.0),
        egui::vec2(args.width + 16.0, args.height + 16.0),
    );

    tracing::info!(
        levels = ?args.levels,
        moves = args.moves,
        kind = args.kind.label(),
        "buffer decoration-rebuild profile starting",
    );

    let mut results = Vec::new();
    for &n in &args.levels {
        results.push(measure_level(args, n, &theme, &diagram_cache, &ctx, screen));
    }

    let _ = std::fs::remove_dir_all(&tmp);
    print_summary(args, &results);
    Ok(())
}

/// Build the corpus for `n` diagrams, warm the caches once, then time `moves`
/// caret-move rebuilds across a sweep of byte offsets (prose + inside fences).
fn measure_level(
    args: &Args,
    n: usize,
    theme: &Theme,
    diagram_cache: &DiagramCacheCtx,
    ctx: &egui::Context,
    screen: egui::Rect,
) -> LevelResult {
    let doc = synth::doc(n, args.kind);
    let bytes = doc.len();
    let editor = Editor::new(&doc);
    let mut view = ViewState {
        font_size: 14.0,
        line_height: 18.0,
        width: args.width,
        height: args.height,
        ..Default::default()
    };
    view.sync_to(&editor);

    let mut lvl = Level {
        editor,
        view,
        cache: DecorationCache::default(),
        paint_cache: PaintCache::default(),
        folds: std::collections::HashSet::new(),
        theme: theme.clone(),
        diagram_cache: diagram_cache.clone(),
        loaded_text: doc.clone(),
    };

    // Warm: a couple of rebuilds populate the in-memory render cache + disk
    // cache so the timed sweep reflects steady-state cache-hit behavior.
    for _ in 0..2 {
        let _ = timed_rebuild(&mut lvl, ctx, screen);
    }

    let offsets = synth::caret_sweep(&doc, args.moves);
    let mut samples = Vec::with_capacity(offsets.len());
    for &off in &offsets {
        lvl.editor.selection = editor_core::selection::Selection::single(off);
        samples.push(timed_rebuild(&mut lvl, ctx, screen));
    }

    let stats = Stats::summarize(&mut samples);
    tracing::info!(
        diagrams = n,
        bytes,
        moves = samples.len(),
        p50_us = stats.p50 as u64,
        p95_us = stats.p95 as u64,
        max_us = stats.max as u64,
        "level profile",
    );
    LevelResult { diagrams: n, bytes, stats }
}

/// Print the labeled summary table the caller copies into the report.
fn print_summary(args: &Args, results: &[LevelResult]) {
    println!(
        "\n=== buffer decoration-rebuild profile ({} caret moves/level, {} diagrams) ===",
        args.moves,
        args.kind.label(),
    );
    println!(
        "{:>9} {:>9} {:>11} {:>11} {:>11}",
        "diagrams", "bytes", "p50 (ms)", "p95 (ms)", "max (ms)",
    );
    for r in results {
        println!(
            "{:>9} {:>9} {:>11.3} {:>11.3} {:>11.3}",
            r.diagrams,
            r.bytes,
            r.stats.p50 as f64 / 1000.0,
            r.stats.p95 as f64 / 1000.0,
            r.stats.max as f64 / 1000.0,
        );
    }
    println!(
        "\nEach row times only `rebuild_editor_layers` (the diagram-widget \
         layers included), called via the real editor widget's decoration-rebuild \
         hook on every caret move. Cost climbing with the diagram count confirms \
         the whole-document widget layers rebuild on every caret move.",
    );
}
