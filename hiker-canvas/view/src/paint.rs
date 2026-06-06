//! Painting nodes, edges, group backgrounds, selection handles, and the grid
//! background into an [`egui::Painter`], all clipped to the visible viewport so
//! a large canvas pays only for what's on screen.
//
// status: canvas-node-frame
// status: canvas-grid-background
// status: canvas-viewport-cull

use egui::epaint::CubicBezierShape;
use egui::{Color32, CornerRadius, FontId, Pos2, Rect, Stroke, StrokeKind, Vec2, Visuals};
use hiker_canvas::color::Color;
use hiker_canvas::geometry::{node_bounds, Point};
use hiker_canvas::model::{Canvas, Edge, Node, NodeKind, Side};

use canvas_view_core::camera::Camera;
use canvas_view_core::edges::{anchor_pos, arrowhead, build_geometry, resolve_sides, EdgeGeometry};
use canvas_view_core::handles::{grown_about_center, handle_rects, Handle, ALL_HANDLES, HANDLE_SIZE, HOVER_GROW};
use canvas_view_core::interaction::{connector_handle_center, GROUP_HEADER_H};

use crate::content::{CardView, NodeContentRenderer};
use crate::palette::{resolve_edge, resolve_node};

/// The card corner radius in canvas units (scaled by zoom at paint time).
const CARD_RADIUS: f32 = 6.0;
/// Inner padding from card border to content, in canvas units.
const CARD_PAD: f32 = 8.0;
/// Below this on-screen size (px), a card is too small to read its body, so the
/// content engine is skipped and a cheap title-block placeholder is painted
/// instead. Tuned with `tools/profile-canvas`: at zoom-to-fit a 300x200 card is
/// ~41x28 px (well below readable), while a card large enough to read text in
/// stays above the threshold and keeps full content.
const LOD_MIN_PX: f32 = 150.0;

/// Whether a screen rect intersects the viewport (viewport culling).
fn visible(viewport: Rect, r: Rect) -> bool {
    viewport.intersects(r)
}

/// Whether `screen` is too small to render readable content: a card narrower
/// than [`LOD_MIN_PX`] *or* shorter than `LOD_MIN_PX * 0.64` paints a cheap
/// placeholder instead of its full body. The height bound is looser than the
/// width bound because cards are wider than tall and a one-line title needs
/// little vertical room. Pure (no painter), so it is unit-testable. Crate-visible
/// so `widget`'s wheel routing can pass scroll through to camera zoom over a LOD
/// placeholder (which has no scrollable content). status: canvas-lod-placeholder
pub(crate) fn is_tiny(screen: Rect) -> bool {
    screen.width() < LOD_MIN_PX || screen.height() < LOD_MIN_PX * 0.64
}

/// A cheap one-line title for a node, derived in the view layer without file IO:
/// a [`NodeKind::File`] yields its basename (no directory, no `.md`), a
/// [`NodeKind::Text`] its first non-empty line, a [`NodeKind::Link`] its host
/// (falling back to the whole url). Groups never reach the placeholder path.
fn lod_title(node: &Node) -> &str {
    match &node.kind {
        NodeKind::File { file, .. } => file_basename(file),
        NodeKind::Text { text } => first_nonempty_line(text),
        NodeKind::Link { url } => url_host(url),
        NodeKind::Group { .. } => "",
    }
}

/// The basename of a vault path with any `.md` extension stripped.
fn file_basename(file: &str) -> &str {
    let base = file.rsplit(['/', '\\']).next().unwrap_or(file);
    base.strip_suffix(".md").unwrap_or(base)
}

/// The first line of `text` that is not blank (after trimming), or `""`.
fn first_nonempty_line(text: &str) -> &str {
    text.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or("")
}

/// The host of a url (the authority between `://` and the next `/`), or the
/// whole url when it has no scheme separator.
fn url_host(url: &str) -> &str {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    after_scheme.split(['/', '?', '#']).next().unwrap_or(after_scheme)
}

/// Paint the optional dotted grid background, spaced at `step` canvas units and
/// scaled with zoom. Skipped when the on-screen spacing is too dense to read.
pub fn grid(painter: &egui::Painter, viewport: Rect, camera: &Camera, step: f32, dark: bool) {
    let spacing = step * camera.scale();
    if spacing < 6.0 {
        return;
    }
    let dot = if dark { Color32::from_gray(60) } else { Color32::from_gray(205) };
    let origin = camera.world_to_screen(viewport, Point::new(0.0, 0.0));
    let start_x = origin.x - (((origin.x - viewport.left()) / spacing).ceil()) * spacing;
    let start_y = origin.y - (((origin.y - viewport.top()) / spacing).ceil()) * spacing;
    let mut y = start_y;
    while y < viewport.bottom() {
        let mut x = start_x;
        while x < viewport.right() {
            painter.circle_filled(Pos2::new(x, y), 1.0, dot);
            x += spacing;
        }
        y += spacing;
    }
}

/// Paint every group node's translucent background behind member content. Groups
/// paint first (lowest z) so members read on top. `header_hover` is the id of the
/// group whose grab strip the pointer is over (if any); that group's header band
/// paints brighter so its draggability reads. status: canvas-handle-hover
pub fn group_backgrounds(
    painter: &egui::Painter,
    viewport: Rect,
    camera: &Camera,
    canvas: &Canvas,
    visuals: &Visuals,
    header_hover: Option<&str>,
) {
    for node in &canvas.nodes {
        let NodeKind::Group { label, .. } = &node.kind else { continue };
        let screen = camera.world_rect_to_screen(viewport, node_bounds(node));
        if !visible(viewport, screen) {
            continue;
        }
        let hovered = header_hover == Some(node.id.as_str());
        paint_group_frame(painter, camera, visuals, node, label.as_deref(), screen, hovered);
    }
}

/// The on-screen point size for a group label, or `None` to drop it.
///
/// Scales with the camera (a readable `12.0` at scale 1) but never grows past the
/// on-screen header band `header_h`: at fit / LOD zoom a tiny group must NOT paint
/// an 8px label over a few-pixel card (the `8.0` floor is for readable interactive
/// zoom, not for a group shrunk to a sliver). Below a readable size, or when the
/// group is too narrow to show even a glyph or two, returns `None` — the same way
/// a node card falls back to an LOD placeholder instead of unreadable text.
fn group_label_size(scale: f32, header_h: f32, screen_w: f32) -> Option<f32> {
    let size = (12.0 * scale).clamp(8.0, 18.0).min(header_h);
    (size >= 6.0 && screen_w >= size * 2.0).then_some(size)
}

/// Paint one group's body tint, its header band (the grab strip), the frame
/// border, and the label. Split out of the loop so the visibility styling lives
/// in one place and the loop stays small. `header_hover` brightens the grab strip
/// when the pointer is over it. status: canvas-handle-hover
fn paint_group_frame(
    painter: &egui::Painter,
    camera: &Camera,
    visuals: &Visuals,
    node: &Node,
    label: Option<&str>,
    screen: Rect,
    header_hover: bool,
) {
    let frame = group_frame_color(node.color.as_ref(), visuals);
    let radius = card_radius(camera);
    // Body tint stays subtle so framed members read on top.
    painter.rect_filled(screen, radius, frame.gamma_multiply(0.07));
    // Header band along the top edge (the `canvas-group-grab` strip): a stronger
    // tint with top-only rounded corners, so the container and its grab handle
    // are clearly visible. On hover the band brightens (a higher gamma factor)
    // so the grab affordance reads before pressing. status: canvas-handle-hover
    // Clamp the header band to the group's on-screen height: when a group is
    // zoomed out below the band's natural height it shrinks with the group
    // rather than overflowing (and a hard min above the height would make
    // `clamp` panic with min > max).
    let max_h = screen.height().max(0.0);
    let header_h = (GROUP_HEADER_H as f32 * camera.scale()).clamp(0.0, max_h);
    let header = Rect::from_min_size(screen.min, Vec2::new(screen.width(), header_h));
    let r = (CARD_RADIUS * camera.scale()).clamp(0.0, 18.0) as u8;
    let top = CornerRadius { nw: r, ne: r, sw: 0, se: 0 };
    let band = if header_hover { 0.4 } else { 0.22 };
    painter.rect_filled(header, top, frame.gamma_multiply(band));
    painter.line_segment([header.left_bottom(), header.right_bottom()], Stroke::new(1.0, frame.gamma_multiply(0.6)));
    // Frame border on top of the fills.
    painter.rect_stroke(screen, radius, Stroke::new(1.5, frame), StrokeKind::Inside);
    if let Some(text) = label {
        if let Some(size) = group_label_size(camera.scale(), header_h, screen.width()) {
            let pos = header.left_center() + Vec2::new(6.0, 0.0);
            painter.text(pos, egui::Align2::LEFT_CENTER, text, FontId::proportional(size), group_label_color(node.color.as_ref(), visuals));
        }
    }
}

/// A group's frame color: its accent when colored, else a readable neutral that
/// contrasts with the canvas background in either theme (the old faded stroke
/// was nearly invisible for the common uncolored container).
fn group_frame_color(color: Option<&Color>, visuals: &Visuals) -> Color32 {
    if color.is_some() {
        resolve_node(color, visuals).stroke
    } else if visuals.dark_mode {
        Color32::from_gray(120)
    } else {
        Color32::from_gray(150)
    }
}

/// A group label's color: the accent for colored groups, else the theme's
/// strong text color so the title reads clearly on the header band.
fn group_label_color(color: Option<&Color>, visuals: &Visuals) -> Color32 {
    if color.is_some() {
        resolve_node(color, visuals).stroke
    } else {
        visuals.strong_text_color()
    }
}

/// Paint a single non-group node's card frame and delegate its content. Returns
/// the *effective* (clamped) vertical scroll the content settled on, so the view
/// can store it back as the card's scroll state (a no-op echo for non-scrolling
/// content). A culled or group node echoes the incoming scroll unchanged.
pub fn node_card(
    ui: &mut egui::Ui,
    viewport: Rect,
    camera: &Camera,
    node: &Node,
    content: &mut dyn NodeContentRenderer,
    view: CardView,
) -> f32 {
    if matches!(node.kind, NodeKind::Group { .. }) {
        return view.scroll_y;
    }
    let screen = camera.world_rect_to_screen(viewport, node_bounds(node));
    if !visible(viewport, screen) {
        return view.scroll_y;
    }
    let visuals = ui.visuals().clone();
    let resolved = resolve_node(node.color.as_ref(), &visuals);
    let radius = card_radius(camera);
    let painter = ui.painter().with_clip_rect(viewport);
    painter.rect_filled(screen, radius, resolved.fill);
    painter.rect_stroke(screen, radius, Stroke::new(1.5, resolved.stroke), StrokeKind::Inside);
    // status: canvas-lod-placeholder
    // When the card is too small to read, skip the (expensive) content engine
    // and paint a title-block placeholder, echoing the incoming scroll.
    if is_tiny(screen) {
        let pad = (CARD_PAD * camera.scale()).max(2.0);
        paint_lod_placeholder(&painter, screen.shrink(pad), lod_title(node), &visuals);
        return view.scroll_y;
    }
    let pad = CARD_PAD * camera.scale();
    let inner = screen.shrink(pad);
    // Lay content out at the full `inner` rect but hand it a child ui clipped to
    // the viewport, so a card straddling the pane edge never paints its body
    // outside the canvas (over the header / tabs / neighbouring panels). The
    // content engine intersects its own clip with this one and returns the
    // scroll it actually used (clamped to content height).
    let clip = inner.intersect(viewport);
    if clip.width() > 1.0 && clip.height() > 1.0 {
        let mut clipped = ui.new_child(egui::UiBuilder::new().max_rect(inner));
        clipped.set_clip_rect(clip);
        content.render(&mut clipped, node, inner, view)
    } else {
        view.scroll_y
    }
}

/// Paint the level-of-detail placeholder inside `inner`: a single clipped title
/// line near the top sized to the card, then a few faint skeleton bars in a
/// muted tint to suggest body text. A handful of paint calls — the whole point
/// of the LOD path is that this is cheap regardless of the node's real content.
fn paint_lod_placeholder(painter: &egui::Painter, inner: Rect, title: &str, visuals: &Visuals) {
    if inner.width() < 2.0 || inner.height() < 2.0 {
        return;
    }
    let painter = painter.with_clip_rect(inner);
    // Title sized to the card height, but never so large it dwarfs a short card.
    let title_h = (inner.height() * 0.22).clamp(6.0, 13.0);
    if !title.is_empty() {
        painter.text(inner.left_top(), egui::Align2::LEFT_TOP, title, FontId::proportional(title_h), visuals.strong_text_color());
    }
    let bar_color = visuals.weak_text_color().gamma_multiply(0.35);
    let bar_h = (title_h * 0.45).max(2.0);
    let gap = bar_h * 1.6;
    let mut y = inner.top() + title_h + gap;
    // 2-3 skeleton bars of decreasing width, stopping when we run out of room.
    for frac in [0.95_f32, 0.8, 0.55] {
        if y + bar_h > inner.bottom() {
            break;
        }
        let bar = Rect::from_min_size(Pos2::new(inner.left(), y), Vec2::new(inner.width() * frac, bar_h));
        painter.rect_filled(bar, CornerRadius::same(1), bar_color);
        y += bar_h + gap;
    }
}

/// Paint the four connector handles — small circles at the side anchors — the
/// visible affordance to start an edge from a hovered (or selected) node.
/// `radius` is the screen-px hit/paint radius; `hot` is the side currently
/// under the pointer, drawn filled in `accent` and grown by [`HOVER_GROW`] (the
/// same gentle affordance the resize handles use) while the rest read subtle.
/// status: canvas-edge-draw, canvas-handle-hover
pub fn connector_handles(painter: &egui::Painter, screen: Rect, accent: Color32, radius: f32, hot: Option<Side>) {
    for side in [Side::Top, Side::Right, Side::Bottom, Side::Left] {
        let center = connector_handle_center(screen, side);
        let active = hot == Some(side);
        let fill = if active { accent } else { Color32::WHITE };
        let r = if active { radius * HOVER_GROW } else { radius };
        painter.circle_filled(center, r, fill);
        painter.circle_stroke(center, r, Stroke::new(1.5, accent));
    }
}

/// Paint the eight resize handles around a single selected node. The handle the
/// pointer is over (`hovered`) grows by [`HOVER_GROW`] about its own center, so
/// the grab target reads before pressing; the rest paint unchanged.
/// status: canvas-handle-hover
pub fn resize_handles(painter: &egui::Painter, screen: Rect, accent: Color32, hovered: Option<Handle>) {
    for (handle, base) in ALL_HANDLES.into_iter().zip(handle_rects(screen)) {
        let r = if hovered == Some(handle) { grown_about_center(base, HOVER_GROW) } else { base };
        painter.rect_filled(r, CornerRadius::same(2), Color32::WHITE);
        painter.rect_stroke(r, CornerRadius::same(2), Stroke::new(1.0, accent), StrokeKind::Outside);
    }
}

/// Paint a selection outline around a node or edge-selected card.
pub fn selection_outline(painter: &egui::Painter, screen: Rect, camera: &Camera, accent: Color32) {
    let radius = card_radius(camera) + 2u8;
    painter.rect_stroke(screen.expand(2.0), radius, Stroke::new(2.0, accent), StrokeKind::Outside);
}

/// Paint all visible edges. Dangling edges (an endpoint that resolves to no live
/// node) are skipped here; the host surfaces them as broken references.
pub fn edges(
    painter: &egui::Painter,
    viewport: Rect,
    camera: &Camera,
    canvas: &Canvas,
    visuals: &Visuals,
    selected: &dyn Fn(&str) -> bool,
) {
    for edge in &canvas.edges {
        one_edge(painter, viewport, camera, canvas, edge, visuals, selected(&edge.id));
    }
}

fn one_edge(
    painter: &egui::Painter,
    viewport: Rect,
    camera: &Camera,
    canvas: &Canvas,
    edge: &Edge,
    visuals: &Visuals,
    selected: bool,
) {
    let Some(from) = canvas.nodes.iter().find(|n| n.id == edge.from_node) else { return };
    let Some(to) = canvas.nodes.iter().find(|n| n.id == edge.to_node) else { return };
    let (from_side, to_side) = resolve_sides(edge, from, to);
    let start = world_anchor(camera, viewport, anchor_pos(from, from_side));
    let end = world_anchor(camera, viewport, anchor_pos(to, to_side));
    let bbox = Rect::from_two_pos(start, end).expand(40.0);
    if !visible(viewport, bbox) {
        return;
    }
    let handle = (start - end).length().clamp(40.0, 320.0) * 0.4;
    let geo = build_geometry(start, end, from_side, to_side, handle);
    let color = if selected {
        visuals.selection.stroke.color
    } else {
        resolve_edge(edge.color.as_ref(), visuals)
    };
    let width = if selected { 2.5 } else { 1.5 };
    let curve = CubicBezierShape::from_points_stroke(
        [geo.start, geo.ctrl_a, geo.ctrl_b, geo.end],
        false,
        Color32::TRANSPARENT,
        Stroke::new(width, color),
    );
    painter.add(curve);
    edge_caps(painter, edge, &geo, color, camera.scale());
    if let Some(label) = &edge.label {
        let mid = bezier_midpoint(geo.start, geo.ctrl_a, geo.ctrl_b, geo.end);
        let size = (11.0 * camera.scale()).clamp(8.0, 16.0);
        painter.text(mid, egui::Align2::CENTER_CENTER, label, FontId::proportional(size), color);
    }
}

/// Paint arrowheads at whichever ends carry an `arrow` cap (`to_end` defaults to
/// arrow when absent; `from_end` defaults to none). The head shrinks with the
/// camera so it stays proportionate to the (tiny) cards and short edges at fit /
/// LOD zoom instead of a full-size 12px head dwarfing them; capped at its natural
/// size and floored so it never disappears.
fn edge_caps(painter: &egui::Painter, edge: &Edge, geo: &EdgeGeometry, color: Color32, scale: f32) {
    use hiker_canvas::model::EndCap;
    let (len, half_w) = arrowhead_size(scale);
    if matches!(edge.to_end, None | Some(EndCap::Arrow)) {
        let tri = arrowhead(geo.end, geo.end - geo.ctrl_b, len, half_w);
        painter.add(egui::Shape::convex_polygon(tri.to_vec(), color, Stroke::NONE));
    }
    if matches!(edge.from_end, Some(EndCap::Arrow)) {
        let tri = arrowhead(geo.start, geo.start - geo.ctrl_a, len, half_w);
        painter.add(egui::Shape::convex_polygon(tri.to_vec(), color, Stroke::NONE));
    }
}

/// Arrowhead `(length, half-width)` in screen px for the current camera `scale`.
/// Natural size is 12×6 at scale 1; it scales down when zoomed out (so it stays
/// proportionate to the shrunken edges) but never past a small floor that keeps
/// it legible, and never grows beyond the natural size when zoomed in.
fn arrowhead_size(scale: f32) -> (f32, f32) {
    let len = (12.0 * scale).clamp(5.0, 12.0);
    (len, len * 0.5)
}

/// Evaluate a cubic Bézier at t = 0.5 for label placement.
fn bezier_midpoint(p0: Pos2, p1: Pos2, p2: Pos2, p3: Pos2) -> Pos2 {
    let t = 0.5;
    let mt = 1.0 - t;
    let a = mt * mt * mt;
    let b = 3.0 * mt * mt * t;
    let c = 3.0 * mt * t * t;
    let d = t * t * t;
    Pos2::new(
        a * p0.x + b * p1.x + c * p2.x + d * p3.x,
        a * p0.y + b * p1.y + c * p2.y + d * p3.y,
    )
}

fn world_anchor(camera: &Camera, viewport: Rect, p: Pos2) -> Pos2 {
    camera.world_to_screen(viewport, Point::new(f64::from(p.x), f64::from(p.y)))
}

fn card_radius(camera: &Camera) -> CornerRadius {
    CornerRadius::same((CARD_RADIUS * camera.scale()).clamp(0.0, 18.0) as u8)
}

/// Re-export the handle size constant for the interaction layer.
#[must_use]
pub const fn handle_size() -> f32 {
    HANDLE_SIZE
}

#[cfg(test)]
mod lod_tests {
    use super::{arrowhead_size, file_basename, first_nonempty_line, group_label_size, is_tiny, lod_title, url_host, LOD_MIN_PX};
    use egui::{Pos2, Rect, Vec2};
    use hiker_canvas::model::{Node, NodeKind};
    use std::collections::BTreeMap;

    fn rect(w: f32, h: f32) -> Rect {
        Rect::from_min_size(Pos2::ZERO, Vec2::new(w, h))
    }

    #[test]
    fn tiny_when_either_dimension_below_threshold() {
        // A 300x200 card at zoom-to-fit (~0.14 scale) lands near 41x28 px: tiny.
        assert!(is_tiny(rect(41.0, 28.0)));
        // Narrow but tall, or wide but short, both count as tiny.
        assert!(is_tiny(rect(LOD_MIN_PX - 1.0, 500.0)));
        assert!(is_tiny(rect(500.0, LOD_MIN_PX * 0.64 - 1.0)));
    }

    #[test]
    fn not_tiny_when_readable() {
        // A card comfortably above both bounds renders full content.
        assert!(!is_tiny(rect(300.0, 200.0)));
        assert!(!is_tiny(rect(LOD_MIN_PX, LOD_MIN_PX)));
    }

    #[test]
    fn group_label_drops_when_group_is_tiny() {
        // Interactive zoom: 28px band, wide group → readable 12px label.
        assert_eq!(group_label_size(1.0, 28.0, 300.0), Some(12.0));
        // Zoomed out so the band is a sliver: label never exceeds the band, and
        // below 6px it's dropped (no 8px text over a 3px card).
        assert_eq!(group_label_size(0.02, 0.6, 200.0), None);
        // The label can shrink with the band rather than being forced to the 8px
        // floor — so it can't dwarf the card.
        assert_eq!(group_label_size(0.5, 7.0, 200.0), Some(7.0));
        // Too narrow to show a glyph or two → dropped even if tall enough.
        assert_eq!(group_label_size(1.0, 28.0, 10.0), None);
    }

    #[test]
    fn arrowhead_shrinks_when_zoomed_out_but_keeps_aspect() {
        // Interactive zoom: natural 12x6 head.
        assert_eq!(arrowhead_size(1.0), (12.0, 6.0));
        // Zoomed in past 1: capped at the natural size, not larger.
        assert_eq!(arrowhead_size(4.0), (12.0, 6.0));
        // Fit / LOD zoom: shrinks toward the floor so it doesn't dwarf tiny cards.
        let (len, half) = arrowhead_size(0.05);
        assert_eq!(len, 5.0, "floored, not the 12px full-size head over a tiny edge");
        assert!((half - len * 0.5).abs() < 1e-6, "half-width tracks length");
    }

    #[test]
    fn file_basename_strips_dir_and_md() {
        assert_eq!(file_basename("notes/sub/My Note.md"), "My Note");
        assert_eq!(file_basename("image.png"), "image.png");
        assert_eq!(file_basename("flat.md"), "flat");
    }

    #[test]
    fn text_first_nonempty_line() {
        assert_eq!(first_nonempty_line("\n\n  Hello world  \nmore"), "Hello world");
        assert_eq!(first_nonempty_line("   \n\t"), "");
    }

    #[test]
    fn url_host_extracts_authority() {
        assert_eq!(url_host("https://example.com/a/b?x=1"), "example.com");
        assert_eq!(url_host("ftp://host.test"), "host.test");
        assert_eq!(url_host("bare-no-scheme"), "bare-no-scheme");
    }

    #[test]
    fn lod_title_dispatches_on_kind() {
        let mk = |kind| Node { id: "n".into(), x: 0, y: 0, width: 10, height: 10, color: None, kind, extra: BTreeMap::new() };
        assert_eq!(lod_title(&mk(NodeKind::File { file: "d/x.md".into(), subpath: None })), "x");
        assert_eq!(lod_title(&mk(NodeKind::Text { text: "First\nSecond".into() })), "First");
        assert_eq!(lod_title(&mk(NodeKind::Link { url: "https://a.io/p".into() })), "a.io");
    }
}
