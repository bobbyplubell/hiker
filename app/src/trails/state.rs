//! Per-activity UI state for the Trails activity.
//!
//! The trail forest itself is NO LONGER stored here — trails are
//! markdown trail-docs on disk, read live each frame via
//! `core::trails::list` / `get_trail` (`feature-trails-migration`). This
//! struct holds only genuinely-transient sidebar UI state (which card is
//! expanded, which side trails are collapsed, whether the "All trails…"
//! picker window is open). The active trail is `vault.active_trail`
//! config, not a field here.
//!
//! Held on `AppState` as the top-level `trails_state` field, surfaced
//! through `SurfaceCtx.state` as `&mut dyn Any` downcastable to
//! `&mut State`.

use std::collections::HashSet;

/// Transient sidebar UI state for the trails activity. The trail data
/// lives on disk (read via core each frame); only ephemeral
/// presentation state lives here.
#[derive(Debug, Default)]
pub struct State {
    /// Vault-relative path of the single waypoint card currently rendered
    /// expanded. `expand_all` overrides this when set.
    pub expanded_path: Option<String>,
    /// "Expand all" toggle from the header chevron — when on, every
    /// waypoint card renders expanded regardless of `expanded_path`.
    pub expand_all: bool,
    /// Set of waypoint paths whose child side-trails are collapsed.
    /// Absence = expanded (the default for a freshly-loaded trail).
    pub side_trail_collapsed: HashSet<String>,
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
