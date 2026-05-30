use super::{
    classify_lines, compute_marks, marks_signature, measure_lines, options_signature,
    resolve_line_colors, Accum, Cache, LineKind, Options, Style,
};
use super::atlas::{rasterize_glyph_cell, CellGeom, GlyphMetric};
use editor_core::decoration::{Color, Decoration, MarkStyle};
use editor_core::rangeset::RangeSet;
use editor_core::state::Editor as EditorState;
use editor_view::command;
use editor_view::events::InputEvent;
use editor_view::viewport::ViewState;
use egui::{Color32, ColorImage, Rect, Vec2};

fn heading_layer(view: &mut ViewState, range: std::ops::Range<usize>) {
    let deco = Decoration::Mark(MarkStyle { bold: true, ..Default::default() });
    view.decorations.push(RangeSet::from_iter([(range, deco)]));
}

#[test]
fn measure_lines_splits_indent_and_visible() {
    let state = EditorState::new("  ab\nxyz\n");
    let m = measure_lines(&state);
    assert_eq!((m[0].indent, m[0].visible), (2, 2));
    assert_eq!((m[1].indent, m[1].visible), (0, 3));
}

#[test]
fn classify_maps_heading_decoration_to_its_line() {
    let state = EditorState::new("plain\n# Heading\nplain\n");
    let mut view = ViewState::default();
    heading_layer(&mut view, 6..15);
    let kinds = classify_lines(&state, &view);
    assert_eq!(kinds[0], LineKind::Plain);
    assert_eq!(kinds[1], LineKind::Heading);
    assert_eq!(kinds[2], LineKind::Plain);
}

#[test]
fn cache_recomputes_only_when_keys_change() {
    let state = EditorState::new("a\nb\nc\n");
    let mut view = ViewState::default();
    heading_layer(&mut view, 0..1);
    let mut cache = Cache::default();
    cache.refresh(&state, &view);
    let kinds0 = cache.kinds.clone();
    let metrics_id0 = cache.metrics_doc_id;
    let kinds_sig0 = cache.kinds_decos_sig;
    cache.refresh(&state, &view);
    assert_eq!(cache.metrics_doc_id, metrics_id0);
    assert_eq!(cache.kinds_decos_sig, kinds_sig0);
    assert_eq!(cache.kinds, kinds0);
    assert_eq!(cache.kinds, classify_lines(&state, &view));
    heading_layer(&mut view, 2..3);
    cache.refresh(&state, &view);
    assert_ne!(cache.kinds_decos_sig, kinds_sig0);
    assert_eq!(cache.metrics_doc_id, metrics_id0);
    assert_eq!(cache.kinds[1], LineKind::Heading);
}

#[test]
fn cache_metrics_key_tracks_document_identity() {
    let view = ViewState::default();
    let mut cache = Cache::default();
    let s1 = EditorState::new("hello\n");
    cache.refresh(&s1, &view);
    let id1 = cache.metrics_doc_id;
    assert_eq!(id1, s1.doc.content_id());
    let tx = s1.insert_at_selections("X");
    let s2 = s1.apply(tx);
    cache.refresh(&s2, &view);
    assert_ne!(cache.metrics_doc_id, id1);
    assert_eq!(cache.metrics_doc_id, s2.doc.content_id());
}

#[test]
fn scroll_delta_clamps_at_top_and_bottom() {
    let state = EditorState::new("x\n");
    let mut view = ViewState { height: 100.0, scroll_y: 5.0, ..Default::default() };
    let _ = command::handle(&state, &mut view, &InputEvent::Scroll { delta_x: 0.0, delta_y: 50.0 });
    assert_eq!(view.scroll_y, 0.0);
    let _ = command::handle(&state, &mut view, &InputEvent::Scroll { delta_x: 0.0, delta_y: -50.0 });
    assert_eq!(view.scroll_y, 0.0);
}

#[test]
fn rasterize_places_glyph_at_baseline() {
    // Fully-covered atlas bitmap.
    let img = ColorImage::new([4, 4], vec![Color32::from_white_alpha(255); 16]);
    // Cell is a 4×8 line box; the glyph bitmap spans points y∈[2,6) (top
    // 4px above the baseline at y=6), full width.
    let g = CellGeom { advance: 4.0, font_h: 8.0, cw: 4, ch: 8 };
    let m = GlyphMetric {
        pos: egui::pos2(0.0, 6.0),
        offset: egui::vec2(0.0, -4.0),
        size: egui::vec2(4.0, 4.0),
        min: [0, 0],
        max: [4, 4],
    };
    let mut out = vec![0.0f32; g.cw * g.ch];
    rasterize_glyph_cell(&img, &m, &g, &mut out);
    // Inside the bitmap band → inked; above and below → empty (baseline
    // preserved, glyph does NOT fill the whole cell).
    assert!(out[3 * g.cw] > 0.9, "row 3 (inside glyph) should be inked");
    assert!(out[0] < 0.1, "row 0 (above glyph) should be empty");
    assert!(out[7 * g.cw] < 0.1, "row 7 (below baseline) should be empty");
}

#[test]
fn resolve_line_colors_overlays_mark_fg() {
    let state = EditorState::new("hello\n");
    let mut view = ViewState::default();
    let red = Color::rgba(200, 30, 30, 255);
    let deco = Decoration::Mark(MarkStyle { fg: Some(red), ..Default::default() });
    // Color bytes 0..3 ("hel") red.
    view.decorations.push(RangeSet::from_iter([(0..3, deco)]));
    let base = Color32::from_gray(100);
    let colors = resolve_line_colors(&state, &view, 0, base);
    assert_eq!(colors.len(), 5); // "hello"
    assert_eq!(colors[0], Color32::from_rgba_unmultiplied(200, 30, 30, 255));
    assert_eq!(colors[2], Color32::from_rgba_unmultiplied(200, 30, 30, 255));
    assert_eq!(colors[3], base);
}

#[test]
fn accum_fill_and_resolve_compose_over_background() {
    let mut acc = Accum::new(2, 1);
    // Opaque white over the left pixel, nothing over the right.
    acc.fill(0.0, 0.0, 1.0, 1.0, Color32::WHITE, 1.0);
    let img = acc.resolve(Color32::from_rgba_premultiplied(0, 0, 0, 40));
    // Left: opaque white content wins.
    assert_eq!(img.pixels[0], Color32::from_rgba_premultiplied(255, 255, 255, 255));
    // Right: only the translucent background shows through.
    assert_eq!(img.pixels[1].a(), 40);
}

#[test]
fn options_signature_tracks_style_and_palette() {
    let bars = Options { style: Style::Bars, ..Default::default() };
    let glyphs = Options { style: Style::Glyphs, ..Default::default() };
    assert_ne!(options_signature(&bars), options_signature(&glyphs));
    // Same options → stable signature (so idle frames don't rebuild).
    assert_eq!(options_signature(&glyphs), options_signature(&Options::default()));
    let recolored = Options { color_heading: Color32::RED, ..Default::default() };
    assert_ne!(options_signature(&recolored), options_signature(&Options::default()));
}

// ── Mark-strip cache tests ────────────────────────────────────────────────

/// Build a test rect with a known size.
fn test_rect() -> Rect {
    Rect::from_min_size(egui::pos2(10.0, 20.0), Vec2::new(72.0, 800.0))
}

#[test]
fn marks_signature_stable_on_scroll_only() {
    // Signature must not change when only scroll_y changes — scroll frames
    // should reuse the cached mark geometry.
    let state = EditorState::new("line one\nline two\nline three\n");
    let opts = Options::default();
    let rect = test_rect();
    let scale = 0.5_f32;

    let view1 = ViewState { scroll_y: 0.0, ..Default::default() };
    let view2 = ViewState { scroll_y: 200.0, ..Default::default() };

    assert_eq!(
        marks_signature(&state, &view1, &opts, rect, scale),
        marks_signature(&state, &view2, &opts, rect, scale),
        "scroll_y change must not invalidate marks_signature",
    );
}

#[test]
fn marks_signature_changes_on_selection_change() {
    let mut state = EditorState::new("hello world\n");
    let opts = Options::default();
    let rect = test_rect();
    let scale = 0.5_f32;
    let view = ViewState::default();

    let sig_no_sel = marks_signature(&state, &view, &opts, rect, scale);

    // Give the state a non-empty selection range.
    state.selection =
        editor_core::selection::Selection::from_range(editor_core::selection::SelRange::new(0, 5));
    let sig_with_sel = marks_signature(&state, &view, &opts, rect, scale);

    assert_ne!(sig_no_sel, sig_with_sel, "non-empty selection must change marks_signature");
}

#[test]
fn marks_signature_changes_on_search_activation() {
    let state = EditorState::new("find me here\n");
    let opts = Options::default();
    let rect = test_rect();
    let scale = 0.5_f32;

    let view_inactive = ViewState::default();
    let mut view_active = ViewState::default();
    view_active.search.active = true;
    view_active.search.matches = vec![4..6];

    let sig_off = marks_signature(&state, &view_inactive, &opts, rect, scale);
    let sig_on = marks_signature(&state, &view_active, &opts, rect, scale);

    assert_ne!(sig_off, sig_on, "search activation must change marks_signature");
}

#[test]
fn marks_signature_changes_on_content_edit() {
    let state1 = EditorState::new("aaa\nbbb\n");
    let tx = state1.insert_at_selections("X");
    let state2 = state1.apply(tx);

    let opts = Options::default();
    let rect = test_rect();
    let scale = 0.5_f32;
    let view = ViewState::default();

    assert_ne!(
        marks_signature(&state1, &view, &opts, rect, scale),
        marks_signature(&state2, &view, &opts, rect, scale),
        "content edit must change marks_signature",
    );
}

/// Produce a `ViewState` whose `height_map` is synced to `line_count`
/// lines at the default 18 px line height, so `text_height(line)` returns
/// non-zero for valid lines and `compute_marks` can produce rects.
fn view_with_lines(line_count: usize) -> ViewState {
    let mut view = ViewState::default();
    view.height_map.sync_to_lines(line_count, view.line_height);
    view
}

#[test]
fn ensure_marks_reuses_cache_on_scroll_only() {
    // After the first call, a second call with only scroll_y changed must
    // not recompute (marks_key unchanged, marks_strips identical object).
    let state = EditorState::new("alpha\nbeta\ngamma\n");
    let opts = Options::default();
    let rect = test_rect();
    let scale = 0.5_f32;
    let mut view = ViewState { scroll_y: 0.0, ..view_with_lines(3) };

    let kinds = classify_lines(&state, &view);
    let mut cache = Cache { kinds, ..Default::default() };

    cache.ensure_marks(&state, &view, &opts, rect, scale);
    let key_after_first = cache.marks_key;

    view.scroll_y = 300.0;
    cache.ensure_marks(&state, &view, &opts, rect, scale);

    assert_eq!(
        cache.marks_key, key_after_first,
        "marks_key must be stable across scroll-only frames",
    );
}

#[test]
fn ensure_marks_rebuilds_on_selection_change() {
    let mut state = EditorState::new("hello\nworld\n");
    let opts = Options::default();
    let rect = test_rect();
    let scale = 0.5_f32;
    let view = view_with_lines(2);

    let kinds = classify_lines(&state, &view);
    let mut cache = Cache { kinds, ..Default::default() };

    cache.ensure_marks(&state, &view, &opts, rect, scale);
    let key0 = cache.marks_key;
    assert!(cache.marks_strips.is_empty(), "no non-empty selection → no strips");

    // Add a non-empty selection spanning the first line.
    state.selection =
        editor_core::selection::Selection::from_range(editor_core::selection::SelRange::new(0, 5));
    cache.ensure_marks(&state, &view, &opts, rect, scale);

    assert_ne!(cache.marks_key, key0, "marks_key must change after selection change");
    assert!(!cache.marks_strips.is_empty(), "selection strips must be computed after change");
}

#[test]
fn ensure_marks_rebuilds_on_search_change() {
    let state = EditorState::new("foo bar baz\n");
    let opts = Options::default();
    let rect = test_rect();
    let scale = 0.5_f32;
    let mut view = view_with_lines(1);

    let kinds = classify_lines(&state, &view);
    let mut cache = Cache { kinds, ..Default::default() };

    cache.ensure_marks(&state, &view, &opts, rect, scale);
    let key0 = cache.marks_key;

    view.search.active = true;
    view.search.matches = vec![0..3, 4..7];
    cache.ensure_marks(&state, &view, &opts, rect, scale);

    assert_ne!(cache.marks_key, key0, "marks_key must change when search activates");
    assert!(!cache.marks_strips.is_empty(), "search strips must be computed");
}

#[test]
fn compute_marks_produces_one_rect_per_touched_line() {
    // A selection spanning both lines should yield two strips (one per line).
    let state = EditorState::new("line0\nline1\n");
    let opts = Options::default();
    let rect = test_rect();
    let scale = 1.0_f32;
    let view = view_with_lines(2);
    let kinds = classify_lines(&state, &view);

    // Selection 0..11 covers both lines.
    let mut sel_state = state.clone();
    sel_state.selection =
        editor_core::selection::Selection::from_range(editor_core::selection::SelRange::new(0, 11));

    let strips = compute_marks(&sel_state, &view, &opts, &kinds, rect, scale);
    // Both line0 and line1 are touched — expect 2 strips.
    assert_eq!(strips.len(), 2, "selection spanning 2 lines → 2 mark rects");
    for mr in &strips {
        assert_eq!(mr.color, opts.color_selection);
    }
}
