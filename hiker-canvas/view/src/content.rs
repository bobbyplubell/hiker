//! The node-content seam that keeps this crate free of content-engine deps.
//!
//! Node *frames* (border, fill, selection handles) and *edges* are this crate's
//! job. Node *contents* — markdown for a text node, an embedded file, a web
//! page for a link node — are painted by a host-supplied [`NodeContentRenderer`]
//! the app injects. The adapter calls the host once per visible node, handing it
//! the node and the already-inset inner rectangle; the host dispatches on
//! [`Node::kind`] and paints with whatever engine it owns (`editor-egui`,
//! `hiker-htmlview`, …). This boundary is load-bearing: it is what lets the
//! content engine for any kind be an app-side change behind one trait while the
//! adapter and core never learn about markdown, images, or HTML.
//
// status: canvas-node-content-trait

use egui::{Align2, Color32, FontId, Rect};
use hiker_canvas::model::{Node, NodeKind};

/// Per-card view parameters handed to the content renderer each frame: how the
/// body should be sized and scrolled. Both are *decoupled from canvas zoom* —
/// a card is a fixed-size window into its content, not a thing that scales with
/// the camera. `zoom` is a per-card text/content scale (default `1.0`, adjusted
/// from the card's context menu); `scroll_y` is the vertical scroll offset in
/// content pixels. status: canvas-card-zoom, canvas-card-scroll
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CardView {
    /// Per-card content scale (font / embed sizing). Independent of camera zoom.
    pub zoom: f32,
    /// Vertical scroll offset of the card body, in content pixels.
    pub scroll_y: f32,
}

impl Default for CardView {
    fn default() -> Self {
        Self { zoom: 1.0, scroll_y: 0.0 }
    }
}

/// Paints the *content* of a canvas node inside its card.
///
/// Implemented by the host (the app's canvas panel). The view calls
/// [`NodeContentRenderer::render`] for each visible node with `inner` already
/// inset from the card border, so an implementation paints only the body and
/// never the frame, selection, or edges.
pub trait NodeContentRenderer {
    /// Paint `node`'s content within `inner`, sized and scrolled per `view`,
    /// using `ui`'s painter and clip rect.
    ///
    /// `inner` is the card's content rectangle in screen pixels (it tracks the
    /// camera so the card stays put spatially). [`CardView`] carries the
    /// per-card zoom + scroll, both *independent of camera zoom*, so the body
    /// is a readable, scrollable window rather than something that shrinks with
    /// the camera. The host must not paint outside `inner`.
    ///
    /// Returns the *effective* (clamped) vertical scroll the body settled on, so
    /// the view stores it back as the card's scroll state — scrollable bodies
    /// (the editor) clamp `view.scroll_y` to their content height; non-scrolling
    /// bodies echo it unchanged.
    fn render(&mut self, ui: &mut egui::Ui, node: &Node, inner: Rect, view: CardView) -> f32;
}

/// A trivial [`NodeContentRenderer`] that paints each node's id and kind as
/// plain text. It lets this crate be exercised (and the app start) before the
/// real markdown / embed / web renderers are wired behind the same trait.
#[derive(Debug, Default, Clone, Copy)]
pub struct DebugContentRenderer;

impl NodeContentRenderer for DebugContentRenderer {
    fn render(&mut self, ui: &mut egui::Ui, node: &Node, inner: Rect, view: CardView) -> f32 {
        if inner.width() < 1.0 || inner.height() < 1.0 {
            return view.scroll_y;
        }
        let painter = ui.painter().with_clip_rect(inner);
        let label = format!("{} [{}]", node.id, kind_label(&node.kind));
        let size = (13.0 * view.zoom).clamp(7.0, 22.0);
        let color = ui.visuals().text_color();
        painter.text(inner.left_top(), Align2::LEFT_TOP, label, FontId::monospace(size), color);
        if let NodeKind::Text { text } = &node.kind {
            paint_preview_body(&painter, inner, text, size, ui.visuals().weak_text_color());
        }
        view.scroll_y
    }
}

/// Paint a short text-node body preview below the id label.
fn paint_preview_body(painter: &egui::Painter, inner: Rect, text: &str, size: f32, color: Color32) {
    let body_top = inner.left_top() + egui::vec2(0.0, size + 4.0);
    if body_top.y >= inner.bottom() || text.is_empty() {
        return;
    }
    let galley = painter.layout(text.to_owned(), FontId::proportional(size), color, inner.width());
    painter.galley(body_top, galley, color);
}

/// A human-readable tag for a node kind, for the debug renderer.
const fn kind_label(kind: &NodeKind) -> &'static str {
    match kind {
        NodeKind::Text { .. } => "text",
        NodeKind::File { .. } => "file",
        NodeKind::Link { .. } => "link",
        NodeKind::Group { .. } => "group",
    }
}
