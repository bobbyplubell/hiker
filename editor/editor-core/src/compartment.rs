//! Compartment-style scoped reconfiguration.
//!
//! A `Compartment<T>` is a typed handle to a slot in a `Store`.
//! The store is a type-erased map keyed by a fresh `Id`, holding
//! `Arc<dyn Any + Send + Sync>` values. Cloning the store is cheap (it clones
//! the inner HashMap; values stay Arc-shared), and `reconfigure` swaps a
//! single slot without touching the others — the foundation for swapping
//! theme/keymap/language data without rebuilding the rest of the state.
//!
//! See IMPLEMENTATION.md §16.5.10.
//!
//! ```ignore
//! let theme: Compartment<MyTheme> = Compartment::new();
//! let mut store = Store::default();
//! store.set(&theme, MyTheme::dark());
//! let store2 = store.reconfigure(&theme, MyTheme::light());
//! assert!(store.get(&theme).unwrap().is_dark());
//! assert!(store2.get(&theme).unwrap().is_light());
//! ```

use std::any::Any;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Id(pub u64);

/// A compartment owns a slot for swappable extension data.
///
/// `Compartment::new()` allocates a fresh, process-unique id. Hold the
/// compartment somewhere stable (typically as a `OnceLock` or in a config
/// builder) so reconfiguration always targets the same slot.
pub struct Compartment<T: Clone + Send + Sync + 'static> {
    id: Id,
    _phantom: PhantomData<fn() -> T>,
}

impl<T: Clone + Send + Sync + 'static> Compartment<T> {
    pub fn new() -> Self {
        let id = Id(NEXT_ID.fetch_add(1, Ordering::Relaxed));
        Self { id, _phantom: PhantomData }
    }

    pub const fn id(&self) -> Id {
        self.id
    }
}

impl<T: Clone + Send + Sync + 'static> Default for Compartment<T> {
    fn default() -> Self {
        Self::new()
    }
}

// Manual Clone so we don't require `T: Clone` on the handle itself
// (the handle is purely an id; the data lives in the store).
impl<T: Clone + Send + Sync + 'static> Clone for Compartment<T> {
    fn clone(&self) -> Self {
        Self { id: self.id, _phantom: PhantomData }
    }
}

/// Type-erased storage of compartment values.
///
/// Values are stored as `Arc<dyn Any + Send + Sync>`. `Clone` of the store
/// clones the underlying HashMap but shares the Arc'd values, so swapping a
/// single compartment via `reconfigure` is cheap.
#[derive(Clone, Default)]
pub struct Store {
    values: HashMap<Id, Arc<dyn Any + Send + Sync>>,
}

impl Store {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get<T: 'static + Clone + Send + Sync>(&self, c: &Compartment<T>) -> Option<&T> {
        self.values.get(&c.id).and_then(|v| v.downcast_ref::<T>())
    }

    pub fn set<T: 'static + Clone + Send + Sync>(&mut self, c: &Compartment<T>, value: T) {
        self.values.insert(c.id, Arc::new(value));
    }

    /// Returns a new store with the given compartment replaced. Other
    /// compartments are shared via Arc — no deep clone.
    pub fn reconfigure<T: 'static + Clone + Send + Sync>(
        &self,
        c: &Compartment<T>,
        value: T,
    ) -> Self {
        let mut next = self.clone();
        next.values.insert(c.id, Arc::new(value));
        next
    }

    /// Insert a pre-built Arc value by raw id. Used by the transaction
    /// effect path where the value has already been type-erased.
    pub(crate) fn set_raw(&mut self, id: Id, value: Arc<dyn Any + Send + Sync>) {
        self.values.insert(id, value);
    }

    /// Fetch the raw Arc for a compartment (useful for ptr-equality tests).
    pub fn get_arc<T: 'static + Clone + Send + Sync>(
        &self,
        c: &Compartment<T>,
    ) -> Option<Arc<dyn Any + Send + Sync>> {
        self.values.get(&c.id).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_ids_are_unique() {
        let a: Compartment<u32> = Compartment::new();
        let b: Compartment<u32> = Compartment::new();
        assert_ne!(a.id(), b.id());
    }

    #[test]
    fn get_set_roundtrip() {
        let c: Compartment<String> = Compartment::new();
        let mut s = Store::default();
        s.set(&c, "hi".to_string());
        assert_eq!(s.get(&c).map(String::as_str), Some("hi"));
    }

    #[test]
    fn reconfigure_does_not_mutate_original() {
        let c: Compartment<String> = Compartment::new();
        let mut s = Store::default();
        s.set(&c, "a".to_string());
        let s2 = s.reconfigure(&c, "b".to_string());
        assert_eq!(s.get(&c).unwrap(), "a");
        assert_eq!(s2.get(&c).unwrap(), "b");
    }
}
