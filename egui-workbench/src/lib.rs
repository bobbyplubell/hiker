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
//! use egui_workbench::{Workbench, DocumentTab};
//!
//! #[derive(Clone, serde::Serialize, serde::Deserialize)]
//! enum MyTab {
//!     File(String),
//!     Settings,
//! }
//!
//! impl DocumentTab for MyTab {
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
//! - [`status_bar`] — thin strip with appendable cells
//! - [`tab`] — `DocumentTab` trait, `TabState` (Regular/Preview/Pinned)
//! - [`behavior`] — `WorkbenchBehavior` trait for app integration
//! - [`workspace`] — `WorkbenchLayout` (serializable), versioned schema

#![doc(html_root_url = "https://docs.rs/egui_workbench/0.1.0")]
// Crate is in scaffolding stage; allow incomplete modules during build-out.
#![allow(dead_code)]

pub mod activity_bar;
pub mod behavior;
pub mod editor_area;
pub mod handle;
pub mod panel_area;
#[cfg(feature = "serde")]
pub mod persistence;
pub mod side_bar;
pub mod status_bar;
pub mod tab;
pub mod theme;
pub mod workspace;

pub(crate) mod internal;
pub(crate) mod icons;

pub use activity_bar::{ActivityBadge, ActivityBar, ActivityItem};
pub use behavior::WorkbenchBehavior;
pub use editor_area::EditorArea;
pub use handle::{GroupHandle, TabHandle};
pub use panel_area::PanelArea;
pub use side_bar::{SideBar, SideBarSide};
pub use status_bar::StatusBar;
pub use tab::{DocumentTab, TabState, TabUiContext};
pub use theme::{TabStyle, WorkbenchTheme};
pub use workspace::{GroupTarget, OpenTabOptions, SplitDir, Workbench};

#[cfg(feature = "serde")]
pub use persistence::{
    GridLayoutDto, LayoutError, LinearDirDto, PersistableTab, TabEntryDto, TileDto, TreeDto,
    WorkbenchLayout, migrate,
};
