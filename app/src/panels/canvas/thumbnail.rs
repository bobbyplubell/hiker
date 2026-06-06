//! Canvas preview thumbnail: a flat SVG sketch of a `.canvas` document's
//! shape — one rounded rect per node (in its kind/preset color), one line per
//! edge — scaled by the zoom-to-fit transform, rasterized through the shared
//! `render::rasterize_svg`.
//!
//! This is deliberately an *approximation* of the canvas, not a faithful
//! render: egui can't capture the live `canvas_view::Widget` to a texture (it
//! needs a live `Ui` and a `!Send` per-node content engine), so the thumbnail
//! draws the node/edge geometry only — no node bodies, no embedded files. It
//! reads enough (geometry + color + node kind) to be recognizable in a list.
//!
//! status: preview-canvas-thumbnail

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use hiker_canvas::color::Color;
use hiker_canvas::geometry::{content_bounds, node_anchor, node_bounds, Rect};
use hiker_canvas::model::{Canvas, Edge, Node, NodeKind};

use crate::widgets::preview::{content_hash, ExpandedPaint, PreviewKey, PreviewKind, ThumbnailProvider};

/// A canvas thumbnail provider over a `.canvas` document's raw bytes. Cheap to
/// construct — it owns the bytes and parses lazily on render; the content hash
/// is over the bytes so any edit invalidates the cache.
pub struct CanvasPreview {
    bytes: String,
    raw_hash: u64,
}

impl CanvasPreview {
    /// Build a provider from the `.canvas` file's text.
    pub fn new(bytes: String) -> Self {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        bytes.hash(&mut h);
        let raw_hash = h.finish();
        Self { bytes, raw_hash }
    }
}

impl ThumbnailProvider for CanvasPreview {
    fn cache_key(&self) -> PreviewKey {
        PreviewKey {
            content_hash: content_hash(PreviewKind::Canvas, self.raw_hash),
            kind: PreviewKind::Canvas,
            // Size bucket is overwritten by the widget per render size.
            size: 0,
        }
    }

    fn render(&self, px: u32) -> Option<image::RgbaImage> {
        let canvas = Canvas::from_json(&self.bytes).ok()?;
        let bounds = content_bounds(&canvas)?;
        let svg = canvas_svg(&canvas, bounds, px);
        let (rgba, w, h) = crate::panels::buffer::widgets::render::rasterize_svg(svg.as_bytes(), 1.0)?;
        image::RgbaImage::from_raw(w, h, rgba)
    }

    /// LIVE-PAINT the expanded canvas preview via the real renderer's display-only
    /// path (`canvas_view::CanvasView::show_static`): parse the bytes (cached per
    /// content hash so a hover doesn't re-parse every frame), build a fresh view,
    /// zoom-to-fit the rect, and paint frames + groups + edges + LOD placeholders
    /// with a no-op content engine. At fit zoom every node is a LOD placeholder,
    /// so no content engine is needed — which is exactly why this works inside the
    /// non-interactable expanded `Area`. `None` on a parse failure → the caller
    /// falls back / shows nothing. status: canvas-static-paint
    fn expanded_paint(&self) -> Option<ExpandedPaint> {
        // Bail early on unparseable / empty canvases so the caller falls back
        // rather than installing a thunk that paints nothing.
        let canvas = parse_cached(self.raw_hash, &self.bytes)?;
        content_bounds(&canvas)?;
        // The thunk must be `Send + Sync` (egui temp store), so it can only
        // capture the bytes + hash, not the `Rc<Canvas>` (which is `!Send`). It
        // re-fetches the parsed canvas from the UI-thread cache when it runs.
        let bytes = self.bytes.clone();
        let hash = self.raw_hash;
        Some(std::sync::Arc::new(move |ui: &mut eframe::egui::Ui, rect: eframe::egui::Rect| {
            let Some(canvas) = parse_cached(hash, &bytes) else {
                return false;
            };
            let mut view = canvas_view::widget::CanvasView::new();
            view.set_grid(false);
            view.fit(rect, &canvas);
            view.show_static(ui, &canvas, &mut canvas_view::content::NoContentRenderer);
            true
        }))
    }
}

thread_local! {
    /// Per-content-hash cache of parsed canvases, so a hover (which fires every
    /// frame) doesn't re-parse the `.canvas` JSON each time. UI-thread-local —
    /// the live-paint thunk only ever runs on the UI thread. A handful of entries
    /// is plenty (the visible/hovered rows), so a tiny bounded map suffices.
    static PARSE_CACHE: RefCell<HashMap<u64, Rc<Canvas>>> = RefCell::new(HashMap::new());
}

/// Soft cap on parsed-canvas cache entries; clear past it so a long session
/// scrolling many canvases can't grow the cache without bound.
const PARSE_CACHE_CAP: usize = 32;

/// Parse `bytes` into a `Canvas`, memoized by `hash` on the UI thread. `None`
/// on a JSON parse failure.
fn parse_cached(hash: u64, bytes: &str) -> Option<Rc<Canvas>> {
    PARSE_CACHE.with(|c| {
        if let Some(hit) = c.borrow().get(&hash) {
            return Some(Rc::clone(hit));
        }
        let canvas = Rc::new(Canvas::from_json(bytes).ok()?);
        let mut map = c.borrow_mut();
        if map.len() >= PARSE_CACHE_CAP {
            map.clear();
        }
        map.insert(hash, Rc::clone(&canvas));
        Some(canvas)
    })
}

/// Emit a flat SVG of the canvas at a `px`-longest-edge viewBox, mapping world
/// coordinates through the zoom-to-fit transform over `bounds`.
///
/// Drawn to RESEMBLE the live renderer's LOD path (`canvas-lod-placeholder`),
/// which is exactly what the real `show_static` paint shows at thumbnail/fit
/// zoom — so the tiny cached thumbnail and the live expanded preview read the
/// same: translucent GROUP rectangles with a header label band paint first
/// (lowest z), then curved edge connectors with small arrowheads, then rounded
/// node CARDS each with a title line + 2–3 decreasing skeleton bars in the
/// node's color. status: preview-canvas-thumbnail
fn canvas_svg(canvas: &Canvas, bounds: Rect, px: u32) -> String {
    let px = px.max(1) as f64;
    // Letterbox the content bounds into a px×px square, preserving aspect.
    let span_x = bounds.width.max(1.0);
    let span_y = bounds.height.max(1.0);
    let scale = (px / span_x).min(px / span_y);
    let out_w = (span_x * scale).round().max(1.0);
    let out_h = (span_y * scale).round().max(1.0);
    let map = |x: f64, y: f64| ((x - bounds.x) * scale, (y - bounds.y) * scale);

    let mut svg = String::new();
    svg.push_str(&format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{out_w}" height="{out_h}" viewBox="0 0 {out_w} {out_h}">"#,
    ));
    // Transparent background so the thumbnail composites onto the row surface.

    // Groups first (lowest z): a tinted body + a stronger header band + label,
    // mirroring the live `group_backgrounds` paint.
    for node in &canvas.nodes {
        if let NodeKind::Group { label, .. } = &node.kind {
            push_group(&mut svg, node, label.as_deref(), &map, scale);
        }
    }

    // Edges next (under the cards): a gentle cubic curve with a small arrowhead,
    // matching the live edge routing's curved look.
    for edge in &canvas.edges {
        if let Some((a, b)) = edge_endpoints(canvas, edge) {
            push_edge(&mut svg, map(a.0, a.1), map(b.0, b.1));
        }
    }

    // Cards on top: rounded rect + title line + skeleton bars (the LOD body).
    for node in &canvas.nodes {
        if !matches!(node.kind, NodeKind::Group { .. }) {
            push_card(&mut svg, node, &map, scale);
        }
    }

    svg.push_str("</svg>");
    svg
}

/// Append a group rectangle: a faint body tint, a stronger header band along the
/// top with the label, and a frame border. Mirrors the live `paint_group_frame`.
fn push_group(svg: &mut String, node: &Node, label: Option<&str>, map: &impl Fn(f64, f64) -> (f64, f64), scale: f64) {
    let nb = node_bounds(node);
    let (x, y) = map(nb.x, nb.y);
    let w = (nb.width * scale).max(1.0);
    let h = (nb.height * scale).max(1.0);
    let color = node_color(node);
    let radius = (w.min(h) * 0.06).clamp(0.0, 4.0);
    // Body tint (subtle) + frame border.
    svg.push_str(&format!(
        r##"<rect x="{x:.1}" y="{y:.1}" width="{w:.1}" height="{h:.1}" rx="{radius:.1}" fill="{color}" fill-opacity="0.07" stroke="{color}" stroke-width="1"/>"##,
    ));
    // Header band along the top edge (the grab strip), a stronger tint.
    let band_h = (h * 0.18).clamp(1.0, 10.0);
    svg.push_str(&format!(
        r##"<rect x="{x:.1}" y="{y:.1}" width="{w:.1}" height="{band_h:.1}" rx="{radius:.1}" fill="{color}" fill-opacity="0.22"/>"##,
    ));
    // Label tick inside the band, when there's room and a label.
    if label.is_some_and(|l| !l.trim().is_empty()) && w > 12.0 {
        let ly = y + band_h * 0.5;
        let lw = (w * 0.4).min(w - 6.0).max(1.0);
        svg.push_str(&format!(
            r##"<rect x="{lx:.1}" y="{ly:.1}" width="{lw:.1}" height="1.4" rx="0.7" fill="{color}" fill-opacity="0.85"/>"##,
            lx = x + 3.0,
            ly = ly - 0.7,
        ));
    }
}

/// Append a node card: a rounded rect (translucent fill + colored border), a
/// title line near the top, then 2–3 decreasing skeleton bars — the same shapes
/// the live LOD placeholder paints. Mirrors `paint_lod_placeholder`.
fn push_card(svg: &mut String, node: &Node, map: &impl Fn(f64, f64) -> (f64, f64), scale: f64) {
    let nb = node_bounds(node);
    let (x, y) = map(nb.x, nb.y);
    let w = (nb.width * scale).max(1.0);
    let h = (nb.height * scale).max(1.0);
    let color = node_color(node);
    let radius = (w.min(h) * 0.12).clamp(0.0, 4.0);
    svg.push_str(&format!(
        r##"<rect x="{x:.1}" y="{y:.1}" width="{w:.1}" height="{h:.1}" rx="{radius:.1}" fill="{color}" fill-opacity="0.14" stroke="{color}" stroke-width="1"/>"##,
    ));
    // Skeleton body needs a little room; below that, the rect alone reads.
    let pad = (w.min(h) * 0.12).clamp(1.0, 4.0);
    let inner_x = x + pad;
    let inner_w = (w - pad * 2.0).max(1.0);
    let inner_top = y + pad;
    let inner_bottom = y + h - pad;
    if inner_w < 4.0 || inner_bottom - inner_top < 4.0 {
        return;
    }
    // Title line (a touch taller / stronger), then decreasing skeleton bars.
    let title_h = ((inner_bottom - inner_top) * 0.22).clamp(1.2, 4.0);
    svg.push_str(&format!(
        r##"<rect x="{inner_x:.1}" y="{inner_top:.1}" width="{tw:.1}" height="{title_h:.1}" rx="1" fill="{color}" fill-opacity="0.7"/>"##,
        tw = inner_w * 0.7,
    ));
    let bar_h = (title_h * 0.5).max(0.8);
    let gap = bar_h * 1.6;
    let mut by = inner_top + title_h + gap;
    for frac in [0.95_f32, 0.8, 0.55] {
        if by + bar_h > inner_bottom {
            break;
        }
        let bw = inner_w * f64::from(frac);
        svg.push_str(&format!(
            r##"<rect x="{inner_x:.1}" y="{by:.1}" width="{bw:.1}" height="{bar_h:.1}" rx="0.5" fill="{color}" fill-opacity="0.3"/>"##,
        ));
        by += bar_h + gap;
    }
}

/// Append one edge as a gentle cubic curve from `a` to `b` with a small
/// arrowhead at `b`, resembling the live curved connectors. Control points pull
/// the curve out perpendicular-ish to give it the same soft bow.
fn push_edge(svg: &mut String, a: (f64, f64), b: (f64, f64)) {
    let (x1, y1) = a;
    let (x2, y2) = b;
    let dx = x2 - x1;
    let dy = y2 - y1;
    let len = (dx * dx + dy * dy).sqrt().max(1.0);
    // Bow factor ~ a quarter of the run, like the live bezier's gentle arc.
    let bow = (len * 0.25).min(24.0);
    let cx1 = x1 + dx * 0.33;
    let cy1 = y1 + dy * 0.33;
    let cx2 = x2 - dx * 0.33;
    let cy2 = y2 - dy * 0.33;
    svg.push_str(&format!(
        r##"<path d="M{x1:.1} {y1:.1} C{cx1:.1} {cy1:.1} {cx2:.1} {cy2:.1} {x2:.1} {y2:.1}" fill="none" stroke="#888" stroke-width="1"/>"##,
    ));
    // Arrowhead: two short strokes back from the tip along the incoming
    // direction, splayed by a fixed angle.
    let ux = dx / len;
    let uy = dy / len;
    let head = bow.clamp(3.0, 7.0);
    let (ax, ay) = (x2, y2);
    // Rotate the back-vector by ±25°.
    let (c, s) = (0.906_f64, 0.423_f64);
    let b1 = (ax - head * (ux * c - uy * s), ay - head * (uy * c + ux * s));
    let b2 = (ax - head * (ux * c + uy * s), ay - head * (uy * c - ux * s));
    svg.push_str(&format!(
        r##"<path d="M{b1x:.1} {b1y:.1} L{ax:.1} {ay:.1} L{b2x:.1} {b2y:.1}" fill="none" stroke="#888" stroke-width="1"/>"##,
        b1x = b1.0, b1y = b1.1, b2x = b2.0, b2y = b2.1,
    ));
}

/// The endpoint pair (world coords) for an edge: the anchor on each node's side
/// (or the node center when the side is unspecified). `None` when either node id
/// doesn't resolve.
fn edge_endpoints(canvas: &Canvas, edge: &Edge) -> Option<((f64, f64), (f64, f64))> {
    let from = canvas.nodes.iter().find(|n| n.id == edge.from_node)?;
    let to = canvas.nodes.iter().find(|n| n.id == edge.to_node)?;
    let a = match edge.from_side {
        Some(side) => point_xy(node_anchor(from, side)),
        None => center(from),
    };
    let b = match edge.to_side {
        Some(side) => point_xy(node_anchor(to, side)),
        None => center(to),
    };
    Some((a, b))
}

const fn point_xy(p: hiker_canvas::geometry::Point) -> (f64, f64) {
    (p.x, p.y)
}

const fn center(node: &Node) -> (f64, f64) {
    let c = node_bounds(node).center();
    (c.x, c.y)
}

/// The six JSON Canvas preset hues (red, orange, yellow, green, cyan, purple),
/// as `#RRGGBB` for the SVG. Kept here (rather than reaching into the egui-
/// coupled `canvas_view::palette`) so this stays a pure, egui-free emitter. The
/// dark-mode preset table is used — thumbnails read on the app's dark surfaces.
const PRESET_HEX: [&str; 6] = ["#ff7b72", "#ffa657", "#e3c54a", "#6cc674", "#56c2d6", "#c48bf0"];

/// Pick a node's stroke color: its preset / hex if set, else a kind-derived
/// neutral so file / text / link / group nodes still read distinctly.
fn node_color(node: &Node) -> String {
    match &node.color {
        Some(Color::Preset(slot)) => PRESET_HEX[(slot.clamp(&1, &6) - 1) as usize].to_string(),
        Some(Color::Hex(hex)) => hex.clone(),
        None => kind_neutral(&node.kind).to_string(),
    }
}

/// Neutral color for an uncolored node, by kind, so the shape still carries a
/// hint of node type at a glance.
const fn kind_neutral(kind: &NodeKind) -> &'static str {
    match kind {
        NodeKind::Text { .. } => "#9aa0a6",
        NodeKind::File { .. } => "#7da3c4",
        NodeKind::Link { .. } => "#6cc674",
        NodeKind::Group { .. } => "#b0b6bd",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "nodes": [
            {"id":"a","x":0,"y":0,"width":100,"height":60,"type":"text","text":"hi","color":"4"},
            {"id":"b","x":200,"y":120,"width":80,"height":40,"type":"file","file":"x.md"}
        ],
        "edges": [
            {"id":"e1","fromNode":"a","toNode":"b"}
        ]
    }"#;

    #[test]
    fn renders_non_empty_image() {
        let t = CanvasPreview::new(SAMPLE.to_string());
        let img = t.render(64).expect("renders");
        assert!(img.width() > 0 && img.height() > 0);
        // Aspect: content is wider-than-tall-ish, longest edge ≈ 64.
        assert!(img.width().max(img.height()) <= 64);
    }

    #[test]
    fn empty_canvas_renders_none() {
        let t = CanvasPreview::new(r#"{"nodes":[],"edges":[]}"#.to_string());
        assert!(t.render(64).is_none(), "no nodes → no content bounds → None");
    }

    #[test]
    fn content_hash_changes_with_bytes() {
        let a = CanvasPreview::new(SAMPLE.to_string()).cache_key().content_hash;
        let b = CanvasPreview::new(SAMPLE.replace("hi", "yo")).cache_key().content_hash;
        assert_ne!(a, b, "an edit must invalidate the cache");
    }

    #[test]
    fn svg_includes_a_card_and_a_curved_edge() {
        let canvas = Canvas::from_json(SAMPLE).unwrap();
        let bounds = content_bounds(&canvas).unwrap();
        let svg = canvas_svg(&canvas, bounds, 64);
        assert!(svg.contains("<rect"), "nodes drawn as rounded card rects");
        // Edges are now curved connectors (cubic paths), not straight lines.
        assert!(svg.contains("<path"), "edges drawn as curved paths");
        assert!(!svg.contains("<line"), "no straight-line edges");
    }

    #[test]
    fn svg_renders_group_header_band() {
        // A group should emit its body tint + a header band + the card on top.
        let json = r#"{
            "nodes": [
                {"id":"g","x":0,"y":0,"width":300,"height":200,"type":"group","label":"G"},
                {"id":"a","x":20,"y":40,"width":100,"height":60,"type":"text","text":"hi"}
            ],
            "edges": []
        }"#;
        let canvas = Canvas::from_json(json).unwrap();
        let bounds = content_bounds(&canvas).unwrap();
        let svg = canvas_svg(&canvas, bounds, 128);
        // Two rects for the group (body + header band) + at least one for the card.
        let rects = svg.matches("<rect").count();
        assert!(rects >= 3, "group body + header band + card rects, got {rects}");
        // The label tick should appear (a thin filled bar inside the band).
        assert!(svg.contains("fill-opacity=\"0.85\""), "group label tick painted");
    }
}
