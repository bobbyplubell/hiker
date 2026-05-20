//! Integration tests for the autocomplete framework (SPEC §9.6).

use std::sync::Arc;

use editor_core::{EditorState, Selection};
use editor_view::command::{self, Action};
use editor_view::{
    CompletionItem, CompletionKind, CompletionSource, InputEvent, ViewState,
};
use smol_str::SmolStr;

struct FakeSource;

impl CompletionSource for FakeSource {
    fn triggers(&self) -> &[char] {
        &['.']
    }
    fn matches(&self, _state: &EditorState, pos: usize) -> Vec<CompletionItem> {
        ["alpha", "beta"]
            .into_iter()
            .map(|s| CompletionItem {
                label: SmolStr::from(s),
                detail: None,
                insert: SmolStr::from(s),
                replace_range: Some(pos..pos),
                kind: CompletionKind::Text,
            })
            .collect()
    }
}

fn drive(state: &mut EditorState, view: &mut ViewState, ev: &InputEvent) {
    if let Action::Replace(next) = command::handle(state, view, ev) {
        *state = next;
    }
}

#[test]
fn trigger_char_opens_popup_with_source_matches() {
    let mut state = EditorState::new("x");
    state.selection = Selection::single(1);
    let mut view = ViewState::default();
    view.completion_sources.push(Arc::new(FakeSource));

    drive(&mut state, &mut view, &InputEvent::Text(".".into()));

    assert!(view.completion.active, "popup should be active after trigger char");
    assert_eq!(view.completion.items.len(), 2);
    assert_eq!(view.completion.items[0].label, "alpha");
    assert_eq!(view.completion.items[1].label, "beta");
    assert_eq!(view.completion.selected, 0);
    // The trigger char itself was inserted into the doc.
    assert_eq!(state.doc.to_string(), "x.");
}

#[test]
fn non_trigger_char_does_not_open_popup() {
    let mut state = EditorState::new("");
    let mut view = ViewState::default();
    view.completion_sources.push(Arc::new(FakeSource));

    drive(&mut state, &mut view, &InputEvent::Text("a".into()));

    assert!(!view.completion.active);
}

#[test]
fn escape_closes_popup() {
    let mut state = EditorState::new("");
    let mut view = ViewState::default();
    view.completion_sources.push(Arc::new(FakeSource));
    drive(&mut state, &mut view, &InputEvent::Text(".".into()));
    assert!(view.completion.active);

    drive(
        &mut state,
        &mut view,
        &InputEvent::Key(editor_view::KeyEvent {
            key: editor_view::Key::Named(editor_view::NamedKey::Escape),
            mods: editor_view::Modifiers::default(),
            repeat: false,
        }),
    );
    assert!(!view.completion.active);
}
