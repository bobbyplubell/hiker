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

use crate::behavior::Host;
use crate::side_bar::Side;
use crate::tab::Document;
use crate::theme::Palette;

/// One entry in the activity bar.
pub struct Item<Mode> {
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
    pub(crate) items: Vec<Item<Mode>>,
    pub(crate) hidden: Vec<Mode>,
    pub(crate) active: Option<Mode>,
    pub(crate) visible: bool,
    pub(crate) side: Side,
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
            side: Side::Left,
            order: Vec::new(),
        }
    }
}

impl<Mode: Clone + Eq + Hash + 'static> ActivityBar<Mode> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Currently selected mode (if any).
    pub const fn active(&self) -> Option<&Mode> {
        self.active.as_ref()
    }

    /// Whether the activity bar itself is shown.
    pub const fn is_visible(&self) -> bool {
        self.visible
    }

    pub const fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
    }

    /// Programmatically select an activity. Pass `None` to clear.
    pub fn set_active(&mut self, mode: Option<Mode>) {
        self.active = mode;
    }

    pub const fn set_side(&mut self, side: Side) {
        self.side = side;
    }

    /// Modes currently filtered out of the strip.
    pub fn hidden(&self) -> &[Mode] {
        &self.hidden
    }

    /// Replace the hidden set wholesale (used by layout restore).
    pub fn set_hidden(&mut self, hidden: Vec<Mode>) {
        self.hidden = hidden;
    }

    /// Whether `mode` is currently hidden from the strip.
    pub fn is_hidden(&self, mode: &Mode) -> bool {
        self.hidden.iter().any(|m| m == mode)
    }

    /// Show or hide a single mode.
    pub fn set_mode_hidden(&mut self, mode: &Mode, hidden: bool) {
        if hidden {
            if !self.is_hidden(mode) {
                self.hidden.push(mode.clone());
            }
        } else {
            self.hidden.retain(|m| m != mode);
        }
    }

    /// Clear the hidden set so every host item shows again.
    pub fn show_all(&mut self) {
        self.hidden.clear();
    }

    /// User-preferred item order (empty = host order).
    pub fn order(&self) -> &[Mode] {
        &self.order
    }

    /// Replace the preferred order wholesale (used by layout restore).
    pub fn set_order(&mut self, order: Vec<Mode>) {
        self.order = order;
    }
}

/// Shared flag id: set true while an activity item is mid-drag so other
/// regions (the primary side bar) can light up as drop targets.
pub(crate) fn drag_active_id() -> egui::Id {
    egui::Id::new("egui_workbench::activity_drag_active")
}

/// Outcome of a single activity-bar frame. Communicated back to the
/// caller (the `Workbench`) so it can act on user interactions —
/// toggling the side bar visibility, updating focus, etc.
pub(crate) struct ActivityBarResponse<Mode> {
    /// User clicked the activity item with this mode. The workbench
    /// toggles side bar visibility OR swaps the active activity.
    pub clicked: Option<Mode>,
    /// User dragged this mode's item out of the strip and released it
    /// over the rest of the window. The workbench adds it as a new
    /// primary side-panel section (VSCode "drag a view into the
    /// sidebar"). Mutually exclusive with `clicked`.
    pub dropped_out: Option<Mode>,
}

impl<Mode> Default for ActivityBarResponse<Mode> {
    fn default() -> Self {
        Self { clicked: None, dropped_out: None }
    }
}

/// Per-frame render context for the activity bar. The frame's work is
/// split into a handful of `&mut self` methods so each can stay under
/// the cognitive-complexity budget while sharing state (the bar, the
/// theme, drag-memory ids, allocated slots) without a wide free
/// helper signature.
struct Render<'a, Tab, Mode, B>
where
    Tab: Document,
    Mode: Clone + Eq + Hash + 'static,
    B: Host<Tab, Mode> + ?Sized,
{
    bar: &'a mut ActivityBar<Mode>,
    theme: &'a Palette,
    behavior: &'a mut B,
    size: f32,
    item_padding: f32,
    item_h: f32,
    drag_src_id: egui::Id,
    drag_grip_id: egui::Id,
    /// Full host item list (mode + label), unfiltered by `hidden`.
    /// Drives the visibility checklist in the context menu.
    all_items: Vec<(Mode, String)>,
    response: ActivityBarResponse<Mode>,
    _doc: std::marker::PhantomData<Tab>,
}

impl<'a, Tab, Mode, B> Render<'a, Tab, Mode, B>
where
    Tab: Document,
    Mode: Clone + Eq + Hash + 'static,
    B: Host<Tab, Mode> + ?Sized,
{
    /// Resolve the host-supplied item list against `bar.hidden` /
    /// `bar.order` and store it back on the bar for this frame.
    fn refresh_items(&mut self) {
        let mut items = self.behavior.activity_items();
        self.all_items = items
            .iter()
            .map(|it| (it.mode.clone(), it.label.clone()))
            .collect();
        items.retain(|it| !self.bar.hidden.iter().any(|m| m == &it.mode));
        if !self.bar.order.is_empty() {
            let mut sorted: Vec<Item<Mode>> = Vec::with_capacity(items.len());
            for mode in &self.bar.order {
                if let Some(pos) = items.iter().position(|it| &it.mode == mode) {
                    sorted.push(items.remove(pos));
                }
            }
            sorted.extend(items);
            items = sorted;
        }
        self.bar.items = items;
    }

    /// Allocate one click-and-drag slot per item. Returned in order.
    fn allocate_slots(&self, ui: &mut egui::Ui) -> Vec<(Rect, egui::Response)> {
        let count = self.bar.items.len();
        let mut slots: Vec<(Rect, egui::Response)> = Vec::with_capacity(count);
        for _ in 0..count {
            let (rect, resp) = ui.allocate_exact_size(
                vec2(self.theme.activity_bar_width, self.item_h),
                Sense::click_and_drag(),
            );
            slots.push((rect, resp));
        }
        slots
    }

    /// On the frame a drag begins, stash the source index and the
    /// pointer's offset within the item so the floating ghost (below)
    /// tracks the cursor where the user grabbed it.
    fn capture_drag_start(
        &self,
        ui: &egui::Ui,
        slots: &[(Rect, egui::Response)],
        pointer_pos: Option<egui::Pos2>,
    ) {
        for (idx, (rect, resp)) in slots.iter().enumerate() {
            if resp.drag_started() {
                let grip = pointer_pos
                    .map(|p| p.y - rect.top())
                    .unwrap_or(self.item_h / 2.0);
                ui.memory_mut(|m| {
                    m.data.insert_temp::<usize>(self.drag_src_id, idx);
                    m.data.insert_temp::<f32>(self.drag_grip_id, grip);
                });
            }
        }
    }

    /// Live-rearrange shift for `idx` given the current drag state.
    fn shift_for(&self, idx: usize, drag_src: Option<usize>, target_idx: Option<usize>) -> f32 {
        match (drag_src, target_idx) {
            (Some(src), Some(tgt)) if idx != src => {
                if src < tgt && idx > src && idx <= tgt {
                    -self.item_h
                } else if src > tgt && idx < src && idx >= tgt {
                    self.item_h
                } else {
                    0.0
                }
            }
            _ => 0.0,
        }
    }

    /// Render a single in-strip item: visuals, badge, hover tooltip,
    /// context menu, and click handling.
    fn render_item(
        &mut self,
        ui: &mut egui::Ui,
        idx: usize,
        slot: &(Rect, egui::Response),
        drag_src: Option<usize>,
        target_idx: Option<usize>,
    ) {
        let (rect, item_response) = (slot.0, slot.1.clone());
        let (mode, label) = {
            let item = &self.bar.items[idx];
            (item.mode.clone(), item.label.clone())
        };
        let is_active = self.bar.active.as_ref() == Some(&mode);
        let shift = self.shift_for(idx, drag_src, target_idx);
        let visual_rect = rect.translate(vec2(0.0, shift));
        let is_source = drag_src == Some(idx);
        if !is_source && ui.is_rect_visible(visual_rect) {
            let visuals = ui.style().interact(&item_response);
            let painter = ui.painter().clone();
            paint_activity_item(
                ui,
                &painter,
                visual_rect,
                &ActivityItemPaint {
                    side: self.bar.side,
                    is_active,
                    hovered: item_response.hovered(),
                    visuals,
                    icon: self.bar.items[idx].icon.clone(),
                    label: &label,
                    size: self.size,
                    accent: self.theme.accent,
                    opacity: 1.0,
                },
            );
            if let Some(badge) = self.bar.items[idx].badge.as_ref() {
                badge.paint(ui, visual_rect, self.theme.accent);
            }
        }
        let item_response = item_response.on_hover_cursor(CursorIcon::PointingHand);
        let item_response = if !label.is_empty() && drag_src.is_none() {
            item_response.on_hover_text(&label)
        } else {
            item_response
        };
        if item_response.clicked() {
            self.response.clicked = Some(mode.clone());
        }
        if drag_src.is_none() {
            self.handle_context_menu(&item_response, &mode, &label);
        }
    }

    fn handle_context_menu(&mut self, item_response: &egui::Response, mode: &Mode, label: &str) {
        let mode_for_menu = mode.clone();
        let hide_label = if label.is_empty() {
            "Hide".to_string()
        } else {
            format!("Hide \"{label}\"")
        };
        item_response.context_menu(|ui| {
            if ui.button(hide_label).clicked() {
                self.bar.set_mode_hidden(&mode_for_menu, true);
                tracing::debug!("workbench: activity item hidden");
                ui.close();
            }
            ui.separator();
            self.visibility_checklist(ui);
            ui.separator();
            self.behavior.activity_context_menu(ui, &mode_for_menu);
        });
    }

    /// Render a checkbox per host item (checked = shown). Toggling a box
    /// flips the mode's membership in `bar.hidden`. Shared by the
    /// per-item context menu and the empty-strip background menu so the
    /// list — including hidden items — is reachable from either.
    fn visibility_checklist(&mut self, ui: &mut egui::Ui) {
        // Snapshot so we can iterate while mutating `bar.hidden`.
        let all = self.all_items.clone();
        for (mode, label) in &all {
            let mut shown = !self.bar.is_hidden(mode);
            let text = if label.is_empty() { "(unnamed)" } else { label.as_str() };
            if ui.checkbox(&mut shown, text).changed() {
                self.bar.set_mode_hidden(mode, !shown);
                tracing::debug!(shown, "workbench: activity item visibility toggled");
            }
        }
    }

    /// Floating ghost of the dragged item + drop-indicator at the
    /// pending target slot. Painted on the tooltip layer so it sits
    /// above the side-top panel's frame clip.
    fn paint_ghost(
        &self,
        ui: &egui::Ui,
        slots: &[(Rect, egui::Response)],
        drag_src: Option<usize>,
        target_idx: Option<usize>,
        drag_grip: f32,
        pointer_pos: Option<egui::Pos2>,
    ) {
        let count = self.bar.items.len();
        let (Some(src), Some(p)) = (drag_src, pointer_pos) else {
            return;
        };
        if src >= count {
            return;
        }
        let ghost_top = p.y - drag_grip;
        let ghost_rect = Rect::from_min_size(
            egui::pos2(slots[src].0.left(), ghost_top),
            vec2(self.theme.activity_bar_width, self.item_h),
        );
        let layer = egui::LayerId::new(
            egui::Order::Tooltip,
            ui.id().with("egui_workbench::activity_drag_ghost"),
        );
        let ghost_painter = ui.ctx().layer_painter(layer);
        let (mode, label) = {
            let item = &self.bar.items[src];
            (item.mode.clone(), item.label.clone())
        };
        let is_active = self.bar.active.as_ref() == Some(&mode);
        let visuals = ui.visuals().widgets.hovered;
        paint_activity_item(
            ui,
            &ghost_painter,
            ghost_rect,
            &ActivityItemPaint {
                side: self.bar.side,
                is_active,
                hovered: true,
                visuals: &visuals,
                icon: self.bar.items[src].icon.clone(),
                label: &label,
                size: self.size,
                accent: self.theme.accent,
                opacity: 0.85,
            },
        );
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
                Stroke::new(2.0, self.theme.accent),
            );
        }
    }

    /// On pointer release commit the reorder + clear drag memory.
    /// While the drag is in flight, request a repaint so the ghost
    /// follows the cursor smoothly.
    fn finish_drag(
        &mut self,
        ui: &egui::Ui,
        drag_src: Option<usize>,
        target_idx: Option<usize>,
        over_strip: bool,
    ) {
        let pointer_released = ui.input(|i| i.pointer.any_released());
        if pointer_released {
            if let Some(s) = drag_src.filter(|&s| s < self.bar.items.len()) {
                if over_strip {
                    // Released back inside the strip → reorder.
                    if let Some(t) = target_idx
                        && s != t
                        && t < self.bar.items.len()
                    {
                        let item = self.bar.items.remove(s);
                        self.bar.items.insert(t, item);
                        self.bar.order =
                            self.bar.items.iter().map(|it| it.mode.clone()).collect();
                        tracing::debug!(from = s, to = t, "workbench: activity item reordered");
                    }
                } else {
                    // Released over the rest of the window → the host adds
                    // it as a side-panel section.
                    self.response.dropped_out = Some(self.bar.items[s].mode.clone());
                    tracing::debug!("workbench: activity item dropped into panel area");
                }
            }
            ui.memory_mut(|m| {
                m.data.remove::<usize>(self.drag_src_id);
                m.data.remove::<f32>(self.drag_grip_id);
            });
        } else if drag_src.is_some() {
            ui.ctx().request_repaint();
        }
    }

    fn run(mut self, ui: &mut egui::Ui) -> ActivityBarResponse<Mode> {
        self.refresh_items();
        // Full strip rect: a drag released outside it is a "drop into the
        // panel" rather than a reorder.
        let strip_rect = ui.max_rect();
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing = Vec2::ZERO;
            ui.add_space(self.item_padding);
            let count = self.bar.items.len();
            if count > 0 {
                let slots = self.allocate_slots(ui);
                let pointer_pos =
                    ui.input(|i| i.pointer.hover_pos().or(i.pointer.interact_pos()));
                self.capture_drag_start(ui, &slots, pointer_pos);
                let drag_src: Option<usize> = ui.memory(|m| m.data.get_temp(self.drag_src_id));
                let drag_grip: f32 = ui
                    .memory(|m| m.data.get_temp::<f32>(self.drag_grip_id))
                    .unwrap_or(self.item_h / 2.0);
                // Publish whether a drag is in flight so the primary side
                // bar can highlight itself as a drop target this frame.
                ui.ctx()
                    .data_mut(|d| d.insert_temp(drag_active_id(), drag_src.is_some()));
                let first_top = slots[0].0.top();
                let over_strip = pointer_pos.is_some_and(|p| strip_rect.contains(p));
                // Only show a reorder target while the pointer is inside
                // the strip; outside, the ghost reads as "drop into panel".
                let target_idx: Option<usize> = match (drag_src, pointer_pos) {
                    (Some(_), Some(p)) if over_strip => {
                        let raw = ((p.y - first_top) / self.item_h).floor();
                        Some((raw.max(0.0) as usize).min(count - 1))
                    }
                    _ => None,
                };
                for (idx, slot) in slots.iter().enumerate() {
                    self.render_item(ui, idx, slot, drag_src, target_idx);
                }
                self.paint_ghost(ui, &slots, drag_src, target_idx, drag_grip, pointer_pos);
                self.finish_drag(ui, drag_src, target_idx, over_strip);
            }
            // Claim the remaining strip area for the visibility menu so a
            // right-click on empty space — including when every item is
            // hidden — still surfaces the checklist that restores items.
            let remaining = ui.available_size_before_wrap();
            if remaining.x > 0.0 && remaining.y > 0.0 {
                let bg = ui.allocate_response(remaining, Sense::click());
                bg.context_menu(|ui| self.visibility_checklist(ui));
            }
        });
        self.response
    }
}

impl<Mode: Clone + Eq + Hash + 'static> ActivityBar<Mode> {
    /// Render the activity bar inside the given `Ui`. Returns the
    /// interaction outcome for the workbench to act on. Thin façade
    /// over `Render::run`.
    pub(crate) fn show<Tab, B>(
        &mut self,
        ui: &mut egui::Ui,
        theme: &Palette,
        behavior: &mut B,
    ) -> ActivityBarResponse<Mode>
    where
        Tab: Document,
        B: Host<Tab, Mode> + ?Sized,
    {
        let size = theme.activity_item_size;
        let item_padding = (theme.activity_bar_width - size).max(0.0) / 2.0;
        let item_h = size + 8.0;
        let drag_src_id = ui.id().with("egui_workbench::activity_drag_src");
        let drag_grip_id = ui.id().with("egui_workbench::activity_drag_grip");
        let render = Render::<Tab, Mode, B> {
            bar: self,
            theme,
            behavior,
            size,
            item_padding,
            item_h,
            drag_src_id,
            drag_grip_id,
            all_items: Vec::new(),
            response: ActivityBarResponse::default(),
            _doc: std::marker::PhantomData,
        };
        render.run(ui)
    }
}

/// Appearance + content of one activity item, independent of where it
/// is painted. Bundles the item's side/state (`side`, `is_active`,
/// `hovered`), its content (`icon`, `label`), and its visual tuning
/// (`visuals`, `size`, `accent`, `opacity`) so both the in-strip render
/// and the floating drag-ghost can describe an item with one value.
struct ActivityItemPaint<'a> {
    side: Side,
    is_active: bool,
    hovered: bool,
    visuals: &'a egui::style::WidgetVisuals,
    icon: Option<egui::Image<'static>>,
    label: &'a str,
    size: f32,
    accent: Color32,
    opacity: f32,
}

/// Paint a single activity item (accent rail, background, icon-or-glyph)
/// into the given rect using the supplied painter. Factored out so the
/// floating drag-ghost can share the exact same visual treatment as the
/// in-strip rendering.
fn paint_activity_item(
    ui: &egui::Ui,
    painter: &egui::Painter,
    rect: Rect,
    item: &ActivityItemPaint<'_>,
) {
    let ActivityItemPaint {
        side,
        is_active,
        hovered,
        visuals,
        icon,
        label,
        size,
        accent,
        opacity,
    } = item;
    let (side, is_active, hovered, size, accent, opacity) =
        (*side, *is_active, *hovered, *size, *accent, *opacity);
    let accent_col = accent.gamma_multiply(opacity);
    // Leading-edge accent rail when active.
    if is_active {
        let accent_x = match side {
            Side::Left => rect.left() + 1.5,
            Side::Right => rect.right() - 1.5,
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
        image.clone().tint(fg).paint_at(ui, icon_rect);
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

impl ActivityBadge {
    /// Paint this badge on top of `item_rect`. Badges go on egui's
    /// **foreground layer** rather than the panel's own painter so
    /// they escape the side-top-panel frame's inner-margin clip.
    /// Without this, the panel's `Frame` would clip the badge at
    /// `panel_right - inner_margin` (typically 4-6 px), occluding the
    /// badge's right edge even though `item_rect.right()` reaches the
    /// panel's outer edge.
    fn paint(&self, ui: &egui::Ui, item_rect: Rect, accent: Color32) {
        let layer = egui::LayerId::new(
            egui::Order::Foreground,
            ui.id().with("egui_workbench::activity_badges"),
        );
        let painter = ui.ctx().layer_painter(layer);
        match self {
            ActivityBadge::Dot => {
                // Dot at the icon's top-right corner. Pulled well
                // inside the activity bar so it stays visible even if
                // the panel's frame margin trims a few pixels.
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
