use std::collections::HashSet;

use editor_core::decoration::Decoration;

use editor_core::state::Editor as EditorState;
use editor_md::folds::fold_decorations;
use editor_md::folds::fold_regions;
use editor_md::folds::FoldKind;
#[test]
fn heading_creates_fold_region() {
    let state = EditorState::new("# Title\n\nbody line 1\nbody line 2\n");
    let regions = fold_regions(&state);
    assert!(regions.iter().any(|r| matches!(r.kind, FoldKind::Heading(1))));
}

#[test]
fn fold_region_spans_until_next_heading_of_same_level() {
    let state = EditorState::new("# A\n\nA body\n\n## A.1\n\nnested\n\n# B\n\nB body\n");
    let regions = fold_regions(&state);
    let a_region = regions
        .iter()
        .find(|r| matches!(r.kind, FoldKind::Heading(1)))
        .expect("# A region");
    // A's body should NOT include "# B" or below — its body ends before B.
    let end_line = a_region.body_lines.end as usize;
    let line_at_end = state.doc.line_str(end_line.saturating_sub(1));
    assert!(
        !line_at_end.starts_with("# B"),
        "A's body should not extend into B (ended at line {end_line}: {line_at_end:?})"
    );
}

#[test]
fn fold_region_includes_lower_level_subheadings() {
    let state = EditorState::new("# A\n\n## A.1\n\nnested\n\n# B\nb\n");
    let regions = fold_regions(&state);
    let a = regions
        .iter()
        .find(|r| matches!(r.kind, FoldKind::Heading(1)))
        .expect("# A");
    let nested_line = 2; // ## A.1
    assert!(
        a.body_lines.contains(&(nested_line as u32)),
        "## A.1 should be inside # A's body"
    );
}

#[test]
fn collapsed_fold_emits_hide_decorations() {
    let state = EditorState::new("# T\nbody1\nbody2\n");
    let regions = fold_regions(&state);
    let id = regions[0].id;
    let mut folds = HashSet::new();
    folds.insert(id);
    let set = fold_decorations(&state, &folds);
    let hide_count = set
        .iter_all()
        .filter(|(_, d)| matches!(d, Decoration::Line(ls) if ls.hide))
        .count();
    assert!(hide_count >= 2, "body lines should be hidden when collapsed");
}

#[test]
fn expanded_fold_only_emits_chevron() {
    let state = EditorState::new("# T\nbody1\nbody2\n");
    let folds = HashSet::new();
    let set = fold_decorations(&state, &folds);
    let hide_count = set
        .iter_all()
        .filter(|(_, d)| matches!(d, Decoration::Line(ls) if ls.hide))
        .count();
    let chevron_count = set
        .iter_all()
        .filter(|(_, d)| matches!(d, Decoration::Line(ls) if ls.fold_chevron.is_some()))
        .count();
    assert_eq!(hide_count, 0, "no hide decorations when fold is expanded");
    assert!(chevron_count >= 1, "chevron should be shown on the heading line");
}

#[test]
fn fold_ids_are_stable_across_body_edits() {
    let a = EditorState::new("# Section\nbody1\nbody2\n");
    let b = EditorState::new("# Section\ndifferent body content\n");
    let id_a = fold_regions(&a)[0].id;
    let id_b = fold_regions(&b)[0].id;
    assert_eq!(id_a, id_b, "fold id must not change when body changes");
}
