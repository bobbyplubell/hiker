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
use hiker_projection::{
    clamp_inside_disk, geodesic_circle, Complex, ProjectionKind, DEFAULT_BOUNDARY_RADIUS,
};

use canvas_view_core::camera::{Camera, CardScaleClamp};
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

/// Below this lens magnification, a card under an active projection collapses to
/// the LOD placeholder regardless of its on-screen size — the magnification half
/// of the LOD ladder (`proj-lod-ladder`). Peripheral cards (low magnification)
/// become dots; central cards (magnification near 1) keep full content. Only
/// consulted when the lens is active, so the affine canvas is unaffected.
const LOD_MAG_THRESHOLD: f32 = 0.4;

/// The deepest LOD tier (`proj-lod-ladder`): below this on-screen size a card is
/// too small to read even a title, so it collapses to a BARE DOT — no frame, no
/// text — and a few hundred such cards read as a clean colored constellation
/// instead of a hairball of overlapping frames + titles ("text soup at scale").
/// Smaller than [`LOD_MIN_PX`] (the title-placeholder tier) so a mid-zoom card
/// still gets its title before disappearing to a point. The height bound is
/// looser (cards are wider than tall), mirroring [`is_tiny`].
const BARE_DOT_PX: f32 = 36.0;

/// Below this lens magnification a card under an active projection collapses all
/// the way to a bare dot regardless of its on-screen size — the dot half of the
/// magnification ladder, deeper than [`LOD_MAG_THRESHOLD`]. Only consulted when
/// the lens is active, so the affine canvas is unaffected.
const BARE_DOT_MAG: f32 = 0.2;

/// Whether `screen` is too small to read even a title — the bare-dot tier. A card
/// narrower than [`BARE_DOT_PX`] *or* shorter than `BARE_DOT_PX * 0.64` collapses
/// to a single colored dot. Pure (no painter), so it is unit-testable.
fn is_bare_dot(screen: Rect) -> bool {
    screen.width() < BARE_DOT_PX || screen.height() < BARE_DOT_PX * 0.64
}

/// Whether a screen rect intersects the viewport (viewport culling).
fn visible(viewport: Rect, r: Rect) -> bool {
    viewport.intersects(r)
}

/// Multiply a colour's alpha by `factor` (clamped to `[0, 1]`). A `factor` of
/// `1.0` returns the colour untouched, so the Off/Fisheye path (where the rim
/// fade is always `1.0`) is byte-identical. Drives the Poincaré rim fade.
fn fade(color: Color32, factor: f32) -> Color32 {
    if factor >= 1.0 {
        return color;
    }
    let a = (f32::from(color.a()) * factor.clamp(0.0, 1.0)).round() as u8;
    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), a)
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
    node_card_filled(ui, viewport, camera, node, content, view, None)
}

/// As [`node_card`], but with an optional neighbor-gap fill scale (from
/// [`fill_scales`]) replacing the card's lens magnification sizing — the
/// "auto-expand to fill the space" path the scene pre-pass drives under a lens.
/// `None` keeps the historical magnification sizing. [proj-card-fill]
pub fn node_card_filled(
    ui: &mut egui::Ui,
    viewport: Rect,
    camera: &Camera,
    node: &Node,
    content: &mut dyn NodeContentRenderer,
    view: CardView,
    fill_scale: Option<f32>,
) -> f32 {
    if matches!(node.kind, NodeKind::Group { .. }) {
        return view.scroll_y;
    }
    // Card-scale compromise (`proj-card-scale`): under a projection lens a card
    // MUST stay an axis-aligned rect (egui can't shear a glyph), so we don't map
    // both corners (that would distort). Instead map the card's world CENTER
    // through the lens-composed `world_to_screen`, then build the screen rect from
    // the card's base (affine-only) screen size times a clamped magnification
    // factor, centered on the projected center. Under Off the lens is the
    // identity, `card_scale == 1.0`, and center-mapping == corner-mapping, so this
    // is byte-identical to the historical affine path.
    let bounds = node_bounds(node);
    let screen = projected_card_rect(camera, viewport, bounds, fill_scale);
    if !visible(viewport, screen) {
        return view.scroll_y;
    }
    let visuals = ui.visuals().clone();
    let resolved = resolve_node(node.color.as_ref(), &visuals);
    let radius = card_radius(camera);
    let painter = ui.painter().with_clip_rect(viewport);
    // Poincaré rim fade: peripheral cards recede toward the disk boundary, so
    // their fill + border fade by the local magnification. `1.0` (no fade) under
    // Off/Fisheye, keeping those modes byte-identical. [proj-canvas-mode]
    let alpha = camera.rim_alpha_at(bounds.center());
    let mag = camera.magnification_at(bounds.center());
    // status: canvas-lod-placeholder, proj-lod-ladder
    // LOD ladder, decided BEFORE any frame is drawn so the deepest tier paints
    // nothing but a point:
    //   1. Bare dot — too small to read even a title (or, under a lens, below
    //      BARE_DOT_MAG magnification): paint ONLY a small filled circle in the
    //      vivid stroke colour, NO frame / title / content. At hundreds of cards
    //      this is what turns the soup into a clean colored constellation.
    //   2. Title placeholder — readable enough for a title but not its body
    //      (is_tiny, or below LOD_MAG_THRESHOLD under a lens): frame + cheap
    //      title-block skeleton.
    //   3. Full card — frame + the real content engine.
    if is_bare_dot(screen) || (camera.lens_active() && mag < BARE_DOT_MAG) {
        let r = (screen.size().min_elem() * 0.5).clamp(1.5, 4.0);
        painter.circle_filled(screen.center(), r, fade(resolved.stroke, alpha));
        return view.scroll_y;
    }
    painter.rect_filled(screen, radius, fade(resolved.fill, alpha));
    painter.rect_stroke(screen, radius, Stroke::new(1.5, fade(resolved.stroke, alpha)), StrokeKind::Inside);
    if is_tiny(screen) || (camera.lens_active() && mag < LOD_MAG_THRESHOLD) {
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

/// Stroke the Poincaré unit-disk boundary ring at the pane-LOCKED disk frame —
/// centre = viewport centre, radius = `poincare_disk_frame`'s radius — in a muted
/// divider colour. The frame is independent of `pan`/`scale`, so the ring stays
/// fixed to the pane (it IS the viewport edge of the disk) instead of drifting
/// with the affine view. A no-op unless the lens is the Poincaré kind AND the
/// boundary toggle is on — so Off/Fisheye never draw it. [proj-canvas-mode]
pub fn poincare_boundary(painter: &egui::Painter, viewport: Rect, camera: &Camera, visuals: &Visuals) {
    if camera.projection().kind != ProjectionKind::Poincare || !camera.show_boundary() {
        return;
    }
    let (center, radius) = camera.poincare_disk_frame(viewport);
    let color = visuals.weak_text_color();
    painter.circle_stroke(center, radius, Stroke::new(1.0, color));
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
    let from_anchor = anchor_pos(from, from_side);
    let to_anchor = anchor_pos(to, to_side);
    let start = world_anchor(camera, viewport, from_anchor);
    let end = world_anchor(camera, viewport, to_anchor);
    let bbox = Rect::from_two_pos(start, end).expand(40.0);
    if !visible(viewport, bbox) {
        return;
    }
    let base_color = if selected {
        visuals.selection.stroke.color
    } else {
        resolve_edge(edge.color.as_ref(), visuals)
    };
    let width = if selected { 2.5 } else { 1.5 };

    // Off / Affine: unchanged cubic-Bézier connector (no projection sampling, no
    // rim fade) — byte-identical to the historical path. [proj-canvas-mode]
    if !camera.lens_active() {
        let handle = (start - end).length().clamp(40.0, 320.0) * 0.4;
        let geo = build_geometry(start, end, from_side, to_side, handle);
        let curve = CubicBezierShape::from_points_stroke(
            [geo.start, geo.ctrl_a, geo.ctrl_b, geo.end],
            false,
            Color32::TRANSPARENT,
            Stroke::new(width, base_color),
        );
        painter.add(curve);
        edge_caps(painter, edge, &geo, base_color, camera.scale());
        if let Some(label) = &edge.label {
            let mid = bezier_midpoint(geo.start, geo.ctrl_a, geo.ctrl_b, geo.end);
            let size = (11.0 * camera.scale()).clamp(8.0, 16.0);
            painter.text(mid, egui::Align2::CENTER_CENTER, label, FontId::proportional(size), base_color);
        }
        return;
    }

    // Lensed: build a screen polyline that follows the projection.
    // - Poincaré: the geodesic between the two disk points, mapped back to
    //   lensed-world then through the AFFINE-only screen map (no second lens
    //   pass) so the curvature is exact — mirroring graph-view's `draw_edges`.
    // - Fisheye (and any non-Affine fallback): the straight world chord
    //   subdivided, each sample pushed through the full lens-composed map so the
    //   edge follows the bulge.
    let alpha = camera
        .rim_alpha_at(anchor_world(from_anchor))
        .min(camera.rim_alpha_at(anchor_world(to_anchor)));
    let color = fade(base_color, alpha);
    let pts = projected_edge_points(camera, viewport, from_anchor, to_anchor);
    painter.add(egui::Shape::line(pts.clone(), Stroke::new(width, color)));
    // Arrowheads at the projected ends, oriented along the first/last segment.
    projected_edge_caps(painter, edge, &pts, color, camera.scale());
    if let Some(label) = &edge.label {
        let mid = polyline_midpoint(&pts);
        let size = (11.0 * camera.scale()).clamp(8.0, 16.0);
        painter.text(mid, egui::Align2::CENTER_CENTER, label, FontId::proportional(size), color);
    }
}

/// Sample the projected edge between two world anchors into a screen polyline.
/// Poincaré samples the geodesic in disk space and maps each point back through
/// the affine-only screen map (avoiding double-lensing); every other active lens
/// (Fisheye) subdivides the world chord and pushes each sample through the full
/// lens-composed `world_to_screen`. Always returns at least the two endpoints.
fn projected_edge_points(camera: &Camera, viewport: Rect, from: Pos2, to: Pos2) -> Vec<Pos2> {
    let cfg = camera.projection();
    let segments = cfg.geodesic_segments.max(1);
    let a = anchor_world(from);
    let b = anchor_world(to);
    match cfg.kind {
        ProjectionKind::Poincare => {
            // The disk is LOCKED to the viewport: sample the geodesic in unit-disk
            // space, then map each sample straight onto the pane-fixed disk frame
            // (centre + z·radius) — NOT through the affine view, which the disk no
            // longer rides. Mirrors the graph view's `disk_to_screen`. The disk
            // points are clamped strictly inside the boundary so near/over-rim
            // anchors don't yield a degenerate arc, and the segment count adapts
            // to the arc's angular span so sharply-curved (high-strength) edges
            // get enough samples to read smooth instead of faceted. [proj-card-fill]
            let lens = camera.lens();
            let za = clamp_inside_disk(lens.disk_point(a), DEFAULT_BOUNDARY_RADIUS);
            let zb = clamp_inside_disk(lens.disk_point(b), DEFAULT_BOUNDARY_RADIUS);
            let effective = adaptive_segments(za, zb, segments);
            let (center, radius) = camera.poincare_disk_frame(viewport);
            hiker_projection::sample_geodesic(za, zb, effective)
                .into_iter()
                .map(|z| center + Vec2::new(z.re, z.im) * radius)
                .collect()
        }
        // Fisheye / any non-Affine fallback: subdivide the world chord and lens
        // each sample so the edge tracks the bulge.
        _ => (0..=segments)
            .map(|i| {
                let t = f64::from(i) / f64::from(segments);
                let p = Point::new(a.x + (b.x - a.x) * t, a.y + (b.y - a.y) * t);
                camera.world_to_screen(viewport, p)
            })
            .collect(),
    }
}

/// Reference angular span (radians) that earns the base segment count. A
/// geodesic arc spanning this much angle is sampled at `base`; sharper arcs scale
/// up linearly toward [`MAX_GEODESIC_SEGMENTS`]. [proj-card-fill]
const REF_GEODESIC_ANGLE: f32 = 0.5;
/// Upper bound on the adaptive geodesic segment count, so a near-rim arc can't
/// explode the polyline. [proj-card-fill]
const MAX_GEODESIC_SEGMENTS: u32 = 64;

/// Segment count for a Poincaré geodesic between two (in-disk) points, adapted to
/// the arc's angular span: a straight diameter (no geodesic circle) keeps the
/// `base`; a curved arc gets `base · span / REF_ANGLE`, ceilinged and clamped to
/// `[base, MAX]`. Sharper arcs (the high-strength, near-rim case) get more
/// samples so they read smooth instead of faceted. Pure — unit-tested without
/// egui. [proj-card-fill]
fn adaptive_segments(za: Complex, zb: Complex, base: u32) -> u32 {
    let base = base.max(1);
    let Some(circle) = geodesic_circle(za, zb) else {
        return base;
    };
    let span = (za - circle.center).arg() - (zb - circle.center).arg();
    let span = wrap_angle(span).abs();
    let scaled = (base as f32 * span / REF_GEODESIC_ANGLE).ceil();
    (scaled as u32).clamp(base, MAX_GEODESIC_SEGMENTS)
}

/// Wrap an angle into `(-π, π]` so the span of the minor arc is measured (the
/// same arc `sample_geodesic` walks).
fn wrap_angle(angle: f32) -> f32 {
    use std::f32::consts::PI;
    let mut value = angle;
    while value <= -PI {
        value += 2.0 * PI;
    }
    while value > PI {
        value -= 2.0 * PI;
    }
    value
}

/// Arrowheads for a projected (polyline) edge: drawn at whichever ends carry an
/// `arrow` cap, oriented along the polyline's final / first segment. Mirrors
/// [`edge_caps`] but reads the direction from the polyline instead of the Bézier
/// control points.
fn projected_edge_caps(painter: &egui::Painter, edge: &Edge, pts: &[Pos2], color: Color32, scale: f32) {
    use hiker_canvas::model::EndCap;
    let (Some(&first), Some(&last)) = (pts.first(), pts.last()) else { return };
    let (len, half_w) = arrowhead_size(scale);
    if matches!(edge.to_end, None | Some(EndCap::Arrow)) {
        let prev = pts.iter().rev().nth(1).copied().unwrap_or(first);
        let tri = arrowhead(last, last - prev, len, half_w);
        painter.add(egui::Shape::convex_polygon(tri.to_vec(), color, Stroke::NONE));
    }
    if matches!(edge.from_end, Some(EndCap::Arrow)) {
        let next = pts.get(1).copied().unwrap_or(last);
        let tri = arrowhead(first, first - next, len, half_w);
        painter.add(egui::Shape::convex_polygon(tri.to_vec(), color, Stroke::NONE));
    }
}

/// The midpoint of a screen polyline — the sample nearest the half-arc-length
/// point, for label placement. Falls back to the geometric mean of the endpoints
/// for a degenerate (zero/one-point) polyline.
fn polyline_midpoint(pts: &[Pos2]) -> Pos2 {
    if pts.len() < 2 {
        return pts.first().copied().unwrap_or(Pos2::ZERO);
    }
    let total: f32 = pts.windows(2).map(|w| (w[1] - w[0]).length()).sum();
    let half = total * 0.5;
    let mut acc = 0.0;
    for w in pts.windows(2) {
        let seg = (w[1] - w[0]).length();
        if acc + seg >= half {
            let t = if seg > f32::EPSILON { (half - acc) / seg } else { 0.0 };
            return w[0].lerp(w[1], t);
        }
        acc += seg;
    }
    pts[pts.len() / 2]
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
    camera.world_to_screen(viewport, anchor_world(p))
}

/// The world-space [`Point`] for a world anchor expressed as a [`Pos2`].
fn anchor_world(p: Pos2) -> Point {
    Point::new(f64::from(p.x), f64::from(p.y))
}

fn card_radius(camera: &Camera) -> CornerRadius {
    CornerRadius::same((CARD_RADIUS * camera.scale()).clamp(0.0, 18.0) as u8)
}

/// The on-screen rect for a card under the card-scale compromise (`proj-card-scale`):
/// the card's world CENTER mapped through the lens-composed `world_to_screen`,
/// with the rect built from the card's base (affine-only, `scale` × world size)
/// dimensions multiplied by a per-card scale — axis-aligned, centered on the
/// projected screen center. `fill_scale` is the optional neighbor-gap "fill the
/// space" factor from [`fill_scales`]: `Some` replaces the magnification-derived
/// `card_scale_at` so cards size to the gap to their nearest neighbour; `None`
/// keeps the historical magnification sizing. Under the Off lens `card_scale ==
/// 1.0` and center-mapping equals corner-mapping, so this returns the exact rect
/// `world_rect_to_screen` would (verified by `affine_card_rect_matches_corner_map`).
fn projected_card_rect(
    camera: &Camera,
    viewport: Rect,
    bounds: hiker_canvas::geometry::Rect,
    fill_scale: Option<f32>,
) -> Rect {
    let center = camera.world_to_screen(viewport, bounds.center());
    let card_scale = fill_scale.unwrap_or_else(|| camera.card_scale_at(bounds.center()));
    let half = Vec2::new(
        bounds.width as f32 * camera.scale() * 0.5 * card_scale,
        bounds.height as f32 * camera.scale() * 0.5 * card_scale,
    );
    Rect::from_center_size(center, half * 2.0)
}

/// Neighbor-gap "fill the space" per-card scales under an active lens: each card
/// grows to roughly the screen distance to its nearest neighbour so sparse
/// regions fill out and dense regions stay compact (natural focus+context).
///
/// `screen_centers` are the projected on-screen centres and `base_sizes` the
/// affine-world on-screen sizes (width/height) of the same cards (parallel
/// slices). For card `i` the target on-screen size is `gap_i · fill`, where
/// `gap_i` is the min distance to any other centre (a lone card uses a sensible
/// default gap). The returned scale is `target / max(base_w, base_h)` — sizing by
/// the *larger* base dimension so cards grow to fill the gap without overlapping
/// — clamped to `clamp`'s `[min, max]`. Pure (no egui painter) so it is
/// unit-testable. [proj-card-fill]
/// Per-node neighbor-gap fill scales for a whole canvas, aligned 1:1 with
/// `canvas.nodes`. Under an active lens every non-group node gets `Some(scale)`
/// from [`fill_scales`] (computed over the projected screen centres of all
/// non-group nodes), and groups get `None`. With the lens Off the whole vector is
/// `None`, so card sizing falls back to the historical affine path — the
/// byte-identical guarantee is preserved. The scene pre-pass calls this once per
/// frame and threads each entry into [`node_card_filled`]. [proj-card-fill]
#[must_use]
pub fn lens_fill_scales(camera: &Camera, viewport: Rect, canvas: &Canvas) -> Vec<Option<f32>> {
    if !camera.lens_active() {
        return vec![None; canvas.nodes.len()];
    }
    // Collect the projected centres and affine-world screen sizes of every
    // non-group card, remembering each card's index in `canvas.nodes` so the
    // computed scales can be scattered back into a node-aligned vector.
    let mut idx = Vec::new();
    let mut centers = Vec::new();
    let mut sizes = Vec::new();
    for (i, node) in canvas.nodes.iter().enumerate() {
        if matches!(node.kind, NodeKind::Group { .. }) {
            continue;
        }
        let bounds = node_bounds(node);
        idx.push(i);
        centers.push(camera.world_to_screen(viewport, bounds.center()));
        sizes.push(Vec2::new(bounds.width as f32 * camera.scale(), bounds.height as f32 * camera.scale()));
    }
    let scales = fill_scales(&centers, &sizes, camera.card_scale_clamp().fill, camera.card_scale_clamp());
    let mut out = vec![None; canvas.nodes.len()];
    for (slot, scale) in idx.into_iter().zip(scales) {
        out[slot] = Some(scale);
    }
    out
}

fn fill_scales(screen_centers: &[Pos2], base_sizes: &[Vec2], fill: f32, clamp: CardScaleClamp) -> Vec<f32> {
    let n = screen_centers.len();
    // Lone card: no neighbour to measure against — fall back to its own size as
    // the gap so it keeps (clamped) its natural footprint instead of dividing by
    // a degenerate distance.
    let default_gap = |i: usize| base_sizes.get(i).map_or(1.0, |s| s.x.max(s.y)).max(1.0);
    (0..n)
        .map(|i| {
            let mut gap = f32::INFINITY;
            for (j, &c) in screen_centers.iter().enumerate() {
                if j != i {
                    gap = gap.min((screen_centers[i] - c).length());
                }
            }
            if !gap.is_finite() {
                gap = default_gap(i);
            }
            let base = base_sizes.get(i).map_or(1.0, |s| s.x.max(s.y)).max(1.0);
            let target = gap * fill;
            clamp.apply(target / base)
        })
        .collect()
}

/// Re-export the handle size constant for the interaction layer.
#[must_use]
pub const fn handle_size() -> f32 {
    HANDLE_SIZE
}

#[cfg(test)]
mod lod_tests {
    use super::{
        adaptive_segments, arrowhead_size, file_basename, fill_scales, first_nonempty_line,
        group_label_size, is_bare_dot, is_tiny, lod_title, projected_card_rect, url_host,
        BARE_DOT_PX, MAX_GEODESIC_SEGMENTS, LOD_MIN_PX,
    };
    use canvas_view_core::camera::{Camera, CardScaleClamp};
    use hiker_projection::Complex;
    use egui::{Pos2, Rect, Vec2};
    use hiker_canvas::geometry::{node_bounds, Point, Rect as CanvasRect};
    use hiker_canvas::model::{Node, NodeKind};
    use std::collections::BTreeMap;

    fn rect(w: f32, h: f32) -> Rect {
        Rect::from_min_size(Pos2::ZERO, Vec2::new(w, h))
    }

    /// Under the Off (Affine) lens the center-mapped card rect MUST equal the
    /// corner-mapped `world_rect_to_screen` rect exactly — the byte-identical
    /// guarantee for a non-projected canvas. [proj-card-scale]
    #[test]
    fn affine_card_rect_matches_corner_map() {
        let mut cam = Camera::default();
        cam.set_pan_scale(Point::new(-40.0, 22.0), 0.75);
        let vp = Rect::from_min_size(Pos2::new(10.0, 5.0), Vec2::new(800.0, 600.0));
        let node = Node {
            id: "n".into(),
            x: 120,
            y: -60,
            width: 300,
            height: 200,
            color: None,
            kind: NodeKind::Text { text: "hi".into() },
            extra: BTreeMap::new(),
        };
        let bounds = node_bounds(&node);
        let proj = projected_card_rect(&cam, vp, bounds, None);
        let corner = cam.world_rect_to_screen(vp, bounds);
        assert!((proj.min.x - corner.min.x).abs() < 1e-3, "{proj:?} vs {corner:?}");
        assert!((proj.min.y - corner.min.y).abs() < 1e-3);
        assert!((proj.max.x - corner.max.x).abs() < 1e-3);
        assert!((proj.max.y - corner.max.y).abs() < 1e-3);
    }

    /// A fisheye lens scales cards by magnification while keeping them
    /// axis-aligned: a card at the focus is larger than one at the rim, and both
    /// rects stay axis-aligned (width/height stay positive, no rotation is
    /// representable in an `egui::Rect`). [proj-card-scale]
    #[test]
    fn fisheye_scales_card_by_magnification_axis_aligned() {
        let mut cam = Camera::default();
        cam.set_projection(hiker_projection::ProjectionConfig {
            kind: hiker_projection::ProjectionKind::Fisheye,
            strength: 1.0,
            size_falloff: 1.0,
            geodesic_segments: 16,
        });
        let world = CanvasRect::new(-500.0, -500.0, 1000.0, 1000.0);
        cam.update_lens(Some(world));
        let vp = Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0));
        let center_node = CanvasRect::new(-50.0, -50.0, 100.0, 100.0);
        let rim_node = CanvasRect::new(440.0, 440.0, 100.0, 100.0);
        let c = projected_card_rect(&cam, vp, center_node, None);
        let r = projected_card_rect(&cam, vp, rim_node, None);
        assert!(c.width() > r.width(), "center card {} wider than rim {}", c.width(), r.width());
        assert!(c.width() > 0.0 && c.height() > 0.0 && r.width() >= 0.0, "axis-aligned, non-negative");
    }

    #[test]
    fn tiny_when_either_dimension_below_threshold() {
        // A 300x200 card at zoom-to-fit (~0.14 scale) lands near 41x28 px: tiny.
        assert!(is_tiny(rect(41.0, 28.0)));
        // Narrow but tall, or wide but short, both count as tiny.
        assert!(is_tiny(rect(LOD_MIN_PX - 1.0, 500.0)));
        assert!(is_tiny(rect(500.0, LOD_MIN_PX * 0.64 - 1.0)));
    }

    /// The deepest LOD tier: a card below `BARE_DOT_PX` in either dimension
    /// collapses to a bare dot, and the dot tier is strictly deeper than the
    /// title-placeholder tier (a bare dot is always also "tiny"). [proj-lod-ladder]
    #[test]
    fn bare_dot_when_too_small_for_a_title() {
        // Below the width or (looser) height bound → bare dot.
        assert!(is_bare_dot(rect(BARE_DOT_PX - 1.0, 500.0)));
        assert!(is_bare_dot(rect(500.0, BARE_DOT_PX * 0.64 - 1.0)));
        // A card large enough for a title (but still LOD-tiny) is NOT a bare dot.
        assert!(!is_bare_dot(rect(BARE_DOT_PX, BARE_DOT_PX)));
        assert!(!is_bare_dot(rect(LOD_MIN_PX - 1.0, LOD_MIN_PX - 1.0)));
        // Every bare dot is also tiny (the ladder nests: dot ⊂ tiny ⊂ readable).
        assert!(is_tiny(rect(BARE_DOT_PX - 1.0, BARE_DOT_PX - 1.0)));
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

    /// Two cards far apart fill more of the empty space (larger scale) than two
    /// cards close together, and every scale stays within the clamp bounds.
    /// [proj-card-fill]
    #[test]
    fn fill_scales_fills_sparse_more_than_dense() {
        let clamp = CardScaleClamp { min: 0.2, max: 4.0, fill: 0.9 };
        let base = vec![Vec2::new(100.0, 60.0); 2];
        let far = fill_scales(&[Pos2::new(0.0, 0.0), Pos2::new(800.0, 0.0)], &base, 0.9, clamp);
        let near = fill_scales(&[Pos2::new(0.0, 0.0), Pos2::new(120.0, 0.0)], &base, 0.9, clamp);
        assert!(far[0] > near[0], "sparse {} should fill more than dense {}", far[0], near[0]);
        for s in far.iter().chain(near.iter()) {
            assert!(*s >= clamp.min - 1e-6 && *s <= clamp.max + 1e-6, "scale {s} out of clamp");
        }
    }

    /// A single node has no neighbour to measure against, so the gap helper must
    /// still return a finite, clamped scale (no divide-by-zero / infinity).
    /// [proj-card-fill]
    #[test]
    fn fill_scale_single_node_is_sane() {
        let clamp = CardScaleClamp::default();
        let scales = fill_scales(&[Pos2::new(40.0, 40.0)], &[Vec2::new(100.0, 60.0)], 0.9, clamp);
        assert_eq!(scales.len(), 1);
        assert!(scales[0].is_finite(), "scale must be finite");
        assert!(scales[0] >= clamp.min - 1e-6 && scales[0] <= clamp.max + 1e-6, "scale {} out of clamp", scales[0]);
    }

    /// A wider geodesic arc earns more samples than a shallow one, never above the
    /// MAX cap. [proj-card-fill]
    #[test]
    fn adaptive_segments_increase_with_arc_span() {
        let base = 8;
        // A near-diameter pair: shallow arc → stays near base.
        let shallow = adaptive_segments(Complex::new(0.1, 0.02), Complex::new(-0.1, -0.015), base);
        // A wide right-angle pair near the rim: sharply-curved arc → more samples.
        let wide = adaptive_segments(Complex::new(0.85, 0.0), Complex::new(0.0, 0.85), base);
        assert!(wide > shallow, "wider arc {wide} should out-sample shallow {shallow}");
        assert!(wide <= MAX_GEODESIC_SEGMENTS, "capped at MAX");
        assert!(shallow >= base, "never below base");
    }

    /// Under Poincaré a central card's drawn rect is larger than a rim card's
    /// when both are sized by the neighbor-gap fill pre-pass: the central card's
    /// neighbours project farther apart on screen than the squished rim card's, so
    /// focus+context is preserved even with fill sizing. [proj-card-fill]
    #[test]
    fn poincare_central_card_fills_larger_than_rim() {
        use canvas_view_core::camera::Camera as Cam;
        use hiker_canvas::geometry::Rect as CanvasRect;
        use hiker_canvas::model::{Node, NodeKind};
        use std::collections::BTreeMap;
        let mut cam = Cam::default();
        cam.set_projection(hiker_projection::ProjectionConfig {
            kind: hiker_projection::ProjectionKind::Poincare,
            strength: 2.2,
            size_falloff: 1.0,
            geodesic_segments: 16,
        });
        // A dense, regular 6×6 grid (uniform world spacing). Under the disk the
        // periphery compresses, so corner cards pack tighter on screen (small
        // neighbour gap → small fill) while the central cards keep room to grow.
        let mk = |row: i64, col: i64| Node {
            id: format!("{row}:{col}"),
            x: -1500 + col * 600,
            y: -1500 + row * 600,
            width: 200,
            height: 130,
            color: None,
            kind: NodeKind::Text { text: "x".into() },
            extra: BTreeMap::new(),
        };
        let mut nodes = Vec::new();
        for row in 0..6 {
            for col in 0..6 {
                nodes.push(mk(row, col));
            }
        }
        // Frame the lens to the grid extent so it fills the disk.
        let world = CanvasRect::new(-1800.0, -1800.0, 3600.0, 3600.0);
        cam.update_lens(Some(world));
        let vp = Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0));
        let canvas = hiker_canvas::model::Canvas { nodes: nodes.clone(), ..Default::default() };
        let fills = super::lens_fill_scales(&cam, vp, &canvas);
        // A near-centre card (row 2, col 2 → index 14) vs a far corner (index 0).
        let central = fills[14].expect("central fill");
        let rim = fills[0].expect("rim fill");
        assert!(central > rim, "central fill {central} should exceed rim {rim}");
    }
}
