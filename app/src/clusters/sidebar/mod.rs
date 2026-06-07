//! Cluster-trees sidebar body. Tree picker + hierarchical node list with
//! expand/collapse, inline rename, drag-and-drop reparenting, and
//! note-row click → editor preview. Backed by `hiker_core::trees::types::Db`.
//!
//! Scope: list/select trees, browse nodes, rename / drop / promote,
//! reorder via DnD, open notes, build (via cluster_review tab),
//! summarize subset (single + multi), regenerate names, multi-select
//! stage-moves + stage-tags into the staging queue, undo/redo, graph
//! view, advanced params. Save-as-triage isn't ported yet.

mod node_menu;
mod tree;

use std::sync::Arc;

use eframe::egui;
use hiker_core::trees::types::Db;

use crate::clusters::state::State;
use crate::activity::SurfaceCtx;
use crate::state::{AppState, Toast, ToastLevel};
use hiker_theme as theme;

/// Shared per-frame context for the cluster sidebar. Wraps the narrow
/// feature `SurfaceCtx` with the `trees` handle (cloned once at the top of
/// `render_body`) so the render/mutation helpers across this module's
/// files can be `&mut self` methods on one receiver. Broad effects
/// (open a tab, open a note) are queued via `ctx.defer`; everything else
/// reads/writes through `ctx` fields — the feature's own `State`
/// (`ctx.state`), the service handles (`ctx.services`), the vault
/// (`ctx.vault`), the config (`ctx.config`), and the toast sink
/// (`ctx.toasts`). The frame already runs inside the tokio runtime
/// guard, so the LLM jobs' ambient `tokio::spawn` keeps working without
/// a `Handle` on `SurfaceCtx`.
pub(super) struct ClusterCtx<'a, 'c> {
    pub(super) ctx: &'a mut SurfaceCtx<'c>,
    pub(super) trees: Arc<Db>,
}

impl ClusterCtx<'_, '_> {
    /// Mutable handle to the feature's own UI state slice.
    pub(super) fn st(&mut self) -> &mut State {
        self.ctx.state.downcast_mut::<State>().expect("clusters state")
    }

    /// Immutable handle to the feature's own UI state slice.
    pub(super) fn st_ref(&self) -> &State {
        self.ctx.state.downcast_ref::<State>().expect("clusters state")
    }

    /// Push a toast onto the shared sink (the narrow `SurfaceCtx` carries the
    /// `Vec<Toast>` directly; there is no `&mut AppState` here for
    /// `push_toast`).
    pub(super) fn toast(&mut self, message: impl Into<String>, level: ToastLevel) {
        push_toast(self.ctx.toasts, message, level);
    }
}

/// Render the cluster-trees sidebar body through the narrow feature
/// `SurfaceCtx`. Clones the `trees` handle once, hydrates, then paints header /
/// picker / toolbar / inline editors / the selected tree.
pub(super) fn render_body(ui: &mut egui::Ui, ctx: &mut SurfaceCtx<'_>) {
    let trees = ctx.services.trees.clone();
    let mut cx = ClusterCtx { ctx, trees };

    cx.hydrate_if_needed();

    // New-tree creation moved to the Clusters accordion-header `+` split-button
    // (`cluster-editor-new-tree-action`), which also exposes presets; the panel
    // body no longer repeats the activity title or carries its own button.
    cx.tree_picker(ui);
    cx.toolbar(ui);
    ui.separator();

    // Summary-edit modal popover. Lives at panel scope so the row
    // renderer can keep its allocate_exact_size shape and not contend
    // with a variable-height inline editor.
    cx.summary_edit_inline(ui);
    cx.policy_editors_inline(ui);
    cx.stage_forms_inline(ui);

    if let Some(tree_id) = cx.st().selected_tree.clone() {
        cx.render_selected_tree(ui, &tree_id);
    } else if cx.st().trees.is_empty() {
        empty_message(
            ui,
            "No trees yet. \"Suggest reorganization\" will build one (stub).",
        );
    } else {
        empty_message(ui, "Select a tree above.");
    }
}

impl ClusterCtx<'_, '_> {
fn advanced_params_popover(&mut self, ui: &mut egui::Ui) {
    let state = self.st();
    egui::Frame::default()
        .fill(theme::active_bg())
        .stroke(egui::Stroke::new(1.0, theme::divider()))
        .inner_margin(8.0)
        .show(ui, |ui| {
            ui.label(egui::RichText::new("Advanced clustering parameters").strong().small());
            let p = &mut state.advanced_params;
            egui::Grid::new("cluster-params-grid")
                .num_columns(2)
                .spacing([8.0, 4.0])
                .show(ui, |ui| {
                    use crate::clusters::param_slider;
                    param_slider(ui, "Min cluster size", &mut p.min_cluster_size, 2..=500, true,
                        "Smallest cluster the algorithm will form");
                    param_slider(ui, "Min samples", &mut p.min_samples, 1..=50, false,
                        "Higher is more conservative — more points fall out as outliers");
                    param_slider(ui, "k nearest", &mut p.k_nearest, 2..=100, false,
                        "Neighbors per node in the Leiden kNN similarity graph");
                    ui.label("Algorithm");
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut p.use_leiden, false, "HDBSCAN");
                        ui.selectable_value(&mut p.use_leiden, true, "Leiden");
                    });
                    ui.end_row();
                    param_slider(ui, "Outlier threshold", &mut p.outlier_threshold, 0.0..=1.0, false,
                        "Notes below this similarity to their cluster become outliers");
                    ui.label("Include outliers");
                    ui.checkbox(&mut p.include_outliers, "")
                        .on_hover_text("Keep unclustered notes in an outliers bucket instead of force-routing them into the nearest cluster");
                    ui.end_row();
                });
            if ui.small_button("Close").clicked() {
                state.showing_advanced_params = false;
            }
        });
    ui.separator();
}
}

fn regenerate_names(cx: &mut ClusterCtx<'_, '_>, tree_id: &str) {
    let trees = cx.trees.clone();
    if cx.st().llm_job_in_flight {
        cx.toast("A naming run is already in flight", ToastLevel::Info);
        return;
    }
    let Some(client) = build_llm_client(cx) else {
        return;
    };
    let nodes = cx.st().nodes.clone();
    let targets: Vec<(String, String)> = nodes
        .iter()
        .filter(|n| {
            matches!(n.kind, hiker_core::trees::types::NodeKind::Cluster) && !n.user_edited_name
        })
        .map(|n| (n.id.clone(), n.summary.clone()))
        .collect();
    if targets.is_empty() {
        cx.toast("No clusters need regeneration", ToastLevel::Info);
        return;
    }

    let trees_for_task = trees.clone();
    let tree_id_owned = tree_id.to_string();
    let (tx, rx) = tokio::sync::oneshot::channel::<crate::clusters::state::LlmJobOutcome>();
    tokio::spawn(async move {
        let mut succeeded = 0usize;
        let mut failed = 0usize;
        for (node_id, summary) in &targets {
            let members = collect_member_titles(&nodes, node_id);
            let prompt = format!(
                "Provide a short name (<6 words) for this cluster.\nMembers:\n{}\n\nRespond with only the name.",
                members.join("\n")
            );
            let messages = vec![hiker_llm::Message::user(prompt)];
            let resp = match client.chat(&messages).await {
                Ok(r) => r,
                Err(err) => {
                    tracing::warn!(node = %node_id, error = %err, "regenerate-names: llm call failed");
                    failed += 1;
                    continue;
                }
            };
            let new_name = resp.lines().next().unwrap_or("").trim().to_string();
            if new_name.is_empty() {
                failed += 1;
                continue;
            }
            match trees_for_task.auto_set_name_summary(&tree_id_owned, node_id, &new_name, summary) {
                Ok(_) => succeeded += 1,
                Err(err) => {
                    tracing::warn!(error = %err, "auto_set_name_summary failed");
                    failed += 1;
                }
            }
        }
        let _ = tx.send((succeeded, failed));
    });
    let st = cx.st();
    st.llm_job_in_flight = true;
    st.llm_job_rx = Some(rx);
    cx.toast("Regenerating cluster names...", ToastLevel::Info);
}

/// Construct an `Arc<dyn Client>` from current settings, pushing an
/// error toast if the LLM is disabled or the client failed to build.
/// Returns `None` in either failure case; callers should early-return.
fn build_llm_client(cx: &mut ClusterCtx<'_, '_>) -> Option<Arc<dyn hiker_llm::Client>> {
    let llm_cfg = cx
        .ctx
        .config
        .read()
        .map(|c| c.llm.clone())
        .unwrap_or_default();
    if !llm_cfg.enabled {
        cx.toast("LLM is disabled in settings", ToastLevel::Warn);
        return None;
    }
    match hiker_core::llm::client_from_config(&llm_cfg) {
        Ok(c) => Some(Arc::new(c) as Arc<dyn hiker_llm::Client>),
        Err(err) => {
            cx.toast(format!("LLM client error: {err}"), ToastLevel::Error);
            None
        }
    }
}

/// Poll the in-flight LLM naming task's result channel; on completion
/// clear the gate, surface a summary toast, and mark the tree dirty so
/// the new names land in the UI.
impl AppState {
pub(crate) fn poll_cluster_llm_job(&mut self) {
    let state = self;
    let Some(mut rx) = state.clusters_state.llm_job_rx.take() else {
        return;
    };
    match rx.try_recv() {
        Ok((succeeded, failed)) => {
            state.clusters_state.llm_job_in_flight = false;
            state.push_toast(
                format!("LLM naming: {succeeded} succeeded, {failed} failed"),
                crate::state::ToastLevel::Info,
            );
            state.clusters_state.dirty = true;
        }
        Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
            state.clusters_state.llm_job_rx = Some(rx);
        }
        Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
            state.clusters_state.llm_job_in_flight = false;
            state.push_toast(
                "LLM naming aborted",
                crate::state::ToastLevel::Warn,
            );
        }
    }
}
}

fn collect_member_titles(
    nodes: &[hiker_core::trees::types::EditableNode],
    cluster_id: &str,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![cluster_id.to_string()];
    while let Some(id) = stack.pop() {
        for n in nodes.iter().filter(|n| n.parent.as_deref() == Some(id.as_str())) {
            if matches!(n.kind, hiker_core::trees::types::NodeKind::Leaf) {
                out.push(n.name.clone());
            } else {
                stack.push(n.id.clone());
            }
        }
        if out.len() > 50 {
            out.truncate(50);
            break;
        }
    }
    out
}

/// Semantic re-split of a subtree. Pulls leaf embeddings from the read
/// store, hands them to `Db::split_cluster`, then optionally regenerates
/// names on the new sub-clusters via the LLM.
impl ClusterCtx<'_, '_> {
pub(super) fn recluster_subtree(
    &mut self,
    tree_id: &str,
    target_node_id: Option<&str>,
) {
    let trees = self.trees.clone();
    let store_mutex = self.ctx.services.read_store.clone();
    let p = self.st().advanced_params.clone();
    let algorithm = if p.use_leiden {
        hiker_core::cluster::Algorithm::Leiden
    } else {
        hiker_core::cluster::Algorithm::Hdbscan
    };
    let leiden = hiker_core::cluster::LeidenParams {
        k_nearest: p.k_nearest as u32,
        ..hiker_core::cluster::LeidenParams::default()
    };
    let params = hiker_core::cluster::Params {
        min_cluster_size: p.min_cluster_size as u32,
        min_samples: Some(p.min_samples as u32),
        algorithm,
        leiden,
        summarize: hiker_core::cluster::SummarizeMode::None,
        ..hiker_core::cluster::Params::default()
    };
    let resolver = |leaf_id: &str| -> Option<Vec<f32>> {
        let store = store_mutex.lock().ok()?;
        store.note_embedding_for_path(leaf_id).ok().flatten()
    };
    match trees.split_cluster(tree_id, target_node_id, &params, &resolver, None) {
        Ok(outcome) => {
            let n = outcome.new_clusters.len();
            self.toast(format!("Recluster produced {n} sub-clusters"), ToastLevel::Info);
            let st = self.st();
            st.mark_dirty();
            // Drop any redo stack — a fresh forward op invalidates it.
            st.redo_stacks.remove(tree_id);
        }
        Err(err) => self.toast(format!("Recluster failed: {err}"), ToastLevel::Error),
    }
}
}

/// LLM-summarize a focused subset of clusters. Pairs of (name, summary)
/// land via `Db::auto_set_name_summary` so user-edited rows are
/// preserved.
pub(super) fn summarize_subset(
    cx: &mut ClusterCtx<'_, '_>,
    tree_id: &str,
    node_ids: &[String],
) {
    let trees = cx.trees.clone();
    if cx.st().llm_job_in_flight {
        cx.toast("A naming run is already in flight", ToastLevel::Info);
        return;
    }
    let Some(client) = build_llm_client(cx) else {
        return;
    };
    let nodes = cx.st().nodes.clone();
    // Materialize the (id, name) targets up front so the spawned task
    // doesn't need to re-walk the nodes list per iteration.
    let targets: Vec<(String, String)> = node_ids
        .iter()
        .filter_map(|nid| {
            let n = nodes.iter().find(|x| &x.id == nid)?;
            if !matches!(n.kind, hiker_core::trees::types::NodeKind::Cluster) {
                return None;
            }
            Some((n.id.clone(), n.name.clone()))
        })
        .collect();
    if targets.is_empty() {
        return;
    }

    let trees_for_task = trees.clone();
    let tree_id_owned = tree_id.to_string();
    let (tx, rx) = tokio::sync::oneshot::channel::<crate::clusters::state::LlmJobOutcome>();
    tokio::spawn(async move {
        let mut succeeded = 0usize;
        let mut failed = 0usize;
        for (nid, name) in &targets {
            let members = collect_member_titles(&nodes, nid);
            let prompt = format!(
                "Write a 1-sentence summary for this cluster.\nMembers:\n{}",
                members.join("\n")
            );
            let messages = vec![hiker_llm::Message::user(prompt)];
            let summary = match client.chat(&messages).await {
                Ok(s) => s.trim().to_string(),
                Err(err) => {
                    tracing::warn!(error = %err, "summarize-subset llm failed");
                    failed += 1;
                    continue;
                }
            };
            if summary.is_empty() {
                failed += 1;
                continue;
            }
            if trees_for_task
                .auto_set_name_summary(&tree_id_owned, nid, name, &summary)
                .is_ok()
            {
                succeeded += 1;
            } else {
                failed += 1;
            }
        }
        let _ = tx.send((succeeded, failed));
    });
    let st = cx.st();
    st.llm_job_in_flight = true;
    st.llm_job_rx = Some(rx);
    cx.toast("Summarizing clusters...", ToastLevel::Info);
}

impl ClusterCtx<'_, '_> {
fn tree_picker(&mut self, ui: &mut egui::Ui) {
    let state = self.st();
    let trees = &state.trees;
    let selected_label = state
        .selected_tree
        .as_ref()
        .and_then(|id| trees.iter().find(|t| &t.id == id))
        .map(|t| format!("{} ({})", t.name, t.state))
        .unwrap_or_else(|| "(select a tree)".to_string());

    egui::ComboBox::from_id_salt("cluster-tree-picker")
        .width(ui.available_width() - 8.0)
        .selected_text(selected_label)
        .show_ui(ui, |ui| {
            if trees.is_empty() {
                ui.label(
                    egui::RichText::new("(no trees)")
                        .color(theme::muted())
                        .italics(),
                );
            }
            for t in trees {
                let is_sel = state.selected_tree.as_deref() == Some(t.id.as_str());
                let label = format!("{}  [{}]", t.name, t.state);
                if ui.selectable_label(is_sel, label).clicked() {
                    state.selected_tree = Some(t.id.clone());
                    state.dirty = true;
                    state.renaming = None;
                }
            }
        });
}

fn render_selected_tree(
    &mut self,
    ui: &mut egui::Ui,
    tree_id: &str,
) {
    if self.st().nodes.is_empty() {
        empty_message(ui, "This tree has no nodes.");
        return;
    }
    self.show_tree(ui, tree_id);
}

fn hydrate_if_needed(&mut self) {
    let trees = self.trees.clone();
    if !self.st().loaded {
        match trees.list_trees() {
            Ok(rows) => {
                let st = self.st();
                st.trees = rows;
                st.loaded = true;
                // Auto-select the first tree on first load so the picker
                // isn't an empty prompt.
                if st.selected_tree.is_none() {
                    st.selected_tree = st.trees.first().map(|t| t.id.clone());
                }
                st.dirty = true;
            }
            Err(err) => {
                self.toast(format!("Failed to list trees: {}", err), ToastLevel::Error);
                self.st().loaded = true;
            }
        }
    }
    if self.st().dirty {
        if let Some(id) = self.st().selected_tree.clone() {
            match trees.list_nodes(&id) {
                Ok(nodes) => self.st().nodes = nodes,
                Err(err) => {
                    self.toast(
                        format!("Failed to load tree nodes: {}", err),
                        ToastLevel::Error,
                    );
                    self.st().nodes.clear();
                }
            }
        } else {
            self.st().nodes.clear();
        }
        self.st().dirty = false;
    }
}
}

fn empty_message(ui: &mut egui::Ui, msg: &str) {
    ui.add_space(20.0);
    ui.label(egui::RichText::new(msg).color(theme::muted()).italics());
}

/// Push a toast directly onto the `SurfaceCtx` toast sink. Free fn (rather than
/// the `ClusterCtx::toast` method) for the inline-editor / toolbar
/// closures, which split-borrow the disjoint `ctx.toasts` field and so
/// can't re-borrow `&mut self`.
pub(super) fn push_toast(toasts: &mut Vec<Toast>, message: impl Into<String>, level: ToastLevel) {
    toasts.push(Toast {
        message: message.into(),
        level,
        created_at: std::time::Instant::now(),
        undo: None,
    });
}

// ── Inline modal-style editors ────────────────────────────────────────
// Edit-summary, tag-policy + move-policy, and stage-moves + stage-tags
// target prompts. Each renders only when its corresponding
// `state.clusters_state.editing_*` slot is `Some(...)` — clicking the
// triggering row or toolbar button populates the slot, and the form
// clears it on Save/Cancel. They sit at panel scope (invoked from
// `clusters_panel` above) so the row renderer can keep its fixed-height
// shape and not contend with a variable-height inline editor.
impl ClusterCtx<'_, '_> {
pub(super) fn summary_edit_inline(
    &mut self,
    ui: &mut egui::Ui,
) {
    let Some((node_id, mut draft)) = self.st().editing_summary.clone() else {
        return;
    };
    let Some(tree_id) = self.st().selected_tree.clone() else {
        return;
    };
    let target_name = self
        .st()
        .nodes
        .iter()
        .find(|n| n.id == node_id)
        .map(|n| n.name.clone())
        .unwrap_or_else(|| node_id.clone());
    let trees = self.trees.clone();
    let state = self.ctx.state.downcast_mut::<State>().expect("clusters state");
    let toasts = &mut *self.ctx.toasts;
    egui::Frame::default()
        .fill(theme::active_bg())
        .stroke(egui::Stroke::new(1.0, theme::divider()))
        .inner_margin(8.0)
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new(format!("Edit summary · {target_name}")).small().strong(),
            );
            let resp = ui.add(
                egui::TextEdit::multiline(&mut draft)
                    .desired_rows(3)
                    .desired_width(ui.available_width()),
            );
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    match trees.set_summary(&tree_id, &node_id, draft.trim()) {
                        Ok(()) => state.mark_dirty(),
                        Err(err) => push_toast(
                            toasts,
                            format!("Set summary failed: {err}"),
                            ToastLevel::Error,
                        ),
                    }
                    state.editing_summary = None;
                    return;
                }
                if ui.button("Cancel").clicked()
                    || (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)))
                {
                    state.editing_summary = None;
                    return;
                }
                state.editing_summary = Some((node_id.clone(), draft.clone()));
            });
        });
    ui.separator();
}

pub(super) fn policy_editors_inline(
    &mut self,
    ui: &mut egui::Ui,
) {
    let Some(tree_id) = self.st().selected_tree.clone() else {
        return;
    };
    let trees = self.trees.clone();
    let state = self.ctx.state.downcast_mut::<State>().expect("clusters state");
    let toasts = &mut *self.ctx.toasts;
    if let Some((node_id, mut slug, mut require_review)) =
        state.editing_tag_policy.clone()
    {
        egui::Frame::default()
            .fill(theme::active_bg())
            .stroke(egui::Stroke::new(1.0, theme::divider()))
            .inner_margin(8.0)
            .show(ui, |ui| {
                ui.label(egui::RichText::new("Tag policy").strong().small());
                ui.horizontal(|ui| {
                    ui.label("Slug");
                    ui.add(
                        egui::TextEdit::singleline(&mut slug)
                            .hint_text("e.g. project-x")
                            .desired_width(160.0),
                    );
                });
                ui.checkbox(&mut require_review, "Require review before apply");
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() && !slug.trim().is_empty() {
                        let policy = hiker_core::trees::types::NodePolicy::Tag {
                            slug: slug.trim().to_string(),
                            require_review,
                        };
                        if let Err(err) = trees.set_policy(&tree_id, &node_id, Some(&policy)) {
                            push_toast(
                                toasts,
                                format!("Set tag policy failed: {err}"),
                                ToastLevel::Error,
                            );
                        } else {
                            state.mark_dirty();
                        }
                        state.editing_tag_policy = None;
                        return;
                    }
                    if ui.button("Cancel").clicked() {
                        state.editing_tag_policy = None;
                        return;
                    }
                    state.editing_tag_policy =
                        Some((node_id.clone(), slug.clone(), require_review));
                });
            });
        ui.separator();
    }
    if let Some((node_id, mut folder, mut require_review)) =
        state.editing_move_policy.clone()
    {
        egui::Frame::default()
            .fill(theme::active_bg())
            .stroke(egui::Stroke::new(1.0, theme::divider()))
            .inner_margin(8.0)
            .show(ui, |ui| {
                ui.label(egui::RichText::new("Move policy").strong().small());
                ui.horizontal(|ui| {
                    ui.label("Folder");
                    ui.add(
                        egui::TextEdit::singleline(&mut folder)
                            .hint_text("e.g. archive/old")
                            .desired_width(200.0),
                    );
                });
                ui.checkbox(&mut require_review, "Require review before apply");
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() && !folder.trim().is_empty() {
                        let policy = hiker_core::trees::types::NodePolicy::Move {
                            folder: folder.trim().to_string(),
                            require_review,
                        };
                        if let Err(err) = trees.set_policy(&tree_id, &node_id, Some(&policy)) {
                            push_toast(
                                toasts,
                                format!("Set move policy failed: {err}"),
                                ToastLevel::Error,
                            );
                        } else {
                            state.mark_dirty();
                        }
                        state.editing_move_policy = None;
                        return;
                    }
                    if ui.button("Cancel").clicked() {
                        state.editing_move_policy = None;
                        return;
                    }
                    state.editing_move_policy =
                        Some((node_id.clone(), folder.clone(), require_review));
                });
            });
        ui.separator();
    }
}

pub(super) fn stage_forms_inline(&mut self, ui: &mut egui::Ui) {
    let trees = self.trees.clone();
    let Some(tree_id) = self.st().selected_tree.clone() else {
        return;
    };
    let SurfaceCtx {
        state,
        toasts,
        services,
        vault,
        ..
    } = &mut *self.ctx;
    let state = state.downcast_mut::<State>().expect("clusters state");
    if let Some(mut target) = state.editing_stage_move_target.clone() {
        let selected: Vec<String> = state.selected_nodes.iter().cloned().collect();
        egui::Frame::default()
            .fill(theme::active_bg())
            .stroke(egui::Stroke::new(1.0, theme::divider()))
            .inner_margin(8.0)
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new(format!(
                        "Stage moves · {} selected",
                        selected.len()
                    ))
                    .strong()
                    .small(),
                );
                ui.horizontal(|ui| {
                    ui.label("Target folder");
                    ui.add(
                        egui::TextEdit::singleline(&mut target)
                            .hint_text("e.g. archive/projects")
                            .desired_width(220.0),
                    );
                });
                ui.horizontal(|ui| {
                    if ui.button("Stage").clicked() && !target.trim().is_empty() {
                        // status: cluster-editor-multi-select-stage-move
                        let store_mutex = services.read_store.clone();
                        let oplog = services.oplog.clone();
                        if let Ok(store) = store_mutex.lock() {
                            let args = hiker_core::suggest::StageMoveArgs {
                                tree_id: &tree_id,
                                node_ids: &selected,
                                target_folder: target.trim(),
                            };
                            match hiker_core::suggest::stage_moves(
                                &trees,
                                &args,
                                &store,
                                vault,
                                &oplog,
                            ) {
                                Ok(outcome) => push_toast(
                                    toasts,
                                    format!("Staged {} moves", outcome.op_ids.len()),
                                    ToastLevel::Info,
                                ),
                                Err(err) => push_toast(
                                    toasts,
                                    format!("Stage moves failed: {err}"),
                                    ToastLevel::Error,
                                ),
                            }
                        }
                        state.editing_stage_move_target = None;
                        return;
                    }
                    if ui.button("Cancel").clicked() {
                        state.editing_stage_move_target = None;
                        return;
                    }
                    state.editing_stage_move_target = Some(target.clone());
                });
            });
        ui.separator();
    }
    if let Some(mut slug) = state.editing_stage_tag_slug.clone() {
        let selected: Vec<String> = state.selected_nodes.iter().cloned().collect();
        egui::Frame::default()
            .fill(theme::active_bg())
            .stroke(egui::Stroke::new(1.0, theme::divider()))
            .inner_margin(8.0)
            .show(ui, |ui| {
                ui.label(
                    egui::RichText::new(format!("Stage tags · {} selected", selected.len()))
                        .strong()
                        .small(),
                );
                ui.horizontal(|ui| {
                    ui.label("Tag slug");
                    ui.add(
                        egui::TextEdit::singleline(&mut slug)
                            .hint_text("e.g. project-x")
                            .desired_width(220.0),
                    );
                });
                ui.horizontal(|ui| {
                    if ui.button("Stage").clicked() && !slug.trim().is_empty() {
                        // status: cluster-editor-multi-select-stage-tag
                        let store_mutex = services.read_store.clone();
                        let oplog = services.oplog.clone();
                        if let Ok(store) = store_mutex.lock() {
                            let args = hiker_core::suggest::StageTagArgs {
                                tree_id: &tree_id,
                                node_ids: &selected,
                                tag_slug: slug.trim(),
                            };
                            match hiker_core::suggest::stage_tags(
                                &trees,
                                &args,
                                vault,
                                &store,
                                &oplog,
                            ) {
                                Ok(ids) => push_toast(
                                    toasts,
                                    format!("Staged {} tags", ids.len()),
                                    ToastLevel::Info,
                                ),
                                Err(err) => push_toast(
                                    toasts,
                                    format!("Stage tags failed: {err}"),
                                    ToastLevel::Error,
                                ),
                            }
                        }
                        state.editing_stage_tag_slug = None;
                        return;
                    }
                    if ui.button("Cancel").clicked() {
                        state.editing_stage_tag_slug = None;
                        return;
                    }
                    state.editing_stage_tag_slug = Some(slug.clone());
                });
            });
        ui.separator();
    }
}
}
