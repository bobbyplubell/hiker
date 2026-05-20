//! Related-notes sub-panel of the right-hand discovery pane.
//!
//! For the active note, calls `Store::related_notes` (vector-similarity
//! query) and renders the hits as result cards. The query result is
//! cached on `RelatedState` and recomputed only when the active note
//! path changes — previously the query fired every frame, holding the
//! shared `read_store` mutex through a non-trivial SQL pass and causing
//! visible scroll lag across every other pane that touched the same
//! store (e.g. the editor's `note_properties` check in `panels::buffer`).

use eframe::egui;

use crate::editor_pane;
use crate::panels::search::{CardAction, DiscoveryHit, result_card};
use crate::state::AppState;
use crate::theme;

pub struct RelatedState {
    /// Per-section collapse state. Persisted per-vault.
    pub related_expanded: bool,
    /// Cached query result. Re-fired when `cached_for` no longer matches
    /// the active note path.
    pub cached_hits: Vec<DiscoveryHit>,
    /// The active-note path the cached hits correspond to. `None` means
    /// the cache is invalid (e.g. on startup, or after the watcher
    /// invalidates it).
    pub cached_for: Option<String>,
    /// True when the last lookup found the note in the index but
    /// returned zero hits. Distinguishes "no related notes" from
    /// "not yet indexed" without re-firing the query each frame.
    pub cached_empty: bool,
    /// True when the active note is not yet in the index (no row in
    /// `notes`). Also a sticky cache flag.
    pub cached_unindexed: bool,
}

impl Default for RelatedState {
    fn default() -> Self {
        Self {
            related_expanded: true,
            cached_hits: Vec::new(),
            cached_for: None,
            cached_empty: false,
            cached_unindexed: false,
        }
    }
}

impl RelatedState {
    pub fn from_config(cfg: &hiker_core::config::Config) -> Self {
        Self {
            related_expanded: cfg.search.sections.related_expanded,
            ..Self::default()
        }
    }

    /// Drop the cache so the next render re-fires the query. Wire to a
    /// watcher event when one becomes available; for now nobody calls
    /// this and the cache invalidates only on active-tab change.
    #[allow(dead_code)]
    pub fn invalidate(&mut self) {
        self.cached_for = None;
    }
}

pub fn show(ui: &mut egui::Ui, app: &mut AppState) {
    ui.add_space(12.0);
    let related_expanded = app.panels.related.related_expanded;
    if crate::panels::discovery_pane::collapsible_header(
        ui,
        "related-notes",
        "Related notes",
        related_expanded,
        0,
    ) {
        app.panels.related.related_expanded = !related_expanded;
        crate::panels::search::persist_search_setting(
            app,
            "search.sections.related_expanded",
            serde_json::json!(app.panels.related.related_expanded),
        );
    }
    if app.panels.related.related_expanded {
        related_for_active(ui, app);
    }
}

fn related_for_active(ui: &mut egui::Ui, app: &mut AppState) {
    let active_path = app.session.active_tab.and_then(|id| {
        app.tab_by_id(id)
            .and_then(|t| t.buffer_path().map(str::to_string))
    });
    let Some(rel) = active_path else {
        // No active buffer; clear the cache so reopening a note re-fires.
        app.panels.related.cached_for = None;
        app.panels.related.cached_hits.clear();
        ui.label(
            egui::RichText::new("(open a note to see related)")
                .color(theme::muted())
                .small(),
        );
        return;
    };

    // Re-fire the lookup only when the active path changes. The
    // similarity query is a SQL+vector pass that holds the shared
    // read_store mutex; running it every frame stalls scroll
    // repaints across every other pane that touches the same store.
    let cache_hit = app
        .panels
        .related
        .cached_for
        .as_deref()
        .map(|p| p == rel)
        .unwrap_or(false);
    if !cache_hit {
        refresh_cache(app, &rel);
    }

    if app.panels.related.cached_unindexed {
        ui.label(
            egui::RichText::new(format!("({} not indexed yet)", rel))
                .color(theme::muted())
                .small(),
        );
        return;
    }
    if app.panels.related.cached_empty {
        ui.label(
            egui::RichText::new(format!("(no related notes for {})", rel))
                .color(theme::muted())
                .small(),
        );
        return;
    }

    let mut to_open: Option<String> = None;
    // Clone the cached hits out so the render loop doesn't hold an
    // `&app.panels.related` borrow while we may call `editor_pane::
    // open_file(app, ...)` (which needs `&mut app`).
    let hits = app.panels.related.cached_hits.clone();
    for hit in &hits {
        if let CardAction::Open { .. } = result_card(ui, hit, /*allow_context=*/ false) {
            to_open = Some(hit.path.clone());
        }
    }
    if let Some(p) = to_open {
        editor_pane::open_file(app, &p, /* sticky */ false);
    }
}

/// Run the similarity query against the read store and store the
/// result on `RelatedState`. Called only when the active path changes.
fn refresh_cache(app: &mut AppState, path: &str) {
    let store_mutex = &app.vault_session.services.read_store;
    let store = match store_mutex.lock() {
        Ok(s) => s,
        Err(_) => return,
    };
    let note_id = match store.id_for_path(path) {
        Ok(Some(id)) => id,
        Ok(None) => {
            drop(store);
            app.panels.related.cached_hits.clear();
            app.panels.related.cached_empty = false;
            app.panels.related.cached_unindexed = true;
            app.panels.related.cached_for = Some(path.to_string());
            return;
        }
        Err(_) => return,
    };
    let raw = store.related_notes(&note_id, 8).unwrap_or_default();
    drop(store);
    let hits: Vec<DiscoveryHit> = raw
        .into_iter()
        .map(|h| DiscoveryHit {
            path: h.path,
            title: h.title,
            heading_path: h.best_heading_path,
            snippet: h.snippet,
            score: h.score,
            source_tag: None,
            chunk_index: 0,
        })
        .collect();
    app.panels.related.cached_empty = hits.is_empty();
    app.panels.related.cached_unindexed = false;
    app.panels.related.cached_hits = hits;
    app.panels.related.cached_for = Some(path.to_string());
}
