use editor_core::decoration::Decoration;
use editor_core::state::Editor as EditorState;
use editor_core::decoration::MarkStyle;
use editor_md::styling::markdown_decorations;

fn has_mark(state: &EditorState, byte: usize, predicate: impl Fn(&MarkStyle) -> bool) -> bool {
    let set = markdown_decorations(state, None);
    set.iter_overlapping(byte..byte + 1).any(|(_, d)| {
        matches!(d, Decoration::Mark(s) if predicate(s))
    })
}

#[test]
fn h1_gets_2x_height_scale() {
    let state = EditorState::new("# Hello\n\nbody text\n");
    let set = markdown_decorations(&state, None);
    let start = state.doc.line_to_byte(0);
    let end = state.doc.line_to_byte(1);
    let found = set.iter_overlapping(start..end).any(|(r, d)| {
        r.start == start && matches!(d, Decoration::Line(s) if s.height_scale == Some(2.0))
    });
    assert!(found);
}

#[test]
fn bold_emits_bold_mark() {
    let state = EditorState::new("Some **bold** text.\n");
    // The cursor is at position 0 (line 0); cursor-line reveal would skip
    // Replace, but Mark always appears. Verify bold mark exists somewhere on
    // the line.
    let inside = "Some **bold** text".find("bold").unwrap();
    let mut s = state.clone();
    // Move cursor to a different line so the reveal rule is "off".
    s.selection = editor_core::selection::Selection::single(s.doc.len_bytes());
    assert!(has_mark(&s, inside, |m| m.bold));
}

#[test]
fn italic_emits_italic_mark() {
    let state = EditorState::new("Some *em* text.\n");
    let inside = "Some *em* text".find("em").unwrap();
    let mut s = state.clone();
    s.selection = editor_core::selection::Selection::single(s.doc.len_bytes());
    assert!(has_mark(&s, inside, |m| m.italic));
}

#[test]
fn code_span_gets_monospace_with_bg() {
    let state = EditorState::new("Has `code` here.\n");
    let inside = "Has `code` here".find("code").unwrap();
    let mut s = state.clone();
    s.selection = editor_core::selection::Selection::single(s.doc.len_bytes());
    assert!(has_mark(&s, inside, |m| m.monospace && m.bg.is_some()));
}

#[test]
fn cursor_on_heading_line_hides_no_replace() {
    let state = {
        let mut s = EditorState::new("# Hello\n\nbody\n");
        s.selection = editor_core::selection::Selection::single(2); // cursor on heading line
        s
    };
    let set = markdown_decorations(&state, None);
    let has_replace_on_heading = set.iter_overlapping(0..7).any(|(_, d)| {
        matches!(d, Decoration::Replace { .. })
    });
    assert!(!has_replace_on_heading, "cursor on heading line should reveal source");
}

#[test]
fn cursor_off_heading_line_replaces_hash() {
    let mut state = EditorState::new("# Hello\n\nbody\n");
    state.selection = editor_core::selection::Selection::single(state.doc.len_bytes());
    let set = markdown_decorations(&state, None);
    let has_replace = set.iter_overlapping(0..2).any(|(_, d)| {
        matches!(d, Decoration::Replace { .. })
    });
    assert!(has_replace, "cursor off heading should hide # marker");
}
