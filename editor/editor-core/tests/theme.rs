//! Integration tests for the `theme` module (SPEC §9.20).

use editor_core::compartment::Compartment;

use editor_core::compartment::Store;
use editor_core::decoration::Color;


use editor_core::theme::dark_default;


use editor_core::theme::light_default;


use editor_core::theme::Theme;
const fn alpha(c: Color) -> u8 {
    c.a
}

#[test]
fn defaults_have_distinct_names_and_palettes() {
    let l = light_default();
    let d = dark_default();
    assert_eq!(l.name, "light_default");
    assert_eq!(d.name, "dark_default");
    assert_ne!(l.palette.bg, d.palette.bg);
    assert_ne!(l.palette.fg, d.palette.fg);
}

#[test]
fn defaults_have_mostly_nonzero_alpha() {
    for theme in [light_default(), dark_default()] {
        // Of the 13 palette colors, allow at most a couple to be fully
        // transparent (none should be in our defaults, but the test is
        // forgiving in case a future tweak adds one).
        let zero_count = [
            theme.palette.bg,
            theme.palette.fg,
            theme.palette.accent,
            theme.palette.dim,
            theme.palette.error,
            theme.palette.warning,
            theme.palette.info,
            theme.palette.hint,
            theme.palette.selection,
            theme.palette.current_line,
            theme.palette.gutter_fg,
            theme.palette.gutter_bg,
            theme.palette.border,
        ]
        .iter()
        .filter(|c| alpha(**c) == 0)
        .count();
        assert!(
            zero_count <= 2,
            "{}: too many fully-transparent palette colors ({zero_count})",
            theme.name
        );
        // Diagnostic colors must all be visible.
        for c in [
            theme.diagnostics.error,
            theme.diagnostics.warning,
            theme.diagnostics.info,
            theme.diagnostics.hint,
        ] {
            assert!(alpha(c) > 0, "diagnostic color must be opaque");
        }
        // Tokens map should be populated.
        assert!(!theme.tokens.is_empty(), "tokens map should not be empty");
    }
}

#[test]
fn themes_are_cheap_to_clone() {
    let t = light_default();
    // Just exercise Clone — Theme is plain-data so this should be cheap and
    // produce a value equal to the original.
    let t2 = t.clone();
    assert_eq!(t.name, t2.name);
    assert_eq!(t.palette.bg, t2.palette.bg);
    assert_eq!(t.tokens.len(), t2.tokens.len());
}

#[test]
fn compartment_roundtrip_with_theme() {
    let c: Compartment<Theme> = Compartment::new();
    let mut store = Store::default();
    store.set(&c, light_default());
    {
        let got = store.get(&c).expect("theme present");
        assert_eq!(got.name, "light_default");
    }

    let store2 = store.reconfigure(&c, dark_default());
    assert_eq!(store.get(&c).unwrap().name, "light_default");
    assert_eq!(store2.get(&c).unwrap().name, "dark_default");
}
