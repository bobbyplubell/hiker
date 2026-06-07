//! Result-panel rendering for the Cluster Review tab: the Tree/Graph view
//! toggle, the live-revealing cluster-row tree with inline rename, the outlier
//! footer, and the graph-view adapter into the persisted cluster-graph
//! renderer. Split out of `panel/mod.rs` as a continuation of the `Review`
//! impl so that file stays within its per-file line budget; everything here is
//! a method (or associated fn) on `super::Review`, sharing its borrowed state.

use std::collections::HashMap;

use eframe::egui;
use hiker_core::cluster::{BuiltClusterNode, BuiltClusterTree};
use hiker_core::trees::types::{EditableNode, NodeKind};
use hiker_theme as theme;

use super::{ResultView, Review};
use crate::state::AppState;
use crate::tab::TabId;

impl Review<'_> {
    pub(super) fn render_result_panel(&mut self, ui: &mut egui::Ui) {
        let tab_id = self.tab_id;
        // View toggle row.
        // status: cluster-review-tab-result-view-toggle
        let current_view = self.app
            .clusters_state
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
            && let Some(pane) = self.app.clusters_state.review_panes.get_mut(&tab_id)
        {
            pane.view = next_view;
        }

        {
            let Some(pane) = self.app.clusters_state.review_panes.get(&tab_id) else {
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
        }

        match next_view {
            ResultView::Tree => self.render_tree_view(ui),
            ResultView::Graph => self.render_graph_view(ui),
        }
    }

    fn render_tree_view(&mut self, ui: &mut egui::Ui) {
        let tab_id = self.tab_id;
        let app = &mut *self.app;
        // Snapshot the data we need so we can pass `app` mutably into the
        // row renderer for `expanded`/`user_renamed`/`editing` mutations.
        let (final_leaf, final_outliers, live_top, live_children, titles, has_done) = {
            let pane = match app.clusters_state.review_panes.get(&tab_id) {
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
                        Self::render_cluster_row(ui, app, tab_id, c, &titles, &HashMap::new(), 0);
                    }
                    if !final_outliers.is_empty() {
                        ui.horizontal(|ui| {
                            ui.label("[~]");
                            ui.label("Outliers");
                            ui.label(
                                egui::RichText::new(format!("({})", final_outliers.len()))
                                    .small()
                                    .color(theme::muted()),
                            );
                        });
                        for m in final_outliers.iter().take(8) {
                            ui.label(
                                egui::RichText::new(format!(
                                    "  • {}",
                                    titles.get(m).cloned().unwrap_or_else(|| m.clone())
                                ))
                                .small()
                                .color(theme::muted()),
                            );
                        }
                        if final_outliers.len() > 8 {
                            ui.label(
                                egui::RichText::new(format!(
                                    "  … and {} more",
                                    final_outliers.len() - 8
                                ))
                                .small()
                                .color(theme::muted()),
                            );
                        }
                    }
                } else {
                    for c in &live_top {
                        Self::render_cluster_row(ui, app, tab_id, c, &titles, &live_children, 0);
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
            let pane = app.clusters_state.review_panes.get(&tab_id);
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
                crate::icons::ICONS.image(crate::icons::Icon::ChevronDown)
            } else {
                crate::icons::ICONS.image(crate::icons::Icon::ChevronRight)
            };
            if ui
                .add(egui::ImageButton::new(chevron).frame(false))
                .clicked()
            {
                let pane = app.clusters_state.review_panes.entry(tab_id).or_default();
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
                let pane = app.clusters_state.review_panes.entry(tab_id).or_default();
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
                    let pane = app.clusters_state.review_panes.entry(tab_id).or_default();
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
                    Self::render_cluster_row(
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
    /// the leaf `note_path` here is a vault-relative path from the build
    /// walk, not necessarily a `read_store`-addressable id.
    ///
    /// status: cluster-review-tab-result-graph-view
    fn render_graph_view(&mut self, ui: &mut egui::Ui) {
        let tab_id = self.tab_id;
        let app = &mut *self.app;
        let (built, live_top, live_children, has_done) = {
            let Some(pane) = app.clusters_state.review_panes.get(&tab_id) else {
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
            .clusters_state
            .review_panes
            .get(&tab_id)
            .map(|p| p.user_renamed.clone())
            .unwrap_or_default();

        let nodes = if has_done {
            let tree = built.expect("has_done implies stored result");
            Self::built_tree_to_editable_nodes(&tree, &user_renamed)
        } else {
            Self::live_to_editable_nodes(&live_top, &live_children, &user_renamed)
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
            // Live preview re-clusters in place — keep the user's pan/zoom.
            /*preserve_view=*/ true,
        );
    }

    /// Adapter (post-Done): synthesize an `EditableNode` row for every
    /// cluster in `tree.levels` + every leaf member. Mirrors the shape
    /// `node_inserts` writes the tree's `.md`, but as
    /// `EditableNode` (the graph renderer's input shape) instead of
    /// `NodeInsert`.
    ///
    /// Honors inline-renamed cluster names from the review pane's
    /// `user_renamed` map so the graph view's labels track the tree view.
    /// No policy color (none exists pre-persistence) and `churn = 0` (no
    /// staleness tint), per the spec.
    ///
    /// status: cluster-review-tab-result-graph-view
    fn built_tree_to_editable_nodes(
        tree: &BuiltClusterTree,
        user_renamed: &HashMap<String, String>,
    ) -> Vec<EditableNode> {
        let mut out: Vec<EditableNode> = Vec::new();
        if tree.levels.is_empty() {
            return out;
        }

        // Parent lookup: cluster levels 1.. carry child-cluster ids in
        // `members`; level 0 carries note ids. Match the persistence
        // builder's logic.
        let mut parent_of: HashMap<String, String> = HashMap::new();
        for level in tree.levels.iter().skip(1) {
            for node in level {
                for child in &node.members {
                    parent_of.insert(child.clone(), node.id.clone());
                }
            }
        }

        let top_level = tree.levels.len() - 1;
        let top = &tree.levels[top_level];
        let synthesized_root = top.len() != 1;
        let root_id = if synthesized_root {
            Some("root".to_string())
        } else {
            None
        };
        if synthesized_root {
            for n in top {
                parent_of.insert(n.id.clone(), "root".to_string());
            }
            out.push(EditableNode {
                id: "root".to_string(),
                parent: None,
                kind: NodeKind::Cluster,
                note_path: None,
                name: "Vault root".to_string(),
                summary: String::new(),
                user_edited_name: false,
                user_edited_summary: false,
                policy: None,
                centroid: None,
                confidence: 1.0,
                summary_membership_churn: 0,
            });
        }

        for (level_idx, level) in tree.levels.iter().enumerate() {
            for node in level {
                let parent = if level_idx == top_level && !synthesized_root {
                    None
                } else {
                    parent_of.get(&node.id).cloned()
                };
                let renamed = user_renamed.get(&node.id).cloned();
                let user_edited = renamed.is_some();
                out.push(EditableNode {
                    id: node.id.clone(),
                    parent,
                    kind: NodeKind::Cluster,
                    note_path: None,
                    name: renamed.unwrap_or_else(|| node.name.clone()),
                    summary: node.summary.clone(),
                    user_edited_name: user_edited,
                    user_edited_summary: false,
                    policy: None,
                    centroid: Some(node.centroid.clone()),
                    confidence: node.confidence,
                    summary_membership_churn: 0,
                });
            }
        }

        // Leaves under level-0 clusters.
        if let Some(leaf_level) = tree.levels.first() {
            for cluster in leaf_level {
                for note_id in &cluster.members {
                    let leaf_id = format!("leaf-{}", note_id);
                    out.push(EditableNode {
                        id: leaf_id,
                        parent: Some(cluster.id.clone()),
                        kind: NodeKind::Leaf,
                        note_path: Some(note_id.clone()),
                        name: note_id.clone(),
                        summary: String::new(),
                        user_edited_name: false,
                        user_edited_summary: false,
                        policy: None,
                        centroid: None,
                        confidence: cluster.confidence,
                        summary_membership_churn: 0,
                    });
                }
            }
        }

        // Outliers under the (real or synthesized) root.
        if !tree.outliers.is_empty()
            && let Some(rid) = root_id.as_deref().or_else(|| top.first().map(|n| n.id.as_str()))
        {
            out.push(EditableNode {
                id: "outliers".to_string(),
                parent: Some(rid.to_string()),
                kind: NodeKind::OutlierBucket,
                note_path: None,
                name: "Outliers".to_string(),
                summary: String::new(),
                user_edited_name: false,
                user_edited_summary: false,
                policy: None,
                centroid: None,
                confidence: 0.0,
                summary_membership_churn: 0,
            });
            for note_id in &tree.outliers {
                out.push(EditableNode {
                    id: format!("leaf-outlier-{}", note_id),
                    parent: Some("outliers".to_string()),
                    kind: NodeKind::Leaf,
                    note_path: Some(note_id.clone()),
                    name: note_id.clone(),
                    summary: String::new(),
                    user_edited_name: false,
                    user_edited_summary: false,
                    policy: None,
                    centroid: None,
                    confidence: 0.0,
                    summary_membership_churn: 0,
                });
            }
        }

        out
    }

    /// Adapter (mid-build live reveal): synthesize an `EditableNode` slice
    /// from the pane's incremental `live_top` + `live_pending_children`
    /// buffers. Same encoding rules as the post-Done adapter; gracefully
    /// degrades when only partial clusters are present so the graph
    /// re-renders as new clusters arrive.
    fn live_to_editable_nodes(
        live_top: &[BuiltClusterNode],
        live_children: &HashMap<String, Vec<BuiltClusterNode>>,
        user_renamed: &HashMap<String, String>,
    ) -> Vec<EditableNode> {
        let mut out: Vec<EditableNode> = Vec::new();
        if live_top.is_empty() {
            return out;
        }

        fn walk(
            node: &BuiltClusterNode,
            parent: Option<&str>,
            live_children: &HashMap<String, Vec<BuiltClusterNode>>,
            user_renamed: &HashMap<String, String>,
            out: &mut Vec<EditableNode>,
        ) {
            let renamed = user_renamed.get(&node.id).cloned();
            let user_edited = renamed.is_some();
            out.push(EditableNode {
                id: node.id.clone(),
                parent: parent.map(std::string::ToString::to_string),
                kind: NodeKind::Cluster,
                note_path: None,
                name: renamed.unwrap_or_else(|| node.name.clone()),
                summary: node.summary.clone(),
                user_edited_name: user_edited,
                user_edited_summary: false,
                policy: None,
                centroid: Some(node.centroid.clone()),
                confidence: node.confidence,
                summary_membership_churn: 0,
            });
            if let Some(children) = live_children.get(&node.id) {
                for child in children {
                    walk(child, Some(&node.id), live_children, user_renamed, out);
                }
            } else {
                for note_id in &node.members {
                    out.push(EditableNode {
                        id: format!("leaf-{}", note_id),
                        parent: Some(node.id.clone()),
                        kind: NodeKind::Leaf,
                        note_path: Some(note_id.clone()),
                        name: note_id.clone(),
                        summary: String::new(),
                        user_edited_name: false,
                        user_edited_summary: false,
                        policy: None,
                        centroid: None,
                        confidence: node.confidence,
                        summary_membership_churn: 0,
                    });
                }
            }
        }
        for n in live_top {
            walk(n, None, live_children, user_renamed, &mut out);
        }
        out
    }
}
