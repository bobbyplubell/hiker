//! Integration tests for Compartment / Store scoped reconfiguration.

use std::sync::Arc;

use editor_core::compartment::Compartment;

use editor_core::compartment::Store;
use editor_core::state::Editor;
use editor_core::transaction::Transaction;

#[test]
fn two_compartments_are_independent() {
    let theme: Compartment<String> = Compartment::new();
    let keymap: Compartment<String> = Compartment::new();

    let mut store = Store::default();
    store.set(&theme, "dark".to_string());
    store.set(&keymap, "vim".to_string());

    assert_eq!(store.get(&theme).unwrap(), "dark");
    assert_eq!(store.get(&keymap).unwrap(), "vim");

    let store2 = store.reconfigure(&theme, "light".to_string());
    // theme changed in store2, keymap unchanged.
    assert_eq!(store2.get(&theme).unwrap(), "light");
    assert_eq!(store2.get(&keymap).unwrap(), "vim");
    // Original store is untouched.
    assert_eq!(store.get(&theme).unwrap(), "dark");
}

#[test]
fn reconfigure_shares_unchanged_arcs() {
    let theme: Compartment<String> = Compartment::new();
    let keymap: Compartment<Vec<u8>> = Compartment::new();

    let mut store = Store::default();
    store.set(&theme, "dark".to_string());
    store.set(&keymap, vec![1, 2, 3]);

    let keymap_arc_before = store.get_arc(&keymap).unwrap();

    let store2 = store.reconfigure(&theme, "light".to_string());

    let keymap_arc_after = store2.get_arc(&keymap).unwrap();
    // The keymap slot was not touched: same Arc pointer.
    assert!(Arc::ptr_eq(&keymap_arc_before, &keymap_arc_after));
}

#[test]
fn editor_state_reconfigure_preserves_doc_and_selection() {
    let theme: Compartment<String> = Compartment::new();
    let s0 = Editor::new("hello").reconfigure(&theme, "dark".to_string());
    assert_eq!(s0.doc.to_string(), "hello");
    assert_eq!(s0.compartments.get(&theme).unwrap(), "dark");

    let s1 = s0.reconfigure(&theme, "light".to_string());
    assert_eq!(s1.doc.to_string(), "hello");
    assert_eq!(s1.compartments.get(&theme).unwrap(), "light");
    // Old state still sees the old value.
    assert_eq!(s0.compartments.get(&theme).unwrap(), "dark");
}

#[test]
fn transaction_reconfigure_effect_applies() {
    let lang: Compartment<String> = Compartment::new();
    let s = Editor::new("fn main() {}");
    let tx = Transaction::reconfigure(s.doc.len_bytes(), &lang, "rust".to_string());
    let s2 = s.apply(tx);
    assert_eq!(s2.compartments.get(&lang).unwrap(), "rust");
    assert_eq!(s2.doc.to_string(), "fn main() {}");
}
