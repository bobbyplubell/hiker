//! Apply mechanic + rejection-history bookkeeping for cluster-tree
//! suggestions (per `docs/suggestions.md`). Used by both the
//! cluster-editor Apply path (`cluster-editor-apply-action`) and the
//! `hiker suggest apply` CLI (`suggestions-apply-cmd`).
//!
//! status: cluster-editor-apply-action
//! status: cluster-editor-policy-resolution-walk-up
//! status: cluster-editor-policy-require-review
//! status: suggestions-mode-move
//! status: suggestions-mode-tag
//! status: suggestions-apply-cmd
//! status: suggestions-rejection-history

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::errors::HikerError;
use crate::frontmatter;
use crate::ops::op_writes;
use crate::oplog::{OpLog, StageOutcome};
use crate::store::Store;
use crate::trees::types::{EditableNode, NodeKind, NodePolicy, Db, Error as TreesError};
use crate::vault::Vault;

/// Stable string the cluster-editor / triage surfaces use for the move
/// action label. Centralized here now that `core::staging` (which formerly
/// owned `ACTION_MOVE_NOTE`) is retired; the op-log `Rename` op-kind is the
/// underlying mechanism but the action label persists for the activity feed.
pub const ACTION_MOVE_NOTE: &str = "move_note";

/// Stable string the cluster-editor surface uses for `Proposal.action` on
/// tag-mode rows. Mirrors `ACTION_MOVE_NOTE` for the move-side actions.
///
/// status: suggestions-mode-tag
pub const ACTION_APPLY_TAG: &str = "apply_tag";

/// Default frontmatter field for cluster-driven tag writes per the
/// `suggestions.tag_field` config (default per `docs/suggestions.md`).
/// Sprint C keeps this hardcoded; full config eligibility lands when the
/// `[suggestions]` config section is wired.
pub const DEFAULT_TAG_FIELD: &str = "hiker.suggested_tags";

/// `surface` value stamped on every op-log op produced by the
/// cluster-editor Apply path. The triage surface uses `"triage"`.
pub const SURFACE_CLUSTER_EDITOR: &str = "cluster-editor";

/// `surface` value stamped on every op-log op produced by the saved-
/// tree triage classifier. Per `docs/suggestions.md` §"Saved-tree triage".
///
/// status: triage-staging-proposals
pub const SURFACE_TRIAGE: &str = "triage";

/// Outcome of one Apply pass over a tree. The counts let the UI render
/// "(N leaves skipped — no policy assigned, M leaves frozen)" per spec.
#[derive(Debug, Clone, Serialize, Default)]
pub struct ApplyOutcome {
    pub tree_id: String,
    /// Op-log pending op ids produced (one or more per `Tag` / `Move` leaf).
    pub staged_ids: Vec<String>,
    /// Leaves with `Move` policy that produced a row.
    pub moves: u32,
    /// Leaves with `Tag` policy that produced a row.
    pub tags: u32,
    /// Leaves under a `Freeze` policy (skipped).
    pub frozen: u32,
    /// Leaves with no resolved policy anywhere up the tree.
    pub unpolicied: u32,
    /// Leaves with no source-note path recorded (e.g. the underlying note
    /// was deleted between build and Apply).
    pub missing: u32,
}

/// Walk the tree, resolve each leaf's effective policy (walk-up rule),
/// and stage one op-log pending op per `Tag` / `Move` leaf. `Move` leaves
/// stage one pending `Rename` per note as a cross-document reorg batch
/// (`op-log-reorg-batch`, author `auto:cluster`); `Tag` leaves stage one
/// pending `SetFrontmatter`-shaped content op per note. Frozen + unpolicied
/// leaves are skipped (counted into the outcome so the UI can surface the
/// "(N leaves skipped)" note). Nothing reaches disk until the user accepts
/// via the batch-review pane.
///
/// All `Move` leaves share one reorg `batch_id` so the batch-review pane can
/// accept/reject them together; tag ops carry their own per-document batch.
/// `out.staged_ids` collects every minted op id across both kinds.
///
/// Per `suggestions-rejection-history`, the producer consults the
/// rejection log and skips any `(tree_member_fingerprint, note_id,
/// action)` combination that was rejected within the TTL. Sprint C wires
/// the log path but leaves the fingerprint coarse-grained (parent
/// cluster name) so the surface ships before the full
/// member-set-Jaccard helper lands.
///
/// status: suggestions-apply-cmd
/// status: op-log-reorg-batch
pub fn apply_tree(
    trees: &Db,
    tree_id: &str,
    vault: &Vault,
    store: &Store,
    log: &OpLog,
    history: Option<&RejectionHistory>,
) -> Result<ApplyOutcome, HikerError> {
    // Path-as-identity: each leaf carries its source note's rel-path
    // (`note_path`) directly, so no doc-id resolution is needed. `store` and
    // `log` are kept on the signature (callers + op staging below).
    let _ = store;
    let nodes = trees
        .list_nodes(tree_id)
        .map_err(|e: TreesError| HikerError::Io(e.to_string()))?;
    let by_id: HashMap<String, EditableNode> =
        nodes.iter().map(|n| (n.id.clone(), n.clone())).collect();
    let mut out = ApplyOutcome {
        tree_id: tree_id.to_string(),
        ..Default::default()
    };
    // Move leaves accumulate into one reorg batch staged at the end so the
    // batch-review pane treats the tree's moves as one display group.
    let mut moves: Vec<op_writes::ReorgMove> = Vec::new();
    for node in nodes.iter().filter(|n| matches!(n.kind, NodeKind::Leaf)) {
        let policy = resolve_effective_policy(&by_id, &node.id);
        let Some(policy) = policy else {
            out.unpolicied += 1;
            continue;
        };
        // Path-as-identity: the leaf carries the source note's rel-path
        // directly (no op-log doc-id round-trip).
        let rel = match &node.note_path {
            Some(p) => p.clone(),
            None => {
                out.missing += 1;
                continue;
            }
        };
        let parent_name = node
            .parent
            .as_ref()
            .and_then(|pid| by_id.get(pid))
            .map(|p| p.name.clone())
            .unwrap_or_default();
        match policy {
            NodePolicy::Freeze => {
                out.frozen += 1;
            }
            NodePolicy::Move { folder, .. } => {
                let action = ACTION_MOVE_NOTE;
                let fingerprint =
                    compute_fingerprint(&parent_name, &rel, action);
                if let Some(h) = history
                    && h.is_rejected(&fingerprint, &rel, action)
                {
                    continue;
                }
                let basename = rel.rsplit('/').next().unwrap_or(&rel);
                let folder_trim = folder.trim_end_matches('/');
                let target = if folder_trim.is_empty() {
                    basename.to_string()
                } else {
                    format!("{folder_trim}/{basename}")
                };
                if target == rel {
                    // No-op move (already in target folder).
                    continue;
                }
                moves.push(op_writes::ReorgMove { from: rel, to: target });
                out.moves += 1;
            }
            NodePolicy::Tag { slug, .. } => {
                let action = ACTION_APPLY_TAG;
                let fingerprint = compute_fingerprint(&parent_name, &rel, action);
                if let Some(h) = history
                    && h.is_rejected(&fingerprint, &rel, action)
                {
                    continue;
                }
                // Pre-compute the new file content with the tag merged into
                // the configured `tag_field`, then stage it as a pending
                // content op. The op-log diffs against current accepted, so a
                // no-op merge is dropped here before staging.
                let disk_text = vault
                    .read_file(&rel)
                    .map_err(|e| HikerError::Io(e.to_string()))?;
                let new_content = merge_tag_into_frontmatter(
                    &disk_text,
                    DEFAULT_TAG_FIELD,
                    &slug,
                )
                .map_err(|e| HikerError::Io(e.to_string()))?;
                if new_content == disk_text {
                    // Tag already present — no-op, skip.
                    continue;
                }
                let outcome = op_writes::stage_auto_content(
                    log,
                    vault,
                    PRODUCER_CLUSTER,
                    SURFACE_CLUSTER_EDITOR,
                    &rel,
                    &new_content,
                )?;
                out.staged_ids.extend(outcome.op_ids);
                out.tags += 1;
            }
        }
    }
    if !moves.is_empty() {
        let outcome = op_writes::stage_reorg_batch(
            log,
            vault,
            PRODUCER_CLUSTER,
            SURFACE_CLUSTER_EDITOR,
            &moves,
        )?;
        out.staged_ids.extend(outcome.op_ids);
    }
    Ok(out)
}

/// Walk up the tree from `node_id` until we hit a node with an explicit
/// policy; return that policy (cloned). `None` means "no policy
/// anywhere up to the root" — the note is left alone.
///
/// status: cluster-editor-policy-resolution-walk-up
/// status: cluster-editor-policy-any-level
/// status: cluster-editor-outlier-policy
pub fn resolve_effective_policy(
    by_id: &HashMap<String, EditableNode>,
    node_id: &str,
) -> Option<NodePolicy> {
    let mut current: Option<String> = Some(node_id.to_string());
    while let Some(id) = current {
        let Some(node) = by_id.get(&id) else { break };
        if let Some(p) = &node.policy {
            return Some(p.clone());
        }
        current = node.parent.clone();
    }
    None
}

/// Cheap deterministic fingerprint for a leaf's reject-history key.
/// Sprint C's coarse form: `hash(parent_cluster_name || "/" || note_path
/// || "/" || action)`. The spec's full member-set Jaccard recovery lands
/// later; this is enough to suppress immediate duplicate proposals on a
/// re-run.
///
/// status: suggestions-rejection-history
pub fn compute_fingerprint(parent_name: &str, note_path: &str, action: &str) -> String {
    let raw = format!("{parent_name}\x00{note_path}\x00{action}");
    crate::hash_string(&raw)
}

pub(crate) fn merge_tag_into_frontmatter(
    source: &str,
    tag_field: &str,
    slug: &str,
) -> Result<String, frontmatter::Error> {
    use serde_yml::Value as Yaml;
    let split = frontmatter::split(source);
    let body = split.body.to_string();
    let mut fm: Yaml = match split.frontmatter {
        Some(v) => v,
        None => Yaml::Mapping(Default::default()),
    };
    if !matches!(fm, Yaml::Mapping(_)) {
        fm = Yaml::Mapping(Default::default());
    }
    // Dot-path: walk down (or create) maps. `hiker.suggested_tags` →
    // map["hiker"] = map; map["suggested_tags"] = list.
    let segments: Vec<&str> = tag_field.split('.').collect();
    if segments.is_empty() {
        return Ok(source.to_string());
    }
    insert_tag_at_path(&mut fm, &segments, slug);
    let assembled = frontmatter::assemble(&fm, &body)?;
    Ok(assembled)
}

fn insert_tag_at_path(node: &mut serde_yml::Value, path: &[&str], slug: &str) {
    use serde_yml::{Mapping, Value as Yaml};
    if path.is_empty() {
        return;
    }
    let Yaml::Mapping(map) = node else {
        *node = Yaml::Mapping(Mapping::new());
        return insert_tag_at_path(node, path, slug);
    };
    let key = Yaml::String(path[0].to_string());
    if path.len() == 1 {
        // Terminal segment — must be a sequence.
        let entry = map.entry(key).or_insert(Yaml::Sequence(Vec::new()));
        if !matches!(entry, Yaml::Sequence(_)) {
            *entry = Yaml::Sequence(Vec::new());
        }
        if let Yaml::Sequence(seq) = entry {
            let already = seq.iter().any(|v| v.as_str() == Some(slug));
            if !already {
                seq.push(Yaml::String(slug.to_string()));
            }
        }
        return;
    }
    let child = map.entry(key).or_insert(Yaml::Mapping(Mapping::new()));
    if !matches!(child, Yaml::Mapping(_)) {
        *child = Yaml::Mapping(Mapping::new());
    }
    insert_tag_at_path(child, &path[1..], slug);
}

// ── Rejection history ───────────────────────────────────────────────
//
// status: suggestions-rejection-history
//
// `.hiker/suggestion-history.json` carries one row per rejected
// (fingerprint, note, action) tuple with a wall-clock timestamp. Default
// TTL 90 days per spec. Module-discipline: one JSON file, exclusive
// open / write, simple parse — durable enough for the small data set
// (rejections per vault are O(low-thousands) in the worst case).

const DEFAULT_TTL_DAYS: u64 = 90;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RejectionRow {
    pub fingerprint: String,
    pub note_path: String,
    pub action: String,
    pub rejected_at_ms: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct HistoryFile {
    #[serde(default)]
    rows: Vec<RejectionRow>,
}

pub struct RejectionHistory {
    path: PathBuf,
    ttl_ms: i64,
}

impl RejectionHistory {
    pub fn open(vault_root: &Path) -> Result<Self, HikerError> {
        let dir = vault_root.join(".hiker");
        std::fs::create_dir_all(&dir).map_err(|e| HikerError::Io(e.to_string()))?;
        let path = dir.join("suggestion-history.json");
        Ok(Self {
            path,
            ttl_ms: (DEFAULT_TTL_DAYS * 86_400_000) as i64,
        })
    }

    fn load(&self) -> Result<HistoryFile, HikerError> {
        if !self.path.exists() {
            return Ok(HistoryFile::default());
        }
        let text = std::fs::read_to_string(&self.path)
            .map_err(|e| HikerError::Io(e.to_string()))?;
        if text.trim().is_empty() {
            return Ok(HistoryFile::default());
        }
        serde_json::from_str(&text).map_err(|e| HikerError::Io(e.to_string()))
    }

    fn save(&self, file: &HistoryFile) -> Result<(), HikerError> {
        let text = serde_json::to_string_pretty(file)
            .map_err(|e| HikerError::Io(e.to_string()))?;
        std::fs::write(&self.path, text).map_err(|e| HikerError::Io(e.to_string()))
    }

    pub fn record_rejection(
        &self,
        fingerprint: &str,
        note_path: &str,
        action: &str,
    ) -> Result<(), HikerError> {
        let mut file = self.load()?;
        let now = now_ms();
        file.rows.retain(|r| !(r.fingerprint == fingerprint
            && r.note_path == note_path
            && r.action == action));
        file.rows.push(RejectionRow {
            fingerprint: fingerprint.to_string(),
            note_path: note_path.to_string(),
            action: action.to_string(),
            rejected_at_ms: now,
        });
        // Garbage-collect expired rows on every write.
        let cutoff = now - self.ttl_ms;
        file.rows.retain(|r| r.rejected_at_ms >= cutoff);
        self.save(&file)
    }

    pub fn is_rejected(&self, fingerprint: &str, note_path: &str, action: &str) -> bool {
        let Ok(file) = self.load() else { return false };
        let cutoff = now_ms() - self.ttl_ms;
        file.rows.iter().any(|r| {
            r.fingerprint == fingerprint
                && r.note_path == note_path
                && r.action == action
                && r.rejected_at_ms >= cutoff
        })
    }
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ── Multi-select Stage move/tag helpers ─────────────────────────────
//
// status: cluster-editor-multi-select-stage-move
// status: cluster-editor-multi-select-stage-tag
// status: op-log-reorg-batch
//
// One-shot batch from the multi-select toolbar: caller passes a set of
// leaf node IDs + the target folder (or tag slug); we stage op-log pending
// ops sharing one cross-document `batch_id`, authored `auto:cluster`. Moves
// produce one pending `Rename` per leaf (the reorg-batch shape per
// `op-log.md`'s "Multi-file reorganization"); tags produce one pending
// `SetFrontmatter`-shaped content edit per leaf. No policy mutation —
// distinct from the Apply path which reads policies off the tree.

/// `producer` label stamped on the `auto:<producer>` author for cluster-
/// editor staged moves/tags (yields `auto:cluster` per `op-log.md`'s
/// "Author classes").
pub const PRODUCER_CLUSTER: &str = "cluster";

/// `producer` label stamped on the `auto:<producer>` author for saved-tree
/// triage matches (yields `auto:triage` per `op-log.md`'s "Author classes").
/// Acceptance is a gate, not authorship — an auto-accepted and a
/// user-accepted triage op both keep `auto:triage`.
pub const PRODUCER_TRIAGE: &str = "triage";

pub struct StageMoveArgs<'a> {
    pub tree_id: &'a str,
    pub node_ids: &'a [String],
    pub target_folder: &'a str,
}

pub struct StageTagArgs<'a> {
    pub tree_id: &'a str,
    pub node_ids: &'a [String],
    pub tag_slug: &'a str,
}

/// Stage a multi-select move as a reorg batch of pending `Rename` ops on the
/// op log (`op-log-reorg-batch`). Resolves each selected leaf's note path,
/// computes its destination under `target_folder`, and stages one pending
/// `Rename` per moved note sharing a cross-document `batch_id` authored
/// `auto:cluster`. Returns the [`StageOutcome`] (batch id + op ids); the
/// cluster UI surfaces the batch for accept/reject via `flip_batch_status`.
///
/// status: cluster-editor-multi-select-stage-move
/// status: op-log-reorg-batch
pub fn stage_moves(
    trees: &Db,
    args: &StageMoveArgs<'_>,
    store: &Store,
    vault: &Vault,
    log: &OpLog,
) -> Result<StageOutcome, HikerError> {
    // Path-as-identity: leaves carry their source note's rel-path
    // (`note_path`) directly. `store` is kept on the signature for callers.
    let _ = store;
    let mut moves = Vec::new();
    for node_id in args.node_ids {
        let Some(node) = trees
            .get_node(args.tree_id, node_id)
            .map_err(|e| HikerError::Io(e.to_string()))?
        else {
            continue;
        };
        if !matches!(node.kind, NodeKind::Leaf) {
            continue;
        }
        let Some(rel) = node.note_path.clone() else { continue };
        let basename = rel.rsplit('/').next().unwrap_or(&rel);
        let folder_trim = args.target_folder.trim_end_matches('/');
        let target = if folder_trim.is_empty() {
            basename.to_string()
        } else {
            format!("{folder_trim}/{basename}")
        };
        if target == rel {
            continue;
        }
        moves.push(op_writes::ReorgMove { from: rel, to: target });
    }
    op_writes::stage_reorg_batch(log, vault, PRODUCER_CLUSTER, SURFACE_CLUSTER_EDITOR, &moves)
}

/// Stage a multi-select tag as a batch of pending `SetFrontmatter`-shaped
/// content edits on the op log. For each selected leaf, re-emits the note's
/// frontmatter with `tag_slug` merged into the configured tag field and
/// stages the new content as one pending op authored `auto:cluster`. Each
/// note's edit gets its own batch id (tags target distinct documents and the
/// op-log content-stage seam is per-document); the returned ids are the
/// staged op ids for the UI summary.
///
/// status: cluster-editor-multi-select-stage-tag
pub fn stage_tags(
    trees: &Db,
    args: &StageTagArgs<'_>,
    vault: &Vault,
    store: &Store,
    log: &OpLog,
) -> Result<Vec<String>, HikerError> {
    // Path-as-identity: see `apply_tree`. `store` is kept on the signature.
    let _ = store;
    let mut ids = Vec::new();
    for node_id in args.node_ids {
        let Some(node) = trees
            .get_node(args.tree_id, node_id)
            .map_err(|e| HikerError::Io(e.to_string()))?
        else {
            continue;
        };
        if !matches!(node.kind, NodeKind::Leaf) {
            continue;
        }
        let Some(rel) = node.note_path.clone() else { continue };
        let disk_text = vault
            .read_file(&rel)
            .map_err(|e| HikerError::Io(e.to_string()))?;
        let new_content = merge_tag_into_frontmatter(&disk_text, DEFAULT_TAG_FIELD, args.tag_slug)
            .map_err(|e| HikerError::Io(e.to_string()))?;
        if new_content == disk_text {
            continue;
        }
        let outcome = op_writes::stage_auto_content(
            log,
            vault,
            PRODUCER_CLUSTER,
            SURFACE_CLUSTER_EDITOR,
            &rel,
            &new_content,
        )?;
        ids.extend(outcome.op_ids);
    }
    Ok(ids)
}

// ── Triage classifier ────────────────────────────────────────────────
//
// status: triage-classifier-engine
// status: triage-staging-proposals
// status: triage-review-required
// status: triage-author-class
//
// Greedy beam-K descent over a saved Evergreen tree. Cheap, no LLM, no
// re-cluster. Produces zero or one op-log op per call (per the resolved
// matched-node policy) — pending when review is required, applied directly
// when not. The on-save / scheduled / modified pathways all funnel here;
// the producer (`cluster-editor-triage-on-save`) owns the trigger.

/// Per-vault triage configuration consumed by the classifier. Mirrors
/// `core::config::TriageConfig` but is duplicated here so the classifier
/// stays free of the full settings dependency (consumers pass the slice
/// they care about).
#[derive(Debug, Clone)]
pub struct TriageOpts {
    /// Composes with `policy.require_review` per the spec — either-true
    /// forces the row pending; both-false auto-accepts at insert time.
    pub review_required: bool,
    /// Source-folder safety boundary. `move_note` rows whose
    /// `source_path` does not start with this prefix are dropped at
    /// classifier time (per `docs/suggestions.md` §"The 'auto' in
    /// auto-organize is bounded").
    pub scope: String,
    /// Beam width for `place_beam_descent`. `2` is the spec default.
    pub beam_width: usize,
}

impl Default for TriageOpts {
    fn default() -> Self {
        Self {
            review_required: false,
            scope: "inbox/".to_string(),
            beam_width: 2,
        }
    }
}

/// Author class for the note we're triaging. Per `triage-author-class`,
/// this distinguishes user-authored notes from agent-authored notes so
/// triage doesn't auto-route an agent draft into the user's folder
/// structure without review. Agent-authored notes are always routed
/// pending for review regardless of `review_required`.
///
/// status: triage-author-class
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NoteAuthorClass {
    /// Note created/edited by the user directly. The default; eligible
    /// for auto-accept when `review_required = false`.
    #[default]
    User,
    /// Note authored by an agent (write_note / edit_note via MCP, ACP,
    /// etc.). Always routed pending so the user can review.
    Agent,
}

/// Outcome of one triage classifier run. Carries enough metadata for the
/// caller (worker) to decide whether to auto-accept and to log the
/// match.
#[derive(Debug, Clone, Default, Serialize)]
pub struct TriageOutcome {
    pub tree_id: String,
    pub source_path: String,
    /// `None` when no policy resolved (or the match dropped — outside
    /// scope, no centroid, etc.). Set to the first staged op id when a
    /// pending op (or auto-accepted op) was emitted to the op log.
    pub staged_id: Option<String>,
    /// The matched leaf node id (when descent succeeded).
    pub matched_node_id: Option<String>,
    pub confidence: f32,
    pub margin: f32,
    /// Effective gating that applies to this match: `policy.require_review
    /// || config.review_required || author_class == Agent`. The auto-
    /// accept path runs only when this is `false`.
    pub effective_requires_review: bool,
    /// Set when the match was dropped before staging (e.g. note path
    /// outside the configured triage scope; policy was `Freeze`; no
    /// effective policy walked up from the matched node).
    pub skip_reason: Option<&'static str>,
}

/// Inputs the triage classifier needs. The caller (worker) supplies:
/// - the note's id + vault-relative path + embedding (the on-save
///   handler reads these from `core::store`);
/// - the saved tree id (the worker is dispatched against one tree at a
///   time);
/// - per-tree handles to `Db` and the op log;
/// - the resolved `TriageOpts`.
pub struct TriageInput<'a> {
    pub tree_id: &'a str,
    pub note_id: &'a str,
    pub source_path: &'a str,
    pub embedding: &'a [f32],
    pub author_class: NoteAuthorClass,
    pub opts: &'a TriageOpts,
}

/// View into the in-memory tree the classifier needs. Implemented over
/// the `EditableNode` rows loaded from the tree's `.md`; built by
/// `build_tree_view` below.
struct LoadedTreeView<'a> {
    root_id: String,
    by_id: HashMap<String, &'a crate::trees::types::EditableNode>,
    nodes: HashMap<String, crate::cluster::Node>,
}

impl<'a> crate::cluster::TreeView for LoadedTreeView<'a> {
    fn root(&self) -> &crate::cluster::NodeId {
        &self.root_id
    }
    fn get(&self, id: &crate::cluster::NodeId) -> Option<&crate::cluster::Node> {
        self.nodes.get(id)
    }
}

/// Build a `TreeView` over the loaded nodes. Only nodes with a centroid
/// participate (leaves never carry one — the spec stores centroids on
/// clusters and outlier buckets only). Children are derived from
/// `parent_id` so the in-memory shape lines up with what
/// `place_beam_descent` expects.
/// Run the triage classifier for one note against one saved tree.
///
/// status: triage-classifier-engine
/// status: cluster-editor-triage-via-staging
pub fn triage_match(
    trees: &Db,
    vault: &Vault,
    store: &Store,
    log: &OpLog,
    input: &TriageInput<'_>,
) -> Result<TriageOutcome, HikerError> {
    let mut outcome = TriageOutcome {
        tree_id: input.tree_id.to_string(),
        source_path: input.source_path.to_string(),
        ..Default::default()
    };

    // Source-folder safety boundary. Notes outside the configured triage
    // scope are off-limits to triage moves; we drop the match entirely
    // rather than emit a no-op pending op. The check intentionally
    // applies even before the tree is walked — a tree built over
    // `research/` whose triage scope is `inbox/` doesn't get to fire on
    // `research/` notes (per `docs/suggestions.md`'s bounded-auto rule).
    let trimmed_scope = input.opts.scope.trim();
    if !trimmed_scope.is_empty()
        && !input.source_path.starts_with(trimmed_scope)
        && !input.source_path.starts_with(trimmed_scope.trim_end_matches('/'))
    {
        outcome.skip_reason = Some("outside-triage-scope");
        return Ok(outcome);
    }

    // Load the tree's nodes. The classifier needs `EditableNode`s rather
    // than `BuiltClusterNode`s because the user may have reshaped /
    // moved / re-parented since build time.
    let nodes = trees
        .list_nodes(input.tree_id)
        .map_err(|e| HikerError::Io(e.to_string()))?;
    if nodes.is_empty() {
        outcome.skip_reason = Some("empty-tree");
        return Ok(outcome);
    }
    let view = {
        let by_id: HashMap<String, &crate::trees::types::EditableNode> =
            nodes.iter().map(|n| (n.id.clone(), n)).collect();
        let mut children: HashMap<String, Vec<String>> = HashMap::new();
        for n in &nodes {
            if let Some(p) = &n.parent {
                children.entry(p.clone()).or_default().push(n.id.clone());
            }
        }
        let root = nodes.iter().find(|n| n.parent.is_none());
        match root {
            None => {
                outcome.skip_reason = Some("tree-has-no-root");
                return Ok(outcome);
            }
            Some(root) => {
                let mut view_nodes: HashMap<String, crate::cluster::Node> = HashMap::new();
                for n in &nodes {
                    let centroid = n.centroid.clone().unwrap_or_default();
                    let children_ids = children
                        .get(&n.id)
                        .cloned()
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|cid| {
                            // Only include children with centroids in the
                            // traversable beam set — leaves stick to their
                            // parent cluster and are matched by the cluster's
                            // own centroid (the spec calls for descent to
                            // leaves at the *cluster* level — a leaf here
                            // means a centroid-bearing terminal cluster).
                            by_id
                                .get(cid)
                                .and_then(|c| c.centroid.as_ref())
                                .map(|c| !c.is_empty())
                                .unwrap_or(false)
                        })
                        .collect();
                    view_nodes.insert(
                        n.id.clone(),
                        crate::cluster::Node {
                            id: n.id.clone(),
                            centroid,
                            children: children_ids,
                        },
                    );
                }
                LoadedTreeView {
                    root_id: root.id.clone(),
                    by_id,
                    nodes: view_nodes,
                }
            }
        }
    };

    let placement = match crate::cluster::tree::place_beam_descent(
        input.embedding,
        &view,
        input.opts.beam_width,
    ) {
        Some(p) => p,
        None => {
            outcome.skip_reason = Some("descent-empty");
            return Ok(outcome);
        }
    };
    outcome.matched_node_id = Some(placement.leaf_node_id.clone());
    outcome.confidence = placement.confidence;
    outcome.margin = placement.margin;

    // Walk up from the matched node to resolve the effective policy.
    let by_id_owned: HashMap<String, crate::trees::types::EditableNode> = nodes
        .iter()
        .cloned()
        .map(|n| (n.id.clone(), n))
        .collect();
    let policy = match resolve_effective_policy(&by_id_owned, &placement.leaf_node_id) {
        Some(p) => p,
        None => {
            outcome.skip_reason = Some("no-policy");
            return Ok(outcome);
        }
    };

    let policy_require_review = match &policy {
        NodePolicy::Tag { require_review, .. } => *require_review,
        NodePolicy::Move { require_review, .. } => *require_review,
        NodePolicy::Freeze => false,
    };
    // Per `triage-review-required`: either-true forces pending. Agent-
    // authored notes are always routed pending (per `triage-author-class`).
    let effective_requires_review = policy_require_review
        || input.opts.review_required
        || matches!(input.author_class, NoteAuthorClass::Agent);
    outcome.effective_requires_review = effective_requires_review;

    match policy {
        NodePolicy::Freeze => {
            outcome.skip_reason = Some("freeze");
            return Ok(outcome);
        }
        NodePolicy::Move {
            folder,
            require_review: _,
        } => {
            let basename = input.source_path.rsplit('/').next().unwrap_or(input.source_path);
            let folder_trim = folder.trim_end_matches('/');
            let target = if folder_trim.is_empty() {
                basename.to_string()
            } else {
                format!("{folder_trim}/{basename}")
            };
            if target == input.source_path {
                outcome.skip_reason = Some("noop-move");
                return Ok(outcome);
            }
            // Stage a one-move reorg batch authored `auto:triage`. The op
            // log's `Rename` op is the move mechanism; the batch is a
            // display group of one. When the placement auto-accepts (no
            // effective review required) the batch is applied immediately,
            // mirroring the spec's `status = accepted` direct-apply path.
            let staged = op_writes::stage_reorg_batch(
                log,
                vault,
                PRODUCER_TRIAGE,
                SURFACE_TRIAGE,
                &[op_writes::ReorgMove {
                    from: input.source_path.to_string(),
                    to: target,
                }],
            )?;
            if !effective_requires_review {
                op_writes::flip_batch_status(log, &staged.batch_id, true)?;
            }
            outcome.staged_id = staged.op_ids.into_iter().next();
        }
        NodePolicy::Tag {
            slug,
            require_review: _,
        } => {
            // Pre-compute the new file content with the tag merged into the
            // configured field, then stage it as a pending content op
            // authored `auto:triage`. A no-op merge is dropped before
            // staging.
            let disk_text = vault
                .read_file(input.source_path)
                .map_err(|e| HikerError::Io(e.to_string()))?;
            let new_content =
                merge_tag_into_frontmatter(&disk_text, DEFAULT_TAG_FIELD, &slug)
                    .map_err(|e| HikerError::Io(e.to_string()))?;
            if new_content == disk_text {
                outcome.skip_reason = Some("tag-already-present");
                return Ok(outcome);
            }
            let staged = op_writes::stage_auto_content(
                log,
                vault,
                PRODUCER_TRIAGE,
                SURFACE_TRIAGE,
                input.source_path,
                &new_content,
            )?;
            if !effective_requires_review {
                op_writes::flip_batch_status(log, &staged.batch_id, true)?;
            }
            outcome.staged_id = staged.op_ids.into_iter().next();
        }
    }
    // `note_id` and `store` are not consulted directly here — the input
    // already carries the path + embedding. Touching them keeps the
    // borrow checker happy and reserves the slot for future enrichment
    // (e.g. embedding-cache lookup keyed by note_id).
    let _ = (input.note_id, store, &view.by_id);
    Ok(outcome)
}

/// Borrowed bundle of the four storage handles plus the per-note
/// inputs that `triage_all_saved_trees` needs. Kept private to this
/// module; the public surface is the function below.
pub struct TriageBatch<'a> {
    pub trees: &'a Db,
    pub vault: &'a Vault,
    pub store: &'a Store,
    pub log: &'a OpLog,
    pub note_id: &'a str,
    pub source_path: &'a str,
    pub embedding: &'a [f32],
    pub author_class: NoteAuthorClass,
    pub opts: &'a TriageOpts,
}

/// Run triage against every saved-as-triage tree. The on-save hook
/// (`cluster-editor-triage-on-save`) iterates this list per note save.
/// Returns one outcome per tree the note was evaluated against.
///
/// status: cluster-editor-triage-on-save
pub fn triage_all_saved_trees(
    batch: &TriageBatch<'_>,
) -> Result<Vec<TriageOutcome>, HikerError> {
    let rows = batch
        .trees
        .list_trees()
        .map_err(|e| HikerError::Io(e.to_string()))?;
    let mut out: Vec<TriageOutcome> = Vec::new();
    for row in rows {
        if row.state != "saved-as-triage" {
            continue;
        }
        // status: cluster-build-scope-source-types
        // Honor each tree's source-types filter at on-save time: a tree
        // built only over .md files shouldn't classify a .txt note. The
        // filter lives on `cluster_trees.scope` as part of the BuildScope
        // shape (empty filter = match every indexable extension, which
        // matches legacy behavior). A scope_json that fails to parse is
        // skipped — the tree is in a corrupt state and we don't want
        // triage to silently fall back to "match everything."
        if let Ok(scope) =
            serde_json::from_str::<crate::cluster::BuildScope>(&row.scope_json)
            && !scope.matches_path(batch.source_path)
        {
            continue;
        }
        let outcome = triage_match(
            batch.trees,
            batch.vault,
            batch.store,
            batch.log,
            &TriageInput {
                tree_id: &row.id,
                note_id: batch.note_id,
                source_path: batch.source_path,
                embedding: batch.embedding,
                author_class: batch.author_class,
                opts: batch.opts,
            },
        )?;
        out.push(outcome);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trees::types::{NodeInsert, NodeKind, NodePolicy, TreeInsert, Db};
    use tempfile::TempDir;

    fn mk_tree(td: &TempDir) -> (Db, String) {
        let trees = Db::new(
            std::sync::Arc::new(crate::oplog::OpLog::open(td.path()).unwrap()),
            std::sync::Arc::new(crate::vault::Vault::open(td.path()).unwrap()),
        )
        .unwrap();
        let id = trees
            .insert_tree(TreeInsert {
                id: Some("t".into()),
                name: "t".into(),
                source: "one-shot".into(),
                state: "draft".into(),
                scope_json: "{}".into(),
                method_json: "{}".into(),
                vault_snapshot: None,
            })
            .unwrap();
        (trees, id)
    }

    fn leaf(id: &str, parent: &str, note_id: &str) -> NodeInsert {
        NodeInsert {
            node_id: id.into(),
            parent_id: Some(parent.into()),
            kind: NodeKind::Leaf,
            note_id: Some(note_id.into()),
            name: id.into(),
            summary: String::new(),
            user_edited_name: false,
            user_edited_summary: false,
            policy: None,
            centroid: None,
            confidence: 1.0,
            summary_membership_churn: 0,
        }
    }

    fn cluster(id: &str, parent: Option<&str>, policy: Option<NodePolicy>) -> NodeInsert {
        NodeInsert {
            node_id: id.into(),
            parent_id: parent.map(std::convert::Into::into),
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
        }
    }

    #[test]
    fn walk_up_resolves_to_nearest_ancestor_policy() {
        let td = TempDir::new().unwrap();
        let (trees, tid) = mk_tree(&td);
        trees
            .insert_nodes(
                &tid,
                &[
                    cluster("root", None, None),
                    cluster(
                        "mid",
                        Some("root"),
                        Some(NodePolicy::Tag {
                            slug: "research".into(),
                            require_review: false,
                        }),
                    ),
                    leaf("l1", "mid", "note-a"),
                ],
            )
            .unwrap();
        let nodes = trees.list_nodes(&tid).unwrap();
        let by_id: HashMap<_, _> = nodes.into_iter().map(|n| (n.id.clone(), n)).collect();
        let p = resolve_effective_policy(&by_id, "l1").unwrap();
        match p {
            NodePolicy::Tag { slug, .. } => assert_eq!(slug, "research"),
            _ => panic!("expected Tag"),
        }
    }

    #[test]
    fn merge_tag_into_frontmatter_creates_dotted_path() {
        let src = "# heading\n\nbody\n";
        let out = merge_tag_into_frontmatter(src, "hiker.suggested_tags", "research").unwrap();
        assert!(out.starts_with("---"));
        assert!(out.contains("hiker:"));
        assert!(out.contains("suggested_tags:"));
        assert!(out.contains("research"));
    }

    #[test]
    fn merge_tag_is_idempotent() {
        let src = "---\nhiker:\n  suggested_tags: [research]\n---\nbody\n";
        let out = merge_tag_into_frontmatter(src, "hiker.suggested_tags", "research").unwrap();
        // Body unchanged (already had the tag).
        let count = out.matches("research").count();
        assert_eq!(count, 1, "tag should only appear once: {out}");
    }

    #[test]
    fn rejection_history_round_trips_with_ttl() {
        let td = TempDir::new().unwrap();
        let h = RejectionHistory::open(td.path()).unwrap();
        assert!(!h.is_rejected("fp", "a.md", "move_note"));
        h.record_rejection("fp", "a.md", "move_note").unwrap();
        assert!(h.is_rejected("fp", "a.md", "move_note"));
        assert!(!h.is_rejected("fp", "b.md", "move_note"));
    }

    #[test]
    fn fingerprint_is_deterministic() {
        let a = compute_fingerprint("parent", "a.md", "move_note");
        let b = compute_fingerprint("parent", "a.md", "move_note");
        let c = compute_fingerprint("parent", "a.md", "apply_tag");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    /// Open an op log on the tempdir, creating the `.hiker` dir first.
    fn open_log(td: &TempDir) -> OpLog {
        std::fs::create_dir_all(td.path().join(".hiker")).unwrap();
        OpLog::open(td.path()).unwrap()
    }

    // status: cluster-editor-apply-action
    // status: suggestions-apply-cmd
    //
    // End-to-end smoke for `apply_tree`: build a tree in tempfile-backed
    // `Db` / `Store` / `Vault` / `OpLog`, attach a `Tag` policy on one
    // cluster + a `Move` policy on another, run `apply_tree`, and verify
    // both stage as op-log pending ops with the right surface / action.
    // Asserts the tree's `state` is *not* advanced by the core mechanic —
    // state flip to `applied` is the UI's responsibility once every emitted
    // op resolves.
    #[test]
    fn apply_tree_stages_tag_and_move_pending_ops() {
        use crate::store::dto::NoteUpsert;
        use crate::store::Store;
        use crate::vault::Vault;

        let td = TempDir::new().unwrap();
        // Seed the on-disk notes so `Vault::read_file` (Tag path) and
        // `Store::path_for_id` (both paths) resolve.
        std::fs::write(td.path().join("a.md"), "# a\nbody-a\n").unwrap();
        std::fs::create_dir_all(td.path().join("inbox")).unwrap();
        std::fs::write(td.path().join("inbox/b.md"), "# b\nbody-b\n").unwrap();

        let vault = Vault::open(td.path()).unwrap();
        let mut store = Store::open(td.path()).unwrap();
        let log = open_log(&td);

        // Path-as-identity: leaves carry the source note's rel-path directly,
        // which `apply_tree` reads as the vault path. Bootstrap seeds the
        // op-log so the staging path the suggestions write through resolves;
        // the resolved doc_ids double as the `notes.id` for the store upserts.
        let _ = crate::ops::op_writes::bootstrap(&vault, &log).unwrap_or(0);
        let note_a = log.doc_id_for_path("a.md").unwrap().expect("seeded");
        let note_b = log
            .doc_id_for_path("inbox/b.md")
            .unwrap()
            .expect("seeded");

        // Index the two notes so the structural query (used elsewhere)
        // also has rows. Use the op-log's doc_ids as `notes.id` per
        // path-as-identity.
        store
            .upsert_note(&NoteUpsert {
                id: &note_a,
                path: "a.md",
                content_hash: "h-a",
                mtime: 1,
                size: 1,
                indexed_at: 1,
                embedder_version: "t",
                chunks: vec![],
            })
            .unwrap();
        store
            .upsert_note(&NoteUpsert {
                id: &note_b,
                path: "inbox/b.md",
                content_hash: "h-b",
                mtime: 1,
                size: 1,
                indexed_at: 1,
                embedder_version: "t",
                chunks: vec![],
            })
            .unwrap();

        // Tree shape:
        //   root
        //     tag-cluster      [Tag(slug=research)]
        //       leaf-a         note-a
        //     move-cluster     [Move(folder=archive)]
        //       leaf-b         note-b
        let (trees, tid) = mk_tree(&td);
        trees
            .insert_nodes(
                &tid,
                &[
                    cluster("root", None, None),
                    cluster(
                        "tag-cluster",
                        Some("root"),
                        Some(NodePolicy::Tag {
                            slug: "research".into(),
                            require_review: false,
                        }),
                    ),
                    leaf("leaf-a", "tag-cluster", "a.md"),
                    cluster(
                        "move-cluster",
                        Some("root"),
                        Some(NodePolicy::Move {
                            folder: "archive".into(),
                            require_review: true,
                        }),
                    ),
                    leaf("leaf-b", "move-cluster", "inbox/b.md"),
                ],
            )
            .unwrap();

        let outcome = apply_tree(&trees, &tid, &vault, &store, &log, None).unwrap();
        assert_eq!(outcome.tree_id, tid);
        assert_eq!(outcome.tags, 1, "one Tag leaf");
        assert_eq!(outcome.moves, 1, "one Move leaf");
        assert_eq!(outcome.frozen, 0);
        assert_eq!(outcome.unpolicied, 0);
        assert_eq!(outcome.missing, 0);
        assert_eq!(outcome.staged_ids.len(), 2);

        // Pull the pending ops back and bucket by action label.
        let rows = op_writes::list_pending_proposals(&log).unwrap();
        assert_eq!(rows.len(), 2, "exactly two pending ops: {rows:?}");

        let move_row = rows
            .iter()
            .find(|r| r.action == "rename")
            .expect("rename op present");
        assert_eq!(move_row.surface, SURFACE_CLUSTER_EDITOR);
        // The rename is still pending, so the doc resolves to its current
        // (accepted) path; the target lives in the op until accepted.
        assert_eq!(move_row.target_path, "inbox/b.md");

        // The tag op is the non-rename op; it targets the original path
        // (no move). Its op-kind is `write_note` here because the source
        // note had no frontmatter fence to land "inside" — the merge
        // prepends a fresh one, so the op-log labels it a body replace.
        let tag_row = rows
            .iter()
            .find(|r| r.action != "rename")
            .expect("tag content op present");
        assert_eq!(tag_row.surface, SURFACE_CLUSTER_EDITOR);
        assert_eq!(tag_row.target_path, "a.md");

        // `apply_tree` doesn't advance tree state on its own.
        let row = trees.get_tree(&tid).unwrap().expect("tree row");
        assert_eq!(row.state, "draft");
    }

    // status: triage-classifier-engine, triage-staging-proposals
    // status: triage-review-required, triage-author-class
    //
    // Triage classifier smoke: build a tiny saved tree with a Move-policy
    // child, feed a matching embedding, and verify the op auto-accepts (no
    // effective review required) — landing the rename on `accepted` so the
    // doc's pending-view path is the target. Also exercises the
    // source-folder safety boundary and the agent-author auto-pending rule.
    #[test]
    fn triage_auto_accepts_move_when_no_review_required() {
        use crate::cluster::algo::l2_normalize;
        use crate::store::dto::NoteUpsert;
        use crate::store::Store;
        use crate::vault::Vault;

        let td = TempDir::new().unwrap();
        std::fs::create_dir_all(td.path().join("inbox")).unwrap();
        std::fs::write(td.path().join("inbox/n.md"), "# n\nbody\n").unwrap();
        let vault = Vault::open(td.path()).unwrap();
        let mut store = Store::open(td.path()).unwrap();
        let log = open_log(&td);
        store
            .upsert_note(&NoteUpsert {
                id: "note-n",
                path: "inbox/n.md",
                content_hash: "h",
                mtime: 1,
                size: 1,
                indexed_at: 1,
                embedder_version: "t",
                chunks: vec![],
            })
            .unwrap();
        let (trees, tid) = mk_tree(&td);
        // Root has centroid pointing (1,1); childA carries Move policy
        // and matches a (1, 0.05) query.
        let root_cent = l2_normalize(&[1.0, 1.0]);
        let a_cent = l2_normalize(&[1.0, 0.05]);
        let mut root = cluster("root", None, None);
        root.centroid = Some(root_cent);
        let mut a = cluster(
            "a",
            Some("root"),
            Some(NodePolicy::Move {
                folder: "archive".into(),
                require_review: false,
            }),
        );
        a.centroid = Some(a_cent);
        trees.insert_nodes(&tid, &[root, a]).unwrap();
        trees.set_tree_state(&tid, "saved-as-triage").unwrap();

        let opts = TriageOpts::default();
        let outcome = triage_match(&trees, &vault, &store, &log, &TriageInput {
                tree_id: &tid,
                note_id: "note-n",
                source_path: "inbox/n.md",
                embedding: &[1.0, 0.05],
                author_class: NoteAuthorClass::User,
                opts: &opts,
            },
        )
        .unwrap();
        assert!(outcome.staged_id.is_some(), "expected a staged op id");
        assert_eq!(outcome.matched_node_id.as_deref(), Some("a"));
        assert!(!outcome.effective_requires_review, "auto-accept path");

        // The move auto-accepted → no pending ops remain, and the doc now
        // lives at the target path on `accepted`.
        let pending = op_writes::list_pending_proposals(&log).unwrap();
        assert!(pending.is_empty(), "auto-accepted move leaves no pending: {pending:?}");
        assert!(
            log.doc_id_for_path("archive/n.md").unwrap().is_some(),
            "doc moved to target path on accepted"
        );
    }

    // Review-required triage leaves the op pending rather than applying it.
    #[test]
    fn triage_stages_pending_when_review_required() {
        use crate::cluster::algo::l2_normalize;
        use crate::store::dto::NoteUpsert;
        use crate::store::Store;
        use crate::vault::Vault;

        let td = TempDir::new().unwrap();
        std::fs::create_dir_all(td.path().join("inbox")).unwrap();
        std::fs::write(td.path().join("inbox/n.md"), "# n\nbody\n").unwrap();
        let vault = Vault::open(td.path()).unwrap();
        let mut store = Store::open(td.path()).unwrap();
        let log = open_log(&td);
        store
            .upsert_note(&NoteUpsert {
                id: "note-n",
                path: "inbox/n.md",
                content_hash: "h",
                mtime: 1,
                size: 1,
                indexed_at: 1,
                embedder_version: "t",
                chunks: vec![],
            })
            .unwrap();
        let (trees, tid) = mk_tree(&td);
        let mut root = cluster("root", None, None);
        root.centroid = Some(l2_normalize(&[1.0, 1.0]));
        let mut a = cluster(
            "a",
            Some("root"),
            Some(NodePolicy::Move {
                folder: "archive".into(),
                require_review: false,
            }),
        );
        a.centroid = Some(l2_normalize(&[1.0, 0.05]));
        trees.insert_nodes(&tid, &[root, a]).unwrap();

        let opts = TriageOpts {
            review_required: true,
            ..Default::default()
        };
        let outcome = triage_match(&trees, &vault, &store, &log, &TriageInput {
                tree_id: &tid,
                note_id: "note-n",
                source_path: "inbox/n.md",
                embedding: &[1.0, 0.05],
                author_class: NoteAuthorClass::User,
                opts: &opts,
            },
        )
        .unwrap();
        assert!(outcome.staged_id.is_some());
        assert!(outcome.effective_requires_review);
        let pending = op_writes::list_pending_proposals(&log).unwrap();
        assert_eq!(pending.len(), 1, "review-required move stays pending");
        assert_eq!(pending[0].action, "rename");
        assert_eq!(pending[0].surface, SURFACE_TRIAGE);
    }

    #[test]
    fn triage_drops_match_outside_scope() {
        use crate::cluster::algo::l2_normalize;
        use crate::store::Store;
        use crate::vault::Vault;

        let td = TempDir::new().unwrap();
        std::fs::create_dir_all(td.path().join("research")).unwrap();
        std::fs::write(td.path().join("research/r.md"), "# r\n").unwrap();
        let vault = Vault::open(td.path()).unwrap();
        let store = Store::open(td.path()).unwrap();
        let log = open_log(&td);
        let (trees, tid) = mk_tree(&td);
        let mut root = cluster("root", None, None);
        root.centroid = Some(l2_normalize(&[1.0, 0.0]));
        let mut a = cluster(
            "a",
            Some("root"),
            Some(NodePolicy::Move {
                folder: "archive".into(),
                require_review: false,
            }),
        );
        a.centroid = Some(l2_normalize(&[1.0, 0.0]));
        trees.insert_nodes(&tid, &[root, a]).unwrap();

        let opts = TriageOpts {
            scope: "inbox/".into(),
            ..Default::default()
        };
        let outcome = triage_match(&trees, &vault, &store, &log, &TriageInput {
                tree_id: &tid,
                note_id: "x",
                source_path: "research/r.md",
                embedding: &[1.0, 0.0],
                author_class: NoteAuthorClass::User,
                opts: &opts,
            },
        )
        .unwrap();
        assert!(outcome.staged_id.is_none());
        assert_eq!(outcome.skip_reason, Some("outside-triage-scope"));
    }

    #[test]
    fn triage_agent_author_forces_pending() {
        use crate::cluster::algo::l2_normalize;
        use crate::store::Store;
        use crate::vault::Vault;

        let td = TempDir::new().unwrap();
        std::fs::create_dir_all(td.path().join("inbox")).unwrap();
        std::fs::write(td.path().join("inbox/g.md"), "# g\n").unwrap();
        let vault = Vault::open(td.path()).unwrap();
        let store = Store::open(td.path()).unwrap();
        let log = open_log(&td);
        let (trees, tid) = mk_tree(&td);
        let mut root = cluster("root", None, None);
        root.centroid = Some(l2_normalize(&[1.0, 0.0]));
        let mut a = cluster(
            "a",
            Some("root"),
            Some(NodePolicy::Move {
                folder: "archive".into(),
                require_review: false,
            }),
        );
        a.centroid = Some(l2_normalize(&[1.0, 0.0]));
        trees.insert_nodes(&tid, &[root, a]).unwrap();

        let opts = TriageOpts::default();
        let outcome = triage_match(&trees, &vault, &store, &log, &TriageInput {
                tree_id: &tid,
                note_id: "g",
                source_path: "inbox/g.md",
                embedding: &[1.0, 0.0],
                author_class: NoteAuthorClass::Agent,
                opts: &opts,
            },
        )
        .unwrap();
        assert!(outcome.effective_requires_review,
            "agent-authored notes always require review");
    }
}
