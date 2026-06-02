//! The thin egui shell over the egui-free [`canvas_view_core`] view layer and
//! the [`hiker_canvas`] JSON Canvas core.
//!
//! Three layers, mirroring the editor (`editor-core` / `editor-view` /
//! `editor-egui`):
//!
//! - [`hiker_canvas`] — the egui-agnostic core: serde model, geometry, and the
//!   pure [`hiker_canvas::ops::EditOp`] verbs.
//! - [`canvas_view_core`] — the egui-free view + interaction layer: pan/zoom
//!   camera, edge routing, handle geometry, pointer hit-testing + gesture
//!   decisions, selection, and the undo stack (depends only on `emath`).
//! - this crate — the egui shell: the [`paint`]er, the [`widget::CanvasView`]
//!   `show` loop, the pointer event plumbing, and the [`content`] seam. It never
//!   depends on a content engine (`editor-egui`, `hiker-htmlview`): node content
//!   is painted by a host-supplied [`content::NodeContentRenderer`], so the
//!   engine for any node kind is an app-side change behind one trait.
//!
//! # Host contract
//!
//! The host (the app's canvas panel) owns the clip rect, owns the `Canvas`
//! document, and persists edits. Each frame it calls [`widget::CanvasView::show`]
//! with the document and a `NodeContentRenderer`. The returned
//! [`widget::CanvasResponse`] carries the [`hiker_canvas::ops::EditOp`]s committed
//! that frame; the view has already applied them to the live `Canvas` for
//! responsiveness, so the host's job is to persist them (diff the re-serialized
//! canvas onto the op-log `working` layer per `canvas.md`). Undo/redo
//! (Ctrl/Cmd-Z, -Shift-Z) likewise produce ops that are applied and reported.
//! Camera and selection are pure view state keyed off stable node/edge ids, so a
//! remote edit never disturbs them.
//! status: canvas-crate-split

pub mod content;
pub mod paint;
pub mod palette;
pub mod widget;
