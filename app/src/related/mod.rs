//! Related-notes view — sidebar surface listing vector-similar notes
//! for the active note. Migrated off `panels::related` + `panels_registry`
//! to a real `View` rendering through the narrow `activity::SurfaceCtx` (reads
//! via ctx; open-note deferred via `ctx.defer`). Surfaced by the
//! `context` container activity (`crate::context`) alongside backlinks.
//! [feature-related-migration]
//!
//! For the active note, calls `Store::related_notes` (vector-similarity
//! query) and renders the hits as plain icon+title rows with a markdown
//! hover-preview (uniform with backlinks / appears-in), in place of the
//! older rich snippet cards. The query result is cached
//! on `State` and recomputed only when the active-note path changes —
//! firing it every frame holds the shared `read_store` mutex through a
//! non-trivial SQL pass and causes visible scroll lag across every other
//! pane that touches the same store.

use eframe::egui;

use crate::editor_pane;
use egui_workbench::activity::View;
use crate::activity::{AppCtx, SurfaceCtx};
use crate::icons;
use crate::search::DiscoveryHit;
use crate::state::Services;
use hiker_core::store::service::IndexerQueryApi;
use hiker_theme as theme;

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

/// Zero-sized `View` descriptor for related notes. State lives in
/// `AppState::related_state`; the surface reaches it via
/// `ctx.state.downcast_mut::<State>()`. Exposed so the `context`
/// container activity can list it among its `views()`.
pub struct RelatedSidebar;

impl View<dyn AppCtx> for RelatedSidebar {
    fn id(&self) -> &'static str {
        "related"
    }
    fn render(&self, ui: &mut egui::Ui, ctx: &mut (dyn AppCtx + 'static)) {
        let Some(mut ctx) = ctx.surface_ctx(self.state_key()) else {
            return;
        };
        let ctx = &mut ctx;
        // The workbench accordion owns the section header + collapse;
        // the body is just the content. [feature-panel-single-accordion]
        ui.add_space(8.0);
        render_body(ui, ctx);
    }
}

fn render_body(ui: &mut egui::Ui, ctx: &mut SurfaceCtx<'_>) {
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
        // Uniform with backlinks / appears-in: a plain icon+title row with a
        // markdown hover-preview, in place of the old rich snippet card. The
        // preview is the affordance for "what's in this note" now, so the row
        // stays terse. [feature-related-migration]
        let resp = ui
            .add(egui::Button::image_and_text(
                icons::ICONS.image(icons::Icon::File),
                related_label(hit),
            ))
            .on_hover_text(&hit.path);
        if resp.hovered() {
            crate::widgets::preview::register_note_hover(ui, resp.rect, &hit.path);
        }
        crate::item_menu::attach_note_item_menu(
            &resp,
            ctx,
            &hit.path,
            crate::item_menu::BaseOpts { reveal: true },
        );
        if resp.clicked() {
            let path = hit.path.clone();
            ctx.defer(move |app| editor_pane::open_file(app, &path, /* sticky */ false));
        }
    }
}

/// The row label for a related hit: the note's indexed title when present, else
/// the basename without its `.md` extension. [feature-related-migration]
fn related_label(hit: &DiscoveryHit) -> String {
    if !hit.title.trim().is_empty() {
        return hit.title.clone();
    }
    let base = hit.path.rsplit('/').next().unwrap_or(hit.path.as_str());
    base.strip_suffix(".md").unwrap_or(base).to_string()
}

/// Run the similarity query against the read store and return a fresh
/// `State` cache for `path`. Returns `None` on a transient failure (lock
/// poisoned, query error) so the caller leaves the old cache intact and
/// retries next frame, matching the legacy early-return behavior. Called
/// only when the active path changes.
/// status: store-path-is-identity
fn refresh_cache(services: &Services, path: &str) -> Option<State> {
    let store = services.read_store.lock().ok()?;
    query_related(&*store, path)
}

/// The pure similarity query against the indexer service. Depends only on
/// [`IndexerQueryApi`], not on how the store is held or locked, so it can move
/// with the feature when `related` becomes a self-contained extension.
fn query_related(index: &dyn IndexerQueryApi, path: &str) -> Option<State> {
    let mut out = State {
        cached_for: Some(path.to_string()),
        ..State::default()
    };
    // The note's path IS its identity (`store-path-is-identity`); the related
    // query keys on it directly.
    let note_path = match index.get_note_by_path(path) {
        Ok(Some(row)) => row.path,
        Ok(None) => {
            out.cached_unindexed = true;
            return Some(out);
        }
        Err(_) => return None,
    };
    let raw = index.related_notes(&note_path, 8).unwrap_or_default();
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
