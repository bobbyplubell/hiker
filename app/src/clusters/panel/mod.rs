//! Cluster Review tab (egui).
//!
//! Two-phase clustering workflow. The user configures scope / method /
//! params, kicks off a **structural** pass (`build_tree_structural_streaming`)
//! running on a background tokio task, reviews the result as cluster
//! rows reveal live, optionally inline-renames placeholder names, and
//! Confirms once. Confirm persists the in-memory `BuiltClusterTree` to
//! the tree's `.md` and lands on the cluster pane. LLM naming is off by
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
use std::time::{Duration, Instant};

use eframe::egui;
use hiker_core::cluster::build::stream::build_tree_structural_streaming;
use hiker_core::trees::build_adapter::node_inserts;
use hiker_core::cluster::algo::LeidenGraph;
use hiker_core::cluster::{
    BuildEvent, BuildMethod, BuildResult, BuildScope, BuiltClusterNode, BuiltClusterTree,
    Algorithm, Params, FolderDeriveParams, NoteInput, Phase, SummarizeMode,
};
use hiker_core::trees::types::TreeInsert;
use tokio::sync::mpsc::Receiver;

use crate::clusters::param_slider;
use crate::state::{AppState, ToastLevel};
use crate::tab::TabId;
use hiker_theme as theme;

mod result;

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

    // ── Live preview (cluster-review-tab-live-preview) ───────────────
    /// Resolved inputs (vault walk + per-note embedding load) cached so a
    /// config tweak re-clusters without re-querying SQLite for every
    /// note's embedding. Keyed by `cached_scope_sig`.
    pub cached_notes: Option<Arc<Vec<NoteInput>>>,
    pub cached_titles: Option<Arc<HashMap<String, String>>>,
    /// Signature of the scope (`source_types` + semantic-vs-folders) the
    /// cached notes were loaded for; a mismatch reloads and invalidates
    /// `cached_top_graph`.
    pub cached_scope_sig: Option<u64>,
    /// Top-level Leiden kNN graph from the last Leiden build, handed back
    /// to the next build so a γ / min-size tweak skips the O(n²) sweep.
    pub cached_top_graph: Option<Arc<LeidenGraph>>,
    /// Config signature of the last run; an auto-rerun fires only when the
    /// current config differs from this.
    pub last_run_sig: Option<u64>,
    /// Debounced pending auto-rerun: the config signature being waited on
    /// plus its fire deadline. Reset whenever the signature changes again.
    pub pending_rerun: Option<(u64, Instant)>,
    /// True while a *live* re-run is in flight. Unlike a manual run, a live
    /// rebuild keeps the old result on screen (swapped atomically on
    /// `Done`), leaves the config section open, and suppresses the progress
    /// row + the done-toast — so tuning updates in place instead of
    /// flashing the page. Per `cluster-review-tab-live-preview`.
    pub live_rebuild: bool,
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

impl ReviewConfig {
    /// Spawn or focus a cluster-review tab. The form lives on the tab kind
    /// (`config_json`); the result is in-memory only.
    pub fn open(&self, app: &mut AppState) {
        use crate::tab::{Tab, TabKind};
        let cfg_json = serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string());
        if let Some(existing) = app.session.tabs.iter().find(|t| {
            matches!(&t.kind, TabKind::ClusterReview { config_json } if config_json == &cfg_json)
        }) {
            app.session.active_tab = Some(existing.id);
            return;
        }
        let id = app.next_tab_id();
        app.session.tabs.push(Tab::new(
            id,
            TabKind::ClusterReview { config_json: cfg_json },
            true,
        ));
        app.session.active_tab = Some(id);
    }
}

pub fn show(ui: &mut egui::Ui, app: &mut AppState, tab_id: TabId, config_json: &str) {
    Review { app, tab_id }.show(ui, config_json);
}

/// Per-frame view context for a single open review tab. Bundles the
/// mutable `AppState` borrow with the tab id so the render/action
/// helpers can be `&mut self` methods on one receiver.
struct Review<'a> {
    app: &'a mut AppState,
    tab_id: TabId,
}

impl Review<'_> {
    fn show(&mut self, ui: &mut egui::Ui, config_json: &str) {
    let mut cfg: ReviewConfig =
        serde_json::from_str(config_json).unwrap_or_else(|_| ReviewConfig::default());

    let trees = self.app.vault_session.services.trees.clone();

    // Drain any pending build events into the pane *before* we draw the
    // UI, so the first paint after Done shows the final tree without a
    // one-frame lag.
    self.drain_events();

    ui.heading("Cluster review");
    ui.add_space(4.0);

    let tab_id = self.tab_id;
    let (pane_has_result, pane_running, pane_collapsed, pane_live_rebuild) = self.app
        .clusters_state
        .review_panes
        .get(&tab_id)
        .map(|p| (p.result.is_some(), p.running, p.config_collapsed, p.live_rebuild))
        .unwrap_or((false, false, false, false));

    // Action row.
    let mut want_run = false;
    let mut want_cancel = false;
    let mut want_confirm = false;
    let mut want_discard = false;
    // status: cluster-preset-save — set to the entered name when the user saves
    // the current params as a reusable preset.
    let mut want_save_preset: Option<String> = None;
    // A live rebuild runs in the background without taking over the
    // controls — only a manual run counts as "busy" for the action row.
    let busy = pane_running && !pane_live_rebuild;
    ui.horizontal_wrapped(|ui| {
        // status: cluster-review-tab-async-pass
        // status: cluster-review-tab-cancel-pass
        if busy {
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
                pane_has_result && !busy,
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
        // Save the current form params as a reusable preset (a `.md` note); the
        // Clusters `+` dropdown lists it. status: cluster-preset-save
        let save_resp = ui.button("Save preset");
        let draft_id = egui::Id::new(("cluster-preset-name", tab_id));
        egui::Popup::menu(&save_resp).show(|ui| {
            let mut name = ui.data_mut(|d| d.get_temp::<String>(draft_id).unwrap_or_default());
            ui.label("Preset name");
            let field = ui.text_edit_singleline(&mut name);
            field.request_focus();
            let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            ui.data_mut(|d| d.insert_temp(draft_id, name.clone()));
            if (ui.button("Save").clicked() || enter) && !name.trim().is_empty() {
                want_save_preset = Some(name.trim().to_string());
                ui.data_mut(|d| d.remove::<String>(draft_id));
                ui.close();
            }
        });
    });

    if let Some(name) = want_save_preset {
        let params = crate::clusters::preset::Params {
            name,
            algorithm: cfg.algorithm,
            source_types: cfg.source_types.clone(),
            name_with_llm_after_confirm: cfg.name_with_llm_after_confirm,
        };
        // Write the preset note + index it on the runtime (mirrors `new_board`);
        // the note carries `hiker.kind: cluster-preset` frontmatter and is the
        // source of truth. status: cluster-preset-save
        let watcher = self.app.vault_session.services.watcher.clone();
        let jobs = self.app.vault_session.services.indexer.job_sender();
        let vault = self.app.vault_session.vault.clone();
        let result = match tokio::runtime::Handle::try_current() {
            Ok(h) => h.block_on(async {
                crate::clusters::preset::save(&watcher, &jobs, &vault, &params).await
            }),
            Err(_) => Err(hiker_core::errors::HikerError::Io("no runtime".into())),
        };
        match result {
            Ok(_) => {
                // Invalidate the cache so the `+` dropdown reloads with the new
                // preset once it's indexed. status: cluster-preset
                self.app.clusters_state.preset_cache = None;
                self.app.push_toast(
                    format!("Saved preset \u{201c}{}\u{201d}", params.name),
                    crate::state::ToastLevel::Info,
                );
            }
            Err(e) => self.app.push_toast(
                format!("Couldn't save preset: {e}"),
                crate::state::ToastLevel::Warn,
            ),
        }
    }

    ui.add_space(4.0);

    // Configuration section. Expanded, the form sits in a fixed-width column
    // beside the result view (stacking it pushes the graph far down and reads
    // awkwardly, especially during live tuning); collapsed, the one-line
    // summary stacks above the full-width result.
    // status: cluster-review-tab-config-section
    let header_label = if pane_collapsed {
        "[+] Configuration"
    } else {
        "[-] Configuration"
    };
    let mut toggle_collapse = false;

    if pane_collapsed {
        if ui.button(header_label).clicked() {
            toggle_collapse = true;
        }
        let p = &self.app.clusters_state.advanced_params;
        let types = if cfg.source_types.trim().is_empty() {
            "all".to_string()
        } else {
            cfg.source_types.clone()
        };
        let summary = format!(
            "{} · types={types} · min_cs={} · include_outliers={}",
            algo_label(cfg.algorithm),
            p.min_cluster_size,
            p.include_outliers,
        );
        ui.label(egui::RichText::new(summary).small().color(theme::muted()));

        // Progress row — only for a manual run; a live rebuild updates in
        // place. status: cluster-review-tab-progress-row
        if pane_running && !pane_live_rebuild {
            self.render_progress_row(ui);
        }
        ui.separator();
        self.render_result_panel(ui);
    } else {
        ui.horizontal_top(|ui| {
            let avail_h = ui.available_height();
            let col_w = (ui.available_width() * 0.42).clamp(300.0, 440.0);
            // Left: config column (header + form), constrained width.
            ui.allocate_ui_with_layout(
                egui::vec2(col_w, avail_h),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.set_max_width(col_w);
                    if ui.button(header_label).clicked() {
                        toggle_collapse = true;
                    }
                    self.render_config_form(ui, &mut cfg);
                },
            );
            ui.separator();
            // Right: progress + result view, takes the remaining width.
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), avail_h),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    if pane_running && !pane_live_rebuild {
                        self.render_progress_row(ui);
                    }
                    self.render_result_panel(ui);
                },
            );
        });
    }

    if toggle_collapse {
        let pane = self.app.clusters_state.review_panes.entry(tab_id).or_default();
        pane.config_collapsed = !pane.config_collapsed;
    }

    // Re-serialize the (possibly mutated) cfg back onto the tab.
    if let Ok(new_json) = serde_json::to_string(&cfg)
        && let Some(tab) = self.app.tab_by_id_mut(tab_id)
    {
        tab.kind = crate::tab::TabKind::ClusterReview { config_json: new_json };
    }

    if want_run {
        self.run_structural_streaming(ui.ctx(), &cfg, /*live=*/ false);
    }
    if want_cancel {
        self.cancel_run();
    }
    if want_confirm {
        self.confirm(&cfg, &trees, cfg.name_with_llm_after_confirm);
    }
    if want_discard {
        self.discard();
    }

    // Live preview — debounced auto-rerun on config change (no-op while a
    // build is running or before the first run). status: cluster-review-tab-live-preview
    self.maybe_live_rerun(ui.ctx(), &cfg);

    // Keep the frame loop ticking while the background build is alive so
    // streamed events surface promptly even if the user isn't moving the
    // mouse.
    if self.app
        .clusters_state
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
fn render_config_form(&mut self, ui: &mut egui::Ui, cfg: &mut ReviewConfig) {
    // Note count drives the live-preview size gate (read before the
    // `advanced_params` borrow below). Zero until the first run loads it.
    let note_count = self
        .app
        .clusters_state
        .review_panes
        .get(&self.tab_id)
        .and_then(|pane| pane.cached_notes.as_ref())
        .map_or(0, |v| v.len());
    let live_gate = live_preview_max(cfg.algorithm);
    let app = &mut *self.app;
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
            let p = &mut app.clusters_state.advanced_params;
            let is_from_folders = matches!(cfg.algorithm, ReviewAlgorithm::FromFolders);
            egui::Grid::new("review-params")
                .num_columns(2)
                .spacing([8.0, 4.0])
                .show(ui, |ui| {
                    // Every numeric knob renders as an `egui::Slider` (see
                    // `param_slider`) so the form reads as one consistent
                    // control set instead of a mix of sliders and drag-boxes.
                    // Booleans stay checkboxes — they aren't numbers.
                    match cfg.algorithm {
                        ReviewAlgorithm::Leiden => {
                            param_slider(ui, "k nearest", &mut p.k_nearest, 2..=100, false,
                                "Neighbors per node in the kNN similarity graph");
                            param_slider(ui, "Edge weight floor", &mut p.edge_weight_floor, 0.0..=1.0, false,
                                "Drop kNN edges below this cosine similarity to sharpen community boundaries");
                            param_slider(ui, "Resolution (γ)", &mut p.resolution, 0.1..=5.0, false,
                                "Higher splits into more, smaller clusters; lower merges into fewer, larger ones");
                            param_slider(ui, "Iterations", &mut p.iterations, 10..=1000, true,
                                "Cap on Leiden refinement passes (it converges fast; this is a safety rail)");
                            param_slider(ui, "Min cluster size", &mut p.min_cluster_size, 2..=500, true,
                                "Communities smaller than this are flagged as outliers");
                        }
                        ReviewAlgorithm::Hdbscan
                        | ReviewAlgorithm::Hybrid
                        | ReviewAlgorithm::Gmm => {
                            param_slider(ui, "Min cluster size", &mut p.min_cluster_size, 2..=500, true,
                                "Smallest cluster the algorithm will form");
                            param_slider(ui, "Min samples", &mut p.min_samples, 1..=50, false,
                                "Higher is more conservative — more points fall out as outliers");
                        }
                        ReviewAlgorithm::FromFolders => {
                            param_slider(ui, "Outlier threshold", &mut p.outlier_threshold, 0.0..=1.0, false,
                                "Notes below this similarity to their folder centroid become outliers");
                        }
                    }
                    if !is_from_folders {
                        param_slider(ui, "Summary confidence threshold", &mut p.summary_confidence_threshold, 0.0..=1.0, false,
                            "Clusters below this confidence are flagged uncertain in the review surface");
                        ui.label("Disable recursion");
                        ui.checkbox(&mut p.disable_recursion, "")
                            .on_hover_text("Run a single-level Split (no recursive sub-splits)");
                        ui.end_row();
                    }
                    ui.label("Include outliers");
                    ui.checkbox(&mut p.include_outliers, "")
                        .on_hover_text("Keep unclustered notes in an outliers bucket instead of force-routing them into the nearest cluster");
                    ui.end_row();

                    // Live-preview size-gate notice. The toggle itself is a
                    // DISPLAY control and lives in the result graph's view/eye
                    // menu, not here — these config knobs hold only clustering
                    // ENGINE params. This row just explains the gate when live
                    // preview can't run for the current note count.
                    // status: cluster-review-tab-live-preview
                    let gated = note_count > live_gate;
                    if gated {
                        // Force live preview off above the gate so a stale
                        // `true` can't drive an auto-rerun over the limit.
                        p.live_preview = false;
                        ui.label("Live preview");
                        ui.label(
                            egui::RichText::new(format!(
                                "off — {note_count} notes over the {live_gate} live limit; use Run"
                            ))
                            .small()
                            .color(theme::muted()),
                        );
                        ui.end_row();
                    }
                });
        });
}
}

// ── Live preview (cluster-review-tab-live-preview) ───────────────────

/// Debounce window before an auto-rerun fires after the last config change.
const LIVE_DEBOUNCE: Duration = Duration::from_millis(250);

/// Per-algorithm note-count ceiling above which live preview auto-disables.
/// Tuned to the measured O(n²) structural-build cost: Leiden re-tunes γ over
/// a *cached* kNN graph (tens of ms even at 2k), so it tolerates a high cap;
/// HDBSCAN re-partitions from scratch on every change (≈0.5s at 600), so it
/// gets a tighter one; FromFolders is O(n) folder grouping — effectively
/// unbounded.
const fn live_preview_max(algo: ReviewAlgorithm) -> usize {
    match algo {
        ReviewAlgorithm::Leiden => 2000,
        ReviewAlgorithm::FromFolders => 100_000,
        ReviewAlgorithm::Hdbscan | ReviewAlgorithm::Hybrid | ReviewAlgorithm::Gmm => 600,
    }
}

/// Hash the scope-determining inputs (file extensions + semantic-vs-folders
/// mode). A change here means the *note set* differs, so cached notes +
/// graph must be reloaded.
fn scope_signature(source_types: &str, is_semantic: bool) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    source_types.trim().hash(&mut h);
    is_semantic.hash(&mut h);
    h.finish()
}

/// Hash every knob that affects the clustering *result* (algorithm + scope +
/// all tunables). A change here triggers a debounced auto-rerun under live
/// preview. Floats hash via `to_bits` so NaN-free param values compare
/// exactly.
fn config_signature(cfg: &ReviewConfig, p: &crate::clusters::state::AdvancedClusterParams) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (cfg.algorithm as u8).hash(&mut h);
    cfg.source_types.trim().hash(&mut h);
    p.min_cluster_size.hash(&mut h);
    p.min_samples.hash(&mut h);
    p.k_nearest.hash(&mut h);
    p.edge_weight_floor.to_bits().hash(&mut h);
    p.iterations.hash(&mut h);
    p.resolution.to_bits().hash(&mut h);
    p.outlier_threshold.to_bits().hash(&mut h);
    p.include_outliers.hash(&mut h);
    p.summary_confidence_threshold.to_bits().hash(&mut h);
    p.disable_recursion.hash(&mut h);
    h.finish()
}

const fn algo_label(a: ReviewAlgorithm) -> &'static str {
    match a {
        ReviewAlgorithm::Hdbscan => "HDBSCAN",
        ReviewAlgorithm::Leiden => "Leiden",
        ReviewAlgorithm::Hybrid => "Hybrid",
        ReviewAlgorithm::Gmm => "GMM (falls back to HDBSCAN)",
        ReviewAlgorithm::FromFolders => "From folders",
    }
}

// ── Async event drain ────────────────────────────────────────────────
//
// status: cluster-review-tab-async-pass
// status: cluster-review-tab-live-cluster-reveal
//
// Drain everything sitting in the pane's `Receiver<BuildEvent>` into the
// pane's state. Called once per `show()` invocation, before any UI draws,
// so the first paint after a `Done` shows the final tree immediately.
impl Review<'_> {
fn drain_events(&mut self) {
    let app = &mut *self.app;
    let tab_id = self.tab_id;
    // Lift the receiver out of the pane briefly to avoid holding a
    // mutable borrow of `review_panes` while we apply updates back.
    let rx = match app.clusters_state.review_panes.get_mut(&tab_id) {
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

    let pane = app.clusters_state.review_panes.entry(tab_id).or_default();
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
            BuildEvent::ClusterDiscovered { mut node, parent } => {
                // Stitch a freshly-discovered cluster into the live-reveal
                // cache. `parent = None` → top-level cluster; `Some(pid)` →
                // buffer under `live_pending_children[pid]` (the backend
                // emits child-first, so the parent hasn't been seen yet, but
                // the buffer is keyed by parent id so order doesn't matter).
                //
                // Any already-buffered children of *this* node are attached
                // implicitly via their own `live_pending_children` entries.
                pane.live_pending_children.remove(&node.id);
                match parent {
                    None => {
                        pane.live_top.push(node);
                    }
                    Some(pid) => {
                        node.members.shrink_to_fit();
                        pane.live_pending_children.entry(pid).or_default().push(node);
                    }
                }
            }
            BuildEvent::Done { tree, top_graph } => {
                // Cache the top-level Leiden graph for the next live-preview
                // run. Keep the prior graph if this build produced none
                // (HDBSCAN / FromFolders) so a Leiden→HDBSCAN→Leiden detour
                // doesn't force a needless rebuild.
                if top_graph.is_some() {
                    pane.cached_top_graph = top_graph;
                }
                let was_live = pane.live_rebuild;
                pane.live_rebuild = false;
                let leaf_count = tree.levels.first().map(std::vec::Vec::len).unwrap_or(0);
                let outlier_count = tree.outliers.len();
                // BuildResult needs scope + method; reconstruct cheap
                // ones — they're only used downstream during persist
                // (`confirm`) which reads them off the pane.
                pane.result = Some(StoredResult {
                    build: BuildResult {
                        scope: BuildScope::Vault { source_types: Vec::new() },
                        method: BuildMethod::Cluster {
                            params: Params::default(),
                        },
                        tree,
                    },
                    note_titles: pane.note_titles.clone(),
                });
                pane.running = false;
                pane.cancel = None;
                pane.events_rx = None;
                // A live rebuild leaves the config open and stays quiet —
                // collapsing + toasting on every debounced tweak is exactly
                // the page-flash the user is tuning to avoid.
                if !was_live {
                    pane.config_collapsed = true;
                    terminal_message = Some((
                        format!(
                            "Clustering done — {leaf_count} clusters, {outlier_count} outliers"
                        ),
                        ToastLevel::Info,
                    ));
                }
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
}

// ── Progress row ─────────────────────────────────────────────────────

impl Review<'_> {
fn render_progress_row(&self, ui: &mut egui::Ui) {
    let app = &*self.app;
    let tab_id = self.tab_id;
    let Some(pane) = app.clusters_state.review_panes.get(&tab_id) else {
        return;
    };
    let phase_text = pane
        .phase
        .as_ref()
        .map(|phase| match phase {
            Phase::LoadingEmbeddings => "loading embeddings".to_string(),
            Phase::PartitioningLevel(n) => format!("partitioning level {n}"),
            Phase::Finalizing => "finalizing".to_string(),
        })
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
}

// ── Run / Cancel ─────────────────────────────────────────────────────

impl Review<'_> {
/// Resolve the build inputs: the vault walk + per-note embedding load.
/// Cached on the pane keyed by scope (`source_types` + semantic mode) so
/// live-preview re-runs don't re-query SQLite for every note's embedding;
/// a scope change reloads and drops the stale top-level graph. Returns
/// `None` (after toasting) when the walk fails or nothing is left to
/// cluster. Per `cluster-review-tab-live-preview`.
fn resolve_notes(
    &mut self,
    cfg: &ReviewConfig,
) -> Option<(Arc<Vec<NoteInput>>, Arc<HashMap<String, String>>)> {
    let tab_id = self.tab_id;
    let is_semantic = !matches!(cfg.algorithm, ReviewAlgorithm::FromFolders);
    let scope_sig = scope_signature(&cfg.source_types, is_semantic);

    // Cache hit: same scope → reuse the loaded embeddings.
    if let Some(pane) = self.app.clusters_state.review_panes.get(&tab_id)
        && pane.cached_scope_sig == Some(scope_sig)
        && let (Some(notes), Some(titles)) =
            (pane.cached_notes.clone(), pane.cached_titles.clone())
    {
        return Some((notes, titles));
    }

    let app = &mut *self.app;
    // Walk failures land as a toast; we never start a build with nothing.
    let paths = match app.vault_session.vault.walk_indexable_files("") {
        Ok(v) => v,
        Err(err) => {
            app.push_toast(format!("walk vault: {err}"), ToastLevel::Error);
            return None;
        }
    };
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
        return None;
    }
    if missing_embedding > 0 {
        app.push_toast(
            format!("Cluster build: {missing_embedding} note(s) skipped (no embedding yet)"),
            ToastLevel::Info,
        );
    }

    let notes = Arc::new(notes);
    let titles = Arc::new(titles);
    let pane = app.clusters_state.review_panes.entry(tab_id).or_default();
    pane.cached_notes = Some(notes.clone());
    pane.cached_titles = Some(titles.clone());
    pane.cached_scope_sig = Some(scope_sig);
    // New note set → any cached top-level graph is stale.
    pane.cached_top_graph = None;
    Some((notes, titles))
}

/// Kick off an async structural pass. Spawns the streaming build on a
/// `spawn_blocking` worker (inside the runtime entered by the egui
/// frame) and stashes the `Receiver<BuildEvent>` + cancel atomic on the
/// pane. The frame loop drains the receiver via `drain_events`. Reuses
/// the cached note set + top-level Leiden graph when available so a
/// live-preview re-run skips the SQLite load and the O(n²) kNN sweep.
///
/// `live = true` is a live-preview rebuild: keep the current result on
/// screen (swapped on `Done`), leave the config open, and skip the progress
/// row + done-toast so the update happens in place. `false` is a manual Run
/// (clears the result, collapses config, shows progress).
///
/// status: cluster-review-tab-async-pass
/// status: cluster-review-tab-live-preview
fn run_structural_streaming(
    &mut self,
    ctx: &egui::Context,
    cfg: &ReviewConfig,
    live: bool,
) {
    let tab_id = self.tab_id;
    if self
        .app
        .clusters_state
        .review_panes
        .get(&tab_id)
        .is_some_and(|p| p.running)
    {
        return;
    }

    let (notes, titles) = match self.resolve_notes(cfg) {
        Some(v) => v,
        None => return,
    };
    let app = &mut *self.app;
    let prebuilt_graph = app
        .clusters_state
        .review_panes
        .get(&tab_id)
        .and_then(|p| p.cached_top_graph.clone());

    // Translate the form into core types.
    let p = app.clusters_state.advanced_params.clone();
    let run_sig = config_signature(cfg, &p);
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
                ReviewAlgorithm::Hdbscan => Algorithm::Hdbscan,
                ReviewAlgorithm::Leiden => Algorithm::Leiden,
                ReviewAlgorithm::Hybrid => Algorithm::Hybrid,
                ReviewAlgorithm::Gmm => Algorithm::Gmm,
                ReviewAlgorithm::FromFolders => unreachable!(),
            };
            let leiden = hiker_core::cluster::LeidenParams {
                k_nearest: p.k_nearest as u32,
                edge_weight_floor: p.edge_weight_floor,
                iterations: p.iterations,
                resolution: p.resolution,
                // Drive the decisive top-level (virtual-root) cut from the
                // same slider the user sees. Previously the top-level Split
                // ran at the hidden `top_level_resolution` default and the
                // slider only affected sub-splits, so turning the knob had
                // no effect on whether the build produced ≥2 clusters.
                top_level_resolution: p.resolution,
                min_cluster_size: p.min_cluster_size as u32,
                ..hiker_core::cluster::LeidenParams::default()
            };
            let params = Params {
                min_cluster_size: p.min_cluster_size as u32,
                min_samples: Some(p.min_samples as u32),
                algorithm,
                leiden,
                summarize: SummarizeMode::None,
                summary_confidence_threshold: p.summary_confidence_threshold,
                include_outliers: p.include_outliers,
                disable_recursion: p.disable_recursion,
                ..Params::default()
            };
            BuildMethod::Cluster { params }
        }
    };

    // Spawn the streaming build. The closure must run inside a tokio
    // runtime: the egui frame loop enters the runtime each tick, so
    // `Handle::current` is live here. The note set is cloned out of the
    // Arc because the build owns its inputs; the embeddings memcpy is
    // negligible next to the SQLite load we just skipped.
    let cancel = Arc::new(AtomicBool::new(false));
    let (_handle, rx) = build_tree_structural_streaming(
        method,
        (*notes).clone(),
        cancel.clone(),
        prebuilt_graph,
    );

    let pane = app.clusters_state.review_panes.entry(tab_id).or_default();
    pane.running = true;
    pane.live_rebuild = live;
    pane.cancel = Some(cancel);
    pane.events_rx = Some(Arc::new(Mutex::new(rx)));
    pane.started_at = Some(Instant::now());
    pane.phase = None;
    pane.counters = ProgressCounters::default();
    // A live rebuild keeps the prior result visible (swapped atomically on
    // Done) and the config open; a manual run clears + collapses.
    if !live {
        pane.result = None;
        pane.config_collapsed = true;
    }
    pane.user_renamed.clear();
    pane.editing = None;
    pane.live_top.clear();
    pane.live_pending_children.clear();
    // View options (Tree/Graph toggle on `pane.view`, expanded chevron
    // set on `pane.expanded`) intentionally persist across re-runs so the
    // user keeps the layout they were working in. Leftover node ids in
    // `expanded` from a prior run are harmless — node ids are fresh per
    // run and stale lookups just return false.
    pane.note_titles = (*titles).clone();
    // Record what config this run reflects so live preview only re-fires
    // on an actual change, and clear any pending debounce.
    pane.last_run_sig = Some(run_sig);
    pane.pending_rerun = None;

    // Prime a repaint so the progress row renders without waiting for
    // user input.
    ctx.request_repaint();
}

/// Live preview: when a config knob changes after a first run, schedule a
/// debounced auto-rerun (and fire it once the debounce elapses). Gated by
/// the global `live_preview` toggle and a per-algorithm note-count ceiling
/// so large vaults don't auto-trigger multi-second rebuilds. Per
/// `cluster-review-tab-live-preview`.
fn maybe_live_rerun(&mut self, ctx: &egui::Context, cfg: &ReviewConfig) {
    let tab_id = self.tab_id;
    let live_on = self.app.clusters_state.advanced_params.live_preview;
    let p = self.app.clusters_state.advanced_params.clone();
    let sig = config_signature(cfg, &p);

    let mut fire = false;
    {
        let Some(pane) = self.app.clusters_state.review_panes.get_mut(&tab_id) else {
            return;
        };
        // Only after a first run (so the note set exists), while idle.
        if pane.running || pane.last_run_sig.is_none() {
            return;
        }
        let n = pane.cached_notes.as_ref().map_or(0, |v| v.len());
        if !live_on || n == 0 || n > live_preview_max(cfg.algorithm) {
            pane.pending_rerun = None;
            return;
        }
        if Some(sig) == pane.last_run_sig {
            pane.pending_rerun = None; // nothing changed since the last run
            return;
        }
        let now = Instant::now();
        match pane.pending_rerun {
            // Same change still pending — fire once the debounce elapses.
            Some((psig, deadline)) if psig == sig => {
                if now >= deadline {
                    pane.pending_rerun = None;
                    fire = true;
                }
            }
            // New (or changed) config → (re)start the debounce window.
            _ => pane.pending_rerun = Some((sig, now + LIVE_DEBOUNCE)),
        }
    }
    if fire {
        self.run_structural_streaming(ctx, cfg, /*live=*/ true);
    } else {
        // Keep the frame loop ticking toward the debounce deadline.
        ctx.request_repaint_after(LIVE_DEBOUNCE);
    }
}

/// Flip the shared cancel atomic. The background task notices on its
/// next periodic check and emits `BuildEvent::Cancelled`, which is
/// applied in `drain_events`.
///
/// status: cluster-review-tab-cancel-pass
fn cancel_run(&mut self) {
    let Some(pane) = self.app.clusters_state.review_panes.get(&self.tab_id) else {
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
    &mut self,
    cfg: &ReviewConfig,
    trees: &Arc<hiker_core::trees::types::Db>,
    submit_naming: bool,
) {
    let tab_id = self.tab_id;
    let Some(pane) = self.app.clusters_state.review_panes.get(&tab_id) else {
        self.app.push_toast("No clustering result to confirm", ToastLevel::Warn);
        return;
    };
    if pane.confirming {
        return;
    }
    let Some(stored) = pane.result.as_ref() else {
        self.app.push_toast("No clustering result to confirm", ToastLevel::Warn);
        return;
    };
    let renamed = pane.user_renamed.clone();
    let build = stored.build.clone();

    let pane = self.app.clusters_state.review_panes.entry(tab_id).or_default();
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
            let pane = self.app.clusters_state.review_panes.entry(tab_id).or_default();
            pane.confirming = false;
            self.app.push_toast(format!("insert_tree: {err}"), ToastLevel::Error);
            return;
        }
    };
    let mut inserts = node_inserts(&build.tree);
    if !renamed.is_empty() {
        for ins in inserts.iter_mut() {
            if let Some(new_name) = renamed.get(&ins.node_id) {
                ins.name = new_name.clone();
                ins.user_edited_name = true;
            }
        }
    }
    if let Err(err) = trees.insert_nodes(&tree_id, &inserts) {
        let pane = self.app.clusters_state.review_panes.entry(tab_id).or_default();
        pane.confirming = false;
        let _ = trees.delete_tree(&tree_id);
        self.app.push_toast(format!("insert_nodes: {err}"), ToastLevel::Error);
        return;
    }
    self.app.clusters_state.selected_tree = Some(tree_id.clone());
    self.app.clusters_state.loaded = false;
    self.app.clusters_state.dirty = true;

    // Drop pane state + close tab — user lands on the cluster sidebar
    // (the egui port doesn't have a separate cluster-pane tab kind yet).
    self.app.clusters_state.review_panes.remove(&tab_id);
    let tabs_pos = self.app.session.tabs.iter().position(|t| t.id == tab_id);
    if let Some(pos) = tabs_pos {
        self.app.session.tabs.remove(pos);
        if self.app.session.active_tab == Some(tab_id) {
            self.app.session.active_tab = self.app.session.tabs.last().map(|t| t.id);
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
        // insertion order — the same order `node_inserts`
        // writes — children before parents at the top of the loop, plus
        // the (optional) synthesized root last. We re-list from the
        // freshly persisted tree to pick up that natural order without
        // recomputing it.
        let submitted = self.submit_naming_tasks(trees, &tree_id);
        self.app.push_toast(
            format!(
                "Tree persisted — {name}. Queued {submitted} naming task(s)."
            ),
            ToastLevel::Info,
        );
    } else {
        self.app.push_toast(
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
/// parent, matching `node_inserts`), filters to
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
    &mut self,
    trees: &Arc<hiker_core::trees::types::Db>,
    tree_id: &str,
) -> usize {
    use hiker_core::tasks::types::{Priority, Task, TaskKind, TaskPayload, TaskShape};
    use hiker_core::trees::types::NodeKind;

    let queue = self.app.vault_session.services.tasks.clone();

    let nodes = match trees.list_nodes(tree_id) {
        Ok(ns) => ns,
        Err(err) => {
            self.app.push_toast(
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
                id: hiker_core::store::dto::new_id(),
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

fn discard(&mut self) {
    let tab_id = self.tab_id;
    let app = &mut *self.app;
    // Best-effort cancel of any in-flight build before tearing the pane
    // down, so the background task wakes up to a dead receiver and
    // exits promptly.
    if let Some(pane) = app.clusters_state.review_panes.get(&tab_id) {
        if let Some(c) = pane.cancel.as_ref() {
            c.store(true, Ordering::Relaxed);
        }
    }
    app.clusters_state.review_panes.remove(&tab_id);
    let pos = app.session.tabs.iter().position(|t| t.id == tab_id);
    if let Some(pos) = pos {
        app.session.tabs.remove(pos);
        if app.session.active_tab == Some(tab_id) {
            app.session.active_tab = app.session.tabs.last().map(|t| t.id);
        }
    }
}
}

