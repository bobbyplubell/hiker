//! Tests for the panel framework. SPEC §9.21.

use editor_view::panels::Panel;

use editor_view::panels::PanelKind;

use editor_view::panels::PanelPlacement;

use editor_view::panels::PanelStack;
#[test]
fn heights_sums_by_placement() {
    let mut stack = PanelStack::default();
    stack.panels.push(Panel {
        id: 1,
        placement: PanelPlacement::Top,
        height: 16.0,
        kind: PanelKind::Label("a".into()),
    });
    stack.panels.push(Panel {
        id: 2,
        placement: PanelPlacement::Top,
        height: 24.0,
        kind: PanelKind::Label("b".into()),
    });
    stack.panels.push(Panel {
        id: 3,
        placement: PanelPlacement::Bottom,
        height: 32.0,
        kind: PanelKind::Search,
    });

    let (top, bottom) = stack.heights();
    assert_eq!(top, 40.0);
    assert_eq!(bottom, 32.0);
}

#[test]
fn empty_stack_zero_heights() {
    let stack = PanelStack::default();
    assert_eq!(stack.heights(), (0.0, 0.0));
}
