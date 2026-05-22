//! Sync the auto-managed Search panel with `view.search.active`, and keep
//! the visible match list in sync with the query. Lifted out of
//! `widget.rs` to keep that file under the workspace's per-file length cap.

use editor_view::panels::Panel;
use editor_view::panels::PanelKind;
use editor_view::panels::PanelPlacement;

use super::Widget;

pub(super) const SEARCH_PANEL_ID: u64 = 0x5EA8_C400_0000_0001;

impl<'a> Widget<'a> {
    /// Synchronize the auto-managed Search panel with `view.search.active`.
    /// When the user opens search (Cmd-F), push a bottom-anchored Search panel;
    /// when they close it, remove any registered Search panel. The panel uses a
    /// reserved id (`SEARCH_PANEL_ID`) so it can be re-found across frames.
    pub(super) fn sync_search_panel(&mut self) {
        let view = &mut *self.view;
        let has = view
            .panels
            .panels
            .iter()
            .any(|p| matches!(p.kind, PanelKind::Search));
        if view.search.active && !has {
            view.panels.panels.push(Panel {
                id: SEARCH_PANEL_ID,
                placement: PanelPlacement::Bottom,
                height: 36.0,
                kind: PanelKind::Search,
            });
        } else if !view.search.active && has {
            view.panels
                .panels
                .retain(|p| !matches!(p.kind, PanelKind::Search));
        }
    }

    /// Re-run the search after panel interaction so the visible match list and
    /// decorations stay in sync with `view.search.query` / flags. Cheap when the
    /// query is empty (early-return inside `run_search`).
    pub(super) fn refresh_search_matches(&mut self) {
        let view = &mut *self.view;
        let state = &*self.state;
        if !view.search.active {
            return;
        }
        let matches = editor_view::find::run_search(state, &view.search.query, view.search.flags);
        if matches != view.search.matches {
            view.search.matches = matches;
            if view.search.matches.is_empty() {
                view.search.current_idx = None;
            } else if view
                .search
                .current_idx
                .map(|i| i >= view.search.matches.len())
                .unwrap_or(true)
            {
                view.search.current_idx = Some(0);
            }
        }
    }
}
