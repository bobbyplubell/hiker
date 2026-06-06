//! Reusable rich-preview thumbnails: a tiny inline image rendered next to a
//! list row, that expands on hover into a larger floating preview which never
//! occludes the rows below it. Domain-agnostic — a caller supplies a
//! [`ThumbnailProvider`] (canvas, cluster-tree, …) and the widget owns
//! rendering, the on-disk cache, the texture upload, and the hover-expand
//! lifecycle.
//!
//! The pieces:
//!
//! - [`ThumbnailProvider`] — a domain plugs in by computing a stable
//!   [`PreviewKey`] (content hash + kind + pixel size) and rendering an
//!   `image::RgbaImage` at a requested pixel size. Renders happen off the live
//!   egui widget tree (egui can't capture a live widget to a texture), so a
//!   provider rasterizes a flat SVG / RGBA approximation — see the canvas and
//!   cluster-tree providers (`preview-canvas-thumbnail` / `preview-tree-thumbnail`).
//! - [`thumbnail`] — the inline widget: allocate a small fixed rect, blit the
//!   cached small texture (rendering + caching on a miss), and on hover register
//!   a [`HoverRequest`] for the expanded preview.
//! - [`render_expanded_preview`] — called ONCE per frame, after the sidebar, by
//!   the frame loop. It draws the large preview in a non-interactable
//!   `Order::Tooltip` `Area` anchored beside the row, so it never senses the
//!   pointer and never overlaps the rows — which is what lets the pointer keep
//!   independently re-triggering each row's own thumbnail hover
//!   (`preview-hover-expand-side-anchor`).
//!
//! status: preview-thumbnail-provider

mod cache;
mod note;
mod thumbnail;

pub(crate) use note::{
    register_note_hover, register_note_hover_interactive, render_note_preview,
};
pub(crate) use thumbnail::{register_hover_only, thumbnail};

use eframe::egui;

/// Bump to invalidate every cached preview when the rendering changes. Folded
/// into [`PreviewKey::content_hash`] by each provider, so a renderer tweak
/// (different node shape, colors, layout) makes every prior PNG a cache miss
/// without any manual sweep.
pub(crate) const PREVIEW_RENDER_VERSION: u32 = 2;

/// Which kind of document a preview depicts. Extensible: a new domain adds a
/// variant and its filename prefix (`cache.rs`) + provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum PreviewKind {
    /// A `.canvas` JSON Canvas document — rendered as its node/edge shape.
    Canvas,
    /// A cluster-tree note — rendered as a force-directed dots-and-lines graph.
    Tree,
}

/// The identity of a cached preview: the content it depicts, its kind, and the
/// pixel size bucket. Two sizes (a small inline thumbnail and a large expanded
/// preview) are separate buckets so they coexist in the cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PreviewKey {
    /// Hash of every input that affects the pixels — the document's bytes /
    /// serialized shape, *plus* [`PREVIEW_RENDER_VERSION`]. Providers fold the
    /// version in via [`content_hash`].
    pub content_hash: u64,
    pub kind: PreviewKind,
    /// The longest-edge pixel size the image was rendered at.
    pub size: u32,
}

/// Fold a domain's raw content hash together with the render version and kind
/// into the final [`PreviewKey::content_hash`]. Every provider routes its hash
/// through this so a render-version bump invalidates every kind at once.
pub(crate) fn content_hash(kind: PreviewKind, raw: u64) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    PREVIEW_RENDER_VERSION.hash(&mut h);
    // Discriminant tag so two kinds with the same raw hash never collide.
    (kind as u8).hash(&mut h);
    raw.hash(&mut h);
    h.finish()
}

/// A thunk that live-paints a provider's EXPANDED preview into a rect, returning
/// `true` when it painted (so the caller skips the cached-image blit). `Arc`-wrapped
/// so the [`thumbnail`] widget can stash it in egui memory and the post-sidebar
/// [`render_expanded_preview`] can run it; `Send + Sync` because egui's temp
/// store requires it. status: canvas-static-paint
pub(crate) type ExpandedPaint = std::sync::Arc<dyn Fn(&mut egui::Ui, egui::Rect) -> bool + Send + Sync>;

/// A domain that can render a rich preview thumbnail. The widget owns caching
/// and hover; an implementor only computes a stable key and rasterizes pixels.
pub(crate) trait ThumbnailProvider {
    /// The cache key at the small inline size. The widget derives the large
    /// (expanded) key from this by swapping the `size` bucket.
    fn cache_key(&self) -> PreviewKey;
    /// Render the preview at `px` (longest edge, physical pixels). `None` on any
    /// failure — the widget then draws a neutral placeholder, never panics.
    fn render(&self, px: u32) -> Option<image::RgbaImage>;
    /// Optionally LIVE-PAINT the EXPANDED hover preview rather than blitting the
    /// cached large image. A provider that returns `Some(thunk)` captures (a
    /// clone of) whatever it needs and hands back a `Send + Sync` thunk the
    /// expanded-preview renderer runs inside its non-interactable `Area`: the
    /// thunk paints the live preview into the given rect and returns `true`, or
    /// `false` to fall back to the cached blit. Default `None` keeps the tiny
    /// thumbnail + tree previews on the cached-image path unchanged.
    /// status: canvas-static-paint
    fn expanded_paint(&self) -> Option<ExpandedPaint> {
        None
    }
}

/// Options for [`thumbnail`]. Kept small + `Copy` so call sites stay terse.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ThumbnailOpts {
    /// Logical side length of the inline thumbnail rect (~16–18 px).
    pub side: f32,
    /// Longest-edge pixel size for the large expanded preview.
    pub large_px: u32,
}

impl Default for ThumbnailOpts {
    fn default() -> Self {
        Self { side: 16.0, large_px: 320 }
    }
}

/// A pending hover-expand request, stashed in egui memory during the sidebar
/// render and consumed once after the sidebar by [`render_expanded_preview`].
/// Carries everything the expanded draw needs without threading `AppState`
/// through the narrow activity `Ctx`.
#[derive(Clone)]
struct HoverRequest {
    key: PreviewKey,
    /// The inline thumbnail's screen rect — the expanded preview flows from
    /// here (to its right, over the editor; flips / clamps near edges).
    anchor: egui::Rect,
    /// When the current uninterrupted hover began. The large preview only draws
    /// after a short hold; until then the cached small image is shown.
    hover_started: f64,
    /// `input.time` of the frame this request was written on. The sidebar
    /// (where the thumbnail writes) renders before [`render_expanded_preview`]
    /// in the same frame, so a request the pointer is still over carries the
    /// current frame's time; a request from a prior frame (the pointer left
    /// every thumbnail) is stale and dropped. This is what makes the expanded
    /// preview vanish when the pointer moves off the row.
    written_at: f64,
    /// A live-paint thunk the hovered provider opted into (`expanded_paint`).
    /// When present, the expanded preview LIVE-PAINTS the real document into the
    /// preview rect rather than blitting the cached large image — the canvas
    /// case (`canvas-static-paint`). `None` keeps the cached-image path (trees).
    live_paint: Option<ExpandedPaint>,
}

/// egui-memory id the hover request lives under (one per frame, last writer
/// wins — the row the pointer is actually over).
fn hover_request_id() -> egui::Id {
    egui::Id::new("preview-hover-request")
}

/// Hover-hold before the large expanded preview replaces the small one, in
/// seconds. Debounces a quick pass down the list (`preview-hover-expand-side-anchor`).
pub(crate) const EXPAND_HOLD_SECS: f64 = 0.12;

/// Max logical size of the expanded preview card (image + frame padding).
const EXPANDED_MAX: egui::Vec2 = egui::vec2(320.0, 320.0);

/// Draw the one pending expanded preview, if any, AFTER the sidebar has
/// rendered. Called once per frame by the frame loop. Paints into a
/// non-interactable `Order::Tooltip` `Area`, so it never senses the pointer and
/// never steals the row hover beneath it. status: preview-hover-expand-side-anchor
pub(crate) fn render_expanded_preview(ctx: &egui::Context, vault_root: &std::path::Path) {
    let Some(req) = ctx.data(|d| d.get_temp::<HoverRequest>(hover_request_id())) else {
        return;
    };
    let now = ctx.input(|i| i.time);
    // Stale request: no thumbnail re-stashed it this frame (the pointer left
    // every thumbnail), so drop it and draw nothing — the expanded preview
    // disappears the instant the pointer moves off the row.
    if req.written_at < now {
        ctx.data_mut(|d| d.remove::<HoverRequest>(hover_request_id()));
        return;
    }
    // Keep the frame alive while a thumbnail is hovered so the debounce timer
    // and the small→large swap actually advance without further input.
    ctx.request_repaint();
    if now - req.hover_started < EXPAND_HOLD_SECS {
        return;
    }

    // Live-paint path (canvas): the hovered provider opted into rendering the
    // real document into the preview rect (`canvas-static-paint`), so skip the
    // cached-image blit entirely and paint live. The painter clips to the rect
    // and runs inside the non-interactable `Area`.
    if let Some(live) = req.live_paint.clone() {
        paint_live_expanded_area(ctx, req.anchor, &live);
        return;
    }

    let cache = cache::PreviewCache::new(vault_root);
    let large_key = req.key;
    let Some(img) = cache.load(large_key) else {
        // The large render is produced lazily by the thumbnail widget the next
        // time it runs the render path; until it lands, show nothing extra
        // (the inline small thumbnail is already visible).
        return;
    };
    let tex = ctx.load_texture(
        format!("preview-large-{:016x}-{}", large_key.content_hash, large_key.size),
        egui::ColorImage::from_rgba_unmultiplied([img.width as usize, img.height as usize], &img.rgba),
        egui::TextureOptions::LINEAR,
    );
    paint_expanded_area(ctx, req.anchor, &tex, egui::vec2(img.width as f32, img.height as f32));
}

/// Inner padding from the expanded card's popup frame to its content.
const EXPANDED_PAD: f32 = 6.0;

/// Where the expanded preview's `Area` sits, given its `draw` content size.
/// Anchored to the RIGHT of the thumbnail (over the editor); flips left when the
/// right would clip, flips up when the bottom would clip, then clamps fully
/// on-screen. Returns the top-left of the popup frame (content + padding).
pub(crate) fn expanded_area_min(ctx: &egui::Context, anchor: egui::Rect, draw: egui::Vec2) -> egui::Pos2 {
    let screen = ctx.screen_rect();
    let pad = EXPANDED_PAD;
    let frame = draw + egui::vec2(pad, pad) * 2.0;
    let gap = 8.0;
    // Default: just right of the thumbnail, vertically centered on it.
    let mut min = egui::pos2(anchor.right() + gap, anchor.center().y - frame.y / 2.0);
    // No room on the right (docked / wide sidebar pushing us off-screen): flip
    // to the left of the thumbnail.
    if min.x + frame.x > screen.right() - pad {
        min.x = anchor.left() - gap - frame.x;
    }
    // Near the bottom: pull up so the card stays on-screen (flip-up).
    if min.y + frame.y > screen.bottom() - pad {
        min.y = screen.bottom() - pad - frame.y;
    }
    min.x = min.x.clamp(screen.left() + pad, (screen.right() - pad - frame.x).max(screen.left() + pad));
    min.y = min.y.clamp(screen.top() + pad, (screen.bottom() - pad - frame.y).max(screen.top() + pad));
    min
}

/// Place + paint the expanded preview's `Area` from a cached texture. Anchored
/// per [`expanded_area_min`]; non-interactable so it never senses the pointer.
fn paint_expanded_area(
    ctx: &egui::Context,
    anchor: egui::Rect,
    texture: &egui::TextureHandle,
    tex_px: egui::Vec2,
) {
    let pad = EXPANDED_PAD;
    // Fit the texture into the size cap, preserving aspect.
    let scale = (EXPANDED_MAX.x / tex_px.x.max(1.0))
        .min(EXPANDED_MAX.y / tex_px.y.max(1.0))
        .min(1.0);
    let draw = tex_px * scale;
    let frame = draw + egui::vec2(pad, pad) * 2.0;
    let min = expanded_area_min(ctx, anchor, draw);

    egui::Area::new(egui::Id::new("preview-hover-expand"))
        .order(egui::Order::Tooltip)
        .interactable(false)
        .fixed_pos(min)
        .show(ctx, |ui| {
            ui.set_max_size(frame);
            egui::Frame::popup(ui.style())
                .inner_margin(egui::Margin::same(pad as i8))
                .show(ui, |ui| {
                    ui.add(
                        egui::Image::new(texture)
                            .fit_to_exact_size(draw)
                            .sense(egui::Sense::hover()),
                    );
                });
        });
}

/// Place + paint the expanded preview's `Area` by LIVE-PAINTING via the
/// provider's `live` thunk (canvas — `canvas-static-paint`). Same placement /
/// non-interactable framing as the cached path, but the inner content rect is
/// handed to the thunk to paint into rather than blitted from a texture. The
/// content is a fixed `EXPANDED_MAX` square so the live render has a stable
/// viewport to fit into. The thunk clips to the rect itself.
fn paint_live_expanded_area(ctx: &egui::Context, anchor: egui::Rect, live: &ExpandedPaint) {
    let pad = EXPANDED_PAD;
    let draw = EXPANDED_MAX;
    let frame = draw + egui::vec2(pad, pad) * 2.0;
    let min = expanded_area_min(ctx, anchor, draw);

    egui::Area::new(egui::Id::new("preview-hover-expand"))
        .order(egui::Order::Tooltip)
        .interactable(false)
        .fixed_pos(min)
        .show(ctx, |ui| {
            ui.set_max_size(frame);
            egui::Frame::popup(ui.style())
                .inner_margin(egui::Margin::same(pad as i8))
                .show(ui, |ui| {
                    let (rect, _) = ui.allocate_exact_size(draw, egui::Sense::hover());
                    let mut child = ui.new_child(
                        egui::UiBuilder::new().max_rect(rect).layout(*ui.layout()),
                    );
                    child.set_clip_rect(rect);
                    live(&mut child, rect);
                });
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_version_folds_into_hash() {
        // Same raw, same kind → same folded hash (stable across calls).
        let a = content_hash(PreviewKind::Canvas, 0xDEAD_BEEF);
        let b = content_hash(PreviewKind::Canvas, 0xDEAD_BEEF);
        assert_eq!(a, b);
    }

    #[test]
    fn kinds_with_equal_raw_hash_differ() {
        let canvas = content_hash(PreviewKind::Canvas, 42);
        let tree = content_hash(PreviewKind::Tree, 42);
        assert_ne!(canvas, tree, "kind tag must disambiguate equal raw hashes");
    }
}
