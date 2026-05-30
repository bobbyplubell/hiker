//! Filetree side-bar panel. Lazily lists directory contents through
//! the `Vfs`; expansion / double-click are rendered synchronously
//! against an in-memory cache that the host populates ahead of time.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::runtime::Handle;

use crate::vfs::{DirEntry, DynVfs, VfsPath};

/// Per-directory listing result. `Err` carries a stringified `VfsError`
/// so the cache can sit behind a single `Mutex` without juggling
/// non-`Send` payloads.
type DirListing = Result<Vec<DirEntry>, String>;

/// Filetree state. Listings are populated asynchronously through the
/// tokio handle and cached here for synchronous render.
#[derive(Default)]
pub struct FileTree {
    cache: Arc<Mutex<HashMap<VfsPath, DirListing>>>,
    /// Set of expanded directories.
    expanded: std::collections::HashSet<VfsPath>,
}

pub enum FileTreeAction {
    None,
    Open(VfsPath),
}

impl FileTree {
    pub fn new() -> Self {
        let mut s = Self::default();
        s.expanded.insert(VfsPath::root());
        s
    }

    /// Kick off async listing of `path` if we haven't loaded it yet.
    fn ensure_loaded(&self, vfs: &DynVfs, runtime: &Handle, path: &VfsPath) {
        let mut cache = self.cache.lock().expect("filetree cache poisoned");
        if cache.contains_key(path) {
            return;
        }
        // Mark "in flight" with an empty Ok so we don't re-spawn while the
        // task runs. The task overwrites the entry on completion.
        cache.insert(path.clone(), Ok(Vec::new()));
        drop(cache);
        let cache = Arc::clone(&self.cache);
        let vfs = Arc::clone(vfs);
        let owned = path.clone();
        runtime.spawn(async move {
            let result = vfs
                .list(&owned)
                .await
                .map_err(|e| e.to_string());
            let mut guard = cache.lock().expect("filetree cache poisoned");
            guard.insert(owned, result);
        });
    }

    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        vfs: &DynVfs,
        runtime: &Handle,
    ) -> FileTreeAction {
        let mut action = FileTreeAction::None;
        ui.horizontal(|ui| {
            if ui.small_button("Refresh").clicked() {
                let mut cache = self.cache.lock().expect("filetree cache poisoned");
                cache.clear();
            }
        });
        ui.separator();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                self.render_dir(ui, vfs, runtime, &VfsPath::root(), 0, &mut action);
            });
        action
    }

    fn render_dir(
        &mut self,
        ui: &mut egui::Ui,
        vfs: &DynVfs,
        runtime: &Handle,
        path: &VfsPath,
        depth: usize,
        action: &mut FileTreeAction,
    ) {
        self.ensure_loaded(vfs, runtime, path);
        let snapshot = {
            let cache = self.cache.lock().expect("filetree cache poisoned");
            cache.get(path).cloned()
        };
        let Some(entry_result) = snapshot else { return };
        let entries = match entry_result {
            Ok(v) => v,
            Err(e) => {
                ui.colored_label(egui::Color32::LIGHT_RED, format!("error: {e}"));
                return;
            }
        };
        for entry in entries {
            self.render_entry(ui, vfs, runtime, &entry, depth, action);
        }
    }

    fn render_entry(
        &mut self,
        ui: &mut egui::Ui,
        vfs: &DynVfs,
        runtime: &Handle,
        entry: &DirEntry,
        depth: usize,
        action: &mut FileTreeAction,
    ) {
        let indent = depth as f32 * 12.0;
        ui.horizontal(|ui| {
            ui.add_space(indent);
            if entry.is_dir {
                let expanded = self.expanded.contains(&entry.path);
                let glyph = if expanded { "v" } else { ">" };
                let label = format!("{glyph} {}", entry.name);
                if ui.selectable_label(false, label).clicked() {
                    if expanded {
                        self.expanded.remove(&entry.path);
                    } else {
                        self.expanded.insert(entry.path.clone());
                    }
                }
            } else {
                let resp = ui.selectable_label(false, &entry.name);
                if resp.double_clicked() || resp.clicked() {
                    *action = FileTreeAction::Open(entry.path.clone());
                }
            }
        });
        if entry.is_dir && self.expanded.contains(&entry.path) {
            self.render_dir(ui, vfs, runtime, &entry.path, depth + 1, action);
        }
    }
}
