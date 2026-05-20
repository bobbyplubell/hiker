//! Public-API smoke tests for `editor-ts`.
//!
//! Real end-to-end parse/highlight tests need a concrete tree-sitter
//! grammar, which this crate intentionally does not depend on (see
//! `languages.rs`). For now we assert the public surface compiles and the
//! type signatures match what the spec requires; once a `lang-*` feature
//! is wired with its corresponding `tree-sitter-<lang>` dep, add a real
//! parse test guarded by `#[cfg(feature = "lang-json")]` (or similar).

use editor_core::{ChangeSet, EditorState, Rope, light_default};
use editor_ts::{TsLanguage, TsState, changeset_to_edits, parse, reparse, ts_decorations};

#[test]
fn public_api_surface_exists() {
    // Type-level assertions only — make sure the function signatures
    // line up with what the spec calls for. We cannot actually invoke
    // `parse` without a real `tree_sitter::Language`, so this is a
    // compile-time check expressed as `let _: fn(...) -> ...`.
    let _: fn(&TsLanguage, &str) -> TsState = parse;
    let _: fn(&TsLanguage, &str, &TsState, &[tree_sitter::InputEdit]) -> TsState = reparse;
    let _: fn(&EditorState, &TsState, Option<&editor_core::Theme>) -> editor_core::DecorationSet =
        ts_decorations;
}

#[test]
fn changeset_to_edits_is_pure() {
    // This helper does not touch tree-sitter at all and is safe to test
    // without a real grammar.
    let before = Rope::from_str("fn main() {}");
    let cs = ChangeSet::of(
        before.len_bytes(),
        [(3..7, "foo".to_string())],
    );
    let edits = changeset_to_edits(&before, &cs);
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].start_byte, 3);
    assert_eq!(edits[0].old_end_byte, 7);
    assert_eq!(edits[0].new_end_byte, 3 + "foo".len());
}

#[test]
fn theme_is_consumable_by_ts_decorations_signature() {
    // We can't construct a TsState without a parser, but we can prove
    // the theme type plumbs through.
    let theme = light_default();
    let _tokens = theme.tokens.len();
}

#[ignore = "needs real grammar; use `cargo test --features lang-json` once language deps are wired"]
#[test]
fn end_to_end_parse_and_highlight() {
    // Placeholder for the real test, kept here so wiring it later is a
    // one-liner: instantiate `editor_ts::languages::json()`, parse a JSON
    // doc, and assert that `ts_decorations` produces at least one Mark.
}
