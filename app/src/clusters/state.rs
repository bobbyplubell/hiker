//! Per-feature UI state for the Clusters feature.
//!
//! Migrated out of `state::PanelStates::clusters` as part of the
//! Feature registry Phase 1 work (`feature-state-ownership`,
//! `feature-cluster-migration`). Each feature now owns its own state
//! type; the registry shell never has to know what each feature stores.
//! Held on `AppState` as a top-level `clusters_state` field, surfaced
//! through `FeatureCtx::state` as `&mut dyn Any` downcastable to
//! `&mut State`.

use std::collections::{HashMap, HashSet};

use tokio::sync::oneshot;

use crate::tab::TabId;

/// Outcome posted by a background LLM-naming task. `(succeeded, failed)`.
pub type LlmJobOutcome = (usize, usize);

impl State {
    /// Mark the surface dirty so the next frame re-lists trees + nodes
    /// from disk. Called by every mutating cluster op.
    pub const fn mark_dirty(&mut self) {
        self.dirty = true;
    }
}

#[derive(Default)]
pub struct State {
    pub trees: Vec<hiker_core::trees::types::TreeRow>,
    pub selected_tree: Option<String>,
    pub nodes: Vec<hiker_core::trees::types::EditableNode>,
    pub expanded: HashSet<String>,
    pub renaming: Option<(String, String)>,
    pub editing_summary: Option<(String, String)>,
    pub editing_tag_policy: Option<(String, String, bool)>,
    pub editing_move_policy: Option<(String, String, bool)>,
    pub selected_nodes: HashSet<String>,
    pub editing_stage_move_target: Option<String>,
    pub editing_stage_tag_slug: Option<String>,
    pub redo_stacks: HashMap<String, Vec<hiker_core::trees::types::HistoryEntry>>,
    pub showing_advanced_params: bool,
    pub advanced_params: AdvancedClusterParams,
    pub dirty: bool,
    pub loaded: bool,
    pub review_panes: HashMap<TabId, crate::clusters::panel::ReviewPane>,
    /// True while a background LLM naming run (regenerate / summarize
    /// subset) is in flight. Gates the "Regenerate names" /
    /// "Summarize subset" buttons so the user can't double-fire.
    pub llm_job_in_flight: bool,
    /// Result channel for the in-flight naming task. The UI loop polls
    /// each frame; on completion we surface a toast and clear the gate.
    pub llm_job_rx: Option<oneshot::Receiver<LlmJobOutcome>>,
}

#[derive(Debug, Clone)]
pub struct AdvancedClusterParams {
    pub min_cluster_size: usize,
    pub min_samples: usize,
    pub k_nearest: usize,
    pub edge_weight_floor: f32,
    pub iterations: u32,
    pub resolution: f32,
    pub use_leiden: bool,
    pub outlier_threshold: f32,
    pub include_outliers: bool,
    pub summary_confidence_threshold: f32,
    pub disable_recursion: bool,
}

impl Default for AdvancedClusterParams {
    fn default() -> Self {
        Self {
            min_cluster_size: 5,
            min_samples: 2,
            k_nearest: 15,
            edge_weight_floor: 0.0,
            iterations: 100,
            resolution: 1.0,
            use_leiden: false,
            outlier_threshold: 0.5,
            include_outliers: true,
            summary_confidence_threshold: 0.5,
            disable_recursion: false,
        }
    }
}
