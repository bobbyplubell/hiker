# Changes log

An append-only log of every write that mutates a note in the vault — user save, agent write via MCP, future sync receive, future import. Lives in `.hiker/changes.db`, separate from the regenerable `.hiker/index.db`. Shared substrate consumed by agent rollback (this doc + `mcp.md`), per-file history views (deferred), and the future sync layer (`design.md` "Ideas for integrated syncing").

The headline decisions:

- **Single append-only log of all vault-content writes.** Every `core::ops` mutation that touches a note appends one row. User saves, agent writes, future sync receives, future imports — all flow through. The `author` field distinguishes them. [changes-log-table]
- **Lives in `.hiker/changes.db`, durable.** Separate from `.hiker/index.db` to preserve the "index is regenerable from content" rule. The log isn't regenerable — losing it means losing agent-rollback history and (eventually) sync state. Backed up alongside vault content per `design.md`'s three-class backup framing. [changes-store-file]
- **Stores post-op content as a blob.** Each row carries the full file content after the op (NULL for deletes). Rollback is "find the row before this one for this path → write its content back to the file." Future sync clients pull content directly off rows. No separate snapshot file directory.
- **Retention is configurable per author class.** Default: keep last 50 entries per `(path, author)` pair. Aggressive enough to bound storage; lenient enough that agent-rollback works for normal usage patterns. [changes-retention]
- **Single writer.** The indexer task (`core::indexer`) is the only writer to `changes.db`, same way it's the only writer to `index.db`. Read connections are shared with the rest of `core`. No multi-writer coordination needed.


## Schema

```sql
CREATE TABLE changes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp INTEGER NOT NULL,            -- unix millis
    path TEXT NOT NULL,                    -- vault-relative; for renames, the post-rename path
    op TEXT NOT NULL,                      -- 'created' | 'modified' | 'deleted' | 'renamed'
    author TEXT NOT NULL,                  -- 'user' | 'agent:<client-id>' | 'sync:<device-id>' | 'import:<source>'
    content_hash TEXT,                     -- post-op content hash (NULL for delete)
    content BLOB,                          -- post-op content (NULL for delete)
    rename_from TEXT,                      -- non-null only for op='renamed'; the pre-rename path
    metadata TEXT NOT NULL DEFAULT '{}'    -- JSON: tool/session info for agent ops, future sync watermarks, etc.
);

CREATE INDEX changes_path_ts ON changes(path, timestamp DESC);
CREATE INDEX changes_author_ts ON changes(author, timestamp DESC);
CREATE INDEX changes_ts ON changes(timestamp DESC);
```

[changes-log-table]

Notes on the schema:

- **`id` is monotonic.** When sync lands, this becomes the watermark used by clients to pull "everything since `last_seen_id`." For now, only used as ordering.
- **`path` is post-rename for `op='renamed'`.** `rename_from` carries the old path. Most queries care about "where did this end up," so post-rename is the indexed-by path.
- **`content` is the full post-op file content**, stored as BLOB. SQLite handles this fine; large files (extracted PDFs, etc.) are bounded by `[indexing] max_file_size` (per `index.md`'s skip-too-large rule), so changelog blobs inherit that ceiling.
- **`metadata` JSON** is open-ended. Per-author conventions documented below; consumers parse only what they need.
- **No content for deletes.** A `'deleted'` row records that the file was removed. Rollback of a delete = read the row *before* it for that path, write that content to the file. The deleted row itself doesn't carry the prior content.


## Author tagging conventions

The `author` field is the load-bearing distinguishing feature; queries filter on it heavily. Format: `class[:identifier]`:

- **`user`** — write originated from a user-initiated action in the UI (save, create, manual rename via tree). No identifier.
- **`agent:<client-id>`** — write originated from an MCP tool call. The `<client-id>` is the agent's name from the MCP `initialize` handshake (e.g. `agent:claude-code`, `agent:goose`, `agent:custom-script`). Lets the UI filter "edits by Claude Code" vs. "edits by my custom agent."
- **`sync:<device-id>`** — future. Write came from a sync push from another device. `<device-id>` identifies the originating device.
- **`import:<source>`** — future. Write came from a bulk import (Apple Notes export, Claude Code transcript, web archival). `<source>` identifies the importer.
- **`auto:<producer>`** — write came from an auto-accepted staging proposal (no user gate at accept time). `<producer>` identifies the producer pipeline: `auto:triage` for saved-tree triage auto-accepts (per `suggestions.md`), `auto:cluster-editor` reserved for future one-shot auto-accept paths. The accompanying `metadata.staging_proposal_id` + `metadata.auto_accepted = true` carry the full trace; the `auto:*` prefix exists so a single `author LIKE 'auto:%'` filter surfaces "everything organized for me without my touch."

User-accepted staging rows stay `author = "user"` — accept-from-the-activity-detail-page is a user-initiated action even when the producer was triage. The `auto:*` prefix is reserved for genuinely-unattended writes.

The class prefix supports both wildcard (`author LIKE 'agent:%'`) and exact (`author = 'agent:claude-code'`) queries. New classes can be added without schema change.


## Write paths

Every write that mutates note content routes through `core::ops` (per `arch_cleanup.md`'s layer split discipline) and from there through the indexer task. The indexer task appends to `changes.db` *before* applying the write — so if the write fails, the changelog row is rolled back with the rest of the operation transaction. [changes-write-path]

Concrete entry points that produce changelog rows:

- `core::ops::create_with_suffix` — `op='created'`, `author='user'` (or `agent:*` if MCP-driven).
- `core::ops::move_note` / `move_folder` — `op='renamed'` for each affected note.
- `core::ops::delete` — `op='deleted'` (singleton row per affected path; `content` is NULL).
- `core::ops::restore` — `op='created'` for restored paths (the prior `'deleted'` row stays in the log; restoration is a new event).
- `core::vault::write_file` (when called from save paths) — `op='modified'` if the file existed, `'created'` if not.
- Future MCP tool dispatch (`apply_tag`, `set_frontmatter`, `write_note`) — `op='modified'`, `author='agent:*'`.

The watcher does *not* directly write changelog rows. Watcher events that originate from external file activity (Syncthing, manual fs ops, another editor) are surfaced to the indexer, which appends `op='modified'` / `'created'` / `'deleted'` rows with `author='user'` (since hiker can't distinguish a user-driven external edit from a script-driven one — both are not-hiker writes). When sync lands, sync-receive paths will stamp `author='sync:*'` explicitly.


## Query API

`core::changes::Changes` exposes the read API. Returns `ChangeRow` DTOs, never raw sqlite types — same module discipline as `core::store`. [changes-query-api]

```rust
pub struct ChangeRow {
    pub id: i64,
    pub timestamp_ms: i64,
    pub path: String,
    pub op: ChangeOp,
    pub author: String,
    pub content_hash: Option<String>,
    pub rename_from: Option<String>,
    pub metadata: serde_json::Value,
    // content blob is NOT in this DTO — fetched separately on demand to keep listings cheap
}

pub enum ChangeOp { Created, Modified, Deleted, Renamed }

impl Changes {
    pub fn recent(&self, limit: usize) -> Result<Vec<ChangeRow>, ChangesError>;
    pub fn recent_by_author(&self, author_pattern: &str, limit: usize) -> Result<Vec<ChangeRow>, ChangesError>;
    pub fn history_for_path(&self, path: &str, limit: usize) -> Result<Vec<ChangeRow>, ChangesError>;
    pub fn content_at(&self, change_id: i64) -> Result<Option<Vec<u8>>, ChangesError>;
    pub fn previous_content_for_path(&self, path: &str, before_id: i64) -> Result<Option<(i64, Vec<u8>)>, ChangesError>;
    // future: pull_since(watermark) for sync
}
```

`previous_content_for_path` is what rollback uses — given "I want to undo change X to path P," it returns the change-id + content of the most recent prior row for P. Caller writes the content back via `core::vault::write_file_checked`.


## Rollback

Rollback is implemented by consumers, not by `core::changes` itself. The log exposes the two primitives both flavors of "go back" are built on: "give me the content blob recorded at this row" (`content_at`) and "give me the prior content for this path before this row" (`previous_content_for_path`). [changes-rollback-helper]

Two flavors live on top of these primitives, and they coexist — pick the one that matches the consumer's mental model:

### Flavor 1: rollback-this-change (agent-shaped)

This is the original spec shape, used by MCP agent rollback (`mcp.md`): "an agent wrote change X, undo it." Walks `previous_content_for_path(path, X)` to find the state immediately before X, writes that back. The change row X stays in the log; a new `'modified'` row is appended carrying the prior state, stamped `metadata.rolled_back_from = X`.

```
1. User clicks "Undo this change" on agent activity entry with change_id = X.
2. Caller resolves the affected `path` from the row.
3. Caller calls `previous_content_for_path(path, X)` → `(prior_id, prior_content)`.
4. Caller writes `prior_content` back via `core::ops::write_file_checked`.
5. A new `'modified'` row is appended with `metadata.rolled_back_from: X`.
```

This is the right framing when the consumer is reasoning about *changes* — "the agent did this thing, undo it, the user shouldn't have to look at versions." Command: `rollback_change`.

### Flavor 2: restore-this-snapshot (version-list-shaped)

This is the home-page recent-activity widget shape: "each row is a saved version; restore writes that version back." Reads `content_at(X)` (the content blob stored on the row itself) and writes that back. Same append-only discipline: a new `'modified'` row is appended with `metadata.restored_from = X`.

```
1. User clicks the row → opens its content blob read-only in the editor.
2. User clicks [Restore this version] (in the banner or per-row).
3. Caller calls `content_at(X)` → `Some(content)`.
4. Caller writes that content via `core::ops::write_file_checked`.
5. A new `'modified'` row is appended with `metadata.restored_from: X`.
```

This is the right framing when the consumer is reasoning about *versions* — "I edited the file three times; show me each version and let me pick one." Command: `restore_snapshot`.

The two share everything: same primitives, same append-only discipline, same rows in the same log. They only differ in *which row's content* gets written back — `previous_content_for_path` walks one step earlier, `content_at` reads the row itself.

### Why both, not just one

The home-page widget needs version-list framing because the user is reviewing their own edit history; "rollback to before this" is confusing because the row IS the version (the content blob lives on the row), so "restore this version" maps cleanly onto what the user just clicked. The MCP agent-edit case stays change-shaped because the consumer is an agent flagging "undo this specific action," not a user picking a version. The two semantic registers each match their use case exactly; trying to collapse them into one operation would distort whichever case lost.

### Append-only discipline (both flavors)

Neither flavor removes rows; both append a new `'modified'` row tagged with linkage metadata (`rolled_back_from` or `restored_from`). The log is honest about what happened — every state within retention is preserved as a content blob, and arbitrary chains of restores compose naturally:

```
R1  modified   v1   author=user
R2  modified   v2   author=user
R3  modified   v3   author=user
R4  modified   v2   author=user   metadata={"restored_from": R2}
                                  — user restored v2 over v3
R5  modified   v3   author=user   metadata={"restored_from": R3}
                                  — user restored v3 (undoing R4)
```

Every row in the trace is structurally identical to every other row. Restores aren't a special op type, just regular `'modified'` rows with linkage metadata. The log handles arbitrary restore chains naturally — no linear undo stack to manage, no "redo state lost on subsequent edit" failure mode. As long as a row is within retention, it's an addressable restore target.

### Baseline-on-first-mutation

A practical edge case: a vault file that pre-dates the changelog has no prior row. Saving it appends one row (the save itself); rolling back from that row finds no prior content. To make first-rollback work, the save path lazy-snapshots the *pre-write* content as a `'created'` row tagged `metadata.baseline = true` whenever the path has no rows yet. `Changes::ensure_baseline` on the core side; called from the host write paths. Idempotent — once any row exists for the path, the call no-ops. [changes-baseline-on-first-mutation]


## Retention

Default policy: **keep the most recent N rows per (path, author) pair**, with `N = 50`. Older rows for that combination are dropped on a periodic GC pass. [changes-retention]

Configurable in `[changes]`:

```toml
[changes]
keep_per_path_per_author = 50          # default 50; -1 = unlimited
gc_interval_hours = 24                 # how often to run GC
```

Why per-(path, author) rather than just per-path: a single note that gets heavy agent activity shouldn't push out the user's manual save history. Per-(path, author) preserves at least N entries from each author class.

GC runs as a low-priority job from the indexer task, opportunistically when no other work is queued.

Special-case: `op='deleted'` rows are never GC'd by the per-path policy alone — they're the rollback target for "undelete" operations. GC removes them only when the path is fully gone (no recent rows of any kind in retention window) and the trash entry for the original delete is also gone (consistency with `vault-trash`).


## Content compression

The `content` BLOB is stored zstd-compressed; `Changes::content_at` and `previous_content_for_path` decode transparently. Markdown compresses 4–8× routinely, and the BLOB is by far the dominant on-disk cost (every other column is small + bounded), so this is where the storage lever is. [changes-content-zstd]

- **Encode at append.** `Changes::append` runs `zstd::encode_all(content, level=3)` before binding the BLOB. Level 3 is zstd's default — fast encode with near-best text ratio at that setting; higher levels gain <10% for ~3× encode time.
- **Decode at read.** `content_at` and `previous_content_for_path` decode before returning `Vec<u8>`. Consumers (`rollback_change`, `restore_snapshot`, the snapshot-preview buffer, future sync push) see the same plaintext bytes; no DTO change, no API change.
- **Empty stays cheap, NULL stays NULL.** `op='deleted'` rows skip the encode path; empty files produce a tiny zstd frame.

### Why zstd specifically

- **Text ratio.** zstd at level 3 hits 4–8× on real markdown; structured prose (headings, lists, fences) routinely beats 6×. zlib reaches similar ratios but encodes ~2× slower at comparable settings; lz4 encodes faster but compresses ~1.5–2× worse on text — wrong tradeoff when rows are written rarely (one per save) and read even more rarely (rollback).
- **Decode speed.** Sub-millisecond for any payload under the indexer's `max_file_size` cap. The activity widget never blocks on it.
- **Mature Rust crate.** `zstd` (libzstd bindings) exposes byte-slice helpers — no streaming machinery needed for whole-file payloads.
- **No tuning surface.** Default level 3 is the right answer; no config knob, no per-vault setting. Compression stays invisible to the user.

### Schema migration

The format change bumps `changes.db` `SCHEMA_VERSION` from 1 to 2. Per the migration stance in the next section, "delete and regenerate" is not an option — the filesystem carries no history. So `Changes::open` on a v1 db runs an in-place migration: walk the `changes` table, re-encode each non-NULL `content` BLOB, write back in a single transaction, bump `user_version`. One-shot; subsequent opens see v2 and skip.

This is the first real schema bump for `changes.db`; the v0→v1 path was just `ensure_schema` from no-such-table.

Failure modes:

- Mid-migration crash → transaction rolls back, db stays v1, next open retries. Idempotent by construction.
- Decode failure on a v2 read → `ChangesError::Corrupt` carrying the row id and content_hash, per `obs-error-context`.

### What's preserved

- **Per-row self-sufficiency.** Every row still carries its own complete post-op content; rollback to row X reads exactly that row's blob. The "every row is structurally identical to every other row" invariant in the Rollback section holds.
- **GC simplicity.** Retention drops rows independently; no chain dependencies, no checkpoint logic.
- **The rollback model.** Both flavors (`rollback_change`, `restore_snapshot`) keep working unchanged — they consume `Vec<u8>` from the API and don't care how it's stored.

### Out of scope for this slug

- Periodic GC vs. open-time GC, tiered retention (full-content vs metadata-only rows), time-based pruning, total-size ceiling — separate slugs; each composes orthogonally with compression.
- Patch / delta storage between consecutive versions. Considered and rejected as the first move: patches entangle GC with chain integrity and break the per-row self-sufficiency invariant. Compression handles the bulk of the storage problem; if measured workload shows it's insufficient, revisit with a reverse-delta-plus-snapshot design rather than a naive forward chain.
- Encryption at rest. Orthogonal; the future sync layer's posture covers cross-device, and on-disk encryption (if it ever lands) wraps the storage layer rather than this column.


## Schema versioning

`changes.db` has its own schema version, separate from `index.db`'s. Same fail-loud rule as `store-version-fail-loud`: opening a `changes.db` with mismatched schema aborts with a clear error. *Unlike* `index.db`, there's no "regenerate from filesystem" recovery path — the user must restore from backup or accept the loss. The `obs-error-context` discipline applies; the error message names the file and the version mismatch explicitly.

Bumping the schema means a real migration path (or accepting data loss for casual users). For v3 the schema bump is just "from no-such-table to the v1 schema above," handled by `ensure_schema` on first open.


## What this strategy doesn't ship

- **Cross-device sync transport.** That's the deferred sync layer. `core::changes` provides the substrate; sync provides the wire protocol on top.
- **Conflict copies.** When sync lands, conflict copies live in `.hiker/conflicts/` per `design.md`. Not a `core::changes` concern.
- **Diff rendering.** The log stores full content per op; computing diffs between two versions is the consumer's job (UI uses a JS diff library; CLI uses a Rust diff crate). `core::changes` just hands over content.
- **Per-character or per-chunk operation tracking.** Whole-file granularity matches the watcher's existing event model and the deferred sync's stated "files are atomic" rule. Anything finer-grained is out of scope.
- **CRDT or operation transformation.** Same reasoning — the sync layer commits to last-write-wins, not merge.
- **Encrypted-at-rest changes.** When sync lands and ciphertext-on-server is real, the local `changes.db` still holds plaintext. The sync transport encrypts on its own.
- **Frontmatter "history" embedded in note files.** All changelog data lives in `.hiker/changes.db`, never in source notes. Notes stay clean.


## Unified activity feed

The activity detail page, the editor status-bar version dropdown, and the queue-bar pending count all need a single chronological feed that interleaves committed `core::changes` rows with pending `core::staging` proposals. To keep the frontend honest, the merge happens in the backend and is exposed as one DTO; consumers don't reconcile two lists. [activity-feed-merged]

The headline decisions:

- **`core::activity` is the merge module.** Thin wrapper that owns the union DTO and the merged query. Depends on `core::changes` and `core::staging`; neither depends on it. No new on-disk state — `core::activity` is pure projection over the two existing stores. [activity-feed-module]
- **One DTO (`ActivityItem`) covers both kinds.** A tagged enum (`kind: "change" | "staging"`) with a flat shared envelope (id, timestamp, path, author, summary) plus a per-kind payload. The frontend renders rows from this DTO with no further reconciliation. [activity-feed-unified-item]
- **Source filter is a first-class arg.** Every query takes a `source` of `changes_only` | `staging_only` | `merged`. `changes_only` and `staging_only` return only their respective rows (existing surfaces keep working unchanged); `merged` returns the interleaved feed. The merged variant is the new third option; the two single-source variants are preserved so per-surface filtering stays cheap. [activity-feed-source-filter]
- **Staging items carry metadata projected from `pending.json`.** The staging payload on `ActivityItem` exposes the proposal's `surface`, `action`, `target_path`, `trail_id`, `session_id`, `content_hash`, `created_at`, and the free-form `metadata` blob — all sourced from the staging index entry, not re-read from the `.md` file. Content blobs are still fetched on demand via the existing `staging_content` command. [activity-feed-staging-metadata]
- **Ordering is single-key.** Items sort by `timestamp_ms` desc with `id` as the deterministic tiebreaker (staging ids are string-sortable timestamps; changes ids are monotonic ints — the merge keys off the timestamp and only consults id when timestamps collide). [activity-feed-merge-ordering]

### DTO

```rust
pub enum ActivitySource { ChangesOnly, StagingOnly, Merged }

pub struct ActivityItem {
    pub timestamp_ms: i64,
    pub path: String,                  // target_path for staging, path for changes
    pub author: String,                // 'pending' for staging items; ChangeRow.author otherwise
    pub summary: ActivitySummary,      // short label material; UI formats display strings
    pub payload: ActivityPayload,
}

pub enum ActivityPayload {
    Change(ChangeRow),                 // existing DTO from core::changes
    Staging(StagingItem),              // see below
}

pub struct StagingItem {
    pub id: String,                    // proposal id
    pub surface: String,               // chat | trail | tree | editor | activity | ...
    pub action: String,                // write_note | apply_tag | set_frontmatter | trail_create | ...
    pub target_path: String,
    pub trail_id: Option<String>,
    pub session_id: Option<String>,
    pub content_hash: Option<String>,
    pub created_at_ms: i64,
    pub metadata: serde_json::Value,   // the rest of pending.json's metadata blob, unparsed
}
```

The `StagingItem` fields mirror the columns the `pending.json` index already carries (per `settings.md` `## Storage`), with the `metadata` blob preserved verbatim so consumers that already know how to read MCP-specific or trail-specific fields keep working without a schema bump here. [activity-feed-staging-metadata]

### Query surface

```rust
impl Activity {
    pub fn list(&self, filter: ActivityFilter) -> Result<Vec<ActivityItem>>;
    pub fn list_for_path(&self, path: &str, filter: ActivityFilter) -> Result<Vec<ActivityItem>>;
    pub fn count(&self, filter: ActivityFilter) -> Result<u32>;
}

pub struct ActivityFilter {
    pub source: ActivitySource,        // ChangesOnly | StagingOnly | Merged
    pub limit: usize,
    pub author_pattern: Option<String>,// e.g. 'agent:%'; staging items match the synthetic 'pending'
    pub since_ms: Option<i64>,         // exclude items older than this
}
```

`ActivitySource::Merged` runs both underlying queries with the same effective limit, then merges by timestamp and truncates. The two source-only variants short-circuit to the existing `Changes::recent` / `Staging::list` paths and wrap each row in the unified envelope — no behavior change for surfaces that ask for a single source. [activity-feed-merged-query]

Host surface: a single `activity_list` command takes `ActivityFilter`; `activity_list_for_path` is the per-buffer variant the status-bar dropdown consumes. The pre-existing `recent_changes` / `staging_list` commands stay (they're cheaper and used by surfaces that genuinely want only one side), but the activity detail page migrates to `activity_list` so its merge-and-render code shrinks to a flat map.

### Consumers

- **Activity detail page** (`vault-home-recent-activity-detail`) — calls `activity_list({ source: Merged })`. The three filter pills (per `vault-home-recent-activity-filter-pills`) compose into the query: the show-staging toggle flips `source` between `Merged` (on) and `ChangesOnly` (off); the user / agent pills set `author_pattern` for the changes side. With show-staging off + agent off + user on, the page is "my own activity only" — the common audit case. Rows render off `ActivityItem`; no client-side merge. [activity-feed-activity-detail-consumer]
- **Editor status-bar version dropdown** (`status-bar-version-dropdown`) — calls `activity_list_for_path(path, { source: Merged })` to populate snapshot + staging entries in one pass.
- **Queue-bar combined badge** (`staging-review-top-bar-badge`) — calls `activity_count({ source: StagingOnly })` for the pending portion of the combined count. No behavior change.

### Module placement

- `core::activity` — `Activity` struct, the DTOs above, `list` / `list_for_path` / `count`. Constructed at vault open with `Arc<Changes>` + `Arc<Staging>` handles; lives next to them on the `VaultSession`. Pure projection — no fs or db writes of its own.

### What this doesn't change

- **`core::changes` and `core::staging` keep their existing APIs.** `Changes::recent` / `recent_by_author` / `history_for_path` and `Staging::list` / `count` stay exactly as today; surfaces that don't want the merge keep calling them directly. The new `Activity` module is additive. [activity-feed-source-filter]
- **No new on-disk state.** No new tables, no new files, no new GC. The merge is a query-time operation over the two existing stores.
- **`metadata` semantics stay opaque at the merge layer.** `core::activity` does not parse the staging `metadata` blob; it forwards it. Per-consumer interpretation (e.g. "this is an MCP write" vs. "this is a batch mutation") happens in the consumer or in a thin helper layered above.


## Forward refs

- `mcp.md` (forthcoming) — agent rollback consumes this; write tools stamp `author='agent:*'`.
- `design.md` "Ideas for integrated syncing" — the deferred sync layer rides on `changes.db` rows. Pull semantics, watermarks, encryption are sync's concern.
- `editor.md` vault home page — the "agent activity" widget queries `recent_by_author('agent:%')`. Detail view exposes per-row diff + rollback.
- Future per-file history view (deferred slug, no spec yet) — queries `history_for_path` for any note.
- Future "today's changes" widget on home page (deferred) — queries `recent` for a configurable time window. Useful daily-review surface.
