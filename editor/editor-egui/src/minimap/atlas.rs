//! Glyph atlas for the minimap's `Style::Glyphs` renderer.
//!
//! Builds a tiny sprite atlas of printable-ASCII glyph coverage by reading
//! back egui's own font rasterization (`Fonts::image()` + per-glyph
//! metrics) and rasterizing each glyph into a shared `cw × ch` line-box
//! cell at its true baseline. Rebuilt only when the editor font size
//! changes — the strip renderer blits these cells, never re-shapes text.

use egui::Color32;

pub(super) const GLYPH_LO: u32 = 0x20;
pub(super) const GLYPH_HI: u32 = 0x7e;
pub(super) const GLYPH_COUNT: usize = (GLYPH_HI - GLYPH_LO + 1) as usize;

/// A tiny sprite atlas of printable-ASCII glyph coverage, built once by
/// reading back egui's font atlas (`Fonts::image()` + per-glyph metrics) and
/// rasterizing each glyph into a shared `cw × ch` line-box cell at its true
/// baseline. Rebuilt only when the editor font size changes.
pub(super) struct GlyphAtlas {
    pub font_size: f32,
    /// `GLYPH_COUNT` cells of `cw * ch` coverage values (0.0..=1.0).
    pub cov: Vec<f32>,
    /// Monospace advance width (points) at `font_size` — the per-column
    /// step used to lay glyphs out in the strip.
    pub advance: f32,
    /// Cell resolution. Sized to roughly the glyph's native pixel box so the
    /// blit is ~1:1 when the doc fits the strip (no upscale mush) and a clean
    /// downscale when the doc is taller than the strip.
    pub cw: usize,
    pub ch: usize,
}

impl GlyphAtlas {
    const fn cell(&self) -> usize {
        self.cw * self.ch
    }

    pub fn coverage(&self, ch: char) -> Option<&[f32]> {
        let c = ch as u32;
        if !(GLYPH_LO..=GLYPH_HI).contains(&c) {
            return None;
        }
        let cell = self.cell();
        let base = (c - GLYPH_LO) as usize * cell;
        Some(&self.cov[base..base + cell])
    }

    /// Build the atlas from egui's font rasterization. Takes a bare `Fonts`
    /// (rather than a `Ui`) so it works headlessly — e.g. the `minimap-render`
    /// PNG tool builds one from a `Context`.
    ///
    /// Each glyph is rasterized into a shared **line-box cell** (advance wide ×
    /// font line-height tall) at its true baseline and size — so `x`/`H`/`g`
    /// keep their relative heights and all glyphs share a baseline. (Stretching
    /// each glyph's tight bbox to fill the cell instead — the obvious shortcut
    /// — makes every letter the same height and the text reads as mush.)
    pub fn build(fonts: &egui::epaint::text::Fonts, font_size: f32) -> Self {
        let font_id = egui::FontId::new(font_size, egui::FontFamily::Monospace);
        let (advance, font_h) = fonts
            .layout_no_wrap("M".to_owned(), font_id.clone(), Color32::WHITE)
            .rows
            .first()
            .and_then(|r| r.row.glyphs.first())
            .map_or((font_size * 0.6, font_size * 1.3), |g| {
                (g.advance_width.max(1.0), g.font_height.max(1.0))
            });
        // 2× supersample the point-sized line box for a crisper downscale.
        let cw = ((advance.round() as usize).max(2)) * 2;
        let ch = ((font_h.round() as usize).max(2)) * 2;
        let cell = cw * ch;
        let mut cov = vec![0.0f32; GLYPH_COUNT * cell];
        // Capture each glyph's placement metrics, then snapshot the atlas once.
        let mut metrics: Vec<Option<GlyphMetric>> = Vec::with_capacity(GLYPH_COUNT);
        for c in GLYPH_LO..=GLYPH_HI {
            let ch = char::from_u32(c).unwrap_or(' ');
            let galley = fonts.layout_no_wrap(ch.to_string(), font_id.clone(), Color32::WHITE);
            metrics.push(galley.rows.first().and_then(|r| r.row.glyphs.first()).map(|g| {
                GlyphMetric {
                    pos: g.pos,
                    offset: g.uv_rect.offset,
                    size: g.uv_rect.size,
                    min: g.uv_rect.min,
                    max: g.uv_rect.max,
                }
            }));
        }
        let img = fonts.image();
        let geom = CellGeom { advance, font_h, cw, ch };
        for (i, m) in metrics.into_iter().enumerate() {
            let Some(m) = m else { continue };
            if m.max[0] <= m.min[0] || m.max[1] <= m.min[1] {
                continue; // empty glyph (e.g. space)
            }
            rasterize_glyph_cell(&img, &m, &geom, &mut cov[i * cell..(i + 1) * cell]);
        }
        Self { font_size, cov, advance, cw, ch }
    }
}

/// A glyph's placement in a single-char galley + its bitmap rect in the font
/// atlas. The bitmap is drawn at `pos + offset`, size `size` (epaint's own
/// formula); `pos.y` is the baseline.
pub(super) struct GlyphMetric {
    pub pos: egui::Pos2,
    pub offset: egui::Vec2,
    pub size: egui::Vec2,
    pub min: [u16; 2],
    pub max: [u16; 2],
}

/// The shared line-box cell the atlas rasterizes each glyph into.
pub(super) struct CellGeom {
    pub advance: f32,
    pub font_h: f32,
    pub cw: usize,
    pub ch: usize,
}

/// Rasterize one glyph into its line-box cell, preserving baseline and natural
/// size. For each cell pixel we map to a point in the cell's `[0,advance] ×
/// [0,font_h]` box, test whether it falls in the glyph's bitmap rect (`pos +
/// offset`, `size`), and if so sample the font atlas alpha there. The font
/// image holds coverage in the alpha channel (`from_white_alpha`).
pub(super) fn rasterize_glyph_cell(
    img: &egui::ColorImage,
    m: &GlyphMetric,
    g: &CellGeom,
    out: &mut [f32],
) {
    let (iw, ih) = (img.size[0], img.size[1]);
    let bx0 = m.pos.x + m.offset.x;
    let by0 = m.pos.y + m.offset.y;
    let bw = m.size.x.max(0.001);
    let bh = m.size.y.max(0.001);
    let (tx0, ty0) = (f32::from(m.min[0]), f32::from(m.min[1]));
    let (tw, th) = (f32::from(m.max[0]) - tx0, f32::from(m.max[1]) - ty0);
    for cy in 0..g.ch {
        let py = (cy as f32 + 0.5) / g.ch as f32 * g.font_h;
        let fy = (py - by0) / bh;
        if !(0.0..1.0).contains(&fy) {
            continue;
        }
        let ty = (ty0 + fy * th) as usize;
        for cx in 0..g.cw {
            let px = (cx as f32 + 0.5) / g.cw as f32 * g.advance;
            let fx = (px - bx0) / bw;
            if !(0.0..1.0).contains(&fx) {
                continue;
            }
            let tx = (tx0 + fx * tw) as usize;
            if tx < iw && ty < ih {
                out[cy * g.cw + cx] = f32::from(img.pixels[ty * iw + tx].a()) / 255.0;
            }
        }
    }
}
