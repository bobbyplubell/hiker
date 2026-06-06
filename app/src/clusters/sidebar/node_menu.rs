//! The cluster-tree per-row context menu, built as a `egui_workbench::menu::Menu`
//! (status: ctxmenu-clusters). One builder, `build_cluster_node_menu`, turns
//! the precomputed per-node context (kind, current policy, the live target /
//! sibling candidate lists gathered on menu-open) into a `Menu<NodeVerb>`;
//! `tree.rs` renders it and applies the returned verb through the same paths
//! the old imperative menu used. The dynamic submenus — the Policy radio, the
//! "Move to…" / "Merge with sibling…" pickers, and the outlier-promote picker —
//! ride `Custom` entries because they render live nested widgets (selectable
//! state, hover tooltips, runtime candidate lists) the pure-data entry kinds
//! can't express.

use hiker_core::trees::types::{EditableNode, NodeKind, NodePolicy};

/// A context-menu verb picked on a cluster-tree row. The menu render records
/// one of these; the mutation runs afterwards through the same code paths the
/// old imperative menu used (inline `renaming`/`editing_summary` seeding,
/// `apply_policy`, `trees.move_node`/`promote_outlier`/`merge_siblings`, …).
pub(super) enum NodeVerb {
    /// A shared note-item base action (Open / Reveal-in-tree / Properties) for a
    /// leaf node's note, composed from [`crate::item_menu::note_item_base`] so
    /// the universal verbs live in one place (status: ctxmenu-item-base).
    Base(crate::item_menu::ItemAction),
    /// Seed the inline rename editor (clusters / outlier buckets only).
    Rename,
    /// Seed the inline edit-summary editor (clusters / outlier buckets only).
    EditSummary,
    /// Apply (or clear) a cluster policy chosen in the Policy submenu.
    SetPolicy(PolicyChoice),
    /// Re-run clustering on this subtree.
    Split,
    /// Generate a new LLM summary for this cluster.
    Summarize,
    /// Flatten one level: grandchildren move up to this node.
    MergeUp,
    /// Drop this cluster, moving its leaves to the outlier bucket.
    Drop,
    /// Send a non-outlier leaf to the outlier bucket.
    SendToOutliers,
    /// Reparent this node (or promote a leaf) under the chosen target.
    MoveTo { target_id: String },
    /// Merge this cluster with the chosen sibling cluster.
    MergeWithSibling { sibling_id: String },
    /// Collapse every expanded node in the tree.
    CollapseAll,
}

/// The Policy submenu's outcomes. `Default`/`Freeze` apply immediately;
/// `SetTag`/`SetMove` instead open the inline policy editors seeded with any
/// existing values (so the dialog reflects the current policy).
pub(super) enum PolicyChoice {
    /// Clear any policy from this cluster.
    Default,
    /// Freeze: reclustering won't touch this subtree.
    Freeze,
    /// Open the Tag-policy editor seeded with `(slug, require_review)`.
    SetTag { slug: String, require_review: bool },
    /// Open the Move-policy editor seeded with `(folder, require_review)`.
    SetMove { folder: String, require_review: bool },
}

/// A "Move to…" / outlier-promote candidate (a destination cluster or bucket).
#[derive(Clone)]
pub(super) struct MoveTarget {
    pub id: String,
    pub name: String,
    pub is_outlier_bucket: bool,
}

/// A "Merge with sibling…" candidate (a sibling cluster).
pub(super) struct SiblingTarget {
    pub id: String,
    pub name: String,
}

/// The precomputed per-node context the menu renders against — everything the
/// pure-data builder can't recompute on its own. Gathered on menu-open in
/// `tree.rs` (status: ctxmenu-build-on-open): the node's kind/policy, whether a
/// leaf sits in an outlier bucket, and the live candidate lists for the
/// dynamic submenus.
pub(super) struct MenuArgs {
    pub node: EditableNode,
    pub leaf_in_outlier_bucket: bool,
    pub move_targets: Vec<MoveTarget>,
    pub siblings: Vec<SiblingTarget>,
}

// status: ctxmenu-clusters
/// Build the cluster-tree row menu (status: ctxmenu-clusters). Pure data
/// construction; the dynamic submenus ride `Custom` entries that render the
/// same live nested `menu_button`s the old imperative menu did.
pub(super) fn build_cluster_node_menu(args: MenuArgs) -> egui_workbench::menu::Menu<NodeVerb> {
    let MenuArgs {
        node,
        leaf_in_outlier_bucket,
        move_targets,
        siblings,
    } = args;
    let is_cluster = node.kind == NodeKind::Cluster;
    let is_leaf = node.kind == NodeKind::Leaf;

    // The old menu emitted exactly one `ui.separator()` — between the Policy
    // submenu and "Split / Recluster", and only for clusters. Everything else
    // flowed without rules. To preserve that, all entries live in one section
    // except the cluster action block, which opens a second section so the
    // renderer draws that single separator. Leaves never gain a separator.
    //
    // A leaf with a backing note prepends the shared note-item base (Open /
    // Reveal-in-tree / Copy-path / Properties) in its own section ahead of the
    // leaf-specific verbs (status: ctxmenu-item-base). Cluster / outlier-bucket
    // nodes have no note path and get no base.
    let mut menu = match node.note_path.as_deref().filter(|_| is_leaf) {
        Some(note_path) => crate::item_menu::note_item_base(
            note_path,
            crate::item_menu::BaseOpts { reveal: true },
            NodeVerb::Base,
        )
        .section(),
        None => egui_workbench::menu::Menu::new(),
    };
    if !is_leaf {
        menu = menu
            .action("Rename", NodeVerb::Rename)
            .action("Edit summary…", NodeVerb::EditSummary);
    }
    if is_cluster {
        // Policy rides a `Custom` entry: its rows are `selectable_label`s with
        // live checkmarks and hover tooltips, which the pure-data toggle/action
        // kinds can't carry together.
        menu = menu
            .custom(policy_submenu(node.policy.clone()))
            // New section → the one separator the old menu drew after Policy.
            .section()
            .action("Split / Recluster", NodeVerb::Split)
            .action("Summarize (LLM)", NodeVerb::Summarize)
            .action("Merge children up", NodeVerb::MergeUp)
            .action("Drop cluster", NodeVerb::Drop);
    }
    // Leaf-only outlier moves. These stayed in the same flat group as the
    // trailing items in the old menu, so no new section here.
    if is_leaf {
        if leaf_in_outlier_bucket {
            // "Promote out of outliers…": faithfully reproduce the old flat
            // button that, on click, rendered the move-targets into the
            // closing menu. Rides `Custom` so the exact widget shape (and its
            // transient picker) is preserved. status: ctxmenu-clusters
            menu = menu.custom(promote_out_of_outliers(move_targets.clone()));
        } else {
            menu = menu.action("Send to outliers", NodeVerb::SendToOutliers);
        }
    }
    // "Move to…": a nested picker of destination clusters / buckets. (Same flat
    // group — for clusters this is the section opened after Policy; for leaves
    // it is the single root section, matching the old separator-free trailing
    // run of Move-to… / Merge-with-sibling… / Collapse-all.)
    menu = menu.custom(move_to_submenu(move_targets));
    if !is_leaf {
        // "Merge with sibling…": a nested picker of sibling clusters.
        menu = menu.custom(merge_with_sibling_submenu(siblings));
    }
    menu.action("Collapse all", NodeVerb::CollapseAll)
}

/// The Policy submenu renderer: the four `selectable_label` rows reproduced
/// from the old `policy_submenu`, returning the chosen [`NodeVerb::SetPolicy`].
fn policy_submenu(
    current: Option<NodePolicy>,
) -> impl FnOnce(&mut egui::Ui) -> Option<NodeVerb> {
    move |ui| {
        let mut verb = None;
        ui.menu_button("Policy", |ui| {
            verb = policy_rows(ui, &current);
        });
        verb
    }
}

/// The four Policy radio rows. Split out so the nested `menu_button` closure
/// stays a single mapping from a clicked `selectable_label` to a verb.
fn policy_rows(ui: &mut egui::Ui, current: &Option<NodePolicy>) -> Option<NodeVerb> {
    let is_freeze = matches!(current, Some(NodePolicy::Freeze));
    let is_tag = matches!(current, Some(NodePolicy::Tag { .. }));
    let is_move = matches!(current, Some(NodePolicy::Move { .. }));
    let mut verb = None;
    if ui
        .selectable_label(current.is_none(), "Default (no policy)")
        .on_hover_text("Clear any policy from this cluster")
        .clicked()
    {
        verb = Some(NodeVerb::SetPolicy(PolicyChoice::Default));
        ui.close();
    }
    if ui
        .selectable_label(is_freeze, "Freeze")
        .on_hover_text("Reclustering won't touch this subtree")
        .clicked()
    {
        verb = Some(NodeVerb::SetPolicy(PolicyChoice::Freeze));
        ui.close();
    }
    let (tag_slug, tag_req) = match current {
        Some(NodePolicy::Tag { slug, require_review }) => (slug.clone(), *require_review),
        _ => (String::new(), false),
    };
    if ui.selectable_label(is_tag, "Set Tag policy…").clicked() {
        verb = Some(NodeVerb::SetPolicy(PolicyChoice::SetTag {
            slug: tag_slug,
            require_review: tag_req,
        }));
        ui.close();
    }
    let (move_folder, move_req) = match current {
        Some(NodePolicy::Move { folder, require_review }) => (folder.clone(), *require_review),
        _ => (String::new(), false),
    };
    if ui.selectable_label(is_move, "Set Move policy…").clicked() {
        verb = Some(NodeVerb::SetPolicy(PolicyChoice::SetMove {
            folder: move_folder,
            require_review: move_req,
        }));
        ui.close();
    }
    verb
}

/// The "Move to…" picker (status: ctxmenu-clusters): one button per candidate
/// destination, returning [`NodeVerb::MoveTo`].
fn move_to_submenu(targets: Vec<MoveTarget>) -> impl FnOnce(&mut egui::Ui) -> Option<NodeVerb> {
    move |ui| {
        let mut verb = None;
        ui.menu_button("Move to…", |ui| {
            verb = move_target_rows(ui, &targets);
        });
        verb
    }
}

/// The flat "Promote out of outliers…" button. Faithful to the old flat
/// `ui.button` that, on click, rendered the move-targets inline and closed.
fn promote_out_of_outliers(
    targets: Vec<MoveTarget>,
) -> impl FnOnce(&mut egui::Ui) -> Option<NodeVerb> {
    move |ui| {
        let mut verb = None;
        if ui.button("Promote out of outliers…").clicked() {
            // v0: route through the Move to… picker, exactly as before.
            verb = move_target_rows(ui, &targets);
            ui.close();
        }
        verb
    }
}

/// Shared row renderer for the move-target pickers: one button per candidate,
/// glyph-prefixed by kind, yielding [`NodeVerb::MoveTo`] on click.
fn move_target_rows(ui: &mut egui::Ui, targets: &[MoveTarget]) -> Option<NodeVerb> {
    if targets.is_empty() {
        ui.label("(no valid targets)");
        return None;
    }
    let mut verb = None;
    for t in targets {
        let glyph = if t.is_outlier_bucket { "?" } else { "*" };
        if ui.button(format!("{} {}", glyph, t.name)).clicked() {
            verb = Some(NodeVerb::MoveTo {
                target_id: t.id.clone(),
            });
            ui.close();
        }
    }
    verb
}

/// The "Merge with sibling…" picker (status: ctxmenu-clusters): one button per
/// sibling cluster, returning [`NodeVerb::MergeWithSibling`].
fn merge_with_sibling_submenu(
    siblings: Vec<SiblingTarget>,
) -> impl FnOnce(&mut egui::Ui) -> Option<NodeVerb> {
    move |ui| {
        let mut verb = None;
        ui.menu_button("Merge with sibling…", |ui| {
            if siblings.is_empty() {
                ui.label("(no sibling clusters)");
            } else {
                for sib in &siblings {
                    if ui.button(format!("* {}", sib.name)).clicked() {
                        verb = Some(NodeVerb::MergeWithSibling {
                            sibling_id: sib.id.clone(),
                        });
                        ui.close();
                    }
                }
            }
        });
        verb
    }
}
