//! The immutable editor state. Mutation is via `apply(Transaction) -> State`.
//!
//! v1 holds the doc, selection, and history as fixed fields. The pluggable
//! StateField / Facet system from the spec is planned for the next iteration;
//! the surface here is stable enough that adding generic extension storage
//! later won't break callers.

use crate::change::Set;
use crate::compartment::{Compartment, Store};
use crate::history::History;
use crate::rope::Rope;
use crate::selection::Selection;
use crate::transaction::{EditType, StateEffect, Transaction};

pub type TransactionListener =
    std::sync::Arc<dyn Fn(&Editor, &Editor, &Transaction) + Send + Sync>;

/// Per SPEC §13.4: pre-apply filter that can veto or rewrite a `Set`.
/// Returning `None` vetoes the changes (selection-only effects still apply).
pub type ChangeFilter =
    std::sync::Arc<dyn Fn(&Editor, &Set) -> Option<Set> + Send + Sync>;

/// Per SPEC §13.4: pre-apply filter that rewrites the whole transaction.
pub type TransactionFilter =
    std::sync::Arc<dyn Fn(&Editor, Transaction) -> Transaction + Send + Sync>;

/// Per SPEC §13.4: post-filter extender that contributes additional effects
/// based on the (already-filtered) transaction.
pub type TransactionExtender =
    std::sync::Arc<dyn Fn(&Editor, &Transaction) -> Vec<StateEffect> + Send + Sync>;

#[derive(Clone, Default)]
pub struct Editor {
    pub doc: Rope,
    pub selection: Selection,
    pub history: History,
    pub compartments: Store,
    listeners: Vec<TransactionListener>,
    change_filters: Vec<ChangeFilter>,
    transaction_filters: Vec<TransactionFilter>,
    transaction_extenders: Vec<TransactionExtender>,
}

impl Editor {
    pub fn new(text: &str) -> Self {
        Self {
            doc: Rope::from_str(text),
            selection: Selection::single(0),
            history: History::new(),
            compartments: Store::default(),
            listeners: Vec::new(),
            change_filters: Vec::new(),
            transaction_filters: Vec::new(),
            transaction_extenders: Vec::new(),
        }
    }

    pub fn from_doc(doc: Rope) -> Self {
        Self {
            doc,
            selection: Selection::single(0),
            history: History::new(),
            compartments: Store::default(),
            listeners: Vec::new(),
            change_filters: Vec::new(),
            transaction_filters: Vec::new(),
            transaction_extenders: Vec::new(),
        }
    }

    /// Out-of-band reconfigure: returns a new state with a compartment
    /// swapped. Listeners run with an identity transaction so they observe
    /// the change.
    pub fn reconfigure<T: 'static + Clone + Send + Sync>(
        &self,
        c: &Compartment<T>,
        v: T,
    ) -> Self {
        let mut next = self.clone();
        next.compartments.set(c, v);
        let tx = Transaction::new(Set::empty(self.doc.len_bytes()));
        for listener in &self.listeners {
            listener(self, &next, &tx);
        }
        next
    }

    /// Register a post-transaction listener. Listeners receive
    /// `(prev_state, next_state, transaction)` after each `apply` and must
    /// not mutate state.
    pub fn with_listener(mut self, l: TransactionListener) -> Self {
        self.listeners.push(l);
        self
    }

    /// Register a change filter (SPEC §13.4). Runs before any transaction
    /// filter, in registration order. Returning `None` vetoes the changes.
    pub fn with_change_filter(mut self, f: ChangeFilter) -> Self {
        self.change_filters.push(f);
        self
    }

    /// Register a transaction filter (SPEC §13.4). Runs after change filters,
    /// in registration order; each receives the transaction produced by the
    /// previous filter.
    pub fn with_transaction_filter(mut self, f: TransactionFilter) -> Self {
        self.transaction_filters.push(f);
        self
    }

    /// Register a transaction extender (SPEC §13.4). Runs after all filters,
    /// and may contribute additional `StateEffect`s that are then applied.
    pub fn with_transaction_extender(mut self, f: TransactionExtender) -> Self {
        self.transaction_extenders.push(f);
        self
    }

    /// Apply a transaction, returning a new state. The previous state is
    /// untouched; cheap clones are encouraged.
    pub fn apply(&self, tx: Transaction) -> Editor {
        let mut tx = tx;

        // SPEC §13.4: run change filters in registration order. `None` vetoes.
        let mut vetoed = false;
        for filter in &self.change_filters {
            if vetoed {
                break;
            }
            match filter(self, &tx.changes) {
                None => {
                    tx.changes = Set::empty(self.doc.len_bytes());
                    vetoed = true;
                }
                Some(new_cs) => {
                    tx.changes = new_cs;
                }
            }
        }

        // SPEC §13.4: thread the transaction through registered tx filters.
        for filter in &self.transaction_filters {
            tx = filter(self, tx);
        }

        // SPEC §13.4: extenders contribute extra effects (post-filter).
        let mut extra_effects: Vec<StateEffect> = Vec::new();
        for extender in &self.transaction_extenders {
            extra_effects.extend(extender(self, &tx));
        }
        tx.effects.extend(extra_effects);

        let mut next = self.clone();
        let selection_before = self.selection.clone();

        if !tx.changes.is_identity() {
            let new_doc = tx.changes.apply(&self.doc);
            let mapped_selection = self.selection.map(&tx.changes);
            next.doc = new_doc;
            next.selection = tx.selection.clone().unwrap_or(mapped_selection);

            let edit_type = tx.annotations.edit_type;
            if !matches!(edit_type, Some(EditType::Undo | EditType::Redo)) {
                next.history.record(
                    &self.doc,
                    &tx,
                    selection_before,
                    next.selection.clone(),
                );
            }
        } else if let Some(sel) = tx.selection.clone() {
            next.selection = sel;
        }

        for effect in &tx.effects {
            match effect {
                StateEffect::Reconfigure { id, value } => {
                    next.compartments.set_raw(*id, value.clone());
                }
            }
        }

        for listener in &self.listeners {
            listener(self, &next, &tx);
        }

        next
    }

    /// Convenience: insert text at every selection range, replacing the
    /// selected text. Returns a transaction (does not apply).
    pub fn insert_at_selections(&self, text: &str) -> Transaction {
        let mut edits: Vec<(std::ops::Range<usize>, String)> = self
            .selection
            .ranges()
            .iter()
            .map(|r| (r.range(), text.to_string()))
            .collect();
        edits.sort_by_key(|(r, _)| r.start);
        let changes = Set::of(self.doc.len_bytes(), edits);
        Transaction::new(changes).with_edit_type(EditType::Input)
    }

    /// Convenience: delete the selection at every range; if empty, delete the
    /// previous byte (backspace semantics — caller should pass char-aware
    /// ranges in real use).
    pub fn delete_at_selections(&self) -> Transaction {
        let mut edits: Vec<(std::ops::Range<usize>, String)> = self
            .selection
            .ranges()
            .iter()
            .map(|r| {
                if r.is_empty() {
                    let start = r.start();
                    if start == 0 {
                        (0..0, String::new())
                    } else {
                        // Step back to previous char boundary.
                        let text = self.doc.to_string();
                        let mut back = start - 1;
                        while back > 0 && !text.is_char_boundary(back) {
                            back -= 1;
                        }
                        // If deleting a '\n' that is preceded by '\r', extend
                        // back one more byte so the whole CRLF pair is removed
                        // as a single unit.
                        if &text[back..start] == "\n" && back > 0 && text.as_bytes()[back - 1] == b'\r' {
                            back -= 1;
                        }
                        (back..start, String::new())
                    }
                } else {
                    (r.range(), String::new())
                }
            })
            .collect();
        edits.sort_by_key(|(r, _)| r.start);
        edits.dedup_by_key(|(r, _)| r.clone());
        let changes = Set::of(self.doc.len_bytes(), edits);
        Transaction::new(changes).with_edit_type(EditType::Delete)
    }

    /// Delete the given byte ranges in a single transaction; empty ranges are
    /// dropped. Used by line-wise cut, where the caller passes the selection
    /// range when non-empty and the whole-line range when empty, so the bytes
    /// deleted match the bytes copied to the clipboard.
    pub fn delete_ranges(&self, ranges: &[std::ops::Range<usize>]) -> Transaction {
        let mut edits: Vec<(std::ops::Range<usize>, String)> = ranges
            .iter()
            .filter(|r| !r.is_empty())
            .map(|r| (r.clone(), String::new()))
            .collect();
        edits.sort_by_key(|(r, _)| r.start);
        edits.dedup_by_key(|(r, _)| r.clone());
        let changes = Set::of(self.doc.len_bytes(), edits);
        Transaction::new(changes).with_edit_type(EditType::Delete)
    }

    /// Convert the current state into a serializable snapshot. Per SPEC §9.9
    /// the history is intentionally NOT persisted (it is too large and
    /// session-bound). Compartments and listeners are also dropped.
    #[cfg(feature = "serde")]
    pub fn to_saved(&self) -> SavedState {
        SavedState {
            format_version: SavedState::CURRENT_VERSION,
            doc_text: self.doc.to_string(),
            selection: self.selection.clone(),
        }
    }

    /// Rebuild an `Editor` from a previously serialized snapshot. The
    /// history starts empty.
    #[cfg(feature = "serde")]
    pub fn from_saved(saved: SavedState) -> Self {
        Self {
            doc: Rope::from_str(&saved.doc_text),
            selection: saved.selection,
            history: History::new(),
            compartments: Store::default(),
            listeners: Vec::new(),
            change_filters: Vec::new(),
            transaction_filters: Vec::new(),
            transaction_extenders: Vec::new(),
        }
    }

    pub fn undo(&self) -> Option<Editor> {
        self.undo_with_changes().map(|(editor, _)| editor)
    }

    pub fn redo(&self) -> Option<Editor> {
        self.redo_with_changes().map(|(editor, _)| editor)
    }

    /// Like [`undo`](Self::undo) but also returns the inverse change set that
    /// was applied, so a host binding can mirror the undo into a higher layer
    /// (e.g. a CRDT `working` layer) instead of having the doc silently revert.
    /// Without this, an undo that only updates `editor.doc` is invisible to the
    /// binding and gets clobbered on the next reverse pass. Returns `None` when
    /// there is nothing to undo.
    pub fn undo_with_changes(&self) -> Option<(Editor, Transaction)> {
        let mut hist = self.history.clone();
        let tx = hist.undo()?;
        Some((self.apply_history_tx(&tx, hist), tx))
    }

    /// Redo counterpart of [`undo_with_changes`](Self::undo_with_changes).
    pub fn redo_with_changes(&self) -> Option<(Editor, Transaction)> {
        let mut hist = self.history.clone();
        let tx = hist.redo()?;
        Some((self.apply_history_tx(&tx, hist), tx))
    }

    /// Build the post-undo/redo `Editor`: apply the history transaction's
    /// change set to the doc, move the selection through it (or use the
    /// transaction's recorded selection), and carry the already-advanced
    /// `history` cursor. History is *not* re-recorded — this is navigation.
    fn apply_history_tx(&self, tx: &Transaction, history: crate::history::History) -> Editor {
        let new_doc = tx.changes.apply(&self.doc);
        let selection = tx.selection.clone().unwrap_or_else(|| self.selection.map(&tx.changes));
        Editor {
            doc: new_doc,
            selection,
            history,
            compartments: self.compartments.clone(),
            listeners: self.listeners.clone(),
            change_filters: self.change_filters.clone(),
            transaction_filters: self.transaction_filters.clone(),
            transaction_extenders: self.transaction_extenders.clone(),
        }
    }
}

/// Persistent on-disk form of an editor state. See SPEC §9.9.
///
/// Only the document text and selection are kept; the history is dropped
/// (per spec — too large to persist across sessions). Bump `format_version`
/// on every incompatible change; `from_saved` should grow a match on the
/// version to migrate old snapshots.
#[cfg(feature = "serde")]
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SavedState {
    pub format_version: u32,
    pub doc_text: String,
    pub selection: Selection,
}

#[cfg(feature = "serde")]
impl SavedState {
    pub const CURRENT_VERSION: u32 = 1;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selection::Selection;

    #[test]
    fn insert_at_cursor() {
        let s = Editor::new("hello world");
        let mut s = s;
        s.selection = Selection::single(5);
        let tx = s.insert_at_selections(", lovely");
        let s2 = s.apply(tx);
        assert_eq!(s2.doc.to_string(), "hello, lovely world");
        assert_eq!(s2.selection.main().start(), 13);
    }

    #[test]
    fn delete_backspace() {
        let mut s = Editor::new("hello");
        s.selection = Selection::single(5);
        let tx = s.delete_at_selections();
        let s2 = s.apply(tx);
        assert_eq!(s2.doc.to_string(), "hell");
        assert_eq!(s2.selection.main().start(), 4);
    }

    #[test]
    fn multi_cursor_insert() {
        let mut s = Editor::new("abc abc abc");
        s.selection = Selection::from_ranges(
            vec![
                crate::selection::SelRange::point(0),
                crate::selection::SelRange::point(4),
                crate::selection::SelRange::point(8),
            ],
            0,
        );
        let tx = s.insert_at_selections("X");
        let s2 = s.apply(tx);
        assert_eq!(s2.doc.to_string(), "Xabc Xabc Xabc");
        // Each cursor advances by 1 (the inserted X)
        assert_eq!(s2.selection.ranges()[0].start(), 1);
        assert_eq!(s2.selection.ranges()[1].start(), 6);
        assert_eq!(s2.selection.ranges()[2].start(), 11);
    }

    #[test]
    fn undo_redo_round_trip() {
        let s0 = Editor::new("hello");
        let mut s = s0.clone();
        s.selection = Selection::single(5);

        let s1 = s.apply(s.insert_at_selections(" world"));
        assert_eq!(s1.doc.to_string(), "hello world");
        assert!(s1.history.can_undo());

        let s2 = s1.undo().unwrap();
        assert_eq!(s2.doc.to_string(), "hello");
        assert!(s2.history.can_redo());

        let s3 = s2.redo().unwrap();
        assert_eq!(s3.doc.to_string(), "hello world");
    }

    #[test]
    fn backspace_crlf_deletes_both_bytes() {
        // Caret is right after the \n (byte offset 3 in "a\r\nb").
        // One backspace must remove the entire \r\n pair, yielding "ab".
        let mut s = Editor::new("a\r\nb");
        s.selection = Selection::single(3); // after '\n'
        let tx = s.delete_at_selections();
        let s2 = s.apply(tx);
        assert_eq!(s2.doc.to_string(), "ab");
        assert_eq!(s2.selection.main().start(), 1);
    }

    #[test]
    fn backspace_lf_only_deletes_one_byte() {
        // Lone LF must still delete only the '\n', yielding "ab".
        let mut s = Editor::new("a\nb");
        s.selection = Selection::single(2); // after '\n'
        let tx = s.delete_at_selections();
        let s2 = s.apply(tx);
        assert_eq!(s2.doc.to_string(), "ab");
        assert_eq!(s2.selection.main().start(), 1);
    }

    #[test]
    fn new_edit_drops_redo_branch() {
        let mut s = Editor::new("hello");
        s.selection = Selection::single(5);
        let s = s.apply(s.insert_at_selections("!"));
        assert_eq!(s.doc.to_string(), "hello!");
        let s = s.undo().unwrap();
        assert_eq!(s.doc.to_string(), "hello");
        assert!(s.history.can_redo());

        let mut s = s;
        s.selection = Selection::single(5);
        let s = s.apply(s.insert_at_selections("?"));
        assert_eq!(s.doc.to_string(), "hello?");
        assert!(!s.history.can_redo());
    }
}
