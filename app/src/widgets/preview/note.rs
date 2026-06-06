//! Read-only note hover-preview: a side-anchored popup rendering a live-preview
//! markdown view (WITH diagrams) of a note when a sidebar row is hovered.
//!
//! This is a SEPARATE mechanism from the canvas/tree thumbnail preview in
//! `mod.rs`. That path stashes an `Arc<dyn Fn(&mut Ui, Rect) -> bool>` thunk and
//! runs it in the post-sidebar frame loop with no `AppState` — fine for the flat
//! image renderers. A note preview cannot work that way: rendering a note with
//! diagrams needs `&mut AppState` (diagram cache, shared buffers, title
//! resolver — all `!Send`), so it can't live in a `Send + Sync` thunk. Instead
//! the request carries only the path + anchor, and the actual render runs at the
//! frame-loop level via [`render_note_preview`], where `&mut AppState` exists.
//!
//! The lifecycle mirrors the thumbnail one: a row's hover calls
//! [`register_note_hover`] (stashing a request under its OWN egui-memory id), and
//! [`render_note_preview`] runs once per frame after the sidebar, dropping a
//! stale request (pointer left the row) and otherwise drawing the popup once the
//! short hover-hold elapses.
//!
//! status: preview-note-hover

use std::cell::RefCell;

use eframe::egui;

use super::{expanded_area_min, EXPAND_HOLD_SECS};
use crate::buffer_view::{EmbedOpts, EmbeddedView};
use crate::state::AppState;

/// A pending note hover-preview, stashed in egui memory under
/// [`note_request_id`] during the sidebar render and consumed once after the
/// sidebar by [`render_note_preview`]. Unlike the thumbnail `HoverRequest`, it
/// carries no render thunk — only the note path + anchor, because the render
/// needs `&mut AppState` and happens at the frame-loop level.
#[derive(Clone)]
struct NotePreviewRequest {
    /// Vault-relative path of the note to preview.
    path: String,
    /// The hovered row's screen rect — the popup flows from here. Passive
    /// (sidebar) previews anchor to its RIGHT (`expanded_area_min`); interactive
    /// (wikilink) previews anchor just BELOW it, near the cursor
    /// (`below_anchor_min`).
    anchor: egui::Rect,
    /// When the current uninterrupted hover began. The popup only draws after a
    /// short hold; preserved across frames while the path is unchanged.
    hover_started: f64,
    /// `input.time` of the frame this request was written on. A request from a
    /// prior frame (the pointer left every row) is stale and dropped — which is
    /// what makes the preview vanish when the pointer moves off the row.
    written_at: f64,
    /// When true the popup is drawn in an INTERACTABLE area (so the embedded
    /// note view scrolls under the wheel) and anchored at the cursor; it stays
    /// alive while the pointer is over the popup itself, not just the source
    /// (the pill → card slide). The wikilink hover (`wikilink-hover-preview`)
    /// sets this; sidebar rows leave it `false` for the original passive,
    /// pointer-transparent, side-anchored preview.
    interactive: bool,
}

/// egui-memory id the note request lives under — distinct from the thumbnail
/// `"preview-hover-request"` so the two mechanisms never clobber each other.
fn note_request_id() -> egui::Id {
    egui::Id::new("preview-note-hover-request")
}

/// egui-memory id holding last frame's interactive-preview screen rect, used to
/// keep the popup alive while the pointer rests over it (the grace window that
/// lets the user slide cursor → card to scroll without it dismissing).
fn note_card_rect_id() -> egui::Id {
    egui::Id::new("preview-note-hover-card-rect")
}

/// Logical content size of the note preview popup (the live-preview render
/// viewport, before the popup frame padding).
const NOTE_PREVIEW_SIZE: egui::Vec2 = egui::vec2(320.0, 380.0);

/// Inner padding from the popup frame to the note render rect.
const NOTE_PREVIEW_PAD: f32 = 6.0;

/// Register a hover over `path`'s row, anchored at `anchor`. Call from a sidebar
/// row when it's hovered. Stashes a [`NotePreviewRequest`]; the `hover_started`
/// timestamp is preserved across consecutive hover frames on the SAME path (so
/// the hold measures one uninterrupted hover) and reset when the path changes
/// (the pointer moved to a different row).
pub(crate) fn register_note_hover(ui: &egui::Ui, anchor: egui::Rect, path: &str) {
    register(ui, anchor, path, false);
}

/// Like [`register_note_hover`], but the popup is INTERACTABLE (the embedded
/// note scrolls under the wheel) and anchored just below the source, near the
/// cursor — and it survives the pointer moving from the source onto the popup
/// itself. For the wikilink pill hover (`wikilink-hover-preview`), where the
/// user expects to slide onto the card and scroll it.
pub(crate) fn register_note_hover_interactive(ui: &egui::Ui, anchor: egui::Rect, path: &str) {
    register(ui, anchor, path, true);
}

/// Shared body of the two registration entry points. `hover_started` is
/// preserved across consecutive frames on the same path (so the hold measures
/// one uninterrupted hover) and reset when the path changes.
fn register(ui: &egui::Ui, anchor: egui::Rect, path: &str, interactive: bool) {
    let ctx = ui.ctx();
    let now = ctx.input(|i| i.time);
    let id = note_request_id();

    let prev = ctx.data(|d| d.get_temp::<NotePreviewRequest>(id));
    let hover_started = match prev {
        Some(p) if p.path == path => p.hover_started,
        _ => now,
    };

    ctx.data_mut(|d| {
        d.insert_temp(
            id,
            NotePreviewRequest {
                path: path.to_owned(),
                anchor,
                hover_started,
                written_at: now,
                interactive,
            },
        );
    });
}

thread_local! {
    /// The single live note-preview view (only one preview shows at a time). The
    /// `EmbeddedView` holds per-view scroll / paint / decoration caches and must
    /// persist across frames for the previewed note; it's recreated when the
    /// previewed path changes. Parked off `AppState` so [`render_note_preview`]
    /// can hold `&mut app` and `&mut embed` at once without aliasing — exactly
    /// the `show_file_edit` trick.
    static PREVIEW_EMBED: RefCell<Option<(String, EmbeddedView)>> = const { RefCell::new(None) };
}

/// Draw the one pending note preview, if any, AFTER the sidebar has rendered.
/// Called once per frame by the frame loop with `&mut AppState`. The passive
/// (sidebar) preview paints into a non-interactable `Order::Tooltip` `Area`, so
/// it never senses the pointer and never steals the row hover beneath it. The
/// interactive (wikilink) preview paints into an interactable area — so it
/// scrolls — and is held open while the pointer is over the card itself. A
/// no-op when nothing is hovered.
pub(crate) fn render_note_preview(ctx: &egui::Context, app: &mut AppState) {
    let id = note_request_id();
    let Some(mut req) = ctx.data(|d| d.get_temp::<NotePreviewRequest>(id)) else {
        return;
    };
    let now = ctx.input(|i| i.time);
    // Stale request: no source re-stashed it this frame (the pointer left every
    // pill / row). Normally that means drop it and draw nothing — the preview
    // disappears the instant the pointer moves off the source. The exception is
    // an interactive preview the pointer slid ONTO: keep it alive (the grace
    // window) so the user can scroll the card. status: wikilink-hover-preview
    if req.written_at < now {
        let kept = req.interactive && pointer_over_card(ctx);
        if !kept {
            ctx.data_mut(|d| {
                d.remove::<NotePreviewRequest>(id);
                d.remove::<egui::Rect>(note_card_rect_id());
            });
            return;
        }
        // Refresh the request so it survives into the next frame's staleness
        // check while the cursor stays on the card (hover_started preserved so
        // the hold doesn't reset).
        req.written_at = now;
        let refreshed = req.clone();
        ctx.data_mut(|d| d.insert_temp(id, refreshed));
    }
    // Keep the frame alive while hovered so the hold timer advances without
    // further input.
    ctx.request_repaint();
    if now - req.hover_started < EXPAND_HOLD_SECS {
        return;
    }

    PREVIEW_EMBED.with(|cell| {
        let mut slot = cell.borrow_mut();
        // Recreate the embed when the previewed note changes (its scroll / paint
        // caches are per-note).
        let needs_new = slot.as_ref().is_none_or(|(p, _)| p != &req.path);
        if needs_new {
            *slot = Some((req.path.clone(), EmbeddedView::new()));
        }
        let Some((_, embed)) = slot.as_mut() else {
            return;
        };
        paint_note_area(ctx, app, &req, embed);
    });
}

/// Place + paint the note preview's `Area`, rendering the note read-only into the
/// popup via [`crate::buffer_view::show_embedded_buffer`]. The passive case keeps
/// the non-interactable, side-anchored framing the sidebar previews use. The
/// interactive case (wikilink) anchors just below the source and runs in an
/// INTERACTABLE area so the embedded view scrolls under the wheel; its screen
/// rect is stashed so [`pointer_over_card`] can hold it open across the
/// cursor → card slide.
fn paint_note_area(
    ctx: &egui::Context,
    app: &mut AppState,
    req: &NotePreviewRequest,
    embed: &mut EmbeddedView,
) {
    let pad = NOTE_PREVIEW_PAD;
    let draw = NOTE_PREVIEW_SIZE;
    let frame = draw + egui::vec2(pad, pad) * 2.0;
    let min = if req.interactive {
        below_anchor_min(ctx, req.anchor, frame)
    } else {
        expanded_area_min(ctx, req.anchor, draw)
    };

    let resp = egui::Area::new(egui::Id::new("preview-note-hover"))
        .order(egui::Order::Tooltip)
        .interactable(req.interactive)
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
                    let opts = EmbedOpts {
                        read_only: true,
                        markdown: true,
                        font_size: 12.0,
                        focus: false,
                    };
                    crate::buffer_view::show_embedded_buffer(&mut child, app, &req.path, embed, &opts);
                });
        });

    // Remember the interactive card's screen rect so the grace window can keep
    // it open while the pointer rests on it (and a small slack so the slide off
    // the pill's bottom edge onto the card doesn't fall through the gap).
    if req.interactive {
        let card = resp.response.rect.expand(NOTE_GRACE_SLACK);
        ctx.data_mut(|d| d.insert_temp(note_card_rect_id(), card));
    }
}

/// Slack (logical px) added around the interactive card's rect for the grace
/// window, covering the small gap between the source pill and the card.
const NOTE_GRACE_SLACK: f32 = 6.0;

/// True when the pointer is over last frame's interactive-preview card rect —
/// the grace test that keeps a scrollable wikilink preview open as the cursor
/// slides off the pill onto the card.
fn pointer_over_card(ctx: &egui::Context) -> bool {
    let Some(rect) = ctx.data(|d| d.get_temp::<egui::Rect>(note_card_rect_id())) else {
        return false;
    };
    ctx.pointer_latest_pos().is_some_and(|p| rect.contains(p))
}

/// Top-left of the interactive preview's `Area`: anchored just BELOW the source
/// rect, left edges aligned, near the cursor. Flips ABOVE when there's no room
/// below, then clamps fully on-screen. `frame` is the popup's outer size
/// (content + padding).
fn below_anchor_min(ctx: &egui::Context, anchor: egui::Rect, frame: egui::Vec2) -> egui::Pos2 {
    let screen = ctx.screen_rect();
    let pad = NOTE_PREVIEW_PAD;
    let gap = 6.0;
    let mut min = egui::pos2(anchor.left(), anchor.bottom() + gap);
    // No room below → flip above the source if it fits there; otherwise clamp.
    if min.y + frame.y > screen.bottom() - pad {
        let above = anchor.top() - gap - frame.y;
        min.y = if above >= screen.top() + pad {
            above
        } else {
            (screen.bottom() - pad - frame.y).max(screen.top() + pad)
        };
    }
    min.x = min.x.clamp(screen.left() + pad, (screen.right() - pad - frame.x).max(screen.left() + pad));
    min
}
