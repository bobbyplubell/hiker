//! `Document` trait + `State`. See `DESIGN.md`.
//!
//! `TabId` itself lives in [`crate::workspace`].

use crate::workspace::TabId;

/// User-facing tab payload. Implement for your app's tab type.
pub trait Document: Clone + 'static {
    fn title(&self) -> egui::WidgetText;
    fn icon(&self) -> Option<egui::Image<'static>> {
        None
    }
    fn is_dirty(&self) -> bool {
        false
    }
    fn tooltip(&self) -> Option<String> {
        None
    }
    fn closable(&self) -> bool {
        true
    }

    /// Should the workbench wrap this tab's body in the standard
    /// `pane_content_inset` margin? Defaults to `true` for the typical
    /// "settings-style" panel where the inset reads as a normal content
    /// margin against the pane background. Tabs whose body is its own
    /// edge-to-edge surface — markdown / source editors painting their
    /// own background — should return `false` so a contrasting strip
    /// of pane fill isn't visible around the editor's bg color.
    fn wants_pane_content_inset(&self) -> bool {
        true
    }
}

/// Per-tab UI state. Distinct from the `Tab` payload itself.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum State {
    /// Default. Opaque title, persists across navigation.
    #[default]
    Regular,
    /// Italic title. Will be replaced by the next preview-open in the
    /// same group. Promotes to [`State::Regular`] on edit / double click.
    Preview,
    /// Sorted leftmost in the strip, smaller, with a pin glyph.
    /// Cannot be closed by "Close others"; only an explicit close removes it.
    Pinned,
}

/// Crate-internal payload entry. The trees store [`TabId`]s; this
/// is the actual data those handles resolve to.
pub(crate) struct TabEntry<Tab> {
    pub tab: Tab,
    pub state: State,
    pub handle: TabId,
}

impl<Tab> TabEntry<Tab> {
    pub(crate) const fn new(tab: Tab, state: State, handle: TabId) -> Self {
        Self { tab, state, handle }
    }
}

/// Context passed to [`crate::Host::pane_ui`]. Carries the
/// metadata about the call site that hosts often need without forcing
/// every host to thread its own state.
#[non_exhaustive]
pub struct UiContext<'a> {
    /// Handle of the tab whose body is being rendered.
    pub handle: TabId,
    /// Group the tab belongs to.
    pub group: crate::workspace::GroupId,
    /// `true` if this tab's group is the currently focused editor group.
    pub focused: bool,
    /// Current UI state of the tab.
    pub state: State,
    /// Phantom so we may add lifetime-bound fields without API churn.
    pub(crate) _marker: std::marker::PhantomData<&'a ()>,
}
