//! Headless performance profiler for the canvas editor.
//!
//! Drives the real [`canvas_view::widget::CanvasView`] widget through
//! `egui::Context::run()` over a `.canvas` document, rendering every visible
//! node's content through a realistic [`ProfRenderer`] (the same `editor-egui`
//! markdown path the app uses in `app::panels::canvas::content`), then reports
//! per-frame wall-clock timing plus the share of that time spent inside
//! per-node content rendering.
//!
//! The canvas culls nodes geometrically only: at zoom-to-fit every node's
//! on-screen rect intersects the viewport, so EVERY file node runs its full
//! editor layout/paint plus a per-frame decoration rebuild — the bottleneck
//! this tool exists to quantify. At a zoomed-in level only a handful of nodes
//! intersect the viewport, so the per-frame cost drops to whatever those few
//! cards plus the grid/edge painting cost. The two levels bracket the LOD
//! optimization the follow-up will target.
//!
//! This is a measurement tool — it does NOT change any rendering code. It
//! mirrors `tools/profile-scroll`'s harness shape: a default `egui::Context`, a
//! fixed `screen_rect`, state held outside the loop, a warm-up pass so caches
//! settle, per-frame `Instant` timing, and p50/p95/max stats via `tracing`.
//!
//! Usage:
//!   cargo run --release -p profile-canvas -- [OPTIONS]
//!     --canvas <path>    load a `.canvas` document
//!     --vault <dir>      file-resolution root (default: the canvas's parent)
//!     --generate <N>     instead of loading, synthesize N file nodes in a grid
//!                        pointing at real `.md` files found by walking --vault
//!     --frames <N>       timed frames per zoom level (default 60)
//!     --warmup <N>       warm-up frames per zoom level (default 8)
//!     --width <W>        screen width px (default 1600)
//!     --height <H>       screen height px (default 1000)
//!
//! Output is `tracing` info-level plus a labeled summary table at the end.

mod renderer;
mod stats;
mod synth;

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context as _, Result};
use egui::{Event, Pos2, Rect, Vec2};
use hiker_canvas::model::Canvas;

use canvas_view::widget::CanvasView;

use crate::renderer::ProfRenderer;
use crate::stats::Stats;

/// Parsed command-line configuration.
struct Args {
    canvas: Option<PathBuf>,
    vault: Option<PathBuf>,
    generate: Option<usize>,
    frames: usize,
    warmup: usize,
    width: f32,
    height: f32,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            canvas: None,
            vault: None,
            generate: None,
            frames: 60,
            warmup: 8,
            width: 1600.0,
            height: 1000.0,
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
            "--canvas" => a.canvas = Some(PathBuf::from(value()?)),
            "--vault" => a.vault = Some(PathBuf::from(value()?)),
            "--generate" => a.generate = Some(value()?.parse()?),
            "--frames" => a.frames = value()?.parse()?,
            "--warmup" => a.warmup = value()?.parse()?,
            "--width" => a.width = value()?.parse()?,
            "--height" => a.height = value()?.parse()?,
            "-h" | "--help" => {
                println!("see source comment for flags");
                std::process::exit(0);
            }
            other => anyhow::bail!("unknown arg: {other}"),
        }
    }
    Ok(a)
}

/// Resolve the canvas document and the vault root to render against.
fn load(args: &Args) -> Result<(Canvas, PathBuf)> {
    if let Some(n) = args.generate {
        let vault = args
            .vault
            .clone()
            .context("--generate requires --vault for the file pool")?;
        let canvas = synth::grid_canvas(n, &vault)?;
        return Ok((canvas, vault));
    }
    let path = args.canvas.clone().context("need --canvas or --generate")?;
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading canvas {}", path.display()))?;
    let canvas = Canvas::from_json(&text).context("parsing canvas JSON")?;
    let vault = args
        .vault
        .clone()
        .or_else(|| path.parent().map(std::path::Path::to_path_buf))
        .context("could not determine vault root")?;
    Ok((canvas, vault))
}

/// One frame of the canvas view, with optional injected events (used to drive
/// the camera to a zoomed-in level via synthetic pinch-zoom before timing).
fn drive_frame(
    ctx: &egui::Context,
    screen: Rect,
    view: &mut CanvasView,
    canvas: &mut Canvas,
    renderer: &mut ProfRenderer,
    events: Vec<Event>,
) {
    let raw = egui::RawInput {
        screen_rect: Some(screen),
        events,
        ..Default::default()
    };
    let _ = ctx.run(raw, |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            view.show(ui, canvas, renderer, &mut crate::renderer::NoMenus);
        });
    });
}

/// A pinch-zoom event centered on the viewport, plus a pointer-move so the
/// widget sees a hover position inside the viewport (its `handle_zoom` requires
/// one). Used to reach the zoomed-in level without touching the widget's API —
/// the camera has no public mutator, so we drive it the way a user would.
fn zoom_events(center: Pos2, factor: f32) -> Vec<Event> {
    vec![
        Event::PointerMoved(center),
        Event::Zoom(factor),
    ]
}

/// Run the timed sweep for one named zoom level and report its stats. Returns
/// nothing — results go out through `tracing` and the caller's summary.
fn measure_level(level: &Level, run: &mut RunState<'_>) -> LevelResult {
    let center = run.screen.center();
    // Warm-up: settle caches (editor galleys, decoration sets). For the
    // zoomed-in level, spread the synthetic zoom across the warm-up frames so
    // the camera reaches the target scale before timing begins.
    for _ in 0..run.warmup {
        let events = match level.zoom_per_warmup {
            Some(factor) => zoom_events(center, factor),
            None => Vec::new(),
        };
        drive_frame(run.ctx, run.screen, run.view, run.canvas, run.renderer, events);
    }

    let mut frame_times = Vec::with_capacity(run.frames);
    let mut content_us: u128 = 0;
    for _ in 0..run.frames {
        run.renderer.reset_content_timer();
        let t0 = Instant::now();
        drive_frame(run.ctx, run.screen, run.view, run.canvas, run.renderer, Vec::new());
        frame_times.push(t0.elapsed());
        content_us += run.renderer.content_micros();
    }
    // Latched at the top of the last loop iteration: the steady-state count of
    // nodes the view called us for (those whose card intersects the viewport).
    let visible = run.renderer.last_visible();

    let stats = Stats::summarize(&mut frame_times);
    let frame_total_us: u128 = frame_times.iter().map(std::time::Duration::as_micros).sum();
    let share = if frame_total_us == 0 {
        0.0
    } else {
        content_us as f64 / frame_total_us as f64 * 100.0
    };
    tracing::info!(
        level = level.name,
        scale = run.view.camera().scale(),
        visible_nodes = visible,
        p50_us = stats.p50 as u64,
        p95_us = stats.p95 as u64,
        max_us = stats.max as u64,
        content_share_pct = share,
        "canvas zoom-level profile",
    );
    LevelResult {
        name: level.name,
        visible,
        scale: run.view.camera().scale(),
        stats,
        share,
    }
}

/// Mutable state threaded through one zoom-level measurement, grouped so the
/// per-level driver stays under the argument-count budget.
struct RunState<'a> {
    ctx: &'a egui::Context,
    screen: Rect,
    view: &'a mut CanvasView,
    canvas: &'a mut Canvas,
    renderer: &'a mut ProfRenderer,
    frames: usize,
    warmup: usize,
}

/// A zoom level to measure: a label and how to reach it.
struct Level {
    name: &'static str,
    /// `None` → zoom-to-fit (set up by the caller before the run). `Some(f)` →
    /// apply a synthetic pinch factor `f` each warm-up frame to zoom in.
    zoom_per_warmup: Option<f32>,
}

/// The summary numbers for one measured zoom level.
struct LevelResult {
    name: &'static str,
    visible: usize,
    scale: f32,
    stats: Stats,
    share: f64,
}

fn run(args: &Args) -> Result<()> {
    let (mut canvas, vault) = load(args)?;
    let node_count = canvas.nodes.len();
    let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(args.width, args.height));
    let ctx = egui::Context::default();
    let mut view = CanvasView::new();
    let mut renderer = ProfRenderer::new(vault.clone());

    tracing::info!(
        nodes = node_count,
        vault = %vault.display(),
        width = args.width,
        height = args.height,
        "canvas profile starting",
    );

    let mut results = Vec::new();

    // (a) Zoom-to-fit: the worst case — every node intersects the viewport.
    view.fit(screen, &canvas);
    let fit = Level { name: "zoom-to-fit", zoom_per_warmup: None };
    results.push(measure_level(&fit, &mut RunState {
        ctx: &ctx,
        screen,
        view: &mut view,
        canvas: &mut canvas,
        renderer: &mut renderer,
        frames: args.frames,
        warmup: args.warmup,
    }));

    // (b) Zoomed in: from the fit camera, pinch-zoom in over the warm-up frames
    // so only a few cards intersect the viewport (near-1:1 reading zoom).
    let zoomed = Level { name: "zoomed-in", zoom_per_warmup: Some(1.6) };
    results.push(measure_level(&zoomed, &mut RunState {
        ctx: &ctx,
        screen,
        view: &mut view,
        canvas: &mut canvas,
        renderer: &mut renderer,
        frames: args.frames,
        warmup: args.warmup,
    }));

    print_summary(node_count, &results);
    Ok(())
}

/// Print the labeled baseline table the caller copies into the report.
fn print_summary(node_count: usize, results: &[LevelResult]) {
    println!("\n=== canvas profile summary ({node_count} nodes) ===");
    println!(
        "{:<14} {:>8} {:>7} {:>9} {:>9} {:>9} {:>13}",
        "level", "visible", "scale", "p50 (ms)", "p95 (ms)", "max (ms)", "content (%)",
    );
    for r in results {
        println!(
            "{:<14} {:>8} {:>7.3} {:>9.2} {:>9.2} {:>9.2} {:>13.1}",
            r.name,
            r.visible,
            r.scale,
            r.stats.p50 as f64 / 1000.0,
            r.stats.p95 as f64 / 1000.0,
            r.stats.max as f64 / 1000.0,
            r.share,
        );
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt().init();
    let args = parse_args()?;
    run(&args)
}
