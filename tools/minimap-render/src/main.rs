//! Headless minimap PNG renderer. Loads a markdown/text file, builds the
//! same `EditorState` + `ViewState` + decoration layers the app would, and
//! calls the production `editor_egui::minimap::render_to_image` — so the PNG
//! is exactly what the live strip rasterizes, lettable us iterate on glyph /
//! bar visuals without firing up the GUI.
//!
//! Usage:
//!   minimap-render <input.md> [--out PATH] [--width N] [--height N]
//!                  [--style glyphs|bars] [--font-size F]
//!
//! Defaults: --out /tmp/minimap.png, --width 120, --height 1400,
//!           --style glyphs, --font-size 14.

use std::fs;

use editor_core::decoration::{Decoration, LineStyle};
use editor_core::state::Editor as EditorState;
use editor_core::theme::light_default;
use editor_egui::minimap::{render_to_image, Options, Style};
use editor_view::viewport::ViewState;
use image::{Rgba, RgbaImage};

struct Args {
    input: String,
    out: String,
    width: usize,
    height: usize,
    style: Style,
    font_size: f32,
    /// Simulated editor content width (points). Drives soft-wrap so the
    /// minimap shows wrapped rows. 0 disables wrap.
    editor_width: f32,
    /// Integer nearest-neighbor upscale of the output PNG, for inspecting
    /// glyph detail. 1 = native.
    zoom: u32,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            input: String::new(),
            out: "/tmp/minimap.png".to_owned(),
            width: 120,
            height: 1400,
            style: Style::Glyphs,
            font_size: 14.0,
            editor_width: 700.0,
            zoom: 1,
        }
    }
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        let mut next = || it.next().ok_or_else(|| format!("missing value after {a}"));
        match a.as_str() {
            "--out" => args.out = next()?,
            "--width" => args.width = next()?.parse().map_err(|e| format!("--width: {e}"))?,
            "--height" => args.height = next()?.parse().map_err(|e| format!("--height: {e}"))?,
            "--font-size" => {
                args.font_size = next()?.parse().map_err(|e| format!("--font-size: {e}"))?;
            }
            "--editor-width" => {
                args.editor_width = next()?.parse().map_err(|e| format!("--editor-width: {e}"))?;
            }
            "--zoom" => args.zoom = next()?.parse().map_err(|e| format!("--zoom: {e}"))?,
            "--style" => {
                args.style = match next()?.as_str() {
                    "glyphs" => Style::Glyphs,
                    "bars" => Style::Bars,
                    other => return Err(format!("--style must be glyphs|bars, got {other}")),
                };
            }
            other if !other.starts_with("--") => args.input = other.to_owned(),
            other => return Err(format!("unknown flag {other}")),
        }
    }
    if args.input.is_empty() {
        return Err("need an input file path".to_owned());
    }
    Ok(args)
}

/// Build a `ViewState` that mirrors what the editor would have after layout:
/// markdown decorations pushed, uniform line height, then heading scale /
/// hidden lines applied to the height map (the part the minimap projects on).
fn build_view(ctx: &egui::Context, editor: &EditorState, font_size: f32, editor_width: f32) -> ViewState {
    let font_id = egui::FontId::new(font_size, egui::FontFamily::Monospace);
    let line_h = ctx.fonts(|f| f.row_height(&font_id)).max(1.0);
    let advance = ctx
        .fonts(|f| f.layout_no_wrap("M".to_owned(), font_id, egui::Color32::WHITE).size().x)
        .max(1.0);
    let mut view = ViewState { font_size, line_height: line_h, ..ViewState::default() };

    let theme = light_default();
    view.decorations.push_with_heights(editor_md::styling::markdown_decorations(editor, Some(&theme)));

    let lines = editor.doc.len_lines();
    view.height_map.sync_to_lines(lines, line_h);

    // Soft-wrap, mirroring the editor: set the wrap geometry, then wrap each
    // line so the minimap can render wrapped visual rows.
    if editor_width > 0.0 {
        view.wrap_map.set_enabled(true);
        view.wrap_map.set_char_width(advance);
        view.wrap_map.set_width(editor_width);
        for line in 0..lines {
            let text = editor.doc.line_str(line);
            let scale = line_font_scale(&view, editor, line);
            // No live-preview spans in the offline minimap render — markers
            // render as raw text, so wrapping sees the full bytes.
            view.wrap_map.get_or_compute(line, &text, scale, &[]);
        }
    }

    apply_heights(&mut view, editor, line_h);
    view
}

/// Max `Mark.font_scale` covering a line (heading promotion), so wrap uses the
/// same effective char width the editor does — mirrors `prewrap_visible`.
fn line_font_scale(view: &ViewState, editor: &EditorState, line: usize) -> f32 {
    let lines = editor.doc.len_lines();
    let start = editor.doc.line_to_byte(line);
    let end = if line + 1 < lines { editor.doc.line_to_byte(line + 1) } else { editor.doc.len_bytes() };
    let probe_end = end.max(start + 1);
    let mut max_scale = 1.0f32;
    for layer in &view.decorations.layers {
        for (_r, deco) in layer.iter_overlapping(start..probe_end) {
            if let Decoration::Mark(ms) = deco
                && let Some(s) = ms.font_scale
                && s > max_scale
            {
                max_scale = s;
            }
        }
    }
    max_scale
}

/// Mirror the editor's height-map driver: heading scale / hidden lines from
/// `Line` decorations, then multiply wrapped lines by their visual-row count.
fn apply_heights(view: &mut ViewState, editor: &EditorState, line_h: f32) {
    let doc_len = editor.doc.len_bytes();
    let mut overrides: Vec<(usize, f32)> = Vec::new();
    for layer in view.decorations.height_layers() {
        for (range, deco) in layer.iter_overlapping(0..doc_len + 1) {
            match deco {
                Decoration::Line(LineStyle { hide: true, .. }) => {
                    overrides.push((editor.doc.byte_to_line(range.start.min(doc_len)), 0.0));
                }
                Decoration::Line(LineStyle { height_scale: Some(s), .. }) => {
                    overrides.push((editor.doc.byte_to_line(range.start), line_h * s));
                }
                _ => {}
            }
        }
    }
    for (line, h) in overrides {
        view.height_map.set_line_height(line, h);
    }
    if view.wrap_map.enabled() {
        for line in 0..editor.doc.len_lines() {
            let vc = view.wrap_map.peek(line).map_or(1, editor_view::wrapping::WrappedLine::visual_count);
            if vc > 1 {
                let h = view.height_map.text_height(line);
                if h > 0.0 {
                    view.height_map.set_line_height(line, h * vc as f32);
                }
            }
        }
    }
    view.height_map.recompute();
}

fn save_png(img: &egui::ColorImage, out: &str, zoom: u32) -> Result<(), String> {
    let w = img.size[0] as u32;
    let h = img.size[1] as u32;
    let mut rgba = RgbaImage::new(w, h);
    for (i, px) in img.pixels.iter().enumerate() {
        let [r, g, b, a] = px.to_srgba_unmultiplied();
        rgba.put_pixel((i as u32) % w, (i as u32) / w, Rgba([r, g, b, a]));
    }
    if zoom > 1 {
        rgba = image::imageops::resize(&rgba, w * zoom, h * zoom, image::imageops::FilterType::Nearest);
    }
    rgba.save(out).map_err(|e| format!("save {out}: {e}"))
}

fn run() -> Result<(), String> {
    let args = parse_args()?;
    let text = fs::read_to_string(&args.input).map_err(|e| format!("read {}: {e}", args.input))?;
    let editor = EditorState::new(&text);

    // Headless egui context: one pass primes the font atlas so `ctx.fonts`
    // (and the glyph-atlas readback inside `render_to_image`) work.
    let ctx = egui::Context::default();
    let _ = ctx.run(egui::RawInput::default(), |_| {});

    let view = build_view(&ctx, &editor, args.font_size, args.editor_width);
    let opts = Options { style: args.style, ..Default::default() };
    let img = ctx.fonts(|f| {
        render_to_image(f, &editor, &view, &opts, [args.width, args.height], 1.0)
    });
    save_png(&img, &args.out, args.zoom)?;
    println!(
        "wrote {} ({}x{}, {} lines, style={:?})",
        args.out,
        args.width,
        args.height,
        editor.doc.len_lines(),
        args.style
    );
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("minimap-render: {e}");
        std::process::exit(1);
    }
}
