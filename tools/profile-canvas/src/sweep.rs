//! The auto-zoom sweep mode (`--mode zoom-sweep`): drive the real
//! [`canvas_view::widget::CanvasView`] over a large synthetic canvas through the
//! full zoom range, from "whole canvas visible" (every card a bare dot /
//! placeholder) to a near zoom (a screenful of full-detail cards), and report the
//! per-frame cost at each zoom step.
//!
//! The point is to SEE the cost curve across the level-of-detail tiers the canvas
//! paints — bare-dot → title-placeholder → full-detail card — and to surface the
//! zoom bands where many cards flip to full-detail (the LOD-transition spikes the
//! later "idle full-detail card caching" optimisation must flatten).
//!
//! Fidelity is identical to the fixed-camera modes: the same [`crate::renderer`]
//! per-node editor + decoration-cache path (parse once per content fingerprint,
//! never per frame), the same real `CanvasView`, caches warmed before each timed
//! pass. Only the camera schedule differs — instead of a few fixed situations it
//! steps the zoom scale geometrically across the corpus's whole visible range.

use egui::Rect;
use hiker_canvas::geometry::{node_bounds, Point};
use hiker_canvas::model::{Canvas, NodeKind};

use canvas_view::widget::CanvasView;

use crate::renderer::ProfRenderer;
use crate::stats::Stats;

/// The bare-dot on-screen width cutoff in screen px, mirrored from
/// `canvas_view::paint::is_bare_dot` (`BARE_DOT_PX`): below this a card paints a
/// single colored dot instead of a title placeholder. Mirrored (the const is
/// crate-private) so the sweep can classify cards into the same tiers the paint
/// layer uses. Kept in sync with that const by hand.
const BARE_DOT_PX: f32 = 36.0;
/// The bare-dot height cutoff in screen px (`BARE_DOT_PX * 0.64` in
/// `is_bare_dot`).
const BARE_DOT_PX_H: f32 = BARE_DOT_PX * 0.64;

/// How a visible card classifies under the canvas LOD ladder at a given camera,
/// mirroring `canvas_view::paint`'s `is_bare_dot` / `is_tiny` tiers.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tier {
    /// Too small to read even a title: a single colored dot. Cheapest.
    BareDot,
    /// Readable as a title but not a body: a placeholder block. Cheap.
    Placeholder,
    /// Renders its full body through the editor path. The expensive tier.
    Full,
}

/// The LOD tier of a node at the view's current camera, from its on-screen rect.
/// `is_bare_dot` is the deepest tier; otherwise `is_tiny` (the placeholder tier,
/// queried via the view's public [`CanvasView::is_node_lod`]); otherwise full.
fn tier_of(view: &CanvasView, viewport: Rect, node: &hiker_canvas::model::Node) -> Tier {
    let screen = view.node_screen_rect(viewport, node);
    if screen.width() < BARE_DOT_PX || screen.height() < BARE_DOT_PX_H {
        Tier::BareDot
    } else if view.is_node_lod(viewport, node) {
        Tier::Placeholder
    } else {
        Tier::Full
    }
}

/// Per-tier counts of the on-screen cards at one zoom step.
#[derive(Clone, Copy, Default)]
struct TierCounts {
    visible: usize,
    full: usize,
    placeholder: usize,
    bare_dot: usize,
}

/// Classify every node at the view's current camera into LOD tiers. Group nodes
/// have no card body, so they are excluded from the visible/full accounting (the
/// content renderer skips them too), matching what the cost numbers reflect.
fn classify(view: &CanvasView, viewport: Rect, canvas: &Canvas) -> TierCounts {
    let mut c = TierCounts::default();
    for node in &canvas.nodes {
        if matches!(node.kind, NodeKind::Group { .. }) {
            continue;
        }
        let screen = view.node_screen_rect(viewport, node);
        if !viewport.intersects(screen) {
            continue;
        }
        c.visible += 1;
        match tier_of(view, viewport, node) {
            Tier::BareDot => c.bare_dot += 1,
            Tier::Placeholder => c.placeholder += 1,
            Tier::Full => c.full += 1,
        }
    }
    c
}

/// The axis-aligned bounds of every node in canvas space, or `None` for an empty
/// canvas. Used to pick the zoomed-out end of the sweep (whole canvas visible).
fn content_bounds(canvas: &Canvas) -> Option<(Point, Point)> {
    let mut it = canvas.nodes.iter();
    let first = it.next()?;
    let b0 = node_bounds(first);
    let mut min = Point::new(b0.x, b0.y);
    let mut max = Point::new(b0.right(), b0.bottom());
    for n in it {
        let b = node_bounds(n);
        min.x = min.x.min(b.x);
        min.y = min.y.min(b.y);
        max.x = max.x.max(b.right());
        max.y = max.y.max(b.bottom());
    }
    Some((min, max))
}

/// The scale (pixels per canvas unit) that frames the whole canvas inside
/// `screen` with a small margin — the zoomed-OUT end of the sweep, where every
/// card is a bare dot or placeholder. Falls back to a tiny scale for an empty /
/// degenerate canvas.
fn fit_scale(screen: Rect, canvas: &Canvas) -> f32 {
    let Some((min, max)) = content_bounds(canvas) else {
        return 0.01;
    };
    let w = (max.x - min.x) as f32;
    let h = (max.y - min.y) as f32;
    if w <= 0.0 || h <= 0.0 {
        return 0.01;
    }
    let pad = 1.1;
    let sx = screen.width() / (w * pad);
    let sy = screen.height() / (h * pad);
    sx.min(sy)
}

/// Position the camera centered on the canvas content at `scale`.
fn center_at(view: &mut CanvasView, screen: Rect, canvas: &Canvas, scale: f32) {
    let center = content_bounds(canvas).map_or(Point::new(0.0, 0.0), |(min, max)| {
        Point::new((min.x + max.x) / 2.0, (min.y + max.y) / 2.0)
    });
    let pan = Point::new(
        center.x - f64::from(screen.width() / 2.0 / scale),
        center.y - f64::from(screen.height() / 2.0 / scale),
    );
    view.restore_view(pan, scale, Vec::new());
}

/// The summary numbers for one zoom step.
struct StepResult {
    scale: f32,
    counts: TierCounts,
    stats: Stats,
}

/// Mutable state threaded through the sweep, grouped so the per-step driver stays
/// within the argument-count budget.
struct SweepState<'a> {
    ctx: &'a egui::Context,
    screen: Rect,
    view: &'a mut CanvasView,
    canvas: &'a mut Canvas,
    renderer: &'a mut ProfRenderer,
    frames: usize,
    warmup: usize,
}

/// Run the whole zoom sweep: `steps` geometric zoom levels from the fit scale up
/// to `near_scale`, each timed over `state.frames` frames after a warm-up. Centers
/// the camera on the content at every step (this measures the static cost of a
/// held view at each zoom, isolating the LOD mix from any pan cost).
fn run_steps(state: &mut SweepState<'_>, steps: usize, near_scale: f32) -> Vec<StepResult> {
    let fit = fit_scale(state.screen, state.canvas);
    let mut results = Vec::with_capacity(steps);
    for i in 0..steps {
        let scale = lerp_geometric(fit, near_scale, i, steps);
        results.push(measure_step(state, scale));
    }
    results
}

/// The geometric interpolation from `lo` to `hi` for step `i` of `n` (`i == 0` is
/// `lo`, `i == n-1` is `hi`). Geometric so the zoom doubles in even visual steps,
/// matching how a user scroll-zooms (a constant multiplier per notch). Falls back
/// to `lo` for a degenerate single-step sweep.
fn lerp_geometric(lo: f32, hi: f32, i: usize, n: usize) -> f32 {
    if n <= 1 {
        return lo;
    }
    let t = i as f32 / (n - 1) as f32;
    lo * (hi / lo).powf(t)
}

/// Measure one zoom step: center at `scale`, warm caches, then time `frames`
/// frames of the held view, classifying the on-screen LOD mix.
fn measure_step(state: &mut SweepState<'_>, scale: f32) -> StepResult {
    center_at(state.view, state.screen, state.canvas, scale);
    for _ in 0..state.warmup {
        drive(state);
    }
    let mut frame_times = Vec::with_capacity(state.frames);
    for _ in 0..state.frames {
        state.renderer.reset_content_timer();
        let t0 = std::time::Instant::now();
        drive(state);
        frame_times.push(t0.elapsed());
    }
    let counts = classify(state.view, state.screen, state.canvas);
    let stats = Stats::summarize(&mut frame_times);
    let actual = state.view.camera().scale();
    tracing::info!(
        scale = actual,
        visible = counts.visible,
        full = counts.full,
        placeholder = counts.placeholder,
        bare_dot = counts.bare_dot,
        p50_us = stats.p50 as u64,
        p95_us = stats.p95 as u64,
        "zoom-sweep step",
    );
    StepResult { scale: actual, counts, stats }
}

/// One frame of the canvas view (no synthetic input — the camera is driven through
/// the view-state seam, so only render cost is exercised). Mirrors the fixed-mode
/// `drive_frame`.
fn drive(state: &mut SweepState<'_>) {
    let raw = egui::RawInput {
        screen_rect: Some(state.screen),
        ..Default::default()
    };
    let _ = state.ctx.run(raw, |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            state
                .view
                .show(ui, state.canvas, state.renderer, &mut crate::renderer::NoMenus);
        });
    });
}

/// Entry point for the sweep mode, called from `main::run`. Builds the harness
/// (real context, view, renderer), runs the steps, and prints the cost-curve
/// table. `near_scale` is the zoomed-in end (a card just above the LOD cutoff,
/// computed by the caller from the corpus).
pub fn run(
    ctx: &egui::Context,
    screen: Rect,
    canvas: &mut Canvas,
    renderer: &mut ProfRenderer,
    config: &SweepConfig,
) {
    let mut view = CanvasView::new();
    let mut state = SweepState {
        ctx,
        screen,
        view: &mut view,
        canvas,
        renderer,
        frames: config.frames,
        warmup: config.warmup,
    };
    let results = run_steps(&mut state, config.steps, config.near_scale);
    print_table(&results);
}

/// Tunables for a sweep run, gathered so the entry point stays within the
/// argument budget.
pub struct SweepConfig {
    /// Number of zoom steps from fit-scale to `near_scale`.
    pub steps: usize,
    /// Timed frames per step.
    pub frames: usize,
    /// Warm-up frames per step (so the first-appearance pane build is excluded).
    pub warmup: usize,
    /// The zoomed-in end of the sweep (pixels per canvas unit).
    pub near_scale: f32,
}

/// Print the per-step cost curve plus the worst-band and transition summaries.
fn print_table(results: &[StepResult]) {
    println!("\n=== canvas zoom-sweep (cost curve, low zoom -> high zoom) ===");
    println!(
        "{:<5} {:>7} {:>7} {:>5} {:>5} {:>4} {:>9} {:>9} {:>9}",
        "step", "scale", "visible", "full", "plc", "dot", "p50 (ms)", "p95 (ms)", "max (ms)",
    );
    for (i, r) in results.iter().enumerate() {
        println!(
            "{:<5} {:>7.4} {:>7} {:>5} {:>5} {:>4} {:>9.2} {:>9.2} {:>9.2}",
            i,
            r.scale,
            r.counts.visible,
            r.counts.full,
            r.counts.placeholder,
            r.counts.bare_dot,
            r.stats.p50 as f64 / 1000.0,
            r.stats.p95 as f64 / 1000.0,
            r.stats.max as f64 / 1000.0,
        );
    }
    print_worst_band(results);
    print_transitions(results);
    println!(
        "\n`full` = on-screen cards rendering their full body (the expensive tier); \
         `plc` = title placeholders; `dot` = bare dots.\nCost should climb as more \
         cards become full-detail at nearer zoom; the bare-dot/placeholder bands are \
         cheap.",
    );
}

/// Print a one-line summary of the slowest (worst p50) zoom band.
fn print_worst_band(results: &[StepResult]) {
    let Some((i, worst)) = results
        .iter()
        .enumerate()
        .max_by_key(|(_, r)| r.stats.p50)
    else {
        return;
    };
    println!(
        "\nworst band: step {i} @ scale {:.4} — {} full-detail cards, p50 {:.2} ms / p95 {:.2} ms",
        worst.scale,
        worst.counts.full,
        worst.stats.p50 as f64 / 1000.0,
        worst.stats.p95 as f64 / 1000.0,
    );
}

/// Print the steps where the full-detail card count jumps the most between
/// consecutive zoom levels — the LOD-transition frames (cards flipping from
/// placeholder to full body) the optimisation should target.
fn print_transitions(results: &[StepResult]) {
    let mut jumps: Vec<(usize, i64)> = results
        .windows(2)
        .enumerate()
        .map(|(i, w)| (i + 1, w[1].counts.full as i64 - w[0].counts.full as i64))
        .collect();
    jumps.sort_by_key(|&(_, delta)| std::cmp::Reverse(delta));
    let top: Vec<&(usize, i64)> = jumps.iter().filter(|(_, d)| *d > 0).take(3).collect();
    if top.is_empty() {
        return;
    }
    println!("LOD transitions (largest jumps in full-detail count):");
    for &(step, delta) in top {
        let r = &results[step];
        println!(
            "  -> step {step} @ scale {:.4}: +{delta} cards became full-detail (now {}), p50 {:.2} ms",
            r.scale,
            r.counts.full,
            r.stats.p50 as f64 / 1000.0,
        );
    }
}
