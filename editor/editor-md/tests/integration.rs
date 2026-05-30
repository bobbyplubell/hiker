use editor_core::decoration::Decoration;
use editor_core::state::Editor as EditorState;
use editor_core::decoration::MarkStyle;
use editor_core::selection::Selection;
use editor_md::styling::markdown_decorations;

fn has_mark(state: &EditorState, byte: usize, predicate: impl Fn(&MarkStyle) -> bool) -> bool {
    let set = markdown_decorations(state, None);
    set.iter_overlapping(byte..byte + 1).any(|(_, d)| {
        matches!(d, Decoration::Mark(s) if predicate(s))
    })
}

/// Decorations for `src` with the cursor parked at EOF (off every interior
/// line so the marker-fade / fence-hide rules are all "on").
fn decorations_cursor_at_end(src: &str) -> editor_core::decoration::Set {
    let mut state = EditorState::new(src);
    state.selection = Selection::single(state.doc.len_bytes());
    markdown_decorations(&state, None)
}

/// True when the byte offset of `needle`'s first char in `src` is covered by
/// a monospace code Mark — i.e. that text was classified as code-block body.
fn byte_is_code_body(src: &str, needle: &str) -> bool {
    let at = src.find(needle).expect("needle present");
    let set = decorations_cursor_at_end(src);
    set.iter_overlapping(at..at + 1)
        .any(|(_, d)| matches!(d, Decoration::Mark(s) if s.monospace))
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

// --- fenced-code-block detection (bug-inline-triple-backtick-breaks-codeblock,
// bug-multiple-codeblocks-mis-detected) ---

#[test]
fn inline_triple_backtick_with_content_does_not_open_codeblock() {
    // A prose line containing ```…``` plus other text (e.g. a Splunk inline
    // comment) must not be treated as a fence delimiter, so no surrounding
    // text gets pulled into code-block styling.
    let src = "foo ```bar``` baz\nplain prose line\n";
    let set = decorations_cursor_at_end(src);
    // No line on the "plain prose line" should carry a code-block background.
    let prose_at = src.find("plain prose").unwrap();
    let prose_is_code = set
        .iter_overlapping(prose_at..prose_at + 1)
        .any(|(_, d)| matches!(d, Decoration::Mark(s) if s.monospace));
    assert!(!prose_is_code, "prose after an inline ``` line must not be code");
    // And no fence line should be hidden (there is no real fence here).
    let any_hide = set
        .iter_overlapping(0..src.len())
        .any(|(_, d)| matches!(d, Decoration::Line(s) if s.hide));
    assert!(!any_hide, "inline triple-backtick line is not a fence");
}

#[test]
fn fence_line_with_trailing_content_is_not_a_closer() {
    // An unterminated fence runs to EOF as one pulldown block; its trailing
    // body line ("more") must render as code body, not be mistaken for a
    // closing fence and hidden.
    let src = "```\ncode line\nmore\n";
    assert!(byte_is_code_body(src, "code line"), "first body line is code");
    assert!(byte_is_code_body(src, "more"), "trailing body line is code, not a fence");
    let set = decorations_cursor_at_end(src);
    let more_at = src.find("more").unwrap();
    let more_hidden = set
        .iter_overlapping(more_at..more_at + 1)
        .any(|(_, d)| matches!(d, Decoration::Line(s) if s.hide));
    assert!(!more_hidden, "non-fence body line must not be hidden");
}

#[test]
fn single_fenced_block_styles_body_and_hides_fences() {
    let src = "```rust\nlet x = 1;\n```\n";
    assert!(byte_is_code_body(src, "let x = 1;"), "block body is monospace");
    let set = decorations_cursor_at_end(src);
    let hidden_fences = set
        .iter_overlapping(0..src.len())
        .filter(|(_, d)| matches!(d, Decoration::Line(s) if s.hide))
        .count();
    assert_eq!(hidden_fences, 2, "open + close fence both hidden");
}

#[test]
fn two_separate_blocks_both_classified_and_prose_between_is_not() {
    let src = "```\nblock one\n```\n\nprose between\n\n```\nblock two\n```\n\nafter\n";
    assert!(byte_is_code_body(src, "block one"), "first block body is code");
    assert!(byte_is_code_body(src, "block two"), "second block body is code");
    assert!(!byte_is_code_body(src, "prose between"), "interstitial prose is not code");
    assert!(!byte_is_code_body(src, "after"), "trailing prose is not code");
    // All four fences (two per block) hide independently.
    let set = decorations_cursor_at_end(src);
    let hidden_fences = set
        .iter_overlapping(0..src.len())
        .filter(|(_, d)| matches!(d, Decoration::Line(s) if s.hide))
        .count();
    assert_eq!(hidden_fences, 4, "both blocks' fences pair independently");
}

// --- fenced-code syntax highlighting (editor-code-syntax-highlight) ---

#[test]
fn fenced_rust_block_emits_per_token_color_marks() {
    // A known language (rust) should produce at least one Mark with fg set
    // somewhere inside the block body — the syntax highlighter ran and at
    // least one token matched the palette.
    let src = "```rust\nfn main() { let x = 1; }\n```\n";
    let set = decorations_cursor_at_end(src);
    let body_start = src.find("fn main").unwrap();
    let body_end = src.find("\n```\n").unwrap();
    let has_color = set
        .iter_overlapping(body_start..body_end)
        .any(|(_, d)| matches!(d, Decoration::Mark(s) if s.fg.is_some() && s.monospace));
    assert!(has_color, "rust block should have at least one colored, monospace Mark");
}

#[test]
fn fenced_unknown_lang_does_not_emit_colors() {
    // An unknown info string ("notalang") should produce no colored Marks
    // inside the block body — body still renders as plain monospace.
    let src = "```notalang\nfn main() { let x = 1; }\n```\n";
    let set = decorations_cursor_at_end(src);
    let body_start = src.find("fn main").unwrap();
    let body_end = src.find("\n```\n").unwrap();
    let any_color = set
        .iter_overlapping(body_start..body_end)
        .any(|(_, d)| matches!(d, Decoration::Mark(s) if s.fg.is_some()));
    assert!(!any_color, "unknown-lang block must not produce colored Marks");
}

#[test]
fn frontmatter_body_is_monospace_and_muted() {
    // YAML frontmatter at the top of a file renders as plain monospace,
    // with no per-token highlighting (the syntax highlighter operates on
    // fenced blocks only, not on frontmatter).
    let src = "---\ntitle: hello\ntags: [a, b]\n---\n\nbody\n";
    let set = decorations_cursor_at_end(src);
    let fm_inside = src.find("title").unwrap();
    let mono = set
        .iter_overlapping(fm_inside..fm_inside + 5)
        .any(|(_, d)| matches!(d, Decoration::Mark(s) if s.monospace));
    assert!(mono, "frontmatter body must be monospace");
    let colored = set
        .iter_overlapping(fm_inside..fm_inside + 5)
        .any(|(_, d)| matches!(d, Decoration::Mark(s) if s.fg.is_some()));
    assert!(!colored, "frontmatter must not be syntax-highlighted");
}

#[test]
fn tilde_fences_are_supported() {
    let src = "~~~\ntilde body\n~~~\n";
    assert!(byte_is_code_body(src, "tilde body"), "tilde block body is code");
    let set = decorations_cursor_at_end(src);
    let hidden_fences = set
        .iter_overlapping(0..src.len())
        .filter(|(_, d)| matches!(d, Decoration::Line(s) if s.hide))
        .count();
    assert_eq!(hidden_fences, 2, "tilde open + close fence both hidden");
}
