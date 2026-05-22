//! Tests for the search engine + state + decoration provider.
//! SPEC §9.13, IMPLEMENTATION §16.6.4.

use editor_core::decoration::Decoration;

use editor_core::state::Editor as EditorState;
use editor_view::find::replace_all;
use editor_view::find::run_search;
use editor_view::find::search_decorations;
use editor_view::find::SearchFlags;
use editor_view::find::SearchState;
#[test]
fn plain_substring_search_finds_all_matches() {
    let state = EditorState::new("foo bar foo baz foo");
    let matches = run_search(&state, "foo", SearchFlags::default());
    assert_eq!(matches, vec![0..3, 8..11, 16..19]);
}

#[test]
fn case_insensitive_search_ignores_case() {
    let state = EditorState::new("Foo FOO foo");
    let cs = run_search(&state, "foo", SearchFlags { case_sensitive: true, ..Default::default() });
    assert_eq!(cs, vec![8..11]);

    let ci =
        run_search(&state, "foo", SearchFlags { case_sensitive: false, ..Default::default() });
    assert_eq!(ci, vec![0..3, 4..7, 8..11]);
}

#[test]
fn whole_word_skips_substring_in_word_matches() {
    let state = EditorState::new("foo foobar foo_bar afoo foo");
    let with_word_boundary =
        run_search(&state, "foo", SearchFlags { whole_word: true, ..Default::default() });
    // Only the standalone occurrences at offsets 0 and 24 qualify.
    assert_eq!(with_word_boundary, vec![0..3, 24..27]);

    let without = run_search(&state, "foo", SearchFlags::default());
    assert_eq!(without.len(), 5);
}

#[test]
fn replace_all_produces_correct_doc_after_apply() {
    let state = EditorState::new("foo bar foo baz foo");
    let matches = run_search(&state, "foo", SearchFlags::default());
    let search = SearchState {
        active: true,
        query: "foo".into(),
        replacement: "qux".into(),
        flags: SearchFlags::default(),
        matches,
        current_idx: Some(0),
    };
    let tx = replace_all(&state, &search).expect("expected a transaction");
    let new_state = state.apply(tx);
    assert_eq!(new_state.doc.to_string(), "qux bar qux baz qux");
}

#[test]
fn search_decorations_emits_stronger_mark_on_current_idx() {
    let state = EditorState::new("foo foo foo");
    let matches = run_search(&state, "foo", SearchFlags::default());
    assert_eq!(matches.len(), 3);
    let search = SearchState {
        active: true,
        query: "foo".into(),
        replacement: String::new(),
        flags: SearchFlags::default(),
        matches: matches.clone(),
        current_idx: Some(1),
    };
    let decos = search_decorations(&state, &search);
    let entries: Vec<_> = decos.iter_all().collect();
    assert_eq!(entries.len(), 3);

    // Pick out the bg for each match by start offset.
    let mut current_bg = None;
    let mut other_bgs = Vec::new();
    for (r, dec) in entries {
        if let Decoration::Mark(style) = dec {
            let bg = style.bg.expect("expected a bg color on search marks");
            if r.start == matches[1].start {
                current_bg = Some(bg);
            } else {
                other_bgs.push(bg);
            }
        } else {
            panic!("expected Mark decoration, got {dec:?}");
        }
    }
    let current = current_bg.expect("current_idx match should have a Mark");
    assert!(other_bgs.iter().all(|bg| *bg != current),
        "current_idx mark should have a different (stronger) color than the others");
    // Stronger = higher alpha.
    for bg in &other_bgs {
        assert!(current.a > bg.a, "current mark alpha {} should exceed other {}", current.a, bg.a);
    }
}
