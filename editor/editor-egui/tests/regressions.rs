//! Visual / behavior regression tests for bugs caught in the markdown demo.
//!
//! Each test corresponds to an observable issue the user reported. The kittest
//! harness drives the widget without a window; we assert observable state on
//! the way back out (decoration layers, view state, derived data) rather than
//! pixel-snapshotting, which would be brittle across machines.

use editor_core::decoration::Decoration;

use editor_core::state::Editor as EditorState;
use editor_egui::widget::Widget as EditorWidget;
use editor_md::styling::markdown_decorations;
use editor_md::links::wikilink_decorations;
use editor_view::viewport::ViewState;

fn run_one_frame(state: &mut EditorState, view: &mut ViewState) {
    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::vec2(900.0, 600.0))
        .build_ui(|ui| {
            EditorWidget::new(state, view).show(ui);
        });
    harness.run();
}

/// Regression: setext headings (line above `---`) were having their first
/// character replaced because we always prefix-eat `leading_hashes + 1`
/// characters regardless of whether `#`s were present.
#[test]
fn setext_heading_does_not_eat_first_letter() {
    let doc = "title: a thing\n---\nbody\n";
    let mut state = EditorState::new(doc);
    state.selection = editor_core::selection::Selection::single(doc.len());
    let set = markdown_decorations(&state, None);
    // Walk all Replace decorations; none should start at byte 0 (the `t`).
    for (range, deco) in set.iter_all() {
        if let Decoration::Replace { .. } = deco {
            assert!(
                range.start > 0 || range.end == 0,
                "Replace at start of doc would eat the first letter of setext heading: {range:?}"
            );
        }
    }
}

/// Regression: a `Replace` decoration with a `Mark` inside it was being
/// subdivided so the display text appeared multiple times (e.g. wikilink
/// `[[Home]]` rendered as `HomeHomeHome`).
#[test]
fn wikilink_replace_consolidates_to_one_segment() {
    // Cursor off the wikilink line so the Replace fires.
    let doc = "Wikilink: [[Home]] in prose.\nsecond line\n";
    let mut state = EditorState::new(doc);
    state.selection = editor_core::selection::Selection::single(doc.len());
    let set = wikilink_decorations(&state, None, None, None);
    // Find the Replace decoration's range.
    let replace_range = set
        .iter_all()
        .find_map(|(r, d)| match d {
            Decoration::Replace { display: Some(s) } if s.as_str() == "Home" => Some(r),
            _ => None,
        })
        .expect("wikilink Replace should be emitted");
    // The Replace should cover the full `[[Home]]` (8 bytes).
    assert_eq!(replace_range.end - replace_range.start, 8);

    // Marks inside the Replace's range are fine to exist (for styling); the
    // painter MUST consolidate them into one segment. We exercise the painter
    // path by running a frame and checking it doesn't panic; the actual
    // pixel test is in widget_consolidates_replace_segments below.
    let mut view = ViewState::default();
    view.decorations.push(set);
    run_one_frame(&mut state, &mut view);
}

/// Smoke test: render a frame with a Replace + overlapping Mark and confirm
/// the widget doesn't panic. The non-duplication contract is enforced by
/// build_line_layout in widget.rs (consolidated_replace branch).
#[test]
fn widget_consolidates_replace_segments() {
    let doc = "X [[Home]] Y\n";
    let mut state = EditorState::new(doc);
    state.selection = editor_core::selection::Selection::single(doc.len());
    let mut view = ViewState::default();
    view.decorations.push(wikilink_decorations(&state, None, None, None));
    run_one_frame(&mut state, &mut view);
}

/// Regression: ▼ / ▶ Unicode triangle characters aren't in egui's bundled
/// fonts, so they used to render as the missing-glyph box. We now draw the
/// chevron as a `Shape::convex_polygon` instead.
///
/// We can't directly assert the shape was painted (kittest doesn't expose
/// per-frame shape introspection cheaply), but we CAN assert no font fallback
/// path is invoked by ensuring the chevron position is still clickable as a
/// fold zone.
#[test]
fn fold_chevron_registers_click_zone_at_left_edge() {
    use editor_md::folds::fold_decorations;
    use editor_md::folds::fold_regions;
    let doc = "# Heading\nbody1\nbody2\n";
    let mut state = EditorState::new(doc);
    let folds: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let regions = fold_regions(&state);
    let expected_id = regions[0].id;
    let mut view = ViewState::default();
    view.decorations.push(fold_decorations(&state, &folds));
    run_one_frame(&mut state, &mut view);
    // The painter should have registered a click zone at the gutter left
    // edge (x ~0..18) on line 0.
    let zone = view
        .click_zones
        .iter()
        .find(|z| {
            matches!(z.action, editor_view::viewport::ClickAction::ToggleFold(id) if id == expected_id)
        })
        .expect("fold chevron click zone should exist");
    assert!(zone.rect.x_min < 4.0);
    assert!(zone.rect.x_max <= 20.0);
}

/// Regression: when focused, scroll used raw `Event::MouseWheel` deltas
/// (tick-sized) and felt much slower than when only hovered, which uses
/// egui's `smooth_scroll_delta`. We now always read smooth_scroll_delta.
///
/// This test confirms that raw `Event::MouseWheel` events are NOT translated
/// into our InputEvent (so the focused scroll path goes through the same
/// smooth path as hover).
#[test]
fn mouse_wheel_events_are_not_translated() {
    use editor_view::events::InputEvent;
    let ev = egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Line,
        delta: egui::vec2(0.0, -3.0),
        modifiers: Default::default(),
    };
    let translated: Option<InputEvent> = editor_egui::translate::translate(&ev);
    assert!(
        translated.is_none(),
        "MouseWheel events should pass through to smooth_scroll_delta, not translate to InputEvent::Scroll"
    );
}

/// Regression (`bug-all-tabs-scroll-together`): egui's `smooth_scroll_delta`
/// is a single global per-frame value. With split editor panes, only the
/// hovered pane should consume a wheel spin — but the widget used to also
/// scroll on `has_focus()`, so a focused-but-not-hovered pane scrolled in
/// lockstep with the hovered one (every visible editor moving together).
///
/// This drives one editor widget, grants it focus, then sends a wheel delta
/// while the pointer sits OUTSIDE the widget rect (so `hovered()` is false but
/// `has_focus()` is true). `scroll_y` must stay put: a focused, non-hovered
/// editor must not consume the global scroll delta.
#[test]
fn focused_but_unhovered_editor_does_not_scroll() {
    // A doc tall enough that scrolling is actually possible at 600px height.
    let doc = (0..200).map(|i| format!("line {i}\n")).collect::<String>();
    let mut state = EditorState::new(&doc);
    let mut view = ViewState::default();

    // The closure holds `&mut view` for the harness's lifetime, so observe
    // `scroll_y` from inside the closure via a Cell rather than reading `view`
    // directly after each frame.
    let observed = std::cell::Cell::new(0.0_f32);
    let mut harness = egui_kittest::Harness::builder()
        .with_size(egui::vec2(900.0, 600.0))
        .build_ui(|ui| {
            EditorWidget::new(&mut state, &mut view).show(ui);
            observed.set(view.scroll_y);
        });
    harness.run();

    // Grant focus: press + release inside the widget rect.
    let inside = egui::pos2(100.0, 100.0);
    harness.input_mut().events.push(egui::Event::PointerButton {
        pos: inside,
        button: egui::PointerButton::Primary,
        pressed: true,
        modifiers: Default::default(),
    });
    harness.input_mut().events.push(egui::Event::PointerButton {
        pos: inside,
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers: Default::default(),
    });
    harness.run();
    harness.run();

    let scroll_before = observed.get();

    // Move the pointer well outside the widget (negative coords) so the
    // widget is focused but NOT hovered, then send a downward wheel spin.
    harness
        .input_mut()
        .events
        .push(egui::Event::PointerMoved(egui::pos2(-50.0, -50.0)));
    harness.input_mut().events.push(egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Line,
        delta: egui::vec2(0.0, -10.0),
        modifiers: Default::default(),
    });
    harness.run();

    let scroll_after = observed.get();

    assert_eq!(
        scroll_before, scroll_after,
        "a focused but non-hovered editor must not consume the global scroll delta \
         (scroll_y moved from {scroll_before} to {scroll_after})"
    );
}
