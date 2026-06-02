//! The floating live edit-preview overlay (`widget-edit-popup-preview`).
//!
//! When the main cursor is inside a math or mermaid widget's *revealed* source
//! span — the state where the decoration providers in [`super`] show the raw
//! source and suppress the in-place rendered widget — this module floats a
//! small, non-interactive preview of the rendered result near that span and
//! keeps it live as the user types. It dismisses the instant the cursor leaves
//! every span (no revealed span → no popup), and shows exactly one popup at a
//! time: the span [`super::active_preview_span`] picks (the one containing the
//! main caret, or — for inline math, whose reveal is per-line — the nearest span
//! on the caret's line).
//!
//! Anchoring is scroll-correct by the same mechanism `wikilink_nav` uses: the
//! span's last source line has an editor-relative y from
//! `ViewState::line_top_y`, which already folds in `scroll_y`; adding the editor
//! body's screen rect origin lands it in screen space, so the popup tracks the
//! span as the buffer scrolls. The overlay is painted in a foreground egui
//! `Area` with interaction sensing off — it never steals focus or the caret.
//!
//! Re-rasterization is avoided by caching the last render keyed on the same
//! `content_hash` `render.rs` computes (`widget-render-cache`): the cache key is
//! recomputed each frame *without* rendering (via `math_content_hash` /
//! `mermaid_content_hash`), so a static span (source unchanged while the user
//! holds the caret in it) reuses the uploaded texture and pays nothing.
//!
//! status: widget-edit-popup-preview

use eframe::egui;
use editor_core::state::Editor as EditorState;
use editor_core::theme::Theme;
use editor_view::viewport::ViewState;

use super::{ActivePreviewSpan, PreviewKind, render};

/// Gap (logical px) between the bottom of the span's last line and the top of
/// the floating preview, so the popup doesn't touch the text being edited.
const ANCHOR_GAP: f32 = 6.0;
/// Padding inside the preview frame around the rendered texture.
const FRAME_PAD: f32 = 6.0;
/// Hard cap on the popup's painted size (logical px) so a large diagram can't
/// cover the whole editor; the texture letterboxes into this box.
const MAX_W: f32 = 520.0;
const MAX_H: f32 = 360.0;

/// One cached render for the live overlay. At most one popup is up at a time, so
/// a single slot suffices. `key` is the render's `content_hash`; while it
/// matches the current span+inputs the [`egui::TextureHandle`] is reused with no
/// re-raster. `size` is the texture's logical size (physical px / dpr).
#[derive(Default)]
pub struct Cache {
    key: Option<u64>,
    texture: Option<egui::TextureHandle>,
    size: egui::Vec2,
    /// Anchor (`inner_range.start`) of the span the user dismissed with Escape
    /// (`widget-edit-popup-dismiss`). While the active span's anchor matches
    /// this, the popup stays hidden though the source remains revealed for
    /// editing; it re-shows once the caret leaves the span (active span clears)
    /// or moves to a different span (anchor differs). Set by the buffer panel's
    /// Escape handler via [`dismiss`](Cache::dismiss).
    pub dismissed_anchor: Option<usize>,
}

impl Cache {
    fn clear(&mut self) {
        self.key = None;
        self.texture = None;
        self.size = egui::Vec2::ZERO;
        // Leaving every span (or gating off) re-arms the popup: a later
        // re-entry shows it again. status: widget-edit-popup-dismiss
        self.dismissed_anchor = None;
    }

    /// Record that the popup at `anchor` was dismissed with Escape.
    /// status: widget-edit-popup-dismiss
    pub const fn dismiss(&mut self, anchor: usize) {
        self.dismissed_anchor = Some(anchor);
    }
}

/// The render inputs the overlay needs, mirroring the decoration providers'
/// per-frame values: the active editor + view geometry, the editor body's
/// screen rect, theme, body font size, device pixel ratio, and the
/// `render_widgets && is_markdown` gate.
pub struct PreviewInputs<'a> {
    pub state: &'a EditorState,
    pub view: &'a ViewState,
    pub editor_rect: egui::Rect,
    pub theme: Option<&'a Theme>,
    pub font_px: f32,
    pub dpr: f32,
    /// `render_widgets && is_markdown` — the same gate the providers apply.
    pub gated: bool,
}

/// Paint the live edit-preview overlay if the main cursor reveals a math /
/// mermaid span, else paint nothing (and drop the cached texture).
///
/// Anchoring is scroll-correct: `view.line_top_y` is editor-relative (already
/// folds in `scroll_y`), so adding `editor_rect`'s origin lands the popup in
/// screen space and it tracks the span through scrolling.
/// status: widget-edit-popup-preview
pub fn show(cache: &mut Cache, ctx: &egui::Context, inputs: &PreviewInputs<'_>) {
    if !inputs.gated {
        cache.clear();
        return;
    }
    let viewport = inputs.view.visible_lines();
    let Some(active) = super::active_preview_span(inputs.state, Some(&viewport)) else {
        cache.clear();
        return;
    };

    // Escape-dismissed (`widget-edit-popup-dismiss`): while the caret stays in
    // the dismissed span, keep the popup hidden (source still shows for
    // editing). A different span (or none) re-arms it.
    if cache.dismissed_anchor == Some(active.inner_range.start) {
        return;
    }
    cache.dismissed_anchor = None;

    let src = inputs.state.doc.to_string();
    let inner = &src[active.inner_range.clone()];
    if !ensure_render(cache, ctx, &active, inner, inputs) {
        // Render failed (parse / unsupported) → no popup, just the source.
        cache.clear();
        return;
    }
    let (Some(texture), size) = (cache.texture.as_ref(), cache.size) else {
        return;
    };

    // Scroll-correct screen y for the bottom of the span's last source line.
    let line_bottom_rel = inputs.view.line_top_y(active.anchor_line + 1);
    let anchor_y = inputs.editor_rect.min.y + line_bottom_rel + ANCHOR_GAP;
    let anchor_x = inputs.editor_rect.min.x + 8.0;

    paint_area(
        ctx,
        inputs.editor_rect,
        texture,
        size,
        egui::pos2(anchor_x, anchor_y),
    );
    // Keep animating while a popup is up so a scroll / edit re-anchors promptly.
    ctx.request_repaint();
}

/// The anchor (`inner_range.start`) of a popup that is *currently visible* and
/// not yet Escape-dismissed, or `None` if there's no such popup. The buffer
/// panel calls this on an Escape press to decide whether to consume the key
/// (dismissing the popup) or let it fall through to the editor: `Some(anchor)`
/// → consume + `cache.dismiss(anchor)`; `None` → a second Escape (already
/// dismissed) or no popup, so Escape reaches the editor. `gated` is the same
/// `render_widgets && live_edit_preview && is_markdown` gate `show` receives.
/// status: widget-edit-popup-dismiss
pub fn dismissible_anchor(
    cache: &Cache,
    state: &EditorState,
    view: &ViewState,
    gated: bool,
) -> Option<usize> {
    if !gated {
        return None;
    }
    let viewport = view.visible_lines();
    let anchor = super::active_preview_span(state, Some(&viewport))?.inner_range.start;
    // Already dismissed → don't re-consume; let Escape pass to the editor.
    if cache.dismissed_anchor == Some(anchor) {
        return None;
    }
    Some(anchor)
}

/// Ensure `cache` holds the texture for `active`'s current source. Returns
/// `false` if the render fails (caller paints nothing). On a cache hit (the
/// content hash is unchanged) this neither renders nor rasterizes.
fn ensure_render(
    cache: &mut Cache,
    ctx: &egui::Context,
    active: &ActivePreviewSpan,
    inner: &str,
    inputs: &PreviewInputs<'_>,
) -> bool {
    let (theme, font_px, dpr) = (inputs.theme, inputs.font_px, inputs.dpr);
    // Compute the would-be content hash WITHOUT rendering so a static span
    // reuses the cached texture (`widget-render-cache`).
    let key = match active.kind {
        // Inline `$…$` previews in display style — more legible than the
        // compact inline form when shown in its own box.
        PreviewKind::InlineMath | PreviewKind::DisplayMath => {
            let fg = super::theme_fg(theme);
            render::math_content_hash(inner, render::MathKind::Display, font_px, dpr, fg)
        }
        PreviewKind::Mermaid => {
            render::mermaid_content_hash(inner, font_px, dpr, super::theme_mermaid_colors(theme))
        }
        PreviewKind::WaveDrom => {
            render::wavedrom_content_hash(inner, font_px, dpr, super::theme_wavedrom_colors(theme))
        }
    };

    if cache.key == Some(key) && cache.texture.is_some() {
        return true; // hit — no render, no raster, no upload
    }

    let rendered: Option<render::RenderedWidget> = match active.kind {
        PreviewKind::InlineMath | PreviewKind::DisplayMath => render::render_math(
            inner,
            render::MathKind::Display,
            font_px,
            dpr,
            super::theme_fg(theme),
            "",
        ),
        PreviewKind::Mermaid => {
            render::render_mermaid(inner, font_px, dpr, super::theme_mermaid_colors(theme))
        }
        PreviewKind::WaveDrom => {
            render::render_wavedrom(inner, font_px, dpr, super::theme_wavedrom_colors(theme))
        }
    };
    let Some(rendered) = rendered else {
        return false;
    };

    let image = egui::ColorImage::from_rgba_unmultiplied(
        [rendered.width as usize, rendered.height as usize],
        &rendered.rgba,
    );
    let handle = ctx.load_texture(
        format!("edit-preview-{key:016x}"),
        image,
        egui::TextureOptions::LINEAR,
    );
    cache.size = egui::vec2(rendered.width as f32 / dpr, rendered.height as f32 / dpr);
    cache.texture = Some(handle);
    cache.key = Some(key);
    true
}

/// Paint the cached texture in a non-interactive foreground `Area` anchored at
/// `anchor` (top-left), nudged to stay within `editor_rect` and on-screen, and
/// letterboxed into the size cap. status: widget-edit-popup-preview
fn paint_area(
    ctx: &egui::Context,
    editor_rect: egui::Rect,
    texture: &egui::TextureHandle,
    tex_size: egui::Vec2,
    anchor: egui::Pos2,
) {
    // Fit the texture into the size cap, preserving aspect.
    let scale = (MAX_W / tex_size.x.max(1.0))
        .min(MAX_H / tex_size.y.max(1.0))
        .min(1.0);
    let draw = tex_size * scale;
    let frame_size = draw + egui::vec2(FRAME_PAD, FRAME_PAD) * 2.0;

    // Clamp so the popup stays within the editor body horizontally and doesn't
    // run off the bottom (flip above the line if it would).
    let mut pos = anchor;
    pos.x = pos
        .x
        .min(editor_rect.max.x - frame_size.x - 4.0)
        .max(editor_rect.min.x + 4.0);
    if pos.y + frame_size.y > editor_rect.max.y - 4.0 {
        // Not enough room below: pull it up so its bottom rests near the editor
        // floor (still anchored to roughly the same span, just above the line).
        pos.y = (editor_rect.max.y - frame_size.y - 4.0).max(editor_rect.min.y + 4.0);
    }

    egui::Area::new(egui::Id::new("widget-edit-popup-preview"))
        .order(egui::Order::Tooltip)
        .interactable(false)
        .fixed_pos(pos)
        .show(ctx, |ui| {
            ui.set_max_size(frame_size);
            egui::Frame::popup(ui.style())
                .inner_margin(egui::Margin::same(FRAME_PAD as i8))
                .show(ui, |ui| {
                    ui.add(
                        egui::Image::new(texture)
                            .fit_to_exact_size(draw)
                            .sense(egui::Sense::hover()),
                    );
                });
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use editor_core::selection::Selection;

    /// status: widget-edit-popup-preview — the new `live_edit_preview` toggle
    /// folds into `gated`; when off, `show` is a no-op that drops the cache,
    /// even if the caret would otherwise reveal a span.
    #[test]
    fn gated_off_clears_cache_and_paints_nothing() {
        let ctx = egui::Context::default();
        let view = ViewState::default();
        let state = EditorState::new("$$\n\\int_0^1 x\\,dx\n$$\n");
        let mut cache = Cache {
            key: Some(7),
            texture: None,
            size: egui::Vec2::ZERO,
            dismissed_anchor: None,
        };
        let inputs = PreviewInputs {
            state: &state,
            view: &view,
            editor_rect: egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0)),
            theme: None,
            font_px: 15.0,
            dpr: 1.0,
            gated: false,
        };
        let _ = ctx.run(Default::default(), |ctx| {
            show(&mut cache, ctx, &inputs);
        });
        assert!(cache.key.is_none(), "gated-off drops the cached render");
    }

    /// status: widget-edit-popup-dismiss — `dismissible_anchor` reports the
    /// active span's anchor when a popup is up + gated, and `None` once that
    /// anchor is already dismissed (so a second Escape falls through).
    #[test]
    fn dismissible_anchor_reports_then_suppresses() {
        let view = ViewState::default();
        let src = "intro\n\n```mermaid\ngraph TD; A-->B\n```\n\nmore\n";
        let mut state = EditorState::new(src);
        state.selection = Selection::single(src.find("graph TD").unwrap());

        let mut cache = Cache::default();
        // Gated off → never dismissible.
        assert_eq!(dismissible_anchor(&cache, &state, &view, false), None);
        // Gated on, popup up → the active span's inner_range.start.
        let anchor = dismissible_anchor(&cache, &state, &view, true)
            .expect("a popup is up to dismiss");
        let span = editor_md::diagrams::mermaid_spans(&state, None)
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(anchor, span.inner_range.start);
        // After dismissing that anchor, it's no longer dismissible.
        cache.dismiss(anchor);
        assert_eq!(
            dismissible_anchor(&cache, &state, &view, true),
            None,
            "already dismissed → Escape falls through"
        );
    }

    /// status: widget-edit-popup-dismiss — a dismissed popup paints nothing
    /// while the caret stays in the span (texture not built), and re-arms once
    /// the caret leaves every span (`show` clears the dismissal).
    #[test]
    fn dismissed_popup_stays_hidden_then_rearms() {
        let ctx = egui::Context::default();
        let view = ViewState::default();
        let src = "intro\n\n```mermaid\ngraph TD; A-->B\n```\n\nmore\n";
        let mut state = EditorState::new(src);
        state.selection = Selection::single(src.find("graph TD").unwrap());
        let span = editor_md::diagrams::mermaid_spans(&state, None)
            .into_iter()
            .next()
            .unwrap();

        let mut cache = Cache::default();
        cache.dismiss(span.inner_range.start);
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0));
        let inputs = PreviewInputs {
            state: &state,
            view: &view,
            editor_rect: rect,
            theme: None,
            font_px: 15.0,
            dpr: 1.0,
            gated: true,
        };
        let _ = ctx.run(Default::default(), |ctx| show(&mut cache, ctx, &inputs));
        assert!(cache.texture.is_none(), "dismissed span builds no texture");
        assert_eq!(
            cache.dismissed_anchor,
            Some(span.inner_range.start),
            "dismissal persists while the caret stays in the span"
        );

        // Move the caret out of every span → `show` clears the dismissal.
        state.selection = Selection::single(src.find("intro").unwrap());
        let inputs = PreviewInputs {
            state: &state,
            view: &view,
            editor_rect: rect,
            theme: None,
            font_px: 15.0,
            dpr: 1.0,
            gated: true,
        };
        let _ = ctx.run(Default::default(), |ctx| show(&mut cache, ctx, &inputs));
        assert!(cache.dismissed_anchor.is_none(), "leaving the span re-arms the popup");
    }
}
