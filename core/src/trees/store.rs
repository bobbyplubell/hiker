//! Per-tree `.md` store for `core::trees` (`trees-md-store`).
//!
//! Each cluster tree is one markdown document at a **visible** vault path —
//! `{new_cluster_tree_dir}/<tree-id>.md` (default `cluster-trees/`) per
//! `cluster-tree-visible-note` / `subsystem-notes-visible`. The full
//! structure lives in the `hiker` frontmatter (`trees-md-frontmatter`); the
//! body is a fixed stub (the human render is produced on demand by the
//! cluster editor, not persisted). Edits load the tree, mutate the in-memory
//! [`TreeDoc`], and rewrite **only the frontmatter fence** through the op-log
//! working layer — so each edit lands as a `SetFrontmatter` op
//! (`trees-edit-setfrontmatter`) and the body bytes never move.
//!
//! Discovery is by the `hiker.kind: cluster-tree` frontmatter query
//! (`store-note-query`), not a directory glob — so a tree the user moved,
//! hand-typed, or imported is found exactly like one hiker authored. A
//! one-time migration (`migrate_legacy_trees`) relocates legacy
//! `.hiker/trees/<id>.md` files to the visible default on first open,
//! preserving each tree's op-log identity.
//!
//! No rusqlite, no schema-version file — the frontmatter is self-describing
//! and non-`hiker` keys round-trip untouched.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_yml::Value as Yaml;

use super::types::{
    Db, EditableNode, Error, NodeInsert, NodeKind, NodePolicy, TreeContainingHit, TreeInsert, TreeRow,
};
use crate::indexer::IndexJobTx;
use crate::oplog::OpLog;
use crate::store::dto::{MetaFilter, NoteQuery};
use crate::vault::Vault;
use crate::watcher::Watcher;

/// Frontmatter `hiker.kind` value that marks a note as a cluster tree. The
/// discovery query (`list_trees`, `path_for_tree`) filters on it; a note the
/// user typed or imported with this `kind` is a tree exactly like one hiker
/// wrote. status: cluster-tree-visible-note
const KIND: &str = "cluster-tree";

/// Default visible directory for new tree `.md` files when the
/// `new_cluster_tree_dir` config hasn't been wired (early open, tests).
/// Mirrors `default_new_cluster_tree_dir` in `core::config`.
const DEFAULT_TREE_DIR: &str = "cluster-trees/";

/// The `query_notes` query that finds every cluster-tree note by frontmatter.
fn cluster_tree_query() -> NoteQuery {
    NoteQuery {
        filters: vec![MetaFilter::Equals {
            key: "hiker.kind".to_string(),
            value: KIND.to_string(),
        }],
        ..Default::default()
    }
}

/// Read the `hiker.id` from a tree `.md`'s frontmatter, confirming it is a
/// `cluster-tree`. `None` for a non-tree note or a tree missing its id.
fn fm_tree_id(text: &str) -> Option<String> {
    let fm = crate::frontmatter::split(text).frontmatter?;
    let hiker = fm.get("hiker")?;
    if hiker.get("kind").and_then(Yaml::as_str) != Some(KIND) {
        return None;
    }
    hiker.get("id").and_then(Yaml::as_str).map(str::to_string)
}

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
    /// Vault-relative path the tree was loaded from (or written to on
    /// insert). `save` rewrites this exact file — the path is decoupled from
    /// the tree id so a tree the user moved keeps saving in place
    /// (`cluster-tree-visible-note`).
    path: String,
    /// Body bytes, preserved across edits.
    body: String,
    /// Non-`hiker` frontmatter keys, preserved on round-trip.
    extra_fm: serde_yml::Mapping,
    /// node_id → recorded note op-log id, so the double-link survives a
    /// round-trip even though `EditableNode` only carries the path half.
    note_ids: HashMap<String, String>,
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
        // No id half is known from an `EditableNode` alone (it carries the
        // path); leave any previously-recorded id for this node intact so a
        // load → mutate → save round-trip preserves the double-link.
        if let Some(idx) = self.position(&n.id) {
            self.nodes[idx] = n;
        } else {
            self.nodes.push(n);
        }
    }

    pub(super) fn remove(&mut self, node_id: &str) {
        self.nodes.retain(|n| n.id != node_id);
        self.note_ids.remove(node_id);
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
    /// Create a trees handle backed by the op-log + vault. No directory is
    /// created — the visible tree dir (`new_cluster_tree_dir`, default
    /// `cluster-trees/`) is created lazily by `vault.write_file` on the first
    /// tree. Runs the one-time legacy-location migration
    /// (`migrate_legacy_trees`) so a vault carrying `.hiker/trees/<id>.md`
    /// files surfaces them at the visible default. The watcher/indexer
    /// handles and the configured dir are wired later via [`Db::wire`] (they
    /// don't exist at construction time).
    pub fn new(oplog: Arc<OpLog>, vault: Arc<Vault>) -> Result<Self, Error> {
        let store = crate::store::Store::open(vault.root()).map_err(|e| Error::Store(e.to_string()))?;
        let db = Self {
            oplog,
            vault,
            centroids: Mutex::new(store),
            history: Mutex::new(HashMap::new()),
            watcher: std::sync::OnceLock::new(),
            index_jobs: std::sync::OnceLock::new(),
            new_tree_dir: Mutex::new(DEFAULT_TREE_DIR.to_string()),
            id_paths: Mutex::new(HashMap::new()),
        };
        db.migrate_legacy_trees()?;
        Ok(db)
    }

    /// Wire the watcher + indexer handles and the configured default tree
    /// directory after the indexer/watcher have started (they postdate
    /// `Db::new` in bootstrap). Idempotent for the handles (`OnceLock::set`
    /// ignores a second set); always refreshes the configured dir.
    pub fn wire(&self, watcher: Arc<Watcher>, jobs: IndexJobTx, new_tree_dir: &str) {
        let _ = self.watcher.set(watcher);
        let _ = self.index_jobs.set(jobs);
        if let Ok(mut dir) = self.new_tree_dir.lock() {
            *dir = new_tree_dir.to_string();
        }
    }

    /// One-time, idempotent relocation of legacy `.hiker/trees/<id>.md` trees
    /// to the visible default (`cluster-trees/<id>.md`), run at `Db::new`.
    /// Legacy trees were unindexed (everything under `.hiker/` is watcher-
    /// ignored), so this is what makes them discoverable by the frontmatter
    /// query. Guarded to no-op when `.hiker/trees/` is absent.
    ///
    /// Per-tree ordering — **the single biggest correctness risk**: the
    /// op-log doc is repointed to the new path (`oplog::writes::rename`, which
    /// preserves the doc_id + full history) **before** the file bytes move.
    /// Doing the fs move first, or a `user_save` at the new path before the
    /// repoint, would leave the op-log mapping at the old path and the next
    /// write would mint a *fresh* doc — forking history. The repoint comes
    /// first; only then do the bytes move. A legacy tree with no op-log
    /// mapping (never saved while the op-log was running) just has its bytes
    /// moved — the bootstrap / full-scan seeds a fresh doc at the new path.
    ///
    /// Indexing is deferred: the indexer's initial full-scan (which runs after
    /// `Db::new` in bootstrap and walks the visible vault) picks the relocated
    /// files up — no `Upsert` enqueue is needed here, and the watcher isn't
    /// running yet so no suppression is needed either.
    ///
    /// status: cluster-tree-migration
    fn migrate_legacy_trees(&self) -> Result<(), Error> {
        let legacy_dir = self.vault.root().join(".hiker").join("trees");
        if !legacy_dir.is_dir() {
            return Ok(());
        }
        let target_dir = DEFAULT_TREE_DIR.trim_end_matches('/');
        let entries = match std::fs::read_dir(&legacy_dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(stem) = name.strip_suffix(".md") else {
                continue;
            };
            let old_rel = format!(".hiker/trees/{name}");
            let new_rel = if target_dir.is_empty() {
                format!("{stem}.md")
            } else {
                format!("{target_dir}/{stem}.md")
            };
            // Idempotent: a tree already at the new path means a prior run (or
            // a hand-moved file) already relocated it; leave the legacy copy
            // for the unlink at loop end and skip.
            if self.vault.abs_path(&new_rel).map(|p| p.exists()).unwrap_or(false) {
                continue;
            }
            // 1. Repoint the op-log doc FIRST (preserves doc_id + history).
            //    No-op when the legacy file was never op-log-seeded.
            if matches!(self.oplog.doc_id_for_path(&old_rel), Ok(Some(_)))
                && let Err(e) = crate::oplog::writes::rename(
                    &self.oplog,
                    &old_rel,
                    &new_rel,
                    &crate::oplog::shapes::Author::User,
                )
            {
                tracing::warn!(error = %e, %old_rel, %new_rel, "tree migration: op-log repoint failed; skipping");
                continue;
            }
            // 2. Move the bytes to the visible path (create parent on first).
            let Ok(old_abs) = self.vault.abs_path(&old_rel) else {
                continue;
            };
            let Ok(new_abs) = self.vault.abs_path(&new_rel) else {
                continue;
            };
            if let Some(parent) = new_abs.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if let Err(e) = std::fs::rename(&old_abs, &new_abs) {
                tracing::warn!(error = %e, %old_rel, %new_rel, "tree migration: file move failed");
                continue;
            }
            // Cache the relocated path so a `load` before the full-scan indexes
            // it still resolves (the id is the filename stem here).
            self.cache_path(stem, &new_rel);
        }
        // 3. Best-effort cleanup of the now-empty legacy shell so a second
        //    open takes the early-return path above.
        let _ = std::fs::remove_dir(&legacy_dir);
        Ok(())
    }

    /// Resolve a tree id to the vault-relative path of its `.md` via the
    /// frontmatter query: every `hiker.kind: cluster-tree` note whose
    /// `hiker.id` matches `tree_id`. `None` when no such note exists. The
    /// path is decoupled from the id, so this is the single id→path seam the
    /// load / delete / state-update paths share. status: cluster-tree-visible-note
    pub(super) fn path_for_tree(&self, tree_id: &str) -> Option<String> {
        // In-process cache first — a tree created / loaded this session is
        // resolvable before the indexer has populated `note_meta`. Verify the
        // cached file still carries the id (a user move/delete invalidates it).
        if let Ok(cache) = self.id_paths.lock()
            && let Some(rel) = cache.get(tree_id)
            && self.vault.read_file(rel).ok().and_then(|t| fm_tree_id(&t)).as_deref() == Some(tree_id)
        {
            return Some(rel.clone());
        }
        // Fallback: the frontmatter query, for trees this process hasn't
        // touched (discovered, sync-arrived, hand-typed).
        let rows = {
            let store = self.centroids.lock().ok()?;
            store.query_notes(&cluster_tree_query()).ok()?
        };
        for row in rows {
            let Ok(text) = self.vault.read_file(&row.path) else {
                continue;
            };
            if fm_tree_id(&text).as_deref() == Some(tree_id) {
                if let Ok(mut cache) = self.id_paths.lock() {
                    cache.insert(tree_id.to_string(), row.path.clone());
                }
                return Some(row.path);
            }
        }
        None
    }

    /// Record an id → path mapping in the in-process cache.
    fn cache_path(&self, tree_id: &str, rel: &str) {
        if let Ok(mut cache) = self.id_paths.lock() {
            cache.insert(tree_id.to_string(), rel.to_string());
        }
    }

    /// Load a tree by id. Resolves the id → path via the frontmatter query
    /// (`path_for_tree`), then reads + parses that `.md`. Returns
    /// `TreeNotFound` when no `cluster-tree` note carries the id, or the
    /// resolved file is missing / lacks `hiker` cluster-tree frontmatter.
    pub(super) fn load(&self, tree_id: &str) -> Result<TreeDoc, Error> {
        let rel = self
            .path_for_tree(tree_id)
            .ok_or_else(|| Error::TreeNotFound(tree_id.to_string()))?;
        self.load_at(&rel, tree_id)
    }

    /// Load + parse the tree `.md` at a known vault-relative path. The
    /// `expect_id` guards against a path whose frontmatter id drifted from
    /// what the caller resolved.
    fn load_at(&self, rel: &str, expect_id: &str) -> Result<TreeDoc, Error> {
        let text = self
            .vault
            .read_file(rel)
            .map_err(|_| Error::TreeNotFound(expect_id.to_string()))?;
        let split = crate::frontmatter::split(&text);
        let body = split.body.to_string();
        let Some(Yaml::Mapping(mut top)) = split.frontmatter else {
            return Err(Error::TreeNotFound(expect_id.to_string()));
        };
        let hiker_key = Yaml::String("hiker".into());
        let hiker_val = top
            .remove(&hiker_key)
            .ok_or_else(|| Error::TreeNotFound(expect_id.to_string()))?;
        let fm: TreeFm = serde_yml::from_value(hiker_val).map_err(|e| Error::Yaml(e.to_string()))?;
        if fm.kind != KIND {
            return Err(Error::TreeNotFound(expect_id.to_string()));
        }
        // Cache the resolved id → path so a subsequent load doesn't need the
        // index (the create flow loads immediately after insert).
        self.cache_path(&fm.id, rel);

        let mut note_ids = HashMap::new();
        let mut nodes: Vec<EditableNode> = fm
            .nodes
            .iter()
            .map(|nf| {
                // Path-as-identity: surface the rel-path in memory. The on-disk
                // `note` double-link keeps both id + path; we stash the id in
                // `note_ids` so a round-trip re-emits it unchanged.
                let note_path = nf.note.as_ref().map(|n| {
                    if !n.id.is_empty() {
                        note_ids.insert(nf.id.clone(), n.id.clone());
                    }
                    n.path.clone()
                });
                EditableNode {
                    id: nf.id.clone(),
                    parent: nf.parent.clone(),
                    kind: nf.kind,
                    note_path,
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
        // Keyed by the tree's own id (`fm.id`), which is path-independent.
        if let Ok(store) = self.centroids.lock()
            && let Ok(cents) = store.cluster_centroids_for_tree(&fm.id)
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
            path: rel.to_string(),
            body,
            extra_fm: top,
            note_ids,
        })
    }

    /// Serialize `doc` back to frontmatter (body preserved) and commit it
    /// through the op-log as a user edit at `doc.path`. Because only the
    /// frontmatter fence changes, the op is labeled `SetFrontmatter`.
    ///
    /// The tree now lives at a visible, indexed path
    /// (`cluster-tree-visible-note`), so — like trail-docs and presets — the
    /// write suppresses the watcher and enqueues an explicit `Upsert` so the
    /// tree is queryable at once and the op-log atomic write isn't echoed
    /// back as an external edit. When the handles aren't wired yet (early
    /// open, tests) the write still lands; the ambient watcher → indexer
    /// route picks it up.
    pub(super) fn save(&self, doc: &TreeDoc) -> Result<(), Error> {
        let fm = TreeFm {
            kind: KIND.into(),
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
                    note: n.note_path.as_ref().map(|path| NoteRefFm {
                        id: doc.note_ids.get(&n.id).cloned().unwrap_or_default(),
                        path: path.clone(),
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
        let rel = &doc.path;
        // Record the id → path mapping so the next `load` resolves without the
        // index (covers the insert_tree → insert_nodes create sequence).
        self.cache_path(&doc.meta.id, rel);
        // Suppress before the op-log atomic write so notify's echo for this
        // self-write is dropped (`watcher-suppress-self-writes`).
        if let Some(watcher) = self.watcher.get() {
            watcher.suppress(rel.clone());
        }
        crate::ops::op_writes::user_save(&self.oplog, &self.vault, rel, &full)?;
        // Re-suppress close to when notify surfaces the write, then index
        // explicitly (the watcher events were suppressed) so the tree is
        // discoverable by the frontmatter query immediately.
        if let Some(watcher) = self.watcher.get() {
            watcher.suppress(rel.clone());
        }
        if let Some(jobs) = self.index_jobs.get() {
            let _ = jobs.try_upsert(rel.clone(), false);
        }
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

    /// Create a new tree `.md` at the configured visible directory
    /// (`{new_cluster_tree_dir}/<tree-id>.md`, default `cluster-trees/`).
    /// Returns the tree id (generated when `None`). The id is the basename so
    /// uniqueness is free; discovery is by frontmatter, so the user can
    /// rename / move the file afterward. status: cluster-tree-visible-note
    pub fn insert_tree(&self, t: TreeInsert) -> Result<super::types::TreeId, Error> {
        let id = t.id.unwrap_or_else(crate::store::dto::new_id);
        let dir = self
            .new_tree_dir
            .lock()
            .map(|d| d.clone())
            .unwrap_or_else(|_| DEFAULT_TREE_DIR.to_string());
        let folder = dir.trim_end_matches('/');
        let path = if folder.is_empty() {
            format!("{id}.md")
        } else {
            format!("{folder}/{id}.md")
        };
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
            path,
            body: BODY_STUB.to_string(),
            extra_fm: serde_yml::Mapping::new(),
            note_ids: HashMap::new(),
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

    /// List every tree, newest first. Discovery is primarily by the
    /// `hiker.kind: cluster-tree` frontmatter query (`store-note-query`) — so
    /// trees are found anywhere in the vault, including notes the user typed
    /// or imported with that frontmatter. The in-process id→path cache is
    /// unioned in so a tree created this session shows up *before* the indexer
    /// has populated `note_meta` (the query alone would miss it until the
    /// explicit `Upsert` is processed). status: cluster-tree-visible-note
    pub fn list_trees(&self) -> Result<Vec<TreeRow>, Error> {
        let query_rows = {
            let store = self.centroids.lock().map_err(|_| Error::Poisoned)?;
            store
                .query_notes(&cluster_tree_query())
                .map_err(|e| Error::Store(e.to_string()))?
        };
        // Union the indexed paths with the in-process cache, de-duped by path,
        // so neither index latency nor a not-yet-cached discovery hides a tree.
        let mut paths: Vec<String> = query_rows.into_iter().map(|r| r.path).collect();
        if let Ok(cache) = self.id_paths.lock() {
            for rel in cache.values() {
                if !paths.contains(rel) {
                    paths.push(rel.clone());
                }
            }
        }
        let mut out = Vec::new();
        for rel in paths {
            // We already hold the path; parse it straight through `load_at`,
            // passing the path's own frontmatter id as the expected id so the
            // guard can't reject a legitimately-discovered tree.
            let Ok(text) = self.vault.read_file(&rel) else {
                continue;
            };
            let Some(id) = fm_tree_id(&text) else {
                continue;
            };
            match self.load_at(&rel, &id) {
                Ok(doc) => out.push(doc.meta),
                Err(Error::TreeNotFound(_)) => continue,
                Err(e) => return Err(e),
            }
        }
        out.sort_by_key(|r| std::cmp::Reverse(r.created_at_ms));
        Ok(out)
    }

    /// The tree id of the cluster-tree note at `rel`, or `None` when `rel` isn't
    /// a cluster-tree note. Reads the note's frontmatter (`hiker.kind ==
    /// cluster-tree` → its `hiker.id`). Lets the host route a cluster-tree to its
    /// force-graph view instead of opening it as raw markdown.
    /// status: cluster-tree-open-routing
    #[must_use]
    pub fn tree_id_at_path(&self, rel: &str) -> Option<String> {
        let text = self.vault.read_file(rel).ok()?;
        fm_tree_id(&text)
    }

    /// Cluster-trees with at least one leaf node referencing `note_path` — the
    /// "appears in" reverse lookup. There's no node index (a tree *is* its `.md`
    /// doc), so this mirrors [`Self::list_trees`]: walk the discovered tree docs,
    /// parse each, and keep those whose nodes reference the note. The tree's
    /// vault path comes for free from the walk, so the hit is directly openable.
    /// status: canvas-appears-in
    pub fn trees_containing_note(&self, note_path: &str) -> Result<Vec<TreeContainingHit>, Error> {
        let query_rows = {
            let store = self.centroids.lock().map_err(|_| Error::Poisoned)?;
            store
                .query_notes(&cluster_tree_query())
                .map_err(|e| Error::Store(e.to_string()))?
        };
        let mut paths: Vec<String> = query_rows.into_iter().map(|r| r.path).collect();
        if let Ok(cache) = self.id_paths.lock() {
            for rel in cache.values() {
                if !paths.contains(rel) {
                    paths.push(rel.clone());
                }
            }
        }
        let mut out = Vec::new();
        for rel in paths {
            let Ok(text) = self.vault.read_file(&rel) else {
                continue;
            };
            let Some(id) = fm_tree_id(&text) else {
                continue;
            };
            let doc = match self.load_at(&rel, &id) {
                Ok(doc) => doc,
                Err(Error::TreeNotFound(_)) => continue,
                Err(e) => return Err(e),
            };
            if doc
                .nodes
                .iter()
                .any(|n| n.note_path.as_deref() == Some(note_path))
            {
                out.push(TreeContainingHit {
                    tree_id: doc.meta.id.clone(),
                    name: doc.meta.name.clone(),
                    path: rel,
                });
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
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
        // Resolve the tree's current visible path via the frontmatter query.
        // A tree whose file is already gone still tombstones any op-log doc
        // and clears its centroids below. The hard `remove_file` is left
        // *un*-suppressed: the file is now visible + indexed, so the watcher's
        // Delete event drives the index-row removal (the same way an ordinary
        // note delete does); suppressing it would orphan the `notes` row.
        if let Some(rel) = self.path_for_tree(tree_id) {
            if let Ok(Some(doc_id)) = self.oplog.doc_id_for_path(&rel) {
                let _ = self
                    .oplog
                    .tombstone_document(&doc_id, &crate::oplog::shapes::Author::User);
            }
            if let Ok(abs) = self.vault.abs_path(&rel) {
                let _ = std::fs::remove_file(abs);
            }
        }
        if let Ok(mut store) = self.centroids.lock() {
            let _ = store.delete_cluster_centroids_for_tree(tree_id);
        }
        if let Ok(mut cache) = self.id_paths.lock() {
            cache.remove(tree_id);
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
        "note_id": n.note_path,
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
        note_path: n.note_id.clone(),
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
