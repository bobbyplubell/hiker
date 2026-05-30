//! # egui_workbench
//!
//! A configurable IDE-style workbench layout for egui. Provides the
//! "activity bar + side panels + tabbed editor groups + bottom panel
//! + status bar" pattern common to many IDE / editor / design apps.
//!
//! Built on [`egui_tiles`] for the underlying dockable tab/split tree.
//! `egui_workbench` adds the surrounding chrome (activity bar, status
//! bar) plus the conventions egui_tiles doesn't enforce (pinned/preview
//! tabs, editor groups distinct from bottom panels, layout
//! persistence, command palette hooks).
//!
//! See `SPEC.md` for user-facing requirements and `DESIGN.md` for the
//! technical architecture.
//!
//! ## Quick start
//!
//! ```ignore
//! use egui_workbench::{Workbench, Document};
//!
//! #[derive(Clone, serde::Serialize, serde::Deserialize)]
//! enum MyTab {
//!     File(String),
//!     Settings,
//! }
//!
//! impl Document for MyTab {
//!     fn title(&self) -> egui::WidgetText {
//!         match self {
//!             MyTab::File(p) => p.into(),
//!             MyTab::Settings => "Settings".into(),
//!         }
//!     }
//! }
//!
//! // In your eframe::App::update:
//! workbench.show(ctx, &mut my_behavior);
//! ```
//!
//! ## Modules
//!
//! - [`activity_bar`] — vertical icon strip on the side, mode switcher
//! - [`side_bar`] — swappable panel host driven by activity selection
//! - [`editor_area`] — tabbed editor groups with split support
//! - [`panel_area`] — bottom dockable area for tools (terminal-shaped)
//! - [`tab`] — `Document` trait, `State` (Regular/Preview/Pinned)
//! - [`behavior`] — `Host` trait for app integration
//! - [`workspace`] — `Workbench` coordinator, `TabId`/`GroupId` handles,
//!   `StatusBar`, and the serializable versioned layout schema

#![doc(html_root_url = "https://docs.rs/egui_workbench/0.1.0")]
// Crate is in scaffolding stage; allow incomplete modules during build-out.
#![allow(dead_code)]

pub mod activity_bar;
pub mod behavior;
pub mod editor_area;
pub mod feature;
pub mod panel_area;
#[cfg(feature = "serde")]
pub mod persistence;
pub mod side_bar;
pub mod side_panel_stack;
pub mod tab;
pub mod theme;
pub mod workspace;

pub(crate) mod internal;

