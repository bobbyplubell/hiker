//! Workbench tab payload. Cheap to clone — the actual editor state
//! lives in [`crate::host::Buffers`] keyed by `TabId`.

use egui_workbench::tab::Document;

use crate::vfs::VfsPath;

#[derive(Clone, Debug)]
pub enum Payload {
    /// A text editor tab. `dirty` is mirrored from the host every frame
    /// so the workbench's bullet marker stays in sync; the
    /// authoritative dirty bit is on the `Buffer`.
    Text {
        path: VfsPath,
        title: String,
        dirty: bool,
    },
    /// Read-only hex dump tab.
    Hex {
        path: VfsPath,
        title: String,
    },
}

impl Payload {
    pub const fn path(&self) -> &VfsPath {
        match self {
            Self::Text { path, .. } | Self::Hex { path, .. } => path,
        }
    }

    pub const fn set_dirty(&mut self, value: bool) {
        if let Self::Text { dirty, .. } = self {
            *dirty = value;
        }
    }
}

impl Document for Payload {
    fn title(&self) -> egui::WidgetText {
        match self {
            Self::Text { title, .. } => title.clone().into(),
            Self::Hex { title, .. } => format!("{title} (hex)").into(),
        }
    }

    fn is_dirty(&self) -> bool {
        matches!(self, Self::Text { dirty: true, .. })
    }

    fn tooltip(&self) -> Option<String> {
        Some(self.path().to_string())
    }

    fn wants_pane_content_inset(&self) -> bool {
        // The editor and hex view paint their own backgrounds
        // edge-to-edge.
        false
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum LiteMode {
    Files,
}

impl LiteMode {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Files => "Files",
        }
    }
}
