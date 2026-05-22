//! Round-trip tests for the opt-in serde support. See SPEC §9.9 +
//! IMPLEMENTATION §16.5.8.

#![cfg(feature = "serde")]

use editor_core::decoration::Diagnostic;

use editor_core::decoration::Severity;
use editor_core::selection::SelRange;

use editor_core::selection::Selection;
use editor_core::state::Editor;
use editor_core::state::SavedState;
use smol_str::SmolStr;

#[test]
fn saved_state_json_round_trip() {
    let mut state = Editor::new("hello world");
    state.selection = Selection::from_ranges(
        vec![SelRange::point(0), SelRange::new(6, 11)],
        1,
    );

    let saved = state.to_saved();
    assert_eq!(saved.format_version, SavedState::CURRENT_VERSION);

    let json = serde_json::to_string(&saved).expect("serialize");
    let decoded: SavedState = serde_json::from_str(&json).expect("deserialize");

    let restored = Editor::from_saved(decoded);
    assert_eq!(restored.doc.to_string(), "hello world");
    assert_eq!(restored.selection.ranges().len(), 2);
    assert_eq!(restored.selection.ranges()[0].start(), 0);
    assert_eq!(restored.selection.ranges()[1].range(), 6..11);
    assert_eq!(restored.selection.main_index(), 1);
}

#[test]
fn diagnostic_json_round_trip() {
    let diag = Diagnostic {
        range: 5..12,
        severity: Severity::Error,
        message: SmolStr::new("mismatched types"),
        source: SmolStr::new("rustc"),
        code: Some(SmolStr::new("E0308")),
    };

    let json = serde_json::to_string(&diag).expect("serialize");
    let decoded: Diagnostic = serde_json::from_str(&json).expect("deserialize");

    assert_eq!(decoded, diag);
}
