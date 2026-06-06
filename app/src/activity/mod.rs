//! Activity registry — small descriptor registry that App-shell consumers
//! (sidebar mode switcher, activity bar, hamburger menu, command palette,
//! keybind registry) iterate to discover what to render.
//!
//! Each `Activity` impl exposes a list of `View` render surfaces (plus
//! optional descriptors for the hamburger menu, etc.). An activity with
//! no views opts out of the activity bar. The registry is built once per vault in
//! `bootstrap::open_vault` and stashed on `AppState::activities`. New
//! consumers iterate the registry generically rather than hardcoding an
//! activity list. See `docs/activity-registry.md`.
//!
//! Layout note: trait, ctx, registry, and builtin-list helper all live
//! in this single root file rather than each in its own sibling under
//! `app/src/activity/`. Per-piece sibling files would each fall under
//! `scripts/check-splits.py`'s 20-line minimum (the shapes here are 5-20
//! lines each), and `pub use` re-exports are banned by clippy's
//! anti-arbitrary-split posture, so the file would be the wrong
//! boundary either way.

use std::any::Any;
use std::sync::{Arc, RwLock};

use eframe::egui;
use hiker_core::config::Config;

use crate::state::{NavState, Services, Toast};

// ---- Activity trait + surface sub-traits -----------------------------

/// A registered activity (a container of one or more `View`s).
/// Singleton in the registry; per-instance state (e.g. multiple tabs of
/// the same activity) lives in tab payloads, not here. An activity
/// declares its `views()`; descriptor accessors default to empty/`None`
/// so a new activity only implements what it wants.
pub trait Activity: Send + Sync {
    /// Stable kebab-case id, e.g. `"clusters"`. Used as the activity
    /// dispatch key and persisted in settings.
    fn id(&self) -> &'static str;
    /// Human-facing label, e.g. `"Cluster trees"`.
    fn label(&self) -> &'static str;
    /// Activity-bar / mode-button icon.
    fn icon(&self) -> egui::Image<'static>;
    /// Optional keybind chord descriptor (e.g. `"ctrl+shift+c"`); for
    /// the future `keybind-registry` consumer. v1 returns `None`.
    fn keybind_chord(&self) -> Option<&'static str> {
        None
    }

    /// Ordered list of render surfaces this activity contributes. A
    /// single-view activity returns one element (rendered headerless);
    /// multi-view containers return several. Empty (the default) opts
    /// out of the activity bar.
    fn views(&self) -> Vec<&dyn View> {
        Vec::new()
    }
    /// View-id for one of this activity's views, per the
    /// `"<activity>/<view>"` convention. Single-view activities (or a
    /// view whose id equals the activity id) collapse to the bare
    /// activity id, keeping wire ids byte-identical; multi-view
    /// containers slash. Centralized here — call sites never hand-build
    /// the id. See [`split_view_id`] for the inverse.
    fn view_id(&self, view: &dyn View) -> String {
        if self.views().len() <= 1 || view.id() == self.id() {
            self.id().to_string()
        } else {
            format!("{}/{}", self.id(), view.id())
        }
    }
    /// Whether this activity belongs on the PRIMARY (left) activity bar.
    /// Default: any activity with at least one `View` whose default
    /// location is the left bar. Right-bar activities (chat) render in
    /// the secondary side bar and are summoned via the right-sidebar
    /// toggle, not the activity strip. [feature-consumer-activity-bar]
    fn on_activity_bar(&self) -> bool {
        !self.views().is_empty()
            && matches!(self.default_location(), egui_workbench::side_bar::Location::LeftBar)
    }
    /// Which side bar this activity's views dock into by default. Left
    /// (the primary accordion driven by the activity bar) for most
    /// activities; right (the secondary accordion) for chat. The
    /// placement overlay can later override this per-view, but the
    /// declaration seeds the default. [feature-multi-region-sidebar]
    fn default_location(&self) -> egui_workbench::side_bar::Location {
        egui_workbench::side_bar::Location::LeftBar
    }
    fn hamburger(&self) -> Option<&dyn HamburgerEntry> {
        None
    }
    fn activity_bar(&self) -> Option<&dyn ActivityBarItem> {
        None
    }
    fn command_palette(&self, _ctx: &Ctx<'_>) -> Vec<PaletteCommand> {
        Vec::new()
    }

    /// Inter-activity action dispatch. Default returns `UnknownAction`.
    /// Activities that want to expose verbs to peers (or to the command
    /// palette / hamburger) override this. `args` and the return value
    /// are `serde_json::Value` so the seam stays generic without
    /// pulling each activity's typed args into the trait. Typed wrappers
    /// (e.g. `crate::trails::actions`) keep callers honest at the call
    /// site. [feature-inter-feature-actions]
    fn dispatch_action(
        &self,
        _ctx: &mut Ctx<'_>,
        action: &str,
        _args: serde_json::Value,
    ) -> Result<serde_json::Value, ActionError> {
        Err(ActionError::UnknownAction {
            activity: self.id().to_string(),
            action: action.to_string(),
        })
    }
}

/// Errors returned by `ActivityRegistry::invoke` /
/// `Activity::dispatch_action`. [feature-inter-feature-actions]
#[derive(Debug, thiserror::Error)]
pub enum ActionError {
    #[error("unknown activity `{0}`")]
    UnknownActivity(String),
    #[error("activity `{activity}` does not have action `{action}`")]
    UnknownAction { activity: String, action: String },
    #[error("invalid args for `{activity}::{action}`: {reason}")]
    InvalidArgs {
        activity: String,
        action: String,
        reason: String,
    },
    #[error("{0}")]
    Failed(String),
}

/// A render surface contributed by an activity. The mode-switcher (per
/// `feature-consumer-sidebar`) invokes `render` for the currently-active
/// view.
pub trait View: Send + Sync {
    /// Stable kebab-case id, unique within its activity (e.g.
    /// `"backlinks"`). For a single-view activity this equals the
    /// activity id. Composed into the wire `ViewId` via
    /// [`Activity::view_id`].
    fn id(&self) -> &'static str;
    /// Key for this view's per-activity state slice in [`with_ctx`].
    /// Defaults to the view id; a view backed by a differently-named
    /// `AppState` slice overrides it.
    fn state_key(&self) -> &'static str {
        self.id()
    }
    fn render(&self, ui: &mut egui::Ui, ctx: &mut Ctx<'_>);
}

/// Top-strip hamburger menu entry. Phase 2 — defined here so the trait
/// vocabulary is stable.
#[allow(dead_code)]
pub trait HamburgerEntry: Send + Sync {
    fn label(&self) -> &'static str;
    fn keybind_id(&self) -> Option<&'static str> {
        None
    }
    fn invoke(&self, ctx: &mut Ctx<'_>);
}

/// Activity-bar item override. By default an activity with a
/// `View` auto-renders an activity item using `Activity::icon`
/// + `Activity::label`; implementing this trait overrides those (e.g.
/// for a dynamic badge). Phase 2.
#[allow(dead_code)]
pub trait ActivityBarItem: Send + Sync {
    fn icon(&self) -> egui::Image<'static>;
    fn tooltip(&self) -> &'static str;
    fn invoke(&self, ctx: &mut Ctx<'_>);
}

/// Command-palette entry. The palette implementation is unfinished
/// (see `editor.md::command-palette`); this struct is the data shape
/// the palette will consume. Phase 2 wires the consumer.
#[allow(dead_code)]
pub struct PaletteCommand {
    pub id: &'static str,
    pub label: String,
    /// Erased action closure. The palette dispatcher invokes it inside
    /// a fresh `Ctx`.
    pub action: Box<dyn FnOnce(&mut Ctx<'_>) + Send>,
}

// ---- Per-surface borrow ----------------------------------------------

/// Per-surface context borrow. Built fresh inside the consumer (sidebar
/// switcher, palette dispatcher, ...) right before invoking a surface;
/// dropped immediately after so the borrow window is single-call.
/// Surfaces touch only their own `state` slice plus the shared
/// `services`/`nav`/`toasts`/`config`, never the wider `AppState`.
pub struct Ctx<'a> {
    pub services: &'a mut Services,
    pub nav: &'a mut NavState,
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
    /// A cheap read filled at ctx-build time so a surface reads "the
    /// current note" here instead of reaching into `session`. Shared
    /// because ≥2 activities (backlinks, related, search, ...) need it.
    pub active_path: Option<String>,
    /// Deferred-effect sink. A surface pushes closures here (via
    /// [`Ctx::defer`]) for cross-cutting effects that need broad
    /// `&mut AppState` (open a note, reveal a path) and so can't run
    /// inside the narrow ctx borrow. The consumer drains them right
    /// after the surface returns. Mirrors helix's compositor callback
    /// queue; closures reuse the existing `&mut AppState` helpers.
    pub effects: &'a mut Vec<Effect>,
}

/// A deferred effect queued by an activity surface and applied by the
/// consumer after the surface returns, with full `&mut AppState`.
pub type Effect = Box<dyn FnOnce(&mut crate::state::AppState)>;

impl Ctx<'_> {
    /// Queue a cross-cutting effect to run after the surface returns,
    /// with full `&mut AppState`. Use for "open this note", "reveal in
    /// tree", etc. — anything the narrow ctx can't do inline.
    pub fn defer(&mut self, f: impl FnOnce(&mut crate::state::AppState) + 'static) {
        self.effects.push(Box::new(f));
    }
}

// ---- Registry --------------------------------------------------------

/// Ordered list of `Arc<dyn Activity>` built once at vault open.
/// Consumers iterate via `iter()`. Order is meaningful: sidebar mode
/// buttons + activity items render in registry order.
pub struct ActivityRegistry {
    activities: Vec<Arc<dyn Activity>>,
}

impl ActivityRegistry {
    /// Build a registry from the given ordered list of activities.
    /// `bootstrap::open_vault` is the canonical call site; tests can
    /// build a registry directly with whichever activity set they need.
    pub fn build(activities: Vec<Arc<dyn Activity>>) -> Arc<Self> {
        Arc::new(Self { activities })
    }

    /// Iterate the registered activities in their stable order.
    #[allow(dead_code)]
    pub fn iter(&self) -> impl Iterator<Item = &Arc<dyn Activity>> {
        self.activities.iter()
    }

    /// O(N) lookup by stable id. N is expected to be ~5-15 activities so
    /// no hashmap is warranted (per the spec's "Lookup is O(N)" point).
    pub fn by_id(&self, id: &str) -> Option<&Arc<dyn Activity>> {
        self.activities.iter().find(|f| f.id() == id)
    }

    /// Dispatch `(activity_id, action, args)` through the registry —
    /// the entry point for inter-activity calls (one activity invoking
    /// another) and for the generic command-palette / hamburger
    /// dispatch path. Returns `UnknownActivity` when the id doesn't
    /// resolve; otherwise forwards to the activity's
    /// `dispatch_action`. [feature-inter-feature-actions]
    pub fn invoke(
        &self,
        ctx: &mut Ctx<'_>,
        activity_id: &str,
        action: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, ActionError> {
        let activity = self
            .activities
            .iter()
            .find(|f| f.id() == activity_id)
            .ok_or_else(|| ActionError::UnknownActivity(activity_id.to_string()))?;
        activity.dispatch_action(ctx, action, args)
    }
}

// ---- Builtins --------------------------------------------------------

/// Split a wire `ViewId` into `(activity_id, view_key)`. The convention
/// is `"<activity>/<view>"`; an id with no slash is a single-view
/// activity, so both halves are the whole string. Inverse of
/// [`Activity::view_id`]. The single point where the slash convention is
/// parsed — call sites never split the string themselves.
pub fn split_view_id(view_id: &str) -> (&str, &str) {
    view_id
        .split_once('/')
        .map_or((view_id, view_id), |(a, v)| (a, v))
}

/// Build a `Ctx` borrow scoped to `state_key` and run `f` against
/// it. Returns `None` when the key doesn't resolve to a state slice on
/// `AppState` (Phase 2 wires only the migrated activities). The key is
/// a view's [`View::state_key`] (== the old activity id for every
/// existing slice).
///
/// The state-slice lookup is inlined here so the compiler can verify
/// the four disjoint borrows of `AppState` fields. An
/// `activity_state_mut` helper returning a single `&mut dyn Any`
/// can't be combined with parallel borrows of the other fields.
pub fn with_ctx<R>(
    app: &mut crate::state::AppState,
    state_key: &str,
    effects: &mut Vec<Effect>,
    f: impl FnOnce(&mut Ctx<'_>) -> R,
) -> Option<R> {
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
        _ => return None,
    };
    let mut ctx = Ctx {
        services: &mut app.vault_session.services,
        nav: &mut app.session.nav,
        toasts: &mut app.toasts,
        config: &app.vault_session.config,
        vault: &app.vault_session.vault,
        state,
        active_path,
        effects,
    };
    Some(f(&mut ctx))
}

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
    let mut effects: Vec<Effect> = Vec::new();
    with_ctx(app, activity_id, &mut effects, |ctx| entry.invoke(ctx));
    for eff in effects {
        eff(app);
    }
}

/// Construct the built-in activity list in sidebar/activity-bar order.
/// `Files` is first (the primary mode). Plugins are appended after
/// built-ins by callers once Phase 3's adapter lands.
pub fn builtin_activities() -> Vec<Arc<dyn Activity>> {
    vec![
        Arc::new(crate::files::Files) as Arc<dyn Activity>,
        Arc::new(crate::clusters::Clusters) as Arc<dyn Activity>,
        Arc::new(crate::trails::Trails) as Arc<dyn Activity>,
        Arc::new(crate::vault_view::Vault) as Arc<dyn Activity>,
        Arc::new(crate::canvas_activity::CanvasActivity) as Arc<dyn Activity>,
        Arc::new(crate::context::Context) as Arc<dyn Activity>,
        Arc::new(crate::search::Search) as Arc<dyn Activity>,
        Arc::new(crate::trash::Trash) as Arc<dyn Activity>,
        Arc::new(crate::chat::Chat) as Arc<dyn Activity>,
    ]
}

// ---- Tests -----------------------------------------------------------

/// Registry + dispatch tests. Use synthetic `Activity` impls so the
/// tests don't need a real `AppState` / `Services` /
/// `VaultSession`; the seam shape is what we're guarding here, not
/// any particular activity's behavior.
#[cfg(test)]
mod registry_tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A no-state synthetic activity whose `dispatch_action` records
    /// every call into a shared counter and echoes the args back as
    /// the return value. Lets a test cross-invoke between two
    /// activities through the registry and confirm the routing.
    struct EchoActivity {
        id: &'static str,
        calls: Arc<AtomicUsize>,
    }

    impl Activity for EchoActivity {
        fn id(&self) -> &'static str {
            self.id
        }
        fn label(&self) -> &'static str {
            "Echo"
        }
        fn icon(&self) -> egui::Image<'static> {
            // Tests never actually render; an empty image is fine.
            egui::Image::new(egui::ImageSource::Bytes {
                uri: "tests/echo".into(),
                bytes: egui::load::Bytes::Static(&[]),
            })
        }
        fn dispatch_action(
            &self,
            _ctx: &mut Ctx<'_>,
            action: &str,
            args: serde_json::Value,
        ) -> Result<serde_json::Value, ActionError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match action {
                "echo" => Ok(json!({ "from": self.id, "args": args })),
                _ => Err(ActionError::UnknownAction {
                    activity: self.id.to_string(),
                    action: action.to_string(),
                }),
            }
        }
    }

    /// Run `f` with a `Ctx` whose state slot points at `state` and
    /// whose service borrows are filled from `services_holder`. The
    /// synthetic activities only touch `state`; the other fields are
    /// present so the trait signature matches.
    fn run_with_state<R>(
        state: &mut dyn Any,
        f: impl FnOnce(&mut Ctx<'_>) -> R,
    ) -> R {
        // Construct just enough surrounding state for `Ctx`. We
        // can't build a real `Services` in the unit test, so we use
        // a small in-memory holder and `transmute`-free pattern:
        // declare a parallel struct mirroring `Ctx`'s fields with
        // standalone owners.
        let mut nav = crate::state::NavState::default();
        let mut toasts: Vec<crate::state::Toast> = Vec::new();
        let cfg = std::sync::Arc::new(std::sync::RwLock::new(
            hiker_core::config::Config::default(),
        ));
        // SAFETY-EQUIVALENT: `Services` requires a live vault to
        // construct. For tests we never read `ctx.services`, so we
        // bypass it by constructing a `Ctx`-shaped tuple with a
        // `&mut Services` we leak from an `Option::None` would be
        // unsound. Instead, store services in a `MaybeUninit` and
        // assert at runtime the test path never reads from it.
        //
        // A cleaner shape: have the synthetic activities not need
        // `Ctx` at all. But the trait fixes the signature. So we
        // build a fake Services via mem::zeroed only if the test
        // genuinely needs the field; the echo activities under test
        // touch only `state`, so we use a small dance: construct a
        // Ctx whose `services` field is borrowed from a properly
        // constructed (but empty-vault) Services... which is heavy.
        //
        // Pragmatic punt: leak a `'static` Services built at test
        // setup. We can't easily, so the tests below DO NOT call
        // any function that reads `ctx.services`. The Box leak is
        // not necessary because we can re-arrange the test to use a
        // raw `&mut` to a heap-allocated stub via Box::leak with a
        // hand-rolled stub Services. Building one is intrusive
        // (Services has 14 fields, most Arc<...> to real engines).
        //
        // Therefore: skip the dance and assert through the
        // `dispatch_action` codepath using a no-op Ctx that is
        // **never invoked** by these tests on the services field.
        // We achieve that by using `ActivityRegistry::invoke` with a
        // synthetic Activity impl whose dispatch_action ignores ctx
        // entirely. The tests below build the Ctx by hand using
        // `MaybeUninit` for the services slot and never touch it.
        use std::mem::MaybeUninit;
        let mut services_uninit: MaybeUninit<crate::state::Services> = MaybeUninit::uninit();
        // SAFETY: `services_uninit` is never read while uninitialized
        // because the synthetic activities under test ignore the field.
        // The reference's lifetime ends with the function; the
        // uninitialized memory is then dropped without being read.
        // `MaybeUninit` does not run `Drop`.
        let services_ref: &mut crate::state::Services =
            unsafe { &mut *services_uninit.as_mut_ptr() };
        // Same punt for `vault`: the echo activities under test never read
        // it, so an uninit `Arc<Vault>` reference is never dereferenced.
        // `MaybeUninit` does not run `Drop`, so the uninit Arc is never
        // dropped either.
        let vault_uninit: MaybeUninit<std::sync::Arc<hiker_core::vault::Vault>> =
            MaybeUninit::uninit();
        let vault_ref: &std::sync::Arc<hiker_core::vault::Vault> =
            unsafe { &*vault_uninit.as_ptr() };
        let mut effects: Vec<Effect> = Vec::new();
        let mut ctx = Ctx {
            services: services_ref,
            nav: &mut nav,
            toasts: &mut toasts,
            config: &cfg,
            vault: vault_ref,
            state,
            active_path: None,
            effects: &mut effects,
        };
        f(&mut ctx)
    }

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

    #[test]
    fn invoke_routes_to_named_activity() {
        let calls_a = Arc::new(AtomicUsize::new(0));
        let calls_b = Arc::new(AtomicUsize::new(0));
        let reg = ActivityRegistry::build(vec![
            Arc::new(EchoActivity {
                id: "alpha",
                calls: calls_a.clone(),
            }) as Arc<dyn Activity>,
            Arc::new(EchoActivity {
                id: "beta",
                calls: calls_b.clone(),
            }) as Arc<dyn Activity>,
        ]);
        let mut sink: () = ();
        let out = run_with_state(&mut sink, |ctx| {
            reg.invoke(ctx, "beta", "echo", json!({"n": 1})).unwrap()
        });
        assert_eq!(out["from"], "beta");
        assert_eq!(calls_b.load(Ordering::SeqCst), 1);
        assert_eq!(calls_a.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn invoke_unknown_activity_errors() {
        let reg = ActivityRegistry::build(vec![]);
        let mut sink: () = ();
        let err = run_with_state(&mut sink, |ctx| {
            reg.invoke(ctx, "nope", "x", json!(null)).unwrap_err()
        });
        assert!(matches!(err, ActionError::UnknownActivity(ref s) if s == "nope"));
    }

    #[test]
    fn invoke_unknown_action_errors() {
        let calls = Arc::new(AtomicUsize::new(0));
        let reg = ActivityRegistry::build(vec![Arc::new(EchoActivity {
            id: "alpha",
            calls,
        }) as Arc<dyn Activity>]);
        let mut sink: () = ();
        let err = run_with_state(&mut sink, |ctx| {
            reg.invoke(ctx, "alpha", "no_such_action", json!(null))
                .unwrap_err()
        });
        match err {
            ActionError::UnknownAction { activity, action } => {
                assert_eq!(activity, "alpha");
                assert_eq!(action, "no_such_action");
            }
            other => panic!("got {other:?}"),
        }
    }

    /// Two activities can invoke each other through the registry
    /// without a circular-dep cycle: activity `caller` calls
    /// `registry.invoke("callee", ...)` from inside its own
    /// `dispatch_action`. Models the real Phase 3 pattern of one
    /// activity triggering another's verb (e.g. boards adding to a
    /// trail) through the seam rather than via direct module access.
    #[test]
    fn activities_can_invoke_each_other() {
        let calls_callee = Arc::new(AtomicUsize::new(0));
        // Caller forwards every action to "callee" through a shared
        // registry handle resolved from a thread_local set up below.
        struct Caller {
            registry: std::sync::Mutex<Option<Arc<ActivityRegistry>>>,
        }
        impl Activity for Caller {
            fn id(&self) -> &'static str {
                "caller"
            }
            fn label(&self) -> &'static str {
                "Caller"
            }
            fn icon(&self) -> egui::Image<'static> {
                egui::Image::new(egui::ImageSource::Bytes {
                    uri: "tests/caller".into(),
                    bytes: egui::load::Bytes::Static(&[]),
                })
            }
            fn dispatch_action(
                &self,
                ctx: &mut Ctx<'_>,
                action: &str,
                args: serde_json::Value,
            ) -> Result<serde_json::Value, ActionError> {
                let reg = self
                    .registry
                    .lock()
                    .unwrap()
                    .clone()
                    .expect("registry set");
                reg.invoke(ctx, "callee", action, args)
            }
        }
        let caller = Arc::new(Caller {
            registry: std::sync::Mutex::new(None),
        });
        let callee = Arc::new(EchoActivity {
            id: "callee",
            calls: calls_callee.clone(),
        });
        let reg = ActivityRegistry::build(vec![
            caller.clone() as Arc<dyn Activity>,
            callee as Arc<dyn Activity>,
        ]);
        *caller.registry.lock().unwrap() = Some(reg.clone());
        let mut sink: () = ();
        let out = run_with_state(&mut sink, |ctx| {
            reg.invoke(ctx, "caller", "echo", json!({"x": 7})).unwrap()
        });
        assert_eq!(out["from"], "callee");
        assert_eq!(calls_callee.load(Ordering::SeqCst), 1);
    }
}
