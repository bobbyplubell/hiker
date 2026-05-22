//! Panel area — bottom-docked tabbed surface for tool panels.
//!
//! Structurally identical to [`crate::EditorArea`] (it owns a tabbed
//! `egui_tiles::Tree<TabId>` + payload map), but visually distinct:
//! it has its own visibility, maximize, and height state, and the
//! workbench renders it inside a bottom `TopBottomPanel` rather than
//! the central panel. All tabbed-area behaviour is shared by composing
//! an [`EditorArea`] into the panel area; helpers below forward the
//! handful of operations the workspace needs.

use crate::editor_area::EditorArea;
use crate::workspace::TabId;
use crate::tab::{Document, State};

/// Bottom panel area. Adds visible / maximized / height chrome on top
/// of the shared tabbed-area surface ([`EditorArea`]).
pub struct PanelArea<Tab: Document> {
    pub visible: bool,
    /// When `true`, the panel area expands to consume the whole central
    /// region. Editor area is hidden until restored.
    pub maximized: bool,
    /// Resizable height (in points) of the panel area when not maximised.
    pub height: f32,
    /// The shared tabbed-area implementation. Public to the crate so
    /// `workspace` and `persistence` can manipulate the tree directly;
    /// public-API consumers stick to the forwarded methods below.
    pub(crate) inner: EditorArea<Tab>,
}

impl<Tab: Document> Default for PanelArea<Tab> {
    fn default() -> Self {
        Self {
            visible: false,
            maximized: false,
            height: 240.0,
            inner: EditorArea::with_tree_id(egui::Id::new("egui_workbench::panel_tree")),
        }
    }
}

impl<Tab: Document> PanelArea<Tab> {
    pub fn new() -> Self {
        Self::default()
    }

    pub const fn toggle(&mut self) {
        self.visible = !self.visible;
    }

    pub fn tab_count(&self) -> usize {
        self.inner.tab_count()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.tab_count() == 0
    }

    pub fn iter_tabs(&self) -> impl Iterator<Item = (TabId, &Tab)> {
        self.inner.iter_tabs()
    }

    pub fn state(&self, handle: TabId) -> Option<State> {
        self.inner.state(handle)
    }
}
