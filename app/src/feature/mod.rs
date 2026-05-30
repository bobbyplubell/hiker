//! Feature registry — small descriptor registry that App-shell consumers
//! (sidebar mode switcher, activity bar, hamburger menu, command palette,
//! keybind registry) iterate to discover what to render.
//!
//! Each `Feature` impl exposes optional surface descriptors
//! (`SidebarSurface`, `PanelSurface`, ...). Returning `None` from a
//! surface method opts out. The registry is built once per vault in
//! `bootstrap::open_vault` and stashed on `AppState::features`. New
//! consumers iterate the registry generically rather than hardcoding a
//! feature list. See `docs/feature-registry.md`.
//!
//! Layout note: trait, ctx, registry, and builtin-list helper all live
//! in this single root file rather than each in its own sibling under
//! `app/src/feature/`. Per-piece sibling files would each fall under
//! `scripts/check-splits.py`'s 20-line minimum (the shapes here are 5-20
//! lines each), and `pub use` re-exports are banned by clippy's
//! anti-arbitrary-split posture, so the file would be the wrong
//! boundary either way.

use std::any::Any;
use std::sync::{Arc, RwLock};

use eframe::egui;
use hiker_core::config::Config;

use crate::state::{NavState, Services, Toast};

// ---- Feature trait + surface sub-traits ------------------------------

/// A registered feature. Singleton in the registry; per-instance state
/// (e.g. multiple tabs of the same feature) lives in tab payloads, not
/// here. Surface accessors default to `None` so a new feature only
/// implements the surfaces it wants.
pub trait Feature: Send + Sync {
    /// Stable kebab-case id, e.g. `"clusters"`. Used as the feature
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

    fn sidebar(&self) -> Option<&dyn SidebarSurface> {
        None
    }
    /// Whether this feature belongs on the PRIMARY (left) activity bar.
    /// Default: any feature with a `sidebar()` surface is a primary
    /// activity mode. Secondary-dock-only features (chat, which renders
    /// in the right side bar, not as an activity mode) override this to
    /// `false`. [feature-consumer-activity-bar]
    fn primary_activity(&self) -> bool {
        self.sidebar().is_some()
    }
    fn panel(&self) -> Option<&dyn PanelSurface> {
        None
    }
    fn hamburger(&self) -> Option<&dyn HamburgerEntry> {
        None
    }
    fn activity_bar(&self) -> Option<&dyn ActivityItem> {
        None
    }
    fn command_palette(&self, _ctx: &Ctx<'_>) -> Vec<PaletteCommand> {
        Vec::new()
    }

    /// Inter-feature action dispatch. Default returns `UnknownAction`.
    /// Features that want to expose verbs to peers (or to the command
    /// palette / hamburger) override this. `args` and the return value
    /// are `serde_json::Value` so the seam stays generic without
    /// pulling each feature's typed args into the trait. Typed wrappers
    /// (e.g. `crate::trails::actions`) keep callers honest at the call
    /// site. [feature-inter-feature-actions]
    fn dispatch_action(
        &self,
        _ctx: &mut Ctx<'_>,
        action: &str,
        _args: serde_json::Value,
    ) -> Result<serde_json::Value, ActionError> {
        Err(ActionError::UnknownAction {
            feature: self.id().to_string(),
            action: action.to_string(),
        })
    }
}

/// Errors returned by `Registry::invoke` / `Feature::dispatch_action`.
/// [feature-inter-feature-actions]
#[derive(Debug, thiserror::Error)]
pub enum ActionError {
    #[error("unknown feature `{0}`")]
    UnknownFeature(String),
    #[error("feature `{feature}` does not have action `{action}`")]
    UnknownAction { feature: String, action: String },
    #[error("invalid args for `{feature}::{action}`: {reason}")]
    InvalidArgs {
        feature: String,
        action: String,
        reason: String,
    },
    #[error("{0}")]
    Failed(String),
}

/// Sidebar mode body. The mode-switcher (per
/// `feature-consumer-sidebar`) invokes `render` for the currently-active
/// feature's sidebar surface.
pub trait SidebarSurface: Send + Sync {
    fn render(&self, ui: &mut egui::Ui, ctx: &mut Ctx<'_>);
}

/// Center-pane tab body for a feature-owned tab kind
/// (`TabKind::Feature { id, payload }`). v1 has no feature-tab dispatch
/// yet — cluster-review still uses `TabKind::ClusterReview`. The trait
/// exists so Phase 2/3 can wire a generic dispatcher without revisiting
/// the surface vocabulary.
#[allow(dead_code)]
pub trait PanelSurface: Send + Sync {
    fn render(&self, ui: &mut egui::Ui, ctx: &mut Ctx<'_>, payload: &str);
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

/// Activity-bar item override. By default a feature with a `SidebarSurface`
/// auto-renders an activity item using `Feature::icon` + `Feature::label`;
/// implementing this trait overrides those (e.g. for a dynamic badge).
/// Phase 2.
#[allow(dead_code)]
pub trait ActivityItem: Send + Sync {
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
    /// several features scan or read vault contents during render
    /// (backlinks, related, search). A disjoint borrow of
    /// `vault_session.vault`, alongside the `&mut services` borrow.
    pub vault: &'a Arc<hiker_core::vault::Vault>,
    /// Per-feature opaque state slice. Each feature downcasts to its
    /// own state struct (e.g.
    /// `ctx.state.downcast_mut::<State>().unwrap()`). The
    /// registry shell never sees the concrete type.
    pub state: &'a mut dyn Any,
    /// Vault-relative path of the active note, if a buffer is focused.
    /// A cheap read filled at ctx-build time so a surface reads "the
    /// current note" here instead of reaching into `session`. Shared
    /// because ≥2 features (backlinks, related, search, ...) need it.
    pub active_path: Option<String>,
    /// Deferred-effect sink. A surface pushes closures here (via
    /// [`Ctx::defer`]) for cross-cutting effects that need broad
    /// `&mut AppState` (open a note, reveal a path) and so can't run
    /// inside the narrow ctx borrow. The consumer drains them right
    /// after the surface returns. Mirrors helix's compositor callback
    /// queue; closures reuse the existing `&mut AppState` helpers.
    pub effects: &'a mut Vec<Effect>,
}

/// A deferred effect queued by a feature surface and applied by the
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

/// Ordered list of `Arc<dyn Feature>` built once at vault open.
/// Consumers iterate via `iter()`. Order is meaningful: sidebar mode
/// buttons + activity items render in registry order.
pub struct Registry {
    features: Vec<Arc<dyn Feature>>,
}

impl Registry {
    /// Build a registry from the given ordered list of features.
    /// `bootstrap::open_vault` is the canonical call site; tests can
    /// build a registry directly with whichever feature set they need.
    pub fn build(features: Vec<Arc<dyn Feature>>) -> Arc<Self> {
        Arc::new(Self { features })
    }

    /// Iterate the registered features in their stable order.
    #[allow(dead_code)]
    pub fn iter(&self) -> impl Iterator<Item = &Arc<dyn Feature>> {
        self.features.iter()
    }

    /// O(N) lookup by stable id. N is expected to be ~5-15 features so
    /// no hashmap is warranted (per the spec's "Lookup is O(N)" point).
    pub fn by_id(&self, id: &str) -> Option<&Arc<dyn Feature>> {
        self.features.iter().find(|f| f.id() == id)
    }

    /// Dispatch `(feature_id, action, args)` through the registry —
    /// the entry point for inter-feature calls (one feature invoking
    /// another) and for the generic command-palette / hamburger
    /// dispatch path. Returns `UnknownFeature` when the id doesn't
    /// resolve; otherwise forwards to the feature's
    /// `dispatch_action`. [feature-inter-feature-actions]
    pub fn invoke(
        &self,
        ctx: &mut Ctx<'_>,
        feature_id: &str,
        action: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, ActionError> {
        let feature = self
            .features
            .iter()
            .find(|f| f.id() == feature_id)
            .ok_or_else(|| ActionError::UnknownFeature(feature_id.to_string()))?;
        feature.dispatch_action(ctx, action, args)
    }
}

// ---- Builtins --------------------------------------------------------

/// Build a `Ctx` borrow scoped to `feature_id` and run `f` against
/// it. Returns `None` when the feature id doesn't resolve to a state
/// slice on `AppState` (Phase 2 wires only the migrated features).
///
/// The state-slice lookup is inlined here so the compiler can verify
/// the four disjoint borrows of `AppState` fields. A
/// `feature_state_mut` helper returning a single `&mut dyn Any`
/// can't be combined with parallel borrows of the other fields.
pub fn with_ctx<R>(
    app: &mut crate::state::AppState,
    feature_id: &str,
    effects: &mut Vec<Effect>,
    f: impl FnOnce(&mut Ctx<'_>) -> R,
) -> Option<R> {
    // Compute the active note path first (immutable read, released
    // before the disjoint &mut field borrows below).
    let active_path = app
        .session
        .active_tab
        .and_then(|id| app.tab_by_id(id))
        .and_then(crate::tab::Tab::buffer_path)
        .map(str::to_string);
    let state: &mut dyn Any = match feature_id {
        "files" => &mut app.file_tree_state,
        "clusters" => &mut app.clusters_state,
        "trails" => &mut app.trails_state,
        "vault" => &mut app.vault_state,
        "backlinks" => &mut app.backlinks_state,
        "related" => &mut app.related_state,
        "search" => &mut app.search_state,
        "trash" => &mut app.trash_state,
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

/// Hamburger-menu dispatch: look up `feature_id` in the registry and
/// invoke its `HamburgerEntry::invoke`. No-op when the feature is
/// missing or has no hamburger entry. [feature-consumer-hamburger]
pub fn dispatch_hamburger(app: &mut crate::state::AppState, feature_id: &str) {
    let features = app.features.clone();
    let Some(feature) = features.by_id(feature_id) else {
        return;
    };
    let Some(entry) = feature.hamburger() else {
        return;
    };
    let mut effects: Vec<Effect> = Vec::new();
    with_ctx(app, feature_id, &mut effects, |ctx| entry.invoke(ctx));
    for eff in effects {
        eff(app);
    }
}

/// Construct the built-in feature list in sidebar/activity-bar order.
/// `Files` is first (the primary mode). Plugins are appended after
/// built-ins by callers once Phase 3's adapter lands.
pub fn builtin_features() -> Vec<Arc<dyn Feature>> {
    vec![
        Arc::new(crate::files::Files) as Arc<dyn Feature>,
        Arc::new(crate::clusters::Clusters) as Arc<dyn Feature>,
        Arc::new(crate::trails::Trails) as Arc<dyn Feature>,
        Arc::new(crate::vault_view::Vault) as Arc<dyn Feature>,
        Arc::new(crate::backlinks::Backlinks) as Arc<dyn Feature>,
        Arc::new(crate::related::Related) as Arc<dyn Feature>,
        Arc::new(crate::search::Search) as Arc<dyn Feature>,
        Arc::new(crate::trash::Trash) as Arc<dyn Feature>,
        Arc::new(crate::chat::Chat) as Arc<dyn Feature>,
    ]
}

// ---- Tests -----------------------------------------------------------

/// Registry + dispatch tests. Use synthetic `Feature` impls so the
/// tests don't need a real `AppState` / `Services` /
/// `VaultSession`; the seam shape is what we're guarding here, not
/// any particular feature's behavior.
#[cfg(test)]
mod registry_tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A no-state synthetic feature whose `dispatch_action` records
    /// every call into a shared counter and echoes the args back as
    /// the return value. Lets a test cross-invoke between two
    /// features through the registry and confirm the routing.
    struct EchoFeature {
        id: &'static str,
        calls: Arc<AtomicUsize>,
    }

    impl Feature for EchoFeature {
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
                    feature: self.id.to_string(),
                    action: action.to_string(),
                }),
            }
        }
    }

    /// Run `f` with a `Ctx` whose state slot points at `state` and
    /// whose service borrows are filled from `services_holder`. The
    /// synthetic features only touch `state`; the other fields are
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
        // A cleaner shape: have the synthetic Features not need
        // `Ctx` at all. But the trait fixes the signature. So we
        // build a fake Services via mem::zeroed only if the test
        // genuinely needs the field; the echo features under test
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
        // We achieve that by using `Registry::invoke` with a
        // synthetic Feature impl whose dispatch_action ignores ctx
        // entirely. The tests below build the Ctx by hand using
        // `MaybeUninit` for the services slot and never touch it.
        use std::mem::MaybeUninit;
        let mut services_uninit: MaybeUninit<crate::state::Services> = MaybeUninit::uninit();
        // SAFETY: `services_uninit` is never read while uninitialized
        // because the synthetic features under test ignore the field.
        // The reference's lifetime ends with the function; the
        // uninitialized memory is then dropped without being read.
        // `MaybeUninit` does not run `Drop`.
        let services_ref: &mut crate::state::Services =
            unsafe { &mut *services_uninit.as_mut_ptr() };
        // Same punt for `vault`: the echo features under test never read
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
        let reg = Registry::build(builtin_features());
        let ids: Vec<&str> = reg.iter().map(|f| f.id()).collect();
        assert_eq!(
            ids,
            vec![
                "files", "clusters", "trails", "vault", "backlinks", "related", "search", "trash",
                "chat"
            ]
        );
    }

    #[test]
    fn invoke_routes_to_named_feature() {
        let calls_a = Arc::new(AtomicUsize::new(0));
        let calls_b = Arc::new(AtomicUsize::new(0));
        let reg = Registry::build(vec![
            Arc::new(EchoFeature {
                id: "alpha",
                calls: calls_a.clone(),
            }) as Arc<dyn Feature>,
            Arc::new(EchoFeature {
                id: "beta",
                calls: calls_b.clone(),
            }) as Arc<dyn Feature>,
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
    fn invoke_unknown_feature_errors() {
        let reg = Registry::build(vec![]);
        let mut sink: () = ();
        let err = run_with_state(&mut sink, |ctx| {
            reg.invoke(ctx, "nope", "x", json!(null)).unwrap_err()
        });
        assert!(matches!(err, ActionError::UnknownFeature(ref s) if s == "nope"));
    }

    #[test]
    fn invoke_unknown_action_errors() {
        let calls = Arc::new(AtomicUsize::new(0));
        let reg = Registry::build(vec![Arc::new(EchoFeature {
            id: "alpha",
            calls,
        }) as Arc<dyn Feature>]);
        let mut sink: () = ();
        let err = run_with_state(&mut sink, |ctx| {
            reg.invoke(ctx, "alpha", "no_such_action", json!(null))
                .unwrap_err()
        });
        match err {
            ActionError::UnknownAction { feature, action } => {
                assert_eq!(feature, "alpha");
                assert_eq!(action, "no_such_action");
            }
            other => panic!("got {other:?}"),
        }
    }

    /// Two features can invoke each other through the registry
    /// without a circular-dep cycle: feature `caller` calls
    /// `registry.invoke("callee", ...)` from inside its own
    /// `dispatch_action`. Models the real Phase 3 pattern of one
    /// feature triggering another's verb (e.g. boards adding to a
    /// trail) through the seam rather than via direct module access.
    #[test]
    fn features_can_invoke_each_other() {
        let calls_callee = Arc::new(AtomicUsize::new(0));
        // Caller forwards every action to "callee" through a shared
        // registry handle resolved from a thread_local set up below.
        struct Caller {
            registry: std::sync::Mutex<Option<Arc<Registry>>>,
        }
        impl Feature for Caller {
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
        let callee = Arc::new(EchoFeature {
            id: "callee",
            calls: calls_callee.clone(),
        });
        let reg = Registry::build(vec![
            caller.clone() as Arc<dyn Feature>,
            callee as Arc<dyn Feature>,
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
