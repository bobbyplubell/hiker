//! `eframe::App` impl that drives the workbench, the editor host, and
//! every panel.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use editor_egui::widget::Widget as EditorWidget;
use egui_workbench::activity_bar::Item;
use egui_workbench::behavior::Host;
use egui_workbench::tab::UiContext;
use egui_workbench::workspace::{OpenTabOptions, Workbench};
use tokio::runtime::Handle;

use crate::host::tab::{LiteMode, Payload};
use crate::host::{Buffer, Buffers, HexBuffer, looks_binary};
use crate::panels::file_search::{self, FileSearch};
use crate::panels::filetree::{FileTree, FileTreeAction};
use crate::panels::find_replace::FindReplace;
use crate::panels::hex_view;
use crate::vfs::native::Backend;
use crate::vfs::{DynVfs, VfsPath};

/// Pending file load — populated by a tokio task, drained by the egui
/// frame loop and turned into a workbench tab.
struct LoadedFile {
    path: VfsPath,
    bytes: Vec<u8>,
    /// `true` when the user explicitly asked for the hex view; otherwise
    /// the loader decides based on `looks_binary`.
    force_hex: bool,
}

pub struct LiteApp {
    runtime: Handle,
    workbench: Workbench<Payload, LiteMode>,
    buffers: Buffers,
    vfs: Option<DynVfs>,
    root: Option<PathBuf>,
    filetree: FileTree,
    file_search: FileSearch,
    find_replace: FindReplace,
    pending_loads: Arc<Mutex<Vec<LoadedFile>>>,
    status: String,
}

impl LiteApp {
    pub fn new(runtime: Handle) -> Self {
        let mut workbench = Workbench::<Payload, LiteMode>::new();
        workbench.activity_bar.set_active(Some(LiteMode::Files));
        Self {
            runtime,
            workbench,
            buffers: Buffers::default(),
            vfs: None,
            root: None,
            filetree: FileTree::new(),
            file_search: FileSearch::default(),
            find_replace: FindReplace::default(),
            pending_loads: Arc::new(Mutex::new(Vec::new())),
            status: "Open a folder to begin.".into(),
        }
    }

    fn open_folder_dialog(&mut self) {
        let Some(picked) = rfd::FileDialog::new().pick_folder() else { return };
        self.set_root(picked);
    }

    fn set_root(&mut self, root: PathBuf) {
        self.vfs = Some(Arc::new(Backend::new(root.clone())));
        self.root = Some(root);
        self.filetree = FileTree::new();
        self.file_search.reset_index();
        self.status = "Folder open.".into();
    }

    /// Spawn an async load of `path`. The completion is drained in
    /// [`Self::drain_loads`] on the next frame.
    fn request_open(&mut self, path: VfsPath, force_hex: bool) {
        // Already open? Just activate the tab.
        if let Some(id) = self.buffers.find_by_path(&path) {
            self.workbench.set_active(id);
            return;
        }
        let Some(vfs) = self.vfs.as_ref().cloned() else { return };
        let pending = Arc::clone(&self.pending_loads);
        self.runtime.spawn(async move {
            match vfs.read(&path).await {
                Ok(bytes) => {
                    if let Ok(mut g) = pending.lock() {
                        g.push(LoadedFile { path, bytes, force_hex });
                    }
                }
                Err(e) => tracing::warn!(?e, "open failed"),
            }
        });
    }

    fn drain_loads(&mut self) {
        let mut loads = Vec::new();
        if let Ok(mut g) = self.pending_loads.lock() {
            std::mem::swap(&mut *g, &mut loads);
        }
        for load in loads {
            self.materialise_load(load);
        }
    }

    fn materialise_load(&mut self, load: LoadedFile) {
        let title = load
            .path
            .file_name()
            .unwrap_or("untitled")
            .to_string();
        if load.force_hex || looks_binary(&load.bytes) {
            let tab = Payload::Hex {
                path: load.path.clone(),
                title,
            };
            let id = self.workbench.open_tab(tab, &OpenTabOptions::default());
            self.buffers.insert_hex(
                id,
                HexBuffer {
                    path: load.path,
                    bytes: load.bytes,
                },
            );
        } else {
            let buf = Buffer::from_bytes(load.path.clone(), &load.bytes);
            let tab = Payload::Text {
                path: load.path,
                title,
                dirty: false,
            };
            let id = self.workbench.open_tab(tab, &OpenTabOptions::default());
            self.buffers.insert_text(id, buf);
        }
    }

    fn save_active(&mut self) {
        let Some(id) = self.workbench.active_handle() else { return };
        let Some(buffer) = self.buffers.get_mut(id) else { return };
        let Some(vfs) = self.vfs.as_ref().cloned() else { return };
        let path = buffer.path.clone();
        let bytes = buffer.contents().into_bytes();
        buffer.mark_saved();
        self.status = format!("Saved {path}");
        self.runtime.spawn(async move {
            if let Err(e) = vfs.write(&path, &bytes).await {
                tracing::warn!(?e, %path, "save failed");
            }
        });
    }

    fn handle_global_keys(&mut self, ctx: &egui::Context) {
        let (cmd_s, cmd_p, cmd_f, cmd_h, cmd_o) = ctx.input(|i| {
            let m = i.modifiers;
            let primary = m.mac_cmd || m.ctrl;
            (
                primary && i.key_pressed(egui::Key::S),
                primary && i.key_pressed(egui::Key::P),
                primary && i.key_pressed(egui::Key::F),
                primary && i.key_pressed(egui::Key::H),
                primary && i.key_pressed(egui::Key::O),
            )
        });
        if cmd_s {
            self.save_active();
        }
        if cmd_p {
            self.file_search.show();
        }
        if cmd_f {
            self.find_replace.open_find();
        }
        if cmd_h {
            self.find_replace.open_replace();
        }
        if cmd_o {
            self.open_folder_dialog();
        }
    }

    fn sync_tab_dirty(&mut self) {
        // Tabs aren't borrowed mutably from the workbench's internal
        // tree directly; the dirty bit lives on the `Buffer` and is
        // exposed via `Payload::is_dirty`. The workbench reads
        // `Document::is_dirty()` each frame, so we need the tab payload
        // to reflect the live buffer state. We mutate via the iterator
        // on the editor area.
        let snapshot: Vec<(egui_workbench::workspace::TabId, bool)> = self
            .workbench
            .iter_tabs()
            .filter_map(|(id, tab)| {
                let Payload::Text { .. } = tab else { return None };
                let dirty = self.buffers.get(id).is_some_and(Buffer::is_dirty);
                Some((id, dirty))
            })
            .collect();
        for (id, dirty) in snapshot {
            if let Some(tab) = self.workbench.editor_area.get_mut(id) {
                tab.set_dirty(dirty);
            }
        }
    }
}

impl eframe::App for LiteApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_loads();
        self.handle_global_keys(ctx);

        // Classic menu bar.
        egui::TopBottomPanel::top("hiker_lite::menubar").show(ctx, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Open Folder…").clicked() {
                        self.open_folder_dialog();
                        ui.close();
                    }
                    if ui.button("Save").clicked() {
                        self.save_active();
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Quit").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
                ui.menu_button("Edit", |ui| {
                    if ui.button("Find…").clicked() {
                        self.find_replace.open_find();
                        ui.close();
                    }
                    if ui.button("Replace…").clicked() {
                        self.find_replace.open_replace();
                        ui.close();
                    }
                });
                ui.menu_button("View", |ui| {
                    if ui.button("Go to File…").clicked() {
                        self.file_search.show();
                        ui.close();
                    }
                    if ui.button("Open as Hex").clicked() {
                        if let Some(id) = self.workbench.active_handle()
                            && let Some(buffer) = self.buffers.get(id)
                        {
                            let path = buffer.path.clone();
                            self.request_open(path, true);
                        }
                        ui.close();
                    }
                });
            });
        });

        // File search overlay (Cmd-P).
        if let Some(root) = self.root.clone() {
            match self.file_search.ui(ctx, &root, &self.runtime) {
                file_search::Action::Open(path) => self.request_open(path, false),
                file_search::Action::None | file_search::Action::Dismiss => {}
            }
        }

        // Find/replace overlay.
        let active_buf = self
            .workbench
            .active_handle()
            .and_then(|id| self.buffers.get_mut(id));
        self.find_replace.ui(ctx, active_buf);

        // Sync tab dirty flags from buffers before the workbench reads them.
        self.sync_tab_dirty();

        let mut behavior = LiteBehavior {
            buffers: &mut self.buffers,
            filetree: &mut self.filetree,
            vfs: self.vfs.clone(),
            runtime: self.runtime.clone(),
            status: self.status.clone(),
            requested_open: None,
        };
        self.workbench.ui(ctx, &mut behavior);
        if let Some(path) = behavior.requested_open {
            self.request_open(path, false);
        }
    }
}

struct LiteBehavior<'a> {
    buffers: &'a mut Buffers,
    filetree: &'a mut FileTree,
    vfs: Option<DynVfs>,
    runtime: Handle,
    status: String,
    requested_open: Option<VfsPath>,
}

impl<'a> Host<Payload, LiteMode> for LiteBehavior<'a> {
    fn pane_ui(&mut self, ui: &mut egui::Ui, tab: &mut Payload, ctx: UiContext<'_>) {
        match tab {
            Payload::Text { .. } => {
                if let Some(buffer) = self.buffers.get_mut(ctx.handle) {
                    // Rebuild markdown decorations on the active buffer when
                    // its content changed. No-op for non-markdown buffers.
                    buffer.refresh_markdown_decorations();
                    EditorWidget::new(&mut buffer.state, &mut buffer.view).show(ui);
                } else {
                    ui.weak("(buffer not loaded)");
                }
            }
            Payload::Hex { .. } => {
                if let Some(buf) = self.buffers.get_hex(ctx.handle) {
                    hex_view::show(ui, buf);
                } else {
                    ui.weak("(hex buffer not loaded)");
                }
            }
        }
    }

    fn on_tab_close(&mut self, tab: &Payload) -> bool {
        // Phase 1: no save-prompt modal; closing a dirty tab drops the
        // unsaved edits. The dirty marker in the tab title is the
        // user-visible warning. Returning `true` lets the workbench
        // complete the close; we drop the underlying buffer in
        // post-frame cleanup below.
        let _ = tab;
        true
    }

    fn side_bar_ui(&mut self, ui: &mut egui::Ui, mode: &LiteMode) {
        match mode {
            LiteMode::Files => {
                if let Some(vfs) = self.vfs.as_ref() {
                    match self.filetree.ui(ui, vfs, &self.runtime) {
                        FileTreeAction::Open(path) => self.requested_open = Some(path),
                        FileTreeAction::None => {}
                    }
                } else {
                    ui.weak("Open a folder to populate the file tree.");
                }
            }
        }
    }

    fn side_bar_title(&self, mode: &LiteMode) -> egui::WidgetText {
        mode.label().into()
    }

    fn status_bar_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.label("hiker-lite");
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(&self.status);
            });
        });
    }

    fn activity_items(&self) -> Vec<Item<LiteMode>> {
        vec![Item {
            mode: LiteMode::Files,
            icon: Some(egui::Image::new(egui::include_image!(
                "../assets/icons/folder.svg"
            ))),
            label: "Files".into(),
            badge: None,
        }]
    }
}
