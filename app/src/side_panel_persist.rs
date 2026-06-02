//! Per-vault persistence of the workbench chrome: the primary side-panel
//! accordion (which feature sections are open top-to-bottom, which are
//! collapsed, their height weights, the focused section, and whether the
//! bar is visible) plus the secondary side bar (Chat) and bottom status
//! bar visibility, so the user's show/hide choices survive a restart.
//!
//! Stored as `.hiker/side-panel.json`. Decoupled from the editor dock
//! layout (`layout.rs` / `.hiker/layout.json`) because the accordion
//! mutates entirely inside `egui_workbench` — the host can't observe its
//! edits to set a dirty flag — so we snapshot each autosave tick and
//! write only when the snapshot changes. [feature-multi-region-sidebar]

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::state::AppState;

/// Schema version. Bumped when the shape changes; unknown versions are
/// ignored on load (the bar falls back to its default single section).
///
/// v2: the activity `Mode` type changed from the `HikerMode` enum to the
/// feature-id `String`, so the serialized section keys are now lowercase
/// ids (`"files"`) instead of enum names (`"Files"`). A v1 file is
/// treated as a version mismatch and reset to the default layout — no
/// migration shim. [feature-consumer-activity-bar]
const VERSION: u32 = 2;

/// Serializable snapshot of the accordion arrangement. Sections are keyed
/// by feature id.
#[derive(Serialize, Deserialize, Clone, PartialEq)]
pub struct SidePanelState {
    pub version: u32,
    /// Open sections, top to bottom.
    pub sections: Vec<String>,
    /// Which sections are collapsed (header only).
    pub collapsed: Vec<String>,
    /// Per-section height weights, parallel-ish to `sections`.
    pub weights: Vec<(String, f32)>,
    /// The focused section (drives the activity-bar highlight).
    pub focused: Option<String>,
    /// Whether the primary side bar is visible.
    pub visible: bool,
    /// Whether the secondary side bar (Chat) is visible. Defaults to
    /// visible so pre-existing files (written before this field) keep the
    /// historical always-shown behaviour rather than hiding the bar.
    #[serde(default = "default_true")]
    pub secondary_visible: bool,
    /// Whether the bottom status bar is visible. Defaults to visible for
    /// the same backward-compat reason as `secondary_visible`.
    #[serde(default = "default_true")]
    pub status_bar_visible: bool,
}

fn default_true() -> bool {
    true
}

fn state_path(vault_root: &Path) -> PathBuf {
    vault_root.join(".hiker/side-panel.json")
}

impl SidePanelState {
    /// Snapshot the live workbench. Iterates `sections` (ordered) for the
    /// weights so the result is deterministic and comparable frame-to-frame.
    fn capture(app: &AppState) -> Self {
        let panels = &app.workbench.primary_panels;
        let sections = panels.open_modes().to_vec();
        let weights = sections
            .iter()
            .map(|m| (m.clone(), panels.section_weight(m)))
            .collect();
        // While reader view has temporarily hidden the chrome, the live
        // workbench flags are all false; persist the user's underlying
        // choices (snapshotted before reader view took over) so we don't
        // write — and later restore — the transient all-hidden state.
        let (primary, secondary, status) = if app.ui.reader_view_chrome_hidden {
            (
                app.ui.reader_view_prev_primary_visible,
                app.ui.reader_view_prev_secondary_visible,
                app.ui.reader_view_prev_status_visible,
            )
        } else {
            (
                app.workbench.primary_side_bar.visible,
                app.workbench.secondary_side_bar.visible,
                app.workbench.status_bar.visible,
            )
        };
        Self {
            version: VERSION,
            collapsed: panels.collapsed_modes(),
            weights,
            focused: panels.focused().cloned(),
            visible: primary,
            secondary_visible: secondary,
            status_bar_visible: status,
            sections,
        }
    }
}

/// Load the persisted arrangement and apply it to the workbench. No-op
/// (keeping the caller's default single section) when the file is
/// missing, unreadable, an unknown version, or has no sections.
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
    if saved.version != VERSION || saved.sections.is_empty() {
        return;
    }
    app.workbench.primary_panels.restore(
        saved.sections.clone(),
        saved.collapsed.clone(),
        saved.weights.clone(),
        saved.focused.clone(),
    );
    app.workbench.primary_side_bar.visible = saved.visible;
    app.workbench.secondary_side_bar.visible = saved.secondary_visible;
    app.workbench.status_bar.visible = saved.status_bar_visible;
    let focused = app.workbench.primary_panels.focused().cloned();
    app.workbench.activity_bar.set_active(focused);
    // Seed the dedup cache so the first autosave tick doesn't rewrite an
    // identical file.
    app.session.side_panel_saved = Some(SidePanelState::capture(app));
}

impl AppState {
    /// Persist the accordion arrangement if it changed since the last
    /// write. Cheap: a small JSON, gated on a value compare. Called from
    /// the autosave tick.
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
