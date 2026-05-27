//! Integration tests for snippet expansion (SPEC §9.22).

use editor_core::state::Editor as EditorState;
use editor_view::command;
use editor_view::command::expand_snippet;
use editor_view::command::Action;
use editor_view::events::InputEvent;
use editor_view::events::Key;
use editor_view::events::KeyEvent;
use editor_view::events::Modifiers;
use editor_view::events::NamedKey;
use editor_view::snippets::Snippet;
use editor_view::viewport::ViewState;
fn drive(state: &mut EditorState, view: &mut ViewState, ev: &InputEvent) {
    if let Action::Replace { state: next, .. } = command::handle(state, view, ev) {
        *state = next;
    }
}

fn tab() -> InputEvent {
    InputEvent::Key(KeyEvent {
        key: Key::Named(NamedKey::Tab),
        mods: Modifiers::default(),
        repeat: false,
    })
}

#[test]
fn parse_numbered_stops_and_text() {
    let s = Snippet::parse("for $1 in $2:\n    $0").unwrap();
    assert!(s.text().contains("for  in :"));
    assert_eq!(s.stops().len(), 3);
}

#[test]
fn parse_placeholder_inlines_default() {
    let s = Snippet::parse("${1:item}").unwrap();
    assert_eq!(s.text(), "item");
    assert_eq!(s.stops().len(), 1);
    let span = &s.stops()[&1][0];
    assert_eq!(&s.text()[span.clone()], "item");
}

#[test]
fn expand_into_empty_doc_lands_at_first_stop() {
    let mut state = EditorState::new("");
    let mut view = ViewState::default();
    let snip = Snippet::parse("for $1 in $2:\n    $0").unwrap();
    let action = expand_snippet(&state, &mut view, &snip, 0..0);
    if let Action::Replace { state: next, .. } = action {
        state = next;
    }
    assert_eq!(state.doc.to_string(), "for  in :\n    ");
    // Caret lands at $1: just after "for " (4 bytes).
    let main = state.selection.main();
    assert_eq!(main.start(), 4);
    assert_eq!(main.end(), 4);
    assert!(view.snippet.is_active());
}

#[test]
fn tab_cycles_to_next_stop() {
    let mut state = EditorState::new("");
    let mut view = ViewState::default();
    let snip = Snippet::parse("for $1 in $2:\n    $0").unwrap();
    if let Action::Replace { state: next, .. } = expand_snippet(&state, &mut view, &snip, 0..0) {
        state = next;
    }
    let first = state.selection.main().start();
    drive(&mut state, &mut view, &tab());
    let second = state.selection.main().start();
    assert_ne!(first, second, "Tab should move the caret to the next stop");
    // $2 sits after "for  in " (8 bytes).
    assert_eq!(second, 8);
    // Another Tab takes us to $0 (the final stop), after "for  in :\n    ".
    drive(&mut state, &mut view, &tab());
    assert_eq!(state.selection.main().start(), "for  in :\n    ".len());
    // One more Tab cancels the snippet.
    drive(&mut state, &mut view, &tab());
    assert!(!view.snippet.is_active());
}

#[test]
fn mirror_sync_replaces_both_occurrences() {
    let mut state = EditorState::new("");
    let mut view = ViewState::default();
    let snip = Snippet::parse("$1 $1").unwrap();
    if let Action::Replace { state: next, .. } = expand_snippet(&state, &mut view, &snip, 0..0) {
        state = next;
    }
    // Initial doc is just a single space (both $1 spans empty).
    assert_eq!(state.doc.to_string(), " ");
    // Type "x" — only the primary cursor inserts; the post-apply mirror
    // sync should propagate the change to the second occurrence.
    drive(&mut state, &mut view, &InputEvent::Text("x".into()));
    assert_eq!(state.doc.to_string(), "x x");
}
