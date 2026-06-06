//! Context activity — a multi-view container surfacing the per-note
//! discovery panels as stacked accordion sections under one activity-bar
//! icon: **backlinks** (notes that wikilink here), **appears-in** (the
//! canvases / boards / trails / trees that reference this note), and
//! **related** (vector-similar notes). Its `views()` returns those three
//! `View`s (defined in `crate::backlinks` / `crate::appears_in` /
//! `crate::related`), so their wire `ViewId`s become `"context/backlinks"`
//! / `"context/appears-in"` / `"context/related"` via [`Activity::view_id`].
//! State stays on the per-view `AppState` slices (`backlinks_state` /
//! `appears_in_state` / `related_state`), reached through each view's
//! `state_key`. [feature-multi-region-sidebar]

use eframe::egui;

use crate::activity::{Activity, View};
use crate::appears_in::AppearsInSidebar;
use crate::backlinks::BacklinksSidebar;
use crate::related::RelatedSidebar;
use crate::icons;

/// Zero-sized container descriptor. Owns no state of its own; each of its
/// views downcasts the relevant `AppState` slice via `Ctx::state`.
pub struct Context;

/// The two view singletons this container exposes, in stacked order
/// (backlinks above related). Static so `views()` can borrow them for the
/// `&dyn View` references it returns.
static BACKLINKS_VIEW: BacklinksSidebar = BacklinksSidebar;
static APPEARS_IN_VIEW: AppearsInSidebar = AppearsInSidebar;
static RELATED_VIEW: RelatedSidebar = RelatedSidebar;

impl Activity for Context {
    fn id(&self) -> &'static str {
        "context"
    }
    fn label(&self) -> &'static str {
        "Context"
    }
    fn icon(&self) -> egui::Image<'static> {
        icons::ICONS.image(icons::Icon::Graph)
    }
    fn views(&self) -> Vec<&dyn View> {
        vec![&BACKLINKS_VIEW, &APPEARS_IN_VIEW, &RELATED_VIEW]
    }
}
