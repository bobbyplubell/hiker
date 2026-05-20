use editor_core::{diff::diff_lines, BlockKind, BlockSide, Decoration, Rope};

#[test]
fn unified_emits_line_bg_for_added_lines() {
    let left = "a\nb\n";
    let right = "a\nb\nc\n";
    let hunks = diff_lines(left, right);
    let set = editor_diff::unified_decorations(&Rope::from_str(right), left, &hunks, 18.0, None);
    assert!(set
        .iter_all()
        .any(|(_, d)| matches!(d, Decoration::Line(_))));
}

#[test]
fn modified_hunk_emits_word_marks() {
    let left = "hello world\n";
    let right = "hello rust\n";
    let hunks = diff_lines(left, right);
    let right_rope = Rope::from_str(right);
    let set = editor_diff::unified_decorations(&right_rope, left, &hunks, 18.0, None);
    assert!(set
        .iter_all()
        .any(|(_, d)| matches!(d, Decoration::Mark(s) if s.bg.is_some())));
}

#[test]
fn alignment_emits_hatched_block_on_shorter_side() {
    let left = "a\nb\nc\nd\ne\n";
    let right = "x\ny\ne\n";
    let hunks = diff_lines(left, right);
    let (_, right_set) = editor_diff::alignment_decorations(
        &Rope::from_str(left),
        &Rope::from_str(right),
        &hunks,
        18.0,
        None,
    );
    assert!(right_set
        .iter_all()
        .any(|(_, d)| matches!(d, Decoration::Block(b) if matches!(b.kind, BlockKind::Hatched(_)))));
}

#[test]
fn alignment_spacer_is_above_so_line_pushes_down() {
    let left = "context\nremoved1\nremoved2\nafter\n";
    let right = "context\nafter\n";
    let hunks = diff_lines(left, right);
    let (_, right_set) = editor_diff::alignment_decorations(
        &Rope::from_str(left),
        &Rope::from_str(right),
        &hunks,
        18.0,
        None,
    );
    let block = right_set
        .iter_all()
        .find_map(|(_, d)| match d {
            Decoration::Block(b) if matches!(b.kind, BlockKind::Hatched(_)) => Some(b.clone()),
            _ => None,
        })
        .expect("hatched block expected");
    assert_eq!(block.side, BlockSide::Above);
    assert!((block.height - 36.0).abs() < 0.01);
}

#[test]
fn unified_injects_removed_text_block() {
    let left = "a\nb\nc\n";
    let right = "x\nc\n";
    let hunks = diff_lines(left, right);
    let set = editor_diff::unified_decorations(&Rope::from_str(right), left, &hunks, 18.0, None);
    assert!(set.iter_all().any(|(_, d)| {
        matches!(d, Decoration::Block(b) if matches!(b.kind, BlockKind::Text { .. }))
    }));
}
