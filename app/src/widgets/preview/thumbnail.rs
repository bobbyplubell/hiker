//! The inline thumbnail widget — a small fixed rect showing the cached small
//! preview texture, that registers a hover-expand request when pointed at.
//!
//! Domain-agnostic: it takes a `&dyn ThumbnailProvider` and never knows whether
//! it's drawing a canvas or a cluster-tree. The provider owns rendering; this
//! file owns the rect allocation, the cache↔texture round-trip, and stashing
//! the [`HoverRequest`] for the post-sidebar expanded draw. The trait + key
//! types it implements live in the module root (`mod.rs`,
//! `status: preview-thumbnail-provider`).

use std::path::Path;

use eframe::egui;

use super::cache::{self, PreviewCache};
use super::{PreviewKey, ThumbnailOpts, ThumbnailProvider};

/// Draw a small preview thumbnail for `provider` and register a hover-expand
/// request when hovered. `vault_root` roots the on-disk cache; pass the open
/// vault's root. Returns the allocated [`egui::Response`] so the caller can
/// compose it (the click is the caller's to interpret — usually "open").
pub(crate) fn thumbnail(
    ui: &mut egui::Ui,
    provider: &dyn ThumbnailProvider,
    vault_root: &Path,
    opts: ThumbnailOpts,
) -> egui::Response {
    let side = opts.side;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(side, side), egui::Sense::hover());

    let small_key = provider.cache_key();
    let dpr = ui.ctx().pixels_per_point();
    // Render the small thumbnail at the physical pixel size of the rect so it's
    // crisp on hi-dpi without over-rendering.
    let small_px = (side * dpr).round().max(1.0) as u32;
    let cache = PreviewCache::new(vault_root);

    if let Some(tex) = resolve_texture(ui.ctx(), &cache, provider, small_key, small_px) {
        let img = egui::Image::new(&tex)
            .fit_to_exact_size(rect.size())
            .sense(egui::Sense::hover());
        ui.put(rect, img);
    } else {
        // Neutral placeholder: a faint rounded rect, so the row layout is
        // stable even when a render fails.
        ui.painter().rect_filled(
            rect.shrink(1.0),
            2.0,
            ui.visuals().widgets.noninteractive.weak_bg_fill,
        );
    }

    if response.hovered() {
        register_hover(ui, &cache, provider, small_key, opts.large_px, rect);
    }

    response
}

/// Load `key` from the cache (rendering + storing on a miss) and upload it as an
/// egui texture. `None` when the provider can't render.
fn resolve_texture(
    ctx: &egui::Context,
    cache: &PreviewCache,
    provider: &dyn ThumbnailProvider,
    key: PreviewKey,
    px: u32,
) -> Option<egui::TextureHandle> {
    let img = load_or_render(cache, provider, key, px)?;
    Some(ctx.load_texture(
        format!("preview-small-{:016x}-{}", key.content_hash, key.size),
        egui::ColorImage::from_rgba_unmultiplied([img.width as usize, img.height as usize], &img.rgba),
        egui::TextureOptions::LINEAR,
    ))
}

/// Cache-or-render a preview image for `key` at `px`. On a cache miss the
/// provider renders, the result is persisted, and the pixels are returned. The
/// `size` bucket on `key` is overwritten to `px` so small and large share one
/// path but key distinct cache entries.
fn load_or_render(
    cache: &PreviewCache,
    provider: &dyn ThumbnailProvider,
    mut key: PreviewKey,
    px: u32,
) -> Option<cache::CachedImage> {
    key.size = px;
    if let Some(hit) = cache.load(key) {
        return Some(hit);
    }
    let img = provider.render(px)?;
    cache.store(key, &img);
    Some(cache::CachedImage {
        width: img.width(),
        height: img.height(),
        rgba: img.into_raw(),
    })
}

/// Register the EXPANDED hover preview for `provider` anchored at `anchor`,
/// WITHOUT drawing an inline thumbnail. For rows that already show their own
/// icon + label and only want the hover-expand (canvas rows in the context
/// panel — they read as a plain canvas icon until hovered). The caller gates on
/// its row `Response::hovered`. status: preview-hover-expand-side-anchor
pub(crate) fn register_hover_only(
    ui: &egui::Ui,
    provider: &dyn ThumbnailProvider,
    vault_root: &Path,
    anchor: egui::Rect,
    opts: ThumbnailOpts,
) {
    let cache = PreviewCache::new(vault_root);
    register_hover(ui, &cache, provider, provider.cache_key(), opts.large_px, anchor);
}

/// On hover: ensure the LARGE preview is rendered + cached (so the post-sidebar
/// expanded draw finds it), then stash the [`HoverRequest`] in egui memory. The
/// `hover_started` timestamp is preserved across consecutive hover frames so the
/// debounce hold measures one uninterrupted hover, and reset when the hovered
/// key changes (the pointer moved to a different thumbnail).
fn register_hover(
    ui: &egui::Ui,
    cache: &PreviewCache,
    provider: &dyn ThumbnailProvider,
    small_key: PreviewKey,
    large_px: u32,
    anchor: egui::Rect,
) {
    let ctx = ui.ctx();
    let now = ctx.input(|i| i.time);
    let id = super::hover_request_id();

    let mut large_key = small_key;
    large_key.size = large_px;
    // A provider that live-paints its expanded preview (canvas) hands back a
    // thunk; the cached large image is then irrelevant for it. Only warm the
    // large cache entry for the cached-image path (trees).
    let live_paint = provider.expanded_paint();
    if live_paint.is_none() {
        // Warm the large cache entry now (cheap on a hit) so
        // `render_expanded_preview` can load it once the hold elapses.
        let _ = load_or_render(cache, provider, large_key, large_px);
    }

    // Preserve the hover-start time while the pointer stays on the SAME
    // thumbnail; restart it when the key changes.
    let prev = ctx.data(|d| d.get_temp::<super::HoverRequest>(id));
    let hover_started = match prev {
        Some(p) if p.key == large_key => p.hover_started,
        _ => now,
    };

    ctx.data_mut(|d| {
        d.insert_temp(
            id,
            super::HoverRequest {
                key: large_key,
                anchor,
                hover_started,
                written_at: now,
                live_paint,
            },
        );
    });
}
