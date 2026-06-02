//! The `egui_workbench` shell for the crawler (`crawler-app-shell`).
//!
//! Adopts hiker-app's workbench chrome so the crawler looks/feels like hiker:
//! an activity bar + a primary side bar for the crawler controls (picked
//! fields, link-strategy, emit buttons, output), and a tabbed central editor
//! area where each tab is one live browser page. Mirrors
//! `app/src/workbench_host.rs`'s `Document` + `Host` + `Workbench` pattern,
//! kept minimal for the crawler.
//!
//! Browsing is multi-tab *via workbench tabs* — there is no Chromium chrome of
//! its own; each CEF browser stays windowless/OSR and its texture is painted
//! into the tab body. The per-tab browser, selection, URL and OSR texture live
//! in [`crate::app::TabState`] keyed by the workbench [`TabId`]; the workbench
//! tab payload ([`CrawlerTab`]) is a thin view-model carrying just the `TabId`
//! and a cached title for the tab strip.

use eframe::egui;
use egui_workbench::activity_bar::Item;
use egui_workbench::behavior::Host;
use egui_workbench::tab::{Document, UiContext};
use egui_workbench::theme::Palette;
use egui_workbench::workspace::TabId;

use crate::app::CrawlerApp;

/// The activity-bar / side-bar mode. The crawler has a single primary panel
/// (the crawler controls), so the mode is a fixed string id, matching hiker's
/// `String`-keyed activity modes.
pub type Mode = String;

/// The id of the one primary side-panel mode (the crawler controls).
pub const MODE_CONTROLS: &str = "controls";

/// A workbench tab: one live browser page. A thin view-model — the real
/// per-tab state (browser, selection, url, texture) lives in
/// [`CrawlerApp::tabs`] keyed by [`Self::id`]. Carries a cached title so the
/// tab strip renders without reaching back into the app.
#[derive(Clone)]
pub struct CrawlerTab {
    /// Stable workbench handle; also the key into [`CrawlerApp::tabs`].
    pub id: TabId,
    /// Cached tab-strip label (the page host, or "New tab"). Refreshed each
    /// frame before [`egui_workbench::workspace::Workbench::ui`].
    pub cached_title: String,
}

impl Document for CrawlerTab {
    fn title(&self) -> egui::WidgetText {
        self.cached_title.clone().into()
    }

    fn wants_pane_content_inset(&self) -> bool {
        // The page body is its own edge-to-edge surface (URL bar + OSR
        // texture); skip the workbench's content inset so the page sits flush.
        false
    }
}

/// Per-frame `Host` adapter. Lives only for the duration of one
/// `Workbench::ui` call, borrowing the app.
pub struct CrawlerBehavior<'a> {
    pub app: &'a mut CrawlerApp,
}

impl Host<CrawlerTab, Mode> for CrawlerBehavior<'_> {
    fn pane_ui(&mut self, ui: &mut egui::Ui, tab: &mut CrawlerTab, _ctx: UiContext<'_>) {
        self.app.page(ui, tab.id);
    }

    fn on_tab_close(&mut self, tab: &CrawlerTab) -> bool {
        // Drop the backing per-tab state (browser + selection + texture) when
        // a tab closes; allow the workbench to remove the pane.
        self.app.tabs.remove(&tab.id);
        true
    }

    fn activity_items(&self) -> Vec<Item<Mode>> {
        vec![Item {
            mode: MODE_CONTROLS.to_string(),
            icon: None,
            label: "Crawler".to_string(),
            badge: None,
        }]
    }

    fn side_bar_title(&self, _mode: &Mode) -> egui::WidgetText {
        "Crawler".into()
    }

    fn side_bar_ui(&mut self, ui: &mut egui::Ui, _mode: &Mode) {
        self.app.side_panel(ui);
    }

    fn side_bar_action_buttons(&mut self, ui: &mut egui::Ui, _mode: &Mode) {
        if ui.button("+ New tab").on_hover_text("Open a new browser tab").clicked() {
            self.app.open_new_tab();
        }
    }

    fn status_bar_ui(&mut self, ui: &mut egui::Ui) {
        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
            let url = self
                .app
                .active_tab()
                .and_then(crate::app::TabState::engine_url)
                .unwrap_or_else(|| "no page".to_string());
            ui.weak(url);
        });
    }

    fn theme(&self, style: &egui::Style) -> Palette {
        // Match hiker: drop the focused-group overlay border (reads as stray
        // white padding on the light theme).
        Palette {
            focused_group_border_width: 0.0,
            ..Palette::from_egui_style(style)
        }
    }
}
