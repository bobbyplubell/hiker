//! Unified activity feed — merges `core::changes` rows with `core::staging`
//! proposals into a single chronological list. See docs/changes.md
//! "## Unified activity feed".
//!
//! Pure projection over the two existing stores: no on-disk state, no
//! writes, no GC. Constructed at vault open with `Arc<Changes>` +
//! `Arc<Staging>` handles. Consumers (activity detail page, status-bar
//! version dropdown, queue-bar combined badge) call one of `list` /
//! `list_for_path` / `count` and render directly off the unified
//! `ActivityItem` DTO.
//
// status: activity-feed-merged
// status: activity-feed-module
// status: activity-feed-unified-item
// status: activity-feed-source-filter
// status: activity-feed-staging-metadata
// status: activity-feed-merged-query
// status: activity-feed-merge-ordering

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::changes::{ChangeOp, ChangeRow, Changes, ChangesError};
use crate::staging::{Proposal, Staging, StagingError, StagingFilter};

#[derive(Debug, Error)]
pub enum ActivityError {
    #[error("changes: {0}")]
    Changes(#[from] ChangesError),
    #[error("staging: {0}")]
    Staging(#[from] StagingError),
}

/// Which underlying store(s) the query should pull from.
///
/// `ChangesOnly` and `StagingOnly` short-circuit to the existing
/// `Changes::recent` / `Staging::list` paths and wrap each row in the
/// unified envelope. `Merged` runs both and interleaves by timestamp.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivitySource {
    ChangesOnly,
    StagingOnly,
    #[default]
    Merged,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ActivityFilter {
    #[serde(default)]
    pub source: ActivitySource,
    pub limit: usize,
    /// Optional SQL LIKE pattern matched against `ChangeRow.author`
    /// (e.g. `agent:%`). Staging items match only the synthetic
    /// `pending` author.
    #[serde(default)]
    pub author_pattern: Option<String>,
    /// Exclude items older than this (unix millis).
    #[serde(default)]
    pub since_ms: Option<i64>,
}

impl Default for ActivityFilter {
    fn default() -> Self {
        Self {
            source: ActivitySource::default(),
            limit: 200,
            author_pattern: None,
            since_ms: None,
        }
    }
}

/// Short label material — UI formats display strings off this.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActivitySummary {
    Change { op: ChangeOp },
    Staging { surface: String, action: String },
}

/// Per-kind payload. Tagged enum so the frontend can switch on `kind`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ActivityPayload {
    Change(ChangeRow),
    Staging(StagingItem),
}

/// Staging projection for the unified feed. Mirrors the columns the
/// `pending.json` index already carries, with `metadata` preserved verbatim
/// so MCP-specific or trail-specific consumers keep working without a
/// schema bump.
///
/// status: activity-feed-staging-metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StagingItem {
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

impl StagingItem {
    fn from_proposal(p: Proposal) -> Self {
        let metadata = p.metadata.unwrap_or(serde_json::Value::Null);
        let session_id = metadata
            .get("session_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        StagingItem {
            id: p.id,
            surface: p.surface,
            action: p.action,
            target_path: p.target_path,
            trail_id: p.trail_id,
            session_id,
            content_hash: p.content_hash,
            created_at_ms: p.created_at_ms,
            metadata,
        }
    }
}

/// One row in the unified feed. Flat envelope (timestamp / path / author /
/// summary) plus a per-kind payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivityItem {
    pub timestamp_ms: i64,
    pub path: String,
    pub author: String,
    pub summary: ActivitySummary,
    pub payload: ActivityPayload,
}

/// Synthetic author stamped on staging items in the unified feed so
/// consumers can filter `author = 'pending'` regardless of which MCP
/// client or surface produced the proposal.
const STAGING_AUTHOR: &str = "pending";

impl ActivityItem {
    fn from_change(row: ChangeRow) -> Self {
        ActivityItem {
            timestamp_ms: row.timestamp_ms,
            path: row.path.clone(),
            author: row.author.clone(),
            summary: ActivitySummary::Change { op: row.op },
            payload: ActivityPayload::Change(row),
        }
    }

    fn from_staging(item: StagingItem) -> Self {
        ActivityItem {
            timestamp_ms: item.created_at_ms,
            path: item.target_path.clone(),
            author: STAGING_AUTHOR.to_string(),
            summary: ActivitySummary::Staging {
                surface: item.surface.clone(),
                action: item.action.clone(),
            },
            payload: ActivityPayload::Staging(item),
        }
    }

    /// Stable tiebreaker for items with identical timestamps. Staging ids
    /// are ULIDs (string-sortable by timestamp); change ids are monotonic
    /// ints. We sort on a `(timestamp_ms desc, secondary desc)` key.
    fn sort_secondary(&self) -> String {
        match &self.payload {
            ActivityPayload::Change(c) => format!("c:{:020}", c.id),
            ActivityPayload::Staging(s) => format!("s:{}", s.id),
        }
    }
}

pub struct Activity {
    changes: Arc<Changes>,
    staging: Arc<Staging>,
}

impl Activity {
    pub fn new(changes: Arc<Changes>, staging: Arc<Staging>) -> Self {
        Self { changes, staging }
    }

    /// Merged listing across the whole vault.
    pub fn list(&self, filter: ActivityFilter) -> Result<Vec<ActivityItem>, ActivityError> {
        self.list_inner(filter, None)
    }

    /// Per-path variant — backs the editor status-bar version dropdown.
    pub fn list_for_path(
        &self,
        path: &str,
        filter: ActivityFilter,
    ) -> Result<Vec<ActivityItem>, ActivityError> {
        self.list_inner(filter, Some(path.to_string()))
    }

    pub fn count(&self, filter: ActivityFilter) -> Result<u32, ActivityError> {
        let items = self.list_inner(filter, None)?;
        Ok(items.len() as u32)
    }

    fn list_inner(
        &self,
        filter: ActivityFilter,
        path: Option<String>,
    ) -> Result<Vec<ActivityItem>, ActivityError> {
        let mut out: Vec<ActivityItem> = Vec::new();

        // Pull from changes when source allows it. The `pending` synthetic
        // author lives on staging only; if the caller restricts by
        // author_pattern to staging-only, skip the changes side.
        let wants_changes = matches!(
            filter.source,
            ActivitySource::ChangesOnly | ActivitySource::Merged
        ) && !is_pending_only_author(&filter.author_pattern);

        if wants_changes {
            let change_rows = match (&filter.author_pattern, &path) {
                (Some(pat), Some(p)) => self
                    .changes
                    .history_for_path(p, filter.limit)?
                    .into_iter()
                    .filter(|r| sql_like_match(&r.author, pat))
                    .collect::<Vec<_>>(),
                (Some(pat), None) => self.changes.recent_by_author(pat, filter.limit)?,
                (None, Some(p)) => self.changes.history_for_path(p, filter.limit)?,
                (None, None) => self.changes.recent(filter.limit)?,
            };
            out.extend(change_rows.into_iter().map(ActivityItem::from_change));
        }

        let wants_staging = matches!(
            filter.source,
            ActivitySource::StagingOnly | ActivitySource::Merged
        ) && staging_author_matches(&filter.author_pattern);

        if wants_staging {
            let staging_filter = StagingFilter {
                path: path.clone(),
                ..StagingFilter::default()
            };
            let proposals = self.staging.list(&staging_filter)?;
            out.extend(
                proposals
                    .into_iter()
                    .map(|p| ActivityItem::from_staging(StagingItem::from_proposal(p))),
            );
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
}

/// Returns true when the author_pattern targets only the synthetic
/// `pending` author used for staging items. `recent_by_author` would
/// always return zero rows, so we skip the changes query entirely.
fn is_pending_only_author(pattern: &Option<String>) -> bool {
    matches!(pattern.as_deref(), Some("pending"))
}

/// Does the author_pattern allow the synthetic `pending` staging author?
/// `None` → yes; otherwise SQL LIKE match.
fn staging_author_matches(pattern: &Option<String>) -> bool {
    match pattern.as_deref() {
        None => true,
        Some(p) => sql_like_match(STAGING_AUTHOR, p),
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
    use crate::changes::ChangeAppend;
    use crate::staging::ProposalInput;
    use tempfile::TempDir;

    fn setup() -> (TempDir, Arc<Changes>, Arc<Staging>, Activity) {
        let dir = TempDir::new().unwrap();
        let vault = dir.path();
        std::fs::create_dir_all(vault.join(".hiker")).unwrap();
        let changes = Arc::new(Changes::open(vault).unwrap());
        let staging = Arc::new(Staging::open(vault).unwrap());
        let act = Activity::new(changes.clone(), staging.clone());
        (dir, changes, staging, act)
    }

    #[test]
    fn merged_orders_by_timestamp_desc() {
        let (_d, changes, staging, act) = setup();
        changes
            .append(ChangeAppend {
                path: "a.md",
                op: ChangeOp::Modified,
                author: "user",
                content: Some(b"v1"),
                content_hash: Some("h1"),
                rename_from: None,
                metadata: serde_json::Value::Null,
            })
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        staging
            .propose(ProposalInput {
                surface: "chat".into(),
                action: "write_note".into(),
                target_path: "b.md".into(),
                trail_id: None,
                content: Some("hello".into()),
                metadata: None,
                source_hash: None,
            })
            .unwrap();
        let items = act
            .list(ActivityFilter {
                source: ActivitySource::Merged,
                limit: 50,
                author_pattern: None,
                since_ms: None,
            })
            .unwrap();
        assert_eq!(items.len(), 2);
        // staging is newer → first
        assert!(matches!(items[0].payload, ActivityPayload::Staging(_)));
        assert!(matches!(items[1].payload, ActivityPayload::Change(_)));
    }

    #[test]
    fn source_filter_short_circuits() {
        let (_d, changes, staging, act) = setup();
        changes
            .append(ChangeAppend {
                path: "a.md",
                op: ChangeOp::Modified,
                author: "user",
                content: Some(b"v"),
                content_hash: Some("h"),
                rename_from: None,
                metadata: serde_json::Value::Null,
            })
            .unwrap();
        staging
            .propose(ProposalInput {
                surface: "chat".into(),
                action: "write_note".into(),
                target_path: "a.md".into(),
                trail_id: None,
                content: Some("c".into()),
                metadata: None,
                source_hash: None,
            })
            .unwrap();
        let only_changes = act
            .list(ActivityFilter {
                source: ActivitySource::ChangesOnly,
                limit: 50,
                author_pattern: None,
                since_ms: None,
            })
            .unwrap();
        assert_eq!(only_changes.len(), 1);
        assert!(matches!(only_changes[0].payload, ActivityPayload::Change(_)));

        let only_staging = act
            .list(ActivityFilter {
                source: ActivitySource::StagingOnly,
                limit: 50,
                author_pattern: None,
                since_ms: None,
            })
            .unwrap();
        assert_eq!(only_staging.len(), 1);
        assert!(matches!(
            only_staging[0].payload,
            ActivityPayload::Staging(_)
        ));
    }

    #[test]
    fn for_path_scopes_both_sides() {
        let (_d, changes, staging, act) = setup();
        for p in &["a.md", "b.md"] {
            changes
                .append(ChangeAppend {
                    path: p,
                    op: ChangeOp::Modified,
                    author: "user",
                    content: Some(b"v"),
                    content_hash: Some("h"),
                    rename_from: None,
                    metadata: serde_json::Value::Null,
                })
                .unwrap();
            staging
                .propose(ProposalInput {
                    surface: "chat".into(),
                    action: "write_note".into(),
                    target_path: (*p).into(),
                    trail_id: None,
                    content: Some("c".into()),
                    metadata: None,
                    source_hash: None,
                })
                .unwrap();
        }
        let items = act
            .list_for_path("a.md", ActivityFilter::default())
            .unwrap();
        assert_eq!(items.len(), 2);
        assert!(items.iter().all(|i| i.path == "a.md"));
    }

    #[test]
    fn pending_author_pattern_skips_changes() {
        let (_d, changes, staging, act) = setup();
        changes
            .append(ChangeAppend {
                path: "a.md",
                op: ChangeOp::Modified,
                author: "user",
                content: Some(b"v"),
                content_hash: Some("h"),
                rename_from: None,
                metadata: serde_json::Value::Null,
            })
            .unwrap();
        staging
            .propose(ProposalInput {
                surface: "chat".into(),
                action: "write_note".into(),
                target_path: "a.md".into(),
                trail_id: None,
                content: Some("c".into()),
                metadata: None,
                source_hash: None,
            })
            .unwrap();
        let items = act
            .list(ActivityFilter {
                source: ActivitySource::Merged,
                limit: 50,
                author_pattern: Some("pending".into()),
                since_ms: None,
            })
            .unwrap();
        assert_eq!(items.len(), 1);
        assert!(matches!(items[0].payload, ActivityPayload::Staging(_)));
    }

    #[test]
    fn session_id_projected_from_metadata() {
        let (_d, _c, staging, act) = setup();
        staging
            .propose(ProposalInput {
                surface: "chat".into(),
                action: "write_note".into(),
                target_path: "a.md".into(),
                trail_id: None,
                content: Some("c".into()),
                metadata: Some(serde_json::json!({"session_id": "s42"})),
                source_hash: None,
            })
            .unwrap();
        let items = act
            .list(ActivityFilter {
                source: ActivitySource::StagingOnly,
                limit: 50,
                author_pattern: None,
                since_ms: None,
            })
            .unwrap();
        let ActivityPayload::Staging(s) = &items[0].payload else {
            panic!("expected staging");
        };
        assert_eq!(s.session_id.as_deref(), Some("s42"));
    }
}
