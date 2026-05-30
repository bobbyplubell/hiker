//! Trails feature module — feature-registry-first home for the
//! trails workflow (sidebar trail picker + waypoint forest + per-feature
//! UI state). Migrated out of `sidebar::trails` as part of Feature
//! registry Phase 2 (`feature-trails-migration`).
//!
//! Layout:
//!
//! - `state.rs` — `State` (was `state::TrailsUiState`). Owned
//!   by `AppState::trails_state`.
//! - `sidebar.rs` — sidebar mode body (trail picker, waypoint forest,
//!   drag-drop reparent, annotation editor, overflow menu).
//!
//! No panel surface — trails is sidebar-only in v1. The `Trails`
//! zero-sized type at the bottom implements `crate::feature::Feature`
//! so the registry can dispatch sidebar / icon / label generically.

pub mod sidebar;
pub mod state;

use eframe::egui;

use crate::feature::{Ctx, Feature, SidebarSurface};
use crate::icons;

/// Zero-sized `Feature` impl for the Trails feature. Pure descriptor:
/// holds no state. The real state (the trail forest + sidebar UI state)
/// lives in `AppState::trails_state`; the sidebar surface reaches it via
/// `Ctx::state.downcast_mut::<State>()` and routes broad effects (open a
/// note, the remove-confirm modal, the trail-doc write) through
/// `Ctx::defer`.
pub struct Trails;

impl Feature for Trails {
    fn id(&self) -> &'static str {
        "trails"
    }
    fn label(&self) -> &'static str {
        "Trails"
    }
    fn icon(&self) -> egui::Image<'static> {
        icons::ICONS.image(icons::Icon::Boot)
    }
    fn sidebar(&self) -> Option<&dyn SidebarSurface> {
        Some(&TrailsSidebar)
    }
}

struct TrailsSidebar;

impl SidebarSurface for TrailsSidebar {
    fn render(&self, ui: &mut egui::Ui, ctx: &mut Ctx<'_>) {
        egui::ScrollArea::vertical()
            .id_salt("panel-trails-body")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                sidebar::TrailsCtx { ctx }.render(ui);
            });
    }
}
