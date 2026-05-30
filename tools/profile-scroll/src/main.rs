//! Headless scroll profiler for the editor.
//!
//! Drives the real [`editor_egui::widget::Widget`] (and optionally the
//! [`editor_egui::minimap::Widget`]) through `egui::Context::run()` with a
//! synthetic Markdown document and a deterministic scroll sweep, then reports
//! per-frame wall-clock timing and (optionally) writes `dhat-heap.json` for
//! allocation-site attribution.
//!
//! egui's font system and per-frame layout (galleys, the minimap rasterizer)
//! all run on CPU and work without a GPU backend, so this captures the bulk
//! of the per-frame cost we care about — only the GPU upload itself is
//! missing. The harness deliberately omits the host app's full decoration
//! stack to keep the tool self-contained; flags toggle in the real
//! `markdown_decorations` layer (full-doc Mark/Replace coverage that drives
//! wrap geometry) and a synthetic viewport-scoped layer that mimics how
//! wikilink/transclusion etc. churn `decorations.signature` on every scroll.
//!
//! Usage:
//!   cargo run --release -p profile-scroll -- [OPTIONS]
//!     --lines N            doc size (default 5000)
//!     --frames N           frames to simulate (default 240)
//!     --scroll-px F        scroll delta per frame in px (default 60)
//!     --width W            editor width px (default 900)
//!     --height H           editor height px (default 700)
//!     --minimap            include the minimap widget
//!     --markdown           push real editor-md markdown_decorations
//!     --viewport-scoped    push a synthetic viewport-scoped Replace layer
//!     --no-wrap            disable soft wrap (default: enabled)
//!     --heap               enable dhat heap profiling
//!
//! Output is `tracing` info-level + (optional) `dhat-heap.json` in cwd.

use std::time::{Duration, Instant};

use anyhow::Result;
use editor_core::decoration::{Decoration, Set as DecorationSet};
use editor_core::rangeset::RangeSet;
use editor_core::state::Editor;
use editor_egui::minimap::{Options as MinimapOptions, Widget as MinimapWidget};
use editor_egui::widget::{PaintCache, Widget as EditorWidget};
use editor_view::viewport::ViewState;

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

#[derive(Clone)]
struct Args {
    lines: usize,
    frames: usize,
    scroll_px: f32,
    width: f32,
    height: f32,
    minimap: bool,
    markdown: bool,
    viewport_scoped: bool,
    wrap: bool,
    heap: bool,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            lines: 5000,
            frames: 240,
            scroll_px: 60.0,
            width: 900.0,
            height: 700.0,
            minimap: false,
            markdown: false,
            viewport_scoped: false,
            wrap: true,
            heap: false,
        }
    }
}

fn parse_args() -> Result<Args> {
    let mut a = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let next_val = |it: &mut std::iter::Skip<std::env::Args>| -> Result<String> {
            it.next()
                .ok_or_else(|| anyhow::anyhow!("missing value for {arg}"))
        };
        match arg.as_str() {
            "--lines" => a.lines = next_val(&mut it)?.parse()?,
            "--frames" => a.frames = next_val(&mut it)?.parse()?,
            "--scroll-px" => a.scroll_px = next_val(&mut it)?.parse()?,
            "--width" => a.width = next_val(&mut it)?.parse()?,
            "--height" => a.height = next_val(&mut it)?.parse()?,
            "--minimap" => a.minimap = true,
            "--markdown" => a.markdown = true,
            "--viewport-scoped" => a.viewport_scoped = true,
            "--no-wrap" => a.wrap = false,
            "--heap" => a.heap = true,
            "-h" | "--help" => {
                println!("see source comment for flags");
                std::process::exit(0);
            }
            other => anyhow::bail!("unknown arg: {other}"),
        }
    }
    Ok(a)
}

/// Build a representative Markdown corpus. Mixes headings (which carry
/// `font_scale` Marks → drives prewrap's per-line scale logic), bold/italic
/// runs (Replace decorations that hide markers → drives the spans path), list
/// items (Replace for the bullet), and plain text. Cycling pattern keeps the
/// generator small while reproducing the variety the real wrap path sees.
fn synthetic_doc(lines: usize) -> String {
    let mut s = String::with_capacity(lines * 80);
    for i in 0..lines {
        match i % 12 {
            0 => s.push_str(&format!("# Heading {i}\n")),
            1 => s.push_str(&format!("## Subheading {i}\n")),
            2 => s.push_str("\n"),
            3 => s.push_str(&format!(
                "This is **bold** and *italic* text on line {i}, with some `inline code` too.\n"
            )),
            4 => s.push_str(&format!(
                "- list item {i} with more **emphasis** and a [[wikilink target]] reference\n"
            )),
            5 => s.push_str(&format!(
                "1. ordered item {i} mentioning ~~strikethrough~~ and an inline ![alt](img.png)\n"
            )),
            6 => s.push_str(&format!(
                "Plain paragraph line {i}. Lorem ipsum dolor sit amet, consectetur adipiscing elit, \
                 sed do eiusmod tempor incididunt ut labore et dolore magna aliqua.\n"
            )),
            7 => s.push_str("> blockquote line with **bold** content\n"),
            8 => s.push_str(&format!("    code-indented line {i}\n")),
            9 => s.push_str(&format!(
                "Another long paragraph line {i} that should soft-wrap at the configured editor \
                 width and exercise the per-line wrap cost when scrolling through.\n"
            )),
            10 => s.push_str("---\n"),
            _ => s.push_str(&format!("trailing line {i} with [link](https://example.com)\n")),
        }
    }
    s
}

/// Synthetic viewport-scoped decoration: emit a `Replace` (hidden, cols=0) on
/// the first few characters of every visible line. Mirrors the cost shape of
/// wikilink/transclusion/etc. in the real app — those layers are rebuilt
/// every scroll frame, generating fresh `Arc`s that flip
/// `decorations.signature` and invalidate the per-line galley cache. The
/// goal isn't accuracy of what they display, only fidelity of the cache-bust
/// pattern.
fn viewport_scoped_layer(editor: &Editor, visible_lines: std::ops::Range<usize>) -> DecorationSet {
    let last_line = editor.doc.len_lines().saturating_sub(1);
    let mut ranges: Vec<(std::ops::Range<usize>, Decoration)> = Vec::new();
    for line in visible_lines.start..visible_lines.end.min(last_line + 1) {
        let line_start = editor.doc.line_to_byte(line);
        let next_start = if line + 1 <= last_line {
            editor.doc.line_to_byte(line + 1)
        } else {
            editor.doc.len_bytes()
        };
        let line_len = next_start.saturating_sub(line_start);
        // Replace the first up-to-2 bytes of the line, ASCII-safe (synthetic
        // doc is ASCII so any short prefix is on a char boundary). Skip empty
        // lines.
        if line_len >= 2 {
            ranges.push((
                line_start..line_start + 2,
                Decoration::Replace { display: None },
            ));
        }
    }
    RangeSet::from_iter(ranges)
}

/// Frame-time stats reported as microseconds.
struct Stats {
    mean: u128,
    p50: u128,
    p95: u128,
    p99: u128,
    max: u128,
}

fn summarize(samples: &mut [Duration]) -> Stats {
    samples.sort();
    let n = samples.len();
    let pct = |p: usize| samples[((n * p) / 100).min(n - 1)].as_micros();
    let mean = samples.iter().map(std::time::Duration::as_micros).sum::<u128>() / n as u128;
    Stats {
        mean,
        p50: pct(50),
        p95: pct(95),
        p99: pct(99),
        max: samples.last().unwrap().as_micros(),
    }
}

fn run(args: &Args) {
    let text = synthetic_doc(args.lines);
    let mut editor = Editor::new(&text);
    let mut view = ViewState {
        font_size: 14.0,
        line_height: 18.0,
        width: args.width,
        height: args.height,
        ..Default::default()
    };
    view.wrap_map.set_enabled(args.wrap);
    view.sync_to(&editor);

    let mut paint_cache = PaintCache::default();
    let mut mm_cache = editor_egui::minimap::Cache::default();
    // Cache the markdown layer by doc_id, mirroring how the host app stores
    // it: rebuilt only when the doc changes, so its `Arc` (and the
    // `geometry_epoch` it contributes to) stays stable across pure-scroll
    // frames. Without this the tool's numbers would overstate the on-scroll
    // cost of the markdown decorator and mask the partial-walk wins.
    let mut markdown_cache: Option<(usize, DecorationSet)> = None;

    let ctx = egui::Context::default();

    // Pick a screen big enough for the editor + a minimap strip beside it.
    let mm_w: f32 = if args.minimap { 80.0 } else { 0.0 };
    let screen = egui::Rect::from_min_size(
        egui::pos2(0.0, 0.0),
        egui::vec2(args.width + mm_w + 16.0, args.height + 16.0),
    );

    // Run one warm-up frame so font metrics, height map, and caches settle
    // before we start sampling.
    let mut scroll_y = 0.0f32;
    drive_frame(&ctx, screen, args, &mut editor, &mut view, &mut paint_cache, &mut mm_cache, &mut markdown_cache, scroll_y);

    let mut frame_times = Vec::with_capacity(args.frames);
    let total_start = Instant::now();

    // Approximate scroll range; we wrap around at the end.
    let scroll_range = (args.lines as f32 * view.line_height - args.height).max(1.0);

    for f in 0..args.frames {
        // Deterministic sweep: advance by `scroll_px` each frame, wrapping
        // when we hit the end so a long run keeps moving.
        scroll_y = ((f as f32 + 1.0) * args.scroll_px) % scroll_range;

        let t0 = Instant::now();
        drive_frame(&ctx, screen, args, &mut editor, &mut view, &mut paint_cache, &mut mm_cache, &mut markdown_cache, scroll_y);
        frame_times.push(t0.elapsed());
    }

    let total = total_start.elapsed();
    let stats = summarize(&mut frame_times);
    tracing::info!(
        lines = args.lines,
        frames = args.frames,
        minimap = args.minimap,
        markdown = args.markdown,
        viewport_scoped = args.viewport_scoped,
        wrap = args.wrap,
        total_ms = total.as_millis() as u64,
        mean_us = stats.mean as u64,
        p50_us = stats.p50 as u64,
        p95_us = stats.p95 as u64,
        p99_us = stats.p99 as u64,
        max_us = stats.max as u64,
        "scroll profile complete",
    );
}

#[allow(clippy::too_many_arguments)]
fn drive_frame(
    ctx: &egui::Context,
    screen: egui::Rect,
    args: &Args,
    editor: &mut Editor,
    view: &mut ViewState,
    paint_cache: &mut PaintCache,
    mm_cache: &mut editor_egui::minimap::Cache,
    markdown_cache: &mut Option<(usize, DecorationSet)>,
    scroll_y: f32,
) {
    // Set scroll directly. We're profiling the per-frame work the widget
    // does in response to a scroll position change, not the input-event
    // handling that translates wheel/key events into scroll_y. Setting
    // directly keeps the harness deterministic.
    view.scroll_y = scroll_y;

    let raw_input = egui::RawInput {
        screen_rect: Some(screen),
        ..Default::default()
    };
    let _ = ctx.run(raw_input, |ctx| {
        egui::CentralPanel::default().show(ctx, |ui| {
            // Editor occupies the left part of the panel.
            let editor_rect = egui::Rect::from_min_size(
                ui.min_rect().min,
                egui::vec2(args.width, args.height),
            );
            let mut editor_ui =
                ui.new_child(egui::UiBuilder::new().max_rect(editor_rect));

            let mut clicks = Vec::new();
            let mut txns = Vec::new();

            let mut rebuild = |state: &Editor, view: &mut ViewState| {
                view.decorations.clear();
                if args.markdown {
                    let doc_id = state.doc.content_id();
                    let set = match markdown_cache {
                        Some((id, cached)) if *id == doc_id => cached.clone(),
                        _ => {
                            let s = editor_md::styling::markdown_decorations(state, None);
                            *markdown_cache = Some((doc_id, s.clone()));
                            s
                        }
                    };
                    view.decorations.push_with_heights(set);
                }
                if args.viewport_scoped {
                    let vis = view.visible_lines();
                    let set = viewport_scoped_layer(state, vis);
                    view.decorations.push_viewport_scoped(set);
                }
            };

            EditorWidget::new(editor, view)
                .with_click_sink(&mut clicks)
                .with_transactions_sink(&mut txns)
                .with_paint_cache(paint_cache)
                .with_decoration_rebuild(&mut rebuild)
                .show(&mut editor_ui);

            if args.minimap {
                let mm_rect = egui::Rect::from_min_size(
                    egui::pos2(editor_rect.right(), editor_rect.top()),
                    egui::vec2(80.0, args.height),
                );
                let mut mm_ui =
                    ui.new_child(egui::UiBuilder::new().max_rect(mm_rect));
                MinimapWidget::new(editor, view)
                    .with_cache(mm_cache)
                    .with_options(MinimapOptions::default())
                    .show(&mut mm_ui);
            }
        });
    });
}

fn main() -> Result<()> {
    tracing_subscriber::fmt().init();
    let args = parse_args()?;
    if args.heap {
        let _profiler = dhat::Profiler::new_heap();
        run(&args);
        // _profiler drops here → writes dhat-heap.json.
    } else {
        run(&args);
    }
    Ok(())
}
