//! Related-notes feature — sidebar surface listing vector-similar notes
//! for the active note. Migrated off `panels::related` + `panels_registry`
//! to a real `Feature` rendering through the narrow `feature::Ctx` (reads
//! via ctx; open-note deferred via `ctx.defer`). [feature-related-migration]
//!
//! For the active note, calls `Store::related_notes` (vector-similarity
//! query) and renders the hits as result cards. The query result is cached
//! on `State` and recomputed only when the active-note path changes —
//! firing it every frame holds the shared `read_store` mutex through a
//! non-trivial SQL pass and causes visible scroll lag across every other
//! pane that touches the same store.

use eframe::egui;

use crate::editor_pane;
use crate::feature::{Ctx, Feature, SidebarSurface};
use crate::search::{CardAction, DiscoveryHit, result_card};
use crate::state::Services;
use crate::theme;

/// Per-feature UI state for the related-notes surface — a cached
/// similarity query, recomputed when the active note changes. Owned by
/// `AppState::related_state` (top-level, per `feature-state-ownership`).
/// Inlined here rather than a sibling `state.rs` (mod.rs is exempt from
/// `scripts/check-splits.py`'s 20-line minimum).
#[derive(Default)]
pub struct State {
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

impl State {
    /// Drop the cache so the next render re-fires the query. Wire to a
    /// watcher event when one becomes available; for now nobody calls
    /// this and the cache invalidates only on active-tab change.
    #[allow(dead_code)]
    pub fn invalidate(&mut self) {
        self.cached_for = None;
    }
}

/// Zero-sized `Feature` descriptor for related notes. State lives in
/// `AppState::related_state`; the surface reaches it via
/// `Ctx::state.downcast_mut::<State>()`.
pub struct Related;

impl Feature for Related {
    fn id(&self) -> &'static str {
        "related"
    }
    fn label(&self) -> &'static str {
        "Related"
    }
    fn icon(&self) -> egui::Image<'static> {
        crate::icons::ICONS.image(crate::icons::Icon::Graph)
    }
    fn sidebar(&self) -> Option<&dyn SidebarSurface> {
        Some(&RelatedSidebar)
    }
}

struct RelatedSidebar;

impl SidebarSurface for RelatedSidebar {
    fn render(&self, ui: &mut egui::Ui, ctx: &mut Ctx<'_>) {
        // The workbench accordion owns the section header + collapse;
        // the body is just the content. [feature-panel-single-accordion]
        ui.add_space(8.0);
        render_body(ui, ctx);
    }
}

fn render_body(ui: &mut egui::Ui, ctx: &mut Ctx<'_>) {
    let Some(rel) = ctx.active_path.clone() else {
        // No active buffer; clear the cache so reopening re-fires.
        let st = ctx.state.downcast_mut::<State>().expect("related state");
        st.cached_for = None;
        st.cached_hits.clear();
        ui.label(
            egui::RichText::new("(open a note to see related)")
                .color(theme::muted())
                .small(),
        );
        return;
    };

    // Re-fire the lookup only when the active path changes. The
    // similarity query is a SQL+vector pass that holds the shared
    // read_store mutex; running it every frame stalls scroll repaints
    // across every other pane that touches the same store.
    let cache_hit = ctx
        .state
        .downcast_ref::<State>()
        .expect("related state")
        .cached_for
        .as_deref()
        == Some(rel.as_str());
    if !cache_hit {
        if let Some(computed) = refresh_cache(ctx.services, &rel) {
            let st = ctx.state.downcast_mut::<State>().expect("related state");
            *st = computed;
        }
    }

    let st = ctx.state.downcast_ref::<State>().expect("related state");
    if st.cached_unindexed {
        ui.label(
            egui::RichText::new(format!("({rel} not indexed yet)"))
                .color(theme::muted())
                .small(),
        );
        return;
    }
    if st.cached_empty {
        ui.label(
            egui::RichText::new(format!("(no related notes for {rel})"))
                .color(theme::muted())
                .small(),
        );
        return;
    }

    // Clone the cached hits out so the render loop doesn't hold an
    // `&state` borrow while we may queue an open-file effect.
    let hits = st.cached_hits.clone();
    for hit in &hits {
        if let CardAction::Open { .. } = result_card(ui, hit, /*allow_context=*/ false) {
            let path = hit.path.clone();
            ctx.defer(move |app| editor_pane::open_file(app, &path, /* sticky */ false));
        }
    }
}

/// Run the similarity query against the read store and return a fresh
/// `State` cache for `path`. Returns `None` on a transient failure (lock
/// poisoned, query error) so the caller leaves the old cache intact and
/// retries next frame, matching the legacy early-return behavior. Called
/// only when the active path changes.
/// status: store-id-from-oplog
fn refresh_cache(services: &Services, path: &str) -> Option<State> {
    let mut out = State {
        cached_for: Some(path.to_string()),
        ..State::default()
    };
    let store = services.read_store.lock().ok()?;
    let note_id = match store.get_note_by_path(path) {
        Ok(Some(row)) => row.id,
        Ok(None) => {
            out.cached_unindexed = true;
            return Some(out);
        }
        Err(_) => return None,
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
    out.cached_empty = hits.is_empty();
    out.cached_hits = hits;
    Some(out)
}
