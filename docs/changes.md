# Change history

User-facing surfaces over the op log (per `op-log.md`): the unified activity feed, per-file change history, rollback / restore verbs, and the author-attribution queries that power them. This doc covers the *consumer* layer; storage and op semantics live in `op-log.md`.

The headline decisions:

- **Every committed change is an accepted op.** The home-page recent-activity widget, the per-file version dropdown, the activity-detail page, and the `recent_by_author` queries all read from `core::oplog` filtered to `status=accepted`. No separate changelog DB. [changes-query-api]
- **Rollback writes new ops, never mutates history.** "Undo this change" and "restore this version" both produce a fresh op against the document; the op being undone stays in the log with its original status. The audit trail is append-only by construction. [changes-rollback-helper]
- **Two rollback flavors share the same primitives.** `rollback_change` (agent-shaped — undo a specific op) walks the log to find the prior accepted materialization for the doc and appends a `Replace { entire_doc, prior_content }` op tagged `metadata.rolled_back_from`. `restore_snapshot` (version-shaped — restore *this* recorded version) reads the materialization-at-op `M_i` for the chosen op and appends the same shape tagged `metadata.restored_from`. Both flavors compose with arbitrary chains. [changes-rollback-helper]
- **Activity feed is one merged stream.** The activity-detail page, the editor status-bar version dropdown, and the queue-bar pending count read a single `Activity` projection over the op log: each op surfaces as one feed item, with `status` distinguishing accepted history from pending proposals. No frontend reconciliation between two stores. [activity-feed-merged]


## Query API

`core::changes::Changes` is a thin projection layer over `core::oplog`. Returns DTOs, never raw op records. [changes-query-api]

```rust
pub struct ChangeRow {
    pub op_id: OpId,
    pub timestamp_ms: i64,
    pub path: String,                  // resolved path-as-of-this-op
    pub op_kind: ChangeOpKind,         // coarse projection of the op's op_kind (per op-log-op-shape)
    pub author: String,
    pub status: OpStatus,
    pub content_hash: Option<String>,  // materialization hash after this op (None if Tombstone)
    pub rename_from: Option<String>,
    pub metadata: serde_json::Value,
}

pub enum ChangeOpKind { Created, Modified, Deleted, Renamed }

impl Changes {
    pub fn recent(&self, limit: usize) -> Result<Vec<ChangeRow>>;
    pub fn recent_by_author(&self, pattern: &str, limit: usize) -> Result<Vec<ChangeRow>>;
    pub fn history_for_path(&self, path: &str, limit: usize) -> Result<Vec<ChangeRow>>;
    pub fn materialization_at(&self, op_id: OpId) -> Result<Option<Vec<u8>>>;
    pub fn previous_materialization(&self, path: &str, before: OpId) -> Result<Option<(OpId, Vec<u8>)>>;
}
```

`materialization_at` returns the document's accepted-content as of (and including) `op_id`; it materializes on demand from the log rather than storing a per-op blob. Bounded by op-log retention (`[op-log] metadata_retention_days`).

`previous_materialization` is what rollback uses — finds the most recent accepted op before `op_id` on the same path and returns its materialization.


## Rollback

The two flavors share the same op-log primitives; they differ only in *which materialization* gets rewritten back. Both append a fresh op rather than touching the original.

### Flavor 1: rollback-this-change (agent-shaped)

Used by MCP agent rollback per `mcp.md`. "Agent committed op X, undo it."

1. User clicks "Undo this change" on an agent activity entry with `op_id = X`.
2. Caller resolves the affected `path` from the row.
3. `previous_materialization(path, X)` → `(prior_id, prior_content)`.
4. Caller appends a new op via `core::ops::write_file_checked` with `metadata.rolled_back_from: X`. The new op materializes the document back to its pre-X state.

[changes-rollback-helper]

The original op X stays in the log with `status=accepted` — rollback doesn't lie about what happened. Command: `rollback_change`.

### Flavor 2: restore-this-snapshot (version-list-shaped)

Used by the home-page recent-activity widget. "Each row is a saved version; restore writes that version back."

1. User clicks the row → opens its `materialization_at(X)` read-only in the editor.
2. User clicks [Restore this version].
3. Caller appends a new op materializing the document to that content, tagged `metadata.restored_from: X`.

Command: `restore_snapshot`.

### Why both, not just one

The home-page widget reasons about *versions* (the user reviewing their own edit history); "restore this version" maps cleanly onto what the user just clicked. The MCP agent-rollback case stays change-shaped because the consumer is an agent flagging "undo this specific action." The two registers each match their use case; collapsing them would distort whichever case lost.


## Unified activity feed

The activity detail page, the editor status-bar version dropdown, and the queue-bar pending count all consume one merged feed over the op log. The merge happens in the backend; consumers don't reconcile two lists. [activity-feed-merged]

- **`core::activity` is the projection module.** Depends on `core::oplog`; no on-disk state of its own. [activity-feed-module]
- **One DTO (`ActivityItem`) per op.** Tagged with `status`: `accepted` items are committed history, `pending` items are unreviewed proposals, `rejected` items surface in audit views only. The frontend renders rows from this DTO with no further reconciliation. [activity-feed-unified-item]
- **Source filter is a first-class arg.** `ActivitySource::AcceptedOnly | PendingOnly | All` keeps the existing call shapes working — the home page shows accepted-only by default; the activity detail page can flip to include pending. [activity-feed-source-filter]
- **Pending items carry producer metadata.** `surface`, `tool`, `session_id`, `target_path`, `content_hash` projected from the op's `metadata` blob. [activity-feed-staging-metadata]
- **Ordering is single-key.** Items sort by `timestamp_ms desc` with `op_id` as the deterministic tiebreaker. [activity-feed-merge-ordering]

### DTO

```rust
pub enum ActivitySource { AcceptedOnly, PendingOnly, All }

pub struct ActivityItem {
    pub op_id: OpId,
    pub timestamp_ms: i64,
    pub path: String,
    pub author: String,
    pub status: OpStatus,
    pub summary: ActivitySummary,
    pub payload: ActivityPayload,
}

pub enum ActivityPayload {
    Change(ChangeRow),                 // status=Accepted ops
    Pending(PendingItem),              // status=Pending ops; carries producer metadata
}

pub struct PendingItem {
    pub surface: String,
    pub action: String,
    pub target_path: String,
    pub trail_id: Option<String>,
    pub session_id: Option<String>,
    pub content_hash: Option<String>,
    pub created_at_ms: i64,
    pub metadata: serde_json::Value,
}
```

### Query surface

```rust
impl Activity {
    pub fn list(&self, filter: ActivityFilter) -> Result<Vec<ActivityItem>>;
    pub fn list_for_path(&self, path: &str, filter: ActivityFilter) -> Result<Vec<ActivityItem>>;
    pub fn count(&self, filter: ActivityFilter) -> Result<u32>;
}

pub struct ActivityFilter {
    pub source: ActivitySource,
    pub limit: usize,
    pub author_pattern: Option<String>,
    pub since_ms: Option<i64>,
}
```

[activity-feed-merged-query]


## Consumers

- **Activity detail page** — `Activity::list({ source: All })` plus the existing three filter pills (per `vault-home-recent-activity-filter-pills`): the show-pending toggle flips `source` between `All` (on) and `AcceptedOnly` (off); user / agent pills set `author_pattern`. [activity-feed-activity-detail-consumer]
- **Editor status-bar version dropdown** — `Activity::list_for_path(path, { source: All })` to populate accepted history + pending proposals in one pass.
- **Queue-bar pending badge** — `Activity::count({ source: PendingOnly })`. Same shape as the prior staging-only count.


## Retention

Configurable in `[op-log]` per `op-log.md`'s `op-log-config-section` — `metadata_retention_days` (accepted-op metadata, default 365) and `rejected_retention_days` (default 14). Per-(path, author) keep-N from the prior design is dropped; whole-log time-based GC is honest about what the log is (an audit trail with a horizon) and avoids the keep-N edge cases (heavy agent activity pushing out user saves).

Op-log GC honors `Tombstone` ops by never removing the *most recent* op for a path with a Tombstone in scope — restoring a deleted file from a long-ago accepted op stays possible until the Tombstone itself falls out of retention.


## Author tagging conventions

The `author` field is the load-bearing distinguishing feature; activity queries filter heavily on it. Vocabulary lives in `op-log.md` `op-log-author-classes`. The class prefix supports both wildcard (`author LIKE 'agent:%'`) and exact (`author = 'agent:claude-code'`) queries. New classes added without schema change.

`changes.md`-era convention preserved: user-accepted writes that originated from a staged producer (e.g., chat-driven `triage` proposals) carry `author = "user"` because the *acceptance* was a user-initiated decision; the original producer surface lives in `metadata`. Unattended writes carry `author = "auto:<producer>"` so a single `author LIKE 'auto:%'` filter surfaces everything organized without explicit user touch.


## Module placement

- `core::oplog` — substrate, see `op-log.md`.
- `core::changes` — the `Changes` projection above. Pure read API over `core::oplog`; no writes of its own beyond delegating to `core::ops::write_file_checked` from the rollback helpers.
- `core::activity` — `Activity` struct, the merged DTO + query API. Pure projection over `core::oplog`.
- Host commands: `recent_changes`, `recent_changes_by_author`, `change_content`, `previous_for_path`, `rollback_change`, `restore_snapshot`, `activity_list`, `activity_list_for_path`, `activity_count`.


## Out of scope

- **Cross-device sync transport.** Substrate is the op log; transport stays deferred per `op-log.md`.
- **Conflict copies.** When sync lands, conflict copies live in `.hiker/conflicts/` per `design.md`. Not a `core::changes` concern.
- **Diff rendering.** Computing diffs between two materializations is the consumer's job (per `diff.md`'s `DiffLayer`). `core::changes` just hands over content.
- **Per-character history view.** The substrate supports it (op-level granularity), but a UI surface for "what changed in this single line over time" is deferred.


## Forward refs

- `op-log.md` — the substrate.
- `mcp.md` — agent rollback consumes `rollback_change`; agent writes append ops with `author='agent:*'`.
- `editor.md` vault home page — the "agent activity" widget queries `recent_by_author('agent:%')`. Detail view exposes per-row diff + rollback.
- `patch-review.md` — pending ops surface here; accept/reject flips their status.
- `design.md` "Ideas for integrated syncing" — the deferred sync layer rides on the op log.
