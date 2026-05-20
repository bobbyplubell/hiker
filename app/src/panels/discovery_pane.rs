//! Legacy discovery-pane module — its composite renderer is gone after
//! step 4 (Search / Related / Backlinks / Chat each became standalone
//! dockable panels). This module survives only as a home for the shared
//! `collapsible_header` helper used by all three sub-panels and the
//! historical `ChatDockState` field still referenced by `PanelStates`.

use eframe::egui;

/// Per-`PanelStates` slot retained for compatibility with old layouts /
/// the toolbar's "new chat" affordance. The dock now hosts chat as its
/// own panel; this struct is essentially unused but is cheap to keep so
/// existing call sites compile.
#[derive(Default)]
#[allow(dead_code)]
pub struct ChatDockState {
    pub chat_collapsed: bool,
}

/// Render a collapsible section header. Returns true if the user
/// clicked the header (caller flips the persisted state). Optional
/// `count` is appended as `(N)` so users see the size at a glance.
pub(crate) fn collapsible_header(
    ui: &mut egui::Ui,
    id: &str,
    title: &str,
    expanded: bool,
    count: usize,
) -> bool {
    let glyph = if expanded { "v" } else { ">" };
    let label = if count > 0 {
        format!("{glyph} {title} ({count})")
    } else {
        format!("{glyph} {title}")
    };
    let resp = ui
        .scope(|ui| {
            ui.add(
                egui::Label::new(egui::RichText::new(label).strong())
                    .sense(egui::Sense::click()),
            )
        })
        .inner;
    let _ = id;
    resp.clicked()
}
