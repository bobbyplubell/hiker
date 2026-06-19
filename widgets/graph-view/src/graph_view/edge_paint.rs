//! Low-level node / edge painting primitives behind the
//! [`State`](super::State) draw loop in `edges.rs`: adaptive geodesic sample
//! density, the per-node LOD tier, node-fill emission to either the egui
//! Painter or the instanced GPU batch, edge-polyline emission, and the
//! sub-polyline / gradient-stroke / hop-potential geometry the hover-flow and
//! fluid-highlight overlays are built from.

use super::gpu::GpuBatch;
use super::source::{NodeDescriptor, NodeShape};

/// Alpha at/below which a node or edge contributes no perceptible pixels and is
/// skipped entirely (≈ 3/255). Saves shape tessellation, label shaping, and
/// geodesic sampling for the rim that the Poincaré fade has already erased.
pub(super) const CULL_ALPHA: f32 = 0.012;

/// Screen-space pixels per geodesic segment. Edges are sampled to hold ~one
/// segment per `SEG_PX` of on-screen chord, so a short edge uses 2 points and
/// only long arcs pay for smoothness — instead of a flat `geodesic_segments`.
const SEG_PX: f32 = 8.0;

/// Segment count for an edge whose on-screen chord is `chord_px` long, clamped to
/// `[2, max]` (`max` = the configured `geodesic_segments`, the old flat constant).
pub(super) fn adaptive_segments(chord_px: f32, max: u32) -> u32 {
    let want = (chord_px / SEG_PX).ceil() as u32;
    want.clamp(2, max.max(2))
}

/// LOD render tier for a node, selected from its magnification.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Lod {
    /// Full detail: the descriptor's circle/square + label (zoom rule still
    /// gates the label).
    Full,
    /// A small filled dot, no label, no hover ring.
    Dot,
    /// A tiny point, no label.
    Marker,
}

/// The position/radius pair a node fill is drawn at, in both spaces, so the
/// GPU batch can pick the one matching its `world_space`.
#[derive(Clone, Copy)]
pub(super) struct NodeFill {
    pub(super) world: egui::Pos2,
    pub(super) base_r: f32,
    pub(super) screen: egui::Pos2,
    pub(super) screen_r: f32,
    pub(super) color: egui::Color32,
}

/// Draw one node's fill at its LOD tier. With `gpu = None` this is the
/// historical egui-Painter path (byte-identical). With `gpu = Some`, the fills
/// route to the instanced batch instead — but the single hovered-node stroke
/// ring (circle ring or square outline) always stays on the Painter so it
/// layers on top of the GPU pass, matching the v1 scope.
/// Push a node fill into the GPU batch in the batch's own space: world centre +
/// base radius when `world_space` (Affine, cacheable; the shader applies the
/// view transform), or the final screen centre + radius otherwise. `square`
/// selects the shape. Only ever called for [`Lod::Full`] in world space (Affine
/// is always Full); the lens/screen path covers Dot/Marker via its capped radii.
fn push_node_fill(batch: &mut GpuBatch, fill: NodeFill, square: bool) {
    let (center, radius) = if batch.world_space {
        (fill.world, fill.base_r)
    } else {
        (fill.screen, fill.screen_r)
    };
    batch.push_node(center, radius, square, fill.color);
}

#[allow(clippy::too_many_arguments)]
pub(super) fn draw_node_fill(
    painter: &egui::Painter,
    gpu: Option<&mut GpuBatch>,
    d: &NodeDescriptor,
    tier: Lod,
    fill_pos: NodeFill,
    is_hover: bool,
) {
    let (p, r, fill) = (fill_pos.screen, fill_pos.screen_r, fill_pos.color);
    match tier {
        Lod::Full => match d.shape {
            NodeShape::Circle => {
                let stroke = if is_hover { d.hover_stroke } else { d.resting_stroke };
                if let Some(batch) = gpu {
                    push_node_fill(batch, fill_pos, false);
                    // The resting stroke is `Stroke::NONE` for code graphs; only the
                    // hover ring is non-trivial, and it stays on the Painter.
                    if stroke.width > 0.0 {
                        painter.circle_stroke(p, r, stroke);
                    }
                } else {
                    painter.circle(p, r, fill, stroke);
                }
            }
            NodeShape::Square => {
                let rect = egui::Rect::from_center_size(p, egui::Vec2::splat(r * 2.0));
                if let Some(batch) = gpu {
                    push_node_fill(batch, fill_pos, true);
                } else {
                    painter.rect_filled(rect, 1.0, fill);
                }
                if is_hover {
                    painter.rect_stroke(
                        rect.expand(2.0),
                        1.0,
                        d.hover_stroke,
                        egui::StrokeKind::Outside,
                    );
                }
            }
        },
        Lod::Dot => {
            let rr = r.min(3.0);
            match gpu {
                // Dot/Marker only occur under a lens (screen-space batch).
                Some(batch) => batch.push_node(p, rr, false, fill),
                None => {
                    painter.circle_filled(p, rr, fill);
                }
            }
        }
        Lod::Marker => match gpu {
            Some(batch) => batch.push_node(p, 1.5, false, fill),
            None => {
                painter.circle_filled(p, 1.5, fill);
            }
        },
    }
}

/// Emit one edge polyline (`pts.len() >= 2`). With `gpu = None` it draws on the
/// egui Painter (a single segment as `line_segment`, a polyline as
/// `Shape::line`) at the configured `width`. With `gpu = Some` the points push
/// into the instanced edge batch (one [`EdgeInstance`] per segment); the
/// configured `width` is applied in-shader from the per-pane uniform, so GPU
/// edges honour the edge-width control just like the Painter path.
pub(super) fn emit_edge(
    painter: &egui::Painter,
    gpu: Option<&mut GpuBatch>,
    pts: &[egui::Pos2],
    width: f32,
    color: egui::Color32,
) {
    if pts.len() < 2 {
        return;
    }
    match gpu {
        Some(batch) => batch.push_polyline(pts, color),
        None => {
            let stroke = egui::Stroke::new(width, color);
            if pts.len() == 2 {
                painter.line_segment([pts[0], pts[1]], stroke);
            } else {
                painter.add(egui::Shape::line(pts.to_vec(), stroke));
            }
        }
    }
}

/// Un-premultiplied alpha tinting of the highlight colour, shared by the glow /
/// flow / fluid overlays so their passes layer translucently.
pub(super) fn tint(col: egui::Color32) -> impl Fn(f32) -> egui::Color32 {
    move |a: f32| {
        egui::Color32::from_rgba_unmultiplied(
            col.r(),
            col.g(),
            col.b(),
            (a.clamp(0.0, 1.0) * 255.0) as u8,
        )
    }
}

/// The sub-polyline covering normalized arc length `[t0, t1]` of `pts` (clamped
/// to `[0, 1]`), with interpolated endpoints — the geometry of the hover-flow
/// pulse window. Empty when the window collapses. status: graph-hover-flow
pub(super) fn sub_polyline(pts: &[egui::Pos2], t0: f32, t1: f32) -> Vec<egui::Pos2> {
    if pts.len() < 2 {
        return Vec::new();
    }
    let mut cum = Vec::with_capacity(pts.len());
    let mut total = 0.0f32;
    cum.push(0.0);
    for w in pts.windows(2) {
        total += (w[1] - w[0]).length();
        cum.push(total);
    }
    if total <= f32::EPSILON {
        return Vec::new();
    }
    let (lo, hi) = ((t0.clamp(0.0, 1.0)) * total, (t1.clamp(0.0, 1.0)) * total);
    if hi - lo <= f32::EPSILON {
        return Vec::new();
    }
    let at = |target: f32| -> egui::Pos2 {
        let seg = cum.partition_point(|&c| c < target).clamp(1, pts.len() - 1);
        let (c0, c1) = (cum[seg - 1], cum[seg]);
        let f = if c1 > c0 { (target - c0) / (c1 - c0) } else { 0.0 };
        pts[seg - 1] + (pts[seg] - pts[seg - 1]) * f
    };
    let mut out = vec![at(lo)];
    for (i, &c) in cum.iter().enumerate() {
        if c > lo && c < hi {
            out.push(pts[i]);
        }
    }
    out.push(at(hi));
    out
}

/// Stroke `pts` as an alpha gradient from `a0` (start end) to `a1` (far end):
/// the polyline is resampled into short chunks, each stroked at its midpoint's
/// lerped alpha — how a fluid edge reads brighter at its more energized end.
/// status: graph-hover-fluid
pub(super) fn gradient_strokes(
    out: &mut Vec<egui::Shape>,
    pts: &[egui::Pos2],
    a0: f32,
    a1: f32,
    core_w: f32,
    soft: f32,
    tinted: &impl Fn(f32) -> egui::Color32,
) {
    const CHUNKS: usize = 6;
    for k in 0..CHUNKS {
        let (t0, t1) = (k as f32 / CHUNKS as f32, (k + 1) as f32 / CHUNKS as f32);
        let seg = sub_polyline(pts, t0, t1);
        if seg.len() < 2 {
            continue;
        }
        let a = a0 + (a1 - a0) * ((t0 + t1) * 0.5);
        if a < 0.01 {
            continue;
        }
        if soft > 0.0 {
            out.push(egui::Shape::line(
                seg.clone(),
                egui::Stroke::new(core_w * (1.0 + 2.0 * soft), tinted(a * 0.25)),
            ));
        }
        out.push(egui::Shape::line(seg, egui::Stroke::new(core_w, tinted(a * 0.8))));
    }
}

/// Hop-distance potential from `selected` over the undirected edge set — the
/// "gravity" the fluid highlight runs down. Flat (all zero → no drift) when
/// nothing is selected; unreachable nodes sit at the maximum so stray energy
/// still drains toward the reachable component. status: graph-hover-fluid
pub(super) fn hop_potential(n: usize, edges: &[(u32, u32)], selected: Option<usize>) -> Vec<f32> {
    let Some(s) = selected.filter(|&s| s < n) else {
        return vec![0.0; n];
    };
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    for &(a, b) in edges {
        let (a, b) = (a as usize, b as usize);
        if a < n && b < n {
            adj[a].push(b);
            adj[b].push(a);
        }
    }
    let mut dist = vec![f32::MAX; n];
    let mut q = std::collections::VecDeque::from([s]);
    dist[s] = 0.0;
    while let Some(u) = q.pop_front() {
        for &v in &adj[u] {
            if dist[v] == f32::MAX {
                dist[v] = dist[u] + 1.0;
                q.push_back(v);
            }
        }
    }
    let max = dist.iter().copied().filter(|d| *d < f32::MAX).fold(0.0f32, f32::max);
    for d in &mut dist {
        if *d == f32::MAX {
            *d = max + 1.0;
        }
    }
    dist
}

#[cfg(test)]
mod overlay_tests {
    use super::{hop_potential, sub_polyline};
    use eframe::egui::pos2;

    #[test]
    fn sub_polyline_clips_with_interpolated_endpoints() {
        // A straight 10px line: the [0.25, 0.75] window is the middle 5px.
        let pts = [pos2(0.0, 0.0), pos2(10.0, 0.0)];
        let w = sub_polyline(&pts, 0.25, 0.75);
        assert_eq!(w.first().copied(), Some(pos2(2.5, 0.0)));
        assert_eq!(w.last().copied(), Some(pos2(7.5, 0.0)));
        // Windows beyond the ends clamp; a collapsed window is empty.
        assert!(sub_polyline(&pts, 1.2, 1.4).is_empty());
        assert!(!sub_polyline(&pts, 0.9, 1.4).is_empty());
        // Interior vertices inside the window are preserved.
        let bent = [pos2(0.0, 0.0), pos2(5.0, 0.0), pos2(5.0, 5.0)];
        let whole = sub_polyline(&bent, 0.0, 1.0);
        assert_eq!(whole, vec![pos2(0.0, 0.0), pos2(5.0, 0.0), pos2(5.0, 5.0)], "full window reproduces the polyline");
    }

    #[test]
    fn hop_potential_is_bfs_distance_with_unreachable_at_max() {
        // 0—1—2, 3 isolated; selected = 0.
        let dist = hop_potential(4, &[(0, 1), (1, 2)], Some(0));
        assert_eq!(dist[0], 0.0);
        assert_eq!(dist[1], 1.0);
        assert_eq!(dist[2], 2.0);
        assert_eq!(dist[3], 3.0, "unreachable = max + 1, still drains toward the component");
        assert_eq!(hop_potential(3, &[(0, 1)], None), vec![0.0; 3], "no selection → flat field");
    }
}
