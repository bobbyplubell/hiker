//! Clusters feature module — feature-registry-first home for the
//! cluster-trees workflow (sidebar tree picker + node browser, the
//! cluster-review tab body, and the per-feature UI state). Migrated out
//! of `panels::cluster_review` + `sidebar::clusters` as part of Feature
//! registry Phase 1 (`feature-cluster-migration`).
//!
//! Layout:
//!
//! - `state.rs` — `State` (was `state::ClusterUiState`). Owned
//!   by `AppState::clusters_state`.
//! - `sidebar/` — sidebar mode body (tree picker, node tree, inline
//!   rename, DnD reparent, stage queue forms, advanced params).
//! - `panel/` — cluster-review center-pane tab body.
//!
//! The `Clusters` zero-sized type at the bottom implements
//! `crate::feature::Feature` so the registry can dispatch sidebar / icon /
//! label generically. Center-pane dispatch still routes through the
//! legacy `TabKind::ClusterReview` path until Phase 2/3 migrates that
//! consumer.

pub mod panel;
pub mod sidebar;
pub mod state;

use eframe::egui;

use crate::feature::{Ctx, Feature, SidebarSurface};
use crate::icons;

/// Zero-sized `Feature` impl for the Clusters feature. Pure descriptor:
/// holds no state. The real state lives in
/// `AppState::clusters_state`; the sidebar surface reaches it via
/// `Ctx::state.downcast_mut::<State>()` and routes broad effects
/// (open a tab / open a note) through `Ctx::defer`.
pub struct Clusters;

impl Feature for Clusters {
    fn id(&self) -> &'static str {
        "clusters"
    }
    fn label(&self) -> &'static str {
        "Clusters"
    }
    fn icon(&self) -> egui::Image<'static> {
        icons::ICONS.image(icons::Icon::ClusterTree)
    }
    fn sidebar(&self) -> Option<&dyn SidebarSurface> {
        Some(&ClustersSidebar)
    }
}

struct ClustersSidebar;

impl SidebarSurface for ClustersSidebar {
    fn render(&self, ui: &mut egui::Ui, ctx: &mut Ctx<'_>) {
        egui::ScrollArea::vertical()
            .id_salt("panel-clusters-body")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                sidebar::render_body(ui, ctx);
            });
    }
}
