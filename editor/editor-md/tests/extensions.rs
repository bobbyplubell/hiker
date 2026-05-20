use std::collections::HashSet;

use editor_core::{Decoration, EditorState, Selection};
use editor_md::{
    callout_decorations, frontmatter_fold, wikilink_decorations, FRONTMATTER_FOLD_ID,
};

#[test]
fn wikilink_with_alias_emits_replace_and_mark_when_cursor_off_line() {
    let src = "first line\nsee [[Target Page|the page]] later\n";
    let mut state = EditorState::new(src);
    // Cursor on line 0 — so line 1 (the wikilink) should be collapsed.
    state.selection = Selection::single(0);
    let set = wikilink_decorations(&state, None, None);

    let link_start = src.find("[[").unwrap();
    let link_end = src.find("]]").unwrap() + 2;
    let alias_start = src.find("the page").unwrap();
    let alias_end = alias_start + "the page".len();

    let mut has_replace = false;
    let mut has_mark = false;
    for (range, dec) in set.iter_overlapping(0..src.len()) {
        match dec {
            Decoration::Replace { display: Some(s) }
                if range.start == link_start
                    && range.end == link_end
                    && s.as_str() == "the page" =>
            {
                has_replace = true;
            }
            Decoration::Mark(m)
                if range.start == alias_start
                    && range.end == alias_end
                    && m.underline
                    && m.fg.is_some() =>
            {
                has_mark = true;
            }
            _ => {}
        }
    }
    assert!(has_replace, "expected Replace covering [[...|...]]");
    assert!(has_mark, "expected underlined Mark on alias text");
}

#[test]
fn wikilink_on_cursor_line_skips_replace() {
    let src = "see [[Target]] now\n";
    let mut state = EditorState::new(src);
    state.selection = Selection::single(0); // line 0 is the wikilink line
    let set = wikilink_decorations(&state, None, None);
    let any_replace = set
        .iter_overlapping(0..src.len())
        .any(|(_, d)| matches!(d, Decoration::Replace { .. }));
    assert!(!any_replace, "cursor on line should leave wikilink as source");
}

#[test]
fn callout_emits_line_bg_and_marker_mark() {
    let src = "> [!warning] heads up\n> body line\n\nafter\n";
    let state = EditorState::new(src);
    let set = callout_decorations(&state, None, None);

    // Line 0 starts at 0; Line 1 starts after first '\n'.
    let line0_start = 0;
    let line1_start = src.find('\n').unwrap() + 1;

    let mut line0_bg = false;
    let mut line1_bg = false;
    let mut marker_fg = false;
    for (range, dec) in set.iter_overlapping(0..src.len()) {
        match dec {
            Decoration::Line(s) if s.bg.is_some() => {
                if range.start == line0_start {
                    line0_bg = true;
                } else if range.start == line1_start {
                    line1_bg = true;
                }
            }
            Decoration::Mark(m) if m.fg.is_some() && range.start == 0 => {
                // The `>` marker on line 0.
                marker_fg = true;
            }
            _ => {}
        }
    }
    assert!(line0_bg, "expected callout line bg on head line");
    assert!(line1_bg, "expected callout line bg on continuation line");
    assert!(marker_fg, "expected colored marker mark on `>`");

    // A non-callout blockquote should produce no callout decorations.
    let plain = "> just a quote\n";
    let s2 = EditorState::new(plain);
    let set2 = callout_decorations(&s2, None, None);
    let any = set2.iter_overlapping(0..plain.len()).next().is_some();
    assert!(!any, "plain blockquote should not get callout decorations");
}

#[test]
fn frontmatter_emits_chevron_and_hides_body_when_collapsed() {
    let src = "---\ntitle: Hello\ntags: [a, b]\n---\nbody\n";
    let state = EditorState::new(src);

    // Not collapsed: chevron present, body lines NOT hidden.
    let empty = HashSet::new();
    let set = frontmatter_fold(&state, &empty, None);
    let mut has_chevron = false;
    let mut hides_any = false;
    for (range, dec) in set.iter_overlapping(0..src.len()) {
        if let Decoration::Line(s) = dec {
            if range.start == 0 {
                if let Some(c) = &s.fold_chevron {
                    if c.id == FRONTMATTER_FOLD_ID && !c.collapsed {
                        has_chevron = true;
                    }
                }
            }
            if s.hide {
                hides_any = true;
            }
        }
    }
    assert!(has_chevron, "expected fold chevron on first --- line");
    assert!(!hides_any, "uncollapsed frontmatter should not hide lines");

    // Collapsed: body lines hidden.
    let mut folds: HashSet<u64> = HashSet::new();
    folds.insert(FRONTMATTER_FOLD_ID);
    let set2 = frontmatter_fold(&state, &folds, None);
    let hidden_count = set2
        .iter_overlapping(0..src.len())
        .filter(|(_, d)| matches!(d, Decoration::Line(s) if s.hide))
        .count();
    assert!(hidden_count >= 3, "expected body lines hidden, got {hidden_count}");

    // No frontmatter present.
    let plain = "no frontmatter here\n";
    let s3 = EditorState::new(plain);
    let set3 = frontmatter_fold(&s3, &empty, None);
    assert!(
        set3.iter_overlapping(0..plain.len()).next().is_none(),
        "no frontmatter, no decorations"
    );
}
