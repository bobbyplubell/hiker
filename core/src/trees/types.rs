//! Shared types for `core::trees`. Pure data — no SQL, no on-disk YAML
//! shape leaks past this module (per `trees-module-discipline`).
//!
//! Cluster trees are per-tree `.md` files at a visible vault path
//! (`{new_cluster_tree_dir}/<tree-id>.md`, default `cluster-trees/`, per
//! `trees-md-store` / `cluster-tree-visible-note`); the `Db` handle owns the
//! op-log + vault references used to read and rewrite a tree's frontmatter,
//! plus the watcher/indexer handles for the visible-note write path.
//! Frontmatter (de)serialization lives in `super::store`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use crate::cluster::{Algorithm, LeidenParams, SummarizeMode};
use crate::indexer::IndexJobTx;
use crate::oplog::OpLog;
use crate::store::Store;
use crate::vault::Vault;
use crate::watcher::Watcher;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("yaml: {0}")]
    Yaml(String),
    #[error("op-log: {0}")]
    OpLog(String),
    #[error("store: {0}")]
    Store(String),
    #[error("cluster: {0}")]
    Cluster(#[from] crate::cluster::BuildError),
    #[error("tree not found: {0}")]
    TreeNotFound(String),
    #[error("node not found: tree={tree_id} node={node_id}")]
    NodeNotFound { tree_id: String, node_id: String },
    #[error("lock poisoned")]
    Poisoned,
}

impl From<Error> for crate::errors::HikerError {
    fn from(e: Error) -> Self {
        use crate::errors::HikerError;
        match e {
            Error::TreeNotFound(_) | Error::NodeNotFound { .. } => {
                HikerError::NotFound(e.to_string())
            }
            _ => HikerError::Io(e.to_string()),
        }
    }
}

impl From<crate::errors::HikerError> for Error {
    fn from(e: crate::errors::HikerError) -> Self {
        Error::OpLog(e.to_string())
    }
}

pub type TreeId = String;
pub type NodeId = String;
pub type NoteId = String;

/// Per `cluster-editor-tree-shape`. Three kinds, all flat — children are
/// implied by `parent` in the frontmatter `nodes` list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NodeKind {
    Cluster,
    Leaf,
    OutlierBucket,
}

impl NodeKind {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            NodeKind::Cluster => "cluster",
            NodeKind::Leaf => "leaf",
            NodeKind::OutlierBucket => "outlier-bucket",
        }
    }
}

/// Per `cluster-editor-policy-types`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum NodePolicy {
    Tag {
        slug: String,
        #[serde(default)]
        require_review: bool,
    },
    Move {
        folder: String,
        #[serde(default)]
        require_review: bool,
    },
    Freeze,
}

/// In-memory editable shape per `cluster-editor-tree-shape`. Hydrated from
/// one entry in the frontmatter `nodes` list; children are looked up by
/// `parent` when the consumer needs them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditableNode {
    pub id: NodeId,
    pub parent: Option<NodeId>,
    pub kind: NodeKind,
    /// Vault-relative path of the leaf's source note. Present only on leaves.
    /// Path-as-identity: this is the single carrier the renderer and the
    /// apply/stage paths key on — no op-log doc-id is held in memory (the
    /// on-disk frontmatter still records both id + path as a double-link, but
    /// the id is not surfaced here). `#[serde(alias = "note_ref")]` keeps any
    /// older serialized tab-state loadable.
    #[serde(alias = "note_ref")]
    pub note_path: Option<String>,
    /// User-editable label. Cluster basename for leaves; LLM-proposed for
    /// clusters until the user edits it.
    pub name: String,
    /// User-editable. Empty on leaves.
    pub summary: String,
    pub user_edited_name: bool,
    pub user_edited_summary: bool,
    pub policy: Option<NodePolicy>,
    /// Cluster centroid, L2-normalized. **Not** persisted in the tree's
    /// `.md` — sourced from `index.db`'s `cluster_centroids` table
    /// (`trees-centroids-index`) and filled in by the placement classifier's
    /// caller when it needs to score. `None` when loaded from frontmatter.
    pub centroid: Option<Vec<f32>>,
    /// 0.0-1.0 from build pass; preserved through edits.
    pub confidence: f32,
    /// Per `cluster-summary-staleness-counter`.
    pub summary_membership_churn: u32,
}

/// Tree-level metadata, mirroring the prior `cluster_trees` row shape. Now
/// hydrated from the tree `.md`'s `hiker` frontmatter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeRow {
    pub id: TreeId,
    pub name: String,
    /// `one-shot` | `saved-triage` per the spec.
    pub source: String,
    /// `draft` | `applied` | `saved-as-triage` per the spec.
    pub state: String,
    /// JSON of `BuildScope` (per `clustering.md`); free-form here since the
    /// cluster module owns the shape.
    pub scope_json: String,
    /// JSON of `BuildMethod`; carries params inside.
    pub method_json: String,
    pub created_at_ms: i64,
    /// Vault rev at build time; advisory.
    pub vault_snapshot: Option<String>,
}

/// A cluster-tree that has a leaf node referencing a given note — the result of
/// the "appears in" reverse lookup ([`super::store::Db::trees_containing_note`]).
/// `path` is the tree doc's vault-relative path, for opening it.
/// status: canvas-appears-in
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeContainingHit {
    pub tree_id: TreeId,
    pub name: String,
    pub path: String,
}

/// Insert payload for a new tree. `id` is generated by the caller if they
/// want to address the tree before insert; pass `None` and we pick a ULID.
#[derive(Debug, Clone)]
pub struct TreeInsert {
    pub id: Option<TreeId>,
    pub name: String,
    pub source: String,
    pub state: String,
    pub scope_json: String,
    pub method_json: String,
    pub vault_snapshot: Option<String>,
}

/// Insert payload for a single node. `centroid` is accepted for the build /
/// split paths but is **not** written to the `.md` — the caller persists it
/// to `index.db`'s `cluster_centroids` (`trees-centroids-index`).
#[derive(Debug, Clone)]
pub struct NodeInsert {
    pub node_id: NodeId,
    pub parent_id: Option<NodeId>,
    pub kind: NodeKind,
    pub note_id: Option<NoteId>,
    pub name: String,
    pub summary: String,
    pub user_edited_name: bool,
    pub user_edited_summary: bool,
    pub policy: Option<NodePolicy>,
    pub centroid: Option<Vec<f32>>,
    pub confidence: f32,
    pub summary_membership_churn: u32,
}

/// One entry in the in-memory session undo/redo stack. Per
/// `cluster-editor-edit-history` / `cluster-editor-undo-redo`: edits ride the
/// op-log on disk, while undo/redo is an in-session concept — `args` and
/// `undo_args` are caller-shaped JSON so this module stays neutral about the
/// operation vocabulary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub seq: i64,
    pub ts_ms: i64,
    pub op: String,
    pub args_json: String,
    pub undo_args_json: String,
}

// ── Ops-framework types ───────────────────────────────────────────────
//
// `Db::split_cluster` / `Db::plan_summarize_sweep` / `Db::apply_rollup`
// form the three "Operations framework" ops per `clustering.md`'s
// ops-framework section. Each carries its own param / outcome struct.

/// Output of `Db::split_cluster`. Per `cluster-op-split`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SplitOutcome {
    pub new_clusters: Vec<NodeId>,
    pub total_levels: u8,
    /// Note ids that landed in the OUTLIER bucket at any branch of the
    /// recursive split. The caller decides whether to route these into the
    /// tree's outlier bucket or surface them to the user — `split_cluster`
    /// itself doesn't touch outlier nodes.
    pub outliers: Vec<NoteId>,
}

/// Scope discriminator for `Db::plan_summarize_sweep`. Per
/// `cluster-op-summarize-sweep`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum SummarizeScope {
    /// Every cluster node in the (sub)tree.
    All,
    /// Nodes where `summary_membership_churn > 0 OR summary is empty OR name
    /// is empty`.
    StaleOrUnfilled,
    /// The listed ids only; missing ids are dropped.
    Subset { ids: Vec<NodeId> },
}

/// Params for `Db::plan_summarize_sweep`. Per `cluster-op-summarize-sweep`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SummarizeParams {
    pub scope: SummarizeScope,
    /// Optional subtree filter — when set, only nodes under (or equal to)
    /// `subtree_root` are considered.
    #[serde(default)]
    pub subtree_root: Option<NodeId>,
    /// Honored only when `subtree_root` is set. Default `true`.
    #[serde(default = "default_summarize_recursive")]
    pub recursive: bool,
    /// Carried through to the per-cluster `RaptorSummarize` task.
    #[serde(default)]
    pub summarize_mode: SummarizeMode,
    /// Default `false` — user-edited rows are preserved unless this flag is
    /// explicitly opted-into.
    #[serde(default)]
    pub overwrite_user_edited: bool,
}

const fn default_summarize_recursive() -> bool {
    true
}

/// Submission plan returned by `Db::plan_summarize_sweep`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SummarizePlan {
    pub tree_id: TreeId,
    /// One of `"all"`, `"stale-or-unfilled"`, `"subset"`.
    pub scope_kind: String,
    /// Cluster node ids in submission order (deepest-first).
    pub enqueued: Vec<NodeId>,
    pub skipped_user_edited: Vec<NodeId>,
    pub skipped_fresh: Vec<NodeId>,
}

/// Params for `Db::apply_rollup`. Per `cluster-op-rollup`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollupParams {
    pub input_node_ids: Vec<NodeId>,
    #[serde(default)]
    pub algorithm: Algorithm,
    #[serde(default)]
    pub leiden: LeidenParams,
    #[serde(default = "default_rollup_min_cluster_size")]
    pub min_cluster_size: u32,
    #[serde(default)]
    pub new_layer_name_pattern: Option<String>,
}

const fn default_rollup_min_cluster_size() -> u32 {
    2
}

/// Per-input shape produced by `Db::validate_rollup_inputs` and consumed by
/// `Db::apply_rollup`.
#[derive(Debug, Clone)]
pub struct RollupInput {
    pub node_id: NodeId,
    pub summary: String,
    pub prior_parent: Option<NodeId>,
}

/// Output of `Db::apply_rollup`. Per `cluster-op-rollup`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum RollupOutcome {
    Inserted { new_parent_ids: Vec<NodeId> },
    Refused { reason: &'static str },
}

/// Owner of the per-tree `.md` files at the visible `new_cluster_tree_dir`
/// (default `cluster-trees/`, `trees-md-store` / `cluster-tree-visible-note`).
/// Reads and rewrites a tree's frontmatter through the
/// op-log working layer; tree edits land as `SetFrontmatter` ops
/// (`trees-edit-setfrontmatter`). The undo/redo session log
/// (`cluster-editor-undo-redo`) is in-memory and per-process — it does not
/// persist across restarts (cross-session revert rides the tree doc's
/// version history instead).
pub struct Db {
    pub(super) oplog: Arc<OpLog>,
    pub(super) vault: Arc<Vault>,
    /// Dedicated `index.db` connection for the derived `cluster_centroids`
    /// table (`trees-centroids-index`). Separate from the app's shared
    /// read-store so hydrating a tree never contends with it on the UI
    /// thread. Centroids are written on node insert and read back on load.
    pub(super) centroids: Mutex<Store>,
    /// In-memory session undo log, keyed by tree id. Replaces the retired
    /// `cluster_tree_history` table (`cluster-editor-edit-history`).
    pub(super) history: Mutex<HashMap<TreeId, Vec<HistoryEntry>>>,
    /// Watcher + indexer handles for the visible-note write path
    /// (`cluster-tree-visible-note`): a tree save suppresses the watcher and
    /// enqueues an explicit `Upsert` so the new file is queryable at once,
    /// the same discipline trail-docs and presets use. Wired after the
    /// indexer/watcher start (they don't exist at `Db::new` time), so both
    /// are `OnceLock` and the write path degrades to the ambient watcher →
    /// indexer route until they're set.
    pub(super) watcher: OnceLock<Arc<Watcher>>,
    pub(super) index_jobs: OnceLock<IndexJobTx>,
    /// Default directory for new tree `.md` files (`new_cluster_tree_dir`).
    /// Settable from config after construction; defaults to `cluster-trees/`.
    pub(super) new_tree_dir: Mutex<String>,
    /// In-process tree-id → vault-relative-path cache. The frontmatter query
    /// (`path_for_tree`) only finds a tree once it's indexed; a freshly
    /// `insert_tree`d tree must be `load`able *immediately* (the create flow
    /// does `insert_tree` then `insert_nodes`), before the indexer has run.
    /// Every insert / load / save records the path here so resolution never
    /// waits on index latency; the query is the fallback for trees this
    /// process hasn't touched yet (discovered, sync-arrived, hand-typed).
    pub(super) id_paths: Mutex<HashMap<TreeId, String>>,
}

#[cfg(test)]
mod error_mapping_tests {
    use super::*;
    use crate::errors::HikerError;

    #[test]
    fn structured_tree_errors_keep_their_shape_through_hikererror() {
        // A real missing-tree/node maps to NotFound instead of being
        // flattened to Io (the bug this guards).
        assert!(matches!(
            HikerError::from(Error::TreeNotFound("t1".into())),
            HikerError::NotFound(_)
        ));
        assert!(matches!(
            HikerError::from(Error::NodeNotFound {
                tree_id: "t1".into(),
                node_id: "n1".into(),
            }),
            HikerError::NotFound(_)
        ));
        // A clustering compute failure is io-shaped to the caller, not
        // not-found — so it can never masquerade as "tree not found".
        assert!(matches!(
            HikerError::from(Error::Cluster(crate::cluster::BuildError::Compute(
                "boom".into()
            ))),
            HikerError::Io(_)
        ));
    }
}
