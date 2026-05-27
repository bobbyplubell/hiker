//! Per-tree `.md` store for `core::trees` (`trees-md-store`).
//!
//! Each cluster tree is one markdown document at
//! `vault/.hiker/trees/<tree-id>.md`. The full structure lives in the
//! `hiker` frontmatter (`trees-md-frontmatter`); the body is a fixed stub
//! (the human render is produced on demand by the cluster editor, not
//! persisted). Edits load the tree, mutate the in-memory [`TreeDoc`], and
//! rewrite **only the frontmatter fence** through the op-log working layer —
//! so each edit lands as a `SetFrontmatter` op (`trees-edit-setfrontmatter`)
//! and the body bytes never move.
//!
//! No rusqlite, no schema-version file, no migration code — the frontmatter
//! is self-describing and non-`hiker` keys round-trip untouched.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_yml::Value as Yaml;

use super::types::{
    Db, EditableNode, Error, NodeInsert, NodeKind, NodePolicy, TreeInsert, TreeRow,
};
use crate::oplog::OpLog;
use crate::vault::Vault;

// ── on-disk frontmatter shape ────────────────────────────────────────────

/// The `hiker:` block of a cluster-tree `.md`. Serde-(de)serialized to/from
/// the frontmatter; the body is handled separately.
#[derive(Debug, Serialize, Deserialize)]
struct TreeFm {
    kind: String,
    id: String,
    name: String,
    source: String,
    state: String,
    #[serde(default)]
    scope: Yaml,
    #[serde(default)]
    method: Yaml,
    created_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    vault_snapshot: Option<String>,
    #[serde(default)]
    nodes: Vec<NodeFm>,
}

#[derive(Debug, Serialize, Deserialize)]
struct NodeFm {
    id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent: Option<String>,
    kind: NodeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    note: Option<NoteRefFm>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    summary: String,
    #[serde(default, skip_serializing_if = "is_false")]
    user_edited_name: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    user_edited_summary: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    policy: Option<NodePolicy>,
    #[serde(default)]
    confidence: f32,
    #[serde(default, skip_serializing_if = "is_zero")]
    churn: u32,
}

/// Double-link to a leaf's source note (`trail-double-link-references`): the
/// ULID is canonical, the rel-path keeps the file legible externally.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct NoteRefFm {
    id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    path: String,
}

// By-ref signatures are required by serde's `skip_serializing_if`
// (`fn(&T) -> bool`), hence the allow; the bodies are const-evaluable.
#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_false(b: &bool) -> bool {
    !*b
}
#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// Fixed body stub. The human-readable render is produced on demand by the
/// cluster editor (`cluster-editor-markdown-view-toggle`); persisting a
/// render that changed per edit would make every edit a `Replace` rather
/// than a `SetFrontmatter` op, so the on-disk body stays constant.
const BODY_STUB: &str = "<!-- Cluster tree. The structure lives in the `hiker:` frontmatter above; \
open this tree in the cluster editor to view and edit it. -->\n";

// ── in-memory working model ────────────────────────────────────────────

/// A loaded tree: metadata + the flat node list + the bytes we must
/// preserve verbatim on save (body + any non-`hiker` frontmatter keys + the
/// leaves' recorded paths).
pub(super) struct TreeDoc {
    pub meta: TreeRow,
    pub nodes: Vec<EditableNode>,
    /// Body bytes, preserved across edits.
    body: String,
    /// Non-`hiker` frontmatter keys, preserved on round-trip.
    extra_fm: serde_yml::Mapping,
    /// node_id → recorded note rel-path, so the double-link survives a
    /// round-trip even though `EditableNode` only carries the id half.
    note_paths: HashMap<String, String>,
}

impl TreeDoc {
    fn position(&self, node_id: &str) -> Option<usize> {
        self.nodes.iter().position(|n| n.id == node_id)
    }

    pub(super) fn get(&self, node_id: &str) -> Option<&EditableNode> {
        self.nodes.iter().find(|n| n.id == node_id)
    }

    pub(super) fn get_mut(&mut self, node_id: &str) -> Option<&mut EditableNode> {
        self.nodes.iter_mut().find(|n| n.id == node_id)
    }

    /// Direct children of `parent` (`None` = root), in stored order.
    pub(super) fn children(&self, parent: Option<&str>) -> Vec<EditableNode> {
        self.nodes
            .iter()
            .filter(|n| n.parent.as_deref() == parent)
            .cloned()
            .collect()
    }

    pub(super) fn child_ids(&self, parent: &str) -> Vec<String> {
        self.nodes
            .iter()
            .filter(|n| n.parent.as_deref() == Some(parent))
            .map(|n| n.id.clone())
            .collect()
    }

    pub(super) fn set_parent(&mut self, node_id: &str, new_parent: Option<&str>) {
        if let Some(n) = self.get_mut(node_id) {
            n.parent = new_parent.map(str::to_string);
        }
    }

    pub(super) fn insert(&mut self, n: EditableNode) {
        if let Some(path) = &n.note_ref {
            self.note_paths
                .entry(n.id.clone())
                .or_insert_with(|| path.clone());
        }
        if let Some(idx) = self.position(&n.id) {
            self.nodes[idx] = n;
        } else {
            self.nodes.push(n);
        }
    }

    pub(super) fn remove(&mut self, node_id: &str) {
        self.nodes.retain(|n| n.id != node_id);
        self.note_paths.remove(node_id);
    }

    /// Node + all ancestors up to the root, as a set. Mirrors the old SQL
    /// `ancestors_inclusive` (used for LCA stop-sets).
    pub(super) fn ancestors_inclusive(&self, node_id: &str) -> HashSet<String> {
        let mut out = HashSet::new();
        let mut cursor = Some(node_id.to_string());
        while let Some(id) = cursor {
            let Some(node) = self.get(&id) else { break };
            out.insert(id.clone());
            cursor = node.parent.clone();
        }
        out
    }

    /// Bump churn on `from` and every ancestor up to (but not including) any
    /// node in `stop_at`. Mirrors the old `bump_churn_chain_until`.
    pub(super) fn bump_churn_until(&mut self, from: &str, stop_at: &HashSet<String>, delta: u32) {
        if delta == 0 {
            return;
        }
        let mut cursor = Some(from.to_string());
        while let Some(id) = cursor {
            if stop_at.contains(&id) {
                break;
            }
            let parent = match self.get(&id) {
                Some(n) => n.parent.clone(),
                None => break,
            };
            if let Some(n) = self.get_mut(&id) {
                n.summary_membership_churn += delta;
            }
            cursor = parent;
        }
    }

    /// Bump churn on `from` and every ancestor up to the root.
    pub(super) fn bump_churn(&mut self, from: &str, delta: u32) {
        self.bump_churn_until(from, &HashSet::new(), delta);
    }

    pub(super) fn set_churn(&mut self, node_id: &str, value: u32) {
        if let Some(n) = self.get_mut(node_id) {
            n.summary_membership_churn = value;
        }
    }
}

// ── construction + load / save ─────────────────────────────────────────

impl Db {
    /// Create a trees handle backed by the op-log + vault. Ensures the
    /// `.hiker/trees/` directory exists. No file is opened — trees are read
    /// and written per-id as `.md` documents.
    pub fn new(oplog: Arc<OpLog>, vault: Arc<Vault>) -> Result<Self, Error> {
        let dir = vault.root().join(".hiker").join("trees");
        std::fs::create_dir_all(&dir)?;
        let store = crate::store::Store::open(vault.root()).map_err(|e| Error::Store(e.to_string()))?;
        Ok(Self {
            oplog,
            vault,
            centroids: Mutex::new(store),
            history: Mutex::new(HashMap::new()),
        })
    }

    /// Vault-relative path of a tree's `.md` file.
    pub(super) fn rel(tree_id: &str) -> String {
        format!(".hiker/trees/{tree_id}.md")
    }

    /// Load a tree from its `.md`. Returns `TreeNotFound` when the file is
    /// missing or carries no `hiker` cluster-tree frontmatter.
    pub(super) fn load(&self, tree_id: &str) -> Result<TreeDoc, Error> {
        let rel = Self::rel(tree_id);
        let text = self
            .vault
            .read_file(&rel)
            .map_err(|_| Error::TreeNotFound(tree_id.to_string()))?;
        let split = crate::frontmatter::split(&text);
        let body = split.body.to_string();
        let Some(Yaml::Mapping(mut top)) = split.frontmatter else {
            return Err(Error::TreeNotFound(tree_id.to_string()));
        };
        let hiker_key = Yaml::String("hiker".into());
        let hiker_val = top
            .remove(&hiker_key)
            .ok_or_else(|| Error::TreeNotFound(tree_id.to_string()))?;
        let fm: TreeFm = serde_yml::from_value(hiker_val).map_err(|e| Error::Yaml(e.to_string()))?;
        if fm.kind != "cluster-tree" {
            return Err(Error::TreeNotFound(tree_id.to_string()));
        }

        let mut note_paths = HashMap::new();
        let mut nodes: Vec<EditableNode> = fm
            .nodes
            .iter()
            .map(|nf| {
                let note_ref = nf.note.as_ref().map(|n| {
                    if !n.path.is_empty() {
                        note_paths.insert(nf.id.clone(), n.path.clone());
                    }
                    n.id.clone()
                });
                EditableNode {
                    id: nf.id.clone(),
                    parent: nf.parent.clone(),
                    kind: nf.kind,
                    note_ref,
                    name: nf.name.clone(),
                    summary: nf.summary.clone(),
                    user_edited_name: nf.user_edited_name,
                    user_edited_summary: nf.user_edited_summary,
                    policy: nf.policy.clone(),
                    centroid: None, // sourced from index.db, never the .md
                    confidence: nf.confidence,
                    summary_membership_churn: nf.churn,
                }
            })
            .collect();

        // Fill centroids from the derived index cache (`trees-centroids-index`).
        if let Ok(store) = self.centroids.lock()
            && let Ok(cents) = store.cluster_centroids_for_tree(tree_id)
        {
            for n in nodes.iter_mut() {
                if let Some(c) = cents.get(&n.id) {
                    n.centroid = Some(c.clone());
                }
            }
        }

        let meta = TreeRow {
            id: fm.id,
            name: fm.name,
            source: fm.source,
            state: fm.state,
            scope_json: yaml_to_json_string(&fm.scope),
            method_json: yaml_to_json_string(&fm.method),
            created_at_ms: fm.created_at_ms,
            vault_snapshot: fm.vault_snapshot,
        };
        Ok(TreeDoc {
            meta,
            nodes,
            body,
            extra_fm: top,
            note_paths,
        })
    }

    /// Serialize `doc` back to frontmatter (body preserved) and commit it
    /// through the op-log as a user edit. Because only the frontmatter fence
    /// changes, the op is labeled `SetFrontmatter`.
    pub(super) fn save(&self, doc: &TreeDoc) -> Result<(), Error> {
        let fm = TreeFm {
            kind: "cluster-tree".into(),
            id: doc.meta.id.clone(),
            name: doc.meta.name.clone(),
            source: doc.meta.source.clone(),
            state: doc.meta.state.clone(),
            scope: json_string_to_yaml(&doc.meta.scope_json),
            method: json_string_to_yaml(&doc.meta.method_json),
            created_at_ms: doc.meta.created_at_ms,
            vault_snapshot: doc.meta.vault_snapshot.clone(),
            nodes: doc
                .nodes
                .iter()
                .map(|n| NodeFm {
                    id: n.id.clone(),
                    parent: n.parent.clone(),
                    kind: n.kind,
                    note: n.note_ref.as_ref().map(|id| NoteRefFm {
                        id: id.clone(),
                        path: doc.note_paths.get(&n.id).cloned().unwrap_or_default(),
                    }),
                    name: n.name.clone(),
                    summary: n.summary.clone(),
                    user_edited_name: n.user_edited_name,
                    user_edited_summary: n.user_edited_summary,
                    policy: n.policy.clone(),
                    confidence: n.confidence,
                    churn: n.summary_membership_churn,
                })
                .collect(),
        };
        let hiker_val = serde_yml::to_value(&fm).map_err(|e| Error::Yaml(e.to_string()))?;
        let mut top = doc.extra_fm.clone();
        top.insert(Yaml::String("hiker".into()), hiker_val);
        let full = crate::frontmatter::assemble(&Yaml::Mapping(top), &doc.body)
            .map_err(|e| Error::Yaml(e.to_string()))?;
        let rel = Self::rel(&doc.meta.id);
        crate::ops::op_writes::user_save(&self.oplog, &self.vault, &rel, &full)?;
        Ok(())
    }

    /// Load → mutate in-memory → save once. Every reshape op routes through
    /// here so each logical edit is a single `SetFrontmatter` write.
    pub(super) fn mutate<R>(
        &self,
        tree_id: &str,
        f: impl FnOnce(&mut TreeDoc) -> Result<R, Error>,
    ) -> Result<R, Error> {
        let mut doc = self.load(tree_id)?;
        let out = f(&mut doc)?;
        self.save(&doc)?;
        Ok(out)
    }

    // ── Tree-level operations ────────────────────────────────────────

    /// Create a new tree `.md`. Returns the tree id (generated when `None`).
    pub fn insert_tree(&self, t: TreeInsert) -> Result<super::types::TreeId, Error> {
        let id = t.id.unwrap_or_else(crate::store::dto::new_id);
        let doc = TreeDoc {
            meta: TreeRow {
                id: id.clone(),
                name: t.name,
                source: t.source,
                state: t.state,
                scope_json: t.scope_json,
                method_json: t.method_json,
                created_at_ms: now_ms(),
                vault_snapshot: t.vault_snapshot,
            },
            nodes: Vec::new(),
            body: BODY_STUB.to_string(),
            extra_fm: serde_yml::Mapping::new(),
            note_paths: HashMap::new(),
        };
        self.save(&doc)?;
        Ok(id)
    }

    /// Look up one tree's metadata. `None` if it doesn't exist.
    pub fn get_tree(&self, tree_id: &str) -> Result<Option<TreeRow>, Error> {
        match self.load(tree_id) {
            Ok(doc) => Ok(Some(doc.meta)),
            Err(Error::TreeNotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// List every tree, newest first.
    pub fn list_trees(&self) -> Result<Vec<TreeRow>, Error> {
        let dir = self.vault.root().join(".hiker").join("trees");
        let mut out = Vec::new();
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e.into()),
        };
        for entry in rd {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(stem) = name.strip_suffix(".md") else {
                continue;
            };
            match self.load(stem) {
                Ok(doc) => out.push(doc.meta),
                Err(Error::TreeNotFound(_)) => continue,
                Err(e) => return Err(e),
            }
        }
        out.sort_by_key(|r| std::cmp::Reverse(r.created_at_ms));
        Ok(out)
    }

    /// Update a tree's state.
    pub fn set_tree_state(&self, tree_id: &str, state: &str) -> Result<(), Error> {
        self.mutate(tree_id, |doc| {
            doc.meta.state = state.to_string();
            Ok(())
        })
        .map_err(|e| match e {
            Error::TreeNotFound(_) => Error::TreeNotFound(tree_id.to_string()),
            other => other,
        })
    }

    /// Delete a tree: tombstone its op-log document and remove the `.md`.
    /// (Trash-on-discard semantics live in the app's discard-draft path per
    /// `cluster-editor-discard-draft`; this is the low-level removal.)
    pub fn delete_tree(&self, tree_id: &str) -> Result<(), Error> {
        let rel = Self::rel(tree_id);
        if let Ok(Some(doc_id)) = self.oplog.doc_id_for_path(&rel) {
            let _ = self
                .oplog
                .tombstone_document(&doc_id, &crate::oplog::shapes::Author::User);
        }
        if let Ok(abs) = self.vault.abs_path(&rel) {
            let _ = std::fs::remove_file(abs);
        }
        if let Ok(mut store) = self.centroids.lock() {
            let _ = store.delete_cluster_centroids_for_tree(tree_id);
        }
        self.history
            .lock()
            .map_err(|_| Error::Poisoned)?
            .remove(tree_id);
        Ok(())
    }

    // ── Node-level operations ────────────────────────────────────────

    /// Bulk-insert nodes for a tree. Used by the build pipeline when it lands
    /// a fresh tree's initial state. Centroids are dropped here — the caller
    /// persists them to `index.db` (`trees-centroids-index`).
    pub fn insert_nodes(&self, tree_id: &str, nodes: &[NodeInsert]) -> Result<(), Error> {
        self.mutate(tree_id, |doc| {
            for n in nodes {
                doc.insert(node_from_insert(n));
            }
            Ok(())
        })?;
        let mut store = self.centroids.lock().map_err(|_| Error::Poisoned)?;
        for n in nodes {
            if let Some(c) = &n.centroid {
                store
                    .put_cluster_centroid(tree_id, &n.node_id, c)
                    .map_err(|e| Error::Store(e.to_string()))?;
            }
        }
        Ok(())
    }

    /// Fetch one node, hydrated into an `EditableNode`.
    pub fn get_node(&self, tree_id: &str, node_id: &str) -> Result<Option<EditableNode>, Error> {
        Ok(self.load(tree_id)?.get(node_id).cloned())
    }

    /// Every node in the tree, in stored order.
    pub fn list_nodes(&self, tree_id: &str) -> Result<Vec<EditableNode>, Error> {
        Ok(self.load(tree_id)?.nodes)
    }

    /// Children of a given parent (or the root when `parent_id` is `None`).
    pub fn children_of(
        &self,
        tree_id: &str,
        parent_id: Option<&str>,
    ) -> Result<Vec<EditableNode>, Error> {
        Ok(self.load(tree_id)?.children(parent_id))
    }

    /// Append a single new node. Used by split + ops that grow the tree
    /// mid-edit. Doesn't record history — the wrapping op does.
    pub fn insert_single_node(&self, tree_id: &str, n: &NodeInsert) -> Result<(), Error> {
        self.mutate(tree_id, |doc| {
            doc.insert(node_from_insert(n));
            Ok(())
        })?;
        if let Some(c) = &n.centroid {
            let mut store = self.centroids.lock().map_err(|_| Error::Poisoned)?;
            store
                .put_cluster_centroid(tree_id, &n.node_id, c)
                .map_err(|e| Error::Store(e.to_string()))?;
        }
        Ok(())
    }

    /// Delete a single node by id.
    ///
    /// status: cluster-summary-staleness-counter
    /// No churn bump here: structural drop of an already-empty cluster shell
    /// (leaf removals proper go through `move_node` / `reparent_many` /
    /// `promote_outlier`, which bump their ancestor chains directly).
    pub fn delete_node(&self, tree_id: &str, node_id: &str) -> Result<(), Error> {
        self.mutate(tree_id, |doc| {
            doc.remove(node_id);
            Ok(())
        })?;
        if let Ok(mut store) = self.centroids.lock() {
            let _ = store.delete_cluster_centroid(tree_id, node_id);
        }
        Ok(())
    }

    /// Collect a node and all of its ancestors up to the root.
    pub fn ancestors_inclusive(
        &self,
        tree_id: &str,
        node_id: &str,
    ) -> Result<HashSet<String>, Error> {
        Ok(self.load(tree_id)?.ancestors_inclusive(node_id))
    }

    /// Bump churn on `from_node` + ancestors, stopping at any node in
    /// `stop_at`. Single-write.
    pub fn bump_churn_chain_until(
        &self,
        tree_id: &str,
        from_node: &str,
        stop_at: &HashSet<String>,
        delta: u32,
    ) -> Result<(), Error> {
        if delta == 0 {
            return Ok(());
        }
        self.mutate(tree_id, |doc| {
            doc.bump_churn_until(from_node, stop_at, delta);
            Ok(())
        })
    }

    /// Bump churn on `from_node` + every ancestor up to the root.
    pub fn bump_churn_chain(&self, tree_id: &str, from_node: &str, delta: u32) -> Result<(), Error> {
        if delta == 0 {
            return Ok(());
        }
        self.mutate(tree_id, |doc| {
            doc.bump_churn(from_node, delta);
            Ok(())
        })
    }

    /// Reset churn on one node to 0 — called after Regenerate.
    pub fn reset_churn(&self, tree_id: &str, node_id: &str) -> Result<(), Error> {
        self.mutate(tree_id, |doc| {
            doc.set_churn(node_id, 0);
            Ok(())
        })
    }

    /// Set churn on one node to a specific value.
    pub fn set_churn(&self, tree_id: &str, node_id: &str, value: u32) -> Result<(), Error> {
        self.mutate(tree_id, |doc| {
            doc.set_churn(node_id, value);
            Ok(())
        })
    }
}

// ── helpers ────────────────────────────────────────────────────────────

/// The undo snapshot of a node's full row, matching the JSON shape the
/// host's undo/redo dispatch reads back (`cluster-editor-undo-redo`).
/// Centroid is omitted (it lives in `index.db`); `policy` is the serialized
/// `NodePolicy` string, mirroring the prior `policy_json` column.
pub(super) fn snapshot_full(n: &EditableNode) -> serde_json::Value {
    serde_json::json!({
        "parent_id": n.parent,
        "kind": n.kind.as_str(),
        "note_id": n.note_ref,
        "name": n.name,
        "summary": n.summary,
        "user_edited_name": n.user_edited_name,
        "user_edited_summary": n.user_edited_summary,
        "policy": n.policy.as_ref().and_then(|p| serde_json::to_string(p).ok()),
        "confidence": n.confidence,
        "summary_membership_churn": n.summary_membership_churn,
    })
}

pub(super) fn node_from_insert(n: &NodeInsert) -> EditableNode {
    EditableNode {
        id: n.node_id.clone(),
        parent: n.parent_id.clone(),
        kind: n.kind,
        note_ref: n.note_id.clone(),
        name: n.name.clone(),
        summary: n.summary.clone(),
        user_edited_name: n.user_edited_name,
        user_edited_summary: n.user_edited_summary,
        policy: n.policy.clone(),
        centroid: n.centroid.clone(),
        confidence: n.confidence,
        summary_membership_churn: n.summary_membership_churn,
    }
}

pub(super) fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A JSON string (as carried on `TreeRow.scope_json` / `method_json`) → a
/// YAML value for the frontmatter. Invalid/empty input becomes an empty
/// mapping so the field always round-trips.
fn json_string_to_yaml(s: &str) -> Yaml {
    let jv: serde_json::Value = serde_json::from_str(s).unwrap_or(serde_json::Value::Null);
    serde_yml::to_value(jv).unwrap_or(Yaml::Mapping(serde_yml::Mapping::new()))
}

/// A frontmatter YAML value → the JSON string `TreeRow` carries. A null /
/// missing value becomes `"{}"`.
fn yaml_to_json_string(y: &Yaml) -> String {
    if matches!(y, Yaml::Null) {
        return "{}".to_string();
    }
    serde_json::to_string(y).unwrap_or_else(|_| "{}".to_string())
}
