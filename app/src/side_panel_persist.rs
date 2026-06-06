//! Per-window persistence of the workbench chrome: the side-panel
//! placements across BOTH locations (the primary/left accordion and the
//! secondary/right accordion) — which views are open, in what order,
//! which are collapsed, their height weights — plus the focused section
//! per side and the three visibility flags (left bar, right bar, bottom
//! status bar), so the user's arrangement survives a restart.
//!
//! Stored as `.hiker/side-panel.json`. Decoupled from the editor dock
//! layout (`layout.rs` / `.hiker/layout.json`) because the accordion
//! mutates entirely inside `egui_workbench` — the host can't observe its
//! edits to set a dirty flag — so we snapshot each autosave tick and
//! write only when the snapshot changes. [feature-multi-region-sidebar]

use std::path::{Path, PathBuf};

use egui_workbench::side_bar::Location;
use serde::{Deserialize, Serialize};

use crate::activity::split_view_id;
use crate::state::AppState;

/// Schema version. Bumped when the shape changes; unknown versions are
/// ignored on load (the bootstrap default seed — Files/Context left,
/// Agent right — stands instead, with no migration shim).
///
/// v2: the activity `Mode` type changed from the `HikerMode` enum to the
/// activity-id `String` (section keys became lowercase ids).
///
/// v3: the schema became a flat list of per-view `PlacementEntry`s
/// spanning BOTH stacks (left + right), each carrying its `location`,
/// `group` (container id), `order`, `collapsed`, and `weight`, plus a
/// per-side focused view and the three visibility flags. A v2 (or v1)
/// file is a version mismatch and resets to the bootstrap default — no
/// migration shim, matching the v1→v2 approach. [feature-multi-region-sidebar]
const VERSION: u32 = 3;

/// One view's placement in the workbench chrome — the unit of the v3
/// persistence schema. `location` partitions views between the left and
/// right stacks; `group` keys the saved-group memory (the container id);
/// `order`/`collapsed`/`weight` reproduce each stack's accordion
/// arrangement. [feature-multi-region-sidebar]
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PlacementEntry {
    /// Wire `ViewId` (`"chat"`, `"context/backlinks"`, …).
    pub view_id: String,
    /// Which stack the view lives in.
    pub location: Location,
    /// The saved-group anchor this view belongs to (the container id).
    pub group: String,
    /// Position within its stack, top to bottom.
    pub order: u32,
    /// Whether the section is collapsed (header only).
    pub collapsed: bool,
    /// Relative height weight within its stack.
    pub weight: f32,
}

/// Serializable snapshot of the workbench chrome: every view's placement
/// across both stacks plus per-side focus and the visibility flags.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SidePanelState {
    pub version: u32,
    /// All open views, both locations, flat. Partitioned by `location`
    /// and sorted by `order` on restore.
    pub placements: Vec<PlacementEntry>,
    /// The focused view in the left (primary) stack.
    pub left_focused: Option<String>,
    /// The focused view in the right (secondary) stack.
    pub right_focused: Option<String>,
    /// Whether the primary (left) side bar is visible.
    pub left_visible: bool,
    /// Whether the secondary (right) side bar is visible.
    pub right_visible: bool,
    /// Whether the bottom status bar is visible.
    pub status_bar_visible: bool,
}

fn state_path(vault_root: &Path) -> PathBuf {
    vault_root.join(".hiker/side-panel.json")
}

/// Collect one stack's open views into `PlacementEntry`s at the given
/// location. Iterates `open_modes()` (ordered) so `order` and the result
/// are deterministic frame-to-frame.
fn capture_stack(
    panels: &egui_workbench::side_panel_stack::SidePanelStack<String>,
    location: Location,
) -> Vec<PlacementEntry> {
    let collapsed = panels.collapsed_modes();
    panels
        .open_modes()
        .iter()
        .enumerate()
        .map(|(i, mode)| PlacementEntry {
            view_id: mode.clone(),
            location,
            group: split_view_id(mode).0.to_string(),
            order: i as u32,
            collapsed: collapsed.contains(mode),
            weight: panels.section_weight(mode),
        })
        .collect()
}

impl SidePanelState {
    /// Snapshot the live workbench across both stacks. Deterministic
    /// (ordered) so the result is comparable frame-to-frame for the
    /// autosave dedup.
    fn capture(app: &AppState) -> Self {
        let mut placements = capture_stack(&app.workbench.primary_panels, Location::LeftBar);
        placements.extend(capture_stack(
            &app.workbench.secondary_panels,
            Location::RightBar,
        ));
        // Reader mode gates the chrome at render time only — it never
        // mutates the `visible` flags — so the live workbench state always
        // holds the user's true collapse choices and is safe to persist
        // even while reader mode is active. [view-reader-mode]
        Self {
            version: VERSION,
            placements,
            left_focused: app.workbench.primary_panels.focused().cloned(),
            right_focused: app.workbench.secondary_panels.focused().cloned(),
            left_visible: app.workbench.primary_side_bar.visible,
            right_visible: app.workbench.secondary_side_bar.visible,
            status_bar_visible: app.workbench.status_bar.visible,
        }
    }
}

/// Apply the placements for one `location` to `panels`: filter the flat
/// list, sort by `order`, then drive the stack's `restore`.
fn restore_stack(
    panels: &mut egui_workbench::side_panel_stack::SidePanelStack<String>,
    placements: &[PlacementEntry],
    location: Location,
    focused: Option<String>,
) {
    let mut entries: Vec<&PlacementEntry> =
        placements.iter().filter(|p| p.location == location).collect();
    entries.sort_by_key(|p| p.order);
    let sections: Vec<String> = entries.iter().map(|p| p.view_id.clone()).collect();
    let collapsed: Vec<String> = entries
        .iter()
        .filter(|p| p.collapsed)
        .map(|p| p.view_id.clone())
        .collect();
    let weights: Vec<(String, f32)> =
        entries.iter().map(|p| (p.view_id.clone(), p.weight)).collect();
    panels.restore(sections, collapsed, weights, focused);
}

/// Load the persisted arrangement and apply it to the workbench across
/// both stacks. No-op — leaving the bootstrap default seed (Files/Context
/// left, Agent right) in place — when the file is missing, unreadable, or
/// an unknown version. Reset-on-mismatch, no migration shim.
pub fn restore(app: &mut AppState, vault_root: &Path) {
    let Ok(bytes) = std::fs::read(state_path(vault_root)) else {
        return;
    };
    let saved: SidePanelState = match serde_json::from_slice(&bytes) {
        Ok(s) => s,
        Err(err) => {
            tracing::warn!(error = %err, "side-panel: parse failed; using default");
            return;
        }
    };
    if saved.version != VERSION {
        return;
    }
    restore_stack(
        &mut app.workbench.primary_panels,
        &saved.placements,
        Location::LeftBar,
        saved.left_focused.clone(),
    );
    restore_stack(
        &mut app.workbench.secondary_panels,
        &saved.placements,
        Location::RightBar,
        saved.right_focused.clone(),
    );
    app.workbench.primary_side_bar.visible = saved.left_visible;
    app.workbench.secondary_side_bar.visible = saved.right_visible;
    app.workbench.status_bar.visible = saved.status_bar_visible;
    let focused = app.workbench.primary_panels.focused().cloned();
    app.workbench.activity_bar.set_active(focused);
    // Seed the dedup cache so the first autosave tick doesn't rewrite an
    // identical file.
    app.session.side_panel_saved = Some(SidePanelState::capture(app));
}

impl AppState {
    /// Persist the chrome arrangement if it changed since the last write.
    /// Cheap: a small JSON, gated on a value compare. Called from the
    /// autosave tick.
    pub fn persist_side_panel(&mut self) {
        let current = SidePanelState::capture(self);
        if self.session.side_panel_saved.as_ref() == Some(&current) {
            return;
        }
        let path = state_path(&self.vault_session.vault_root);
        match serde_json::to_vec_pretty(&current) {
            Ok(bytes) => {
                if let Err(err) = std::fs::write(&path, bytes) {
                    tracing::debug!(error = %err, "side-panel persist failed");
                } else {
                    self.session.side_panel_saved = Some(current);
                }
            }
            Err(err) => tracing::debug!(error = %err, "side-panel serialize failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_workbench::side_panel_stack::SidePanelStack;

    /// A two-location arrangement (Files+Context left, Agent right, one
    /// collapsed section, custom weights) round-trips through
    /// serialize → deserialize → restore byte-for-byte at the
    /// placement/focus/visibility level.
    #[test]
    fn v3_round_trips_both_stacks() {
        // Build the "saved" state directly (avoids needing a full
        // AppState): Files+Context left (Context collapsed, custom
        // weights), Agent right.
        let saved = SidePanelState {
            version: VERSION,
            placements: vec![
                PlacementEntry {
                    view_id: "files".into(),
                    location: Location::LeftBar,
                    group: "files".into(),
                    order: 0,
                    collapsed: false,
                    weight: 2.5,
                },
                PlacementEntry {
                    view_id: "context/backlinks".into(),
                    location: Location::LeftBar,
                    group: "context".into(),
                    order: 1,
                    collapsed: true,
                    weight: 0.5,
                },
                PlacementEntry {
                    view_id: "chat".into(),
                    location: Location::RightBar,
                    group: "chat".into(),
                    order: 0,
                    collapsed: false,
                    weight: 1.0,
                },
            ],
            left_focused: Some("files".into()),
            right_focused: Some("chat".into()),
            left_visible: true,
            right_visible: false,
            status_bar_visible: true,
        };

        let bytes = serde_json::to_vec_pretty(&saved).unwrap();
        let back: SidePanelState = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(saved, back);

        // Drive the restore helpers against fresh stacks and confirm each
        // location reproduces its arrangement.
        let mut left = SidePanelStack::<String>::new();
        let mut right = SidePanelStack::<String>::new();
        restore_stack(&mut left, &back.placements, Location::LeftBar, back.left_focused.clone());
        restore_stack(&mut right, &back.placements, Location::RightBar, back.right_focused.clone());

        assert_eq!(left.open_modes(), &["files".to_string(), "context/backlinks".to_string()]);
        assert_eq!(left.collapsed_modes(), vec!["context/backlinks".to_string()]);
        assert_eq!(left.section_weight(&"files".to_string()), 2.5);
        assert_eq!(left.section_weight(&"context/backlinks".to_string()), 0.5);
        assert_eq!(left.focused(), Some(&"files".to_string()));

        assert_eq!(right.open_modes(), &["chat".to_string()]);
        assert_eq!(right.focused(), Some(&"chat".to_string()));
    }
}
