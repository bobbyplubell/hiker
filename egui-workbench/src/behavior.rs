//! `WorkbenchBehavior` trait — host integration surface.
//!
//! Every method has a sensible default; the only one a host must
//! implement is [`WorkbenchBehavior::pane_ui`]. New methods on this
//! trait are backwards-compatible additions as long as they ship with
//! default implementations.

use std::hash::Hash;

use crate::activity_bar::ActivityItem;
use crate::tab::{DocumentTab, TabUiContext};
use crate::theme::{TabStyle, WorkbenchTheme};

/// Host-implemented trait that supplies the workbench with rendering,
/// state, and lifecycle hooks. See `DESIGN.md` for the full method set.
pub trait WorkbenchBehavior<Tab: DocumentTab, Mode: Clone + Eq + Hash + 'static> {
    // === Tab rendering ===

    /// Render the body of a tab in the given `Ui`. Required.
    fn pane_ui(&mut self, ui: &mut egui::Ui, tab: &mut Tab, ctx: TabUiContext<'_>);

    /// Optional per-tab style override. Default returns `None` (inherit ambient theme).
    fn tab_style(&self, _tab: &Tab) -> Option<TabStyle> {
        None
    }

    // === Tab lifecycle hooks ===

    /// Called when the user clicks the close button on a tab.
    /// Return `false` to veto the close (e.g., to show a save-prompt
    /// modal); the host can later call [`crate::Workbench::close_tab`].
    fn on_tab_close(&mut self, _tab: &Tab) -> bool {
        true
    }

    /// Called when a Preview tab transitions to Regular.
    fn on_preview_promoted(&mut self, _tab: &Tab) {}

    /// Add custom items to the tab right-click context menu.
    /// The crate adds Close / Close Others / Pin / Unpin around this.
    fn tab_context_menu(&mut self, _ui: &mut egui::Ui, _tab: &Tab) {}

    // === Side bar content ===

    /// Render the content of the primary side bar for the currently
    /// active activity.
    fn side_bar_ui(&mut self, _ui: &mut egui::Ui, _mode: &Mode) {}

    /// Render the content of the secondary side bar. The secondary bar
    /// is not driven by the activity bar — its content is fixed by the
    /// host (typical use: a chat / inspector / output panel that lives
    /// on the opposite edge from the primary). Default is empty.
    fn secondary_side_bar_ui(&mut self, _ui: &mut egui::Ui) {}

    /// Title shown in the primary side bar header. Defaults to empty.
    fn side_bar_title(&self, _mode: &Mode) -> egui::WidgetText {
        egui::WidgetText::default()
    }

    /// Title shown in the secondary side bar header. Defaults to empty.
    fn secondary_side_bar_title(&self) -> egui::WidgetText {
        egui::WidgetText::default()
    }

    // === Activity bar ===

    /// The activities to render, in order. Default is empty (hidden bar
    /// content, though the bar itself still draws).
    fn activity_items(&self) -> Vec<ActivityItem<Mode>> {
        Vec::new()
    }

    /// Right-click context menu items for an activity.
    fn activity_context_menu(&mut self, _ui: &mut egui::Ui, _mode: &Mode) {}

    // === Status bar ===

    /// Render the status bar cells. Use `ui.with_layout` for alignment.
    fn status_bar_ui(&mut self, _ui: &mut egui::Ui) {}

    // === Theming ===

    /// Per-workbench theme overrides. Default returns the ambient
    /// `egui::Style`-derived theme.
    fn theme(&self, style: &egui::Style) -> WorkbenchTheme {
        WorkbenchTheme::from_egui_style(style)
    }
}
