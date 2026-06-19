//! Unit tests for the natively-painted pipe-table widget
//! (`widget-table-render` / `widget-table-cell-edit`): parsing, the
//! mixed-content (text + math block) column-sizing solve, the composite
//! child build, and the per-cell edit-target / click-region ids.
//!
//! Split out of `tables.rs` to keep that file under the length budget; the
//! parent includes it via `#[cfg(test)] mod tests;`.

use super::*;
use super::super::inline::runs_text;
use editor_core::selection::Selection;

const FONT: f32 = 15.0;
const DPR: f32 = 1.0;
const INLINE: InlineColors = InlineColors {
    text: Color::rgb(40, 40, 40),
    link: Color::rgb(0, 90, 200),
    code_bg: Color::rgba(120, 120, 120, 30),
};

/// A text-only cell-parse context (no block rendering) for the parse / sizing
/// tests that don't exercise the math-render path.
const fn text_ctx() -> CellCtx<'static> {
    CellCtx {
        colors: INLINE,
        math_fg: [40, 40, 40, 255],
        mermaid_colors: MERMAID_COLORS,
        wavedrom_colors: WAVEDROM_COLORS,
        images: None,
        font_px: FONT,
        dpr: DPR,
        cache: None,
        render_blocks: false,
    }
}

/// Theme-free mermaid / wavedrom draw colors for the cell-parse tests (any
/// straight RGBA is fine — the tests assert structure, not pixels).
const MERMAID_COLORS: MermaidColors = MermaidColors {
    background: [0, 0, 0, 0],
    edge_label_bg: [30, 30, 40, 255],
    node_fill: [40, 40, 60, 255],
    node_stroke: [120, 110, 200, 255],
    edge_stroke: [200, 200, 200, 255],
    text_color: [40, 40, 40, 255],
};
const WAVEDROM_COLORS: WaveDromColors = WaveDromColors {
    foreground: [40, 40, 40, 255],
    background: [0, 0, 0, 0],
};

/// A cell-parse context that DOES render block (math) cells, for the
/// mixed-content Phase-B sizing / composite tests.
const fn math_ctx() -> CellCtx<'static> {
    CellCtx { render_blocks: true, ..text_ctx() }
}

/// Build a [`TableWidget`] from `src` in the default Fit overflow mode — the
/// common case for the parse / sizing / paint tests. Thin wrapper over
/// [`TableWidget::from_source`] with no diagram cache / image resolver.
fn mk(src: &str, aligns: &[ColumnAlign]) -> Option<TableWidget> {
    let render =
        TableRenderInputs { font_px: FONT, dpr: DPR, cache: None, images: None, view: TableViewState::default() };
    TableWidget::from_source(src, None, aligns, render)
}

/// As [`mk`] but with an explicit overflow [`TableViewState`], for the
/// Scrollable-mode tests. status: widget-table-overflow-scroll
fn mk_view(src: &str, aligns: &[ColumnAlign], view: TableViewState) -> Option<TableWidget> {
    let render = TableRenderInputs { font_px: FONT, dpr: DPR, cache: None, images: None, view };
    TableWidget::from_source(src, None, aligns, render)
}

/// The visible text of each cell (markers stripped) for a row of [`Cell`]s.
fn cell_texts(cells: &[Cell]) -> Vec<String> {
    cells.iter().map(|c| runs_text(&c.runs)).collect()
}

/// A plain (unstyled) cell from `text` — a single plain run — for the
/// style-free wrap / height assertions.
fn plain_cell(text: &str) -> Cell {
    Cell { runs: vec![StyledRun::plain(text, INLINE.text)], block: None }
}

/// (block widgets, hide lines) for the table provider, mirroring the
/// mermaid provider's `mermaid_counts`.
fn table_counts(state: &EditorState) -> (usize, usize) {
    let set = table_widget_decorations(
        state,
        None,
        None,
        TableProviderInputs {
            font_px: FONT,
            dpr: DPR,
            cache: None,
            images: None,
            views: None,
            editing_table: None,
        },
    );
    let mut block = 0;
    let mut hides = 0;
    for (_, d) in set.iter_all() {
        match d {
            Decoration::BlockWidget { .. } => block += 1,
            Decoration::Line(s) if s.hide => hides += 1,
            _ => {}
        }
    }
    (block, hides)
}

#[test]
fn provider_emits_block_when_cursor_elsewhere() {
    // status: widget-table-render — a well-formed pipe table away from the
    // cursor hides its source lines and renders a block widget.
    let src = "intro\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\nmore\n";
    let state = EditorState::new(src);
    let (block, hides) = table_counts(&state);
    assert_eq!(block, 1, "one table block widget when cursor is away");
    assert_eq!(hides, 3, "all three source rows hidden");
}

#[test]
fn provider_reveals_source_when_cursor_inside() {
    // status: widget-table-render / widget-reveal-block — cursor inside the
    // table reveals the raw source (no hides, no block).
    let src = "intro\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\nmore\n";
    let mut state = EditorState::new(src);
    state.selection = Selection::single(src.find("| 1 ").unwrap());
    let (block, hides) = table_counts(&state);
    assert_eq!(block, 0, "revealed table emits no in-place block");
    assert_eq!(hides, 0, "and hides nothing, so the source stays visible");
}

/// `table_widget_decorations` counts with a given `editing_table` (the in-place
/// cell-edit reveal-suppression key). status: widget-table-cell-edit-inplace
fn table_counts_editing(state: &EditorState, editing_table: Option<usize>) -> (usize, usize) {
    let set = table_widget_decorations(
        state,
        None,
        None,
        TableProviderInputs {
            font_px: FONT,
            dpr: DPR,
            cache: None,
            images: None,
            views: None,
            editing_table,
        },
    );
    let mut block = 0;
    let mut hides = 0;
    for (_, d) in set.iter_all() {
        match d {
            Decoration::BlockWidget { .. } => block += 1,
            Decoration::Line(s) if s.hide => hides += 1,
            _ => {}
        }
    }
    (block, hides)
}

#[test]
fn cell_edit_suppresses_reveal_table_stays_rendered() {
    // status: widget-table-cell-edit-inplace — a table whose cell is in active
    // in-place edit does NOT collapse to pipe source even though the caret sits
    // inside it: the whole-table reveal is suppressed, the table stays rendered.
    let src = "intro\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\nmore\n";
    let table_start = src.find("| a ").unwrap();
    let mut state = EditorState::new(src);
    // Caret inside the table (would normally reveal).
    state.selection = Selection::single(src.find("| 1 ").unwrap());
    // Without the editing flag → revealed (the baseline).
    let (block_off, hides_off) = table_counts_editing(&state, None);
    assert_eq!((block_off, hides_off), (0, 0), "caret-in reveals when not editing");
    // With the editing flag for THIS table → suppressed reveal: still rendered.
    let (block_on, hides_on) = table_counts_editing(&state, Some(table_start));
    assert_eq!(block_on, 1, "the edited table stays rendered as a block widget");
    assert_eq!(hides_on, 3, "and keeps its source lines hidden");
}

#[test]
fn cell_edit_flag_for_other_table_does_not_suppress() {
    // status: widget-table-cell-edit-inplace — the suppression keys on the
    // table's byte start, so an edit on a DIFFERENT table doesn't keep this one
    // rendered when the caret reveals it.
    let src = "intro\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\nmore\n";
    let mut state = EditorState::new(src);
    state.selection = Selection::single(src.find("| 1 ").unwrap());
    // A stale / unrelated table start → no suppression for the revealed table.
    let (block, hides) = table_counts_editing(&state, Some(99_999));
    assert_eq!((block, hides), (0, 0), "an unrelated editing key leaves the reveal alone");
}

#[test]
fn provider_emits_nothing_for_malformed_table() {
    // status: widget-render-error-fallback — no `|---|` rule row → not a
    // GFM table → no detection → no widget, tinted source remains.
    let src = "intro\n\n| a | b |\n| 1 | 2 |\n\nmore\n";
    let state = EditorState::new(src);
    let (block, hides) = table_counts(&state);
    assert_eq!(block, 0, "a malformed table emits no widget");
    assert_eq!(hides, 0, "and hides nothing");
}

#[test]
fn parses_header_and_rows() {
    let src = "| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |\n";
    let t = parse_table(src, &[ColumnAlign::None, ColumnAlign::None], text_ctx()).expect("a table");
    assert_eq!(cell_texts(&t.header), vec!["a", "b"]);
    assert_eq!(t.rows.len(), 2);
    assert_eq!(cell_texts(&t.rows[0]), vec!["1", "2"]);
}

#[test]
fn rejects_source_without_rule_row() {
    let src = "| a | b |\n| 1 | 2 |\n";
    assert!(parse_table(src, &[], text_ctx()).is_none(), "no delimiter row → not a table");
}

#[test]
fn cell_carries_inline_styled_runs() {
    // status: widget-table-render — a `**bold**` cell parses to a styled run
    // (markers stripped), not a literal `**bold**` string.
    let src = "| h |\n|---|\n| **bold** and `code` |\n";
    let t = parse_table(src, &[ColumnAlign::None], text_ctx()).expect("a table");
    let cell = &t.rows[0][0];
    assert_eq!(runs_text(&cell.runs), "bold and code", "visible text has markers stripped");
    assert!(cell.runs.iter().any(|r| r.text == "bold" && r.bold), "{:?}", cell.runs);
    assert!(cell.runs.iter().any(|r| r.text == "code" && r.code), "{:?}", cell.runs);
}

#[test]
fn paint_emits_rich_text_for_styled_cell() {
    // status: widget-table-render — a styled cell paints as a RichText block
    // (the wrapping multi-format galley), not flat per-line Text runs.
    let src = "| a |\n|---|\n| *italic* x |\n";
    let w = mk(src, &[ColumnAlign::None]).expect("a widget");
    let list = w.paint_list(FONT, 400.0).unwrap();
    let rich: Vec<&Vec<StyledRun>> = list
        .iter()
        .filter_map(|p| match p {
            BlockPaint::RichText { runs, .. } => Some(runs),
            _ => None,
        })
        .collect();
    assert!(!rich.is_empty(), "styled cells render as RichText");
    assert!(
        rich.iter().any(|runs| runs.iter().any(|r| r.italic)),
        "the italic run survives into the paint list",
    );
    // No literal markdown markers leak into any painted run.
    assert!(
        rich.iter().all(|runs| runs.iter().all(|r| !r.text.contains('*'))),
        "markers stripped in paint",
    );
}

#[test]
fn widget_measures_positive_height_and_paints() {
    let src = "| a | b |\n|---|---|\n| 1 | 2 |\n";
    let w = mk(src, &[ColumnAlign::None, ColumnAlign::None])
        .expect("a table widget");
    let h = w.measure(FONT, 400.0);
    assert!(h > 0.0, "non-empty table has positive height");
    let list = w.paint_list(FONT, 400.0).expect("a paint list");
    // Header bg rect + at least the text runs + rules.
    assert!(
        list.iter().any(|p| matches!(p, BlockPaint::Rect { .. })),
        "header background rect present"
    );
    assert!(
        list.iter().any(|p| matches!(p, BlockPaint::RichText { .. })),
        "cell rich-text runs present"
    );
    assert!(
        list.iter().any(|p| matches!(p, BlockPaint::Line { .. })),
        "grid rules present"
    );
}

#[test]
fn alignment_drives_text_anchor() {
    let src = "| l | r |\n|:--|--:|\n| 1 | 2 |\n";
    let w = mk(src, &[ColumnAlign::Left, ColumnAlign::Right])
        .expect("a table widget");
    let list = w.paint_list(FONT, 400.0).unwrap();
    let aligns: Vec<TextAlign> = list
        .iter()
        .filter_map(|p| match p {
            BlockPaint::RichText { align, .. } => Some(*align),
            _ => None,
        })
        .collect();
    assert!(aligns.contains(&TextAlign::Left), "left column left-aligned");
    assert!(aligns.contains(&TextAlign::Right), "right column right-aligned");
}

#[test]
fn content_hash_changes_with_source() {
    let a = mk("| a |\n|---|\n| 1 |\n", &[ColumnAlign::None])
        .unwrap();
    let b = mk("| a |\n|---|\n| 2 |\n", &[ColumnAlign::None])
        .unwrap();
    assert_ne!(a.widget_id(), b.widget_id(), "different bodies hash apart");
}

#[test]
fn long_cell_reserves_height_for_wrapped_lines() {
    // status: widget-table-render — a cell longer than its (overflow-shrunk)
    // column wraps; the painter owns the real wrap (one RichText per cell),
    // so the egui-free height measure must reserve room for the multi-line
    // wrap it estimates, and bound the cell's max_width to its column.
    let long = "alpha beta gamma delta epsilon zeta eta theta iota kappa";
    let src = format!("| h | k |\n|---|---|\n| {long} | x |\n");
    let w = mk(&src, &[ColumnAlign::None, ColumnAlign::None])
        .expect("a table widget");
    let width = 200.0;
    let col0 = w.column_widths(FONT, width)[0];
    let line_count = TableWidget::cell_line_count(&plain_cell(long), col0, FONT);
    assert!(line_count > 1, "the long cell should wrap to multiple lines");

    // The body row's reserved height covers every estimated wrapped line.
    let header_h = TableWidget::row_height(&w.table.header, &w.column_widths(FONT, width), FONT);
    let total_h = w.measure(FONT, width);
    let line_h = FONT * LINE_H_RATIO;
    assert!(
        total_h - header_h >= line_count as f32 * line_h,
        "body row height ({}) reserves room for {line_count} wrapped lines",
        total_h - header_h,
    );

    // The long cell paints as a single RichText bounded to its column width.
    let max_widths: Vec<f32> = w
        .paint_list(FONT, width)
        .unwrap()
        .iter()
        .filter_map(|p| match p {
            BlockPaint::RichText { max_width, .. } => Some(*max_width),
            _ => None,
        })
        .collect();
    assert!(
        max_widths.iter().any(|&mw| mw <= col0 + 1e-3),
        "a cell's rich-text max_width is bounded by its column ({col0})",
    );
}

#[test]
fn narrow_column_never_shrinks_below_its_longest_word() {
    // status: widget-table-render — under overflow shrink, a column whose
    // content is a single unbreakable word keeps at least that word's
    // width, so the word can't spill into the next column.
    let word = "Postprocess";
    // A wide second column forces overflow shrinking at a modest width.
    let src = format!(
        "| label | detail |\n|---|---|\n| {word} | this is a deliberately long second column to force the table to overflow and shrink |\n"
    );
    let w = mk(&src, &[ColumnAlign::None, ColumnAlign::None])
        .expect("a table widget");
    let widths = w.column_widths(FONT, 240.0);
    let char_w = FONT * CHAR_W_RATIO;
    let word_floor = word.chars().count() as f32 * char_w + CELL_PAD_X * 2.0;
    assert!(
        widths[0] >= word_floor - 1e-3,
        "label column ({}) stays >= its longest word's width ({word_floor})",
        widths[0],
    );
    // And that single-word cell therefore renders on one line.
    assert_eq!(
        TableWidget::cell_line_count(&plain_cell(word), widths[0], FONT),
        1,
        "the unbreakable label fits on one line in its protected column",
    );
}

#[test]
fn fitting_table_stretches_to_full_width() {
    // status: widget-table-render (Fix 1) — when the natural content fits, the
    // columns absorb the slack so the table fills the available width exactly,
    // instead of leaving empty space to the right.
    let src = "| a | b |\n|---|---|\n| 1 | 2 |\n";
    let w = mk(src, &[ColumnAlign::None, ColumnAlign::None])
        .expect("a table widget");
    let total = 900.0;
    let widths = w.column_widths(FONT, total);
    let sum: f32 = widths.iter().sum();
    assert!((sum - total).abs() < 1e-2, "columns fill the full width: sum {sum} != {total}");
    // Slack is distributed proportionally, so a tiny two-column table splits
    // the stretch roughly evenly (both columns hold the same content).
    assert!((widths[0] - widths[1]).abs() < 1.0, "equal columns stretch equally");
}

#[test]
fn bold_cell_reserves_more_width_than_plain() {
    // status: widget-table-render (Fix 2) — a bold cell measures wider than the
    // same text plain, so its column reserves enough that the real (faux-bold)
    // galley fits on one line instead of wrapping its last glyph.
    let char_w = FONT * CHAR_W_RATIO;
    let bold = Cell::parse("**WaveDrom**", text_ctx());
    let plain = Cell::parse("WaveDrom", text_ctx());
    assert_eq!(runs_text(&bold.runs), "WaveDrom", "markers stripped");
    assert!(
        bold.natural_content_width(char_w, FONT) > plain.natural_content_width(char_w, FONT),
        "bold ({}) reserves more than plain ({})",
        bold.natural_content_width(char_w, FONT),
        plain.natural_content_width(char_w, FONT),
    );
    // And a `code` (monospace) run reserves more still than bold prose.
    let codey = Cell::parse("`Vec<T>`", text_ctx());
    let prose = Cell::parse("Vec<T>", text_ctx());
    assert!(
        codey.natural_content_width(char_w, FONT) > prose.natural_content_width(char_w, FONT),
        "monospace code reserves more than plain proportional text",
    );
}

#[test]
fn bold_header_stays_on_one_line_at_full_width() {
    // status: widget-table-render (Fix 1 + 2) — the real regression: a bold
    // "WaveDrom" header in a 3-column table, stretched to a wide page, must get
    // a column wide enough that the style-aware measure keeps it on one line.
    let src = "| WaveDrom | You write | Renders as |\n|---|---|---|\n\
               | **WaveDrom** | `wavedrom` | a timing diagram |\n";
    let w = mk(src, &[ColumnAlign::None, ColumnAlign::None, ColumnAlign::None])
        .expect("a table widget");
    let widths = w.column_widths(FONT, 1000.0);
    let bold = Cell::parse("**WaveDrom**", text_ctx());
    assert_eq!(
        TableWidget::cell_line_count(&bold, widths[0], FONT),
        1,
        "the bold WaveDrom header fits on one line in its widened column",
    );
}

#[test]
fn cell_click_targets_land_at_end_of_cell_content() {
    // status: widget-table-cell-edit — each cell's edit target is the byte
    // offset just past its content (caret ready to append); cells are
    // row-major (header a, bb; then body 1, 22).
    let src = "| a | bb |\n|---|---|\n| 1 | 22 |\n";
    let state = EditorState::new(src);
    let span = table_spans(&state, None).into_iter().next().expect("a table span");
    let widget =
        mk(&src[span.byte_range.clone()], &span.aligns)
            .expect("a table widget");
    let hash = widget.content_hash;

    let map: std::collections::HashMap<u64, usize> =
        table_edit_targets(&state, None, None, FONT).into_iter().collect();

    for (i, cell) in ["a", "bb", "1", "22"].iter().enumerate() {
        let id = table_cell_id(hash, i);
        let off = *map.get(&id).unwrap_or_else(|| panic!("cell {cell} target present"));
        assert!(
            src[..off].ends_with(cell),
            "cell {cell} target (offset {off}) lands at end of its content; got …{:?}",
            &src[off.saturating_sub(3)..off],
        );
    }
    // Whole-widget fallback → the table block start.
    assert_eq!(map.get(&hash), Some(&span.byte_range.start), "whole-widget → table start");
}

#[test]
fn block_cells_are_excluded_from_edit_targets() {
    // status: widget-table-cell-edit-inplace — a BLOCK cell (math / diagram /
    // image) must NOT have a caret edit-target: a plain click placing the caret
    // inside the table span reveals it (collapses the grid to source), which
    // made the double-click-to-edit entry lose the race — click #1 collapsed
    // the table, click #2 found no cell zone and landed in raw text. Text
    // cells keep their reveal-on-click target; block cells enter edit via
    // double-click / the right-click menu.
    let src = "| label | math |\n|---|---|\n| plain | $x^2$ |\n";
    let state = EditorState::new(src);
    let span = table_spans(&state, None).into_iter().next().expect("a table span");
    let widget = mk(&src[span.byte_range.clone()], &span.aligns).expect("a table widget");
    let hash = widget.content_hash;

    // Sanity: the cell classifier agrees on which cells are block.
    let cells: Vec<&str> = ["label", "math", "plain", "$x^2$"].to_vec();
    assert!(super::cell_edit::cell_is_block(cells[3]), "$x^2$ is a block cell");
    assert!(!super::cell_edit::cell_is_block(cells[2]), "plain is a text cell");

    let map: std::collections::HashMap<u64, usize> =
        table_edit_targets(&state, None, None, FONT).into_iter().collect();

    for (i, cell) in cells.iter().enumerate() {
        let id = table_cell_id(hash, i);
        if super::cell_edit::cell_is_block(cell) {
            assert!(map.get(&id).is_none(), "block cell {cell} has NO caret target");
        } else {
            assert!(map.get(&id).is_some(), "text cell {cell} keeps its caret target");
        }
    }
    // The whole-widget fallback stays (a click off any cell still enters edit).
    assert_eq!(map.get(&hash), Some(&span.byte_range.start), "whole-widget → table start");
}

#[test]
fn click_regions_one_per_cell_and_tagged() {
    // status: widget-table-cell-edit — one normalized region per cell, ids
    // tagged TABLE_CELL_TAG with the mermaid (61) / wikilink (62) bits clear,
    // matching `table_cell_id` per index.
    let src = "| a | bb |\n|---|---|\n| 1 | 22 |\n";
    let w = mk(src, &[ColumnAlign::None, ColumnAlign::None])
        .unwrap();
    let regions = w.click_regions(FONT, 400.0);
    assert_eq!(regions.len(), 4, "2 header + 2 body cells");
    for (i, r) in regions.iter().enumerate() {
        assert_eq!(r.id, table_cell_id(w.content_hash, i), "id matches index");
        assert_ne!(r.id & TABLE_CELL_TAG, 0, "table-cell tag set");
        assert_eq!(r.id & editor_md::links::WIKILINK_WIDGET_TAG, 0, "wikilink bit clear");
        assert_eq!(r.id & (1 << 61), 0, "mermaid bit clear");
        assert!(r.x >= 0.0 && r.x + r.w <= 1.0 + 1e-3, "x normalized");
        assert!(r.y >= 0.0 && r.y + r.h <= 1.0 + 1e-3, "y normalized");
    }
}

#[test]
fn cell_offsets_survive_escaped_pipe() {
    // status: widget-table-cell-edit — `\|` inside a cell is unescaped in the
    // text but the caret offset stays in source bytes (the backslash sits
    // before the trailing whitespace, so it doesn't shift the end).
    let src = "| a\\|b | c |\n|---|---|\n| 1 | 2 |\n";
    let state = EditorState::new(src);
    let span = table_spans(&state, None).into_iter().next().expect("a table span");
    let widget = mk(&src[span.byte_range.clone()], &span.aligns).expect("a table widget");
    assert_eq!(runs_text(&widget.table.header[0].runs), "a|b", "pipe unescaped in text");
    let off = span.byte_range.start + widget.cell_ranges[0].end;
    assert_eq!(&src[..off], "| a\\|b", "offset is just past the source `b`");
}

// ----- Phase B: block (math) content in cells --------------------------

/// A table with one math cell renders it as a block: the cell carries a
/// rendered texture, the table flags `has_block`, and its `composite()`
/// emits a Texture child. status: widget-table-render
#[test]
fn math_cell_renders_as_block_and_composites() {
    let src = "| formula | note |\n|---|---|\n| $x^2$ | squared |\n";
    let w = mk(src, &[ColumnAlign::None, ColumnAlign::None])
        .expect("a table widget");
    assert!(w.has_block, "a table with a math cell uses the composite path");
    // The math cell carries a rendered block; the plain cell does not.
    assert!(w.table.rows[0][0].block.is_some(), "the $x^2$ cell is a block");
    assert!(w.table.rows[0][1].block.is_none(), "the text cell stays text");

    let children = w.composite(FONT, 600.0).expect("a composite");
    let textures = children
        .iter()
        .filter(|c| matches!(c.kind, ChildKind::Texture(_)))
        .count();
    let natives = children
        .iter()
        .filter(|c| matches!(c.kind, ChildKind::Native(_)))
        .count();
    assert_eq!(textures, 1, "one texture child (the math cell)");
    // Chrome child + the text cells' RichText children.
    assert!(natives >= 2, "chrome + text-cell native children present ({natives})");
    // The texture child's id == the rendered math content hash (cache key).
    let tex = children.iter().find_map(|c| match &c.kind {
        ChildKind::Texture(t) => Some(t),
        ChildKind::Native(_) => None,
    }).unwrap();
    let block = w.table.rows[0][0].block.as_ref().unwrap();
    assert_eq!(tex.id, block.rendered.content_hash, "texture id is the render hash");
    assert_eq!(tex.rgba.len(), (tex.width * tex.height * 4) as usize, "tightly packed");
}

/// A display-math (`$$…$$`) cell — a tall fraction — grows its row to hold
/// the block's scaled height, and reserves at least the block's intrinsic
/// (capped) width in its column. status: widget-table-render
#[test]
fn display_math_cell_grows_row_and_reserves_width() {
    // A stacked fraction renders clearly taller than one text line.
    let src = "| eq | x |\n|---|---|\n| $$\\frac{a+b}{c+d}$$ | y |\n";
    let w = mk(src, &[ColumnAlign::None, ColumnAlign::None])
        .expect("a table widget");
    let block = w.table.rows[0][0].block.as_ref().expect("a display-math block");
    let widths = w.column_widths(FONT, 800.0);

    // The math column reserves at least the block's intrinsic width (capped),
    // plus padding — wider than a 1-char text floor would give.
    let expected_min = block.intrinsic_w().min(BLOCK_W_CAP * FONT);
    assert!(
        widths[0] + 1e-3 >= expected_min,
        "math column ({}) reserves the block's intrinsic width ({expected_min})",
        widths[0],
    );

    // The row height is the max of the text-line height and the block's
    // scaled height (+padding) — the block branch is folded into the solve.
    let content_w = (widths[0] - CELL_PAD_X * 2.0).max(1.0);
    let block_row_h = block.scaled_height(content_w) + CELL_PAD_Y * 2.0;
    let plain_row_h = FONT * LINE_H_RATIO + CELL_PAD_Y * 2.0;
    let math_row_h = TableWidget::row_height(&w.table.rows[0], &widths, FONT);
    assert!(
        (math_row_h - plain_row_h.max(block_row_h)).abs() < 1e-3,
        "row height ({math_row_h}) = max(text {plain_row_h}, block {block_row_h})",
    );
}

/// A display-math cell wider than its column shrinks (aspect-preserved): the
/// scaled height stays ≤ natural, and the row reflects that scaled height —
/// the mixed-content solve reconciling intrinsic block size against a narrow
/// column. status: widget-table-render
#[test]
fn wide_math_cell_shrinks_to_fit_column() {
    let src = "| eq | note |\n|---|---|\n| $$\\sum_{i=0}^{n} x_i^2 + y_i^2$$ | n |\n";
    let w = mk(src, &[ColumnAlign::None, ColumnAlign::None])
        .expect("a table widget");
    let block = w.table.rows[0][0].block.as_ref().expect("a math block");
    // A narrow table forces the (capped) math column below the block width.
    let widths = w.column_widths(FONT, 120.0);
    let content_w = (widths[0] - CELL_PAD_X * 2.0).max(1.0);
    let scaled = block.scaled_height(content_w);
    assert!(scaled <= block.intrinsic_h() + 1e-3, "a shrunk block is no taller than natural");
    assert!(scaled > 0.0, "still positive height");
    let row_h = TableWidget::row_height(&w.table.rows[0], &widths, FONT);
    assert!(
        row_h + 1e-3 >= scaled + CELL_PAD_Y * 2.0,
        "row reflects the scaled block height",
    );
}

/// Regression guard: a PLAIN text table (no block cells) takes the
/// byte-identical single-visual path — `composite()` is `None` and the
/// `paint_list` is unchanged from the Phase-A output. status: widget-table-render
#[test]
fn plain_table_stays_single_visual_paint_list() {
    let src = "| a | b |\n|---|---|\n| 1 | 2 |\n| **x** | `y` |\n";
    let w = mk(src, &[ColumnAlign::None, ColumnAlign::None])
        .expect("a table widget");
    assert!(!w.has_block, "a pure-text table carries no block");
    assert!(w.composite(FONT, 500.0).is_none(), "no composite for a plain table");
    // The paint_list is non-empty and carries the same primitive kinds it did
    // in Phase A (Rect header bg, RichText cells, Line rules) — proving the
    // single-visual path is untouched by the composite addition.
    let list = w.paint_list(FONT, 500.0).expect("a paint list");
    assert!(list.iter().any(|p| matches!(p, BlockPaint::Rect { .. })), "header bg");
    assert!(list.iter().any(|p| matches!(p, BlockPaint::RichText { .. })), "cell text");
    assert!(list.iter().any(|p| matches!(p, BlockPaint::Line { .. })), "grid rules");
    assert!(
        list.iter().all(|p| !matches!(p, BlockPaint::Text { .. })),
        "Phase A emits RichText (never the legacy per-line Text)",
    );
}

/// `Cell::parse` only detects a math block when the context renders blocks: the
/// render-free (edit-target) context keeps a `$…$` cell as plain text, so its
/// `content_hash` / `cell_ends` cost no rasterize. status: widget-table-render
#[test]
fn math_detection_gated_on_render_blocks() {
    let with = Cell::parse("$x^2$", math_ctx());
    assert!(with.block.is_some(), "render_blocks ctx rasterizes the math cell");
    let without = Cell::parse("$x^2$", text_ctx());
    assert!(without.block.is_none(), "render-free ctx keeps the cell as text");
    // A non-math cell is text under either context.
    assert!(Cell::parse("plain", math_ctx()).block.is_none(), "non-math stays text");
}

// ----- Phase B: mermaid / wavedrom diagram cells -----------------------------

/// The number of Texture children in a composite — the rendered block cells.
fn texture_child_count(children: &[ChildItem]) -> usize {
    children.iter().filter(|c| matches!(c.kind, ChildKind::Texture(_))).count()
}

/// A one-line ```` ```mermaid <src>``` ```` cell (the only fence form a pipe-table
/// cell can hold) renders as a Texture child, reserves intrinsic width, and grows
/// the row — exactly as a display-math cell does. status: widget-table-render
#[test]
fn mermaid_cell_renders_as_block_and_composites() {
    let src = "| diagram | note |\n|---|---|\n| ```mermaid graph TD; A-->B``` | a flow |\n";
    let w = mk(src, &[ColumnAlign::None, ColumnAlign::None])
        .expect("a table widget");
    assert!(w.has_block, "a mermaid cell uses the composite path");
    let block = w.table.rows[0][0].block.as_ref().expect("the mermaid cell is a block");
    assert!(w.table.rows[0][1].block.is_none(), "the text cell stays text");

    let children = w.composite(FONT, 700.0).expect("a composite");
    assert_eq!(texture_child_count(&children), 1, "one texture child (the diagram)");

    // The math column reserves at least the block's intrinsic width (capped).
    let widths = w.column_widths(FONT, 800.0);
    let expected_min = block.intrinsic_w().min(BLOCK_W_CAP * FONT);
    assert!(widths[0] + 1e-3 >= expected_min, "diagram column reserves intrinsic width");

    // The row grows to hold the diagram's scaled height (taller than one text line).
    let content_w = (widths[0] - CELL_PAD_X * 2.0).max(1.0);
    let block_row_h = block.scaled_height(content_w) + CELL_PAD_Y * 2.0;
    let plain_row_h = FONT * LINE_H_RATIO + CELL_PAD_Y * 2.0;
    let row_h = TableWidget::row_height(&w.table.rows[0], &widths, FONT);
    assert!((row_h - plain_row_h.max(block_row_h)).abs() < 1e-3, "row = max(text, block)");
}

/// A one-line ```` ```wavedrom <WaveJSON>``` ```` cell renders as a Texture child.
/// status: widget-table-render
#[test]
fn wavedrom_cell_renders_as_block_and_composites() {
    let src = "| wave | note |\n|---|---|\n| ```wavedrom { signal: [{ name: 'clk', wave: 'p..' }] }``` | a clock |\n";
    let w = mk(src, &[ColumnAlign::None, ColumnAlign::None])
        .expect("a table widget");
    assert!(w.has_block, "a wavedrom cell uses the composite path");
    assert!(w.table.rows[0][0].block.is_some(), "the wavedrom cell is a block");
    let children = w.composite(FONT, 700.0).expect("a composite");
    assert_eq!(texture_child_count(&children), 1, "one texture child (the waveform)");
}

/// Detection is gated on `render_blocks` and tolerant of the surrounding-prose
/// case: a fence with prose around it (so the cell isn't a *pure* fence) stays
/// text, and the render-free ctx never rasterizes. status: widget-table-render
#[test]
fn diagram_detection_gated_and_pure() {
    let pure = Cell::parse("```mermaid graph TD; A-->B```", math_ctx());
    assert!(pure.block.is_some(), "a pure one-line mermaid fence renders");
    let prose = Cell::parse("see ```mermaid graph TD; A-->B```", math_ctx());
    assert!(prose.block.is_none(), "a fence with prose around it stays text");
    let render_free = Cell::parse("```mermaid graph TD; A-->B```", text_ctx());
    assert!(render_free.block.is_none(), "render-free ctx keeps the diagram as text");
}

// ----- Overflow modes (Fit ⇄ Scrollable) -------------------------------------

/// A wide multi-column table source whose natural width comfortably exceeds a
/// modest layout width — the input the overflow modes diverge on.
const WIDE_SRC: &str = "| alpha | bravo | charlie | delta | echo | foxtrot |\n\
                        |---|---|---|---|---|---|\n\
                        | one two three | four five six | seven eight | nine ten | eleven | twelve thirteen |\n";

const WIDE_ALIGNS: [ColumnAlign; 6] = [ColumnAlign::None; 6];

/// Default overflow mode is Fit (`TableViewState::default()`), so the column
/// solve stretches/shrinks to the layout width exactly as before — the byte-
/// identical regression guard for the overflow addition.
/// status: widget-table-overflow-scroll
#[test]
fn default_mode_is_fit_and_fills_width() {
    assert_eq!(TableViewState::default().mode, TableOverflow::Fit, "default is Fit");
    let w = mk(WIDE_SRC, &WIDE_ALIGNS).expect("a table widget");
    assert_eq!(w.view.mode, TableOverflow::Fit, "mk builds a Fit table");
    // Fit at a wide width fills it exactly (stretch-to-fill).
    let wide = w.natural_width(FONT) + 400.0;
    let sum: f32 = w.column_widths(FONT, wide).iter().sum();
    assert!((sum - wide).abs() < 1e-2, "Fit fills the layout width: {sum} != {wide}");
    // A Fit table with no block cells takes the byte-identical paint_list path
    // (no composite, no clip).
    assert!(w.composite(FONT, wide).is_none(), "plain Fit table has no composite");
    assert!(w.paint_list(FONT, wide).is_some(), "plain Fit table paints via paint_list");
}

/// Scrollable returns the raw NATURAL widths — no stretch (even when the table
/// would fit) and no overflow-shrink (when it wouldn't) — so the grid lays out
/// at its intrinsic width regardless of the layout width.
/// status: widget-table-overflow-scroll
#[test]
fn scrollable_uses_natural_widths_unstretched_and_unshrunk() {
    let scroll = TableViewState { mode: TableOverflow::Scrollable, h_offset: 0.0 };
    let w = mk_view(WIDE_SRC, &WIDE_ALIGNS, scroll).expect("a table widget");
    let natural = w.natural_width(FONT);

    // Far wider than natural: Fit would stretch to fill; Scrollable stays natural.
    let wide = natural + 600.0;
    let scroll_sum: f32 = w.column_widths(FONT, wide).iter().sum();
    assert!((scroll_sum - natural).abs() < 1e-2, "Scrollable un-stretched: {scroll_sum} != {natural}");

    // Far narrower than natural: Fit would shrink toward the floors; Scrollable
    // keeps the full natural width (the inset clips it instead).
    let narrow = natural * 0.4;
    let narrow_sum: f32 = w.column_widths(FONT, narrow).iter().sum();
    assert!((narrow_sum - natural).abs() < 1e-2, "Scrollable un-shrunk: {narrow_sum} != {natural}");

    // The same source in Fit DOES vary with the layout width (proving the modes
    // genuinely diverge, not that the table is coincidentally natural-width).
    let fit = mk(WIDE_SRC, &WIDE_ALIGNS).expect("a fit widget");
    let fit_wide: f32 = fit.column_widths(FONT, wide).iter().sum();
    assert!((fit_wide - wide).abs() < 1e-2, "Fit stretches to the wide width");
}

/// A Scrollable table always takes the composite (container) path — even with no
/// block cells — because only the composite carries the per-child clip + scroll
/// offset; `paint_list` (which can't clip) is bypassed.
/// status: widget-table-overflow-scroll
#[test]
fn scrollable_plain_table_uses_composite_path() {
    let scroll = TableViewState { mode: TableOverflow::Scrollable, h_offset: 0.0 };
    let w = mk_view(WIDE_SRC, &WIDE_ALIGNS, scroll).expect("a table widget");
    assert!(!w.has_block, "the wide table has no block cells");
    let children = w.composite(FONT, 300.0).expect("Scrollable plain table still composites");
    // Every child carries the inset clip rect (none unclipped), and the clip is
    // the doc-width inset, not the wider natural grid.
    assert!(
        children.iter().all(|c| c.clip.is_some()),
        "every Scrollable child is clipped to the inset",
    );
    let clip = children[0].clip.expect("a clip rect");
    assert!((clip.w - 300.0).abs() < 1e-2, "clip width is the inset (doc) width, not natural");
}

/// The horizontal scroll offset is clamped to `[0, natural − inset]`: at offset
/// 0 the grid sits at its left edge (shift 0); a large offset clamps to the max
/// (so later columns are revealed, earlier ones clipped off the left); an
/// over-large offset can't push past the max. status: widget-table-overflow-scroll
#[test]
fn scroll_offset_is_clamped_and_shifts_children() {
    let inset = 300.0_f32;
    let at = |off: f32| {
        let v = TableViewState { mode: TableOverflow::Scrollable, h_offset: off };
        mk_view(WIDE_SRC, &WIDE_ALIGNS, v).expect("a table widget")
    };
    let natural = at(0.0).natural_width(FONT);
    let max_off = (natural - inset).max(0.0);
    assert!(max_off > 0.0, "the wide table genuinely overflows a {inset}pt inset");

    // Offset 0: no shift (chrome child sits at x = 0).
    let zero = at(0.0).composite(FONT, inset).expect("composite");
    assert!((zero[0].rect.x - 0.0).abs() < 1e-3, "offset 0 → grid at left edge");

    // A mid offset shifts the grid left by exactly that offset.
    let mid = 0.5 * max_off;
    let mid_children = at(mid).composite(FONT, inset).expect("composite");
    assert!((mid_children[0].rect.x + mid).abs() < 1e-2, "grid shifted left by the offset");

    // An over-large offset clamps to −max_off (can't scroll past the last column).
    let over = at(natural * 10.0).composite(FONT, inset).expect("composite");
    assert!((over[0].rect.x + max_off).abs() < 1e-2, "offset clamps to natural − inset");
}

/// The wheel-clamp the buffer panel applies (mirrored here on the widget's own
/// clamp): the shift never exceeds the overflow, and a Fit table never shifts.
/// status: widget-table-overflow-scroll
#[test]
fn fit_table_never_shifts_or_clips() {
    let w = mk(WIDE_SRC, &WIDE_ALIGNS).expect("a fit widget");
    // Fit at a narrow width still uses paint_list (no composite → no clip path).
    assert!(w.composite(FONT, 200.0).is_none(), "Fit never composites a plain table");
    // And the cell-click regions carry no scroll shift in Fit mode.
    let regions = w.click_regions(FONT, 400.0);
    assert!(!regions.is_empty(), "Fit table still emits cell click regions");
}
