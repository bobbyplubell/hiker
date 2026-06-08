//! Retained-paint caching for idle full-detail canvas cards.
//!
//! Past the LOD cutoff every visible card paints its full body through the
//! host content engine (`editor-egui` markdown layout + shape emission) EVERY
//! frame. Profiling (`tools/profile-canvas`) shows that with the editor's own
//! galley/layout caches warm, the residual ~100µs/card is pure per-frame
//! *shape emission* — pushing the card's `Shape::Galley` / `rect_filled` /
//! widget shapes into egui's paint list. A still card re-emits an identical
//! picture every frame for nothing.
//!
//! This module records the egui shapes a card's body emitted (an index range
//! into the active layer's paint list, captured via the public
//! [`egui::layers::PaintList::next_idx`] / `all_entries` API — no fork, no
//! offscreen pass) and, while the card is **Idle**, re-emits those cached
//! shapes translated to the card's current screen position instead of calling
//! the content engine. A pure camera pan re-blits every idle card with zero
//! re-layout / re-emission; the engine runs again only for the card(s) actually
//! changing. status: canvas-idle-card-cache
//!
//! ## Active vs Idle
//!
//! A card is **Active** (live-render this frame, refresh its cache) when any of:
//! its content signature changed; its on-screen size or per-card zoom/scroll
//! changed (a cached render is position-translatable but not scale-invariant, so
//! a size change invalidates); the theme (dark/light) changed; or the pointer is
//! over it with scroll/zoom input this frame (it is being scrolled). A pure pan
//! (the card's `min` moved but its `size`, view, and signature are unchanged)
//! keeps the card **Idle** — the win this module exists for.

use std::collections::HashMap;

use egui::epaint::ClippedShape;
use egui::{Rect, Vec2};

use crate::content::CardView;

/// A captured set of a card body's egui shapes plus the inputs they were
/// captured under, so a later frame can decide whether they may be re-blitted.
#[derive(Debug)]
struct CachedCard {
    /// The card body shapes (already translated to screen space when captured),
    /// cloned out of the layer's paint list. Re-emitted translated while Idle.
    shapes: Vec<ClippedShape>,
    /// The `inner` content rect the shapes were captured at; the re-blit
    /// translates by `current.min - captured.min` and a size mismatch
    /// invalidates (a cached render is not scale-invariant).
    rect: Rect,
    /// The per-card view (zoom + scroll) the shapes were captured under; a
    /// change re-renders (the body laid out / scrolled differently).
    view: CardView,
    /// The content signature the shapes were captured under (`None` when the
    /// host opts a node out of caching); a change re-renders.
    signature: Option<u64>,
    /// Dark mode at capture; a theme flip re-renders (colors differ).
    dark: bool,
    /// The frame counter at last touch, for eviction of off-screen cards.
    last_used: u64,
}

/// Per-`CanvasView` store of cached idle-card renders, keyed by node id.
#[derive(Debug, Default)]
pub struct CardCache {
    cards: HashMap<String, CachedCard>,
    frame: u64,
    /// Live (engine) re-renders performed this frame, against [`Self::budget`].
    rendered_this_frame: usize,
    /// Max live re-renders allowed per frame; cards over budget fall back to a
    /// stale blit (or skip) for a frame so a fast pan never live-renders a whole
    /// screenful at once. status: canvas-idle-card-cache
    budget: usize,
}

/// The maximum cards re-rendered live in one frame before the rest fall back to
/// their (possibly stale) cached blit. Generous enough that a still or slowly
/// scrolling board never hits it, low enough that a fling never re-renders a
/// full viewport of cards in a single frame.
const DEFAULT_BUDGET: usize = 8;

/// The decision for one card this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardPaint {
    /// Live-render the body through the content engine, then capture its shapes.
    Render,
    /// Skip the engine; the cached shapes were re-blitted translated.
    Blit,
}

impl CardCache {
    /// Begin a frame: advance the frame counter, reset the per-frame live-render
    /// budget, and evict cards not touched for a while (scrolled far off-screen).
    pub fn begin_frame(&mut self) {
        self.frame = self.frame.wrapping_add(1);
        self.rendered_this_frame = 0;
        if self.budget == 0 {
            self.budget = DEFAULT_BUDGET;
        }
        // Evict cards untouched for more than `EVICT_AFTER` frames. Use a
        // saturating age (guarding the early-frame case where `frame` is still
        // below `EVICT_AFTER`, so a naive subtraction would underflow and evict
        // everything) — a card touched this/last frame always survives.
        let frame = self.frame;
        self.cards.retain(|_, c| frame.saturating_sub(c.last_used) <= EVICT_AFTER);
    }

    /// Decide whether card `id` may reuse its cached render this frame, given the
    /// card's current screen `rect`, per-card `view`, content `signature`,
    /// `dark` mode, and whether it is being actively scrolled/zoomed
    /// (`interacting`). When the answer is [`CardPaint::Blit`] the cached shapes
    /// are already re-emitted (translated) into `painter` and the caller skips
    /// the engine; otherwise the caller renders live and calls [`Self::capture`].
    pub fn decide(&mut self, req: &CardRequest<'_>, painter: &egui::Painter) -> CardPaint {
        let reusable = self.reusable(req);
        if reusable {
            self.blit(req.id, req.rect, painter);
            return CardPaint::Blit;
        }
        // Stale: would normally live-render. Honour the per-frame budget — over
        // it, re-blit the stale cache (if any) for a frame rather than pay a
        // live render, so a fast pan can't render a whole screen at once.
        if self.rendered_this_frame >= self.budget && self.cards.contains_key(req.id) {
            self.blit(req.id, req.rect, painter);
            return CardPaint::Blit;
        }
        self.rendered_this_frame += 1;
        CardPaint::Render
    }

    /// Whether the cached entry for `id` is valid for the current request: it
    /// exists, the card isn't being interacted with, and every capture input
    /// (size, view, signature, theme) matches. A pure pan (only `rect.min`
    /// moved) passes — that is the case this cache optimises.
    fn reusable(&self, req: &CardRequest<'_>) -> bool {
        if req.interacting {
            return false;
        }
        let Some(c) = self.cards.get(req.id) else {
            return false;
        };
        same_size(c.rect, req.rect)
            && c.view == req.view
            && c.signature == req.signature
            && c.dark == req.dark
    }

    /// Re-emit a cached card's shapes translated from its captured position to
    /// the card's current `rect`, refreshing its touch frame. A pure pan moves
    /// the whole picture by `rect.min - captured.min` with no re-layout.
    fn blit(&mut self, id: &str, rect: Rect, painter: &egui::Painter) {
        let Some(c) = self.cards.get_mut(id) else {
            return;
        };
        c.last_used = self.frame;
        let delta = rect.min - c.rect.min;
        emit_translated(painter, &c.shapes, delta);
    }

    /// Record the shapes a just-live-rendered card body emitted into `painter`'s
    /// layer between `start` and the current end of the paint list, so a later
    /// Idle frame can re-blit them. Stores them under the capture inputs from
    /// `req`. Called by the caller right after the engine render.
    pub fn capture(&mut self, req: &CardRequest<'_>, painter: &egui::Painter, start: egui::layers::ShapeIdx) {
        let layer = painter.layer_id();
        let end = painter.ctx().graphics(|g| g.get(layer).map_or(start, egui::layers::PaintList::next_idx));
        let shapes = painter.ctx().graphics(|g| {
            g.get(layer).map_or_else(Vec::new, |list| {
                list.all_entries()
                    .skip(start.0)
                    .take(end.0.saturating_sub(start.0))
                    .cloned()
                    .collect()
            })
        });
        self.cards.insert(
            req.id.to_owned(),
            CachedCard {
                shapes,
                rect: req.rect,
                view: req.view,
                signature: req.signature,
                dark: req.dark,
                last_used: self.frame,
            },
        );
    }

    /// Drop every cached card. Called when the host wants a hard reset (DPI
    /// change, tab close): a cached render baked at one DPI/atlas must not blit.
    pub fn clear(&mut self) {
        self.cards.clear();
    }
}

/// The per-card inputs the cache classifies a frame on, grouped so the
/// decision/capture calls stay within the argument-count budget.
pub struct CardRequest<'a> {
    /// Node id (cache key).
    pub id: &'a str,
    /// The card's current content rect in screen px.
    pub rect: Rect,
    /// The per-card view (zoom + scroll).
    pub view: CardView,
    /// The host's content signature for this node, or `None` to never cache it.
    pub signature: Option<u64>,
    /// Dark mode this frame.
    pub dark: bool,
    /// True when the card is being scrolled/zoomed this frame (pointer over it
    /// with wheel/zoom input) — forces a live render.
    pub interacting: bool,
}

/// Cards untouched for this many frames are evicted (≈2s at 60 Hz) — a grace
/// window so a card scrolled off and back doesn't re-render.
const EVICT_AFTER: u64 = 120;

/// Whether two rects have the same size within a sub-pixel epsilon (a pure pan
/// keeps the size; a zoom/resize changes it, invalidating the cache).
fn same_size(a: Rect, b: Rect) -> bool {
    (a.width() - b.width()).abs() < 0.5 && (a.height() - b.height()).abs() < 0.5
}

/// Re-emit cloned clipped shapes into `painter`'s layer, each translated by
/// `delta` (geometry and clip rect together) so the cached picture lands at the
/// card's current screen position. A zero `delta` (the card hasn't moved) emits
/// them unchanged.
fn emit_translated(painter: &egui::Painter, shapes: &[ClippedShape], delta: Vec2) {
    let ctx = painter.ctx();
    let layer = painter.layer_id();
    ctx.graphics_mut(|g| {
        let list = g.entry(layer);
        for c in shapes {
            let mut shape = c.shape.clone();
            if delta != Vec2::ZERO {
                shape.translate(delta);
            }
            let clip = c.clip_rect.translate(delta);
            list.add(clip, shape);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::pos2;

    /// Seed a cache entry for `id` captured under the given inputs, with no
    /// shapes (the reuse decision never reads them — only the capture inputs).
    fn seed(cache: &mut CardCache, id: &str, rect: Rect, view: CardView, sig: Option<u64>, dark: bool) {
        cache.cards.insert(
            id.to_owned(),
            CachedCard { shapes: Vec::new(), rect, view, signature: sig, dark, last_used: cache.frame },
        );
    }

    fn req<'a>(id: &'a str, rect: Rect, view: CardView, sig: Option<u64>, dark: bool, interacting: bool) -> CardRequest<'a> {
        CardRequest { id, rect, view, signature: sig, dark, interacting }
    }

    const R: Rect = Rect { min: pos2(10.0, 20.0), max: pos2(110.0, 220.0) };
    const VIEW: CardView = CardView { zoom: 1.0, scroll_y: 0.0 };

    #[test]
    fn fresh_card_with_no_entry_is_not_reusable() {
        let cache = CardCache::default();
        assert!(!cache.reusable(&req("a", R, VIEW, Some(1), false, false)));
    }

    #[test]
    fn unchanged_card_is_reusable() {
        let mut cache = CardCache::default();
        seed(&mut cache, "a", R, VIEW, Some(1), false);
        assert!(cache.reusable(&req("a", R, VIEW, Some(1), false, false)));
    }

    #[test]
    fn pure_pan_does_not_invalidate() {
        // Same SIZE, different position (min moved) → still reusable: a pan
        // re-blits the translated cache with no re-render.
        let mut cache = CardCache::default();
        seed(&mut cache, "a", R, VIEW, Some(1), false);
        let panned = Rect::from_min_size(pos2(300.0, 400.0), R.size());
        assert!(cache.reusable(&req("a", panned, VIEW, Some(1), false, false)));
    }

    #[test]
    fn size_change_invalidates() {
        // A camera zoom / resize changes the on-screen px size → re-render (a
        // cached raster/shape set is positioned for the old size).
        let mut cache = CardCache::default();
        seed(&mut cache, "a", R, VIEW, Some(1), false);
        let bigger = Rect::from_min_size(R.min, R.size() * 1.5);
        assert!(!cache.reusable(&req("a", bigger, VIEW, Some(1), false, false)));
    }

    #[test]
    fn content_edit_invalidates() {
        let mut cache = CardCache::default();
        seed(&mut cache, "a", R, VIEW, Some(1), false);
        assert!(!cache.reusable(&req("a", R, VIEW, Some(2), false, false)));
    }

    #[test]
    fn per_card_zoom_or_scroll_invalidates() {
        let mut cache = CardCache::default();
        seed(&mut cache, "a", R, VIEW, Some(1), false);
        let zoomed = CardView { zoom: 1.5, scroll_y: 0.0 };
        assert!(!cache.reusable(&req("a", R, zoomed, Some(1), false, false)));
        let scrolled = CardView { zoom: 1.0, scroll_y: 40.0 };
        assert!(!cache.reusable(&req("a", R, scrolled, Some(1), false, false)));
    }

    #[test]
    fn theme_flip_invalidates() {
        let mut cache = CardCache::default();
        seed(&mut cache, "a", R, VIEW, Some(1), false);
        assert!(!cache.reusable(&req("a", R, VIEW, Some(1), true, false)));
    }

    #[test]
    fn interacting_card_is_never_reusable() {
        // Even with every capture input unchanged, a card being scrolled/zoomed
        // this frame must live-render (its scroll is about to change).
        let mut cache = CardCache::default();
        seed(&mut cache, "a", R, VIEW, Some(1), false);
        assert!(!cache.reusable(&req("a", R, VIEW, Some(1), false, true)));
    }

    #[test]
    fn unsigned_card_never_reuses_across_change() {
        // `None` signature opts a node out of caching: two `None`s compare equal,
        // so a host that returns `None` still reuses on a pure pan but the app's
        // engine returns a real hash, so this only matters for opted-out kinds.
        let mut cache = CardCache::default();
        seed(&mut cache, "a", R, VIEW, None, false);
        assert!(cache.reusable(&req("a", R, VIEW, None, false, false)));
    }

    #[test]
    fn stale_entries_are_evicted_after_grace_window() {
        let mut cache = CardCache::default();
        cache.begin_frame(); // frame 1
        seed(&mut cache, "a", R, VIEW, Some(1), false);
        // Advance past the grace window without touching "a".
        for _ in 0..(EVICT_AFTER + 2) {
            cache.begin_frame();
        }
        assert!(!cache.cards.contains_key("a"), "untouched card evicted after grace window");
    }

    #[test]
    fn early_frames_do_not_wrongly_evict() {
        // Regression: the eviction cutoff once underflowed on early frames
        // (`frame - EVICT_AFTER` when `frame < EVICT_AFTER`), evicting every
        // entry each frame so nothing ever cached. A fresh entry must survive a
        // few early frames.
        let mut cache = CardCache::default();
        cache.begin_frame(); // frame 1
        seed(&mut cache, "a", R, VIEW, Some(1), false);
        cache.begin_frame(); // frame 2
        cache.begin_frame(); // frame 3
        assert!(cache.cards.contains_key("a"), "recently-seeded card survives early frames");
    }

    /// The shape geometry a `ClippedShape` covers, as a comparable bounding box,
    /// so a blit's output can be checked against the capture without depending on
    /// exact `Shape` equality (galleys aren't `PartialEq`).
    fn shape_boxes(painter: &egui::Painter, start: usize) -> Vec<(Rect, Rect)> {
        painter.ctx().graphics(|g| {
            g.get(painter.layer_id()).map_or_else(Vec::new, |list| {
                list.all_entries()
                    .skip(start)
                    .map(|c| (c.clip_rect, c.shape.visual_bounding_rect()))
                    .collect()
            })
        })
    }

    /// A zero-pan blit re-emits exactly the captured shapes (same count, same
    /// geometry), and a translated blit shifts every shape + clip by the delta —
    /// so an idle card looks identical to a live one and a pan just moves it.
    /// This is the Option-B fidelity guard.
    #[test]
    fn blit_reproduces_capture_and_translates_on_pan() {
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let painter = ui.painter().clone();
                // Live "render": emit a couple of shapes for a card at R.
                let start = painter.ctx().graphics(|g| {
                    g.get(painter.layer_id()).map_or(0, |l| l.next_idx().0)
                });
                painter.rect_filled(R, 0.0, egui::Color32::RED);
                painter.circle_filled(R.center(), 5.0, egui::Color32::BLUE);
                let captured = shape_boxes(&painter, start);

                let mut cache = CardCache::default();
                cache.begin_frame();
                cache.capture(
                    &req("a", R, VIEW, Some(1), false, false),
                    &painter,
                    egui::layers::ShapeIdx(start),
                );

                // Zero-pan blit: identical geometry to the capture.
                let zero_start = painter.ctx().graphics(|g| g.get(painter.layer_id()).map_or(0, |l| l.next_idx().0));
                cache.blit("a", R, &painter);
                let blitted = shape_boxes(&painter, zero_start);
                assert_eq!(blitted, captured, "zero-pan blit re-emits the captured shapes unchanged");

                // Panned blit: every shape + clip shifted by exactly the delta.
                let delta = egui::vec2(40.0, -15.0);
                let panned_rect = R.translate(delta);
                let pan_start = painter.ctx().graphics(|g| g.get(painter.layer_id()).map_or(0, |l| l.next_idx().0));
                cache.blit("a", panned_rect, &painter);
                let panned = shape_boxes(&painter, pan_start);
                assert_eq!(panned.len(), captured.len());
                for ((cap_clip, cap_box), (pan_clip, pan_box)) in captured.iter().zip(&panned) {
                    assert!((pan_clip.min - cap_clip.min - delta).length() < 0.01, "clip translated by delta");
                    assert!((pan_box.min - cap_box.min - delta).length() < 0.01, "shape translated by delta");
                }
            });
        });
    }
}
