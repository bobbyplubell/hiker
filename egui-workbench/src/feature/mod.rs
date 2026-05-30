//! Generic feature-registry foundation. Consumers (hiker-app, hiker-lite,
//! future apps built on `egui_workbench`) supply their own `Ctx` impl;
//! features parameterize on a `Ctx`-trait bound so they can be shared
//! across apps without coupling to either one's `AppState`.
//!
//! See `docs/feature-registry.md` (in the hiker repo) for the design.
//!
//! ## Quick map
//!
//! - [`Ctx`] — universal contract. Every app's Ctx struct impls it.
//! - [`Feature`] — the feature trait, generic over `C: Ctx + ?Sized`.
//! - [`SidebarSurface`], [`PanelSurface`], [`HamburgerEntry`],
//!   [`ActivityItem`] — surface sub-traits, each parameterized on the
//!   same `C` so features that aren't shared bind a concrete app's
//!   umbrella ctx and shared features bind a per-feature requirement
//!   trait.
//! - [`Registry`] — ordered list of `Arc<dyn Feature<C>>`. One per app,
//!   built once at app startup.
//! - [`ActionError`] — error type for the inter-feature action seam.

use std::any::Any;
use std::sync::Arc;

// ---- Universal Ctx contract -----------------------------------------

/// The minimum contract every consumer app's per-surface Ctx impl
/// satisfies. Features see callers as `&mut C` where `C: Ctx + ?Sized`
/// (often `C = dyn HikerCtx` for hiker-app, `C = dyn LiteCtx` for
/// hiker-lite). The only universal method is the per-feature opaque
/// state slot; every other accessor lives on a per-feature
/// *requirement trait* the app's umbrella Ctx supertraits.
///
/// Why `state()` is on the universal trait: the registry's host needs
/// to thread a per-feature state slice into the call regardless of
/// which feature is rendering. Putting it here keeps that universal
/// without forcing every per-feature requirement trait to redeclare it.
pub trait Ctx {
    /// Per-feature opaque state slice. Each feature downcasts to its
    /// own concrete state struct via
    /// `ctx.state().downcast_mut::<FooState>()`.
    fn state(&mut self) -> &mut dyn Any;
}

// ---- Feature trait + surface sub-traits ------------------------------

/// A registered feature. Singleton in the registry; per-instance state
/// (multiple tabs of the same feature) lives in tab payloads, not on
/// the `Feature` impl itself.
///
/// Surface accessors default to `None` so a new feature only implements
/// what it wants. Generic over `C: Ctx + ?Sized` so:
///
/// - Features owned by one app bind a concrete app-umbrella Ctx
///   (`Feature<dyn HikerCtx>`).
/// - Features in `hiker-features/` (shared) impl
///   `Feature<C>` for `C: FooCtx + ?Sized` where `FooCtx` is the
///   feature's own Ctx requirement trait. Both apps' umbrella Ctx
///   types supertrait `FooCtx`, so the same `Arc<Foo>` works in
///   either app's registry.
pub trait Feature<C: Ctx + ?Sized + 'static>: Send + Sync {
    /// Stable kebab-case id (e.g. `"clusters"`). Used as the dispatch
    /// key from the registry + persisted in settings.
    fn id(&self) -> &'static str;

    /// Human-facing label (e.g. `"Cluster trees"`).
    fn label(&self) -> &'static str;

    /// Activity-bar / mode-button icon.
    fn icon(&self) -> egui::Image<'static>;

    /// Optional keybind chord descriptor (e.g. `"ctrl+shift+c"`); used
    /// by the keybind registry consumer. Default `None`.
    fn keybind_chord(&self) -> Option<&'static str> {
        None
    }

    fn sidebar(&self) -> Option<&dyn SidebarSurface<C>> {
        None
    }
    fn panel(&self) -> Option<&dyn PanelSurface<C>> {
        None
    }
    fn hamburger(&self) -> Option<&dyn HamburgerEntry<C>> {
        None
    }
    fn activity_bar(&self) -> Option<&dyn ActivityItem<C>> {
        None
    }
    fn command_palette(&self, _ctx: &mut C) -> Vec<PaletteCommand<C>> {
        Vec::new()
    }

    /// Inter-feature action dispatch. Default returns
    /// [`ActionError::UnknownAction`]. Features that expose verbs to
    /// peers (or to the palette / hamburger) override this.
    fn dispatch_action(
        &self,
        _ctx: &mut C,
        action: &str,
        _args: serde_json::Value,
    ) -> Result<serde_json::Value, ActionError> {
        Err(ActionError::UnknownAction {
            feature: self.id().to_string(),
            action: action.to_string(),
        })
    }
}

/// Sidebar mode body. The mode-switcher invokes `render` for the
/// currently-active feature's sidebar surface.
pub trait SidebarSurface<C: Ctx + ?Sized>: Send + Sync {
    fn render(&self, ui: &mut egui::Ui, ctx: &mut C);
}

/// Center-pane tab body for a feature-owned tab kind. `payload` is
/// the tab's serialized state.
pub trait PanelSurface<C: Ctx + ?Sized>: Send + Sync {
    fn render(&self, ui: &mut egui::Ui, ctx: &mut C, payload: &str);
}

/// Top-strip hamburger menu entry.
pub trait HamburgerEntry<C: Ctx + ?Sized>: Send + Sync {
    fn label(&self) -> &'static str;
    fn keybind_id(&self) -> Option<&'static str> {
        None
    }
    fn invoke(&self, ctx: &mut C);
}

/// Activity-bar item override. By default a feature with a
/// `SidebarSurface` auto-renders an activity item using `Feature::icon`
/// + `Feature::label`; implementing this trait overrides those (e.g.
/// for a dynamic badge).
pub trait ActivityItem<C: Ctx + ?Sized>: Send + Sync {
    fn icon(&self) -> egui::Image<'static>;
    fn tooltip(&self) -> &'static str;
    fn invoke(&self, ctx: &mut C);
}

/// One entry returned by `Feature::command_palette`. The palette
/// dispatcher invokes `action` inside a fresh `Ctx` borrow.
pub struct PaletteCommand<C: Ctx + ?Sized> {
    pub id: &'static str,
    pub label: String,
    pub action: Box<dyn FnOnce(&mut C) + Send>,
}

// ---- Action seam errors ---------------------------------------------

/// Errors returned by [`Registry::invoke`] /
/// [`Feature::dispatch_action`].
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

// ---- Registry --------------------------------------------------------

/// Ordered list of `Arc<dyn Feature<C>>` built once at app startup.
/// Consumers iterate via [`Registry::iter`]. Order is meaningful:
/// sidebar mode buttons + activity items render in registry order.
pub struct Registry<C: Ctx + ?Sized + 'static> {
    features: Vec<Arc<dyn Feature<C>>>,
}

impl<C: Ctx + ?Sized + 'static> Registry<C> {
    /// Build a registry from an ordered list of features.
    pub fn build(features: Vec<Arc<dyn Feature<C>>>) -> Arc<Self> {
        Arc::new(Self { features })
    }

    /// Iterate the registered features in their stable order.
    pub fn iter(&self) -> impl Iterator<Item = &Arc<dyn Feature<C>>> {
        self.features.iter()
    }

    /// O(N) lookup by stable id. N is ~5-15 features per app, so no
    /// hashmap is warranted.
    pub fn by_id(&self, id: &str) -> Option<&Arc<dyn Feature<C>>> {
        self.features.iter().find(|f| f.id() == id)
    }

    /// Dispatch `(feature_id, action, args)` through the registry.
    /// Entry point for inter-feature calls and the command-palette /
    /// hamburger dispatch paths. Returns [`ActionError::UnknownFeature`]
    /// when the id doesn't resolve; otherwise forwards to the feature's
    /// [`Feature::dispatch_action`].
    pub fn invoke(
        &self,
        ctx: &mut C,
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

// ---- Tests -----------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Synthetic Ctx impl for tests — owns its state as a Box so the
    /// trait-object lifetime stays `'static` (`dyn Ctx` defaults to
    /// `dyn Ctx + 'static`, so Ctx impls coerced to `&mut dyn Ctx`
    /// must themselves be `'static`). Real apps build their Ctx impl
    /// with disjoint `&mut` borrows of AppState fields — they sidestep
    /// the lifetime issue because the impl struct's lifetime is the
    /// scope of the surface invocation, and the trait object is
    /// reborrowed fresh each call.
    struct TestCtx {
        state: Box<dyn Any>,
    }
    impl Ctx for TestCtx {
        fn state(&mut self) -> &mut dyn Any {
            self.state.as_mut()
        }
    }

    /// Feature whose `dispatch_action` records the call count and
    /// echoes the args. Bound to `dyn Ctx` so it works against any
    /// app's Ctx — the simplest case.
    struct EchoFeature {
        id: &'static str,
        calls: Arc<AtomicUsize>,
    }
    impl Feature<dyn Ctx> for EchoFeature {
        fn id(&self) -> &'static str {
            self.id
        }
        fn label(&self) -> &'static str {
            "Echo"
        }
        fn icon(&self) -> egui::Image<'static> {
            egui::Image::new(egui::ImageSource::Bytes {
                uri: "tests/echo".into(),
                bytes: egui::load::Bytes::Static(&[]),
            })
        }
        fn dispatch_action(
            &self,
            _ctx: &mut (dyn Ctx + 'static),
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

    fn run<R>(f: impl FnOnce(&mut (dyn Ctx + 'static)) -> R) -> R {
        let mut ctx = TestCtx {
            state: Box::new(()),
        };
        f(&mut ctx)
    }

    #[test]
    fn invoke_routes_to_named_feature() {
        let calls_a = Arc::new(AtomicUsize::new(0));
        let calls_b = Arc::new(AtomicUsize::new(0));
        let reg: Arc<Registry<dyn Ctx>> = Registry::build(vec![
            Arc::new(EchoFeature {
                id: "alpha",
                calls: calls_a.clone(),
            }) as Arc<dyn Feature<dyn Ctx>>,
            Arc::new(EchoFeature {
                id: "beta",
                calls: calls_b.clone(),
            }) as Arc<dyn Feature<dyn Ctx>>,
        ]);
        let out = run(|ctx| reg.invoke(ctx, "beta", "echo", json!({"n": 1})).unwrap());
        assert_eq!(out["from"], "beta");
        assert_eq!(calls_b.load(Ordering::SeqCst), 1);
        assert_eq!(calls_a.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn invoke_unknown_feature_errors() {
        let reg: Arc<Registry<dyn Ctx>> = Registry::build(vec![]);
        let err = run(|ctx| reg.invoke(ctx, "nope", "x", json!(null)).unwrap_err());
        assert!(matches!(err, ActionError::UnknownFeature(ref s) if s == "nope"));
    }

    #[test]
    fn invoke_unknown_action_errors() {
        let calls = Arc::new(AtomicUsize::new(0));
        let reg: Arc<Registry<dyn Ctx>> = Registry::build(vec![Arc::new(EchoFeature {
            id: "alpha",
            calls,
        })
            as Arc<dyn Feature<dyn Ctx>>]);
        let err = run(|ctx| {
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
}
