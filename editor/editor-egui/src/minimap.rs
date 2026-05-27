//! Minimap widget: a narrow strip mirroring the whole document.
//!
//! The strip is rasterized once into an offscreen [`egui::ColorImage`],
//! uploaded as a single texture, and painted as one quad. The texture is
//! rebuilt only when the document, decoration layers, theme, strip size, or
//! style change — never on scroll. That keeps per-frame cost O(1) regardless
//! of document length (the previous renderer issued one shape per visible
//! line every frame, which stuttered even on small files). See
//! `editor/SPEC.md` §9.23 and `editor/IMPLEMENTATION.md` §16.6.18.
//!
//! Two render styles:
//! - [`Style::Glyphs`] (default): a literal scaled-down view — one cell per
//!   character, indentation and density preserved, each cell tinted by the
//!   same decoration/syntax color the editor paints that span with. The tiny
//!   glyph cells come from a sprite atlas built by reading back egui's own
//!   font rasterization ([`GlyphAtlas`]), so no extra dependency is pulled in.
//! - [`Style::Bars`]: the structural abstraction — one bar per line, width by
//!   visible length, color by structural role.
//!
//! Both share the editor's `height_map` projection (soft wrap, heading scale,
//! hidden lines) so the strip and the viewport thumb stay in lockstep with
//! what's on screen. The thumb, selection/search marks, and click/drag/wheel
//! interaction stay live (off-texture) because they track scroll position.

use egui::{
    Color32, CornerRadius, Pos2, Rect, Sense, Stroke, TextureHandle, TextureOptions, Vec2,
};

use editor_core::decoration::Decoration;
use editor_core::state::Editor as EditorState;
use editor_view::command;
use editor_view::events::InputEvent;
use editor_view::viewport::ViewState;

use crate::widget::layout::{display_rows, DisplayRow};

/// Strip-points per content-point. Bars fill the strip height (the overview
/// look the user expects); glyphs use a **uniform** scale — the smaller of the
/// vertical fit and the width fit — so soft-wrapped rows fit the strip width
/// without vertically stretching the glyphs. For a short doc that means the
/// glyph minimap occupies the top of the strip at true aspect rather than
/// magnifying to fill it.
fn content_scale(
    view: &ViewState,
    opts: &Options,
    strip_w_pt: f32,
    strip_h_pt: f32,
    total_content: f32,
) -> f32 {
    let fit = strip_h_pt / total_content.max(1.0);
    let wrap_w = view.wrap_map.width();
    if opts.style == Style::Glyphs && view.wrap_map.enabled() && wrap_w > 0.0 {
        let usable = (strip_w_pt - opts.bar_padding_left - opts.bar_padding_right).max(1.0);
        fit.min(usable / wrap_w)
    } else {
        fit
    }
}

/// Printable ASCII range the atlas covers (space..=tilde).
const GLYPH_LO: u32 = 0x20;
const GLYPH_HI: u32 = 0x7e;
const GLYPH_COUNT: usize = (GLYPH_HI - GLYPH_LO + 1) as usize;

/// Which renderer the strip uses. Selectable by the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum Style {
    /// Structural bars (one per line).
    Bars,
    /// Literal scaled-down glyph render (default).
    #[default]
    Glyphs,
}

/// What a given doc line looks like structurally. Higher variants beat
/// lower ones when multiple decorations overlap the same line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum LineKind {
    Hidden,
    Plain,
    Quote,
    Code,
    Emphasis,
    Heading,
}

/// Visual + behavior knobs. All sizes are pixels.
#[derive(Clone, Debug)]
pub struct Options {
    pub style: Style,
    pub width: f32,
    pub bar_padding_left: f32,
    pub bar_padding_right: f32,
    pub bar_corner_radius: f32,
    pub min_bar_width: f32,
    /// Vertical gap between consecutive bars, in pixels (fractional).
    pub bar_gap: f32,
    pub colored: bool,
    pub show_section_rules: bool,
    pub show_viewport: bool,
    pub show_left_edge: bool,
    pub color_heading: Color32,
    pub color_code: Color32,
    pub color_emphasis: Color32,
    pub color_quote: Color32,
    pub color_plain: Color32,
    pub color_background: Color32,
    pub color_section_rule: Color32,
    pub color_viewport: Color32,
    pub color_viewport_hover: Color32,
    /// Mark drawn over lines touched by a non-empty selection range.
    pub color_selection: Color32,
    /// Mark drawn over lines touched by a search match.
    pub color_search: Color32,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            style: Style::Glyphs,
            width: 72.0,
            bar_padding_left: 5.0,
            bar_padding_right: 5.0,
            bar_corner_radius: 1.0,
            min_bar_width: 2.0,
            bar_gap: 0.5,
            colored: true,
            show_section_rules: true,
            show_viewport: true,
            show_left_edge: true,
            color_heading: Color32::from_rgba_premultiplied(60, 122, 220, 240),
            color_code: Color32::from_rgba_premultiplied(60, 149, 197, 220),
            color_emphasis: Color32::from_rgba_premultiplied(201, 138, 60, 220),
            color_quote: Color32::from_rgba_premultiplied(122, 133, 165, 160),
            color_plain: Color32::from_rgba_premultiplied(106, 111, 128, 180),
            color_background: Color32::from_rgba_premultiplied(0, 0, 0, 20),
            color_section_rule: Color32::from_rgba_premultiplied(0, 0, 0, 28),
            color_viewport: Color32::from_rgba_premultiplied(60, 100, 180, 28),
            color_viewport_hover: Color32::from_rgba_premultiplied(60, 100, 180, 50),
            color_selection: Color32::from_rgba_premultiplied(110, 150, 220, 150),
            color_search: Color32::from_rgba_premultiplied(220, 190, 70, 170),
        }
    }
}

impl LineKind {
    const fn color(self, opts: &Options) -> Color32 {
        if !opts.colored {
            return match self {
                LineKind::Hidden => Color32::TRANSPARENT,
                _ => opts.color_plain,
            };
        }
        match self {
            LineKind::Hidden => Color32::TRANSPARENT,
            LineKind::Plain => opts.color_plain,
            LineKind::Quote => opts.color_quote,
            LineKind::Code => opts.color_code,
            LineKind::Emphasis => opts.color_emphasis,
            LineKind::Heading => opts.color_heading,
        }
    }
}

/// Per-line "how much of the line is visible content vs. leading
/// whitespace", in bytes.
#[derive(Clone, Copy, Default)]
struct LineMetrics {
    indent: u32,
    visible: u32,
}

/// A tiny sprite atlas of printable-ASCII glyph coverage, built once by
/// reading back egui's font atlas (`Fonts::image()` + per-glyph metrics) and
/// rasterizing each glyph into a shared `cw × ch` line-box cell at its true
/// baseline. Rebuilt only when the editor font size changes.
struct GlyphAtlas {
    font_size: f32,
    /// `GLYPH_COUNT` cells of `cw * ch` coverage values (0.0..=1.0).
    cov: Vec<f32>,
    /// Monospace advance width (points) at `font_size` — the per-column
    /// step used to lay glyphs out in the strip.
    advance: f32,
    /// Cell resolution. Sized to roughly the glyph's native pixel box so the
    /// blit is ~1:1 when the doc fits the strip (no upscale mush) and a clean
    /// downscale when the doc is taller than the strip.
    cw: usize,
    ch: usize,
}

impl GlyphAtlas {
    const fn cell(&self) -> usize {
        self.cw * self.ch
    }

    fn coverage(&self, ch: char) -> Option<&[f32]> {
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
    fn build(fonts: &egui::epaint::text::Fonts, font_size: f32) -> Self {
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
struct GlyphMetric {
    pos: egui::Pos2,
    offset: egui::Vec2,
    size: egui::Vec2,
    min: [u16; 2],
    max: [u16; 2],
}

/// The shared line-box cell the atlas rasterizes each glyph into.
struct CellGeom {
    advance: f32,
    font_h: f32,
    cw: usize,
    ch: usize,
}

/// Rasterize one glyph into its line-box cell, preserving baseline and natural
/// size. For each cell pixel we map to a point in the cell's `[0,advance] ×
/// [0,font_h]` box, test whether it falls in the glyph's bitmap rect (`pos +
/// offset`, `size`), and if so sample the font atlas alpha there. The font
/// image holds coverage in the alpha channel (`from_white_alpha`).
fn rasterize_glyph_cell(img: &egui::ColorImage, m: &GlyphMetric, g: &CellGeom, out: &mut [f32]) {
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

/// Accumulation buffer for rasterizing the strip. Contributions are summed
/// in premultiplied-alpha space (egui `Color32` components are already
/// premultiplied), then resolved over the background in [`Self::resolve`].
/// `rgb` is in the 0..=255 scale; `a` in 0.0..=1.0.
struct Accum {
    w: usize,
    h: usize,
    rgb: Vec<[f32; 3]>,
    a: Vec<f32>,
}

impl Accum {
    fn new(w: usize, h: usize) -> Self {
        let n = w * h;
        Self { w, h, rgb: vec![[0.0; 3]; n], a: vec![0.0; n] }
    }

    /// Add `weight` of `color` (premultiplied) to one pixel.
    fn add(&mut self, x: usize, y: usize, color: Color32, weight: f32) {
        if weight <= 0.0 || x >= self.w || y >= self.h {
            return;
        }
        let i = y * self.w + x;
        self.rgb[i][0] += f32::from(color.r()) * weight;
        self.rgb[i][1] += f32::from(color.g()) * weight;
        self.rgb[i][2] += f32::from(color.b()) * weight;
        self.a[i] += (f32::from(color.a()) / 255.0) * weight;
    }

    /// Fill an axis-aligned (possibly sub-pixel / fractional) rect with
    /// `color` at `strength`, distributing coverage by per-pixel overlap area.
    fn fill(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, color: Color32, strength: f32) {
        if strength <= 0.0 || x1 <= x0 || y1 <= y0 {
            return;
        }
        let px0 = x0.floor().max(0.0) as usize;
        let py0 = y0.floor().max(0.0) as usize;
        let px1 = (x1.ceil() as usize).min(self.w);
        let py1 = (y1.ceil() as usize).min(self.h);
        for py in py0..py1 {
            let cov_y = (y1.min(py as f32 + 1.0) - y0.max(py as f32)).clamp(0.0, 1.0);
            if cov_y <= 0.0 {
                continue;
            }
            for px in px0..px1 {
                let cov_x = (x1.min(px as f32 + 1.0) - x0.max(px as f32)).clamp(0.0, 1.0);
                self.add(px, py, color, strength * cov_x * cov_y);
            }
        }
    }

    /// Blit a glyph coverage cell into `rect`, tinted by `color`. Each atlas
    /// texel is splatted across the destination pixels it overlaps, so the
    /// blit downscales (or upscales) to any cell size and sub-pixel cells
    /// blend naturally into density.
    fn blit_glyph(&mut self, sprite: &[f32], cells: [usize; 2], rect: Rect, color: Color32) {
        let [cells_w, cells_h] = cells;
        let dw = rect.width();
        let dh = rect.height();
        for ay in 0..cells_h {
            let dy0 = rect.top() + ay as f32 * dh / cells_h as f32;
            let dy1 = rect.top() + (ay + 1) as f32 * dh / cells_h as f32;
            for ax in 0..cells_w {
                let cov = sprite[ay * cells_w + ax];
                if cov <= 0.0 {
                    continue;
                }
                let dx0 = rect.left() + ax as f32 * dw / cells_w as f32;
                let dx1 = rect.left() + (ax + 1) as f32 * dw / cells_w as f32;
                // Contrast curve: lift mid-coverage so anti-aliased stems read
                // as solid ink at minimap scale instead of washing out to grey.
                self.fill(dx0, dy0, dx1, dy1, color, cov.powf(0.6));
            }
        }
    }

    /// Composite the accumulated content over `bg` and produce the texture
    /// image. Output alpha is left below opaque where `bg` is translucent so
    /// the editor background shows through, matching the old strip's look.
    fn resolve(&self, bg: Color32) -> egui::ColorImage {
        let n = self.w * self.h;
        let mut px = Vec::with_capacity(n);
        let (bgr, bgg, bgb) = (f32::from(bg.r()), f32::from(bg.g()), f32::from(bg.b()));
        let bga = f32::from(bg.a()) / 255.0;
        for i in 0..n {
            let a = self.a[i];
            let ca = a.min(1.0);
            let s = if a > 1.0 { 1.0 / a } else { 1.0 };
            let cr = (self.rgb[i][0] * s).min(255.0);
            let cg = (self.rgb[i][1] * s).min(255.0);
            let cb = (self.rgb[i][2] * s).min(255.0);
            let inv = 1.0 - ca;
            let or = (cr + bgr * inv).min(255.0) as u8;
            let og = (cg + bgg * inv).min(255.0) as u8;
            let ob = (cb + bgb * inv).min(255.0) as u8;
            let oa = ((ca + bga * inv).min(1.0) * 255.0) as u8;
            px.push(Color32::from_rgba_premultiplied(or, og, ob, oa));
        }
        egui::ColorImage::new([self.w, self.h], px)
    }
}

/// Host-owned, cross-frame cache: per-line metrics + classification (only
/// recomputed on edit / decoration-swap) plus the rasterized texture and its
/// rebuild key, and the glyph atlas. Lives on the host (e.g. a `Buffer`)
/// rather than inside `ViewState` so the `editor-view` crate stays free of
/// egui types — the same split `PaintCache` uses.
#[derive(Default)]
pub struct Cache {
    metrics: Vec<LineMetrics>,
    kinds: Vec<LineKind>,
    metrics_doc_id: usize,
    kinds_doc_id: usize,
    kinds_decos_sig: u64,
    primed: bool,
    tex: Option<TextureHandle>,
    tex_key: u64,
    atlas: Option<GlyphAtlas>,
}

impl Cache {
    /// Recompute metrics / classification only for the parts whose key
    /// changed. `metrics` depends solely on the document text; `kinds`
    /// additionally on the decoration layers.
    fn refresh(&mut self, state: &EditorState, view: &ViewState) {
        let doc_id = state.doc.content_id();
        let decos_sig = view.decorations.signature;
        if !self.primed || self.metrics_doc_id != doc_id {
            self.metrics = measure_lines(state);
            self.metrics_doc_id = doc_id;
        }
        if !self.primed || self.kinds_doc_id != doc_id || self.kinds_decos_sig != decos_sig {
            self.kinds = classify_lines(state, view);
            self.kinds_doc_id = doc_id;
            self.kinds_decos_sig = decos_sig;
        }
        self.primed = true;
    }

    /// Rebuild the strip texture iff its inputs changed. Pure scroll frames
    /// hit the early return and reuse the cached texture.
    fn ensure_texture(&mut self, ui: &mut egui::Ui, inp: &TexInputs<'_>) {
        let ppp = ui.ctx().pixels_per_point();
        let w = ((inp.rect.width() * ppp).round() as usize).max(1);
        let h = ((inp.rect.height() * ppp).round() as usize).max(1);
        let key = pixel_signature(inp, w, h);
        if self.tex.is_some() && self.tex_key == key {
            return;
        }
        if inp.opts.style == Style::Glyphs
            && self.atlas.as_ref().is_none_or(|a| a.font_size != inp.view.font_size)
        {
            let fs = inp.view.font_size;
            self.atlas = Some(ui.fonts(|f| GlyphAtlas::build(f, fs)));
        }
        let atlas = if inp.opts.style == Style::Glyphs { self.atlas.as_ref() } else { None };
        let img = Raster {
            state: inp.state,
            view: inp.view,
            opts: inp.opts,
            kinds: &self.kinds,
            metrics: &self.metrics,
            atlas,
            w,
            h,
            ppp,
            scale_px: inp.scale * ppp,
            line_h: inp.line_h,
        }
        .run();
        match &mut self.tex {
            Some(t) => t.set(img, TextureOptions::LINEAR),
            None => self.tex = Some(ui.ctx().load_texture("editor-minimap", img, TextureOptions::LINEAR)),
        }
        self.tex_key = key;
    }
}

/// Inputs to a texture rebuild, bundled so [`Cache::ensure_texture`] stays a
/// two-argument call.
struct TexInputs<'a> {
    state: &'a EditorState,
    view: &'a ViewState,
    opts: &'a Options,
    rect: Rect,
    total_content: f32,
    /// Strip-points per content-point (see [`content_scale`]). Computed once
    /// in `show` so the texture and the live overlays share one projection.
    scale: f32,
    line_h: f32,
}

/// Fingerprint of every input that affects the rasterized pixels. Excludes
/// scroll position and the live-overlay colors (thumb / selection / search)
/// so those never trigger a rebuild.
fn pixel_signature(inp: &TexInputs<'_>, w: usize, h: usize) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hh = std::collections::hash_map::DefaultHasher::new();
    (inp.state.doc.content_id() as u64).hash(&mut hh);
    inp.view.decorations.signature.hash(&mut hh);
    w.hash(&mut hh);
    h.hash(&mut hh);
    inp.view.font_size.to_bits().hash(&mut hh);
    // Total content height + wrap state: changing the editor width reflows
    // soft-wrap (new visual rows / heights) without touching the doc or
    // decorations, so fold these in to rebuild on reflow.
    inp.total_content.to_bits().hash(&mut hh);
    inp.view.wrap_map.width().to_bits().hash(&mut hh);
    inp.view.wrap_map.enabled().hash(&mut hh);
    options_signature(inp.opts).hash(&mut hh);
    hh.finish()
}

fn options_signature(o: &Options) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hh = std::collections::hash_map::DefaultHasher::new();
    (o.style as u8).hash(&mut hh);
    for c in [
        o.color_heading,
        o.color_code,
        o.color_emphasis,
        o.color_quote,
        o.color_plain,
        o.color_background,
        o.color_section_rule,
    ] {
        c.to_array().hash(&mut hh);
    }
    for b in [o.colored, o.show_section_rules, o.show_left_edge] {
        b.hash(&mut hh);
    }
    for v in [
        o.width,
        o.bar_padding_left,
        o.bar_padding_right,
        o.min_bar_width,
        o.bar_gap,
    ] {
        v.to_bits().hash(&mut hh);
    }
    hh.finish()
}

/// Headless one-shot rasterization: build the strip image directly from a
/// `Fonts` snapshot (no `Ui`, no texture upload, no `Cache`). The
/// `minimap-render` PNG tool calls this to iterate on visuals offline.
pub fn render_to_image(
    fonts: &egui::epaint::text::Fonts,
    state: &EditorState,
    view: &ViewState,
    opts: &Options,
    size: [usize; 2],
    ppp: f32,
) -> egui::ColorImage {
    let kinds = classify_lines(state, view);
    let metrics = measure_lines(state);
    let atlas = (opts.style == Style::Glyphs).then(|| GlyphAtlas::build(fonts, view.font_size));
    let line_count = state.doc.len_lines();
    let line_h = view.line_height.max(1.0);
    let total_content = view
        .height_map
        .total_height()
        .max(line_count as f32 * line_h)
        .max(1.0);
    let strip_w_pt = size[0] as f32 / ppp;
    let strip_h_pt = size[1] as f32 / ppp;
    let scale_px = content_scale(view, opts, strip_w_pt, strip_h_pt, total_content) * ppp;
    Raster {
        state,
        view,
        opts,
        kinds: &kinds,
        metrics: &metrics,
        atlas: atlas.as_ref(),
        w: size[0],
        h: size[1],
        ppp,
        scale_px,
        line_h,
    }
    .run()
}

/// Unmultiply a premultiplied `Color32` back to its straight color at full
/// opacity. Used to turn the semi-transparent `color_plain` into solid glyph
/// ink without the host having to expose a separate text color.
fn opaque(c: Color32) -> Color32 {
    let a = f32::from(c.a()) / 255.0;
    if a <= 0.0 {
        return c;
    }
    let up = |v: u8| ((f32::from(v) / a).round().min(255.0)) as u8;
    Color32::from_rgb(up(c.r()), up(c.g()), up(c.b()))
}

/// Per-byte foreground color for a line, base color overlaid by any `Mark`
/// decoration's `fg` — the same colors the editor paints the glyphs with.
fn resolve_line_colors(state: &EditorState, view: &ViewState, line: usize, base: Color32) -> Vec<Color32> {
    let s = state.doc.line_str(line);
    let len = s.trim_end_matches(['\n', '\r']).len();
    let mut colors = vec![base; len];
    if len == 0 {
        return colors;
    }
    let start = state.doc.line_to_byte(line);
    let end = start + len;
    for layer in &view.decorations.layers {
        for (range, deco) in layer.iter_overlapping(start..end) {
            let Decoration::Mark(m) = deco else { continue };
            let Some(fg) = m.fg else { continue };
            let c = Color32::from_rgba_unmultiplied(fg.r, fg.g, fg.b, fg.a);
            let lo = range.start.max(start) - start;
            let hi = (range.end.min(end)) - start;
            for slot in colors.iter_mut().take(hi).skip(lo) {
                *slot = c;
            }
        }
    }
    colors
}

/// One frame's rasterization context. Produces a `ColorImage` in physical
/// pixels via [`Self::run`].
struct Raster<'a> {
    state: &'a EditorState,
    view: &'a ViewState,
    opts: &'a Options,
    kinds: &'a [LineKind],
    metrics: &'a [LineMetrics],
    atlas: Option<&'a GlyphAtlas>,
    w: usize,
    h: usize,
    ppp: f32,
    /// Physical pixels per content-point (uniform for glyphs, fit for bars).
    scale_px: f32,
    line_h: f32,
}

/// Per-line glyph layout knobs, bundled to keep `glyph_line` small.
struct GlyphRow {
    pad_l: f32,
    usable: f32,
    cw: f32,
    cw_eff: f32,
    col_step: usize,
    base: Color32,
}

/// Vertical placement + font scale of one display row.
struct RowGeom {
    ry: f32,
    row_h: f32,
    fscale: f32,
}

/// Per-line bar layout knobs, bundled to keep `bar_line` small.
struct BarRow {
    pad_l: f32,
    usable: f32,
    max_visible: f32,
    min_bw: f32,
    gap: f32,
}

impl Raster<'_> {
    fn run(&self) -> egui::ColorImage {
        let mut acc = Accum::new(self.w, self.h);
        self.background_edges(&mut acc);
        let scale = self.scale_px;
        if self.atlas.is_some() {
            self.render_glyphs(&mut acc, scale);
        } else {
            self.render_bars(&mut acc, scale);
        }
        acc.resolve(self.opts.color_background)
    }

    /// Left gutter rule; the background fill itself is applied in `resolve`.
    fn background_edges(&self, acc: &mut Accum) {
        if self.opts.show_left_edge {
            let lw = self.ppp.max(1.0);
            acc.fill(0.0, 0.0, lw, self.h as f32, self.opts.color_section_rule, 1.0);
        }
    }

    /// Number of buffer lines collapsed into one strip pixel row, so tall
    /// documents render ~one representative line per row (VSCode-style
    /// sampling) and the rebuild stays bounded by the strip height.
    fn line_step(&self, scale: f32) -> usize {
        let mlh = self.line_h * scale;
        if mlh < 1.0 {
            (1.0 / mlh).ceil() as usize
        } else {
            1
        }
    }

    fn render_glyphs(&self, acc: &mut Accum, scale: f32) {
        let Some(atlas) = self.atlas else { return };
        let pad_l = self.opts.bar_padding_left * self.ppp;
        let pad_r = self.opts.bar_padding_right * self.ppp;
        let usable = (self.w as f32 - pad_l - pad_r).max(1.0);
        let cw = (atlas.advance * scale).max(0.05);
        let col_step = if cw < 1.0 { (1.0 / cw).floor().max(1.0) as usize } else { 1 };
        let row = GlyphRow {
            pad_l,
            usable,
            cw,
            cw_eff: cw * col_step as f32,
            col_step,
            // Plain glyph ink: the structural plain color at full opacity.
            // `color_plain` is semi-transparent (tuned for bars stacking over
            // the bg); at glyph scale that washes text out, so unmultiply it
            // back to a solid ink.
            base: opaque(self.opts.color_plain),
        };
        // When lines are at least a couple of pixels tall, render the editor's
        // live-preview display model across soft-wrapped visual rows (hidden
        // markers, heading styling, wrap) so the strip reads as a true mini
        // editor. Below that, glyphs are sub-pixel anyway — fall back to the
        // cheap per-line, decimated path that just conveys density.
        let readable = self.view.wrap_map.enabled() && self.line_h * scale >= 2.0;
        let step = self.line_step(scale);
        let mut line = 0;
        while line < self.state.doc.len_lines() {
            if readable {
                self.glyph_line_wrapped(acc, atlas, line, scale, &row);
            } else {
                self.glyph_line(acc, atlas, line, scale, &row);
            }
            line += step;
        }
    }

    /// Live-preview + soft-wrap glyph rendering for one buffer line: each
    /// visual row from the editor's wrap map becomes a minimap row, painted
    /// from the decorated display segments (markers already hidden/replaced).
    fn glyph_line_wrapped(&self, acc: &mut Accum, atlas: &GlyphAtlas, line: usize, scale: f32, row: &GlyphRow) {
        if self.kinds.get(line).copied() == Some(LineKind::Hidden) {
            return;
        }
        let lh = self.view.height_map.text_height(line);
        if lh <= 0.0 {
            return;
        }
        let y_line = self.view.height_map.y_at_text(line) * scale;
        if self.opts.show_section_rules && self.kinds.get(line).copied() == Some(LineKind::Heading) {
            self.section_rule(acc, y_line, row.pad_l, row.usable);
        }
        let rows = display_rows(self.state, self.view, line, row.base);
        let vc = rows.len().max(1) as f32;
        let row_h = (lh * scale) / vc;
        // Per-row font scale (headings are taller AND wider than the base
        // cell). `row_h == line_h * fscale * scale`, so recover `fscale` and
        // widen the glyph advance by it too — otherwise heading glyphs get
        // stretched tall-and-thin.
        let fscale = (lh / (self.line_h * vc)).max(1.0);
        for (vi, drow) in rows.iter().enumerate() {
            let ry = y_line + vi as f32 * row_h;
            self.render_display_row(acc, atlas, drow, &RowGeom { ry, row_h, fscale }, row);
        }
    }

    /// Lay one visual row's display runs left-to-right, blitting each glyph at
    /// the shared per-column advance (widened by the row's font scale for
    /// headings). Inline-widget runs render as a small block.
    fn render_display_row(&self, acc: &mut Accum, atlas: &GlyphAtlas, drow: &DisplayRow, geom: &RowGeom, row: &GlyphRow) {
        let cw = row.cw * geom.fscale;
        let cw_eff = row.cw_eff * geom.fscale;
        let mut col = 0usize;
        for run in &drow.runs {
            if run.is_widget {
                let x0 = row.pad_l + col as f32 * cw;
                if x0 < row.pad_l + row.usable {
                    acc.fill(x0, geom.ry + geom.row_h * 0.2, x0 + cw * 0.8, geom.ry + geom.row_h * 0.8, run.fg, 0.5);
                }
                col += 1;
                continue;
            }
            for ch in run.text.chars() {
                if col % row.col_step != 0 {
                    col += 1;
                    continue;
                }
                let x0 = row.pad_l + col as f32 * cw;
                col += 1;
                if x0 >= row.pad_l + row.usable {
                    continue;
                }
                if ch == ' ' || ch == '\t' {
                    continue;
                }
                let r = Rect::from_min_size(Pos2::new(x0, geom.ry), Vec2::new(cw_eff, geom.row_h));
                match atlas.coverage(ch) {
                    Some(sp) => acc.blit_glyph(sp, [atlas.cw, atlas.ch], r, run.fg),
                    None => acc.fill(r.left(), r.top(), r.right(), r.bottom(), run.fg, 0.45),
                }
            }
        }
    }

    fn glyph_line(&self, acc: &mut Accum, atlas: &GlyphAtlas, line: usize, scale: f32, row: &GlyphRow) {
        if self.kinds.get(line).copied() == Some(LineKind::Hidden) {
            return;
        }
        let lh = self.view.height_map.text_height(line);
        if lh <= 0.0 {
            return;
        }
        let y0 = self.view.height_map.y_at_text(line) * scale;
        let gh = (lh * scale).max(1.0);
        if self.opts.show_section_rules && self.kinds.get(line).copied() == Some(LineKind::Heading) {
            self.section_rule(acc, y0, row.pad_l, row.usable);
        }
        let s = self.state.doc.line_str(line);
        let tl = s.trim_end_matches(['\n', '\r']);
        if tl.is_empty() {
            return;
        }
        let colors = resolve_line_colors(self.state, self.view, line, row.base);
        let mut col = 0usize;
        for (b, ch) in tl.char_indices() {
            if col % row.col_step != 0 {
                col += 1;
                continue;
            }
            let x0 = row.pad_l + col as f32 * row.cw;
            if x0 >= row.pad_l + row.usable {
                break;
            }
            if ch != ' ' && ch != '\t' {
                let color = colors.get(b).copied().unwrap_or(row.base);
                let r = Rect::from_min_size(Pos2::new(x0, y0), Vec2::new(row.cw_eff, gh));
                match atlas.coverage(ch) {
                    Some(sp) => acc.blit_glyph(sp, [atlas.cw, atlas.ch], r, color),
                    None => acc.fill(r.left(), r.top(), r.right(), r.bottom(), color, 0.45),
                }
            }
            col += 1;
        }
    }

    fn render_bars(&self, acc: &mut Accum, scale: f32) {
        let pad_l = self.opts.bar_padding_left * self.ppp;
        let pad_r = self.opts.bar_padding_right * self.ppp;
        let max_visible = self.metrics.iter().map(|m| m.visible).max().unwrap_or(1).max(1) as f32;
        let row = BarRow {
            pad_l,
            usable: (self.w as f32 - pad_l - pad_r).max(1.0),
            max_visible,
            min_bw: self.opts.min_bar_width * self.ppp,
            gap: self.opts.bar_gap * self.ppp,
        };
        let step = self.line_step(scale);
        let mut line = 0;
        while line < self.state.doc.len_lines() {
            self.bar_line(acc, line, scale, &row);
            line += step;
        }
    }

    fn bar_line(&self, acc: &mut Accum, line: usize, scale: f32, row: &BarRow) {
        let kind = self.kinds.get(line).copied().unwrap_or(LineKind::Plain);
        if kind == LineKind::Hidden {
            return;
        }
        let lh = self.view.height_map.text_height(line);
        if lh <= 0.0 {
            return;
        }
        let y0 = self.view.height_map.y_at_text(line) * scale;
        let gh = (lh * scale).max(1.0);
        if kind == LineKind::Heading && self.opts.show_section_rules {
            self.section_rule(acc, y0, row.pad_l, row.usable);
        }
        let m = self.metrics.get(line).copied().unwrap_or_default();
        if m.visible == 0 && m.indent == 0 {
            return;
        }
        let bx = row.pad_l + (m.indent as f32 / row.max_visible) * row.usable;
        let bw = ((m.visible as f32 / row.max_visible) * row.usable).max(row.min_bw);
        let bh = (gh - row.gap).max(1.0);
        acc.fill(bx, y0, bx + bw, y0 + bh, kind.color(self.opts), 1.0);
    }

    fn section_rule(&self, acc: &mut Accum, y0: f32, pad_l: f32, usable: f32) {
        let h = self.ppp.max(1.0);
        let y = (y0 - h).max(0.0);
        acc.fill(pad_l - self.ppp, y, pad_l + usable + self.ppp, y + h, self.opts.color_section_rule, 1.0);
    }
}

pub struct Widget<'a> {
    state: &'a EditorState,
    view: &'a mut ViewState,
    opts: Options,
    cache: Option<&'a mut Cache>,
}

impl<'a> Widget<'a> {
    pub fn new(state: &'a EditorState, view: &'a mut ViewState) -> Self {
        Self { state, view, opts: Options::default(), cache: None }
    }

    /// Plug in a host-owned [`Cache`] so metrics, classification, and the
    /// rasterized texture survive across frames. Without it the widget
    /// rebuilds everything on every `show` — fine for one-shot renders.
    pub const fn with_cache(mut self, cache: &'a mut Cache) -> Self {
        self.cache = Some(cache);
        self
    }

    pub const fn with_width(mut self, width: f32) -> Self {
        self.opts.width = width;
        self
    }

    pub const fn with_options(mut self, opts: Options) -> Self {
        self.opts = opts;
        self
    }

    pub fn show(self, ui: &mut egui::Ui) -> egui::Response {
        let Self { state, view, opts, cache } = self;
        let mut transient;
        let cache = match cache {
            Some(c) => c,
            None => {
                transient = Cache::default();
                &mut transient
            }
        };
        cache.refresh(state, view);

        let height = ui.available_height().max(0.0);
        let (rect, response) =
            ui.allocate_exact_size(Vec2::new(opts.width, height), Sense::click_and_drag());
        let line_count = state.doc.len_lines();
        if line_count == 0 || height <= 1.0 {
            return response;
        }

        // Project on the real content axis: `total_content` reflects soft
        // wrap, heading scale, block widgets, and hidden lines, and
        // `scroll_y` / `view.height` share its units.
        let line_h = view.line_height.max(1.0);
        let total_content = view
            .height_map
            .total_height()
            .max(line_count as f32 * line_h)
            .max(1.0);
        let scale = content_scale(view, &opts, opts.width, rect.height(), total_content);

        cache.ensure_texture(
            ui,
            &TexInputs { state, view, opts: &opts, rect, total_content, scale, line_h },
        );
        let painter = ui.painter_at(rect);
        if let Some(tex) = &cache.tex {
            let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
            painter.image(tex.id(), rect, uv, Color32::WHITE);
        }

        paint_marks(state, view, &opts, &MarkPaint { painter: &painter, rect, scale }, &cache.kinds);
        if opts.show_viewport {
            paint_thumb(view, &opts, &response, &painter, rect, scale);
        }
        handle_interaction(state, view, &response, ui, &Geom { rect, scale, total_content });
        response
    }
}

/// Painter + geometry bundle for the live overlays.
struct MarkPaint<'a> {
    painter: &'a egui::Painter,
    rect: Rect,
    scale: f32,
}

/// Selection + search marks: thin strips along the strip's left gutter for
/// every line a non-empty selection range / search match touches. Drawn over
/// the texture (they track cursor/search state, not scroll, so baking them in
/// would force a rebuild on every cursor move).
fn paint_marks(
    state: &EditorState,
    view: &ViewState,
    opts: &Options,
    mp: &MarkPaint<'_>,
    kinds: &[LineKind],
) {
    let rect = mp.rect;
    let scale = mp.scale;
    let line_count = state.doc.len_lines();
    let doc_len = state.doc.len_bytes();
    let pad_l = opts.bar_padding_left;
    let usable = (rect.width() - pad_l - opts.bar_padding_right).max(1.0);
    let mark_w = (usable * 0.35).clamp(1.5, 4.0);
    let paint = |start: usize, end: usize, color: Color32| {
        let lo = state.doc.byte_to_line(start.min(doc_len));
        let hi = state.doc.byte_to_line(end.saturating_sub(1).max(start).min(doc_len));
        for line in lo..=hi.min(line_count.saturating_sub(1)) {
            if kinds.get(line).copied() == Some(LineKind::Hidden) {
                continue;
            }
            let lh = view.height_map.text_height(line).max(0.0);
            if lh <= 0.0 {
                continue;
            }
            let y = rect.top() + view.height_map.y_at_text(line) * scale;
            let h = (lh * scale).max(1.0);
            let r = Rect::from_min_size(Pos2::new(rect.left() + pad_l, y), Vec2::new(mark_w, h));
            mp.painter.rect_filled(r, CornerRadius::same(1), color);
        }
    };
    for r in state.selection.ranges() {
        if !r.range().is_empty() {
            paint(r.start(), r.end(), opts.color_selection);
        }
    }
    if view.search.active {
        for m in &view.search.matches {
            paint(m.start, m.end, opts.color_search);
        }
    }
}

/// Viewport thumb: a framed rect over the slice of the document currently
/// visible. `scroll_y` and `view.height` are in `total_content` units, so
/// the fractions reflect soft wrap and tall lines the same way the editor does.
fn paint_thumb(
    view: &ViewState,
    opts: &Options,
    response: &egui::Response,
    painter: &egui::Painter,
    rect: Rect,
    scale: f32,
) {
    let active = response.hovered() || response.dragged();
    let fill = if active { opts.color_viewport_hover } else { opts.color_viewport };
    let stroke = {
        let a = (f32::from(fill.a()) * 2.2).clamp(0.0, 255.0) as u8;
        Color32::from_rgba_unmultiplied(fill.r(), fill.g(), fill.b(), a)
    };
    // Project on the shared `scale`, so the thumb tracks the same region the
    // content occupies (which, for glyphs, may be only the top of the strip).
    let vp_y = rect.top() + (view.scroll_y * scale).max(0.0);
    let vp_h = (view.height * scale).clamp(8.0, rect.height());
    let vp = Rect::from_min_size(
        Pos2::new(rect.left() + 1.0, vp_y),
        Vec2::new(rect.width() - 1.0, vp_h),
    );
    painter.rect_filled(vp, CornerRadius::same(2), fill);
    painter.rect_stroke(vp, CornerRadius::same(2), Stroke::new(1.0, stroke), egui::StrokeKind::Inside);
}

/// Strip geometry shared by the press-to-scroll handler.
struct Geom {
    rect: Rect,
    scale: f32,
    total_content: f32,
}

/// Click/drag snaps the viewport to the pressed position; wheel over the
/// strip scrolls the document through the editor's own clamp.
fn handle_interaction(
    state: &EditorState,
    view: &mut ViewState,
    response: &egui::Response,
    ui: &egui::Ui,
    geom: &Geom,
) {
    if let Some(pos) = response.interact_pointer_pos()
        && response.is_pointer_button_down_on()
    {
        // Map the pressed pixel back through the shared `scale` to a content
        // offset, then center the viewport there. `scale` matches the texture
        // projection so the click lands where the user sees it.
        let content_y = (pos.y - geom.rect.top()) / geom.scale.max(f32::EPSILON);
        let target = content_y - view.height * 0.5;
        let max_scroll = (geom.total_content - view.height).max(0.0);
        view.scroll_y = target.clamp(0.0, max_scroll);
    } else if response.hovered() {
        let scrolled = ui.input(|i| i.smooth_scroll_delta.y);
        if scrolled.abs() > 0.0 {
            let speed = if view.scroll_speed > 0.0 { view.scroll_speed } else { 1.0 };
            let _ = command::handle(
                state,
                view,
                &InputEvent::Scroll { delta_x: 0.0, delta_y: scrolled * speed },
            );
        }
    }
}

/// Per-line visible-content metrics. Pure function of the document text;
/// memoized by [`Cache`] on `doc.content_id()`.
fn measure_lines(state: &EditorState) -> Vec<LineMetrics> {
    let line_count = state.doc.len_lines();
    let mut out = Vec::with_capacity(line_count);
    for line in 0..line_count {
        let s = state.doc.line_str(line);
        let total = s.trim_end_matches(['\n', '\r']).len() as u32;
        let indent = s.bytes().take_while(|b| matches!(b, b' ' | b'\t')).count() as u32;
        out.push(LineMetrics { indent, visible: total.saturating_sub(indent) });
    }
    out
}

/// Walk every decoration layer and assign each line the highest-priority kind
/// that overlaps it. Memoized by [`Cache`] on `(content_id, decorations.signature)`.
fn classify_lines(state: &EditorState, view: &ViewState) -> Vec<LineKind> {
    let line_count = state.doc.len_lines();
    let mut out = vec![LineKind::Plain; line_count];
    if line_count == 0 {
        return out;
    }
    let doc_len = state.doc.len_bytes();
    let promote = |slot: &mut LineKind, kind: LineKind| {
        if kind > *slot {
            *slot = kind;
        }
    };
    for layer in &view.decorations.layers {
        for (range, deco) in layer.iter_overlapping(0..doc_len) {
            let lo = state.doc.byte_to_line(range.start.min(doc_len));
            let hi = state
                .doc
                .byte_to_line(range.end.saturating_sub(1).max(range.start).min(doc_len));
            let Some(kind) = deco_kind(deco) else { continue };
            for slot in out.iter_mut().take(hi.min(line_count - 1) + 1).skip(lo) {
                if kind == LineKind::Hidden {
                    *slot = LineKind::Hidden;
                } else if *slot != LineKind::Hidden {
                    promote(slot, kind);
                }
            }
        }
    }
    out
}

/// Map a decoration to the structural line kind it implies, if any.
fn deco_kind(deco: &Decoration) -> Option<LineKind> {
    match deco {
        Decoration::Mark(m) => {
            if m.font_scale.map(|s| s > 1.05).unwrap_or(false) || m.bold {
                Some(LineKind::Heading)
            } else if m.monospace {
                Some(LineKind::Code)
            } else if m.bg.is_some() {
                Some(LineKind::Emphasis)
            } else {
                None
            }
        }
        Decoration::Line(l) => {
            if l.hide {
                Some(LineKind::Hidden)
            } else if l.bg.is_some() {
                Some(LineKind::Quote)
            } else {
                None
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        classify_lines, measure_lines, options_signature, rasterize_glyph_cell,
        resolve_line_colors, Accum, Cache, CellGeom, GlyphMetric, LineKind, Options, Style,
    };
    use editor_core::decoration::{Color, Decoration, MarkStyle};
    use editor_core::rangeset::RangeSet;
    use editor_core::state::Editor as EditorState;
    use editor_view::command;
    use editor_view::events::InputEvent;
    use editor_view::viewport::ViewState;
    use egui::{Color32, ColorImage};

    fn heading_layer(view: &mut ViewState, range: std::ops::Range<usize>) {
        let deco = Decoration::Mark(MarkStyle { bold: true, ..Default::default() });
        view.decorations.push(RangeSet::from_iter([(range, deco)]));
    }

    #[test]
    fn measure_lines_splits_indent_and_visible() {
        let state = EditorState::new("  ab\nxyz\n");
        let m = measure_lines(&state);
        assert_eq!((m[0].indent, m[0].visible), (2, 2));
        assert_eq!((m[1].indent, m[1].visible), (0, 3));
    }

    #[test]
    fn classify_maps_heading_decoration_to_its_line() {
        let state = EditorState::new("plain\n# Heading\nplain\n");
        let mut view = ViewState::default();
        heading_layer(&mut view, 6..15);
        let kinds = classify_lines(&state, &view);
        assert_eq!(kinds[0], LineKind::Plain);
        assert_eq!(kinds[1], LineKind::Heading);
        assert_eq!(kinds[2], LineKind::Plain);
    }

    #[test]
    fn cache_recomputes_only_when_keys_change() {
        let state = EditorState::new("a\nb\nc\n");
        let mut view = ViewState::default();
        heading_layer(&mut view, 0..1);
        let mut cache = Cache::default();
        cache.refresh(&state, &view);
        let kinds0 = cache.kinds.clone();
        let metrics_id0 = cache.metrics_doc_id;
        let kinds_sig0 = cache.kinds_decos_sig;
        cache.refresh(&state, &view);
        assert_eq!(cache.metrics_doc_id, metrics_id0);
        assert_eq!(cache.kinds_decos_sig, kinds_sig0);
        assert_eq!(cache.kinds, kinds0);
        assert_eq!(cache.kinds, classify_lines(&state, &view));
        heading_layer(&mut view, 2..3);
        cache.refresh(&state, &view);
        assert_ne!(cache.kinds_decos_sig, kinds_sig0);
        assert_eq!(cache.metrics_doc_id, metrics_id0);
        assert_eq!(cache.kinds[1], LineKind::Heading);
    }

    #[test]
    fn cache_metrics_key_tracks_document_identity() {
        let view = ViewState::default();
        let mut cache = Cache::default();
        let s1 = EditorState::new("hello\n");
        cache.refresh(&s1, &view);
        let id1 = cache.metrics_doc_id;
        assert_eq!(id1, s1.doc.content_id());
        let tx = s1.insert_at_selections("X");
        let s2 = s1.apply(tx);
        cache.refresh(&s2, &view);
        assert_ne!(cache.metrics_doc_id, id1);
        assert_eq!(cache.metrics_doc_id, s2.doc.content_id());
    }

    #[test]
    fn scroll_delta_clamps_at_top_and_bottom() {
        let state = EditorState::new("x\n");
        let mut view = ViewState { height: 100.0, scroll_y: 5.0, ..Default::default() };
        let _ = command::handle(&state, &mut view, &InputEvent::Scroll { delta_x: 0.0, delta_y: 50.0 });
        assert_eq!(view.scroll_y, 0.0);
        let _ = command::handle(&state, &mut view, &InputEvent::Scroll { delta_x: 0.0, delta_y: -50.0 });
        assert_eq!(view.scroll_y, 0.0);
    }

    #[test]
    fn rasterize_places_glyph_at_baseline() {
        // Fully-covered atlas bitmap.
        let img = ColorImage::new([4, 4], vec![Color32::from_white_alpha(255); 16]);
        // Cell is a 4×8 line box; the glyph bitmap spans points y∈[2,6) (top
        // 4px above the baseline at y=6), full width.
        let g = CellGeom { advance: 4.0, font_h: 8.0, cw: 4, ch: 8 };
        let m = GlyphMetric {
            pos: egui::pos2(0.0, 6.0),
            offset: egui::vec2(0.0, -4.0),
            size: egui::vec2(4.0, 4.0),
            min: [0, 0],
            max: [4, 4],
        };
        let mut out = vec![0.0f32; g.cw * g.ch];
        rasterize_glyph_cell(&img, &m, &g, &mut out);
        // Inside the bitmap band → inked; above and below → empty (baseline
        // preserved, glyph does NOT fill the whole cell).
        assert!(out[3 * g.cw] > 0.9, "row 3 (inside glyph) should be inked");
        assert!(out[0] < 0.1, "row 0 (above glyph) should be empty");
        assert!(out[7 * g.cw] < 0.1, "row 7 (below baseline) should be empty");
    }

    #[test]
    fn resolve_line_colors_overlays_mark_fg() {
        let state = EditorState::new("hello\n");
        let mut view = ViewState::default();
        let red = Color::rgba(200, 30, 30, 255);
        let deco = Decoration::Mark(MarkStyle { fg: Some(red), ..Default::default() });
        // Color bytes 0..3 ("hel") red.
        view.decorations.push(RangeSet::from_iter([(0..3, deco)]));
        let base = Color32::from_gray(100);
        let colors = resolve_line_colors(&state, &view, 0, base);
        assert_eq!(colors.len(), 5); // "hello"
        assert_eq!(colors[0], Color32::from_rgba_unmultiplied(200, 30, 30, 255));
        assert_eq!(colors[2], Color32::from_rgba_unmultiplied(200, 30, 30, 255));
        assert_eq!(colors[3], base);
    }

    #[test]
    fn accum_fill_and_resolve_compose_over_background() {
        let mut acc = Accum::new(2, 1);
        // Opaque white over the left pixel, nothing over the right.
        acc.fill(0.0, 0.0, 1.0, 1.0, Color32::WHITE, 1.0);
        let img = acc.resolve(Color32::from_rgba_premultiplied(0, 0, 0, 40));
        // Left: opaque white content wins.
        assert_eq!(img.pixels[0], Color32::from_rgba_premultiplied(255, 255, 255, 255));
        // Right: only the translucent background shows through.
        assert_eq!(img.pixels[1].a(), 40);
    }

    #[test]
    fn options_signature_tracks_style_and_palette() {
        let bars = Options { style: Style::Bars, ..Default::default() };
        let glyphs = Options { style: Style::Glyphs, ..Default::default() };
        assert_ne!(options_signature(&bars), options_signature(&glyphs));
        // Same options → stable signature (so idle frames don't rebuild).
        assert_eq!(options_signature(&glyphs), options_signature(&Options::default()));
        let recolored = Options { color_heading: Color32::RED, ..Default::default() };
        assert_ne!(options_signature(&recolored), options_signature(&Options::default()));
    }
}
