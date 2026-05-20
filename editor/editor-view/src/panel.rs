//! Panel framework: backend-neutral descriptors for thin horizontal strips
//! (e.g. status bar, find panel) docked above or below the text-editing area.
//!
//! SPEC §9.21, IMPLEMENTATION §16.6.13.
//!
//! The renderer (e.g. `editor-egui`) walks [`PanelStack::panels`] each frame
//! and paints each [`Panel`] based on its [`PanelKind`]. The widget reserves
//! the corresponding vertical space at the top / bottom of its rect before
//! computing the text-area geometry.

use smol_str::SmolStr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelPlacement {
    Top,
    Bottom,
}

/// Backend-neutral description of a panel. The painter knows how to draw
/// `PanelKind` variants; extensions can register them on `PanelStack`.
#[derive(Clone, Debug)]
pub struct Panel {
    pub id: u64,
    pub placement: PanelPlacement,
    pub height: f32,
    pub kind: PanelKind,
}

#[derive(Clone, Debug)]
pub enum PanelKind {
    /// First-class search panel (the egui adapter knows how to draw it
    /// using the SearchState from ViewState).
    Search,
    /// Generic text label panel (e.g. status bar). Future variants for
    /// custom egui-painted panels can be added here.
    Label(SmolStr),
}

#[derive(Clone, Debug, Default)]
pub struct PanelStack {
    pub panels: Vec<Panel>,
}

impl PanelStack {
    /// Sum panel heights by placement so the widget can compute the
    /// text-area rect: `(top_total, bottom_total)`.
    pub fn heights(&self) -> (f32, f32) {
        let mut top = 0.0f32;
        let mut bottom = 0.0f32;
        for p in &self.panels {
            match p.placement {
                PanelPlacement::Top => top += p.height,
                PanelPlacement::Bottom => bottom += p.height,
            }
        }
        (top, bottom)
    }
}
