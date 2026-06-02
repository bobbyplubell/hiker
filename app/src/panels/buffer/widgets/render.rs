//! App-side render → RGBA helper for editor widgets (LaTeX math, Mermaid).
//!
//! This is the `app` half of the widget render pipeline (docs/editor-widgets.md,
//! `widget-render-pipeline`): it turns a math/mermaid *source string* into
//! rasterized RGBA pixels using the egui-agnostic `hiker-render` crates
//! (`hiker_math`, `hiker_mermaid`), rasterizing their SVG output with `resvg`
//! the same way `hiker-htmlview` does. It owns no editor wiring and no
//! decoration emission — the editor crates consume the [`RenderedWidget`]
//! (raw RGBA + size + a content hash) and own the GPU texture upload/cache/blit.
//!
//! Pixel format: straight (un-premultiplied) RGBA8, tightly packed
//! `width * height * 4` bytes, in physical pixels (logical size × `dpr`).
//!
//! status: widget-render-pipeline

use std::hash::{Hash, Hasher};

use hiker_math::{MathOptions, MathRender, MathStyle, render_latex_with_preamble};
use hiker_mermaid::{
    HitRegion, MermaidOptions, render as render_mermaid_svg, render_with_regions,
};
use hiker_wavedrom::{WaveDromOptions, render as render_wavedrom_svg};

/// A rasterized widget: straight RGBA8 pixels plus the metrics the editor needs
/// to size, baseline-align, and cache it.
#[derive(Clone, Debug, PartialEq)]
pub struct RenderedWidget {
    /// Tightly-packed straight (un-premultiplied) RGBA8, `width * height * 4`
    /// bytes, in physical pixels.
    pub rgba: Vec<u8>,
    /// Physical pixel width.
    pub width: u32,
    /// Physical pixel height.
    pub height: u32,
    /// For inline math only: distance from the box top to the baseline in
    /// physical px (so the formula sits on the surrounding text's baseline).
    /// `None` for block widgets (display math, mermaid).
    pub baseline: Option<f32>,
    /// Hash of every input that affects the pixels (source, kind/style,
    /// `font_px`, `dpr`, theme colors). Used downstream as the widget's
    /// `widget_id` for the texture cache.
    pub content_hash: u64,
}

/// Which LaTeX style a math source renders in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MathKind {
    /// `$…$` — compact, baseline-aligned inline widget.
    Inline,
    /// `$$…$$` — full-size, centered block widget.
    Display,
}

impl MathKind {
    const fn to_style(self) -> MathStyle {
        match self {
            MathKind::Inline => MathStyle::Inline,
            MathKind::Display => MathStyle::Display,
        }
    }
}

/// Guard against a pathological SVG viewBox or `dpr` blowing up memory — the
/// same cap `hiker-htmlview` uses for its inline diagrams.
const MAX_DIM_PX: f32 = 8192.0;

/// Render a LaTeX `src` to RGBA pixels at `font_px * dpr` physical size.
///
/// `fg` is the glyph foreground color as straight RGBA, threaded into
/// [`MathOptions::color`] so light/dark contrast is correct
/// (`widget-render-theme-color`). `preamble` is the optional per-vault macro
/// string (`widget-latex-preamble`, plumbed but typically empty in v1).
/// Returns `None` on a parse/layout failure — the caller falls back to tinted
/// source, never a crash.
pub fn render_math(
    src: &str,
    kind: MathKind,
    font_px: f32,
    dpr: f32,
    fg: [u8; 4],
    preamble: &str,
) -> Option<RenderedWidget> {
    let opts = MathOptions {
        font_size_px: font_px,
        color: fg,
        style: kind.to_style(),
    };
    let MathRender {
        svg,
        width_px: _,
        height_px: _,
        baseline_px,
    } = render_latex_with_preamble(src, preamble, &opts)?;

    let content_hash = hash_math(src, kind, font_px, dpr, fg, preamble);
    let (rgba, width, height) = rasterize_svg(svg.as_bytes(), dpr)?;

    // The SVG is authored at logical (CSS) px; rasterizing at `dpr` scales the
    // whole canvas, so the baseline metric scales the same way.
    let baseline = match kind {
        MathKind::Inline => Some(baseline_px * dpr),
        MathKind::Display => None,
    };

    Some(RenderedWidget {
        rgba,
        width,
        height,
        baseline,
        content_hash,
    })
}

/// Render mermaid `src` to RGBA pixels at `font_px * dpr` physical size.
///
/// Theme colors are threaded into [`MermaidOptions`] (`widget-render-theme-color`).
/// Returns `None` on any `MermaidError` (parse / unsupported type / empty) so
/// the caller falls back to tinted source.
pub fn render_mermaid(
    src: &str,
    font_px: f32,
    dpr: f32,
    colors: MermaidColors,
) -> Option<RenderedWidget> {
    let mut opts = MermaidOptions {
        font_size_px: font_px,
        ..MermaidOptions::default()
    };
    colors.apply(&mut opts);

    let rendered = render_mermaid_svg(src, &opts).ok()?;
    let content_hash = hash_mermaid(src, font_px, dpr, colors);
    let (rgba, width, height) = rasterize_svg(rendered.svg.as_bytes(), dpr)?;

    Some(RenderedWidget {
        rgba,
        width,
        height,
        // Block widget: no baseline alignment.
        baseline: None,
        content_hash,
    })
}

/// Render WaveDrom WaveJSON `src` to RGBA pixels at `font_px * dpr` physical
/// size. WaveDrom is a block widget like display math / mermaid (no baseline,
/// no interaction regions — WaveJSON has no link/click model). Theme colors are
/// threaded into [`WaveDromOptions`] (`widget-render-theme-color`). Returns
/// `None` on any `WaveDromError` (parse / empty / unsupported) so the caller
/// falls back to tinted source. status: widget-wavedrom-render
pub fn render_wavedrom(
    src: &str,
    font_px: f32,
    dpr: f32,
    colors: WaveDromColors,
) -> Option<RenderedWidget> {
    let opts = WaveDromOptions {
        font_size_px: font_px,
        foreground: colors.foreground,
        background: colors.background,
        ..WaveDromOptions::default()
    };

    let rendered = render_wavedrom_svg(src, &opts).ok()?;
    let content_hash = hash_wavedrom(src, font_px, dpr, colors);
    let (rgba, width, height) = rasterize_svg(rendered.svg.as_bytes(), dpr)?;

    Some(RenderedWidget {
        rgba,
        width,
        height,
        // Block widget: no baseline alignment.
        baseline: None,
        content_hash,
    })
}

/// A clickable / hoverable sub-region of a rendered mermaid diagram, in
/// NORMALIZED widget coordinates: `x`/`y`/`w`/`h` are fractions in `0.0..1.0`
/// of the diagram's painted box (= the `hiker_mermaid::HitRegion`'s pixel rect
/// divided by the render's `width_px` / `height_px`). The editor maps these
/// through the same aspect-preserving letterbox it uses to blit the texture, so
/// a region lines up with what's drawn regardless of scale or DPR.
///
/// `link` is the diagram's `click X "…"` target (classified at dispatch time by
/// `core::url::classify`); `tooltip` is the optional hover title. `callback`
/// from the mermaid model is dropped — there is no JS engine to invoke.
/// status: widget-mermaid-links
#[derive(Clone, Debug, PartialEq)]
pub struct DiagramRegion {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub link: Option<String>,
    pub tooltip: Option<String>,
}

/// Render mermaid `src` like [`render_mermaid`], additionally returning the
/// per-element interaction regions for diagram types that carry them
/// (flowcharts via `graph`/`flowchart`, class diagrams via `classDiagram`, and
/// the state / ER variants `hiker_mermaid::render_with_regions` recognizes).
///
/// Region pixel rects (`HitRegion.{x,y,w,h}`, in the SVG `viewBox` space) are
/// normalized to `0.0..1.0` fractions of the render's `width_px` / `height_px`
/// so they survive the editor's letterbox blit. Diagram types with no
/// interaction model yield an empty region list (the diagram still renders).
/// Returns `None` on any render failure, exactly like [`render_mermaid`], so
/// the caller falls back to tinted source. status: widget-mermaid-links
pub fn render_mermaid_with_regions(
    src: &str,
    font_px: f32,
    dpr: f32,
    colors: MermaidColors,
) -> Option<(RenderedWidget, Vec<DiagramRegion>)> {
    let MermaidLayout { svg, content_hash, regions } =
        mermaid_layout(src, font_px, dpr, colors)?;
    let (rgba, width, height) = rasterize_svg(svg.as_bytes(), dpr)?;

    Some((
        RenderedWidget {
            rgba,
            width,
            height,
            // Block widget: no baseline alignment.
            baseline: None,
            content_hash,
        },
        regions,
    ))
}

/// The raster-free product of a mermaid parse + layout: the SVG, the
/// content hash, and the normalized interaction regions. Shared by the
/// rasterizing [`render_mermaid_with_regions`] and the raster-free
/// [`mermaid_regions`] so both derive identical hashes + regions.
struct MermaidLayout {
    svg: String,
    content_hash: u64,
    regions: Vec<DiagramRegion>,
}

/// Parse + lay out mermaid `src`, returning its SVG, `content_hash`, and
/// normalized [`DiagramRegion`]s — *without* rasterizing (no resvg). The
/// mermaid parse/layout is cheap relative to the resvg blit, so this is the
/// per-frame path for the region registry / edit-target builders.
fn mermaid_layout(
    src: &str,
    font_px: f32,
    dpr: f32,
    colors: MermaidColors,
) -> Option<MermaidLayout> {
    let mut opts = MermaidOptions {
        font_size_px: font_px,
        ..MermaidOptions::default()
    };
    colors.apply(&mut opts);

    let (rendered, hits) = render_with_regions(src, &opts).ok()?;
    let content_hash = hash_mermaid(src, font_px, dpr, colors);
    let regions = normalize_regions(&hits, rendered.width_px, rendered.height_px);
    Some(MermaidLayout { svg: rendered.svg, content_hash, regions })
}

/// Derive a mermaid diagram's `content_hash` + normalized interaction regions
/// *without* rasterizing (no resvg blit). The per-frame region registry and the
/// click-to-edit target map use this so they don't pay the SVG → RGBA cost the
/// widget layer's [`render_mermaid_with_regions`] does; the `content_hash` is
/// identical to that path's so ids / cache keys match. Returns `None` on any
/// render failure, exactly like [`render_mermaid_with_regions`].
/// status: widget-mermaid-links
pub fn mermaid_regions(
    src: &str,
    font_px: f32,
    dpr: f32,
    colors: MermaidColors,
) -> Option<(u64, Vec<DiagramRegion>)> {
    let MermaidLayout { content_hash, regions, .. } =
        mermaid_layout(src, font_px, dpr, colors)?;
    Some((content_hash, regions))
}

/// Convert `hiker_mermaid` viewBox-pixel hit regions into normalized
/// `0.0..1.0` [`DiagramRegion`]s against the render's pixel size. A degenerate
/// render size (≤ 0) yields no regions — there is nothing to hit-test against.
fn normalize_regions(hits: &[HitRegion], width_px: f32, height_px: f32) -> Vec<DiagramRegion> {
    if width_px <= 0.0 || height_px <= 0.0 {
        return Vec::new();
    }
    hits
        .iter()
        .map(|h| DiagramRegion {
            x: h.x / width_px,
            y: h.y / height_px,
            w: h.w / width_px,
            h: h.h / height_px,
            link: h.link.clone(),
            tooltip: h.tooltip.clone(),
        })
        .collect()
}

/// Theme-derived mermaid colors, all straight RGBA. Threaded into
/// [`MermaidOptions`] so light/dark render correct contrast without a parallel
/// stylesheet (`widget-render-theme-color`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MermaidColors {
    /// Canvas background (alpha 0 → transparent).
    pub background: [u8; 4],
    /// Node fill / stroke.
    pub node_fill: [u8; 4],
    pub node_stroke: [u8; 4],
    /// Edge line color.
    pub edge_stroke: [u8; 4],
    /// Label text color.
    pub text_color: [u8; 4],
}

impl MermaidColors {
    const fn apply(self, opts: &mut MermaidOptions) {
        opts.background = self.background;
        opts.node_fill = self.node_fill;
        opts.node_stroke = self.node_stroke;
        opts.edge_stroke = self.edge_stroke;
        opts.text_color = self.text_color;
    }
}

/// Theme-derived WaveDrom colors, all straight RGBA. Threaded into
/// [`WaveDromOptions`] so light/dark render correct contrast
/// (`widget-render-theme-color`). The categorical series palette (data buses /
/// bitfields) keeps WaveDrom's default skin — it's purpose-chosen and theme-
/// neutral — so only foreground (lines/text) and background are theme-driven.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WaveDromColors {
    /// Lines + label text color.
    pub foreground: [u8; 4],
    /// Canvas background (alpha 0 → transparent, sits on the editor surface).
    pub background: [u8; 4],
}

/// Shared SVG font database: system fonts plus the bundled mermaid sans
/// (`LiberationSans`, the default `font-family: sans-serif` the diagrams emit),
/// loaded once. Without a populated `fontdb`, resvg renders no glyphs for the
/// `<text>` elements mermaid uses for node/edge labels — so the diagram draws
/// but every label is blank. Mirrors `hiker-htmlview`'s `svg_fontdb`, and also
/// loads the bundled face so labels resolve even on a system with no
/// `sans-serif`.
fn svg_fontdb() -> std::sync::Arc<resvg::usvg::fontdb::Database> {
    use std::sync::{Arc, OnceLock};
    static DB: OnceLock<Arc<resvg::usvg::fontdb::Database>> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = resvg::usvg::fontdb::Database::new();
        db.load_system_fonts();
        db.load_font_data(hiker_mermaid::font::FONT_BYTES.to_vec());
        // fontdb defaults the generic `sans-serif` family to "Arial", which is
        // absent on Linux — so `<text font-family="sans-serif">` (what mermaid
        // emits) resolves to nothing and every label renders blank. Point the
        // generics at the bundled Liberation face (always present) / installed
        // Liberation siblings, matching `hiker-mermaid`'s own rasterize example.
        db.set_sans_serif_family(hiker_mermaid::font::FONT_FAMILY);
        db.set_serif_family("Liberation Serif");
        db.set_monospace_family("Liberation Mono");
        Arc::new(db)
    })
    .clone()
}

/// Rasterize an SVG document to straight RGBA8 at `dpr` physical density.
///
/// Mirrors `hiker-htmlview`'s resvg → tiny-skia pixmap path. tiny-skia stores
/// pixels *premultiplied*, so we un-premultiply to straight RGBA8 (the natural
/// `RGBA8` contract callers and egui's `from_rgba_unmultiplied` expect).
/// Returns `None` on a parse failure or a degenerate size.
fn rasterize_svg(bytes: &[u8], dpr: f32) -> Option<(Vec<u8>, u32, u32)> {
    let opt = resvg::usvg::Options {
        fontdb: svg_fontdb(),
        ..resvg::usvg::Options::default()
    };
    let rtree = resvg::usvg::Tree::from_data(bytes, &opt).ok()?;
    let size = rtree.size();
    let iw = size.width();
    let ih = size.height();
    if iw <= 0.0 || ih <= 0.0 {
        return None;
    }

    let density = if dpr.is_finite() && dpr > 0.0 { dpr } else { 1.0 };
    let tw = (iw * density).round().clamp(1.0, MAX_DIM_PX);
    let th = (ih * density).round().clamp(1.0, MAX_DIM_PX);

    let mut pixmap = resvg::tiny_skia::Pixmap::new(tw as u32, th as u32)?;
    let transform = resvg::tiny_skia::Transform::from_scale(tw / iw, th / ih);
    resvg::render(&rtree, transform, &mut pixmap.as_mut());

    let width = pixmap.width();
    let height = pixmap.height();
    let mut rgba = pixmap.take();
    unpremultiply(&mut rgba);
    Some((rgba, width, height))
}

/// Convert premultiplied RGBA8 (tiny-skia's storage) to straight RGBA8 in place.
fn unpremultiply(px: &mut [u8]) {
    for chunk in px.chunks_exact_mut(4) {
        let a = chunk[3];
        if a == 0 || a == 255 {
            continue;
        }
        let a = u32::from(a);
        for c in &mut chunk[..3] {
            // round(c * 255 / a)
            let v = (u32::from(*c) * 255 + a / 2) / a;
            *c = v.min(255) as u8;
        }
    }
}

/// The `content_hash` [`render_math`] would compute for these inputs, without
/// parsing or rasterizing. The edit-preview overlay calls this to decide whether
/// its cached texture is still current before paying the SVG → RGBA cost on a
/// static span (`widget-edit-popup-preview`, `widget-render-cache`).
pub fn math_content_hash(src: &str, kind: MathKind, font_px: f32, dpr: f32, fg: [u8; 4]) -> u64 {
    hash_math(src, kind, font_px, dpr, fg, "")
}

/// The `content_hash` [`render_mermaid`] would compute for these inputs, without
/// parsing or rasterizing — the diagram counterpart to [`math_content_hash`].
pub fn mermaid_content_hash(src: &str, font_px: f32, dpr: f32, colors: MermaidColors) -> u64 {
    hash_mermaid(src, font_px, dpr, colors)
}

/// The `content_hash` [`render_wavedrom`] would compute for these inputs,
/// without parsing or rasterizing — used by the edit-target map and the
/// edit-preview overlay so ids / cache keys match the rendered widget.
/// status: widget-wavedrom-render
pub fn wavedrom_content_hash(src: &str, font_px: f32, dpr: f32, colors: WaveDromColors) -> u64 {
    hash_wavedrom(src, font_px, dpr, colors)
}

fn hash_math(
    src: &str,
    kind: MathKind,
    font_px: f32,
    dpr: f32,
    fg: [u8; 4],
    preamble: &str,
) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    // Domain tag so a math hash never collides with a mermaid hash.
    "math".hash(&mut h);
    src.hash(&mut h);
    kind.hash(&mut h);
    font_px.to_bits().hash(&mut h);
    dpr.to_bits().hash(&mut h);
    fg.hash(&mut h);
    preamble.hash(&mut h);
    h.finish()
}

fn hash_mermaid(src: &str, font_px: f32, dpr: f32, colors: MermaidColors) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    "mermaid".hash(&mut h);
    src.hash(&mut h);
    font_px.to_bits().hash(&mut h);
    dpr.to_bits().hash(&mut h);
    colors.hash(&mut h);
    h.finish()
}

fn hash_wavedrom(src: &str, font_px: f32, dpr: f32, colors: WaveDromColors) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    // Domain tag so a wavedrom hash never collides with a math / mermaid hash.
    "wavedrom".hash(&mut h);
    src.hash(&mut h);
    font_px.to_bits().hash(&mut h);
    dpr.to_bits().hash(&mut h);
    colors.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FG: [u8; 4] = [220, 220, 220, 255];

    fn mermaid_colors() -> MermaidColors {
        MermaidColors {
            background: [0, 0, 0, 0],
            node_fill: [40, 40, 60, 255],
            node_stroke: [120, 110, 200, 255],
            edge_stroke: [200, 200, 200, 255],
            text_color: [220, 220, 220, 255],
        }
    }

    fn assert_well_formed(w: &RenderedWidget) {
        assert!(w.width > 0, "width should be non-zero");
        assert!(w.height > 0, "height should be non-zero");
        assert_eq!(
            w.rgba.len(),
            (w.width as usize) * (w.height as usize) * 4,
            "rgba must be tightly packed width*height*4"
        );
        assert!(!w.rgba.is_empty(), "rgba must be non-empty");
    }

    #[test]
    fn inline_math_renders_with_baseline() {
        let w = render_math("x^2", MathKind::Inline, 16.0, 2.0, FG, "")
            .expect("x^2 should render");
        assert_well_formed(&w);
        assert!(w.baseline.is_some(), "inline math carries a baseline");
    }

    #[test]
    fn display_math_renders_without_baseline() {
        let w = render_math("\\frac{a}{b}", MathKind::Display, 18.0, 1.5, FG, "")
            .expect("\\frac{a}{b} should render");
        assert_well_formed(&w);
        assert!(w.baseline.is_none(), "block math has no baseline");
    }

    #[test]
    fn mermaid_renders() {
        let w = render_mermaid("graph TD; A-->B", 16.0, 1.0, mermaid_colors())
            .expect("a trivial flowchart should render");
        assert_well_formed(&w);
        assert!(w.baseline.is_none(), "mermaid is a block widget");
    }

    #[test]
    fn broken_mermaid_returns_none() {
        let w = render_mermaid(
            "not a real diagram type at all",
            16.0,
            1.0,
            mermaid_colors(),
        );
        assert!(w.is_none(), "an unparseable diagram falls back (None)");
    }

    #[test]
    fn mermaid_with_regions_carries_link_and_tooltip() {
        // status: widget-mermaid-links — a flowchart with a `click` directive
        // yields a normalized region carrying the link + tooltip.
        let (w, regions) = render_mermaid_with_regions(
            "graph TD\n A[Start]\n click A \"https://x\" \"go\"",
            16.0,
            1.0,
            mermaid_colors(),
        )
        .expect("flowchart should render");
        assert_well_formed(&w);
        let linked = regions
            .iter()
            .find(|r| r.link.is_some())
            .expect("a linked region");
        assert_eq!(linked.link.as_deref(), Some("https://x"));
        assert_eq!(linked.tooltip.as_deref(), Some("go"));
        // Normalized: within the unit box.
        assert!(linked.x >= 0.0 && linked.x + linked.w <= 1.001);
        assert!(linked.y >= 0.0 && linked.y + linked.h <= 1.001);
    }

    #[test]
    fn mermaid_with_regions_empty_for_non_interactive() {
        // A pie chart has no interaction model: it renders, with no regions.
        let (w, regions) =
            render_mermaid_with_regions("pie\n \"A\" : 10\n \"B\" : 20", 16.0, 1.0, mermaid_colors())
                .expect("pie should render");
        assert_well_formed(&w);
        assert!(regions.is_empty(), "non-interactive diagram has no regions");
    }

    #[test]
    fn mermaid_with_regions_hash_matches_plain() {
        // The region-bearing render shares the plain render's content hash for
        // the same inputs, so the texture cache key is identical.
        let plain = render_mermaid("graph TD; A-->B", 16.0, 1.0, mermaid_colors()).unwrap();
        let (with, _) =
            render_mermaid_with_regions("graph TD; A-->B", 16.0, 1.0, mermaid_colors()).unwrap();
        assert_eq!(plain.content_hash, with.content_hash);
    }

    #[test]
    fn raster_free_regions_match_rasterizing_path() {
        // status: widget-mermaid-links — the raster-free `mermaid_regions`
        // returns the same content hash + region count as the rasterizing
        // `render_mermaid_with_regions` for the same input, so the registry /
        // edit-target ids and cache keys match without paying the resvg blit.
        let src = "graph TD\n A[Start]\n click A \"https://x\" \"go\"";
        let (rendered, regions) =
            render_mermaid_with_regions(src, 16.0, 1.0, mermaid_colors()).expect("rasterized");
        let (hash, raster_free) =
            mermaid_regions(src, 16.0, 1.0, mermaid_colors()).expect("raster-free");
        assert_eq!(rendered.content_hash, hash, "content hash matches");
        assert_eq!(regions.len(), raster_free.len(), "region count matches");
        assert_eq!(regions, raster_free, "regions identical");
    }

    #[test]
    fn same_inputs_same_hash() {
        let a = render_math("x^2", MathKind::Inline, 16.0, 2.0, FG, "").unwrap();
        let b = render_math("x^2", MathKind::Inline, 16.0, 2.0, FG, "").unwrap();
        assert_eq!(a.content_hash, b.content_hash);
    }

    #[test]
    fn different_font_px_different_hash() {
        let a = render_math("x^2", MathKind::Inline, 16.0, 2.0, FG, "").unwrap();
        let b = render_math("x^2", MathKind::Inline, 20.0, 2.0, FG, "").unwrap();
        assert_ne!(a.content_hash, b.content_hash);
    }

    #[test]
    fn math_and_mermaid_hashes_dont_collide() {
        let m = render_math("x^2", MathKind::Inline, 16.0, 1.0, FG, "").unwrap();
        let d = render_mermaid("graph TD; A-->B", 16.0, 1.0, mermaid_colors()).unwrap();
        assert_ne!(m.content_hash, d.content_hash);
    }

    fn wavedrom_colors() -> WaveDromColors {
        WaveDromColors {
            foreground: [220, 220, 220, 255],
            background: [0, 0, 0, 0],
        }
    }

    #[test]
    fn wavedrom_renders() {
        let w = render_wavedrom(
            "{ signal: [{ name: 'clk', wave: 'p...' }] }",
            16.0,
            1.0,
            wavedrom_colors(),
        )
        .expect("a trivial waveform should render");
        assert_well_formed(&w);
        assert!(w.baseline.is_none(), "wavedrom is a block widget");
    }

    #[test]
    fn broken_wavedrom_returns_none() {
        // Not WaveJSON at all → parse error → None (fall back to tinted source).
        let w = render_wavedrom("this is not wavejson", 16.0, 1.0, wavedrom_colors());
        assert!(w.is_none(), "unparseable WaveJSON falls back (None)");
    }

    #[test]
    fn wavedrom_same_inputs_same_hash() {
        let src = "{ signal: [{ name: 'clk', wave: 'p..' }] }";
        let a = render_wavedrom(src, 16.0, 1.0, wavedrom_colors()).unwrap();
        let b = render_wavedrom(src, 16.0, 1.0, wavedrom_colors()).unwrap();
        assert_eq!(a.content_hash, b.content_hash);
        assert_eq!(
            a.content_hash,
            wavedrom_content_hash(src, 16.0, 1.0, wavedrom_colors()),
            "raster-free hash matches the rendered hash"
        );
    }

    #[test]
    fn wavedrom_hash_distinct_from_math_and_mermaid() {
        // Same source string through different domains must not collide.
        let wd = wavedrom_content_hash("graph TD; A-->B", 16.0, 1.0, wavedrom_colors());
        let mm = render_mermaid("graph TD; A-->B", 16.0, 1.0, mermaid_colors()).unwrap();
        assert_ne!(wd, mm.content_hash, "wavedrom vs mermaid domain-tagged");
    }
}
