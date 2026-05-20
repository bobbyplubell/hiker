//! Activity bar — vertical icon strip on the side, mode switcher.
//!
//! Implements `SPEC.md` §1. The activity bar lives in a fixed-width
//! `egui::SidePanel` on the leading edge of the window. Each item is
//! an icon button: clicking toggles the side bar visibility AND
//! selects that mode. Right-click opens a host-extensible context menu.

use std::hash::Hash;

use egui::{
    Align2, Color32, CursorIcon, FontId, Rect, Sense, Stroke, StrokeKind, TextStyle, Vec2, vec2,
};

use crate::behavior::WorkbenchBehavior;
use crate::side_bar::SideBarSide;
use crate::tab::DocumentTab;
use crate::theme::WorkbenchTheme;

/// One entry in the activity bar.
pub struct ActivityItem<Mode> {
    pub mode: Mode,
    /// Optional icon. When `None`, the activity bar paints the first
    /// letter of `label` as a fallback glyph.
    pub icon: Option<egui::Image<'static>>,
    pub label: String,
    pub badge: Option<ActivityBadge>,
}

/// Small overlay rendered on top of an activity item.
pub enum ActivityBadge {
    /// Unobtrusive coloured dot.
    Dot,
    /// Numeric badge (capped to "99+" if larger).
    Count(usize),
    /// Arbitrary short text (3–4 chars max).
    Text(String),
}

/// Vertical icon strip bound to the host's `Mode` type.
pub struct ActivityBar<Mode> {
    pub(crate) items: Vec<ActivityItem<Mode>>,
    pub(crate) hidden: Vec<Mode>,
    pub(crate) active: Option<Mode>,
    pub(crate) visible: bool,
    pub(crate) side: SideBarSide,
    /// User-preferred order of activity modes. When non-empty, the
    /// bar reorders the host-supplied items to match this list before
    /// rendering. Modes not present here are appended at the end in
    /// host order.
    pub(crate) order: Vec<Mode>,
}

impl<Mode> Default for ActivityBar<Mode> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            hidden: Vec::new(),
            active: None,
            visible: true,
            side: SideBarSide::Left,
            order: Vec::new(),
        }
    }
}

impl<Mode: Clone + Eq + Hash + 'static> ActivityBar<Mode> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Currently selected mode (if any).
    pub fn active(&self) -> Option<&Mode> {
        self.active.as_ref()
    }

    /// Whether the activity bar itself is shown.
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    pub fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
    }

    /// Programmatically select an activity. Pass `None` to clear.
    pub fn set_active(&mut self, mode: Option<Mode>) {
        self.active = mode;
    }

    pub fn set_side(&mut self, side: SideBarSide) {
        self.side = side;
    }
}

/// Outcome of a single activity-bar frame. Communicated back to the
/// caller (the `Workbench`) so it can act on user interactions —
/// toggling the side bar visibility, updating focus, etc.
pub(crate) struct ActivityBarResponse<Mode> {
    /// User clicked the activity item with this mode. The workbench
    /// toggles side bar visibility OR swaps the active activity.
    pub clicked: Option<Mode>,
}

impl<Mode> Default for ActivityBarResponse<Mode> {
    fn default() -> Self {
        Self { clicked: None }
    }
}

/// Render the activity bar inside the given `Ui`. Returns the
/// interaction outcome for the workbench to act on.
pub(crate) fn show_activity_bar<Tab, Mode, B>(
    bar: &mut ActivityBar<Mode>,
    ui: &mut egui::Ui,
    theme: &WorkbenchTheme,
    behavior: &mut B,
) -> ActivityBarResponse<Mode>
where
    Tab: DocumentTab,
    Mode: Clone + Eq + Hash + 'static,
    B: WorkbenchBehavior<Tab, Mode> + ?Sized,
{
    let mut items = behavior.activity_items();
    // Drop any items whose mode the user has chosen to hide.
    items.retain(|it| !bar.hidden.iter().any(|m| m == &it.mode));
    // If the user has reordered items in a previous frame, apply that
    // permutation to the host-supplied list. Items whose mode isn't in
    // `bar.order` keep their host-side relative position at the tail.
    if !bar.order.is_empty() {
        let mut sorted: Vec<ActivityItem<Mode>> = Vec::with_capacity(items.len());
        for mode in &bar.order {
            if let Some(pos) = items.iter().position(|it| &it.mode == mode) {
                sorted.push(items.remove(pos));
            }
        }
        sorted.extend(items);
        items = sorted;
    }
    bar.items = items;

    let mut response = ActivityBarResponse::default();
    let size = theme.activity_item_size;
    let item_padding = (theme.activity_bar_width - size).max(0.0) / 2.0;
    let item_h = size + 8.0;

    // Drag state lives in egui memory so it spans frames:
    // - `drag_src`  — the index of the item being dragged.
    // - `drag_grip` — pointer offset within the item at drag start, so
    //   the floating ghost tracks the cursor where the user grabbed it
    //   rather than snapping to its centre.
    let drag_src_id = ui.id().with("egui_workbench::activity_drag_src");
    let drag_grip_id = ui.id().with("egui_workbench::activity_drag_grip");

    ui.vertical(|ui| {
        // No vertical gaps between items so the bar reads as a single
        // continuous strip and the drag-shift maths line up to the pixel.
        ui.spacing_mut().item_spacing = Vec2::ZERO;
        ui.add_space(item_padding);

        let count = bar.items.len();
        if count == 0 {
            return;
        }

        // Pass 1: allocate every item's slot up front so we know the
        // strip's first-item top and can compute the drag target slot
        // from the pointer before painting.
        let mut slots: Vec<(Rect, egui::Response)> = Vec::with_capacity(count);
        for _ in 0..count {
            let (rect, resp) = ui.allocate_exact_size(
                vec2(theme.activity_bar_width, item_h),
                Sense::click_and_drag(),
            );
            slots.push((rect, resp));
        }

        // Drag-start detection: stash src + grip on the frame a drag
        // begins on any item.
        let pointer_pos = ui.input(|i| i.pointer.hover_pos().or(i.pointer.interact_pos()));
        for (idx, (rect, resp)) in slots.iter().enumerate() {
            if resp.drag_started() {
                let grip = pointer_pos.map(|p| p.y - rect.top()).unwrap_or(item_h / 2.0);
                ui.memory_mut(|m| {
                    m.data.insert_temp::<usize>(drag_src_id, idx);
                    m.data.insert_temp::<f32>(drag_grip_id, grip);
                });
            }
        }

        let drag_src: Option<usize> = ui.memory(|m| m.data.get_temp(drag_src_id));
        let drag_grip: f32 = ui
            .memory(|m| m.data.get_temp::<f32>(drag_grip_id))
            .unwrap_or(item_h / 2.0);

        // While a drag is in flight, compute the slot the cursor wants
        // to land in. Items between the source and the target shift to
        // make room (live rearrange — same feel as the activity bar in
        // a typical IDE).
        let first_top = slots[0].0.top();
        let target_idx: Option<usize> = match (drag_src, pointer_pos) {
            (Some(_), Some(p)) => {
                let raw = ((p.y - first_top) / item_h).floor();
                Some((raw.max(0.0) as usize).min(count - 1))
            }
            _ => None,
        };

        // Pass 2: paint each non-source item at its (possibly shifted)
        // visual position. Hit-testing still uses the original rect so
        // hover / click behaviour doesn't fight the animation.
        for (idx, slot) in slots.iter().enumerate() {
            let (rect, item_response) = (slot.0, slot.1.clone());
            let (mode, label) = {
                let item = &bar.items[idx];
                (item.mode.clone(), item.label.clone())
            };
            let is_active = bar.active.as_ref() == Some(&mode);

            let shift = match (drag_src, target_idx) {
                (Some(src), Some(tgt)) if idx != src => {
                    if src < tgt && idx > src && idx <= tgt {
                        -item_h
                    } else if src > tgt && idx < src && idx >= tgt {
                        item_h
                    } else {
                        0.0
                    }
                }
                _ => 0.0,
            };
            let visual_rect = rect.translate(vec2(0.0, shift));

            // Source item is painted as a floating ghost (below); skip
            // its in-strip slot so the gap reads as the drop target.
            let is_source = drag_src == Some(idx);
            if !is_source && ui.is_rect_visible(visual_rect) {
                let visuals = ui.style().interact(&item_response);
                let painter = ui.painter().clone();
                paint_activity_item(
                    ui,
                    &painter,
                    visual_rect,
                    bar.side,
                    is_active,
                    item_response.hovered(),
                    visuals,
                    bar.items[idx].icon.clone(),
                    &label,
                    size,
                    theme.accent,
                    1.0,
                );

                if let Some(badge) = bar.items[idx].badge.as_ref() {
                    paint_badge(ui, visual_rect, badge, theme.accent);
                }
            }

            let item_response = item_response.on_hover_cursor(CursorIcon::PointingHand);
            let item_response = if !label.is_empty() && drag_src.is_none() {
                item_response.on_hover_text(&label)
            } else {
                item_response
            };

            if item_response.clicked() {
                response.clicked = Some(mode.clone());
            }

            // Context menu is only useful when not in the middle of a
            // drag — suppress while dragging to avoid a flicker.
            if drag_src.is_none() {
                let mode_for_menu = mode.clone();
                let mut hide_this = false;
                item_response.context_menu(|ui| {
                    if ui.button("Hide").clicked() {
                        hide_this = true;
                        ui.close();
                    }
                    ui.separator();
                    behavior.activity_context_menu(ui, &mode_for_menu);
                });
                if hide_this {
                    if !bar.hidden.iter().any(|m| m == &mode) {
                        bar.hidden.push(mode.clone());
                    }
                    tracing::debug!("workbench: activity item hidden");
                }
            }
        }

        // Floating ghost: paint the source item on a foreground layer
        // tracking the pointer. Translucent so it's clearly "in motion".
        if let (Some(src), Some(p)) = (drag_src, pointer_pos)
            && src < count
        {
            let ghost_top = p.y - drag_grip;
            let ghost_rect = Rect::from_min_size(
                egui::pos2(slots[src].0.left(), ghost_top),
                vec2(theme.activity_bar_width, item_h),
            );
            let layer = egui::LayerId::new(
                egui::Order::Tooltip,
                ui.id().with("egui_workbench::activity_drag_ghost"),
            );
            let ghost_painter = ui.ctx().layer_painter(layer);
            let (mode, label) = {
                let item = &bar.items[src];
                (item.mode.clone(), item.label.clone())
            };
            let is_active = bar.active.as_ref() == Some(&mode);
            let visuals = ui.visuals().widgets.hovered;
            paint_activity_item(
                ui,
                &ghost_painter,
                ghost_rect,
                bar.side,
                is_active,
                /* hovered */ true,
                &visuals,
                bar.items[src].icon.clone(),
                &label,
                size,
                theme.accent,
                0.85,
            );

            // Drop-indicator bar at the edge of the target slot so the
            // user gets a precise insertion cue in addition to the
            // shifted items.
            if let Some(tgt) = target_idx {
                let tgt_rect = slots[tgt].0;
                let y = if tgt >= src {
                    tgt_rect.bottom() - 1.0
                } else {
                    tgt_rect.top()
                };
                ghost_painter.line_segment(
                    [
                        egui::pos2(tgt_rect.left() + 2.0, y),
                        egui::pos2(tgt_rect.right() - 2.0, y),
                    ],
                    Stroke::new(2.0, theme.accent),
                );
            }
        }

        // Commit the reorder on pointer release.
        let pointer_released = ui.input(|i| i.pointer.any_released());
        if pointer_released {
            if let (Some(s), Some(t)) = (drag_src, target_idx)
                && s != t
                && s < bar.items.len()
                && t < bar.items.len()
            {
                let item = bar.items.remove(s);
                bar.items.insert(t, item);
                bar.order = bar.items.iter().map(|it| it.mode.clone()).collect();
                tracing::debug!(from = s, to = t, "workbench: activity item reordered");
            }
            ui.memory_mut(|m| {
                m.data.remove::<usize>(drag_src_id);
                m.data.remove::<f32>(drag_grip_id);
            });
        } else if drag_src.is_some() {
            // Repaint continuously while a drag is in flight so the
            // ghost follows the cursor smoothly without waiting for
            // unrelated input.
            ui.ctx().request_repaint();
        }
    });

    response
}

/// Paint a single activity item (accent rail, background, icon-or-glyph)
/// into the given rect using the supplied painter. Factored out so the
/// floating drag-ghost can share the exact same visual treatment as the
/// in-strip rendering.
#[allow(clippy::too_many_arguments)]
fn paint_activity_item(
    ui: &egui::Ui,
    painter: &egui::Painter,
    rect: Rect,
    side: SideBarSide,
    is_active: bool,
    hovered: bool,
    visuals: &egui::style::WidgetVisuals,
    icon: Option<egui::Image<'static>>,
    label: &str,
    size: f32,
    accent: Color32,
    opacity: f32,
) {
    let accent_col = accent.gamma_multiply(opacity);
    // Leading-edge accent rail when active.
    if is_active {
        let accent_x = match side {
            SideBarSide::Left => rect.left() + 1.5,
            SideBarSide::Right => rect.right() - 1.5,
        };
        painter.line_segment(
            [
                egui::pos2(accent_x, rect.top() + 4.0),
                egui::pos2(accent_x, rect.bottom() - 4.0),
            ],
            Stroke::new(2.0, accent_col),
        );
    }

    if hovered || is_active {
        let bg = if is_active {
            visuals.bg_fill.gamma_multiply(0.4 * opacity)
        } else {
            visuals.bg_fill.gamma_multiply(0.2 * opacity)
        };
        painter.rect(
            rect.shrink(2.0),
            2.0,
            bg,
            Stroke::NONE,
            StrokeKind::Inside,
        );
    }

    let icon_rect = Rect::from_center_size(rect.center(), Vec2::splat(size));
    let fg = if is_active {
        visuals.fg_stroke.color
    } else {
        visuals.text_color()
    };
    let fg = fg.gamma_multiply(opacity);
    if let Some(image) = icon {
        image.tint(fg).paint_at(ui, icon_rect);
    } else {
        let glyph = label
            .chars()
            .next()
            .unwrap_or('?')
            .to_uppercase()
            .next()
            .unwrap_or('?');
        let font = FontId::new(size * 0.55, TextStyle::Button.resolve(ui.style()).family);
        painter.text(rect.center(), Align2::CENTER_CENTER, glyph, font, fg);
    }
}

/// A `Painter` for activity bar badges. Badges are drawn on egui's
/// **foreground layer** rather than the panel's own painter so they
/// escape the side-top-panel frame's inner-margin clip. Without this,
/// the panel's `Frame` would clip the badge at `panel_right -
/// inner_margin` (typically 4-6 px), occluding the badge's right edge
/// even though `item_rect.right()` reaches the panel's outer edge.
fn badge_painter(ui: &egui::Ui) -> egui::Painter {
    let layer = egui::LayerId::new(
        egui::Order::Foreground,
        ui.id().with("egui_workbench::activity_badges"),
    );
    ui.ctx().layer_painter(layer)
}

fn paint_badge(ui: &egui::Ui, item_rect: Rect, badge: &ActivityBadge, accent: Color32) {
    let painter = badge_painter(ui);
    match badge {
        ActivityBadge::Dot => {
            // Dot at the icon's top-right corner. Pulled well inside
            // the activity bar so it stays visible even if the panel's
            // frame margin trims a few pixels.
            let center = item_rect.right_top() + vec2(-8.0, 8.0);
            painter.circle_filled(center, 3.5, accent);
        }
        ActivityBadge::Count(n) => {
            let text = if *n > 99 { "99+".to_string() } else { n.to_string() };
            paint_badge_pill(ui, &painter, item_rect, &text, accent);
        }
        ActivityBadge::Text(s) => {
            paint_badge_pill(ui, &painter, item_rect, s, accent);
        }
    }
}

/// Draw a pill-shaped badge in the top-right of the item rect. The
/// pill is positioned ENTIRELY INSIDE the item rect (right_inset=6,
/// top_inset=2) so it has comfortable clearance from the panel's
/// frame margin. Painted via the supplied foreground-layer `Painter`
/// so the side-top-panel's clip can't trim it.
fn paint_badge_pill(
    ui: &egui::Ui,
    painter: &egui::Painter,
    item_rect: Rect,
    text: &str,
    accent: Color32,
) {
    let font = FontId::new(10.0, TextStyle::Body.resolve(ui.style()).family);
    let galley = painter.layout_no_wrap(text.to_string(), font.clone(), Color32::WHITE);
    let pad = Vec2::new(3.0, 1.0);
    let size = galley.size() + pad * 2.0;
    let right_inset = 6.0_f32;
    let top_inset = 2.0_f32;
    let max = egui::pos2(
        item_rect.right() - right_inset,
        item_rect.top() + top_inset + size.y,
    );
    let min = max - size;
    let rect = Rect::from_min_max(min, max);
    painter.rect(
        rect,
        size.y / 2.0,
        accent,
        Stroke::NONE,
        StrokeKind::Inside,
    );
    painter.galley(rect.min + pad, galley, Color32::WHITE);
}
