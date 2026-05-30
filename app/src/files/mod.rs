//! Files feature module — feature-registry-first home for the filetree
//! sidebar (lazy directory tree + open / rename / move / duplicate /
//! delete verbs). Migrated out of `sidebar::files` as part of Feature
//! registry Phase 2 (`feature-filetree-migration`). Files is the primary
//! sidebar mode, so the `Files` descriptor is registered first.
//!
//! Layout:
//!
//! - `sidebar.rs` — the sidebar mode body: the lazy file tree rendered
//!   through the narrow `feature::Ctx`. Tree UI state lives in
//!   `AppState::file_tree_state` (reached via `Ctx::state`); directory
//!   listings via `Ctx::vault`; index markers via `Ctx::services`; the
//!   active-note highlight via `Ctx::active_path`. Broad mutations (open
//!   a note / board, drag-drop move, rename, duplicate, reindex,
//!   add-to-trail / set-active-trail, add-to-board, delete) are routed
//!   through `Ctx::defer`.
//!
//! The host (`workbench_host`) keeps the title-row `+` new-note / new-board
//! buttons and the `⋯` refresh / sort menu — those have full
//! `&mut AppState` and don't fit the narrow `Ctx`.
//!
//! Decoration snapshots: a few row decorations need data the narrow
//! `Ctx` deliberately doesn't carry — the dirty-buffer set
//! (`session.buffers`), the skipped-paths set (the vault-session event
//! channel), and the active-trail membership set (another feature's
//! state). Rather than grow `Ctx` with those reads or special-case the
//! generic sidebar consumer, the surface refreshes
//! `file_tree_state.deco` once per frame via a single deferred pre-pass
//! closure (`Ctx::defer`, which runs with full `&mut AppState`). The
//! render path then reads only that opaque snapshot — at most one frame
//! stale, which is cosmetically harmless for these markers.

pub mod sidebar;

use eframe::egui;

use crate::feature::{Ctx, Feature, SidebarSurface};
use crate::icons;

/// Zero-sized `Feature` impl for the Files feature. Pure descriptor:
/// holds no state. The real state (the file-tree UI state) lives in
/// `AppState::file_tree_state`; the sidebar surface reaches it via
/// `Ctx::state.downcast_mut::<FileTreeState>()` and routes broad effects
/// (open a note / board, move, rename, delete, ...) through `Ctx::defer`.
pub struct Files;

impl Feature for Files {
    fn id(&self) -> &'static str {
        "files"
    }
    fn label(&self) -> &'static str {
        "Files"
    }
    fn icon(&self) -> egui::Image<'static> {
        icons::ICONS.image(icons::Icon::Folder)
    }
    fn sidebar(&self) -> Option<&dyn SidebarSurface> {
        Some(&FilesSidebar)
    }
}

struct FilesSidebar;

impl SidebarSurface for FilesSidebar {
    fn render(&self, ui: &mut egui::Ui, ctx: &mut Ctx<'_>) {
        egui::ScrollArea::vertical()
            .id_salt("panel-files-body")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                sidebar::FilesCtx { ctx }.render(ui);
            });
    }
}
