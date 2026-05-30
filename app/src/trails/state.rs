//! Per-feature UI state for the Trails feature.
//!
//! Migrated out of `state::PanelStates::trails_ui` as part of the
//! Feature registry Phase 2 work (`feature-state-ownership`,
//! `feature-trails-migration`). Each feature now owns its own state
//! type; the registry shell never has to know what each feature stores.
//! Held on `AppState` as a top-level `trails_state` field, surfaced
//! through `feature::Ctx::state` as `&mut dyn Any` downcastable to
//! `&mut State`.

use std::collections::HashSet;

/// Per-feature domain + UI state for the trails feature. Held on
/// `AppState` as the top-level `trails_state` field; the trails sidebar
/// surface downcasts the `feature::Ctx::state` opaque slot to
/// `&mut State`. Owns both the trail forest (the feature's core data,
/// persisted to `.hiker/trails.json`) and the sidebar's transient UI
/// state.
#[derive(Debug, Default)]
pub struct State {
    /// The trail forest — the feature's core data. Persisted to
    /// `<vault>/.hiker/trails.json`. Relocated off `Session::trails`
    /// during the trails feature migration.
    pub trails: Vec<crate::state::Trail>,
    /// Id of the trail that receives manual append-waypoint actions.
    /// `None` = no active trail; the Add-to-trail verbs hide/disable.
    pub active_trail: Option<String>,
    /// Inline-rename draft for the trails sidebar:
    /// `(trail_id, draft_name)`. `None` = no rename in progress.
    pub trail_rename: Option<(String, String)>,
    /// Path of the single waypoint card currently rendered in its
    /// expanded form (parent path + timestamp + annotation body).
    /// `expand_all` overrides this when set.
    pub expanded_path: Option<String>,
    /// "Expand all" toggle from the header chevron — when on, every
    /// waypoint card renders in its expanded form regardless of
    /// `expanded_path`.
    pub expand_all: bool,
    /// Set of waypoint paths whose child side-trails are collapsed.
    /// Absence = expanded (the default for a freshly-loaded trail).
    pub side_trail_collapsed: HashSet<String>,
    /// Active inline annotation editor: `(waypoint_path, draft_text)`.
    /// `None` = no editor open.
    pub annotation_edit: Option<(String, String)>,
    /// True while the "All trails..." flat picker window is showing.
    pub all_trails_picker_open: bool,
}

impl State {
    /// True if `path`'s card should render expanded right now —
    /// either `expand_all` is set, or `expanded_path` matches.
    pub fn is_expanded(&self, path: &str) -> bool {
        self.expand_all || self.expanded_path.as_deref() == Some(path)
    }

    /// Toggle the per-card expand state. `expand_all` overrides this
    /// in `is_expanded`, but flipping the per-card cursor still lets
    /// the user pick up where they left off when `expand_all` is off.
    pub fn toggle_expanded(&mut self, path: &str) {
        if self.expanded_path.as_deref() == Some(path) {
            self.expanded_path = None;
        } else {
            self.expanded_path = Some(path.to_string());
        }
    }
}
