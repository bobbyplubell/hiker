//! Regression: removed lines in the inline diff are rendered as
//! `BlockSide::Above` phantom text blocks pushed through
//! `push_with_heights`. The widget's `measure()` must reserve vertical
//! space for them (via `apply_line_height_decorations` → `add_block_above`)
//! on a stable (non-edit) frame, or the removed lines paint with zero
//! height and vanish while the added lines (real buffer lines) still show.

use editor_core::decoration::{
    BlockDeco, BlockKind, BlockSide, BlockTextLine, Decoration,
};
use editor_core::rangeset::RangeSet;
use editor_core::state::Editor as EditorState;
use editor_diff::{DiffLayer, DiffOwner};
use editor_egui::widget::Widget as EditorWidget;
use editor_view::viewport::ViewState;

#[test]
fn removed_block_reserves_height() {
    let mut state = EditorState::new("alpha\nbravo\ncharlie\n");
    let mut view = ViewState::default();

    // A removed-line block above line 1 (the "bravo" line), exactly the shape
    // the inline diff emits for a removed hunk.
    let line1_start = state.doc.line_to_byte(1);
    let block = Decoration::Block(BlockDeco {
        side: BlockSide::Above,
        height: 18.0,
        kind: BlockKind::Text {
            lines: vec![BlockTextLine {
                text: "removed line".into(),
                bg: None,
                fg: None,
                gutter_marker: None,
                marks: Vec::new(),
                strikethrough: true,
            }],
        },
    });
    let set = RangeSet::from_iter([(line1_start..line1_start, block)]);
    view.decorations.push_with_heights(set);

    {
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(400.0, 300.0))
            .build_ui(|ui| {
                EditorWidget::new(&mut state, &mut view).show(ui);
            });
        harness.run();
    }

    assert!(
        view.height_map.block_above(1) >= 18.0,
        "removed-line block should reserve >=18px above line 1, got {}",
        view.height_map.block_above(1)
    );
}

/// End-to-end through the real `DiffLayer`: a replace edit (line "bravo" →
/// "DIFFERENT") must (a) generate a removed-line block-above decoration and
/// (b) have the widget reserve vertical space for it. This is the path the
/// inline patch-review + dirty-buffer diff-vs-disk toggle both ride.
#[test]
fn diff_layer_replace_renders_removed_line() {
    let base = "alpha\nbravo\ncharlie\n".to_string();
    let mut state = EditorState::new("alpha\nDIFFERENT\ncharlie\n");
    let mut view = ViewState::default();

    let layer = DiffLayer::from_base_text(base, state.doc.clone(), DiffOwner::Manual);
    assert!(!layer.is_empty(), "replace should produce a non-empty diff");
    let set = layer.decorations(18.0, None, true);

    // The decoration set must carry a removed-line phantom block (the only
    // place the old "bravo" text can show, since it's gone from the buffer).
    let has_removed_block = set.iter_all().any(|(_, d)| {
        matches!(
            d,
            Decoration::Block(BlockDeco { side: BlockSide::Above, kind: BlockKind::Text { .. }, .. })
        )
    });
    assert!(
        has_removed_block,
        "diff of a replaced line must emit a removed-line Text block above the new line"
    );

    view.decorations.push_with_heights(set);
    {
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(400.0, 300.0))
            .build_ui(|ui| {
                EditorWidget::new(&mut state, &mut view).show(ui);
            });
        harness.run();
    }

    // Whichever line the removed block anchored to, *some* line must reserve
    // block-above space — otherwise the removed text paints at zero height.
    let any_reserved = (0..state.doc.len_lines()).any(|l| view.height_map.block_above(l) > 0.0);
    assert!(
        any_reserved,
        "no line reserved block-above height; removed line would be invisible"
    );
}

/// A *similar*-line replace ("Body text here." -> "Body text now.") pairs as
/// a Modified line (similarity >= 0.5). The unified inline view must still
/// convey the removed text somehow: either a removed-line block-above OR a
/// red/deleted intraline mark. Today it renders only the green inserts, so
/// the removed part is invisible.
#[test]
fn similar_line_replace_shows_removed_part() {
    let base = "alpha\nBody text here.\ncharlie\n".to_string();
    let current = editor_core::rope::Rope::from_str("alpha\nBody text now.\ncharlie\n");
    let layer = DiffLayer::from_base_text(base, current, DiffOwner::Manual);
    assert!(!layer.is_empty(), "replace should produce a diff");
    let set = layer.decorations(18.0, None, true);

    let has_removed_block = set.iter_all().any(|(_, d)| {
        matches!(
            d,
            Decoration::Block(BlockDeco { side: BlockSide::Above, kind: BlockKind::Text { .. }, .. })
        )
    });
    // A "removed" intraline mark would use a distinct (removed) bg; we can't
    // read the color easily, but we CAN count Mark decorations: if only the
    // inserts are marked, there's exactly the added-side emphasis and no
    // removed-side rendering at all.
    let mark_count = set
        .iter_all()
        .filter(|(_, d)| matches!(d, Decoration::Mark(_)))
        .count();

    eprintln!("similar replace: has_removed_block={has_removed_block} mark_count={mark_count}");
    assert!(
        has_removed_block,
        "similar-line replace renders no removed-line block: the deleted text is invisible inline"
    );
}
