//! Transaction: an immutable description of a state change.
//!
//! A transaction bundles a `Set`, an optional new `Selection`, and
//! metadata (annotations) that downstream consumers (like the history field)
//! use for grouping decisions.

use std::any::Any;
use std::sync::Arc;

use crate::change::Set;
use crate::compartment::Id;
use crate::selection::Selection;

/// Side-effects layered on top of the doc/selection change.
///
/// Currently the only effect is `Reconfigure`, which swaps the value held
/// in a compartment. Apply happens inside `Editor::apply`.
#[derive(Clone)]
pub enum StateEffect {
    Reconfigure {
        id: Id,
        value: Arc<dyn Any + Send + Sync>,
    },
}

impl std::fmt::Debug for StateEffect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StateEffect::Reconfigure { id, .. } => {
                f.debug_struct("Reconfigure").field("id", id).finish_non_exhaustive()
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum EditType {
    Input,
    Delete,
    Paste,
    Indent,
    Reformat,
    Undo,
    Redo,
    Other,
}

#[derive(Clone, Debug, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Annotations {
    pub edit_type: Option<EditType>,
    /// Whether this transaction should be grouped with its predecessor in the
    /// history (overrides the default coalescing heuristics).
    pub join_with_previous: bool,
}

// TODO(serde): `Transaction` is not serializable because it carries a
// `Set` (sumtree-backed) and `effects: Vec<StateEffect>` containing
// `Arc<dyn Any>`. A future revision will add a serializable wire form of
// `Set` and replace effects with a tagged enum so transactions can be
// persisted (e.g. for collab/replay). See SPEC §9.9.
#[derive(Clone, Debug)]
pub struct Transaction {
    pub changes: Set,
    pub selection: Option<Selection>,
    pub annotations: Annotations,
    pub effects: Vec<StateEffect>,
}

impl Transaction {
    pub fn new(changes: Set) -> Self {
        Self {
            changes,
            selection: None,
            annotations: Annotations::default(),
            effects: Vec::new(),
        }
    }

    pub fn with_effect(mut self, effect: StateEffect) -> Self {
        self.effects.push(effect);
        self
    }

    /// Build a no-change transaction that reconfigures a compartment.
    pub fn reconfigure<T: 'static + Clone + Send + Sync>(
        doc_len: usize,
        compartment: &crate::compartment::Compartment<T>,
        value: T,
    ) -> Self {
        let effect = StateEffect::Reconfigure {
            id: compartment.id(),
            value: Arc::new(value),
        };
        Self::new(Set::empty(doc_len)).with_effect(effect)
    }

    pub fn with_selection(mut self, sel: Selection) -> Self {
        self.selection = Some(sel);
        self
    }

    pub const fn with_edit_type(mut self, t: EditType) -> Self {
        self.annotations.edit_type = Some(t);
        self
    }

    pub const fn joined(mut self) -> Self {
        self.annotations.join_with_previous = true;
        self
    }
}
