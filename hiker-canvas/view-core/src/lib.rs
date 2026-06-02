//! The egui-free view + interaction layer over the [`hiker_canvas`] JSON Canvas
//! core — the analog of the editor's `editor-view` over `editor-core`.
//!
//! This crate holds everything the spatial editor needs that is *not* egui
//! rendering or egui event plumbing: the pan/zoom [`camera`], cubic-Bézier
//! [`edges`] routing, resize/connector [`handles`] geometry, pointer
//! hit-testing and gesture→[`hiker_canvas::ops::EditOp`] decisions in
//! [`interaction`], and the selection / in-progress gesture / undo-stack view
//! [`state`]. It depends only on `emath` (egui's math types — `Pos2` / `Rect` /
//! `Vec2`, no rendering) and `hiker-canvas`, so it is unit-testable without a
//! UI and reusable by a non-egui frontend.
//!
//! The thin egui shell (`canvas-view`) builds the painter + widget on top.
//! status: canvas-crate-split

pub mod camera;
pub mod edges;
pub mod handles;
pub mod interaction;
pub mod state;
