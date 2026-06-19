//! Right-click overflow toggle + inset wheel-scroll for rendered pipe tables
//! (`widget-table-overflow-scroll`).
//!
//! A table renders as a click-only `BlockWidget`; this module adds the two new
//! interactions the overflow escape hatch needs, both host-side (the editor-core
//! widget stays interaction-agnostic — it only emits a whole-widget click zone
//! keyed on its `content_hash`, exactly like a body-click target):
//!
//! - **Right-click → Fit ⇄ Scrollable.** Hit-test the secondary click against
//!   the table's whole-widget click zone (mirroring [`chart_under_right_click`])
//!   to resolve the table under the pointer, then offer the toggle as an item in
//!   the editor's existing `context_menu` (egui allows only one per response, so
//!   it folds into the clipboard menu the way the chart "Open in builder" item
//!   does). A checkmark marks the current mode.
//! - **Inset wheel-scroll.** When the pointer hovers a Scrollable table, feed the
//!   frame's vertical (or shift-horizontal) wheel delta into that table's inset
//!   `h_offset`, clamped to `[0, natural − inset]`. The editor itself never
//!   scrolls horizontally — the overflow is confined to the table's inset.
//!
//! [`chart_under_right_click`]: super::chart_under_right_click

use eframe::egui;

use super::widgets::tables::{TableOverflow, TableOverflowTarget, TableViewMap, TableViewState};

/// The table under a secondary (right) click, if any, resolved from this frame's
/// table whole-widget click zones (`widget-table-overflow-scroll`). Mirrors
/// `chart_under_right_click`: on a secondary click, hit-test the global pointer
/// against the table zones and stash the resolved [`TableOverflowTarget`] (or
/// `None`) in egui temp memory keyed by `menu_id`, so the choice persists while
/// the menu is open and self-corrects on each right-click; then return the
/// currently-stashed target. Independent of the editor response's `Sense`.
pub fn table_under_right_click(
    ctx: &egui::Context,
    editor_rect: egui::Rect,
    zones: &[editor_view::viewport::ClickZone],
    targets: &std::collections::HashMap<u64, TableOverflowTarget>,
    menu_id: egui::Id,
) -> Option<TableOverflowTarget> {
    let (secondary, pos) =
        ctx.input(|i| (i.pointer.secondary_clicked(), i.pointer.interact_pos()));
    if secondary {
        let hit = pos.filter(|p| editor_rect.contains(*p)).and_then(|p| {
            let (lx, ly) = (p.x - editor_rect.min.x, p.y - editor_rect.min.y);
            zones.iter().find_map(|z| match z.action {
                editor_view::viewport::ClickAction::WidgetClick(id) if z.rect.contains(lx, ly) => {
                    targets.get(&id).copied()
                }
                _ => None,
            })
        });
        ctx.data_mut(|d| d.insert_temp(menu_id, hit));
    }
    ctx.data(|d| d.get_temp::<Option<TableOverflowTarget>>(menu_id)).flatten()
}

/// Render the Fit ⇄ Scrollable toggle for `target` inside an already-open
/// `context_menu` `ui`, returning the table's start + the mode the user picked
/// (or `None` if they didn't pick). The caller applies the result to
/// `buffer.table_overflow` after the editor borrow ends. A radio dot marks the
/// table's current mode (read from `views`, default Fit).
/// status: widget-table-overflow-scroll
pub fn menu_items(
    ui: &mut egui::Ui,
    target: TableOverflowTarget,
    views: &TableViewMap,
) -> Option<(usize, TableOverflow)> {
    let current = views.get(&target.byte_start).map_or(TableOverflow::Fit, |v| v.mode);
    let mut chosen = None;
    ui.label("Table overflow");
    if ui
        .selectable_label(current == TableOverflow::Fit, "Fit (wrap to width)")
        .clicked()
    {
        chosen = Some((target.byte_start, TableOverflow::Fit));
        ui.close();
    }
    if ui
        .selectable_label(current == TableOverflow::Scrollable, "Scrollable (natural width)")
        .clicked()
    {
        chosen = Some((target.byte_start, TableOverflow::Scrollable));
        ui.close();
    }
    ui.separator();
    chosen
}

/// Apply a chosen overflow mode to the per-table map: switching to Fit drops the
/// entry (Fit is the default-absent state, and Fit ignores `h_offset` so the
/// stale offset shouldn't linger); switching to Scrollable inserts / updates the
/// entry, preserving any existing scroll offset. status: widget-table-overflow-scroll
pub fn apply_mode(views: &mut TableViewMap, byte_start: usize, mode: TableOverflow) {
    match mode {
        TableOverflow::Fit => {
            views.remove(&byte_start);
        }
        TableOverflow::Scrollable => {
            let entry = views.entry(byte_start).or_default();
            entry.mode = TableOverflow::Scrollable;
        }
    }
}

/// Feed this frame's wheel delta into the Scrollable table under the pointer
/// (`widget-table-overflow-scroll`). Hit-tests the pointer against the table
/// whole-widget zones; if the hovered table is Scrollable, advances its
/// `h_offset` by the wheel's horizontal component (shift-wheel or a trackpad's
/// x-scroll) plus its vertical component (so a plain wheel over a wide table
/// pans it — the editor's own vertical scroll still happens, this just adds
/// horizontal panning inside the inset), clamped to `[0, natural − inset]`. The
/// editor never scrolls horizontally; the offset lives entirely in the table's
/// inset. Returns true when an offset changed (the caller requests a repaint).
pub fn scroll_hovered(
    ctx: &egui::Context,
    editor_rect: egui::Rect,
    inset_width: f32,
    zones: &[editor_view::viewport::ClickZone],
    targets: &std::collections::HashMap<u64, TableOverflowTarget>,
    views: &mut TableViewMap,
) -> bool {
    let (delta, pos) = ctx.input(|i| (i.raw_scroll_delta, i.pointer.interact_pos()));
    // egui reports scroll as content-translation (positive = content moves down /
    // right); panning right through the columns means INCREASING the offset, so
    // negate. Prefer the explicit horizontal component; fall back to the vertical
    // wheel so a plain mouse wheel over a wide table still pans it.
    let pan = if delta.x.abs() > f32::EPSILON { -delta.x } else { -delta.y };
    if pan.abs() < f32::EPSILON {
        return false;
    }
    let Some(p) = pos.filter(|p| editor_rect.contains(*p)) else {
        return false;
    };
    let (lx, ly) = (p.x - editor_rect.min.x, p.y - editor_rect.min.y);
    let Some(target) = zones.iter().find_map(|z| match z.action {
        editor_view::viewport::ClickAction::WidgetClick(id) if z.rect.contains(lx, ly) => {
            targets.get(&id).copied()
        }
        _ => None,
    }) else {
        return false;
    };
    // Only a Scrollable table consumes the pan (a Fit table has no inset).
    let Some(state) = views.get(&target.byte_start).copied() else {
        return false;
    };
    if state.mode != TableOverflow::Scrollable {
        return false;
    }
    let max_off = (target.natural_width - inset_width.max(1.0)).max(0.0);
    let next = (state.h_offset + pan).clamp(0.0, max_off);
    if (next - state.h_offset).abs() < f32::EPSILON {
        return false;
    }
    views.insert(target.byte_start, TableViewState { mode: TableOverflow::Scrollable, h_offset: next });
    true
}
