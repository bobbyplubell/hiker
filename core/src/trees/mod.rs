//! Cluster-tree storage. See `docs/cluster-editor.md` §"Tree storage:
//! `trees.db`" and `docs/clustering.md` for the full spec.
//!
//! `core::trees` owns `vault/.hiker/trees.db` — a SQLite database with
//! three tables (`cluster_trees`, `cluster_nodes`, `cluster_tree_history`).
//! Mirrors the module-discipline pattern used by `core::store`,
//! `core::staging`, and `core::changes`: every rusqlite import lives in
//! the `storage` submodule, callers consume plain Rust types only. Pre-1.0
//! schema policy is delete-on-bump (no migration code).
//!
//! Submodule layout (per `trees-module-discipline`):
//!
//! - `types`         — public DTOs (`Trees`, `EditableNode`, `NodeKind`, …)
//! - `storage`       — rusqlite + schema + CRUD + SQL helpers (the **only**
//!                      submodule that imports `rusqlite::params` /
//!                      `OptionalExtension` / `Connection`)
//! - `history`       — append/pop/read history + `record_*` helpers
//! - `ops::edit`     — `rename` / `set_summary` / `set_policy` /
//!                      `auto_set_name_summary`
//! - `ops::move_node`— `move_node` / `reparent_many` / `promote_outlier`
//! - `ops::merge`    — `merge_siblings` / `merge_children_up`
//! - `ops::drop`     — `drop_cluster`
//! - `ops::folder_rename` — `update_for_folder_rename`
//! - `ops::split`    — `split_cluster` + recursive helper
//! - `ops::summarize`— `plan_summarize_sweep`
//! - `ops::rollup`   — `validate_rollup_inputs` + `apply_rollup`
//!
//! status: trees-db
//! status: trees-db-schema
//! status: trees-module-discipline
//! status: cluster-editor-tree-shape
//! status: cluster-editor-edit-history

mod history;
mod ops;
mod storage;
mod types;

pub use types::{
    EditableNode, HistoryEntry, NodeId, NodeInsert, NodeKind, NodePolicy, NoteId, RollupInput,
    RollupOutcome, RollupParams, SplitOutcome, SummarizeParams, SummarizePlan, SummarizeScope,
    TreeId, TreeInsert, TreeRow, Trees, TreesError, SCHEMA_VERSION,
};

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn open_tmp() -> (TempDir, Trees) {
        let dir = TempDir::new().unwrap();
        let trees = Trees::open(dir.path()).unwrap();
        (dir, trees)
    }

    #[test]
    fn insert_and_get_tree() {
        let (_d, trees) = open_tmp();
        let id = trees
            .insert_tree(TreeInsert {
                id: None,
                name: "test".into(),
                source: "one-shot".into(),
                state: "draft".into(),
                scope_json: "{\"kind\":\"vault\"}".into(),
                method_json: "{\"kind\":\"cluster\"}".into(),
                vault_snapshot: None,
            })
            .unwrap();
        let row = trees.get_tree(&id).unwrap().expect("tree exists");
        assert_eq!(row.name, "test");
        assert_eq!(row.state, "draft");
    }

    #[test]
    fn insert_nodes_and_hydrate() {
        let (_d, trees) = open_tmp();
        let tree_id = trees
            .insert_tree(TreeInsert {
                id: Some("t1".into()),
                name: "t".into(),
                source: "one-shot".into(),
                state: "draft".into(),
                scope_json: "{}".into(),
                method_json: "{}".into(),
                vault_snapshot: None,
            })
            .unwrap();
        trees
            .insert_nodes(
                &tree_id,
                &[
                    NodeInsert {
                        node_id: "root".into(),
                        parent_id: None,
                        kind: NodeKind::Cluster,
                        note_id: None,
                        name: "Vault root".into(),
                        summary: "".into(),
                        user_edited_name: false,
                        user_edited_summary: false,
                        policy: None,
                        centroid: Some(vec![0.5, 0.5, 0.5]),
                        confidence: 1.0,
                        summary_membership_churn: 0,
                    },
                    NodeInsert {
                        node_id: "leaf1".into(),
                        parent_id: Some("root".into()),
                        kind: NodeKind::Leaf,
                        note_id: Some("note-a".into()),
                        name: "note-a".into(),
                        summary: "".into(),
                        user_edited_name: false,
                        user_edited_summary: false,
                        policy: None,
                        centroid: None,
                        confidence: 1.0,
                        summary_membership_churn: 0,
                    },
                ],
            )
            .unwrap();
        let root = trees.get_node(&tree_id, "root").unwrap().unwrap();
        assert_eq!(root.kind, NodeKind::Cluster);
        assert_eq!(root.centroid.as_ref().unwrap().len(), 3);
        let kids = trees.children_of(&tree_id, Some("root")).unwrap();
        assert_eq!(kids.len(), 1);
        assert_eq!(kids[0].id, "leaf1");
        assert_eq!(kids[0].note_ref.as_deref(), Some("note-a"));
    }

    #[test]
    fn history_records_edits() {
        let (_d, trees) = open_tmp();
        let tree_id = trees
            .insert_tree(TreeInsert {
                id: Some("t2".into()),
                name: "t".into(),
                source: "one-shot".into(),
                state: "draft".into(),
                scope_json: "{}".into(),
                method_json: "{}".into(),
                vault_snapshot: None,
            })
            .unwrap();
        trees
            .insert_nodes(
                &tree_id,
                &[NodeInsert {
                    node_id: "n1".into(),
                    parent_id: None,
                    kind: NodeKind::Cluster,
                    note_id: None,
                    name: "old".into(),
                    summary: "old-summary".into(),
                    user_edited_name: false,
                    user_edited_summary: false,
                    policy: None,
                    centroid: None,
                    confidence: 0.8,
                    summary_membership_churn: 0,
                }],
            )
            .unwrap();
        trees.rename(&tree_id, "n1", "new").unwrap();
        trees
            .set_policy(
                &tree_id,
                "n1",
                Some(NodePolicy::Tag {
                    slug: "research".into(),
                    require_review: false,
                }),
            )
            .unwrap();
        let h = trees.history(&tree_id).unwrap();
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].op, "rename");
        assert_eq!(h[1].op, "set-policy");
        let n = trees.get_node(&tree_id, "n1").unwrap().unwrap();
        assert_eq!(n.name, "new");
        assert!(n.user_edited_name);
        assert!(matches!(n.policy, Some(NodePolicy::Tag { .. })));
    }

    #[test]
    fn churn_bubbles_to_ancestors() {
        let (_d, trees) = open_tmp();
        let tid = trees
            .insert_tree(TreeInsert {
                id: Some("t3".into()),
                name: "t".into(),
                source: "one-shot".into(),
                state: "draft".into(),
                scope_json: "{}".into(),
                method_json: "{}".into(),
                vault_snapshot: None,
            })
            .unwrap();
        trees
            .insert_nodes(
                &tid,
                &[
                    NodeInsert {
                        node_id: "r".into(),
                        parent_id: None,
                        kind: NodeKind::Cluster,
                        note_id: None,
                        name: "r".into(),
                        summary: "".into(),
                        user_edited_name: false,
                        user_edited_summary: false,
                        policy: None,
                        centroid: None,
                        confidence: 1.0,
                        summary_membership_churn: 0,
                    },
                    NodeInsert {
                        node_id: "c".into(),
                        parent_id: Some("r".into()),
                        kind: NodeKind::Cluster,
                        note_id: None,
                        name: "c".into(),
                        summary: "".into(),
                        user_edited_name: false,
                        user_edited_summary: false,
                        policy: None,
                        centroid: None,
                        confidence: 1.0,
                        summary_membership_churn: 0,
                    },
                    NodeInsert {
                        node_id: "l".into(),
                        parent_id: Some("c".into()),
                        kind: NodeKind::Leaf,
                        note_id: Some("note".into()),
                        name: "leaf".into(),
                        summary: "".into(),
                        user_edited_name: false,
                        user_edited_summary: false,
                        policy: None,
                        centroid: None,
                        confidence: 1.0,
                        summary_membership_churn: 0,
                    },
                ],
            )
            .unwrap();
        trees.bump_churn_chain(&tid, "l", 1).unwrap();
        // The leaf, its parent, and the root all increment.
        let leaf = trees.get_node(&tid, "l").unwrap().unwrap();
        let mid = trees.get_node(&tid, "c").unwrap().unwrap();
        let root = trees.get_node(&tid, "r").unwrap().unwrap();
        assert_eq!(leaf.summary_membership_churn, 1);
        assert_eq!(mid.summary_membership_churn, 1);
        assert_eq!(root.summary_membership_churn, 1);
        trees.reset_churn(&tid, "c").unwrap();
        let mid = trees.get_node(&tid, "c").unwrap().unwrap();
        assert_eq!(mid.summary_membership_churn, 0);
    }

    // status: cluster-editor-undo-redo
    //
    // Round-trips the reshape ops: forward → snapshot state → simulate
    // undo via the recorded `undo_args` → re-run forward (redo) →
    // confirm the post-redo state matches the post-forward state.
    //
    // The actual undo machinery lives in `ui/src-tauri/src/lib.rs`
    // (Tauri command surface); here we exercise the underlying invariant
    // that the forward ops are idempotent against existing-IDs and that
    // the recorded `args_json` carries enough info to replay.
    #[test]
    fn merge_siblings_redo_roundtrip() {
        let (_d, trees) = open_tmp();
        let tid = trees
            .insert_tree(TreeInsert {
                id: Some("trip".into()),
                name: "t".into(),
                source: "one-shot".into(),
                state: "draft".into(),
                scope_json: "{}".into(),
                method_json: "{}".into(),
                vault_snapshot: None,
            })
            .unwrap();
        let mk = |id: &str, parent: Option<&str>, kind: NodeKind| NodeInsert {
            node_id: id.into(),
            parent_id: parent.map(|s| s.into()),
            kind,
            note_id: None,
            name: id.into(),
            summary: String::new(),
            user_edited_name: false,
            user_edited_summary: false,
            policy: None,
            centroid: None,
            confidence: 1.0,
            summary_membership_churn: 0,
        };
        trees
            .insert_nodes(
                &tid,
                &[
                    mk("root", None, NodeKind::Cluster),
                    mk("a", Some("root"), NodeKind::Cluster),
                    mk("b", Some("root"), NodeKind::Cluster),
                    mk("a-child", Some("a"), NodeKind::Cluster),
                    mk("b-child", Some("b"), NodeKind::Cluster),
                ],
            )
            .unwrap();
        // Forward merge: a absorbs b.
        let survivor = trees
            .merge_siblings(&tid, &["a".into(), "b".into()])
            .unwrap();
        assert_eq!(survivor, "a");
        assert!(trees.get_node(&tid, "b").unwrap().is_none());
        // a should now own a-child and b-child.
        let kids = trees.children_of(&tid, Some("a")).unwrap();
        let mut k_ids: Vec<String> = kids.iter().map(|n| n.id.clone()).collect();
        k_ids.sort();
        assert_eq!(k_ids, vec!["a-child", "b-child"]);

        // Simulate undo: read undo_args and restore.
        let h = trees.history(&tid).unwrap();
        let entry = h.last().cloned().unwrap();
        assert_eq!(entry.op, "merge-siblings");
        let _undo: serde_json::Value = serde_json::from_str(&entry.undo_args_json).unwrap();
        // Restore the absorbed node "b".
        trees
            .insert_single_node(
                &tid,
                NodeInsert {
                    node_id: "b".into(),
                    parent_id: Some("root".into()),
                    kind: NodeKind::Cluster,
                    note_id: None,
                    name: "b".into(),
                    summary: String::new(),
                    user_edited_name: false,
                    user_edited_summary: false,
                    policy: None,
                    centroid: None,
                    confidence: 1.0,
                    summary_membership_churn: 0,
                },
            )
            .unwrap();
        // Re-parent b-child back under b.
        trees
            .reparent_many(&tid, &[("b-child".into(), Some("b".into()))])
            .unwrap();
        // Confirm state is restored.
        assert!(trees.get_node(&tid, "b").unwrap().is_some());
        let a_kids = trees.children_of(&tid, Some("a")).unwrap();
        assert_eq!(a_kids.len(), 1);
        assert_eq!(a_kids[0].id, "a-child");

        // Redo: re-run the forward op against the args from history.
        let args: serde_json::Value = serde_json::from_str(&entry.args_json).unwrap();
        let survivor = args.get("survivor").and_then(|v| v.as_str()).unwrap().to_string();
        let absorbed: Vec<String> = args
            .get("absorbed")
            .and_then(|v| v.as_array())
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        let mut redo_ids = vec![survivor];
        redo_ids.extend(absorbed);
        trees.merge_siblings(&tid, &redo_ids).unwrap();
        // Post-redo state should match post-forward.
        assert!(trees.get_node(&tid, "b").unwrap().is_none());
        let kids = trees.children_of(&tid, Some("a")).unwrap();
        let mut k_ids: Vec<String> = kids.iter().map(|n| n.id.clone()).collect();
        k_ids.sort();
        assert_eq!(k_ids, vec!["a-child", "b-child"]);
    }

    #[test]
    fn split_record_snapshots_new_clusters_for_redo() {
        // Confirms `record_split` persists enough state on `args_json`
        // to replay a split without re-running HDBSCAN. The redo path
        // in `ui/src-tauri` reads `new_clusters` + `leaf_moves` and
        // restores via `insert_single_node` + `reparent_many`.
        let (_d, trees) = open_tmp();
        let tid = trees
            .insert_tree(TreeInsert {
                id: Some("split-trip".into()),
                name: "t".into(),
                source: "one-shot".into(),
                state: "draft".into(),
                scope_json: "{}".into(),
                method_json: "{}".into(),
                vault_snapshot: None,
            })
            .unwrap();
        let new_clusters = vec![
            serde_json::json!({
                "node_id": "split-x-0",
                "parent_id": "x",
                "kind": "cluster",
                "name": "alpha / beta",
                "summary": "",
                "user_edited_name": false,
                "user_edited_summary": false,
                "policy": null,
                "confidence": 0.5,
                "summary_membership_churn": 0,
            }),
            serde_json::json!({
                "node_id": "split-x-1",
                "parent_id": "x",
                "kind": "cluster",
                "name": "gamma / delta",
                "summary": "",
                "user_edited_name": false,
                "user_edited_summary": false,
                "policy": null,
                "confidence": 0.5,
                "summary_membership_churn": 0,
            }),
        ];
        let leaf_moves: Vec<(String, Option<String>)> = vec![
            ("leaf-1".into(), Some("split-x-0".into())),
            ("leaf-2".into(), Some("split-x-1".into())),
        ];
        trees
            .record_split(&tid, "x", &new_clusters, &leaf_moves)
            .unwrap();
        let h = trees.history(&tid).unwrap();
        let entry = h.last().unwrap();
        let args: serde_json::Value = serde_json::from_str(&entry.args_json).unwrap();
        let nc = args.get("new_clusters").and_then(|v| v.as_array()).unwrap();
        assert_eq!(nc.len(), 2);
        assert_eq!(
            nc[0].get("node_id").and_then(|v| v.as_str()),
            Some("split-x-0"),
        );
        assert_eq!(
            nc[0].get("name").and_then(|v| v.as_str()),
            Some("alpha / beta"),
        );
        let lm = args.get("leaf_moves").and_then(|v| v.as_array()).unwrap();
        assert_eq!(lm.len(), 2);
    }

    // status: cluster-summary-staleness-counter
    // status: cluster-build-from-folders-summary-staleness
    #[test]
    fn move_node_bumps_churn_on_both_chains() {
        let (_d, trees) = open_tmp();
        let tid = trees
            .insert_tree(TreeInsert {
                id: Some("ct".into()),
                name: "t".into(),
                source: "one-shot".into(),
                state: "draft".into(),
                scope_json: "{}".into(),
                method_json: "{}".into(),
                vault_snapshot: None,
            })
            .unwrap();
        let mk = |id: &str, parent: Option<&str>, kind: NodeKind| NodeInsert {
            node_id: id.into(),
            parent_id: parent.map(|s| s.into()),
            kind,
            note_id: None,
            name: id.into(),
            summary: String::new(),
            user_edited_name: false,
            user_edited_summary: false,
            policy: None,
            centroid: None,
            confidence: 1.0,
            summary_membership_churn: 0,
        };
        trees
            .insert_nodes(
                &tid,
                &[
                    mk("root", None, NodeKind::Cluster),
                    mk("a", Some("root"), NodeKind::Cluster),
                    mk("b", Some("root"), NodeKind::Cluster),
                    NodeInsert {
                        node_id: "leaf-x".into(),
                        parent_id: Some("a".into()),
                        kind: NodeKind::Leaf,
                        note_id: Some("note-x".into()),
                        name: "x".into(),
                        summary: String::new(),
                        user_edited_name: false,
                        user_edited_summary: false,
                        policy: None,
                        centroid: None,
                        confidence: 1.0,
                        summary_membership_churn: 0,
                    },
                ],
            )
            .unwrap();
        trees.move_node(&tid, "leaf-x", Some("b")).unwrap();
        let a = trees.get_node(&tid, "a").unwrap().unwrap();
        let b = trees.get_node(&tid, "b").unwrap().unwrap();
        let root = trees.get_node(&tid, "root").unwrap().unwrap();
        // Old direct parent and new direct parent each get +1: their
        // subtree leaf set changed.
        assert_eq!(a.summary_membership_churn, 1);
        assert_eq!(b.summary_membership_churn, 1);
        // Root is the LCA of `a` and `b` — its subtree's leaf set is
        // unchanged by an internal move, so it should NOT bump.
        assert_eq!(root.summary_membership_churn, 0);
        // Regenerate resets just one node.
        trees.reset_churn(&tid, "a").unwrap();
        let a = trees.get_node(&tid, "a").unwrap().unwrap();
        assert_eq!(a.summary_membership_churn, 0);
    }

    // status: cluster-summary-staleness-counter
    //
    // Splitting a cluster `p` into sub-clusters and reparenting leaves
    // is a purely internal reshape of `p`'s subtree, but each new
    // sub-cluster legitimately gains a leaf — so `reparent_many` bumps
    // churn on each destination sub-cluster, but NOT on the LCA (`p`)
    // or anything above it. Wrapping ops (e.g. `cluster_op_split`)
    // reset churn on the freshly-inserted sub-clusters, since their
    // summaries are generated against the moved-in leaves.
    #[test]
    fn split_within_subtree_bumps_only_new_subclusters() {
        let (_d, trees) = open_tmp();
        let tid = trees
            .insert_tree(TreeInsert {
                id: Some("split-churn".into()),
                name: "t".into(),
                source: "one-shot".into(),
                state: "draft".into(),
                scope_json: "{}".into(),
                method_json: "{}".into(),
                vault_snapshot: None,
            })
            .unwrap();
        let cluster = |id: &str, parent: Option<&str>| NodeInsert {
            node_id: id.into(),
            parent_id: parent.map(|s| s.into()),
            kind: NodeKind::Cluster,
            note_id: None,
            name: id.into(),
            summary: String::new(),
            user_edited_name: false,
            user_edited_summary: false,
            policy: None,
            centroid: None,
            confidence: 1.0,
            summary_membership_churn: 0,
        };
        let leaf = |id: &str, parent: &str| NodeInsert {
            node_id: id.into(),
            parent_id: Some(parent.into()),
            kind: NodeKind::Leaf,
            note_id: Some(format!("note-{id}")),
            name: id.into(),
            summary: String::new(),
            user_edited_name: false,
            user_edited_summary: false,
            policy: None,
            centroid: None,
            confidence: 1.0,
            summary_membership_churn: 0,
        };
        trees
            .insert_nodes(
                &tid,
                &[
                    cluster("root", None),
                    cluster("p", Some("root")),
                    cluster("sub-0", Some("p")),
                    cluster("sub-1", Some("p")),
                    leaf("leaf-a", "p"),
                    leaf("leaf-b", "p"),
                ],
            )
            .unwrap();
        // Reparent both leaves out of `p` and into the new sub-clusters.
        trees
            .reparent_many(
                &tid,
                &[
                    ("leaf-a".into(), Some("sub-0".into())),
                    ("leaf-b".into(), Some("sub-1".into())),
                ],
            )
            .unwrap();
        let p = trees.get_node(&tid, "p").unwrap().unwrap();
        let sub0 = trees.get_node(&tid, "sub-0").unwrap().unwrap();
        let sub1 = trees.get_node(&tid, "sub-1").unwrap().unwrap();
        let root = trees.get_node(&tid, "root").unwrap().unwrap();
        // LCA of each leaf move is `p`. The old-parent walk from `p`
        // finds itself in the new-parent ancestor set and stops without
        // bumping; the new-parent walk from each sub-cluster bumps the
        // sub-cluster and stops at `p`. Above the LCA: no visits.
        assert_eq!(p.summary_membership_churn, 0);
        assert_eq!(sub0.summary_membership_churn, 1);
        assert_eq!(sub1.summary_membership_churn, 1);
        assert_eq!(root.summary_membership_churn, 0);
    }

    // status: cluster-build-from-folders-live-update
    #[test]
    fn folder_rename_relocates_leaf_and_drops_empty_folder() {
        let (_d, trees) = open_tmp();
        let tid = trees
            .insert_tree(TreeInsert {
                id: Some("ff".into()),
                name: "ff".into(),
                source: "saved-triage".into(),
                state: "saved-as-triage".into(),
                scope_json: "{}".into(),
                method_json: "{\"kind\":\"from-folders\"}".into(),
                vault_snapshot: None,
            })
            .unwrap();
        let leaf = NodeInsert {
            node_id: "leaf-foo".into(),
            parent_id: Some("f-inbox".into()),
            kind: NodeKind::Leaf,
            note_id: Some("note-foo".into()),
            name: "foo".into(),
            summary: String::new(),
            user_edited_name: false,
            user_edited_summary: false,
            policy: None,
            centroid: None,
            confidence: 1.0,
            summary_membership_churn: 0,
        };
        let mkc = |id: &str, parent: Option<&str>| NodeInsert {
            node_id: id.into(),
            parent_id: parent.map(|s| s.into()),
            kind: NodeKind::Cluster,
            note_id: None,
            name: id.into(),
            summary: String::new(),
            user_edited_name: false,
            user_edited_summary: false,
            policy: None,
            centroid: None,
            confidence: 1.0,
            summary_membership_churn: 0,
        };
        trees
            .insert_nodes(
                &tid,
                &[mkc("root", None), mkc("f-inbox", Some("root")), leaf],
            )
            .unwrap();
        let moved = trees
            .update_for_folder_rename(&tid, "note-foo", "research")
            .unwrap();
        assert!(moved);
        // New folder cluster auto-created.
        let dest = trees.get_node(&tid, "f-research").unwrap().unwrap();
        assert!(matches!(dest.kind, NodeKind::Cluster));
        // Old folder dropped (no policy, no children left).
        let gone = trees.get_node(&tid, "f-inbox").unwrap();
        assert!(gone.is_none(), "empty folder cluster should be GC'd");
        // Leaf re-parented.
        let leaf_now = trees.get_node(&tid, "leaf-foo").unwrap().unwrap();
        assert_eq!(leaf_now.parent.as_deref(), Some("f-research"));
    }

    #[test]
    fn folder_rename_keeps_policied_empty_folder() {
        let (_d, trees) = open_tmp();
        let tid = trees
            .insert_tree(TreeInsert {
                id: Some("ff2".into()),
                name: "ff2".into(),
                source: "saved-triage".into(),
                state: "saved-as-triage".into(),
                scope_json: "{}".into(),
                method_json: "{}".into(),
                vault_snapshot: None,
            })
            .unwrap();
        let leaf = NodeInsert {
            node_id: "leaf-x".into(),
            parent_id: Some("f-inbox".into()),
            kind: NodeKind::Leaf,
            note_id: Some("note-x".into()),
            name: "x".into(),
            summary: String::new(),
            user_edited_name: false,
            user_edited_summary: false,
            policy: None,
            centroid: None,
            confidence: 1.0,
            summary_membership_churn: 0,
        };
        let mkc = |id: &str, parent: Option<&str>, policy: Option<NodePolicy>| NodeInsert {
            node_id: id.into(),
            parent_id: parent.map(|s| s.into()),
            kind: NodeKind::Cluster,
            note_id: None,
            name: id.into(),
            summary: String::new(),
            user_edited_name: false,
            user_edited_summary: false,
            policy,
            centroid: None,
            confidence: 1.0,
            summary_membership_churn: 0,
        };
        trees
            .insert_nodes(
                &tid,
                &[
                    mkc("root", None, None),
                    mkc(
                        "f-inbox",
                        Some("root"),
                        Some(NodePolicy::Tag {
                            slug: "inbox".into(),
                            require_review: false,
                        }),
                    ),
                    leaf,
                ],
            )
            .unwrap();
        trees
            .update_for_folder_rename(&tid, "note-x", "research")
            .unwrap();
        // Empty folder cluster with a policy must survive.
        let kept = trees.get_node(&tid, "f-inbox").unwrap();
        assert!(
            kept.is_some(),
            "policied empty folder should survive"
        );
    }
}
