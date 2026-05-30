//! Editor host — open-files map, dirty tracking, save dispatch.
//!
//! Tabs themselves are `Payload` values living inside the workbench; the
//! buffer state for an editor tab lives here in `Buffers` keyed by `TabId`.
//! This split lets us hand the workbench a cheap, `Clone`-able tab payload
//! while the editor's `EditorState` + `ViewState` (neither `Clone`-friendly
//! for our purposes) stay borrowed through a `RefCell`-free map on the host.

pub mod tab;

use std::collections::HashMap;

use editor_core::rope::Rope;
use editor_core::state::Editor as EditorState;
use editor_core::theme::light_default;
use editor_md::styling::markdown_decorations;
use editor_view::viewport::ViewState;
use egui_workbench::workspace::TabId;

use crate::vfs::VfsPath;

/// One open editor buffer.
pub struct Buffer {
    pub path: VfsPath,
    pub state: EditorState,
    pub view: ViewState,
    /// `state.doc.content_id()` at the last save (or load). Comparing
    /// against the current `content_id()` gives a free dirty bit without
    /// us having to listen for transactions.
    pub saved_content_id: usize,
    /// `state.doc.content_id()` the markdown decoration set was last
    /// rebuilt against. `0` means "never built" (or non-markdown buffer).
    /// Cheap equality check per frame avoids rebuilding when nothing
    /// changed.
    md_decorations_content_id: usize,
}

impl Buffer {
    pub fn from_bytes(path: VfsPath, bytes: &[u8]) -> Self {
        // Lossy decode is acceptable for Phase 1; the hex view handles
        // binary files separately.
        let text = String::from_utf8_lossy(bytes).into_owned();
        let state = EditorState::from_doc(Rope::from_str(&text));
        let saved_content_id = state.doc.content_id();
        Self {
            path,
            state,
            view: ViewState::default(),
            saved_content_id,
            md_decorations_content_id: 0,
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.state.doc.content_id() != self.saved_content_id
    }

    pub fn mark_saved(&mut self) {
        self.saved_content_id = self.state.doc.content_id();
    }

    pub fn contents(&self) -> String {
        self.state.doc.to_string()
    }

    /// Rebuild the markdown decoration set when the buffer's content
    /// (or selection) has changed since the last rebuild. No-op for
    /// non-markdown buffers and when nothing has changed. Drives
    /// per-language fenced-code syntax highlighting (every tree-sitter
    /// grammar `editor-md` ships), heading/emphasis/list styling, and
    /// the live-preview marker-fade behaviour.
    pub fn refresh_markdown_decorations(&mut self) {
        if !path_is_markdown(&self.path) {
            return;
        }
        let now = self.state.doc.content_id();
        if now == self.md_decorations_content_id {
            return;
        }
        let theme = light_default();
        self.view.decorations.clear();
        self.view
            .decorations
            .push(markdown_decorations(&self.state, Some(&theme)));
        self.md_decorations_content_id = now;
    }
}

/// `true` when the buffer's path looks like markdown (`.md` /
/// `.markdown`). Used to gate decoration application — non-markdown
/// buffers render as plain text.
fn path_is_markdown(path: &VfsPath) -> bool {
    let Some(name) = path.file_name() else { return false };
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".md") || lower.ends_with(".markdown")
}

/// Map from workbench tab handle → editor buffer (or hex view).
#[derive(Default)]
pub struct Buffers {
    text: HashMap<TabId, Buffer>,
    hex: HashMap<TabId, HexBuffer>,
}

pub struct HexBuffer {
    pub path: VfsPath,
    pub bytes: Vec<u8>,
}

impl Buffers {
    pub fn insert_text(&mut self, id: TabId, buf: Buffer) {
        self.text.insert(id, buf);
    }

    pub fn insert_hex(&mut self, id: TabId, buf: HexBuffer) {
        self.hex.insert(id, buf);
    }

    pub fn get(&self, id: TabId) -> Option<&Buffer> {
        self.text.get(&id)
    }

    pub fn get_mut(&mut self, id: TabId) -> Option<&mut Buffer> {
        self.text.get_mut(&id)
    }

    pub fn get_hex(&self, id: TabId) -> Option<&HexBuffer> {
        self.hex.get(&id)
    }

    pub fn find_by_path(&self, path: &VfsPath) -> Option<TabId> {
        self.text
            .iter()
            .find(|(_, b)| b.path == *path)
            .map(|(id, _)| *id)
            .or_else(|| {
                self.hex
                    .iter()
                    .find(|(_, b)| b.path == *path)
                    .map(|(id, _)| *id)
            })
    }
}

/// Heuristic: declare a buffer "binary" when any of the first 8KB are
/// NUL bytes. Same trick git uses; cheap and good enough for routing
/// the hex view.
pub fn looks_binary(bytes: &[u8]) -> bool {
    let probe = &bytes[..bytes.len().min(8 * 1024)];
    probe.contains(&0)
}
