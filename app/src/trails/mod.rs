//! Trails activity module — activity-registry-first home for the
//! trails workflow (sidebar trail picker + waypoint forest + per-activity
//! UI state). Migrated out of `sidebar::trails` as part of activity
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
//! zero-sized type at the bottom implements `crate::activity::Activity`
//! so the registry can dispatch sidebar / icon / label generically.

pub mod bridge;
pub mod sidebar;
pub mod state;

use eframe::egui;

use egui_workbench::activity::{Activity, View};
use crate::activity::AppCtx;
use crate::icons;

/// Zero-sized `Activity` impl for the Trails activity. Pure descriptor:
/// holds no state. The real state (the trail forest + sidebar UI state)
/// lives in `AppState::trails_state`; the sidebar surface reaches it via
/// `ctx.state.downcast_mut::<State>()` and routes broad effects (open a
/// note, the remove-confirm modal, the trail-doc write) through
/// `SurfaceCtx::defer`.
pub struct Trails;

impl Activity<dyn AppCtx> for Trails {
    fn id(&self) -> &'static str {
        "trails"
    }
    fn label(&self) -> &'static str {
        "Trails"
    }
    fn icon(&self) -> egui::Image<'static> {
        icons::ICONS.image(icons::Icon::Boot)
    }
    fn views(&self) -> Vec<&dyn View<dyn AppCtx>> {
        vec![&TrailsSidebar]
    }
}

struct TrailsSidebar;

impl View<dyn AppCtx> for TrailsSidebar {
    fn id(&self) -> &'static str {
        "trails"
    }
    fn render(&self, ui: &mut egui::Ui, ctx: &mut (dyn AppCtx + 'static)) {
        let Some(mut ctx) = ctx.surface_ctx(self.state_key()) else {
            return;
        };
        let ctx = &mut ctx;
        egui::ScrollArea::vertical()
            .id_salt("panel-trails-body")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                sidebar::TrailsCtx { ctx }.render(ui);
            });
    }
}
