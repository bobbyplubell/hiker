use std::sync::{Arc, Mutex};

use editor_core::selection::Selection;
use editor_core::state::Editor;
use editor_core::state::TransactionListener;
#[test]
fn listener_observes_post_transaction_state() {
    let observed: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let observed_clone = Arc::clone(&observed);

    let listener: TransactionListener = Arc::new(move |_prev, next, _tx| {
        observed_clone.lock().unwrap().push(next.doc.to_string());
    });

    let mut state = Editor::new("hello").with_listener(listener);
    state.selection = Selection::single(5);

    let tx = state.insert_at_selections(", world");
    let next = state.apply(tx);

    assert_eq!(next.doc.to_string(), "hello, world");

    let seen = observed.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0], "hello, world");
}

#[test]
fn listener_survives_clone() {
    let counter: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
    let counter_clone = Arc::clone(&counter);

    let listener: TransactionListener = Arc::new(move |_prev, _next, _tx| {
        *counter_clone.lock().unwrap() += 1;
    });

    let mut state = Editor::new("a").with_listener(listener);
    state.selection = Selection::single(1);

    // Clone, then apply on the clone — listener should still fire.
    let mut cloned = state.clone();
    cloned.selection = Selection::single(1);
    let _ = cloned.apply(cloned.insert_at_selections("b"));

    assert_eq!(*counter.lock().unwrap(), 1);
}
