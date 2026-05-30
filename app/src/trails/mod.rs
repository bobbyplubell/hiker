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

use crate::feature::{Feature, SidebarSurface};
use crate::feature::Ctx;
use crate::icons;
use crate::state::AppState;

/// Render entry point for the Trails sidebar body. Bridges the static
/// `panels_registry::DockPanel::render` `fn` pointer to the inherent
/// rendering on `TrailsView`.
pub fn render_sidebar(ui: &mut egui::Ui, app: &mut AppState) {
    egui::ScrollArea::vertical()
        .id_salt("panel-trails-body")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            sidebar::TrailsView { ui, state: app }.render();
        });
}

/// Zero-sized `Feature` impl for the Trails feature. Pure descriptor:
/// holds no state. The real state lives in `AppState::trails_state`;
/// surfaces reach it via `Ctx::state.downcast_mut::<State>()`.
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
        // v1 wraps the legacy free-fn body. Same shape as
        // `ClustersSidebar` — the downcast proves the wiring; the
        // legacy renderer is invoked via the panels_registry path.
        let _state = ctx
            .state
            .downcast_mut::<state::State>()
            .expect("TrailsSidebar invoked with the wrong state type");
        ui.weak("(trails sidebar — routed via panels_registry in v1)");
    }
}
