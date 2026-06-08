//! Headless performance profiler for the canvas editor.
//!
//! Drives the real [`canvas_view::widget::CanvasView`] widget through
//! `egui::Context::run()` over a `.canvas` document, rendering every visible
//! node's content through a realistic [`ProfRenderer`] (the same `editor-egui`
//! markdown path the app uses in `app::panels::canvas::content`), then reports
//! per-frame wall-clock timing plus the share of that time spent inside
//! per-node content rendering.
//!
//! It measures three camera situations that bracket the canvas's cost curve:
//!
//!   * `zoom-to-fit` — the geometric worst case for *breadth*: every node's
//!     on-screen rect intersects the viewport, but each is below the LOD
//!     threshold so the view paints cheap placeholders. Lots of nodes, each cheap.
//!   * `full-detail` — the camera positioned at a scale just above the LOD
//!     threshold (`canvas-lod-placeholder`), held STILL. The most full-detail
//!     cards that can be on screen at once while LOD is *not* in effect — every
//!     visible card runs its full editor layout/paint + per-frame decoration
//!     rebuild. The static cost of reading at the readable zoom.
//!   * `scroll` — the same full-detail scale, but the camera PANS a fixed number
//!     of screen pixels every timed frame, so full-detail cards stream through
//!     the viewport. This is the case this tool exists to characterise: scrolling
//!     when LOD is *not* in effect. Comparing it against `full-detail` isolates
//!     the marginal per-frame cost of panning a viewport full of live cards
//!     (cards entering the cull set, decoration rebuilds on every moving card)
//!     from the static render cost.
//!
//! The two full-detail levels share a target scale computed from the corpus so a
//! card sits just above the LOD cutoff — the worst case for scroll, since that is
//! where the *maximum* number of full-detail cards fit on screen at once.
//!
//! This is a measurement tool — it does NOT change any rendering code. It mirrors
//! `tools/profile-scroll`'s harness shape: a default `egui::Context`, a fixed
//! `screen_rect`, state held outside the loop, a warm-up pass so caches settle,
//! per-frame `Instant` timing, and p50/p95/max stats via `tracing`.
//!
//! There is also an auto-zoom SWEEP mode (`--mode zoom-sweep`) that drives a large
//! synthetic canvas through the full zoom range — from "whole canvas visible"
//! (every card a bare dot / placeholder) to a near zoom (a screenful of
//! full-detail cards) — and reports the per-frame cost at each zoom step, so the
//! LOD-transition cost curve (bare-dot → title-placeholder → full-detail) is
//! visible. See [`crate::sweep`].
//!
//! Usage:
//!   cargo run --release -p profile-canvas -- [OPTIONS]
//!     --mode <m>           `levels` (default: the three fixed-camera situations)
//!                          or `zoom-sweep` (the auto-zoom cost-curve sweep)
//!     --canvas <path>      load a `.canvas` document
//!     --vault <dir>        file-resolution root (default: the canvas's parent)
//!     --generate <N>       instead of loading, synthesize N file nodes in a grid
//!                          pointing at real `.md` files found by walking --vault.
//!                          In zoom-sweep mode this defaults to 800 and the grid
//!                          gains edges between neighbouring cards.
//!     --frames <N>         timed frames per level/step (default 60 levels, 5 sweep)
//!     --warmup <N>         warm-up frames before each timed pass (default 8)
//!     --steps <N>          zoom steps for zoom-sweep, fit-scale → near (default 40)
//!     --scroll-step <px>   camera pan per timed frame for the scroll level, in
//!                          screen px (default 6); larger = faster scroll
//!     --scroll-scale <s>   override the auto-computed full-detail scale (pixels
//!                          per canvas unit) used by `full-detail`/`scroll`, and the
//!                          near (zoomed-in) end of the zoom-sweep
//!     --width <W>          screen width px (default 1600)
//!     --height <H>         screen height px (default 1000)
//!
//! Output is `tracing` info-level plus a labeled summary table at the end.

mod renderer;
mod stats;
mod sweep;
mod synth;

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context as _, Result};
use egui::{Pos2, Rect, Vec2};
use hiker_canvas::geometry::Point;
use hiker_canvas::model::{Canvas, NodeKind};

use canvas_view::widget::CanvasView;

use crate::renderer::ProfRenderer;
use crate::stats::Stats;

/// The LOD width cutoff in screen px, mirrored from `canvas_view::paint::is_tiny`
/// (`LOD_MIN_PX`): below this a card paints a placeholder instead of its body.
/// Mirrored (not imported — it is crate-private) so the profiler can target a
/// scale that sits just above it. Kept in sync with that const by hand.
const LOD_MIN_PX: f32 = 150.0;
/// The LOD height cutoff in screen px (`LOD_MIN_PX * 0.64` in `is_tiny`).
const LOD_MIN_PX_H: f32 = LOD_MIN_PX * 0.64;

/// Which profiling mode to run.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// The three fixed-camera situations (zoom-to-fit / full-detail / scroll).
    Levels,
    /// The auto-zoom cost-curve sweep over a large synthetic canvas.
    ZoomSweep,
}

/// The default number of zoom steps for the sweep.
const DEFAULT_SWEEP_STEPS: usize = 40;
/// The default timed frames per zoom step (the sweep has many steps, so fewer
/// frames each keeps a run fast while p50/p95 stay meaningful).
const DEFAULT_SWEEP_FRAMES: usize = 5;
/// The default synthetic card count when `--generate` is omitted in sweep mode.
const DEFAULT_SWEEP_CARDS: usize = 800;

/// Parsed command-line configuration.
struct Args {
    mode: Mode,
    canvas: Option<PathBuf>,
    vault: Option<PathBuf>,
    generate: Option<usize>,
    frames: Option<usize>,
    warmup: usize,
    steps: usize,
    scroll_step: f32,
    scroll_scale: Option<f32>,
    width: f32,
    height: f32,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            mode: Mode::Levels,
            canvas: None,
            vault: None,
            generate: None,
            frames: None,
            warmup: 8,
            steps: DEFAULT_SWEEP_STEPS,
            scroll_step: 6.0,
            scroll_scale: None,
            width: 1600.0,
            height: 1000.0,
        }
    }
}

impl Args {
    /// Timed frames per level/step, defaulting per mode (the sweep wants fewer per
    /// step since there are many steps).
    fn frames(&self) -> usize {
        self.frames.unwrap_or(match self.mode {
            Mode::Levels => 60,
            Mode::ZoomSweep => DEFAULT_SWEEP_FRAMES,
        })
    }
}

/// Parse a `--mode` value into a [`Mode`].
fn parse_mode(s: &str) -> Result<Mode> {
    match s {
        "levels" => Ok(Mode::Levels),
        "zoom-sweep" => Ok(Mode::ZoomSweep),
        other => anyhow::bail!("unknown --mode: {other} (want `levels` or `zoom-sweep`)"),
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
            "--mode" => a.mode = parse_mode(&value()?)?,
            "--canvas" => a.canvas = Some(PathBuf::from(value()?)),
            "--vault" => a.vault = Some(PathBuf::from(value()?)),
            "--generate" => a.generate = Some(value()?.parse()?),
            "--frames" => a.frames = Some(value()?.parse()?),
            "--warmup" => a.warmup = value()?.parse()?,
            "--steps" => a.steps = value()?.parse()?,
            "--scroll-step" => a.scroll_step = value()?.parse()?,
            "--scroll-scale" => a.scroll_scale = Some(value()?.parse()?),
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
    // The sweep wants a large EDGED grid and synthesises one by default (no
    // `--canvas` needed); the fixed modes only synthesise on an explicit
    // `--generate`.
    let synth_count = match args.mode {
        Mode::ZoomSweep => Some(args.generate.unwrap_or(DEFAULT_SWEEP_CARDS)),
        Mode::Levels => args.generate,
    };
    if let Some(n) = synth_count {
        let vault = args
            .vault
            .clone()
            .context("synthesising a canvas requires --vault for the file pool")?;
        let canvas = match args.mode {
            Mode::ZoomSweep => synth::sweep_canvas(n, &vault)?,
            Mode::Levels => synth::grid_canvas(n, &vault)?,
        };
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

/// The axis-aligned bounds of every node in canvas space, or `None` for an empty
/// canvas. Used to center the camera for the full-detail levels.
fn content_bounds(canvas: &Canvas) -> Option<(Point, Point)> {
    let mut it = canvas.nodes.iter();
    let first = it.next()?;
    let mut min = Point::new(first.x as f64, first.y as f64);
    let mut max = Point::new((first.x + first.width) as f64, (first.y + first.height) as f64);
    for n in it {
        min.x = min.x.min(n.x as f64);
        min.y = min.y.min(n.y as f64);
        max.x = max.x.max((n.x + n.width) as f64);
        max.y = max.y.max((n.y + n.height) as f64);
    }
    Some((min, max))
}

/// The center of the canvas content in canvas space (origin if empty).
fn content_center(canvas: &Canvas) -> Point {
    content_bounds(canvas).map_or(Point::new(0.0, 0.0), |(min, max)| {
        Point::new((min.x + max.x) / 2.0, (min.y + max.y) / 2.0)
    })
}

/// The camera scale (pixels per canvas unit) that puts a *typical* card just
/// above the LOD cutoff — the worst case for scrolling, where the most
/// full-detail cards fit on screen at once. Computed from the median non-group
/// card size so one oversized group or tiny stray node doesn't skew it, then
/// nudged 10% past the cutoff so the typical card renders its body (not a
/// placeholder). Falls back to `1.0` when there are no sizeable cards.
fn full_detail_scale(canvas: &Canvas) -> f32 {
    let mut widths: Vec<f32> = Vec::new();
    let mut heights: Vec<f32> = Vec::new();
    for n in &canvas.nodes {
        if matches!(n.kind, NodeKind::Group { .. }) {
            continue;
        }
        widths.push(n.width.max(1) as f32);
        heights.push(n.height.max(1) as f32);
    }
    if widths.is_empty() {
        return 1.0;
    }
    widths.sort_by(f32::total_cmp);
    heights.sort_by(f32::total_cmp);
    let med = |v: &[f32]| v[v.len() / 2];
    let by_w = LOD_MIN_PX / med(&widths);
    let by_h = LOD_MIN_PX_H / med(&heights);
    // A card clears LOD only when it satisfies BOTH cutoffs, so take the larger
    // (more zoomed-in) requirement, then nudge just past it.
    (by_w.max(by_h) * 1.1).clamp(0.05, 20.0)
}

/// Count how many nodes are on screen and how many of those are below the LOD
/// cutoff (painting placeholders) at the view's current camera. An honesty check
/// for the scroll level: it should report ~0 LOD nodes, confirming LOD is genuinely
/// not in effect during the scroll measurement.
fn visible_and_lod(view: &CanvasView, viewport: Rect, canvas: &Canvas) -> (usize, usize) {
    let (mut visible, mut lod) = (0, 0);
    for node in &canvas.nodes {
        let screen = view.node_screen_rect(viewport, node);
        if !viewport.intersects(screen) {
            continue;
        }
        visible += 1;
        if view.is_node_lod(viewport, node) {
            lod += 1;
        }
    }
    (visible, lod)
}

/// Center the camera on the content at `scale`. When `back_off` is non-zero the
/// camera starts that many screen px *before* center along the scroll direction,
/// so a subsequent pan sweep passes through the densest middle of the corpus
/// rather than immediately running off one edge.
fn position_camera(view: &mut CanvasView, screen: Rect, canvas: &Canvas, scale: f32, back_off: Vec2) {
    let c = content_center(canvas);
    // pan = the canvas point at the viewport's top-left. Put `c` at the viewport
    // center, then shift back along the scroll direction by `back_off` (screen px
    // → canvas units at this scale).
    let pan = Point::new(
        c.x - f64::from(screen.width() / 2.0 / scale) - f64::from(back_off.x / scale),
        c.y - f64::from(screen.height() / 2.0 / scale) - f64::from(back_off.y / scale),
    );
    view.restore_view(pan, scale, Vec::new());
}

/// Advance the camera by `step` screen px (a single scroll increment). Goes
/// through the public view-state seam — there is no direct camera mutator — by
/// snapshotting pan/scale and restoring them shifted. Card view state rides
/// along untouched. The pane cache (the expensive editor state) lives in
/// `ProfRenderer`, not here, so it survives across the advance.
fn pan_camera(view: &mut CanvasView, step: Vec2) {
    let (pan, scale, cards) = view.view_snapshot();
    let shifted = Point::new(
        pan.x + f64::from(step.x / scale),
        pan.y + f64::from(step.y / scale),
    );
    view.restore_view(shifted, scale, cards);
}

/// One frame of the canvas view. No synthetic input events — the camera is
/// driven directly through the view-state seam (see [`pan_camera`]), since the
/// measurement target is render cost, not the input handlers.
fn drive_frame(
    ctx: &egui::Context,
    screen: Rect,
    view: &mut CanvasView,
    canvas: &mut Canvas,
    renderer: &mut ProfRenderer,
) {
    let raw = egui::RawInput {
        screen_rect: Some(screen),
        ..Default::default()
    };
    let _ = ctx.run(raw, |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            view.show(ui, canvas, renderer, &mut crate::renderer::NoMenus);
        });
    });
}

/// Run the timed sweep for one named level and report its stats.
fn measure_level(level: &Level, run: &mut RunState<'_>) -> LevelResult {
    // Position the camera and warm caches so the timed frames are steady-state.
    setup_and_warm(level, run);

    let mut frame_times = Vec::with_capacity(run.frames);
    let mut content_us: u128 = 0;
    for _ in 0..run.frames {
        // Advance the scroll BEFORE timing so only the render is measured, never
        // the camera bookkeeping. Static levels pass a zero step (no-op pan).
        pan_camera(run.view, level.scroll);
        run.renderer.reset_content_timer();
        let t0 = Instant::now();
        drive_frame(run.ctx, run.screen, run.view, run.canvas, run.renderer);
        frame_times.push(t0.elapsed());
        content_us += run.renderer.content_micros();
    }
    // Latched at the top of the last loop iteration: the steady-state count of
    // full-detail cards the view called us to render (those whose card is on
    // screen and above the LOD cutoff).
    let full_detail = run.renderer.last_visible();
    let (visible, lod) = visible_and_lod(run.view, run.screen, run.canvas);

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
        full_detail,
        lod,
        p50_us = stats.p50 as u64,
        p95_us = stats.p95 as u64,
        max_us = stats.max as u64,
        content_share_pct = share,
        "canvas level profile",
    );
    LevelResult {
        name: level.name,
        visible,
        full_detail,
        lod,
        scale: run.view.camera().scale(),
        stats,
        share,
    }
}

/// Position the camera for `level` and run its warm-up pass. Scroll levels warm
/// by panning across the *whole* timed travel once (un-timed) so every card that
/// will stream past has its editor pane built before measurement — otherwise a
/// card's first appearance pays a one-time pane build that shows up as a spike,
/// not the steady-state scroll cost. The camera is then reset to the start.
fn setup_and_warm(level: &Level, run: &mut RunState<'_>) {
    let scrolling = level.scroll != Vec2::ZERO;
    // Start half the timed travel before center so the sweep crosses the middle.
    let back_off = level.scroll * (run.frames as f32 / 2.0);

    match level.scale {
        None => run.view.fit(run.screen, run.canvas),
        Some(scale) => position_camera(run.view, run.screen, run.canvas, scale, back_off),
    }

    if scrolling {
        // Un-timed warm pass over the full travel, building panes as cards appear.
        for _ in 0..run.frames {
            pan_camera(run.view, level.scroll);
            drive_frame(run.ctx, run.screen, run.view, run.canvas, run.renderer);
        }
        // Reset to the start of the sweep for the timed pass.
        if let Some(scale) = level.scale {
            position_camera(run.view, run.screen, run.canvas, scale, back_off);
        }
    } else {
        for _ in 0..run.warmup {
            drive_frame(run.ctx, run.screen, run.view, run.canvas, run.renderer);
        }
    }
}

/// Mutable state threaded through one level measurement, grouped so the per-level
/// driver stays under the argument-count budget.
struct RunState<'a> {
    ctx: &'a egui::Context,
    screen: Rect,
    view: &'a mut CanvasView,
    canvas: &'a mut Canvas,
    renderer: &'a mut ProfRenderer,
    frames: usize,
    warmup: usize,
}

/// A level to measure: a label, the camera scale to reach (`None` = zoom-to-fit),
/// and the per-timed-frame camera pan in screen px (`ZERO` = static).
struct Level {
    name: &'static str,
    scale: Option<f32>,
    scroll: Vec2,
}

/// The summary numbers for one measured level.
struct LevelResult {
    name: &'static str,
    visible: usize,
    full_detail: usize,
    lod: usize,
    scale: f32,
    stats: Stats,
    share: f64,
}

fn run(args: &Args) -> Result<()> {
    let (mut canvas, vault) = load(args)?;
    match args.mode {
        Mode::Levels => run_levels(args, &mut canvas, &vault),
        Mode::ZoomSweep => run_sweep(args, &mut canvas, &vault),
    }
    Ok(())
}

/// How far past the just-above-LOD scale the auto-computed sweep zooms in: the
/// `full_detail_scale` lands a card right at the LOD cutoff (1 full step), but the
/// sweep wants several full-detail bands — a screenful of full cards thinning to a
/// few — so the cost of the expensive tier is clearly characterised. Overridden by
/// an explicit `--scroll-scale`.
const SWEEP_NEAR_OVERSHOOT: f32 = 3.0;

/// Run the auto-zoom cost-curve sweep over `canvas`: a large synthetic board swept
/// from whole-canvas-visible to a near zoom, reporting the per-frame cost as cards
/// cross the LOD tiers. The near end is a few multiples past the just-above-LOD
/// scale the fixed-mode full-detail level uses (so the full-detail tail spans
/// several steps), overridable with `--scroll-scale`.
fn run_sweep(args: &Args, canvas: &mut Canvas, vault: &std::path::Path) {
    let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(args.width, args.height));
    let ctx = egui::Context::default();
    let mut renderer = ProfRenderer::new(vault.to_path_buf());
    let near_scale = args
        .scroll_scale
        .unwrap_or_else(|| full_detail_scale(canvas) * SWEEP_NEAR_OVERSHOOT);
    tracing::info!(
        nodes = canvas.nodes.len(),
        edges = canvas.edges.len(),
        vault = %vault.display(),
        steps = args.steps,
        frames = args.frames(),
        near_scale,
        "canvas zoom-sweep starting",
    );
    sweep::run(&ctx, screen, canvas, &mut renderer, &sweep::SweepConfig {
        steps: args.steps.max(1),
        frames: args.frames(),
        warmup: args.warmup,
        near_scale,
    });
}

/// Run the three fixed-camera situations (the original profiler behaviour).
fn run_levels(args: &Args, canvas: &mut Canvas, vault: &std::path::Path) {
    let node_count = canvas.nodes.len();
    let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(args.width, args.height));
    let ctx = egui::Context::default();
    let mut view = CanvasView::new();
    let mut renderer = ProfRenderer::new(vault.to_path_buf());

    let detail_scale = args.scroll_scale.unwrap_or_else(|| full_detail_scale(canvas));
    // Scroll diagonally (mostly horizontal) so the sweep streams new columns and
    // rows past the cull boundary, not just one axis.
    let step = Vec2::new(args.scroll_step, args.scroll_step * 0.4);

    tracing::info!(
        nodes = node_count,
        vault = %vault.display(),
        width = args.width,
        height = args.height,
        detail_scale,
        scroll_step = args.scroll_step,
        "canvas profile starting",
    );

    let levels = [
        // (a) Zoom-to-fit: every node on screen, but each below LOD → placeholders.
        Level { name: "zoom-to-fit", scale: None, scroll: Vec2::ZERO },
        // (b) Full-detail, held still: the readable zoom, LOD not in effect.
        Level { name: "full-detail", scale: Some(detail_scale), scroll: Vec2::ZERO },
        // (c) Full-detail, scrolling: the headline case — panning a viewport full
        //     of live cards. Subtract (b) to read the marginal cost of scrolling.
        Level { name: "scroll", scale: Some(detail_scale), scroll: step },
    ];

    let mut results = Vec::new();
    for level in &levels {
        results.push(measure_level(level, &mut RunState {
            ctx: &ctx,
            screen,
            view: &mut view,
            canvas,
            renderer: &mut renderer,
            frames: args.frames(),
            warmup: args.warmup,
        }));
    }

    print_summary(node_count, &results);
}

/// Print the labeled baseline table the caller copies into the report.
fn print_summary(node_count: usize, results: &[LevelResult]) {
    println!("\n=== canvas profile summary ({node_count} nodes) ===");
    println!(
        "{:<13} {:>7} {:>6} {:>4} {:>6} {:>9} {:>9} {:>9} {:>11}",
        "level", "visible", "full", "lod", "scale", "p50 (ms)", "p95 (ms)", "max (ms)", "content (%)",
    );
    for r in results {
        println!(
            "{:<13} {:>7} {:>6} {:>4} {:>6.3} {:>9.2} {:>9.2} {:>9.2} {:>11.1}",
            r.name,
            r.visible,
            r.full_detail,
            r.lod,
            r.scale,
            r.stats.p50 as f64 / 1000.0,
            r.stats.p95 as f64 / 1000.0,
            r.stats.max as f64 / 1000.0,
            r.share,
        );
    }
    println!(
        "\n`full` = on-screen cards rendering their full body (LOD off); `lod` = \
         on-screen cards painting a placeholder.\nscroll vs full-detail isolates \
         the per-frame cost of panning a viewport full of live cards.",
    );
}

fn main() -> Result<()> {
    tracing_subscriber::fmt().init();
    let args = parse_args()?;
    run(&args)
}
