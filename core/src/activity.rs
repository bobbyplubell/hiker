//! Change history + unified activity feed — projections over the op log.
//!
//! The op log is the changelog: accepted ops are committed history,
//! pending ops are unreviewed proposals. This module owns the user-facing
//! projection layer: the `ChangeRow` DTO and the accepted-op feed
//! (`AcceptedFeed`) plus the merged `Activity` feed that interleaves
//! accepted + pending ops into one stream.
//!
//! Pure projection over `core::oplog`: no on-disk state, no writes, no GC.
//! Constructed at vault open with an `Arc<OpLog>`. Consumers (activity
//! detail page, status-bar version dropdown, queue-bar combined badge,
//! per-file history) call one of `list` / `list_for_path` / `count` or the
//! `AcceptedFeed` methods and render directly off the DTOs.
//
// status: activity-feed-merged
// status: activity-feed-module
// status: activity-feed-unified-item
// status: activity-feed-source-filter
// status: activity-feed-staging-metadata
// status: activity-feed-merged-query
// status: activity-feed-merge-ordering
// status: changes-query-api

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::oplog::error::Error as OpLogError;
use crate::oplog::meta::{Filter as MetaFilter, OpMetadata, OpStatus};
use crate::oplog::OpLog;

#[derive(Debug, Error)]
pub enum Error {
    #[error("op-log: {0}")]
    OpLog(#[from] OpLogError),
}

// ── Change-row projection (changes-query-api) ──────────────────────────
//
// A `ChangeRow` is a projection of an accepted `op_metadata` row. The DTO
// shape is consumed by the host commands and the activity-feed UI.

/// One accepted-op row, projected for the activity feed and history
/// surfaces. `content` is not carried — consumers materialize content on
/// demand via `core::ops::op_writes::content_at_op`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeRow {
    /// The side table has no monotonic int id; this field holds
    /// `timestamp_ms` so `id`-keyed UI ordering keeps working. The stable
    /// handle is the ulid `op_id`, carried in `metadata`.
    pub id: i64,
    pub timestamp_ms: i64,
    pub path: String,
    pub op: ChangeOp,
    pub author: String,
    /// Originating producer surface (`"triage"`, `"chat"`, …), or `None`
    /// for direct writes (user typing, plain saves) with no producer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface: Option<String>,
    pub content_hash: Option<String>,
    pub rename_from: Option<String>,
    pub metadata: serde_json::Value,
    /// True when this row is the most recent change for its `path` within
    /// the returned window. Stamped by the listing query.
    #[serde(default)]
    pub is_current: bool,
    /// Coarse author classification derived from `author`. The wire format
    /// of `author` is `class[:identifier]`; UIs and filter pills only need
    /// the class half, so it's surfaced as a typed enum here.
    #[serde(default)]
    pub author_class: AuthorClass,
}

/// Coarse author taxonomy from `design.md`'s authorship trichotomy
/// (user / agent / sync / import / auto). The wire format of
/// `ChangeRow.author` is `class[:identifier]` — e.g. `agent:claude-code`,
/// `sync:phone`. `Other` is a forward-compat slot for unknown classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AuthorClass {
    #[default]
    User,
    Agent,
    Sync,
    Import,
    /// Internal automation write. Wire form is `auto:<producer>` — e.g.
    /// `auto:triage` for the saved-tree triage classifier.
    Auto,
    Other,
}

impl AuthorClass {
    /// Parse the class prefix from a wire-format `author` string.
    pub fn from_author(author: &str) -> Self {
        let class = match author.find(':') {
            Some(i) => &author[..i],
            None => author,
        };
        match class {
            "user" => AuthorClass::User,
            "agent" => AuthorClass::Agent,
            "sync" => AuthorClass::Sync,
            "import" => AuthorClass::Import,
            "auto" => AuthorClass::Auto,
            _ => AuthorClass::Other,
        }
    }
}

/// Coarse user-facing op classification (per `op-log-op-shape`
/// "Projections").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeOp {
    Created,
    Modified,
    Deleted,
    Renamed,
}

/// Map an op-log `op_kind` wire string to the coarse `ChangeOp`:
/// `create → Created`, `replace` / `set_frontmatter → Modified`,
/// `tombstone → Deleted`, `rename → Renamed`. Unknown kinds fall back to
/// `Modified`.
///
/// status: changes-query-api
pub fn op_kind_to_change_op(op_kind: &str) -> ChangeOp {
    match op_kind {
        "create" => ChangeOp::Created,
        "tombstone" => ChangeOp::Deleted,
        "rename" => ChangeOp::Renamed,
        // "replace" | "set_frontmatter" and anything unknown.
        _ => ChangeOp::Modified,
    }
}

/// Project one accepted `OpMetadata` row to a `ChangeRow`. `path` is the
/// resolved current path for the op's `doc_id`. The op's ulid `op_id` and
/// `doc_id` are preserved in the row's `metadata` (the `id: i64` field
/// can't hold a ulid) so content / rollback consumers can recover them.
/// `is_current` is left `false`; the listing query stamps it.
fn change_row_from_meta(meta: OpMetadata, path: String) -> ChangeRow {
    let author_class = AuthorClass::from_author(&meta.author.as_wire());
    // Coerce non-object metadata to an object so the ulid handles always
    // land — the `ChangeRow.id: i64` field can't carry a ulid.
    let mut metadata = match meta.metadata {
        serde_json::Value::Object(map) => serde_json::Value::Object(map),
        _ => serde_json::Value::Object(serde_json::Map::new()),
    };
    if let serde_json::Value::Object(map) = &mut metadata {
        map.insert(
            "op_id".to_string(),
            serde_json::Value::String(meta.op_id.clone()),
        );
        map.insert(
            "doc_id".to_string(),
            serde_json::Value::String(meta.doc_id.clone()),
        );
    }
    ChangeRow {
        id: meta.timestamp_ms,
        timestamp_ms: meta.timestamp_ms,
        path,
        op: op_kind_to_change_op(&meta.op_kind),
        author: meta.author.as_wire(),
        surface: meta.surface,
        content_hash: None,
        rename_from: meta.rename_from,
        metadata,
        is_current: false,
        author_class,
    }
}

/// The accepted-op history projection over `core::oplog`. Returns
/// `ChangeRow` DTOs; `doc_id`-less filtering happens server-side in the
/// substrate, path resolution and `is_current` stamping happen here.
///
/// status: changes-query-api
pub struct AcceptedFeed<'a> {
    log: &'a OpLog,
}

impl<'a> AcceptedFeed<'a> {
    pub const fn new(log: &'a OpLog) -> Self {
        Self { log }
    }

    /// Most recent N accepted ops across the whole vault, newest first.
    pub fn recent(&self, limit: usize) -> Result<Vec<ChangeRow>, OpLogError> {
        self.query(&MetaFilter {
            status: Some(OpStatus::Accepted),
            limit: Some(limit),
            ..MetaFilter::default()
        })
    }

    /// Most recent N accepted ops whose author matches the SQL LIKE pattern
    /// (e.g. `agent:%`). Translates the pattern to the substrate's
    /// `author_class` prefix filter when it ends in `:%`, else an exact
    /// match on the bare author.
    pub fn recent_by_author(
        &self,
        author_pattern: &str,
        limit: usize,
    ) -> Result<Vec<ChangeRow>, OpLogError> {
        let mut filter = MetaFilter {
            status: Some(OpStatus::Accepted),
            limit: Some(limit),
            ..MetaFilter::default()
        };
        if let Some(class) = author_pattern.strip_suffix(":%") {
            filter.author_class = Some(class.to_string());
        } else if let Some(class) = author_pattern.strip_suffix('%') {
            filter.author_class = Some(class.to_string());
        } else {
            filter.author_exact = Some(author_pattern.to_string());
        }
        self.query(&filter)
    }

    /// Most recent N accepted ops for a single path, newest first. Resolves
    /// the path to its doc_id and scopes the substrate query to it.
    pub fn history_for_path(
        &self,
        path: &str,
        limit: usize,
    ) -> Result<Vec<ChangeRow>, OpLogError> {
        let Some(doc_id) = self.log.doc_id_for_path(path)? else {
            return Ok(Vec::new());
        };
        self.query(&MetaFilter {
            doc_id: Some(doc_id),
            status: Some(OpStatus::Accepted),
            limit: Some(limit),
            ..MetaFilter::default()
        })
    }

    /// Run a substrate metadata query and project each row to a `ChangeRow`,
    /// resolving each op's `doc_id` to a path and stamping `is_current` on
    /// the newest accepted op per path within the returned window.
    fn query(&self, filter: &MetaFilter) -> Result<Vec<ChangeRow>, OpLogError> {
        let metas = self.log.query_metadata(filter)?;
        let mut rows: Vec<ChangeRow> = Vec::with_capacity(metas.len());
        for meta in metas {
            let path = self
                .log
                .path_for_doc(&meta.doc_id)?
                .unwrap_or_else(|| meta.doc_id.clone());
            rows.push(change_row_from_meta(meta, path));
        }
        stamp_is_current(&mut rows);
        Ok(rows)
    }
}

/// Stamp `is_current = true` on the newest row (by `timestamp_ms`, ulid
/// `op_id` tiebreak) for each distinct path in the window.
fn stamp_is_current(rows: &mut [ChangeRow]) {
    use std::collections::HashMap;
    let mut best: HashMap<String, usize> = HashMap::new();
    for (i, row) in rows.iter().enumerate() {
        match best.get(&row.path) {
            Some(&j) => {
                let cur = &rows[j];
                let newer = row.timestamp_ms > cur.timestamp_ms
                    || (row.timestamp_ms == cur.timestamp_ms
                        && op_id_of(row) > op_id_of(cur));
                if newer {
                    best.insert(row.path.clone(), i);
                }
            }
            None => {
                best.insert(row.path.clone(), i);
            }
        }
    }
    for &i in best.values() {
        rows[i].is_current = true;
    }
}

/// The ulid `op_id` carried in a projected row's `metadata`, or empty when
/// absent.
fn op_id_of(row: &ChangeRow) -> &str {
    row.metadata
        .get("op_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
}

/// Which side of the op log the query should pull from.
///
/// `ChangesOnly` and `PendingOnly` short-circuit to the accepted-op feed /
/// the pending-op projection respectively and wrap each row in the unified
/// envelope. `Merged` runs both and interleaves by timestamp.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    ChangesOnly,
    PendingOnly,
    #[default]
    Merged,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Filter {
    #[serde(default)]
    pub source: Source,
    pub limit: usize,
    /// Optional SQL LIKE pattern matched against `ChangeRow.author`
    /// (e.g. `agent:%`). Pending items match only the synthetic
    /// `pending` author.
    #[serde(default)]
    pub author_pattern: Option<String>,
    /// Exclude items older than this (unix millis).
    #[serde(default)]
    pub since_ms: Option<i64>,
}

impl Default for Filter {
    fn default() -> Self {
        Self {
            source: Source::default(),
            limit: 200,
            author_pattern: None,
            since_ms: None,
        }
    }
}

/// Short label material — UI formats display strings off this.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Summary {
    Change { op: ChangeOp },
    Pending { surface: String, action: String },
}

/// Per-kind payload. Tagged enum so the frontend can switch on `kind`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Payload {
    Change(ChangeRow),
    Pending(PendingItem),
}

/// Pending-op projection for the unified feed. Mirrors the columns the
/// `pending.json` index already carries, with `metadata` preserved verbatim
/// so MCP-specific or trail-specific consumers keep working without a
/// schema bump.
///
/// status: activity-feed-staging-metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingItem {
    pub id: String,
    pub surface: String,
    pub action: String,
    pub target_path: String,
    pub trail_id: Option<String>,
    pub session_id: Option<String>,
    pub content_hash: Option<String>,
    pub created_at_ms: i64,
    pub metadata: serde_json::Value,
}

/// One row in the unified feed. Flat envelope (timestamp / path / author /
/// summary) plus a per-kind payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub timestamp_ms: i64,
    pub path: String,
    pub author: String,
    pub summary: Summary,
    pub payload: Payload,
}

/// Synthetic author stamped on pending items in the unified feed so
/// consumers can filter `author = 'pending'` regardless of which MCP
/// client or surface produced the proposal.
const PENDING_AUTHOR: &str = "pending";

impl Item {
    /// Stable tiebreaker for items with identical timestamps. Pending ids
    /// are ULIDs (string-sortable by timestamp); change ids are monotonic
    /// ints. We sort on a `(timestamp_ms desc, secondary desc)` key.
    fn sort_secondary(&self) -> String {
        match &self.payload {
            Payload::Change(c) => format!("c:{:020}", c.id),
            Payload::Pending(s) => format!("s:{}", s.id),
        }
    }
}

pub struct Activity {
    log: Arc<OpLog>,
}

impl Activity {
    /// Construct the merged feed over the vault op log. Accepted ops project
    /// to change rows; pending ops project to pending proposal rows.
    ///
    /// status: activity-feed-module
    pub const fn new(log: Arc<OpLog>) -> Self {
        Self { log }
    }

    /// Merged listing across the whole vault.
    pub fn list(&self, filter: &Filter) -> Result<Vec<Item>, Error> {
        self.collect_items(filter, None)
    }

    /// Per-path variant — backs the editor status-bar version dropdown.
    pub fn list_for_path(
        &self,
        path: &str,
        filter: &Filter,
    ) -> Result<Vec<Item>, Error> {
        self.collect_items(filter, Some(path))
    }

    pub fn count(&self, filter: &Filter) -> Result<u32, Error> {
        let items = self.collect_items(filter, None)?;
        Ok(items.len() as u32)
    }

    fn collect_items(
        &self,
        filter: &Filter,
        path: Option<&str>,
    ) -> Result<Vec<Item>, Error> {
        let mut out: Vec<Item> = Vec::new();

        // Accepted ops (the "change" side). The `pending` synthetic author
        // lives on pending ops only; restricting to that pattern skips the
        // accepted side.
        let wants_changes = matches!(filter.source, Source::ChangesOnly | Source::Merged)
            && !matches!(filter.author_pattern.as_deref(), Some("pending"));

        if wants_changes {
            let changes = AcceptedFeed::new(&self.log);
            let change_rows = match (&filter.author_pattern, path) {
                (Some(pat), Some(p)) => changes
                    .history_for_path(p, filter.limit)?
                    .into_iter()
                    .filter(|r| sql_like_match(&r.author, pat))
                    .collect::<Vec<_>>(),
                (Some(pat), None) => changes.recent_by_author(pat, filter.limit)?,
                (None, Some(p)) => changes.history_for_path(p, filter.limit)?,
                (None, None) => changes.recent(filter.limit)?,
            };
            out.extend(change_rows.into_iter().map(|row| Item {
                timestamp_ms: row.timestamp_ms,
                path: row.path.clone(),
                author: row.author.clone(),
                summary: Summary::Change { op: row.op },
                payload: Payload::Change(row),
            }));
        }

        let wants_pending = matches!(filter.source, Source::PendingOnly | Source::Merged)
            && match filter.author_pattern.as_deref() {
                None => true,
                Some(p) => sql_like_match(PENDING_AUTHOR, p),
            };

        if wants_pending {
            out.extend(self.pending_items(path)?);
        }

        if let Some(since) = filter.since_ms {
            out.retain(|i| i.timestamp_ms >= since);
        }

        // Single-key sort: timestamp desc, secondary id desc as tiebreaker.
        out.sort_by(|a, b| {
            b.timestamp_ms
                .cmp(&a.timestamp_ms)
                .then_with(|| b.sort_secondary().cmp(&a.sort_secondary()))
        });
        if out.len() > filter.limit {
            out.truncate(filter.limit);
        }
        Ok(out)
    }

    /// Project the vault's pending ops to pending-proposal feed rows. When
    /// `path` is set, only ops whose document currently lives at that path
    /// are kept (resolved via `OpLog::path_for_doc`).
    ///
    /// status: activity-feed-staging-metadata
    fn pending_items(&self, path: Option<&str>) -> Result<Vec<Item>, Error> {
        let target_doc = match path {
            Some(p) => self.log.doc_id_for_path(p)?,
            None => None,
        };
        let mut out = Vec::new();
        for (doc_id, op) in self.log.all_pending_ops()? {
            if let (Some(_), Some(want)) = (path, &target_doc)
                && &doc_id != want
            {
                continue;
            } else if path.is_some() && target_doc.is_none() {
                // The path has no doc → no pending ops can match it.
                continue;
            }
            let target_path = self
                .log
                .path_for_doc(&doc_id)?
                .unwrap_or_else(|| doc_id.clone());
            let action = op.op_kind.as_str().to_string();
            let content_hash = op
                .metadata
                .get("new_str")
                .and_then(|v| v.as_str())
                .map(crate::hash_string);
            let pending_item = PendingItem {
                id: op.op_id.clone(),
                surface: op.surface.clone(),
                action,
                target_path: target_path.clone(),
                trail_id: op
                    .metadata
                    .get("trail_id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                session_id: op.session_id.clone(),
                content_hash,
                created_at_ms: op.created_at_ms,
                metadata: op.metadata.clone(),
            };
            out.push(Item {
                timestamp_ms: pending_item.created_at_ms,
                path: target_path,
                author: PENDING_AUTHOR.to_string(),
                summary: Summary::Pending {
                    surface: pending_item.surface.clone(),
                    action: pending_item.action.clone(),
                },
                payload: Payload::Pending(pending_item),
            });
        }
        Ok(out)
    }
}

/// Minimal SQL-LIKE matcher supporting `%` (any-run) and `_` (any-one).
/// Used to filter `Changes::history_for_path` results by author client-side
/// (the SQL path already handles patterns server-side via
/// `recent_by_author`).
fn sql_like_match(s: &str, pattern: &str) -> bool {
    like(s.as_bytes(), pattern.as_bytes())
}

fn like(s: &[u8], p: &[u8]) -> bool {
    if p.is_empty() {
        return s.is_empty();
    }
    match p[0] {
        b'%' => {
            // Skip consecutive '%'.
            let rest = &p[1..];
            if rest.is_empty() {
                return true;
            }
            (0..=s.len()).any(|i| like(&s[i..], rest))
        }
        b'_' => !s.is_empty() && like(&s[1..], &p[1..]),
        c => !s.is_empty() && s[0] == c && like(&s[1..], &p[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oplog::shapes::Author;
    use crate::oplog::{EditSpec, ProducerCtx};
    use tempfile::TempDir;

    /// A vault dir with an open op log. Seeds documents directly through the
    /// substrate (accepted via `create_document`, pending via `stage_pending`)
    /// so the projection is exercised end-to-end.
    fn setup() -> (TempDir, Arc<OpLog>, Activity) {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".hiker")).unwrap();
        let log = Arc::new(OpLog::open(dir.path()).unwrap());
        let act = Activity::new(log.clone());
        (dir, log, act)
    }

    fn stage_one(log: &OpLog, doc_id: &str, new_str: &str) {
        let ctx = ProducerCtx {
            author: Author::Agent("claude-code".into()),
            surface: "mcp-tool-call".into(),
            session_id: Some("claude-code".into()),
        };
        log.stage_pending(
            doc_id,
            &[EditSpec {
                old_str: None,
                new_str: new_str.into(),
            }],
            &ctx,
        )
        .unwrap();
    }

    #[test]
    fn merged_orders_by_timestamp_desc() {
        let (_d, log, act) = setup();
        // Accepted op (a created document).
        log.create_document("a.md", "markdown", "v1", &Author::User)
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        // Pending op on a second document.
        log.create_document("b.md", "markdown", "base", &Author::User)
            .unwrap();
        stage_one(&log, &log.doc_id_for_path("b.md").unwrap().unwrap(), "hello");
        let items = act
            .list(&Filter {
                source: Source::Merged,
                limit: 50,
                author_pattern: None,
                since_ms: None,
            })
            .unwrap();
        // Each create_document records one accepted op (Create, spanning the
        // seed): a.md = 1, b.md = 1, plus b.md's pending op = 3.
        assert_eq!(items.len(), 3);
        // The pending op is newest → first.
        assert!(matches!(items[0].payload, Payload::Pending(_)));
    }

    #[test]
    fn source_filter_short_circuits() {
        let (_d, log, act) = setup();
        log.create_document("a.md", "markdown", "v", &Author::User)
            .unwrap();
        stage_one(&log, &log.doc_id_for_path("a.md").unwrap().unwrap(), "c");

        let only_changes = act
            .list(&Filter {
                source: Source::ChangesOnly,
                limit: 50,
                author_pattern: None,
                since_ms: None,
            })
            .unwrap();
        assert!(only_changes.iter().all(|i| matches!(i.payload, Payload::Change(_))));
        assert!(!only_changes.is_empty());

        let only_pending = act
            .list(&Filter {
                source: Source::PendingOnly,
                limit: 50,
                author_pattern: None,
                since_ms: None,
            })
            .unwrap();
        assert_eq!(only_pending.len(), 1);
        assert!(matches!(only_pending[0].payload, Payload::Pending(_)));
    }

    #[test]
    fn for_path_scopes_both_sides() {
        let (_d, log, act) = setup();
        for p in &["a.md", "b.md"] {
            log.create_document(p, "markdown", "v", &Author::User).unwrap();
            stage_one(&log, &log.doc_id_for_path(p).unwrap().unwrap(), "c");
        }
        let items = act.list_for_path("a.md", &Filter::default()).unwrap();
        assert!(items.iter().all(|i| i.path == "a.md"));
        // a.md: one accepted (Create) + one pending.
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn pending_author_pattern_skips_changes() {
        let (_d, log, act) = setup();
        log.create_document("a.md", "markdown", "v", &Author::User)
            .unwrap();
        stage_one(&log, &log.doc_id_for_path("a.md").unwrap().unwrap(), "c");
        let items = act
            .list(&Filter {
                source: Source::Merged,
                limit: 50,
                author_pattern: Some("pending".into()),
                since_ms: None,
            })
            .unwrap();
        assert_eq!(items.len(), 1);
        assert!(matches!(items[0].payload, Payload::Pending(_)));
    }

    #[test]
    fn session_id_projected_from_pending_op() {
        let (_d, log, act) = setup();
        log.create_document("a.md", "markdown", "v", &Author::User)
            .unwrap();
        stage_one(&log, &log.doc_id_for_path("a.md").unwrap().unwrap(), "c");
        let items = act
            .list(&Filter {
                source: Source::PendingOnly,
                limit: 50,
                author_pattern: None,
                since_ms: None,
            })
            .unwrap();
        let Payload::Pending(s) = &items[0].payload else {
            panic!("expected pending");
        };
        assert_eq!(s.session_id.as_deref(), Some("claude-code"));
    }

    #[test]
    fn op_kind_maps_to_change_op() {
        assert_eq!(op_kind_to_change_op("create"), ChangeOp::Created);
        assert_eq!(op_kind_to_change_op("replace"), ChangeOp::Modified);
        assert_eq!(op_kind_to_change_op("set_frontmatter"), ChangeOp::Modified);
        assert_eq!(op_kind_to_change_op("tombstone"), ChangeOp::Deleted);
        assert_eq!(op_kind_to_change_op("rename"), ChangeOp::Renamed);
        // Unknown falls back to Modified.
        assert_eq!(op_kind_to_change_op("something-new"), ChangeOp::Modified);
    }

    #[test]
    fn author_class_parses_prefixes() {
        assert_eq!(AuthorClass::from_author("auto:triage"), AuthorClass::Auto);
        assert_eq!(AuthorClass::from_author("auto"), AuthorClass::Auto);
        assert_eq!(AuthorClass::from_author("user"), AuthorClass::User);
        assert_eq!(
            AuthorClass::from_author("agent:claude-code"),
            AuthorClass::Agent
        );
    }

    #[test]
    fn recent_projects_accepted_ops_with_op_id_preserved() {
        let (_d, log, _act) = setup();
        log.create_document("note.md", "markdown", "hello", &Author::User)
            .unwrap();
        let feed = AcceptedFeed::new(&log);
        let rows = feed.recent(50).unwrap();
        // A single Create op (spanning the seed) = 1 accepted row.
        assert_eq!(rows.len(), 1);
        for row in &rows {
            assert_eq!(row.path, "note.md");
            assert!(row.metadata.get("op_id").and_then(|v| v.as_str()).is_some());
        }
        assert!(rows.iter().any(|r| r.op == ChangeOp::Created));
    }

    #[test]
    fn tombstone_and_rename_project_to_deleted_and_renamed() {
        let (_d, log, _act) = setup();
        log.create_document("a.md", "markdown", "x", &Author::User)
            .unwrap();
        let doc_id = log.doc_id_for_path("a.md").unwrap().unwrap();
        log.rename_document(&doc_id, "b.md", &Author::User).unwrap();
        log.tombstone_document(&doc_id, &Author::User).unwrap();

        let feed = AcceptedFeed::new(&log);
        let rows = feed.history_for_path("b.md", 50).unwrap();
        assert!(rows.iter().any(|r| r.op == ChangeOp::Renamed));
        assert!(rows.iter().any(|r| r.op == ChangeOp::Deleted));
        let renamed = rows.iter().find(|r| r.op == ChangeOp::Renamed).unwrap();
        assert_eq!(renamed.rename_from.as_deref(), Some("a.md"));
    }

    #[test]
    fn is_current_marks_only_newest_per_path() {
        let (_d, log, _act) = setup();
        log.create_document("a.md", "markdown", "v1", &Author::User)
            .unwrap();
        let feed = AcceptedFeed::new(&log);
        let rows = feed.history_for_path("a.md", 50).unwrap();
        let current: Vec<_> = rows.iter().filter(|r| r.is_current).collect();
        assert_eq!(current.len(), 1, "exactly one row is current per path");
    }

    #[test]
    fn recent_by_author_filters_to_class() {
        let (_d, log, _act) = setup();
        log.create_document("u.md", "markdown", "x", &Author::User)
            .unwrap();
        let doc_id = log.doc_id_for_path("u.md").unwrap().unwrap();
        let outcome = log
            .stage_pending(
                &doc_id,
                &[EditSpec {
                    old_str: Some("x".into()),
                    new_str: "y".into(),
                }],
                &ProducerCtx {
                    author: Author::Agent("claude-code".into()),
                    surface: "mcp-tool-call".into(),
                    session_id: Some("claude-code".into()),
                },
            )
            .unwrap();
        log.accept_pending(&doc_id, &outcome.op_ids[0]).unwrap();

        let feed = AcceptedFeed::new(&log);
        let agents = feed.recent_by_author("agent:%", 50).unwrap();
        assert!(!agents.is_empty());
        assert!(agents.iter().all(|r| r.author.starts_with("agent:")));
        assert!(agents.iter().all(|r| r.author != "user"));
    }
}

