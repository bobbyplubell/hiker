//! Files activity module — activity-registry-first home for the filetree
//! sidebar (lazy directory tree + open / rename / move / duplicate /
//! delete verbs). Migrated out of `sidebar::files` as part of activity
//! registry Phase 2 (`feature-filetree-migration`). Files is the primary
//! sidebar mode, so the `Files` descriptor is registered first.
//!
//! Layout:
//!
//! - `sidebar.rs` — the sidebar mode body: the lazy file tree rendered
//!   through the narrow `activity::SurfaceCtx`. Tree UI state lives in
//!   `AppState::file_tree_state` (reached via `SurfaceCtx.state`); directory
//!   listings via `Ctx::vault`; index markers via `Ctx::services`; the
//!   active-note highlight via `Ctx::active_path`. Broad mutations (open
//!   a note / board, drag-drop move, rename, duplicate, reindex,
//!   add-to-trail / set-active-trail, add-to-board, delete) are routed
//!   through `SurfaceCtx::defer`.
//! - `rename.rs` — the inline-rename machinery: the egui-memory draft
//!   lifecycle behind the in-tree rename `TextEdit`, the commit path through
//!   the indexer-driven `move_note`, and the open-buffer/tab repointing +
//!   observed-rename git commit shared with drag-drop moves.
//!
//! The host (`workbench_host`) keeps the title-row `+` new-note / new-board
//! buttons and the `⋯` refresh / sort menu — those have full
//! `&mut AppState` and don't fit the narrow `SurfaceCtx`.
//!
//! Decoration snapshots: a few row decorations need data the narrow
//! `SurfaceCtx` deliberately doesn't carry — the dirty-buffer set
//! (`session.buffers`), the skipped-paths set (the vault-session event
//! channel), and the active-trail membership set (another activity's
//! state). Rather than grow `SurfaceCtx` with those reads or special-case the
//! generic sidebar consumer, the surface refreshes
//! `file_tree_state.deco` once per frame via a single deferred pre-pass
//! closure (`SurfaceCtx::defer`, which runs with full `&mut AppState`). The
//! render path then reads only that opaque snapshot — at most one frame
//! stale, which is cosmetically harmless for these markers.

pub mod rename;
pub mod sidebar;

use eframe::egui;

use egui_workbench::activity::{Activity, View};
use crate::activity::AppCtx;
use crate::icons;

/// Zero-sized `Activity` impl for the Files activity. Pure descriptor:
/// holds no state. The real state (the file-tree UI state) lives in
/// `AppState::file_tree_state`; the sidebar surface reaches it via
/// `ctx.state.downcast_mut::<FileTreeState>()` and routes broad effects
/// (open a note / board, move, rename, delete, ...) through `SurfaceCtx::defer`.
pub struct Files;

impl Activity<dyn AppCtx> for Files {
    fn id(&self) -> &'static str {
        "files"
    }
    fn label(&self) -> &'static str {
        "Files"
    }
    fn icon(&self) -> egui::Image<'static> {
        icons::ICONS.image(icons::Icon::Folder)
    }
    fn views(&self) -> Vec<&dyn View<dyn AppCtx>> {
        vec![&FilesSidebar]
    }
}

struct FilesSidebar;

impl View<dyn AppCtx> for FilesSidebar {
    fn id(&self) -> &'static str {
        "files"
    }
    fn render(&self, ui: &mut egui::Ui, ctx: &mut (dyn AppCtx + 'static)) {
        let Some(mut ctx) = ctx.surface_ctx(self.state_key()) else {
            return;
        };
        let ctx = &mut ctx;
        // The sidebar owns its own `ScrollArea` so it can virtualize the tree
        // (`show_rows` — only the rows in the viewport are laid out / painted,
        // keeping a large vault's per-frame cost O(visible) not O(vault)).
        sidebar::FilesCtx { ctx }.render(ui);
    }
}
