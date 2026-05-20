//! egui backend for the editor widget.

pub mod completion;
pub mod minimap;
pub mod panel;
pub mod tooltip;
pub mod translate;
pub mod widget;

pub use completion::paint_completion_popup;
pub use minimap::{MinimapOptions, MinimapWidget};
pub use panel::paint_panels;
pub use tooltip::paint_tooltips;
pub use widget::{EditorWidget, PaintCache};
