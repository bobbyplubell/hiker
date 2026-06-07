//! App-side binding of the generic activity registry that lives in the
//! `egui_workbench` shell. The trait machinery (`Activity`/`View`/the
//! surface sub-traits/`ActivityRegistry`/`ActionError`/`split_view_id`)
//! is the shell's generic family, re-exported here; this module supplies
//! only the hiker-specific glue: the umbrella requirement trait
//! [`AppCtx`], the per-surface [`SurfaceCtx`] borrow it hands back, and
//! the `impl AppCtx for AppState` whose body resolves the disjoint
//! `AppState`-field borrows per `state_key`. See `docs/activity-registry.md`.
//!
//! App-owned activities impl `Activity<dyn AppCtx>`; each `View::render`
//! opens its borrow with one line — `ctx.surface_ctx(self.state_key())` —
//! then works against `SurfaceCtx`, with simultaneous disjoint access to
//! `services`/`state`/`active_path`/`defer`.
//!
//! Layout note: the app-side glue (trait + session + builtin list) all
//! lives in this single root file rather than each in its own sibling
//! under `app/src/activity/`. Per-piece sibling files would each fall
//! under `scripts/check-splits.py`'s 20-line minimum, and `pub use`
//! re-exports are banned by clippy's anti-arbitrary-split posture, so the
//! file would be the wrong boundary either way.

use std::any::Any;
use std::sync::{Arc, RwLock};

use egui_workbench::activity::{Activity, Ctx};
use hiker_core::config::Config;

use crate::state::{Services, Toast};

/// The app's activity registry: the shell's generic `ActivityRegistry`
/// bound to hiker's umbrella [`AppCtx`]. A type alias (not a `pub use`
/// re-export, which the project's clippy posture forbids) so consumers
/// name `crate::activity::ActivityRegistry` without spelling the binding.
pub type ActivityRegistry = egui_workbench::activity::ActivityRegistry<dyn AppCtx>;

/// Parse a wire view-id back into `(activity_id, view_key)`. Thin
/// forwarder to the shell's [`egui_workbench::activity::split_view_id`]
/// so call sites stay on `crate::activity::split_view_id`.
pub fn split_view_id(view_id: &str) -> (&str, &str) {
    egui_workbench::activity::split_view_id(view_id)
}

// ---- Per-surface borrow ----------------------------------------------

/// Per-surface context borrow. Built fresh inside [`AppCtx::surface_ctx`]
/// right before invoking a surface and dropped immediately after, so the
/// borrow window is single-call. Surfaces touch only their own `state`
/// slice plus the shared `services`/`toasts`/`config`/`vault`,
/// never the wider `AppState`.
pub struct SurfaceCtx<'a> {
    pub services: &'a mut Services,
    pub toasts: &'a mut Vec<Toast>,
    pub config: &'a Arc<RwLock<Config>>,
    /// Read handle to the open vault (list/read files). Shared because
    /// several activities scan or read vault contents during render
    /// (backlinks, related, search). A disjoint borrow of
    /// `vault_session.vault`, alongside the `&mut services` borrow.
    pub vault: &'a Arc<hiker_core::vault::Vault>,
    /// Per-activity opaque state slice. Each activity downcasts to its
    /// own state struct (e.g.
    /// `ctx.state.downcast_mut::<State>().unwrap()`). The
    /// registry shell never sees the concrete type.
    pub state: &'a mut dyn Any,
    /// Vault-relative path of the active note, if a buffer is focused.
    /// A cheap read filled at session-build time so a surface reads "the
    /// current note" here instead of reaching into `session`. Shared
    /// because ≥2 activities (backlinks, related, search, ...) need it.
    pub active_path: Option<String>,
    /// Deferred-effect sink. A surface pushes closures here (via
    /// [`SurfaceCtx::defer`]) for cross-cutting effects that need broad
    /// `&mut AppState` (open a note, reveal a path) and so can't run
    /// inside the narrow session borrow. The consumer drains them right
    /// after the surface returns. Mirrors helix's compositor callback
    /// queue; closures reuse the existing `&mut AppState` helpers.
    pub effects: &'a mut Vec<Effect>,
}

/// A deferred effect queued by an activity surface and applied by the
/// consumer after the surface returns, with full `&mut AppState`.
///
/// `+ Send` so the field that holds the queue ([`AppState::pending_effects`])
/// doesn't make `AppState` itself `!Send` — the vault-switch path builds a
/// fresh `AppState` on a worker thread and sends it back to the UI thread.
/// The closures themselves only ever run on the UI thread (drained
/// synchronously right after each surface), so the bound costs nothing in
/// practice and every existing effect already captures only `Send` data.
pub type Effect = Box<dyn FnOnce(&mut crate::state::AppState) + Send>;

impl SurfaceCtx<'_> {
    /// Queue a cross-cutting effect to run after the surface returns,
    /// with full `&mut AppState`. Use for "open this note", "reveal in
    /// tree", etc. — anything the narrow session can't do inline.
    pub fn defer(&mut self, f: impl FnOnce(&mut crate::state::AppState) + Send + 'static) {
        self.effects.push(Box::new(f));
    }
}

// ---- Umbrella requirement trait --------------------------------------

/// Hiker's umbrella requirement trait — the `'static` trait object the
/// shell binds as `Activity<dyn AppCtx>`. It supertraits the universal
/// [`Ctx`] marker base and adds the one accessor that hands an activity
/// its per-surface [`SurfaceCtx`]. App-owned activities take
/// `&mut dyn AppCtx` and open their borrow with
/// `ctx.surface_ctx(self.state_key())`; the resolution of the disjoint
/// `AppState`-field borrows + the `state_key` → slice match is
/// irreducibly app-specific and lives in [`AppCtx::surface_ctx`].
pub trait AppCtx: Ctx {
    /// Build a [`SurfaceCtx`] scoped to `state_key`. Returns `None` when
    /// the key doesn't resolve to a state slice on `AppState` — which,
    /// now that every activity is migrated, means a bug (a registered
    /// view whose `state_key` has no slice), logged at the `_` arm. The
    /// key is a view's [`View::state_key`].
    fn surface_ctx(&mut self, state_key: &str) -> Option<SurfaceCtx<'_>>;
}

// `Ctx` is a marker base (no methods); hiker reaches per-surface state
// through [`AppCtx::surface_ctx`], not a universal single-slice accessor.
impl Ctx for crate::state::AppState {}

impl AppCtx for crate::state::AppState {
    fn surface_ctx(&mut self, state_key: &str) -> Option<SurfaceCtx<'_>> {
        let app = self;
        // Compute the active note path first (immutable read, released
        // before the disjoint &mut field borrows below). A canvas tab with a File
        // node in inline-edit surfaces the EDITED note as the active note, so the
        // context panel follows what you're editing on the canvas rather than the
        // `.canvas` file itself. status: canvas-inline-edit
        let active_path = app.session.active_tab.and_then(|id| {
            crate::panels::canvas::inline_edited_note(app, id).or_else(|| {
                app.tab_by_id(id)
                    .and_then(crate::tab::Tab::buffer_path)
                    .map(str::to_string)
            })
        });
        // The state-slice lookup is inlined here so the compiler can
        // verify the disjoint borrows of `AppState` fields. A helper
        // returning a single `&mut dyn Any` can't be combined with the
        // parallel borrows of the other fields.
        let state: &mut dyn Any = match state_key {
            "files" => &mut app.file_tree_state,
            "clusters" => &mut app.clusters_state,
            "trails" => &mut app.trails_state,
            "vault" => &mut app.vault_state,
            "backlinks" => &mut app.backlinks_state,
            "appears-in" => &mut app.appears_in_state,
            "related" => &mut app.related_state,
            "search" => &mut app.search_state,
            "trash" => &mut app.trash_state,
            "canvases" => &mut app.canvases_activity_state,
            "chat" => &mut app.chat_state,
            _ => {
                tracing::warn!(
                    state_key,
                    "activity surface_ctx: no AppState slice for this state_key"
                );
                return None;
            }
        };
        Some(SurfaceCtx {
            services: &mut app.vault_session.services,
            toasts: &mut app.toasts,
            config: &app.vault_session.config,
            vault: &app.vault_session.vault,
            state,
            active_path,
            effects: &mut app.pending_effects,
        })
    }
}

// ---- Builtins --------------------------------------------------------

/// Hamburger-menu dispatch: look up `activity_id` in the registry and
/// invoke its `HamburgerEntry::invoke`. No-op when the activity is
/// missing or has no hamburger entry. [feature-consumer-hamburger]
pub fn dispatch_hamburger(app: &mut crate::state::AppState, activity_id: &str) {
    let activities = app.activities.clone();
    let Some(activity) = activities.by_id(activity_id) else {
        return;
    };
    let Some(entry) = activity.hamburger() else {
        return;
    };
    entry.invoke(app);
    for eff in std::mem::take(&mut app.pending_effects) {
        eff(app);
    }
}

/// Construct the built-in activity list in sidebar/activity-bar order.
/// `Files` is first (the primary mode). Plugins are appended after
/// built-ins by callers once Phase 3's adapter lands.
pub fn builtin_activities() -> Vec<Arc<dyn Activity<dyn AppCtx>>> {
    vec![
        Arc::new(crate::files::Files) as Arc<dyn Activity<dyn AppCtx>>,
        Arc::new(crate::clusters::Clusters) as Arc<dyn Activity<dyn AppCtx>>,
        Arc::new(crate::trails::Trails) as Arc<dyn Activity<dyn AppCtx>>,
        Arc::new(crate::vault_view::Vault) as Arc<dyn Activity<dyn AppCtx>>,
        Arc::new(crate::canvas_activity::CanvasActivity) as Arc<dyn Activity<dyn AppCtx>>,
        Arc::new(crate::context::Context) as Arc<dyn Activity<dyn AppCtx>>,
        Arc::new(crate::search::Search) as Arc<dyn Activity<dyn AppCtx>>,
        Arc::new(crate::trash::Trash) as Arc<dyn Activity<dyn AppCtx>>,
        Arc::new(crate::chat::Chat) as Arc<dyn Activity<dyn AppCtx>>,
    ]
}

// ---- Tests -----------------------------------------------------------

/// App-side registry tests. The generic trait machinery (routing,
/// view-id parsing, `on_activity_bar`) is covered by the shell's own
/// tests; here we only guard the hiker-specific builtin list order.
#[cfg(test)]
mod registry_tests {
    use super::*;

    #[test]
    fn builtins_in_expected_order() {
        let reg = ActivityRegistry::build(builtin_activities());
        let ids: Vec<&str> = reg.iter().map(|f| f.id()).collect();
        assert_eq!(
            ids,
            vec![
                "files", "clusters", "trails", "vault", "canvases", "context", "search", "trash",
                "chat"
            ]
        );
    }
}
