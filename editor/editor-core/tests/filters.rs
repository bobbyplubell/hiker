//! Tests for SPEC §13.4 transaction hook filters.

use std::sync::Arc;

use editor_core::change::Set;
use editor_core::change::Op;
use editor_core::compartment::Compartment;
use editor_core::selection::Selection;
use editor_core::state::ChangeFilter;
use editor_core::state::Editor;
use editor_core::state::TransactionExtender;
use editor_core::state::TransactionFilter;
use editor_core::transaction::StateEffect;

/// A change filter that vetoes any Set that inserts the substring "BAD".
#[test]
fn change_filter_veto_blocks_insert() {
    let filter: ChangeFilter = Arc::new(|_state, cs| {
        for op in cs.ops() {
            if let Op::Insert(s) = op {
                if s.contains("BAD") {
                    return None;
                }
            }
        }
        Some(cs.clone())
    });

    let mut state = Editor::new("hello").with_change_filter(filter);
    state.selection = Selection::single(5);

    let tx = state.insert_at_selections("BAD");
    let next = state.apply(tx);

    // Veto: doc must be unchanged.
    assert_eq!(next.doc.to_string(), "hello");

    // Sanity: a non-vetoed insert should still work on the same state.
    let mut state2 = state.clone();
    state2.selection = Selection::single(5);
    let tx2 = state2.insert_at_selections(" world");
    let next2 = state2.apply(tx2);
    assert_eq!(next2.doc.to_string(), "hello world");
}

/// A transaction filter that strips trailing ASCII whitespace from every
/// Insert op in the Set.
#[test]
fn transaction_filter_rewrites_inserts() {
    let filter: TransactionFilter = Arc::new(|state, mut tx| {
        // Walk ops, trimming trailing whitespace from inserts. Rebuild as a
        // new Set using the same retain/delete structure but trimmed
        // inserts.
        let mut edits: Vec<(std::ops::Range<usize>, String)> = Vec::new();
        let mut pos = 0usize;
        for op in tx.changes.ops() {
            match op {
                Op::Retain(n) => pos += *n as usize,
                Op::Delete(n) => {
                    edits.push((pos..pos + *n as usize, String::new()));
                    pos += *n as usize;
                }
                Op::Insert(s) => {
                    let trimmed = s.trim_end_matches([' ', '\t']);
                    // If the previous edit is a delete at the same position
                    // with no insert, merge by appending the inserted text.
                    if let Some(last) = edits.last_mut() {
                        if last.0.end == pos && last.1.is_empty() {
                            last.1 = trimmed.to_string();
                            continue;
                        }
                    }
                    edits.push((pos..pos, trimmed.to_string()));
                }
            }
        }
        tx.changes = Set::of(state.doc.len_bytes(), edits);
        tx
    });

    let mut state = Editor::new("").with_transaction_filter(filter);
    state.selection = Selection::single(0);

    let tx = state.insert_at_selections("hello   ");
    let next = state.apply(tx);

    assert_eq!(next.doc.to_string(), "hello");
}

/// A transaction extender that emits a `Reconfigure` effect bumping a counter
/// in a compartment whenever the doc actually changes.
#[test]
fn transaction_extender_adds_effect_on_doc_change() {
    let counter: Compartment<u32> = Compartment::new();
    let counter_for_filter = counter.clone();

    let extender: TransactionExtender = Arc::new(move |state, tx| {
        if tx.changes.is_identity() {
            return Vec::new();
        }
        let current = state.compartments.get(&counter_for_filter).copied().unwrap_or(0);
        vec![StateEffect::Reconfigure {
            id: counter_for_filter.id(),
            value: Arc::new(current + 1),
        }]
    });

    let mut state = Editor::new("a").with_transaction_extender(extender);
    state.compartments.set(&counter, 0u32);
    state.selection = Selection::single(1);

    let tx = state.insert_at_selections("b");
    let next = state.apply(tx);

    assert_eq!(next.doc.to_string(), "ab");
    assert_eq!(next.compartments.get(&counter).copied(), Some(1));

    // A second edit increments again.
    let mut next = next;
    next.selection = Selection::single(2);
    let tx2 = next.insert_at_selections("c");
    let next2 = next.apply(tx2);
    assert_eq!(next2.compartments.get(&counter).copied(), Some(2));
}
