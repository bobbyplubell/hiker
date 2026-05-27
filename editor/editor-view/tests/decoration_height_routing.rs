//! Regression for `bug-decoration-heightmap-footgun`: a height-affecting
//! decoration pushed via the plain `DecorationLayers::push` must land on a
//! height-tracked layer (so the heightmap driver, which scans only those
//! layers, sees it). A paint-only decoration pushed the same way must stay
//! off the height layers and keep the signature-mix path.

use editor_core::decoration::{Decoration, LineStyle, MarkStyle};
use editor_core::decoration::Set as DecorationSet;
use editor_core::rangeset::RangeSet;
use editor_view::viewport::DecorationLayers;

fn height_set() -> DecorationSet {
    RangeSet::from_iter([(
        0..1,
        Decoration::Line(LineStyle { height_scale: Some(2.0), ..Default::default() }),
    )])
}

fn hide_set() -> DecorationSet {
    RangeSet::from_iter([(0..1, Decoration::Line(LineStyle { hide: true, ..Default::default() }))])
}

fn paint_set() -> DecorationSet {
    RangeSet::from_iter([(0..1, Decoration::Mark(MarkStyle { bold: true, ..Default::default() }))])
}

#[test]
fn plain_push_routes_height_affecting_set_to_height_layer() {
    let mut layers = DecorationLayers::default();
    layers.push(height_set());

    assert_eq!(layers.layers.len(), 1);
    assert_eq!(
        layers.height_indices,
        vec![0],
        "a height-affecting set pushed via plain push() must be tracked as a height layer",
    );
    assert_ne!(layers.height_signature, 0, "height_signature must mix the height layer in");
    assert_eq!(
        layers.height_layers().count(),
        1,
        "the heightmap driver must see the height-affecting layer",
    );
}

#[test]
fn plain_push_keeps_paint_only_set_off_height_layers() {
    let mut layers = DecorationLayers::default();
    layers.push(paint_set());

    assert_eq!(layers.layers.len(), 1);
    assert!(
        layers.height_indices.is_empty(),
        "a paint-only set must not be scanned by the heightmap driver",
    );
    assert_eq!(
        layers.height_signature, 0,
        "paint-only push must not perturb the height signature",
    );
    assert_ne!(layers.signature, 0, "paint-only push still mixes the overall signature");
    assert_eq!(layers.height_layers().count(), 0);
}

#[test]
fn mixed_layer_stack_tracks_only_height_layers() {
    let mut layers = DecorationLayers::default();
    layers.push(paint_set()); // index 0 — paint only
    layers.push(hide_set()); // index 1 — height affecting
    layers.push(paint_set()); // index 2 — paint only
    layers.push(height_set()); // index 3 — height affecting

    assert_eq!(layers.layers.len(), 4);
    assert_eq!(
        layers.height_indices,
        vec![1, 3],
        "only the height-affecting layers are tracked, regardless of push order",
    );
    assert_eq!(layers.height_layers().count(), 2);
}

#[test]
fn clear_resets_height_tracking() {
    let mut layers = DecorationLayers::default();
    layers.push(height_set());
    layers.clear();

    assert!(layers.layers.is_empty());
    assert!(layers.height_indices.is_empty());
    assert_eq!(layers.signature, 0);
    assert_eq!(layers.height_signature, 0);
}
