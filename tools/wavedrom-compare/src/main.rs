//! `wavedrom-compare` — visual-parity harness for the pure-Rust WaveDrom
//! renderer (`hiker_wavedrom::render`) against real wavedrom.js (wavedrom-cli,
//! run in the oracle container).
//!
//! Unlike `dagre-compare` (which diffs numeric layout coordinates), WaveDrom
//! emits **SVG**, so parity is judged VISUALLY + by PALETTE:
//!
//!   1. render OURS    — `hiker_wavedrom::render` → SVG.
//!   2. render REF     — wavedrom-cli in Docker   → SVG  (see oracle/).
//!   3. rasterize BOTH — resvg 0.47 with the SAME font setup the crate's own
//!      example uses (bundled Liberation Sans + system fonts), so neither side
//!      gets a font-substitution advantage.
//!   4. compose        — side-by-side PNG (ours | reference) with a divider and
//!      "OURS" / "wavedrom.js" labels, written to `out/<name>.png`.
//!   5. measure        — a coarse RGB color histogram per side (quantized to
//!      ~32-step bins over non-background pixels), printing the top colors per
//!      side + a histogram-intersection score (0..1). This directly quantifies
//!      palette parity — the main reason this tool exists.
//!
//! Two subcommands, glued together by `run.sh`:
//!   * `emit-svg <fixture.json>`
//!         render ours, print the SVG on stdout.
//!   * `compose --ours <ours.svg> --ref <ref.svg> --out <png> [--name NAME]`
//!         rasterize both, write the side-by-side composite, print the
//!         color-histogram report + each side's pixel dimensions.
//!
//! Fixtures are RAW WaveJSON files, passed verbatim to both sides — no schema.

use std::process::ExitCode;

use hiker_wavedrom::{WaveDromOptions, render};
use resvg::tiny_skia::{Pixmap, Transform};
use resvg::usvg;

// ---------------------------------------------------------------------------
// Rasterization — identical setup for ours and the reference.
// ---------------------------------------------------------------------------

/// A rasterized SVG: RGBA8 pixels (premultiplied-undone by tiny-skia's
/// `data()` which is straight RGBA) plus dimensions.
struct Raster {
    w: u32,
    h: u32,
    /// Row-major RGBA8.
    rgba: Vec<u8>,
}

/// Rasterize an SVG string via resvg, mirroring the font setup in
/// `hiker-render/wavedrom/examples/render_wavedrom.rs` so OURS and the
/// reference rasterize under identical font resolution. We additionally load
/// the system fonts so wavedrom.js's font references resolve, then pin the
/// sans-serif family to Liberation Sans (the bundled font) so both sides
/// measure/draw text with the same metrics.
fn rasterize(svg: &str, label: &str) -> Result<Raster, String> {
    let mut opt = usvg::Options::default();
    {
        let db = opt.fontdb_mut();
        db.load_system_fonts();
        db.load_font_data(hiker_wavedrom::font::FONT_BYTES.to_vec());
        db.set_sans_serif_family("Liberation Sans");
    }
    let tree = usvg::Tree::from_str(svg, &opt)
        .map_err(|e| format!("{label}: svg parse failed: {e}"))?;

    let size = tree.size();
    let w = (size.width().ceil() as u32).max(1);
    let h = (size.height().ceil() as u32).max(1);
    let mut pixmap = Pixmap::new(w, h).ok_or_else(|| format!("{label}: pixmap alloc failed"))?;
    resvg::render(&tree, Transform::identity(), &mut pixmap.as_mut());

    Ok(Raster {
        w,
        h,
        rgba: pixmap.data().to_vec(),
    })
}

// ---------------------------------------------------------------------------
// Color histogram — coarse RGB bins over non-background pixels.
// ---------------------------------------------------------------------------

/// Quantization step: each channel is bucketed into bins of this width, so the
/// histogram is robust to ~1-level rasterization noise and antialiasing
/// fringes while still separating WaveDrom's distinct bus/`type` fills.
const QUANT: u32 = 32;

/// A quantized color bucket key: the bin index per channel packed into one u32.
type Bucket = u32;

const fn quantize(r: u8, g: u8, b: u8) -> Bucket {
    let rb = (r as u32) / QUANT;
    let gb = (g as u32) / QUANT;
    let bb = (b as u32) / QUANT;
    (rb << 16) | (gb << 8) | bb
}

/// Representative (bin-center) RGB for a bucket, for human-readable hex output.
fn bucket_rgb(b: Bucket) -> (u8, u8, u8) {
    let rb = (b >> 16) & 0xff;
    let gb = (b >> 8) & 0xff;
    let bb = b & 0xff;
    let center = |bin: u32| ((bin * QUANT) + QUANT / 2).min(255) as u8;
    (center(rb), center(gb), center(bb))
}

/// A color histogram over the "ink" of an image: non-background, sufficiently
/// opaque pixels, keyed by quantized RGB bucket → pixel count.
struct Histogram {
    counts: std::collections::HashMap<Bucket, u64>,
    total: u64,
}

/// Treat near-white and near-black as "background/foreground structure" that
/// dominates every diagram and drowns out the palette signal we care about.
/// We keep them OUT of the palette histogram so the score reflects fills.
const fn is_structural(r: u8, g: u8, b: u8) -> bool {
    let near_white = r > 244 && g > 244 && b > 244;
    let near_black = r < 24 && g < 24 && b < 24;
    near_white || near_black
}

impl Histogram {
    fn of(raster: &Raster) -> Histogram {
        let mut counts: std::collections::HashMap<Bucket, u64> = std::collections::HashMap::new();
        let mut total = 0u64;
        for px in raster.rgba.chunks_exact(4) {
            let (r, g, b, a) = (px[0], px[1], px[2], px[3]);
            if a < 128 {
                continue; // transparent → background
            }
            if is_structural(r, g, b) {
                continue; // white canvas / black lines+text
            }
            *counts.entry(quantize(r, g, b)).or_insert(0) += 1;
            total += 1;
        }
        Histogram { counts, total }
    }

    /// Top-N buckets by pixel count, as (hex, share-of-ink) descending.
    fn top(&self, n: usize) -> Vec<(String, f64)> {
        let mut v: Vec<(Bucket, u64)> = self.counts.iter().map(|(&k, &c)| (k, c)).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        v.truncate(n);
        v.into_iter()
            .map(|(bucket, c)| {
                let (r, g, b) = bucket_rgb(bucket);
                let share = if self.total == 0 {
                    0.0
                } else {
                    c as f64 / self.total as f64
                };
                (format!("#{r:02x}{g:02x}{b:02x}"), share)
            })
            .collect()
    }
}

/// Histogram-intersection score in 0..1: sum of per-bucket min(shareA, shareB)
/// over the union of buckets. 1.0 = identical palette distribution, 0.0 = no
/// shared colors. Normalized by share (not raw count) so the two sides'
/// differing ink areas don't bias it.
fn intersection(a: &Histogram, b: &Histogram) -> f64 {
    if a.total == 0 || b.total == 0 {
        return 0.0;
    }
    let mut score = 0.0;
    for (&bucket, &ca) in &a.counts {
        if let Some(&cb) = b.counts.get(&bucket) {
            let sa = ca as f64 / a.total as f64;
            let sb = cb as f64 / b.total as f64;
            score += sa.min(sb);
        }
    }
    score
}

// ---------------------------------------------------------------------------
// Composite — side-by-side PNG with divider + labels.
// ---------------------------------------------------------------------------

const PAD: u32 = 12;
const DIVIDER: u32 = 4;
const LABEL_H: u32 = 18;
const BG: [u8; 4] = [250, 250, 250, 255];
const DIVIDER_COLOR: [u8; 4] = [120, 120, 120, 255];
const LABEL_COLOR: [u8; 4] = [40, 40, 40, 255];

/// Blit a raster onto the canvas at (ox, oy), compositing over the canvas bg.
fn blit(canvas: &mut [u8], cw: u32, src: &Raster, ox: u32, oy: u32) {
    for y in 0..src.h {
        for x in 0..src.w {
            let si = ((y * src.w + x) * 4) as usize;
            let (r, g, b, a) = (
                src.rgba[si],
                src.rgba[si + 1],
                src.rgba[si + 2],
                src.rgba[si + 3] as u32,
            );
            let di = (((oy + y) * cw + (ox + x)) * 4) as usize;
            // Alpha-over the existing canvas pixel.
            let inv = 255 - a;
            canvas[di] = ((r as u32 * a + canvas[di] as u32 * inv) / 255) as u8;
            canvas[di + 1] = ((g as u32 * a + canvas[di + 1] as u32 * inv) / 255) as u8;
            canvas[di + 2] = ((b as u32 * a + canvas[di + 2] as u32 * inv) / 255) as u8;
            canvas[di + 3] = 255;
        }
    }
}

fn fill_rect(canvas: &mut [u8], cw: u32, x: u32, y: u32, w: u32, h: u32, color: [u8; 4]) {
    for yy in y..y + h {
        for xx in x..x + w {
            let di = ((yy * cw + xx) * 4) as usize;
            canvas[di..di + 4].copy_from_slice(&color);
        }
    }
}

/// Render a short ASCII label into the canvas via the crate's bundled font is
/// overkill; instead draw labels as a tiny 5x7 bitmap font so the composite is
/// self-describing without pulling text-shaping into the harness.
fn draw_label(canvas: &mut [u8], cw: u32, text: &str, x: u32, y: u32, color: [u8; 4]) {
    let mut cx = x;
    for ch in text.chars() {
        let glyph = glyph5x7(ch);
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..5u32 {
                if bits & (1 << (4 - col)) != 0 {
                    let px = cx + col;
                    let py = y + row as u32;
                    if px < cw {
                        let di = ((py * cw + px) * 4) as usize;
                        if di + 4 <= canvas.len() {
                            canvas[di..di + 4].copy_from_slice(&color);
                        }
                    }
                }
            }
        }
        cx += 6;
    }
}

/// Minimal 5x7 bitmap font covering just the glyphs used by our labels
/// (uppercase letters, lowercase letters, digits, and a few symbols). Each
/// glyph is 7 rows of a 5-bit mask. Unknown chars render as a blank box.
const fn glyph5x7(ch: char) -> [u8; 7] {
    match ch.to_ascii_uppercase() {
        'O' => [0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'U' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'R' => [0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11],
        'S' => [0x0F, 0x10, 0x10, 0x0E, 0x01, 0x01, 0x1E],
        'W' => [0x11, 0x11, 0x11, 0x15, 0x15, 0x1B, 0x11],
        'A' => [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'V' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x0A, 0x04],
        'E' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F],
        'D' => [0x1E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1E],
        'M' => [0x11, 0x1B, 0x15, 0x15, 0x11, 0x11, 0x11],
        'J' => [0x01, 0x01, 0x01, 0x01, 0x11, 0x11, 0x0E],
        '.' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04],
        ' ' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
        _ => [0x1F, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1F],
    }
}

fn compose(
    ours: &Raster,
    reference: &Raster,
    out_path: &str,
) -> Result<(u32, u32), String> {
    let panel_h = ours.h.max(reference.h);
    let cw = PAD + ours.w + PAD + DIVIDER + PAD + reference.w + PAD;
    let ch = LABEL_H + PAD + panel_h + PAD;

    let mut canvas = vec![0u8; (cw * ch * 4) as usize];
    // Fill background.
    for px in canvas.chunks_exact_mut(4) {
        px.copy_from_slice(&BG);
    }

    let left_x = PAD;
    let right_x = PAD + ours.w + PAD + DIVIDER + PAD;
    let panel_y = LABEL_H + PAD;

    // Labels.
    draw_label(&mut canvas, cw, "OURS", left_x, PAD / 2, LABEL_COLOR);
    draw_label(&mut canvas, cw, "WAVEDROM.JS", right_x, PAD / 2, LABEL_COLOR);

    // Diagrams.
    blit(&mut canvas, cw, ours, left_x, panel_y);
    blit(&mut canvas, cw, reference, right_x, panel_y);

    // Divider.
    let div_x = PAD + ours.w + PAD;
    fill_rect(&mut canvas, cw, div_x, 0, DIVIDER, ch, DIVIDER_COLOR);

    image::save_buffer(
        out_path,
        &canvas,
        cw,
        ch,
        image::ExtendedColorType::Rgba8,
    )
    .map_err(|e| format!("write {out_path}: {e}"))?;

    Ok((cw, ch))
}

// ---------------------------------------------------------------------------
// Subcommands.
// ---------------------------------------------------------------------------

fn emit_svg(path: &str) -> Result<(), String> {
    let src = std::fs::read_to_string(path).map_err(|e| format!("read {path}: {e}"))?;
    let r = render(&src, &WaveDromOptions::default())
        .map_err(|e| format!("render {path}: {e:?}"))?;
    print!("{}", r.svg);
    Ok(())
}

fn cmd_compose(args: &[String]) -> Result<(), String> {
    let get = |flag: &str| -> Option<String> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let ours_path = get("--ours").ok_or("compose: missing --ours")?;
    let ref_path = get("--ref").ok_or("compose: missing --ref")?;
    let out_path = get("--out").ok_or("compose: missing --out")?;
    let name = get("--name").unwrap_or_else(|| "diagram".to_string());

    let ours_svg = std::fs::read_to_string(&ours_path).map_err(|e| format!("read {ours_path}: {e}"))?;
    let ref_svg = std::fs::read_to_string(&ref_path).map_err(|e| format!("read {ref_path}: {e}"))?;

    let ours = rasterize(&ours_svg, "ours")?;
    let reference = rasterize(&ref_svg, "ref")?;

    let (cw, ch) = compose(&ours, &reference, &out_path)?;

    let h_ours = Histogram::of(&ours);
    let h_ref = Histogram::of(&reference);
    let score = intersection(&h_ours, &h_ref);

    println!("### {name}");
    println!(
        "  ours:      {}x{} px   ({} ink px)",
        ours.w, ours.h, h_ours.total
    );
    println!(
        "  wavedrom:  {}x{} px   ({} ink px)",
        reference.w, reference.h, h_ref.total
    );
    println!("  composite: {out_path}  ({cw}x{ch} px)");
    println!("  palette histogram-intersection: {score:.3}  (1.0 = identical palette)");

    println!("  top colors OURS:");
    for (hex, share) in h_ours.top(6) {
        println!("    {hex}  {:5.1}%", share * 100.0);
    }
    println!("  top colors WAVEDROM.JS:");
    for (hex, share) in h_ref.top(6) {
        println!("    {hex}  {:5.1}%", share * 100.0);
    }

    Ok(())
}

fn usage() -> ExitCode {
    eprintln!(
        "usage:\n  \
         wavedrom-compare emit-svg <fixture.json>\n  \
         wavedrom-compare compose --ours <ours.svg> --ref <ref.svg> --out <png> [--name NAME]"
    );
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("emit-svg") => {
            let Some(path) = args.get(1) else {
                return usage();
            };
            emit_svg(path)
        }
        Some("compose") => cmd_compose(&args[1..]),
        _ => return usage(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
