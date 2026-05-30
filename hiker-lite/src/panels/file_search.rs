//! Filename fuzzy search overlay (Cmd-P).
//!
//! On first open we walk the root once via `walkdir` and cache the
//! resulting paths. Matching is done synchronously by `nucleo-matcher`
//! against the typed query.

use std::sync::{Arc, Mutex};

use nucleo_matcher::{Config, Matcher, Utf32String, pattern::{CaseMatching, Normalization, Pattern}};
use tokio::runtime::Handle;
use walkdir::WalkDir;

use crate::vfs::VfsPath;

#[derive(Default)]
pub struct FileSearch {
    pub open: bool,
    query: String,
    selected: usize,
    /// All file paths under the root (relative). Loaded lazily.
    index: Arc<Mutex<Option<Vec<VfsPath>>>>,
}

pub enum Action {
    None,
    Open(VfsPath),
    Dismiss,
}

impl FileSearch {
    pub fn show(&mut self) {
        self.open = true;
        self.query.clear();
        self.selected = 0;
    }

    /// Drop the cached index — call after the root changes.
    pub fn reset_index(&self) {
        if let Ok(mut g) = self.index.lock() {
            *g = None;
        }
    }

    fn ensure_index(&self, root: &std::path::Path, runtime: &Handle) {
        let mut guard = self.index.lock().expect("file_search index poisoned");
        if guard.is_some() {
            return;
        }
        // Mark loading with an empty vec so we don't kick repeated walks.
        *guard = Some(Vec::new());
        drop(guard);
        let index = Arc::clone(&self.index);
        let root = root.to_path_buf();
        runtime.spawn_blocking(move || {
            let paths = walk_root(&root);
            if let Ok(mut g) = index.lock() {
                *g = Some(paths);
            }
        });
    }

    pub fn ui(
        &mut self,
        ctx: &egui::Context,
        root: &std::path::Path,
        runtime: &Handle,
    ) -> Action {
        if !self.open {
            return Action::None;
        }
        self.ensure_index(root, runtime);
        let mut action = Action::None;

        // Render a centered floating window.
        let screen = ctx.screen_rect();
        let width = 560.0_f32.min(screen.width() * 0.7);
        let pos = egui::pos2(screen.center().x - width * 0.5, screen.top() + 80.0);
        egui::Window::new("Go to file")
            .fixed_pos(pos)
            .fixed_size([width, 0.0])
            .collapsible(false)
            .resizable(false)
            .title_bar(false)
            .show(ctx, |ui| {
                self.render_body(ui, &mut action);
            });
        action
    }

    fn render_body(&mut self, ui: &mut egui::Ui, action: &mut Action) {
        let input = ui.text_edit_singleline(&mut self.query);
        input.request_focus();

        let key_enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
        let key_esc = ui.input(|i| i.key_pressed(egui::Key::Escape));
        let key_up = ui.input(|i| i.key_pressed(egui::Key::ArrowUp));
        let key_down = ui.input(|i| i.key_pressed(egui::Key::ArrowDown));

        if key_esc {
            self.open = false;
            *action = Action::Dismiss;
            return;
        }

        let matches = self.compute_matches();
        if !matches.is_empty() {
            if key_up {
                self.selected = self.selected.saturating_sub(1);
            }
            if key_down && self.selected + 1 < matches.len() {
                self.selected += 1;
            }
        }

        ui.separator();
        let limit = matches.len().min(50);
        egui::ScrollArea::vertical().max_height(400.0).show(ui, |ui| {
            for (idx, path) in matches.iter().take(limit).enumerate() {
                let selected = idx == self.selected;
                let resp = ui.selectable_label(selected, path.to_string());
                if resp.clicked() {
                    self.open = false;
                    *action = Action::Open(path.clone());
                }
            }
        });

        if key_enter
            && let Some(p) = matches.get(self.selected)
        {
            self.open = false;
            *action = Action::Open(p.clone());
        }
    }

    fn compute_matches(&self) -> Vec<VfsPath> {
        let guard = self.index.lock().expect("file_search index poisoned");
        let Some(paths) = guard.as_ref() else { return Vec::new() };
        if self.query.is_empty() {
            return paths.iter().take(50).cloned().collect();
        }
        let mut matcher = Matcher::new(Config::DEFAULT);
        let pattern = Pattern::parse(
            &self.query,
            CaseMatching::Smart,
            Normalization::Smart,
        );
        // Score each path; nucleo wants Utf32String haystacks.
        let mut scored: Vec<(u32, VfsPath)> = paths
            .iter()
            .filter_map(|p| {
                let haystack = Utf32String::from(p.to_string().as_str());
                pattern
                    .score(haystack.slice(..), &mut matcher)
                    .map(|s| (s, p.clone()))
            })
            .collect();
        scored.sort_by_key(|s| std::cmp::Reverse(s.0));
        scored.into_iter().map(|(_, p)| p).collect()
    }
}

fn walk_root(root: &std::path::Path) -> Vec<VfsPath> {
    let mut out = Vec::new();
    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            e.file_name()
                .to_str()
                .map(|n| !n.starts_with('.'))
                .unwrap_or(true)
        })
        .flatten()
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else { continue };
        out.push(rel_to_vfs(rel));
    }
    out.sort();
    out
}

fn rel_to_vfs(rel: &std::path::Path) -> VfsPath {
    let mut path = VfsPath::root();
    for c in rel.components() {
        if let std::path::Component::Normal(s) = c {
            path = path.join(&s.to_string_lossy());
        }
    }
    path
}

