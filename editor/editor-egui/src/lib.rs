//! egui rendering backend for the editor: a drop-in [`widget::Widget`] that
//! paints an `editor-core` document plus its `editor-view` viewport, driving
//! input, scrolling, and per-frame layout. Companion modules render the
//! minimap, completion popup, hover tooltip, and floating panels, and translate
//! egui events into the view layer's input model.

pub mod completion;
pub mod minimap;
pub mod panel;
pub mod tooltip;
pub mod translate;
pub mod widget;

