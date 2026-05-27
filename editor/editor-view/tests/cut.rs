//! Cut command: Ctrl+X must both place the selection on the clipboard and
//! delete it. Regression guard for the bug where cut deleted the text but
//! dropped the clipboard write (it could only return one `Action`).

use editor_core::selection::{SelRange, Selection};
use editor_core::state::Editor as EditorState;
use editor_view::command::{handle, Action};
use editor_view::events::InputEvent;
use editor_view::viewport::ViewState;

#[test]
fn cut_with_selection_yields_clipboard_text_and_deletes() {
    let mut state = EditorState::new("hello world\n");
    // Select "hello".
    state.selection = Selection::from_range(SelRange::new(0, 5));
    let mut view = ViewState::default();
    view.sync_to(&state);

    let act = handle(&state, &mut view, &InputEvent::Cut);
    match act {
        Action::Cut { text, state: new_state, tx } => {
            assert_eq!(text, "hello", "cut must copy the selection to the clipboard");
            assert_eq!(
                new_state.doc.to_string(),
                " world\n",
                "cut must delete the selection from the document"
            );
            // The change set rides along so the host can mirror the deletion
            // into a `working` layer; applying it to the original doc must
            // reproduce the post-cut text.
            assert!(!tx.changes.is_identity(), "cut carries a real deletion change set");
            assert_eq!(tx.changes.apply(&state.doc).to_string(), " world\n");
        }
        _ => panic!("expected Action::Cut, cut dropped the clipboard write"),
    }
}

#[test]
fn cut_empty_selection_takes_whole_line() {
    // Mirrors copy's VSCode behavior: empty selection cuts the whole line.
    let mut state = EditorState::new("alpha\nbeta\n");
    state.selection = Selection::from_range(SelRange::point(2)); // inside "alpha"
    let mut view = ViewState::default();
    view.sync_to(&state);

    let act = handle(&state, &mut view, &InputEvent::Cut);
    match act {
        Action::Cut { text, state: new_state, tx } => {
            assert_eq!(text, "alpha\n", "empty-selection cut copies the whole line");
            assert_eq!(new_state.doc.to_string(), "beta\n");
            assert_eq!(tx.changes.apply(&state.doc).to_string(), "beta\n");
        }
        _ => panic!("expected Action::Cut"),
    }
}
