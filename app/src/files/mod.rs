//! Files activity module — activity-registry-first home for the filetree
//! sidebar (lazy directory tree + open / rename / move / duplicate /
//! delete verbs). Migrated out of `sidebar::files` as part of activity
//! registry Phase 2 (`feature-filetree-migration`). Files is the primary
//! sidebar mode, so the `Files` descriptor is registered first.
//!
//! Layout:
//!
//! - `sidebar.rs` — the sidebar mode body: the lazy file tree rendered
//!   through the narrow `activity::Ctx`. Tree UI state lives in
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
//! channel), and the active-trail membership set (another activity's
//! state). Rather than grow `Ctx` with those reads or special-case the
//! generic sidebar consumer, the surface refreshes
//! `file_tree_state.deco` once per frame via a single deferred pre-pass
//! closure (`Ctx::defer`, which runs with full `&mut AppState`). The
//! render path then reads only that opaque snapshot — at most one frame
//! stale, which is cosmetically harmless for these markers.

pub mod sidebar;

use eframe::egui;

use crate::activity::{Activity, Ctx, View};
use crate::icons;

/// Zero-sized `Activity` impl for the Files activity. Pure descriptor:
/// holds no state. The real state (the file-tree UI state) lives in
/// `AppState::file_tree_state`; the sidebar surface reaches it via
/// `Ctx::state.downcast_mut::<FileTreeState>()` and routes broad effects
/// (open a note / board, move, rename, delete, ...) through `Ctx::defer`.
pub struct Files;

impl Activity for Files {
    fn id(&self) -> &'static str {
        "files"
    }
    fn label(&self) -> &'static str {
        "Files"
    }
    fn icon(&self) -> egui::Image<'static> {
        icons::ICONS.image(icons::Icon::Folder)
    }
    fn views(&self) -> Vec<&dyn View> {
        vec![&FilesSidebar]
    }
}

struct FilesSidebar;

impl View for FilesSidebar {
    fn id(&self) -> &'static str {
        "files"
    }
    fn render(&self, ui: &mut egui::Ui, ctx: &mut Ctx<'_>) {
        egui::ScrollArea::vertical()
            .id_salt("panel-files-body")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                sidebar::FilesCtx { ctx }.render(ui);
            });
    }
}
