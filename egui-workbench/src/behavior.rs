//! `Host` trait — host integration surface.
//!
//! Every method has a sensible default; the only one a host must
//! implement is [`Host::pane_ui`]. New methods on this
//! trait are backwards-compatible additions as long as they ship with
//! default implementations.

use std::hash::Hash;

use crate::activity_bar::Item;
use crate::tab::{Document, UiContext};
use crate::theme::{TabStyle, Palette};

/// Host-implemented trait that supplies the workbench with rendering,
/// state, and lifecycle hooks. See `DESIGN.md` for the full method set.
pub trait Host<Tab: Document, Mode: Clone + Eq + Hash + 'static> {
    // === Tab rendering ===

    /// Render the body of a tab in the given `Ui`. Required.
    fn pane_ui(&mut self, ui: &mut egui::Ui, tab: &mut Tab, ctx: UiContext<'_>);

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

    /// Custom rendering for the secondary side bar's title slot. Default
    /// just labels `secondary_side_bar_title()`. Override to draw an
    /// icon or other rich content in place of the text.
    fn secondary_side_bar_title_ui(&mut self, ui: &mut egui::Ui) {
        ui.label(self.secondary_side_bar_title());
    }

    /// Extra buttons rendered next to the "…" menu in the primary side
    /// bar's title row. Mode-specific (e.g. "new note" for a file panel).
    fn side_bar_action_buttons(&mut self, _ui: &mut egui::Ui, _mode: &Mode) {}

    /// Contents of the popup that opens from the primary side bar's "…"
    /// button. Mode-specific (e.g. refresh / sort menu for a file panel).
    /// Default surfaces nothing extra; the workbench still appends its
    /// own "Move to Other Side" / "Hide" items after.
    fn side_bar_actions_menu(&mut self, _ui: &mut egui::Ui, _mode: &Mode) {}

    /// Extra buttons rendered next to the "…" menu in the secondary side
    /// bar's title row.
    fn secondary_side_bar_action_buttons(&mut self, _ui: &mut egui::Ui) {}

    /// Contents of the popup that opens from the secondary side bar's
    /// "…" button.
    fn secondary_side_bar_actions_menu(&mut self, _ui: &mut egui::Ui) {}

    // === Activity bar ===

    /// The activities to render, in order. Default is empty (hidden bar
    /// content, though the bar itself still draws).
    fn activity_items(&self) -> Vec<Item<Mode>> {
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
    fn theme(&self, style: &egui::Style) -> Palette {
        Palette::from_egui_style(style)
    }
}
