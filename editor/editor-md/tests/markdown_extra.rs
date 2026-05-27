use editor_core::decoration::Decoration;
use editor_core::state::Editor as EditorState;
use editor_core::selection::Selection;
use editor_md::notes::footnote_decorations;
use editor_md::equations::math_decorations;
use editor_md::diagrams::mermaid_decorations;
use editor_md::embeds::transclusion_decorations;
#[test]
fn transclusion_emits_replace_and_mark_when_cursor_off_line() {
    let src = "first\nsee ![[OtherNote]] now\n";
    let mut state = EditorState::new(src);
    state.selection = Selection::single(0); // cursor on line 0
    let set = transclusion_decorations(&state, None, None);

    let start = src.find("![[").unwrap();
    let end = src.find("]]").unwrap() + 2;

    let mut has_replace = false;
    let mut has_mark = false;
    for (range, dec) in set.iter_overlapping(0..src.len()) {
        match dec {
            Decoration::Replace { display: Some(s) }
                if range.start == start && range.end == end =>
            {
                assert!(s.contains("OtherNote"));
                has_replace = true;
            }
            Decoration::Mark(m)
                if range.start == start && range.end == end && m.atomic && m.fg.is_some() =>
            {
                has_mark = true;
            }
            _ => {}
        }
    }
    assert!(has_replace, "expected Replace covering ![[…]]");
    assert!(has_mark, "expected atomic Mark over ![[…]]");
}

#[test]
fn transclusion_with_section_uses_target_only() {
    let src = "x ![[Note#sub]] y\n";
    let mut state = EditorState::new(src);
    state.selection = Selection::single(src.len()); // cursor at end (line 1, empty)
    let set = transclusion_decorations(&state, None, None);
    let mut found = false;
    for (_, dec) in set.iter_overlapping(0..src.len()) {
        if let Decoration::Replace { display: Some(s) } = dec {
            assert!(s.contains("Note"));
            assert!(!s.contains('#'));
            found = true;
        }
    }
    assert!(found, "expected Replace with target-only display");
}

#[test]
fn footnote_inline_ref_and_definition() {
    let src = "Text[^1] more.\n[^1]: A footnote.\n";
    let state = EditorState::new(src);
    let set = footnote_decorations(&state, None, None);

    let ref_start = src.find("[^1]").unwrap();
    let ref_end = ref_start + "[^1]".len();
    let def_line_start = src.find('\n').unwrap() + 1;

    let mut has_inline_replace = false;
    let mut has_inline_mark = false;
    let mut has_def_line_bg = false;
    let mut has_def_prefix_mark = false;
    for (range, dec) in set.iter_overlapping(0..src.len()) {
        match dec {
            Decoration::Replace { display: Some(s) }
                if range.start == ref_start && range.end == ref_end =>
            {
                assert_eq!(s.as_str(), "[^1]");
                has_inline_replace = true;
            }
            Decoration::Mark(m)
                if range.start == ref_start
                    && range.end == ref_end
                    && m.font_scale == Some(0.7)
                    && m.atomic =>
            {
                has_inline_mark = true;
            }
            Decoration::Line(s)
                if range.start == def_line_start && s.bg.is_some() =>
            {
                has_def_line_bg = true;
            }
            Decoration::Mark(m)
                if range.start == def_line_start && m.fg.is_some() && m.bold =>
            {
                has_def_prefix_mark = true;
            }
            _ => {}
        }
    }
    assert!(has_inline_replace, "expected inline Replace");
    assert!(has_inline_mark, "expected inline Mark (font_scale 0.7)");
    assert!(has_def_line_bg, "expected Line bg on definition line");
    assert!(has_def_prefix_mark, "expected colored prefix mark on def line");
}

#[test]
fn math_inline_and_block() {
    let src = "before $E = mc^2$ after\n\n$$\na = b\n$$\n";
    let state = EditorState::new(src);
    let set = math_decorations(&state, None, None);

    let inline_start = src.find("$E").unwrap();
    let inline_end = src.find("c^2$").unwrap() + "c^2$".len();

    let mut has_inline_mark = false;
    let mut block_line_count = 0;
    for (range, dec) in set.iter_overlapping(0..src.len()) {
        match dec {
            Decoration::Mark(m)
                if range.start == inline_start
                    && range.end == inline_end
                    && m.monospace
                    && m.fg.is_some() =>
            {
                has_inline_mark = true;
            }
            Decoration::Line(s) if s.bg.is_some() => {
                block_line_count += 1;
            }
            _ => {}
        }
    }
    assert!(has_inline_mark, "expected inline math Mark");
    assert!(
        block_line_count >= 3,
        "expected per-line bg over 3 block-math lines, got {block_line_count}"
    );
}

#[test]
fn mermaid_block_emits_per_line_bg() {
    let src = "intro\n```mermaid\ngraph TD\n  A --> B\n```\nafter\n";
    let state = EditorState::new(src);
    let set = mermaid_decorations(&state, None, None);

    let mut line_bgs = 0;
    for (_, dec) in set.iter_overlapping(0..src.len()) {
        if let Decoration::Line(s) = dec {
            if s.bg.is_some() {
                line_bgs += 1;
            }
        }
    }
    assert_eq!(
        line_bgs, 4,
        "expected bg on opening fence, 2 body lines, closing fence"
    );

    let plain = "```rust\nfn x() {}\n```\n";
    let s2 = EditorState::new(plain);
    let set2 = mermaid_decorations(&s2, None, None);
    assert!(
        set2.iter_overlapping(0..plain.len()).next().is_none(),
        "non-mermaid fence should not be decorated"
    );
}

// ── highlight (`==text==`) + colored `<span>` live-preview rendering ────────

#[test]
fn highlight_marks_inner_and_hides_equals_off_cursor_line() {
    let src = "first\nthis is ==marked== text\n";
    let mut state = EditorState::new(src);
    state.selection = Selection::single(0); // cursor on line 0, off the highlight
    let set = editor_md::styling::markdown_decorations(&state, None);

    let inner_start = src.find("marked").unwrap();
    let inner_end = inner_start + "marked".len();
    let mut has_bg = false;
    let mut hidden_open = false;
    let mut hidden_close = false;
    for (range, dec) in set.iter_overlapping(0..src.len()) {
        match dec {
            Decoration::Mark(m)
                if range.start == inner_start && range.end == inner_end && m.bg.is_some() =>
            {
                has_bg = true;
            }
            Decoration::Replace { display: None } if range.end == inner_start => hidden_open = true,
            Decoration::Replace { display: None } if range.start == inner_end => hidden_close = true,
            _ => {}
        }
    }
    assert!(has_bg, "expected a highlight background over the inner text");
    assert!(hidden_open && hidden_close, "expected both == markers hidden off the cursor line");
}

#[test]
fn highlight_reveals_equals_on_cursor_line() {
    let src = "this is ==marked== text\n";
    let mut state = EditorState::new(src);
    state.selection = Selection::single(src.find("marked").unwrap()); // cursor inside
    let set = editor_md::styling::markdown_decorations(&state, None);
    let any_hidden = set
        .iter_overlapping(0..src.len())
        .any(|(_, d)| matches!(d, Decoration::Replace { display: None }));
    assert!(!any_hidden, "markers stay visible while the cursor is on the line");
}

#[test]
fn highlight_inside_inline_code_is_ignored() {
    let src = "a `==x==` b\n";
    let state = EditorState::new(src);
    let set = editor_md::styling::markdown_decorations(&state, None);
    // The only Mark with a bg here should be the code background, never a
    // highlight over `x` — the scan must skip code regions.
    let highlight_over_x = set.iter_overlapping(0..src.len()).any(|(range, d)| {
        matches!(d, Decoration::Mark(m) if m.bg.is_some())
            && src.get(range.clone()) == Some("x")
    });
    assert!(!highlight_over_x, "highlight scan must skip inline code");
}

#[test]
fn color_span_marks_inner_with_parsed_color() {
    let src = "go <span style=\"color:#2e5e3a\">green</span> now\n";
    let mut state = EditorState::new(src);
    state.selection = Selection::single(0); // off the span line? same line; put at 0 (line 0)
    let set = editor_md::styling::markdown_decorations(&state, None);
    let inner_start = src.find("green").unwrap();
    let inner_end = inner_start + "green".len();
    let colored = set.iter_overlapping(0..src.len()).any(|(range, d)| {
        matches!(d, Decoration::Mark(m)
            if m.fg == Some(editor_core::decoration::Color::rgb(0x2e, 0x5e, 0x3a)))
            && range.start == inner_start
            && range.end == inner_end
    });
    assert!(colored, "expected the inner text colored with the span's hex");
}
