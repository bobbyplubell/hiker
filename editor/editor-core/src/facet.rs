//! Facet / StateField / ViewPlugin extension surface (SPEC §13.1–§13.3).
//!
//! Opt-in alongside the existing direct-field API on `Editor` /
//! `ViewState`. New extensions that want typed reactive composition can use
//! these; older code keeps reading the named fields.
//!
//! **Status**: scaffold only. The `Store` and `FieldStore` infrastructure
//! is in place and tested; built-in fields (history, folds, IME, …) have
//! *not* been migrated. They remain as direct fields. Migration is a
//! follow-up — the goal here is to land the surface so extensions don't have
//! to bake their state into `ViewState` going forward.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

/// A typed multi-provider channel: many extensions contribute `Input`s and
/// the editor combines them into a single `Output`.
///
/// Implementors are zero-sized marker types (use `()` or a unit struct) that
/// identify the facet at compile time.
pub trait Facet: 'static + Send + Sync {
    type Input: Clone + Send + Sync + 'static;
    type Output: Clone + Send + Sync + 'static;
    fn combine(inputs: &[Self::Input]) -> Self::Output;
}

/// Type-erased multi-provider store. Keyed by the facet's `TypeId`.
#[derive(Clone, Default)]
pub struct Store {
    /// Per-facet list of provider values, stored as `Arc<dyn Any>` so the
    /// store doesn't need to know the facet type. Combined lazily on `get`.
    inputs: HashMap<TypeId, Vec<Arc<dyn Any + Send + Sync>>>,
}

impl Store {
    pub fn new() -> Self {
        Self { inputs: HashMap::new() }
    }

    /// Register a provider for facet `F`.
    pub fn provide<F: Facet>(&mut self, value: F::Input) {
        let entry = self.inputs.entry(TypeId::of::<F>()).or_default();
        entry.push(Arc::new(value));
    }

    /// Combine all providers for facet `F` and return the output. `None` if
    /// no providers registered.
    pub fn get<F: Facet>(&self) -> Option<F::Output> {
        let inputs = self.inputs.get(&TypeId::of::<F>())?;
        let typed: Vec<F::Input> = inputs
            .iter()
            .filter_map(|a| a.downcast_ref::<F::Input>().cloned())
            .collect();
        if typed.is_empty() {
            None
        } else {
            Some(F::combine(&typed))
        }
    }

    /// True if the facet has any providers.
    pub fn has<F: Facet>(&self) -> bool {
        self.inputs.get(&TypeId::of::<F>()).is_some_and(|v| !v.is_empty())
    }

    /// Replace all providers for a facet with a single new value. Used by
    /// compartment reconfigure paths.
    pub fn replace<F: Facet>(&mut self, value: F::Input) {
        self.inputs
            .insert(TypeId::of::<F>(), vec![Arc::new(value)]);
    }

    pub fn clear<F: Facet>(&mut self) {
        self.inputs.remove(&TypeId::of::<F>());
    }
}

/// Extension-owned reactive state slot.
///
/// Hosts register a `StateField` once; the editor calls `create` on init and
/// `update` after each transaction. Stored in `FieldStore` keyed by the
/// field marker type.
pub trait StateField: 'static + Send + Sync {
    type T: 'static + Clone + Send + Sync;
    fn create() -> Self::T;
    /// Called after each transaction is applied. Receives the previous value
    /// plus the transaction; returns the next value. Should be pure.
    fn update(prev: &Self::T, tx: &crate::transaction::Transaction) -> Self::T;
}

/// Type-erased map of `TypeId → Arc<T>` for state fields.
#[derive(Clone, Default)]
pub struct FieldStore {
    fields: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
}

impl FieldStore {
    pub fn new() -> Self {
        Self { fields: HashMap::new() }
    }

    /// Register a field; calls `F::create` once and stores the result.
    pub fn register<F: StateField>(&mut self) {
        self.fields
            .entry(TypeId::of::<F>())
            .or_insert_with(|| Arc::new(F::create()));
    }

    /// Get the current value for field `F`.
    pub fn get<F: StateField>(&self) -> Option<F::T> {
        self.fields
            .get(&TypeId::of::<F>())
            .and_then(|a| a.downcast_ref::<F::T>().cloned())
    }

    /// Manually set a value (used by tests + external mutators that bypass
    /// the transaction pipeline).
    pub fn set<F: StateField>(&mut self, value: F::T) {
        self.fields.insert(TypeId::of::<F>(), Arc::new(value));
    }

    /// Run the field's `update` for every registered field, given a
    /// transaction. Used by `Editor::apply` to keep fields in sync.
    ///
    /// v1: walks a caller-supplied list of updater closures. The
    /// `Editor::apply` integration is a follow-up — exposing the
    /// surface here lets hosts wire it manually for now.
    #[allow(clippy::type_complexity)]
    pub fn apply_updaters(
        &mut self,
        tx: &crate::transaction::Transaction,
        updaters: &[Arc<dyn Fn(&mut FieldStore, &crate::transaction::Transaction) + Send + Sync>],
    ) {
        for f in updaters {
            f(self, tx);
        }
    }

    pub fn has<F: StateField>(&self) -> bool {
        self.fields.contains_key(&TypeId::of::<F>())
    }
}

/// Per-view stateful plugin. Lives in `ViewState` (when wired) and receives
/// frame-by-frame updates.
///
/// **Status**: trait surface only; no `ViewState` integration yet. Hosts can
/// instantiate and drive these manually via their own update loop.
pub trait ViewPlugin: Send + Sync {
    /// Called once when the plugin is registered.
    fn create() -> Self
    where
        Self: Sized;
    /// Called once per frame with the transactions that landed since the
    /// last update. v1: caller-supplied; future: the widget will track this.
    fn update(&mut self, transactions: &[crate::transaction::Transaction]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::Set;
use crate::transaction::Transaction;

    struct ColorFacet;
    impl Facet for ColorFacet {
        type Input = u32;
        type Output = u32;
        // Combine: bitwise-OR all providers. Trivially associative.
        fn combine(inputs: &[u32]) -> u32 {
            inputs.iter().fold(0, |a, b| a | b)
        }
    }

    #[test]
    fn facet_combines_multiple_providers() {
        let mut store = Store::new();
        store.provide::<ColorFacet>(0b0001);
        store.provide::<ColorFacet>(0b0100);
        store.provide::<ColorFacet>(0b1000);
        assert_eq!(store.get::<ColorFacet>(), Some(0b1101));
    }

    #[test]
    fn facet_get_none_when_no_providers() {
        let store = Store::new();
        assert_eq!(store.get::<ColorFacet>(), None);
    }

    #[test]
    fn facet_replace_resets_providers() {
        let mut store = Store::new();
        store.provide::<ColorFacet>(0b0001);
        store.provide::<ColorFacet>(0b0010);
        store.replace::<ColorFacet>(0b1111_0000);
        assert_eq!(store.get::<ColorFacet>(), Some(0b1111_0000));
    }

    struct CounterField;
    impl StateField for CounterField {
        type T = u32;
        fn create() -> u32 {
            0
        }
        fn update(prev: &u32, _tx: &Transaction) -> u32 {
            prev + 1
        }
    }

    #[test]
    fn statefield_register_and_get() {
        let mut store = FieldStore::new();
        store.register::<CounterField>();
        assert_eq!(store.get::<CounterField>(), Some(0));
        store.set::<CounterField>(42);
        assert_eq!(store.get::<CounterField>(), Some(42));
    }

    #[test]
    fn statefield_apply_updaters_runs_each() {
        let mut store = FieldStore::new();
        store.register::<CounterField>();
        #[allow(clippy::type_complexity)]
        let updater: Arc<dyn Fn(&mut FieldStore, &Transaction) + Send + Sync> =
            Arc::new(|s, tx| {
                let prev = s.get::<CounterField>().unwrap();
                s.set::<CounterField>(CounterField::update(&prev, tx));
            });
        let tx = Transaction::new(Set::empty(0));
        store.apply_updaters(&tx, &[updater.clone(), updater.clone()]);
        assert_eq!(store.get::<CounterField>(), Some(2));
    }

    struct TestPlugin {
        tx_count: usize,
    }
    impl ViewPlugin for TestPlugin {
        fn create() -> Self {
            Self { tx_count: 0 }
        }
        fn update(&mut self, transactions: &[Transaction]) {
            self.tx_count += transactions.len();
        }
    }

    #[test]
    fn view_plugin_create_and_update() {
        let mut p = TestPlugin::create();
        assert_eq!(p.tx_count, 0);
        let tx = Transaction::new(Set::empty(0));
        p.update(&[tx.clone(), tx]);
        assert_eq!(p.tx_count, 2);
    }
}
