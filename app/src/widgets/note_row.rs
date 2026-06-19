//! Shared note-row gesture helpers (`docs/interaction.md`): the sticky-open
//! modifier branch every note-open site shares ([modclick-sticky]) and the
//! drag-source arming + floating ghost chip every note row shares
//! ([drag-note-payload]). An item kind's capability must not vary by which
//! list renders it, so these live here once — the files tree, search results,
//! backlinks, related, vault-view rows, and the git-diff panel all call the
//! same two functions instead of re-deciding the modifier or re-painting the
//! chip per surface.

use eframe::egui;

use hiker_theme as theme;

/// Whether a primary click should open STICKY — bypassing the preview-tab
/// slot — per `docs/interaction.md` [modclick-sticky]. The branch the
/// wikilink pills and search cards established: `Modifiers::command`
/// (Cmd on macOS, Ctrl elsewhere), with raw Ctrl accepted everywhere.
pub(crate) const fn open_sticky(modifiers: egui::Modifiers) -> bool {
    modifiers.command || modifiers.ctrl
}

/// Arm `resp` as a note drag source: payload is the vault-relative path
/// (`String`) — the files-tree convention every drop target (folders, board
/// lanes, canvas) already accepts — and a floating chip of `label` follows
/// the pointer while the row is dragged. The response must sense drag
/// (`Sense::click_and_drag()`); egui's drag threshold keeps `clicked()`
/// working unchanged.
pub(crate) fn note_drag_source(ui: &egui::Ui, resp: &egui::Response, rel: &str, label: &str) {
    resp.dnd_set_drag_payload::<String>(rel.to_string());
    if resp.dragged() {
        drag_ghost(ui, label);
    }
}

/// Paint the floating drag ghost: a chip of the dragged row's label at the
/// cursor so the item visibly "follows" the pointer (the dnd payload itself
/// is invisible). Drawn in the Tooltip layer so it floats above the source
/// list, the editor, and any drop target.
pub(crate) fn drag_ghost(ui: &egui::Ui, label: &str) {
    let Some(pointer) = ui.ctx().pointer_interact_pos() else {
        return;
    };
    let ghost = ui.ctx().layer_painter(egui::LayerId::new(
        egui::Order::Tooltip,
        egui::Id::new("note-drag-ghost"),
    ));
    let color = ui.style().visuals.text_color();
    let g = ui.fonts(|f| {
        f.layout_no_wrap(label.to_string(), egui::FontId::proportional(13.0), color)
    });
    let pad = egui::vec2(8.0, 4.0);
    // Offset down-right of the cursor so the pointer (and any drop
    // highlight under it) stays visible.
    let origin = pointer + egui::vec2(12.0, 8.0);
    let chip = egui::Rect::from_min_size(origin, g.size() + pad * 2.0);
    ghost.rect_filled(chip, 4.0, theme::active_bg().gamma_multiply(0.96));
    ghost.rect_stroke(
        chip,
        4.0,
        egui::Stroke::new(1.0, theme::divider()),
        egui::StrokeKind::Inside,
    );
    ghost.galley(origin + pad, g, color);
}

#[cfg(test)]
mod open_sticky_tests {
    use super::open_sticky;

    /// The platform command modifier (Cmd on macOS, Ctrl elsewhere) and raw
    /// Ctrl both mean "open sticky" — the branch wikilink pills established.
    #[test]
    fn command_or_ctrl_opens_sticky() {
        let command = egui::Modifiers { command: true, ..Default::default() };
        let ctrl = egui::Modifiers { ctrl: true, ..Default::default() };
        assert!(open_sticky(command));
        assert!(open_sticky(ctrl));
        assert!(open_sticky(egui::Modifiers { ctrl: true, command: true, ..Default::default() }));
    }

    /// A plain click — and clicks with unrelated modifiers — open into the
    /// preview slot, never sticky.
    #[test]
    fn plain_and_unrelated_modifiers_stay_preview() {
        assert!(!open_sticky(egui::Modifiers::default()));
        assert!(!open_sticky(egui::Modifiers { shift: true, ..Default::default() }));
        assert!(!open_sticky(egui::Modifiers { alt: true, ..Default::default() }));
    }
}
