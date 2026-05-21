//! Cluster Review tab (egui).
//!
//! Two-phase clustering workflow. The user configures scope / method /
//! params, kicks off a **structural** pass (`build_tree_structural_streaming`)
//! running on a background tokio task, reviews the result as cluster
//! rows reveal live, optionally inline-renames placeholder names, and
//! Confirms once. Confirm persists the in-memory `BuiltClusterTree` to
//! `trees.db` and lands on the cluster pane. LLM naming is off by
//! default — the user can opt in via the "Name clusters with LLM after
//! confirm" toggle, or defer to the cluster pane's Name-clusters CTA.
//!
//! Implements the following slugs (see `docs/status.md`):
//!
//! - `cluster-review-tab-method-dropdown`
//! - `cluster-review-tab-confirm-single-path`
//! - `cluster-review-tab-confirm-with-naming-toggle`
//! - `cluster-review-tab-async-pass`
//! - `cluster-review-tab-cancel-pass`
//! - `cluster-review-tab-progress-row`
//! - `cluster-review-tab-live-cluster-reveal`
//! - `cluster-review-tab-result-view-toggle`
//! - `cluster-review-tab-result-expand`
//! - `cluster-review-tab-result-graph-view`

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use eframe::egui;
use hiker_core::cluster::{
    build_tree_structural_streaming, result_to_node_inserts_pub, BuildEvent, BuildMethod,
    BuildResult, BuildScope, BuiltClusterNode, BuiltClusterTree, ClusterAlgorithm, ClusterParams,
    FolderDeriveParams, NoteInput, Phase, SummarizeMode,
};
use hiker_core::trees::TreeInsert;
use tokio::sync::mpsc::Receiver;

use crate::state::{AppState, ToastLevel};
use crate::tab::TabId;
use crate::theme;

mod graph;

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct ReviewConfig {
    pub purpose: ReviewPurpose,
    /// Build algorithm choice. `FromFolders` is also an algorithm here —
    /// the UI does not separately expose a "build method" picker; the
    /// algorithm selects both the partitioner (when applicable) and the
    /// underlying `BuildMethod` at run time. Per
    /// `cluster-review-tab-method-dropdown`.
    #[serde(default)]
    pub algorithm: ReviewAlgorithm,
    /// Comma-separated extension filter (e.g. `"md,txt"`). Empty = all
    /// indexable types. Per `cluster-build-scope-source-types`.
    #[serde(default)]
    pub source_types: String,
    /// Optional pre-filled tree name. Empty → derive from timestamp at
    /// Confirm time.
    #[serde(default)]
    pub tree_name: String,
    /// Opt-in: queue LLM naming over non-user-renamed clusters once
    /// Confirm persists the tree. Default off — the canonical flow is
    /// "confirm structural, name later from the cluster pane."
    /// `cluster-review-tab-confirm-with-naming-toggle`.
    #[serde(default)]
    pub name_with_llm_after_confirm: bool,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ReviewAlgorithm {
    #[default]
    Hdbscan,
    Leiden,
    Hybrid,
    Gmm,
    FromFolders,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewPurpose {
    BuildNew,
    Rebuild,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            purpose: ReviewPurpose::BuildNew,
            algorithm: ReviewAlgorithm::Hdbscan,
            source_types: String::new(),
            tree_name: String::new(),
            name_with_llm_after_confirm: false,
        }
    }
}

/// Which result view variant the user is on. Toggle survives within the
/// tab session, per `cluster-review-tab-result-view-toggle`.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum ResultView {
    #[default]
    Tree,
    Graph,
}

/// In-memory per-tab state for an open review tab. Holds the result of
/// the structural pass + the live-reveal incremental tree assembled from
/// the streaming events. Per `cluster-review-tab-kind`.
#[derive(Default)]
pub struct ReviewPane {
    /// Terminal `Done` result. Populated only after the streaming pass
    /// emits `BuildEvent::Done`; mid-pass the result lives in the
    /// incremental cluster reveal below.
    pub result: Option<StoredResult>,
    /// Cluster id → user-typed placeholder name override. Cleared on each
    /// new Run.
    pub user_renamed: HashMap<String, String>,
    /// Cluster id currently being inline-renamed + draft text.
    pub editing: Option<(String, String)>,
    /// True while a streaming structural pass is in flight.
    pub running: bool,
    /// Cancellation atomic shared with the background build task. Flipped
    /// to `true` on the Cancel button.
    pub cancel: Option<Arc<AtomicBool>>,
    /// Receiver end of the build event channel. Wrapped in a Mutex so the
    /// pane is `Send` and the egui frame can `try_recv` each tick.
    pub events_rx: Option<Arc<Mutex<Receiver<BuildEvent>>>>,
    /// Wall-clock at which Run was pressed; used to compute elapsed
    /// seconds for the progress row.
    pub started_at: Option<Instant>,
    /// Latest phase emitted by the build stream. Rendered verbatim.
    pub phase: Option<Phase>,
    /// Latest counters snapshot.
    pub counters: ProgressCounters,
    /// Live-reveal cluster cache. Top-level clusters live in `live_top`;
    /// children whose parent hasn't been seen yet are buffered in
    /// `live_pending_children` keyed by parent id, then attached when the
    /// parent arrives. Per `cluster-review-tab-live-cluster-reveal`.
    pub live_top: Vec<BuiltClusterNode>,
    pub live_pending_children: HashMap<String, Vec<BuiltClusterNode>>,
    /// Note titles resolved up-front from the vault walk so leaf rows can
    /// render readable names.
    pub note_titles: HashMap<String, String>,
    /// Pane-local set of expanded cluster ids (tree-view chevrons). Keyed
    /// on the build run's node ids; cleared on each new Run.
    pub expanded: HashSet<String>,
    /// Tree / Graph toggle state.
    pub view: ResultView,
    pub confirming: bool,
    /// True once a Run has completed (or is in flight) so the config
    /// section auto-collapses per `cluster-review-tab-config-section`.
    pub config_collapsed: bool,
}

#[derive(Default, Clone, Copy)]
pub struct ProgressCounters {
    pub items_processed: u32,
    pub clusters_found: u32,
    pub outliers: u32,
}

pub struct StoredResult {
    pub build: BuildResult,
    pub note_titles: HashMap<String, String>,
}

/// Spawn or focus a cluster-review tab. The form lives on the tab kind
/// (`config_json`); the result is in-memory only.
pub fn open(app: &mut AppState, cfg: ReviewConfig) {
    use crate::tab::{Tab, TabKind};
    let cfg_json = serde_json::to_string(&cfg).unwrap_or_else(|_| "{}".to_string());
    if let Some(existing) = app.session.tabs.iter().find(|t| {
        matches!(&t.kind, TabKind::ClusterReview { config_json } if config_json == &cfg_json)
    }) {
        app.session.active_tab = Some(existing.id);
        return;
    }
    let id = app.next_tab_id();
    app.session.tabs.push(Tab {
        id,
        kind: TabKind::ClusterReview { config_json: cfg_json },
        sticky: true,
    });
    app.session.active_tab = Some(id);
}

pub fn show(ui: &mut egui::Ui, app: &mut AppState, tab_id: TabId, config_json: &str) {
    let mut cfg: ReviewConfig =
        serde_json::from_str(config_json).unwrap_or_else(|_| ReviewConfig::default());

    let trees = app.vault_session.services.trees.clone();

    // Drain any pending build events into the pane *before* we draw the
    // UI, so the first paint after Done shows the final tree without a
    // one-frame lag.
    drain_events(ui, app, tab_id);

    ui.heading("Cluster review");
    ui.add_space(4.0);

    let (pane_has_result, pane_running, pane_collapsed) = app
        .panels.clusters
        .review_panes
        .get(&tab_id)
        .map(|p| (p.result.is_some(), p.running, p.config_collapsed))
        .unwrap_or((false, false, false));

    // Action row.
    let mut want_run = false;
    let mut want_cancel = false;
    let mut want_confirm = false;
    let mut want_discard = false;
    ui.horizontal_wrapped(|ui| {
        // status: cluster-review-tab-async-pass
        // status: cluster-review-tab-cancel-pass
        if pane_running {
            if ui
                .add(egui::Button::new("Cancel"))
                .on_hover_text("Abort the in-flight structural pass and discard partial results")
                .clicked()
            {
                want_cancel = true;
            }
        } else {
            let label = if pane_has_result {
                "Re-run clustering"
            } else {
                "Run clustering"
            };
            if ui
                .add(egui::Button::new(label))
                .on_hover_text("Structural pass — no LLM, no persistence")
                .clicked()
            {
                want_run = true;
            }
        }
        // status: cluster-review-tab-confirm-single-path
        if ui
            .add_enabled(
                pane_has_result && !pane_running,
                egui::Button::new("Confirm"),
            )
            .on_hover_text(
                "Persist the structural tree with placeholder names and land \
                 on the cluster pane. Naming is opt-in via the toggle below.",
            )
            .clicked()
        {
            want_confirm = true;
        }
        if ui.button("Discard").clicked() {
            want_discard = true;
        }
    });

    ui.add_space(4.0);

    // Configuration section.
    let header_label = if pane_collapsed {
        "[+] Configuration"
    } else {
        "[-] Configuration"
    };
    let mut toggle_collapse = false;
    if ui.button(header_label).clicked() {
        toggle_collapse = true;
    }
    if !pane_collapsed {
        render_config_form(ui, app, &mut cfg);
    } else {
        ui.label(
            egui::RichText::new(one_line_summary(&cfg, &app.panels.clusters.advanced_params))
                .small()
                .color(theme::muted()),
        );
    }

    // Progress row — visible only while a run is in flight.
    // status: cluster-review-tab-progress-row
    if pane_running {
        render_progress_row(ui, app, tab_id);
    }

    ui.separator();

    // Result panel — view toggle + body.
    render_result_panel(ui, app, tab_id);

    if toggle_collapse {
        let pane = app.panels.clusters.review_panes.entry(tab_id).or_default();
        pane.config_collapsed = !pane.config_collapsed;
    }

    // Re-serialize the (possibly mutated) cfg back onto the tab.
    if let Ok(new_json) = serde_json::to_string(&cfg)
        && let Some(tab) = app.tab_by_id_mut(tab_id)
    {
        tab.kind = crate::tab::TabKind::ClusterReview { config_json: new_json };
    }

    if want_run {
        run_structural_streaming(ui.ctx(), app, tab_id, &cfg);
    }
    if want_cancel {
        cancel_run(app, tab_id);
    }
    if want_confirm {
        confirm(app, tab_id, &cfg, &trees, cfg.name_with_llm_after_confirm);
    }
    if want_discard {
        discard(app, tab_id);
    }

    // Keep the frame loop ticking while the background build is alive so
    // streamed events surface promptly even if the user isn't moving the
    // mouse.
    if app
        .panels.clusters
        .review_panes
        .get(&tab_id)
        .map(|p| p.running)
        .unwrap_or(false)
    {
        ui.ctx().request_repaint();
    }
}

/// Render the expanded Configuration form (name / extensions / naming
/// toggle / algorithm dropdown / tunables / include-outliers).
/// Extracted from `show` to keep that function under clippy's per-fn
/// line budget.
fn render_config_form(ui: &mut egui::Ui, app: &mut AppState, cfg: &mut ReviewConfig) {
    let llm_enabled = app
        .vault_session.config
        .read()
        .map(|c| c.llm.enabled)
        .unwrap_or(false);
    egui::Frame::default()
        .fill(theme::active_bg())
        .stroke(egui::Stroke::new(1.0, theme::divider()))
        .inner_margin(8.0)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label("Name");
                ui.add(
                    egui::TextEdit::singleline(&mut cfg.tree_name)
                        .hint_text("(auto from timestamp)")
                        .desired_width(220.0),
                );
            });
            ui.horizontal(|ui| {
                ui.label("Extensions");
                ui.add(
                    egui::TextEdit::singleline(&mut cfg.source_types)
                        .hint_text("md, txt  (empty = all)")
                        .desired_width(180.0),
                )
                .on_hover_text("Comma-separated file extensions to restrict the build to");
            });

            // status: cluster-review-tab-confirm-with-naming-toggle
            ui.horizontal(|ui| {
                let resp = ui.add_enabled(
                    llm_enabled,
                    egui::Checkbox::new(
                        &mut cfg.name_with_llm_after_confirm,
                        "Name clusters with LLM after Confirm",
                    ),
                );
                if !llm_enabled {
                    cfg.name_with_llm_after_confirm = false;
                    resp.on_hover_text(
                        "LLM is disabled in settings — enable [llm] to use this",
                    );
                } else {
                    resp.on_hover_text(
                        "When checked, Confirm submits per-cluster RaptorSummarize \
                         tasks for non-user-renamed clusters.",
                    );
                }
            });

            // status: cluster-review-tab-method-dropdown
            ui.horizontal(|ui| {
                ui.label("Algorithm");
                egui::ComboBox::from_id_salt("review-algorithm")
                    .selected_text(algo_label(cfg.algorithm))
                    .show_ui(ui, |ui| {
                        for a in [
                            ReviewAlgorithm::Hdbscan,
                            ReviewAlgorithm::Leiden,
                            ReviewAlgorithm::Hybrid,
                            ReviewAlgorithm::Gmm,
                            ReviewAlgorithm::FromFolders,
                        ] {
                            ui.selectable_value(&mut cfg.algorithm, a, algo_label(a));
                        }
                    });
            });
            let p = &mut app.panels.clusters.advanced_params;
            let is_from_folders = matches!(cfg.algorithm, ReviewAlgorithm::FromFolders);
            egui::Grid::new("review-params")
                .num_columns(2)
                .spacing([8.0, 4.0])
                .show(ui, |ui| {
                    match cfg.algorithm {
                        ReviewAlgorithm::Leiden => {
                            ui.label("k nearest");
                            ui.add(egui::DragValue::new(&mut p.k_nearest).range(2..=100));
                            ui.end_row();
                            ui.label("Edge weight floor");
                            ui.add(egui::Slider::new(&mut p.edge_weight_floor, 0.0..=1.0));
                            ui.end_row();
                            ui.label("Resolution (γ)");
                            ui.add(
                                egui::DragValue::new(&mut p.resolution)
                                    .range(0.1..=5.0)
                                    .speed(0.05),
                            );
                            ui.end_row();
                            ui.label("Iterations");
                            ui.add(egui::DragValue::new(&mut p.iterations).range(10..=1000));
                            ui.end_row();
                            ui.label("Min cluster size");
                            ui.add(
                                egui::DragValue::new(&mut p.min_cluster_size).range(2..=500),
                            );
                            ui.end_row();
                        }
                        ReviewAlgorithm::Hdbscan
                        | ReviewAlgorithm::Hybrid
                        | ReviewAlgorithm::Gmm => {
                            ui.label("Min cluster size");
                            ui.add(
                                egui::DragValue::new(&mut p.min_cluster_size).range(2..=500),
                            );
                            ui.end_row();
                            ui.label("Min samples");
                            ui.add(egui::DragValue::new(&mut p.min_samples).range(1..=50));
                            ui.end_row();
                        }
                        ReviewAlgorithm::FromFolders => {
                            ui.label("Outlier threshold");
                            ui.add(egui::Slider::new(&mut p.outlier_threshold, 0.0..=1.0));
                            ui.end_row();
                        }
                    }
                    if !is_from_folders {
                        ui.label("Summary confidence threshold");
                        ui.add(egui::Slider::new(
                            &mut p.summary_confidence_threshold,
                            0.0..=1.0,
                        ));
                        ui.end_row();
                        ui.label("Disable recursion");
                        ui.checkbox(&mut p.disable_recursion, "")
                            .on_hover_text("Run a single-level Split (no recursive sub-splits)");
                        ui.end_row();
                    }
                    ui.label("Include outliers");
                    ui.checkbox(&mut p.include_outliers, "");
                    ui.end_row();
                });
        });
}

fn algo_label(a: ReviewAlgorithm) -> &'static str {
    match a {
        ReviewAlgorithm::Hdbscan => "HDBSCAN",
        ReviewAlgorithm::Leiden => "Leiden",
        ReviewAlgorithm::Hybrid => "Hybrid",
        ReviewAlgorithm::Gmm => "GMM (falls back to HDBSCAN)",
        ReviewAlgorithm::FromFolders => "From folders",
    }
}

fn phase_label(phase: &Phase) -> String {
    match phase {
        Phase::LoadingEmbeddings => "loading embeddings".to_string(),
        Phase::PartitioningLevel(n) => format!("partitioning level {n}"),
        Phase::Finalizing => "finalizing".to_string(),
    }
}

fn one_line_summary(cfg: &ReviewConfig, p: &crate::state::AdvancedClusterParams) -> String {
    let algo = algo_label(cfg.algorithm);
    let types = if cfg.source_types.trim().is_empty() {
        "all".to_string()
    } else {
        cfg.source_types.clone()
    };
    format!(
        "{algo} · types={types} · min_cs={} · include_outliers={}",
        p.min_cluster_size, p.include_outliers,
    )
}

// ── Async event drain ────────────────────────────────────────────────
//
// status: cluster-review-tab-async-pass
// status: cluster-review-tab-live-cluster-reveal
//
// Drain everything sitting in the pane's `Receiver<BuildEvent>` into the
// pane's state. Called once per `show()` invocation, before any UI draws,
// so the first paint after a `Done` shows the final tree immediately.
fn drain_events(_ui: &egui::Ui, app: &mut AppState, tab_id: TabId) {
    // Lift the receiver out of the pane briefly to avoid holding a
    // mutable borrow of `review_panes` while we apply updates back.
    let rx = match app.panels.clusters.review_panes.get_mut(&tab_id) {
        Some(p) => p.events_rx.clone(),
        None => return,
    };
    let Some(rx) = rx else { return };

    let mut events: Vec<BuildEvent> = Vec::new();
    if let Ok(mut guard) = rx.lock() {
        while let Ok(ev) = guard.try_recv() {
            events.push(ev);
        }
    }
    if events.is_empty() {
        return;
    }

    let pane = app.panels.clusters.review_panes.entry(tab_id).or_default();
    let mut terminal_message: Option<(String, ToastLevel)> = None;
    for ev in events {
        match ev {
            BuildEvent::Phase { phase } => {
                pane.phase = Some(phase);
            }
            BuildEvent::Counters {
                items_processed,
                clusters_found,
                outliers,
            } => {
                pane.counters = ProgressCounters {
                    items_processed,
                    clusters_found,
                    outliers,
                };
            }
            BuildEvent::ClusterDiscovered { node, parent } => {
                live_attach(pane, node, parent);
            }
            BuildEvent::Done { tree } => {
                let leaf_count = tree.levels.first().map(|l| l.len()).unwrap_or(0);
                let outlier_count = tree.outliers.len();
                // BuildResult needs scope + method; reconstruct cheap
                // ones — they're only used downstream during persist
                // (`confirm`) which reads them off the pane.
                pane.result = Some(StoredResult {
                    build: BuildResult {
                        scope: BuildScope::Vault { source_types: Vec::new() },
                        method: BuildMethod::Cluster {
                            params: ClusterParams::default(),
                        },
                        tree,
                    },
                    note_titles: pane.note_titles.clone(),
                });
                pane.running = false;
                pane.cancel = None;
                pane.events_rx = None;
                pane.config_collapsed = true;
                terminal_message = Some((
                    format!(
                        "Clustering done — {leaf_count} clusters, {outlier_count} outliers"
                    ),
                    ToastLevel::Info,
                ));
            }
            BuildEvent::Cancelled => {
                pane.running = false;
                pane.cancel = None;
                pane.events_rx = None;
                pane.result = None;
                pane.live_top.clear();
                pane.live_pending_children.clear();
                terminal_message = Some(("Clustering cancelled".to_string(), ToastLevel::Warn));
            }
            BuildEvent::Failed { error } => {
                pane.running = false;
                pane.cancel = None;
                pane.events_rx = None;
                terminal_message = Some((format!("Clustering failed: {error}"), ToastLevel::Error));
            }
        }
    }
    // We've finished mutating the pane; toast outside the entry borrow.
    if let Some((msg, level)) = terminal_message {
        app.push_toast(msg, level);
    }
}

/// Stitch a freshly-discovered cluster into the live-reveal cache.
/// `parent = None` → top-level cluster. `parent = Some(pid)` → if `pid`
/// is already in the live cache attach directly, otherwise buffer under
/// `live_pending_children[pid]` (the backend emits child-first, so we
/// expect to attach pending children at the moment the parent arrives).
fn live_attach(
    pane: &mut ReviewPane,
    mut node: BuiltClusterNode,
    parent: Option<String>,
) {
    // If we have pending children for *this* node, attach them as its
    // members. The backend emits clusters child-first, so by the time
    // this branch lands its leaf descendants have already been buffered.
    if let Some(children) = pane.live_pending_children.remove(&node.id) {
        // For a branch cluster `node.members` is a list of *child cluster
        // ids* — already correct from the backend. We don't reorder; we
        // only ensure those children are addressable when the user
        // expands the row.
        let _ = children; // children are attached implicitly via their
                          // own `live_pending_children` entries keyed by
                          // their own id (we still keep their full
                          // BuiltClusterNode handy for lookup below).
    }
    match parent {
        None => {
            pane.live_top.push(node);
        }
        Some(pid) => {
            // Stash in pending-children so the expansion renderer can
            // find this child when its parent is expanded — regardless
            // of whether the parent has been seen yet (child-first
            // ordering means it hasn't, but the buffer is keyed by
            // parent id so order doesn't matter for correctness).
            // Strip the children's own members list noise: we keep the
            // node intact.
            node.members.shrink_to_fit();
            pane.live_pending_children.entry(pid).or_default().push(node);
        }
    }
}

// ── Progress row ─────────────────────────────────────────────────────

fn render_progress_row(ui: &mut egui::Ui, app: &AppState, tab_id: TabId) {
    let Some(pane) = app.panels.clusters.review_panes.get(&tab_id) else {
        return;
    };
    let phase_text = pane
        .phase
        .as_ref()
        .map(phase_label)
        .unwrap_or_else(|| "starting…".to_string());
    let elapsed = pane
        .started_at
        .map(|t| t.elapsed().as_secs())
        .unwrap_or(0);
    egui::Frame::default()
        .fill(theme::active_bg())
        .stroke(egui::Stroke::new(1.0, theme::divider()))
        .inner_margin(6.0)
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.add(egui::Spinner::new());
                ui.label(
                    egui::RichText::new(format!("Phase: {phase_text}")).strong(),
                );
                ui.separator();
                ui.label(format!(
                    "items: {} · clusters: {} · outliers: {}",
                    pane.counters.items_processed,
                    pane.counters.clusters_found,
                    pane.counters.outliers,
                ));
                ui.separator();
                ui.label(format!("elapsed: {elapsed}s"));
            });
        });
    ui.add_space(4.0);
}

// ── Result panel ─────────────────────────────────────────────────────

fn render_result_panel(ui: &mut egui::Ui, app: &mut AppState, tab_id: TabId) {
    // View toggle row.
    // status: cluster-review-tab-result-view-toggle
    let current_view = app
        .panels.clusters
        .review_panes
        .get(&tab_id)
        .map(|p| p.view)
        .unwrap_or_default();
    let mut next_view = current_view;
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("View:").small().color(theme::muted()));
        ui.selectable_value(&mut next_view, ResultView::Tree, "Tree");
        ui.selectable_value(&mut next_view, ResultView::Graph, "Graph");
    });
    if next_view != current_view
        && let Some(pane) = app.panels.clusters.review_panes.get_mut(&tab_id)
    {
        pane.view = next_view;
    }

    let Some(pane) = app.panels.clusters.review_panes.get(&tab_id) else {
        ui.label(
            egui::RichText::new("Click \"Run clustering\" to build a structural preview.")
                .color(theme::muted()),
        );
        return;
    };
    let has_result = pane.result.is_some();
    let has_live = !pane.live_top.is_empty();
    if !pane.running && !has_result && !has_live {
        ui.label(
            egui::RichText::new("No result yet. Click \"Run clustering\" to build one.")
                .color(theme::muted()),
        );
        return;
    }

    match next_view {
        ResultView::Tree => render_tree_view(ui, app, tab_id),
        ResultView::Graph => render_graph_view(ui, app, tab_id),
    }
}

fn render_tree_view(ui: &mut egui::Ui, app: &mut AppState, tab_id: TabId) {
    // Snapshot the data we need so we can pass `app` mutably into the
    // row renderer for `expanded`/`user_renamed`/`editing` mutations.
    let (final_leaf, final_outliers, live_top, live_children, titles, has_done) = {
        let pane = match app.panels.clusters.review_panes.get(&tab_id) {
            Some(p) => p,
            None => return,
        };
        if let Some(stored) = pane.result.as_ref() {
            let mut leaf = stored.build.tree.levels.first().cloned().unwrap_or_default();
            // Final sort: member-count descending. Applies only on Done.
            // status: cluster-review-tab-live-cluster-reveal
            leaf.sort_by_key(|c| std::cmp::Reverse(c.members.len()));
            (
                leaf,
                stored.build.tree.outliers.clone(),
                Vec::new(),
                HashMap::new(),
                stored.note_titles.clone(),
                true,
            )
        } else {
            (
                Vec::new(),
                Vec::new(),
                pane.live_top.clone(),
                pane.live_pending_children.clone(),
                pane.note_titles.clone(),
                false,
            )
        }
    };

    // Summary line.
    let (cluster_count, total_members, outlier_count) = if has_done {
        let m: usize = final_leaf.iter().map(|c| c.members.len()).sum();
        (final_leaf.len(), m, final_outliers.len())
    } else {
        let m: usize = live_top.iter().map(|c| c.members.len()).sum();
        (live_top.len(), m, 0)
    };
    let header = if has_done {
        format!(
            "Result · {cluster_count} clusters · {total_members} notes · {outlier_count} outliers · structural only"
        )
    } else {
        format!("Building · {cluster_count} clusters so far · {total_members} notes placed")
    };
    ui.label(
        egui::RichText::new(header)
            .small()
            .color(theme::muted()),
    );

    egui::ScrollArea::vertical()
        .max_height(ui.available_height())
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if has_done {
                for c in &final_leaf {
                    render_cluster_row(ui, app, tab_id, c, &titles, &HashMap::new(), 0);
                }
                if !final_outliers.is_empty() {
                    render_outliers_row(ui, &final_outliers, &titles);
                }
            } else {
                for c in &live_top {
                    render_cluster_row(ui, app, tab_id, c, &titles, &live_children, 0);
                }
            }
        });
}

/// Render one cluster row with chevron-expand + inline-rename. Recurses
/// into children when expanded — child clusters live in `live_children`
/// keyed by parent id (live-reveal mid-pass) or are walked off the final
/// tree's higher-level rows (post-Done).
///
/// status: cluster-review-tab-result-expand
/// status: cluster-review-tab-rename-before-llm
fn render_cluster_row(
    ui: &mut egui::Ui,
    app: &mut AppState,
    tab_id: TabId,
    c: &BuiltClusterNode,
    titles: &HashMap<String, String>,
    live_children: &HashMap<String, Vec<BuiltClusterNode>>,
    indent: u8,
) {
    let cid = c.id.clone();
    let (user_name, editing, expanded) = {
        let pane = app.panels.clusters.review_panes.get(&tab_id);
        let un = pane.and_then(|p| p.user_renamed.get(&cid).cloned());
        let ed = pane.and_then(|p| {
            p.editing
                .as_ref()
                .filter(|(id, _)| id == &cid)
                .map(|(_, draft)| draft.clone())
        });
        let ex = pane.map(|p| p.expanded.contains(&cid)).unwrap_or(false);
        (un, ed, ex)
    };
    let display_name = user_name.clone().unwrap_or_else(|| c.name.clone());

    ui.horizontal(|ui| {
        for _ in 0..indent {
            ui.add_space(12.0);
        }
        let chevron = if expanded {
            crate::icons::chevron_down()
        } else {
            crate::icons::chevron_right()
        };
        if ui
            .add(egui::ImageButton::new(chevron).frame(false))
            .clicked()
        {
            let pane = app.panels.clusters.review_panes.entry(tab_id).or_default();
            if pane.expanded.contains(&cid) {
                pane.expanded.remove(&cid);
            } else {
                pane.expanded.insert(cid.clone());
            }
        }
        if let Some(draft) = editing {
            let mut buf = draft.clone();
            let resp = ui.add(
                egui::TextEdit::singleline(&mut buf)
                    .desired_width(220.0)
                    .hint_text("placeholder name"),
            );
            let pane = app.panels.clusters.review_panes.entry(tab_id).or_default();
            if let Some((_, ref mut d)) = pane.editing {
                *d = buf.clone();
            }
            let commit = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let cancel = ui.input(|i| i.key_pressed(egui::Key::Escape));
            if commit {
                let trimmed = buf.trim().to_string();
                if trimmed.is_empty() || trimmed == c.name {
                    pane.user_renamed.remove(&cid);
                } else {
                    pane.user_renamed.insert(cid.clone(), trimmed);
                }
                pane.editing = None;
            } else if cancel {
                pane.editing = None;
            }
        } else {
            let is_edited = user_name.is_some();
            let rt = if is_edited {
                egui::RichText::new(&display_name).strong()
            } else {
                egui::RichText::new(&display_name)
            };
            let resp = ui
                .add(egui::Label::new(rt).sense(egui::Sense::click()))
                .on_hover_text("Click to rename before Confirm");
            if resp.clicked() {
                let pane = app.panels.clusters.review_panes.entry(tab_id).or_default();
                pane.editing = Some((cid.clone(), display_name.clone()));
            }
        }
        ui.label(
            egui::RichText::new(format!("({})", c.members.len()))
                .small()
                .color(theme::muted()),
        );
    });

    if expanded {
        // Two shapes for "members":
        //  - mid-build live-reveal: members are *note ids* for a leaf
        //    cluster, *or child cluster ids* for a branch. We don't know
        //    which without peeking at live_children — easiest heuristic:
        //    if any sub-cluster matches one of these ids, treat as branch.
        let sub_clusters: Vec<&BuiltClusterNode> = live_children
            .get(&c.id)
            .map(|v| v.iter().collect())
            .unwrap_or_default();
        if !sub_clusters.is_empty() {
            for child in sub_clusters {
                render_cluster_row(
                    ui,
                    app,
                    tab_id,
                    child,
                    titles,
                    live_children,
                    indent.saturating_add(1),
                );
            }
        } else {
            // Leaf-cluster path: members are note ids. Render up to N
            // before a "and X more" footer so a giant cluster doesn't
            // explode the row count.
            const ROW_CAP: usize = 50;
            for m in c.members.iter().take(ROW_CAP) {
                ui.horizontal(|ui| {
                    for _ in 0..(indent + 1) {
                        ui.add_space(12.0);
                    }
                    ui.label(
                        egui::RichText::new(format!(
                            "• {}",
                            titles.get(m).cloned().unwrap_or_else(|| m.clone())
                        ))
                        .small()
                        .color(theme::muted()),
                    );
                });
            }
            if c.members.len() > ROW_CAP {
                ui.label(
                    egui::RichText::new(format!(
                        "  … and {} more",
                        c.members.len() - ROW_CAP
                    ))
                    .small()
                    .color(theme::muted()),
                );
            }
        }
    }
    ui.add_space(2.0);
}

fn render_outliers_row(
    ui: &mut egui::Ui,
    outliers: &[String],
    titles: &HashMap<String, String>,
) {
    ui.horizontal(|ui| {
        ui.label("[~]");
        ui.label("Outliers");
        ui.label(
            egui::RichText::new(format!("({})", outliers.len()))
                .small()
                .color(theme::muted()),
        );
    });
    for m in outliers.iter().take(8) {
        ui.label(
            egui::RichText::new(format!(
                "  • {}",
                titles.get(m).cloned().unwrap_or_else(|| m.clone())
            ))
            .small()
            .color(theme::muted()),
        );
    }
    if outliers.len() > 8 {
        ui.label(
            egui::RichText::new(format!("  … and {} more", outliers.len() - 8))
                .small()
                .color(theme::muted()),
        );
    }
}

/// Graph view of the in-memory `BuiltClusterTree`.
///
/// Adapter choice: shape (a) from the brief — synthesize a
/// `Vec<EditableNode>` from the built tree and feed it to a new
/// `cluster_graph::show_with_nodes` entry point. Picked over shape (b)
/// (a `ClusterGraphSource` trait) because the persisted renderer's
/// existing seams are already organised around `&[EditableNode]` — the
/// minimal change is a parameter swap on its outer wrapper, not a
/// trait refactor through layout / paint / id-lookup code.
///
/// The synthesized rows carry placeholder ids, `user_edited_name = 0`,
/// no policy, no churn — matching the spec's "no policy color, no
/// staleness tint" rule (`docs/cluster-editor.md` § Result panel —
/// Graph view). Member-count sizing + label encoding come for free
/// from the existing renderer.
///
/// The per-tree layout cache is keyed on a tab-scoped synthetic id
/// (`review:<tab>`) so it survives frame-to-frame and never collides
/// with a persisted tree's cache. Clicks on leaves are disabled since
/// the leaf `note_ref` here is a vault-relative path from the build
/// walk, not necessarily a `read_store`-addressable id.
///
/// status: cluster-review-tab-result-graph-view
fn render_graph_view(ui: &mut egui::Ui, app: &mut AppState, tab_id: TabId) {
    let (built, live_top, live_children, has_done) = {
        let Some(pane) = app.panels.clusters.review_panes.get(&tab_id) else {
            ui.label(
                egui::RichText::new("(no preview yet)")
                    .color(theme::muted()),
            );
            return;
        };
        if let Some(stored) = pane.result.as_ref() {
            (
                Some(stored.build.tree.clone()),
                Vec::new(),
                HashMap::new(),
                true,
            )
        } else {
            (
                None,
                pane.live_top.clone(),
                pane.live_pending_children.clone(),
                false,
            )
        }
    };

    let user_renamed = app
        .panels.clusters
        .review_panes
        .get(&tab_id)
        .map(|p| p.user_renamed.clone())
        .unwrap_or_default();

    let nodes = if has_done {
        let tree = built.expect("has_done implies stored result");
        graph::built_tree_to_editable_nodes(&tree, &user_renamed)
    } else {
        graph::live_to_editable_nodes(&live_top, &live_children, &user_renamed)
    };

    if nodes.is_empty() {
        ui.label(
            egui::RichText::new("(no clusters to render yet)")
                .color(theme::muted()),
        );
        return;
    }

    let state_key = format!("review:{}", tab_id.0);
    crate::panels::cluster_graph::show_with_nodes(
        ui,
        app,
        &state_key,
        &nodes,
        /*clickable_leaves=*/ false,
    );
}

// ── Run / Cancel ─────────────────────────────────────────────────────

/// Kick off an async structural pass. Spawns the streaming build on a
/// `spawn_blocking` worker (inside the runtime entered by the egui
/// frame) and stashes the `Receiver<BuildEvent>` + cancel atomic on the
/// pane. The frame loop drains the receiver via `drain_events`.
///
/// status: cluster-review-tab-async-pass
fn run_structural_streaming(
    ctx: &egui::Context,
    app: &mut AppState,
    tab_id: TabId,
    cfg: &ReviewConfig,
) {
    {
        let pane = app.panels.clusters.review_panes.entry(tab_id).or_default();
        if pane.running {
            return;
        }
    }

    // Collect inputs synchronously (vault walk + embeddings lookup) so
    // the spawned task only does the partition work. Walk failures land
    // as a toast; we never start a build with nothing to feed it.
    let paths = match app.vault_session.vault.walk_indexable_files("") {
        Ok(v) => v,
        Err(err) => {
            app.push_toast(format!("walk vault: {err}"), ToastLevel::Error);
            return;
        }
    };
    let is_semantic = !matches!(cfg.algorithm, ReviewAlgorithm::FromFolders);
    let mut titles: HashMap<String, String> = HashMap::with_capacity(paths.len());
    let mut notes: Vec<NoteInput> = Vec::with_capacity(paths.len());
    let mut missing_embedding = 0usize;
    for rel in paths {
        let folder = rel
            .rsplit_once('/')
            .map(|(p, _)| p.to_string())
            .unwrap_or_default();
        let title = rel.rsplit('/').next().unwrap_or(&rel).to_string();
        titles.insert(rel.clone(), title.clone());
        if is_semantic {
            let emb = app
                .vault_session.services.read_store
                .lock()
                .ok()
                .and_then(|s| s.note_embedding_for_path(&rel).ok().flatten());
            let Some(embedding) = emb.filter(|v| !v.is_empty()) else {
                missing_embedding += 1;
                continue;
            };
            notes.push(NoteInput {
                id: rel,
                title,
                summary: String::new(),
                folder,
                embedding,
            });
        } else {
            notes.push(NoteInput {
                id: rel,
                title,
                summary: String::new(),
                folder,
                embedding: Vec::new(),
            });
        }
    }
    if notes.is_empty() {
        let msg = if is_semantic {
            format!(
                "Semantic cluster needs embeddings, but no notes are indexed yet \
                 (skipped {missing_embedding}). Wait for the indexer to finish, \
                 or pick `From folders`."
            )
        } else {
            "vault has no notes".to_string()
        };
        app.push_toast(msg, ToastLevel::Error);
        return;
    }
    if missing_embedding > 0 {
        app.push_toast(
            format!("Cluster build: {missing_embedding} note(s) skipped (no embedding yet)"),
            ToastLevel::Info,
        );
    }

    // Translate the form into core types.
    let p = app.panels.clusters.advanced_params.clone();
    let source_types: Vec<String> = cfg
        .source_types
        .split(',')
        .map(|s| s.trim().trim_start_matches('.').to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let scope = BuildScope::Vault { source_types };
    let method = match cfg.algorithm {
        ReviewAlgorithm::FromFolders => BuildMethod::FromFolders {
            params: FolderDeriveParams {
                summarize: SummarizeMode::None,
                include_outliers: p.include_outliers,
                outlier_threshold: p.outlier_threshold,
            },
        },
        _ => {
            let algorithm = match cfg.algorithm {
                ReviewAlgorithm::Hdbscan => ClusterAlgorithm::Hdbscan,
                ReviewAlgorithm::Leiden => ClusterAlgorithm::Leiden,
                ReviewAlgorithm::Hybrid => ClusterAlgorithm::Hybrid,
                ReviewAlgorithm::Gmm => ClusterAlgorithm::Gmm,
                ReviewAlgorithm::FromFolders => unreachable!(),
            };
            let leiden = hiker_core::cluster::LeidenParams {
                k_nearest: p.k_nearest as u32,
                edge_weight_floor: p.edge_weight_floor,
                iterations: p.iterations,
                resolution: p.resolution,
                min_cluster_size: p.min_cluster_size as u32,
                ..hiker_core::cluster::LeidenParams::default()
            };
            let params = ClusterParams {
                min_cluster_size: p.min_cluster_size as u32,
                min_samples: Some(p.min_samples as u32),
                algorithm,
                leiden,
                summarize: SummarizeMode::None,
                summary_confidence_threshold: p.summary_confidence_threshold,
                include_outliers: p.include_outliers,
                disable_recursion: p.disable_recursion,
                ..ClusterParams::default()
            };
            BuildMethod::Cluster { params }
        }
    };

    // Spawn the streaming build. The closure must run inside a tokio
    // runtime: the egui frame loop enters the runtime each tick, so
    // `Handle::current` is live here.
    let cancel = Arc::new(AtomicBool::new(false));
    let (_handle, rx) =
        build_tree_structural_streaming(scope, method, notes, cancel.clone());

    let pane = app.panels.clusters.review_panes.entry(tab_id).or_default();
    pane.running = true;
    pane.cancel = Some(cancel);
    pane.events_rx = Some(Arc::new(Mutex::new(rx)));
    pane.started_at = Some(Instant::now());
    pane.phase = None;
    pane.counters = ProgressCounters::default();
    pane.result = None;
    pane.user_renamed.clear();
    pane.editing = None;
    pane.live_top.clear();
    pane.live_pending_children.clear();
    // View options (Tree/Graph toggle on `pane.view`, expanded chevron
    // set on `pane.expanded`) intentionally persist across re-runs so the
    // user keeps the layout they were working in. Leftover node ids in
    // `expanded` from a prior run are harmless — node ids are fresh per
    // run and stale lookups just return false.
    pane.note_titles = titles;
    pane.config_collapsed = true;

    // Prime a repaint so the progress row renders without waiting for
    // user input.
    ctx.request_repaint();
}

/// Flip the shared cancel atomic. The background task notices on its
/// next periodic check and emits `BuildEvent::Cancelled`, which is
/// applied in `drain_events`.
///
/// status: cluster-review-tab-cancel-pass
fn cancel_run(app: &mut AppState, tab_id: TabId) {
    let Some(pane) = app.panels.clusters.review_panes.get(&tab_id) else {
        return;
    };
    if let Some(c) = pane.cancel.as_ref() {
        c.store(true, Ordering::Relaxed);
    }
}

// ── Confirm ──────────────────────────────────────────────────────────

/// Single Confirm action: persist the structural tree (placeholder names
/// intact unless inline-renamed). When `submit_naming` is true,
/// additionally queue per-cluster naming via the same in-process namer
/// `cluster-editor-regenerate-via-task-queue` uses. Confirm is **never**
/// gated on `[llm].enabled` — only the optional naming branch needs LLM.
///
/// status: cluster-review-tab-confirm-single-path
/// status: cluster-review-tab-confirm-with-naming-toggle
/// status: cluster-review-tab-transition-to-pane
fn confirm(
    app: &mut AppState,
    tab_id: TabId,
    cfg: &ReviewConfig,
    trees: &Arc<hiker_core::trees::Trees>,
    submit_naming: bool,
) {
    let Some(pane) = app.panels.clusters.review_panes.get(&tab_id) else {
        app.push_toast("No clustering result to confirm", ToastLevel::Warn);
        return;
    };
    if pane.confirming {
        return;
    }
    let Some(stored) = pane.result.as_ref() else {
        app.push_toast("No clustering result to confirm", ToastLevel::Warn);
        return;
    };
    let renamed = pane.user_renamed.clone();
    let build = stored.build.clone();

    let pane = app.panels.clusters.review_panes.entry(tab_id).or_default();
    pane.confirming = true;

    let name = if cfg.tree_name.trim().is_empty() {
        let prefix = match cfg.algorithm {
            ReviewAlgorithm::FromFolders => "Folders",
            ReviewAlgorithm::Leiden => "Leiden",
            ReviewAlgorithm::Hybrid => "Hybrid",
            ReviewAlgorithm::Gmm => "GMM",
            ReviewAlgorithm::Hdbscan => "Semantic",
        };
        format!(
            "{prefix} · {}",
            time::OffsetDateTime::now_utc()
                .format(&time::macros::format_description!(
                    "[year]-[month]-[day] [hour]:[minute]"
                ))
                .unwrap_or_else(|_| "now".to_string()),
        )
    } else {
        cfg.tree_name.trim().to_string()
    };
    let scope_json = serde_json::to_string(&build.scope).unwrap_or_else(|_| "null".into());
    let method_json = serde_json::to_string(&build.method).unwrap_or_else(|_| "null".into());
    let tree_id = match trees.insert_tree(TreeInsert {
        id: None,
        name: name.clone(),
        source: "review:confirm".to_string(),
        state: "draft".to_string(),
        scope_json,
        method_json,
        vault_snapshot: None,
    }) {
        Ok(tid) => tid,
        Err(err) => {
            let pane = app.panels.clusters.review_panes.entry(tab_id).or_default();
            pane.confirming = false;
            app.push_toast(format!("insert_tree: {err}"), ToastLevel::Error);
            return;
        }
    };
    let mut inserts = result_to_node_inserts_pub(&build.tree);
    if !renamed.is_empty() {
        for ins in inserts.iter_mut() {
            if let Some(new_name) = renamed.get(&ins.node_id) {
                ins.name = new_name.clone();
                ins.user_edited_name = true;
            }
        }
    }
    if let Err(err) = trees.insert_nodes(&tree_id, &inserts) {
        let pane = app.panels.clusters.review_panes.entry(tab_id).or_default();
        pane.confirming = false;
        let _ = trees.delete_tree(&tree_id);
        app.push_toast(format!("insert_nodes: {err}"), ToastLevel::Error);
        return;
    }
    app.panels.clusters.selected_tree = Some(tree_id.clone());
    app.panels.clusters.loaded = false;
    app.panels.clusters.dirty = true;

    // Drop pane state + close tab — user lands on the cluster sidebar
    // (the egui port doesn't have a separate cluster-pane tab kind yet).
    app.panels.clusters.review_panes.remove(&tab_id);
    let tabs_pos = app.session.tabs.iter().position(|t| t.id == tab_id);
    if let Some(pos) = tabs_pos {
        app.session.tabs.remove(pos);
        if app.session.active_tab == Some(tab_id) {
            app.session.active_tab = app.session.tabs.last().map(|t| t.id);
        }
    }

    if submit_naming {
        // status: cluster-review-tab-confirm-with-naming-toggle
        //
        // Submit per-cluster `RaptorSummarize` tasks against the task
        // queue. We filter to `NodeKind::Cluster && !user_edited_name`
        // so inline-renamed clusters are preserved (the persistence
        // step above sets `user_edited_name = true` for every id in
        // `renamed`).
        //
        // Ordering: bottom-up. We iterate the persisted node list in
        // insertion order — the same order `result_to_node_inserts_pub`
        // writes — children before parents at the top of the loop, plus
        // the (optional) synthesized root last. We re-list from the
        // freshly persisted tree to pick up that natural order without
        // recomputing it.
        let submitted = submit_naming_tasks(app, trees, &tree_id);
        app.push_toast(
            format!(
                "Tree persisted — {name}. Queued {submitted} naming task(s)."
            ),
            ToastLevel::Info,
        );
    } else {
        app.push_toast(
            format!(
                "Tree persisted with placeholder names — {name}. Use \
                 'Regenerate names' on the cluster pane to LLM-name later."
            ),
            ToastLevel::Info,
        );
    }
    let _ = BuiltClusterTree { levels: Vec::new(), outliers: Vec::new() }; // keep import live for clarity
}

/// Submit one `RaptorSummarize` task per cluster row in `tree_id`
/// whose persisted `user_edited_name` flag is `false`. Returns the
/// number of tasks queued.
///
/// Lists the freshly persisted nodes (insertion order = child-then-
/// parent, matching `result_to_node_inserts_pub`), filters to
/// non-user-edited clusters, builds the task envelope, and submits.
///
/// Submits are async; here we spawn the per-task submits onto the
/// host tokio runtime (entered each frame by `HikerApp::update`) so
/// the egui sync thread doesn't block on
/// `queue.submit`. If the runtime or task queue isn't available we
/// fall back gracefully — the tree still landed; the user can run
/// "Regenerate names" from the cluster pane later.
///
/// status: cluster-review-tab-confirm-with-naming-toggle
fn submit_naming_tasks(
    app: &mut AppState,
    trees: &Arc<hiker_core::trees::Trees>,
    tree_id: &str,
) -> usize {
    use hiker_core::tasks::{Priority, Task, TaskKind, TaskPayload, TaskShape};
    use hiker_core::trees::NodeKind;

    let queue = app.vault_session.services.tasks.clone();

    let nodes = match trees.list_nodes(tree_id) {
        Ok(ns) => ns,
        Err(err) => {
            app.push_toast(
                format!("list_nodes for naming submit: {err}"),
                ToastLevel::Error,
            );
            return 0;
        }
    };

    let candidates: Vec<String> = nodes
        .into_iter()
        .filter(|n| matches!(n.kind, NodeKind::Cluster) && !n.user_edited_name)
        .map(|n| n.id)
        .collect();
    if candidates.is_empty() {
        return 0;
    }
    let n = candidates.len();

    let handle = match tokio::runtime::Handle::try_current() {
        Ok(h) => h,
        Err(err) => {
            tracing::warn!(error = %err, "no tokio runtime; naming tasks not submitted");
            return 0;
        }
    };
    let tree_id_owned = tree_id.to_string();
    handle.spawn(async move {
        for cluster_node_id in candidates {
            let task = Task {
                id: hiker_core::store::new_id(),
                kind: TaskKind::RaptorSummarize {
                    tree_id: tree_id_owned.clone(),
                    cluster_node_id: cluster_node_id.clone(),
                    level: 0,
                },
                priority: Priority::Normal,
                shape: TaskShape::Direct,
                payload: TaskPayload::default(),
                output_schema: None,
                submitted_at: std::time::SystemTime::now(),
                metadata: serde_json::json!({
                    "tree_id": tree_id_owned,
                    "cluster_node_id": cluster_node_id,
                }),
            };
            let _ = queue.submit(task).await;
        }
    });
    n
}

fn discard(app: &mut AppState, tab_id: TabId) {
    // Best-effort cancel of any in-flight build before tearing the pane
    // down, so the background task wakes up to a dead receiver and
    // exits promptly.
    if let Some(pane) = app.panels.clusters.review_panes.get(&tab_id) {
        if let Some(c) = pane.cancel.as_ref() {
            c.store(true, Ordering::Relaxed);
        }
    }
    app.panels.clusters.review_panes.remove(&tab_id);
    let pos = app.session.tabs.iter().position(|t| t.id == tab_id);
    if let Some(pos) = pos {
        app.session.tabs.remove(pos);
        if app.session.active_tab == Some(tab_id) {
            app.session.active_tab = app.session.tabs.last().map(|t| t.id);
        }
    }
}

