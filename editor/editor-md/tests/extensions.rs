use std::collections::HashSet;

use editor_core::decoration::Decoration;

use editor_core::state::Editor as EditorState;
use editor_core::selection::Selection;
use editor_md::admonitions::callout_decorations;
use editor_md::meta::frontmatter_fold;
use editor_md::links::wikilink_decorations;
use editor_md::meta::FRONTMATTER_FOLD_ID;
#[test]
fn wikilink_path_form_emits_replace_and_mark_when_cursor_off_line() {
    // Under path-form (`wikilink-path-form`) both a bare basename and an
    // explicit vault-relative path are valid bodies; both should collapse to
    // a Replace + Mark when the cursor is off the link's line. There is no
    // `|alias` half — the body is the target verbatim and its label.
    for src in [
        "first line\nsee [[Name]] later\n",
        "first line\nsee [[folder/sub/Name]] later\n",
    ] {
        let mut state = EditorState::new(src);
        // Cursor on line 0 — so line 1 (the wikilink) should be collapsed.
        state.selection = Selection::single(0);
        let set = wikilink_decorations(&state, None, None, None);

        let link_start = src.find("[[").unwrap();
        let link_end = src.find("]]").unwrap() + 2;
        let body = &src[link_start + 2..link_end - 2];
        let body_start = link_start + 2;
        let body_end = body_start + body.len();

        let mut has_replace = false;
        let mut has_mark = false;
        for (range, dec) in set.iter_overlapping(0..src.len()) {
            match dec {
                Decoration::Replace { display: Some(s) }
                    if range.start == link_start
                        && range.end == link_end
                        && s.as_str() == body =>
                {
                    has_replace = true;
                }
                Decoration::Mark(m)
                    if range.start == body_start
                        && range.end == body_end
                        && m.underline
                        && m.fg.is_some() =>
                {
                    has_mark = true;
                }
                _ => {}
            }
        }
        assert!(has_replace, "expected Replace covering [[...]] for {src:?}");
        assert!(has_mark, "expected underlined Mark on body text for {src:?}");
    }
}

#[test]
fn wikilink_with_resolver_emits_clickable_live_title_widget() {
    use editor_md::links::WIKILINK_WIDGET_TAG;
    // Path-form: the body is the target the resolver receives. The resolver
    // maps a path (bare basename here, uniquely identifies the target) to its
    // current title — basename or frontmatter `title`, the resolver's pick.
    let src = "intro line\nsee [[meeting]] later\n";
    let mut state = EditorState::new(src);
    state.selection = Selection::single(0); // cursor off the link line
    let resolve = |target: &str| -> Option<String> {
        (target == "meeting").then(|| "Weekly Sync".to_string())
    };
    let set = wikilink_decorations(&state, None, None, Some(&resolve));
    let link_start = src.find("[[").unwrap();
    let widget = set.iter_overlapping(0..src.len()).find_map(|(range, d)| match d {
        Decoration::InlineWidget { widget, .. } if range.start == link_start => Some(widget.clone()),
        _ => None,
    });
    let widget = widget.expect("expected an InlineWidget pill for the resolved link");
    assert!(widget.handles_click(), "wikilink pill must be clickable");
    assert_eq!(widget.widget_id(), WIKILINK_WIDGET_TAG | link_start as u64);
    let disp = widget.display().expect("pill renders as text");
    assert_eq!(
        disp.text.as_str(),
        "Weekly Sync",
        "resolver's current title drives the pill label",
    );
}

#[test]
fn wikilink_unresolved_target_renders_distinctly() {
    use editor_md::links::COLOR_WIKILINK_UNRESOLVED;
    // Both a bare-name link with no match and an explicit-path link with no
    // file behind it should render in the unresolved style and fall back to
    // the raw path the user typed. [wikilink-unresolved]
    for src in [
        "x\nsee [[NoSuchNote]] end\n",
        "x\nsee [[folder/missing]] end\n",
    ] {
        let mut state = EditorState::new(src);
        state.selection = Selection::single(0);
        let resolve = |_t: &str| -> Option<String> { None }; // nothing resolves
        let set = wikilink_decorations(&state, None, None, Some(&resolve));
        let link_start = src.find("[[").unwrap();
        let link_end = src.find("]]").unwrap() + 2;
        let body = &src[link_start + 2..link_end - 2];
        let disp = set
            .iter_overlapping(0..src.len())
            .find_map(|(range, d)| match d {
                Decoration::InlineWidget { widget, .. } if range.start == link_start => {
                    widget.display()
                }
                _ => None,
            })
            .expect("unresolved link still renders a pill");
        assert_eq!(
            disp.text.as_str(),
            body,
            "unresolved pill falls back to the raw target",
        );
        assert_eq!(
            disp.fg,
            Some(COLOR_WIKILINK_UNRESOLVED),
            "unresolved uses the broken-link color for {src:?}",
        );
    }
}

#[test]
fn wikilink_on_cursor_line_skips_replace() {
    let src = "see [[Target]] now\n";
    let mut state = EditorState::new(src);
    state.selection = Selection::single(0); // line 0 is the wikilink line
    let set = wikilink_decorations(&state, None, None, None);
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
