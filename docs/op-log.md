# Op log

The single substrate every write rides on. One Yrs document per hiker document plus a side-table of editorial metadata (author, status, surface). Markdown on disk is the canonical materialization of *accepted* operations; pending agent operations are held in a per-document queue until the user accepts. Replaces the prior `changes.db` + `staging.db` split.

The headline decisions:

- **One Yrs `Doc` per hiker document.** Native markdown notes have a Yrs Doc keyed by the note's `doc_id`. Non-md sources (PDF, image, audio, external) have Yrs Docs keyed by the *sidecar* — the .md alongside the source (or the `.hiker/external/<id>--slug.md` for external-pointer mode per `design.md`'s "Source-derived notes"). Sidecars are the unit of CRDT history; the underlying source is read-only as far as the log is concerned. [op-log-document-identity, op-log-sidecar-document]
- **Yrs is the CRDT library.** Mature Rust port of Yjs; structured-data primitives (`Y.Text`, `Y.Map`, `Y.Array`) cover both markdown body and structured frontmatter. Concurrent edits to e.g. `frontmatter.tags` merge as set-union rather than producing YAML conflict salad. Per-document `client_id` distinguishes user from agent from remote devices. [op-log-yrs-backed]
- **Markdown on disk is canonical.** For every native-md note and every sidecar, the on-disk `.md` file equals the materialization of the document's *accepted* Yrs state. Copying the vault's markdown out gives the latest accepted state with no separate state to ship alongside. [op-log-disk-canonical]
- **Two-doc model per document: `accepted` + `pending_view`.** `accepted` is the synced CRDT state — all operations authorized to land on disk and propagate across devices. `pending_view` is a local-only Doc derived by cloning `accepted` and applying queued pending operations on top; the buffer renders from `pending_view`. The diff between them is what the user reviews. [op-log-two-doc-model]
- **Pending operations live in a local queue, not the synced CRDT.** Agent operations enter as serialized Yrs updates in `<doc-id>.pending` paired with side-table metadata (`author`, `surface`, `session_id`, `batch_id`). Accept applies the update to `accepted`; reject discards. Pending ops never sync — they're editorial state, not collaborative state. [op-log-pending-queue]
- **Editorial metadata lives in a side table.** Yrs operations are positional CRDT primitives without per-op author tags; hiker keeps `oplog_meta.db` keyed by `(doc_id, op_id)` with `{ author, op_kind, status, surface, session_id, batch_id, metadata }`. The `author` vocabulary is the same as the prior changelog. [op-log-author-classes, op-log-status-states, op-log-side-table]
- **Each op carries a logical kind.** A typed `OpKind` (`Replace` / `SetFrontmatter` / `Rename` / `Create` / `Tombstone`) rides on every op — on the `PendingOp` while pending, copied to the `op_metadata` row on accept — so the activity feed, rollback, and agent introspection have a typed handle over the otherwise-opaque Yrs update. [op-log-op-shape]
- **Hunks are a view over two materializations.** The diff the user reviews is `diff(materialize(accepted), materialize(pending_view(session)))`. Per-hunk accept applies the contributing pending updates to `accepted`; per-hunk reject drops them from the queue. The diff is recomputed from ropes each frame — `DiffLayer` from `diff.md` is the rendering primitive, unchanged. [op-log-hunk-view, op-log-per-hunk-accept-reject]
- **External edits are reconciled into the CRDT.** A `.md` file that changes on disk outside hiker (Syncthing receive, manual edit) is detected by hash mismatch against `materialize(accepted)`; the delta is computed and applied to `accepted` as a Yrs update tagged `author=external`. The CRDT absorbs external state cleanly. [op-log-external-edit-sync]
- **Embeddings sync as an LWW cache, not through the CRDT.** Embedding vectors are content-derived; they don't have meaningful concurrent edits. Sync as a separate content-addressed blob store keyed by `(content_hash, model_version)`. Each device can regenerate locally if the cache is missing. [op-log-embeddings-lww-cache]


## Document identity

A document is whatever owns one Yrs Doc + one pending queue. Three kinds:

| Source location | Source type | Document is             | Yrs Doc keyed by |
| --------------- | ----------- | ----------------------- | ---------------- |
| Vault-internal  | markdown    | the `.md` file itself   | `doc_id` (ulid)  |
| Vault-internal  | non-md      | the sidecar `<src>.md`  | sidecar's `doc_id` |
| External        | any         | `.hiker/external/<id>--slug.md` | sidecar's `doc_id` |

[op-log-document-identity, op-log-sidecar-document]

Native markdown notes: the file on disk is the document. Sidecars decouple user-edited content (the sidecar `.md`) from source (PDF, image, etc.); the Yrs Doc applies to the sidecar.

**External handles** for source pointers outside the vault use a logical scheme that survives device differences: [op-log-external-handle]

```yaml
hiker:
  source_ref: myrepo://docs/api.md
```

`myrepo` is a named root resolved per-device via user-scope `[external_roots]` config (not synced). The sidecar (and its Yrs Doc) syncs across devices; the root-resolution mapping is device-local.


## Yrs document shape

Each hiker document is one Yrs Doc with a fixed root layout:

```
Y.Doc
├── body: Y.Text          # the markdown body (frontmatter stripped)
├── frontmatter: Y.Map    # parsed YAML/TOML frontmatter
│     ├── hiker: Y.Map         (nested CRDT-correct merges per key)
│     ├── tags: Y.Array        (concurrent edits union as set semantics)
│     └── ... user-defined keys, mapped to Y.Map / Y.Array / Y.Text / primitive as appropriate
└── meta: Y.Map           # document-level state not in the file
      ├── kind: string         ("note" | "sidecar" | "trail" | ...)
      ├── source_ref: string?  (for sidecars; the logical handle)
      └── tombstone: bool      (true iff deleted)
```

[op-log-document-shape]

**Why `body` as `Y.Text` rather than a sequence of paragraphs:** matches the user's mental model (a markdown file is a stream of text), supports keystroke-level CRDT merge during concurrent edits, and lets the editor's existing rope-based buffer stay unchanged — the buffer just reads `Y.Text` as a string and emits Yrs operations on edit.

**Why frontmatter as `Y.Map` of typed children:** so that `frontmatter.tags.push("new-tag")` on device A merges set-union with `frontmatter.tags.push("other-tag")` on device B, rather than producing a YAML text conflict. Same applies to `frontmatter.hiker.suggested_tags`, `frontmatter.references`, etc. The frontmatter parser maps YAML strings to `Y.Text`, lists to `Y.Array`, dicts to nested `Y.Map`. Re-serialization on materialize.

**Materialization writes the canonical `.md`:**

```rust
fn materialize(doc: &yrs::Doc) -> String {
    let txn = doc.transact();
    let body = doc.get_text("body").get_string(&txn);
    let fm   = doc.get_map("frontmatter");
    let yaml = yaml_serialize(fm, &txn);   // structured → YAML
    format!("---\n{yaml}---\n\n{body}")
}
```

Pure function over Yrs state; no I/O.


## Two-doc model

Each document maintains two Yrs Docs in memory and on disk:

- **`accepted`** — the canonical CRDT state. Synced across devices. Contains every op the user has authorized to reach disk: their own typing, external edits, sync receives, accepted agent edits, accepted extractor re-runs.
- **`pending_view(session)`** — a per-agent-session local-only Doc. Constructed by cloning `accepted` and applying the session's queued pending updates on top. The editor's buffer reads from `pending_view`; the diff against `accepted` produces the review hunks.

[op-log-two-doc-model]

When more than one agent session has pending ops on the same document, each gets its own `pending_view(session_X)`. The file pill lists sessions; clicking one swaps the active view.

Why this shape rather than a single Doc with status tags on every op: Yrs operations are positional CRDT primitives without first-class metadata, and Yrs doesn't natively support "materialize a subset of operations." Two Docs is the practical workaround — `accepted` is the synced truth, the pending queue is a deferred-apply buffer. The cost is that pending operations don't propagate across devices (a pending agent edit on laptop doesn't show up on phone) — which is actually the desired semantic. Pending state is editorial, per-device, not collaborative.


## Pending queue

```rust
// <doc-id>.pending — one row per pending update
pub struct PendingOp {
    pub op_id: OpId,                    // ulid
    pub yrs_update: Vec<u8>,            // serialized Yrs update bytes; applies against `accepted`'s current state
    pub op_kind: OpKind,                // logical shape of the edit; see "Op shapes"
    pub author: Author,                 // agent:<client-id> | auto:* | extractor:*
    pub session_id: Option<String>,
    pub surface: String,                // "mcp-tool-call" | "triage" | "extractor" | ...
    pub batch_id: Option<String>,       // groups e.g. multi-edit `edit_note` calls
    pub created_at_ms: i64,
    pub metadata: serde_json::Value,
}
```

[op-log-pending-queue]

Pending updates are pre-computed: when the agent calls `edit_note(rel_path, [{ old_str, new_str }, ...])`, the producer:

1. Reads `materialize(accepted)` once.
2. For each edit, resolves `old_str` to a byte range against the materialization.
3. Translates the byte range to Yrs `Y.Text` positions inside a *clone* of `accepted`.
4. Applies each edit to the clone, serializing the resulting Yrs update.
5. Stores each update as one `PendingOp` row sharing a `batch_id`.

The clone is discarded; only the serialized update bytes are kept. Accept applies the bytes to `accepted`; reject drops them. [op-log-agent-replica]

**Drift detection.** When `accepted` advances (user typing, external edit, sync receive, an earlier pending op accepted), the queued updates may no longer apply cleanly — their position anchors point at content that has changed. Drift is derived on demand: try to apply the update to a clone of current `accepted`; if Yrs reports a position-resolution failure or the resulting materialization doesn't contain the agent's intended new content, the op is *drifted*. Surface in the file pill's `(M drifted)` count; Accept disabled, Reject active. Per `op-log.md` config, `auto_reject_on_drift = true` flips drifted ops to rejected automatically.


## Editorial metadata side table

Yrs ops don't carry author/status/surface. Hiker layers this in `oplog_meta.db` — one SQLite database for the whole vault: [op-log-side-table]

```sql
CREATE TABLE op_metadata (
    doc_id        TEXT NOT NULL,
    op_id         TEXT NOT NULL,          -- ulid; for accepted ops, matches a Yrs op range
    yrs_client_id INTEGER NOT NULL,       -- which Yrs client_id authored this op range
    yrs_clock_lo  INTEGER NOT NULL,       -- inclusive lower bound on Yrs clock
    yrs_clock_hi  INTEGER NOT NULL,       -- exclusive upper bound
    author        TEXT NOT NULL,          -- 'user' | 'agent:<id>' | 'external' | 'extractor:<id>' | 'auto:<producer>' | 'sync:<device>'
    op_kind       TEXT NOT NULL,          -- 'replace' | 'set_frontmatter' | 'rename' | 'create' | 'tombstone' (see "Op shapes")
    rename_from   TEXT,                   -- prior vault-relative path; non-NULL only when op_kind = 'rename'
    status        TEXT NOT NULL,          -- 'accepted' | 'rejected'  (pending lives in <doc-id>.pending)
    timestamp_ms  INTEGER NOT NULL,
    surface       TEXT,                   -- producer's surface name
    session_id    TEXT,
    batch_id      TEXT,
    metadata      TEXT NOT NULL DEFAULT '{}'
);

CREATE INDEX op_metadata_doc_ts ON op_metadata(doc_id, timestamp_ms DESC);
CREATE INDEX op_metadata_author_ts ON op_metadata(author, timestamp_ms DESC);
CREATE INDEX op_metadata_status ON op_metadata(status, timestamp_ms DESC);
```

The `(yrs_client_id, yrs_clock_lo, yrs_clock_hi)` tuple identifies the Yrs op range this metadata row describes. Hiker logical ops can span multiple Yrs ops (e.g. `edit_note`'s `Replace { old_str, new_str }` = delete + insert in Yrs); one metadata row covers the whole range.

**Why a single side-table file for the whole vault** rather than per-document: cross-document queries (the activity feed, "show me everything agent claude did today") need to scan many documents. One indexed SQLite database serves these efficiently; per-document files would require scanning every file. The trade-off: opening a document needs both the Yrs Doc *and* a small metadata query — fine in practice.

`oplog_meta.db` is durable. Like the prior `changes.db`, losing it loses authorship history but not document content (which lives in the Yrs Docs themselves).


## Op shapes

A Yrs update is an opaque position-delta; hiker layers a logical `OpKind` over each one so the activity feed, rollback, and agent introspection have a typed handle. The kind is born with the op — carried on `PendingOp.op_kind` while pending, copied to the `op_metadata` row on accept — so it's available before *and* after the op reaches `accepted`. [op-log-op-shape]

```rust
pub enum OpKind {
    Replace { anchor: Option<AnchorHint> },  // a body Y.Text edit; `anchor` carries the edit_note old_str
    SetFrontmatter,                          // a frontmatter Y.Map edit
    Rename { from: String },                 // a meta.path change; `from` is the prior vault-relative path
    Create,                                  // the first op establishing a new document
    Tombstone,                               // sets meta.tombstone = true
}
```

One logical op = one Yrs update = one `op_metadata` row over one `(yrs_client_id, yrs_clock_lo, yrs_clock_hi)` range. A `Replace`'s delete+insert is two Yrs ops but one logical op (one row). Producer → op-kind mapping:

| Producer call | Op kind(s) |
| ------------- | ---------- |
| `edit_note([e1, e2, …])` | one `Replace` per edit, all sharing a `batch_id`; accept/reject each independently |
| `write_note` (whole body) | one `Replace` spanning the whole `body` |
| `set_frontmatter` / `apply_tag` / `remove_tag` | one `SetFrontmatter` |
| `move_note` / rename | one `Rename { from }` |
| create a new note | one `Create`, then the content `Replace` |
| delete a note | one `Tombstone` |

**`AnchorHint`** is the `Replace`'s `old_str` kept as `{ hash, preview }` (not the full text). Two consumers re-check it against `materialize(accepted)`: drift detection (per "Pending queue") and `mcp.md`'s `get_pending_proposal` anchor-status. Whole-body `Replace`, `SetFrontmatter`, and `Rename` carry no anchor.

**Projections.** `changes.md`'s `ChangeOpKind` is the coarse user-facing projection of `op_kind`: `Create → Created`, `Replace` / `SetFrontmatter → Modified`, `Tombstone → Deleted`, `Rename → Renamed` (with `rename_from` from the side-table column). `mcp.md`'s pending-op introspection reads `op_kind` (and `anchor`) directly off the `PendingOp`.


## Storage layout

```
vault/.hiker/oplog/
  <doc-id>.yrs               # Yrs Doc serialized state (binary, Yrs v2 update format)
  <doc-id>.pending           # bincode-serialized Vec<PendingOp>
  oplog_meta.db              # SQLite side table (one for the whole vault)
  doc-index.db               # SQLite: path → doc_id mapping
```

[op-log-store-layout]

- **`<doc-id>.yrs`** is the Yrs Doc's serialized state. Written via `Doc::encode_state_as_update_v2(&Default::default())` (full state) on save; sync delivers incremental `update_v2` payloads that get applied via `Doc::apply_update`. Yrs v2 update format includes the necessary compression / RLE.
- **`<doc-id>.pending`** is appended on agent op submit, rewritten on accept/reject. Pending ops persist here across restarts; reopening a document reconstitutes them. [op-log-pending-survives-restart]
- **`oplog_meta.db`** is one vault-wide SQLite database (WAL mode, `synchronous=NORMAL`).
- **`doc-index.db`** maps vault-relative paths to `doc_id` for path-based lookup. Regenerable by scanning Yrs Docs' `meta.path` field — losing it triggers a full rescan.

**Compaction.** Yrs Docs accumulate operation history; over time the `.yrs` file grows even without user-visible content growth (tombstones, position metadata). Yrs supports `Doc::encode_state_as_update_v2(&prior_state_vector)` to produce a compact snapshot. Schedule: on vault open, if `<doc-id>.yrs` is larger than 4× the materialized content size, rewrite as a fresh snapshot. Side table's `status='rejected'` rows GC per `[op-log] rejected_retention_days`. [op-log-compaction]


## Materialization

```rust
fn materialize(doc: &yrs::Doc) -> Materialized {
    let txn = doc.transact();
    Materialized {
        body: doc.get_text("body").get_string(&txn).into(),
        frontmatter: yaml_from_ymap(doc.get_map("frontmatter"), &txn),
        tombstone: doc.get_map("meta").get(&txn, "tombstone").as_bool().unwrap_or(false),
    }
}
```

[op-log-materialization]

Pure read over the Yrs Doc. Drives every diff render, save-to-disk, accept dry-run. The buffer renders from `materialize(pending_view(session))`; the canonical disk file equals `materialize(accepted)`.


## Disk write invariant

Saving runs in this order: [op-log-atomic-write]

1. Hiker commits in-memory Yrs operations to the `accepted` Doc via a `Doc::transact_mut`.
2. Persist the updated Yrs state to `<doc-id>.yrs` (write-temp-then-rename + fsync).
3. Persist any new `op_metadata` rows.
4. Compute `materialize(accepted)` and atomically write the `.md` to disk (write-temp-then-rename + fsync).

If the process crashes between (2) and (4), the next open re-runs (4) — Yrs is the source of truth, the `.md` is its projection. If the process crashes during (2), the partial write is detected (Yrs's update format has integrity checks); the previous good state is preserved.

Pending operations don't trigger steps (2)-(4) — they remain in `<doc-id>.pending` only.


## External-edit sync

Watcher (per `watcher.md`) reports a `.md` file change hiker didn't initiate: [op-log-external-edit-sync]

1. Read the file's current bytes.
2. Compute `materialize(accepted)`.
3. If they match, the watcher event is a self-write echo — ignore (existing `watcher-suppress-self-writes` machinery still applies; this is the safety net).
4. If they differ, diff materialization → disk_bytes, translate the diff into Yrs `Y.Text` / `Y.Map` operations, apply to `accepted` inside a transaction tagged with `author=external` in the metadata side table.

The CRDT absorbs the external edit cleanly. Concurrent in-app edits race the same way they do today — last writer wins at the materialization layer, and the editor's `pre-write-drift-check` still fires when an in-buffer save would overwrite an external edit it hadn't seen.

For sidecars, external edits on the *source* (the PDF) trigger re-extraction, not a direct synthesized op on the sidecar — see "Re-extraction" below.


## Author classes

The `author` field is recorded in `op_metadata` for every Yrs operation range hiker authors. Vocabulary: [op-log-author-classes]

- `user` — keystroke / save / direct UI action.
- `agent:<client-id>` — an MCP-attached agent's tool call. `<client-id>` from MCP handshake.
- `external` — file on disk changed outside hiker; reconciled via the external-edit-sync path.
- `extractor:<plugin-id>` — a source extractor re-ran. Preserves the future WASM-extractor case without committing to it now.
- `auto:<producer>` — unattended write from internal automation (`auto:triage` per `suggestions.md`).
- `sync:<device-id>` — Yrs operations received from another device via the sync transport.

Class prefix supports wildcard (`author LIKE 'agent:%'`) and exact (`author = 'agent:claude-code'`) queries.


## Status states

Three states, tracked across the side table + pending queue: [op-log-status-states]

- **`accepted`** — the Yrs op is in `accepted`'s state; the side-table row's `status = 'accepted'`. Counted in `materialize(accepted)` and reaches disk.
- **`pending`** — the op is in `<doc-id>.pending` as a serialized update; *not yet* applied to `accepted`. Visible only to the producing session's `pending_view`. No side-table row exists yet (the op has no Yrs client_id range until it lands in `accepted`).
- **`rejected`** — the op was previously pending, the user said no; an audit row is written to `op_metadata` with `status = 'rejected'` and the serialized update bytes stashed in the row's metadata for "what did the agent suggest that I said no to?" queries. The op never enters `accepted`.

GC removes `rejected` rows after `[op-log] rejected_retention_days`. Pending entries never auto-GC — they sit in the queue until the user resolves them.


## Hunk view

```rust
let base    = materialize(accepted).body;
let current = materialize(pending_view(active_session)).body;
let layer   = DiffLayer { base, current, owner: DiffOwner::Agent };
```

[op-log-hunk-view]

Per-hunk accept queries the pending queue for ops whose `yrs_update` overlaps the hunk's `current_range` (resolved by applying the update to a clone and checking the affected position range). Applies those updates to `accepted`, removes them from `<doc-id>.pending`, writes side-table rows with `status='accepted'`. [op-log-per-hunk-accept-reject]

Per-hunk reject does the same lookup but writes side-table rows with `status='rejected'` instead of applying to `accepted`.

Per-op flip (sub-hunk granularity) is the underlying primitive; expose as UI later if real workflows need it. [op-log-per-op-status-flip]

**User's own typing** appends Yrs operations directly to `accepted` with `author='user'`, `status='accepted'`. Their typing shows up in `pending_view` (because `pending_view` is `accepted + pending` and accepted now includes the typing) but produces no diff hunks against itself.


## Whole-file and structured proposals

Some agent operation shapes don't compose into the per-hunk view:

- **Whole-body rewrite** — `write_note` MCP calls produce a Yrs update that clears `body` and inserts new content. Reviewed via the whole-file review surface (`patch-review.md` `write-note-review-surface`), not per-hunk.
- **`SetFrontmatter` patches** — produce Yrs updates against `frontmatter`. Reviewed as a structured key-by-key diff.
- **`Create` / `Tombstone` / `Rename`** — top-level lifecycle operations; reviewed as confirm-style cards.

Yrs handles all of these uniformly as updates against the Doc; the review surface picks the right rendering per the op's `op_kind` (per "Op shapes", `op-log-op-shape`).


## Re-extraction

Sidecar documents whose source has changed (or whose extractor version has bumped) need a controlled way to re-pull extracted content. User picks the policy per re-run: [op-log-reextract-replace]

| Policy | Behavior |
| ------ | -------- |
| **Replace** | Apply a Yrs update to `accepted` that replaces `body` with the new extraction. `author = extractor:<id>`, `status = accepted`. Concurrent user edits in *non-extracted* regions (frontmatter, hand-annotated additions) are preserved by Yrs's CRDT merge. |
| **Skip** | Don't run the extractor. The sidecar stays as-is. (Matches the prior `link_state: unlinked` semantics.) |
| **Merge** (deferred) | Apply the new extraction as a pending op so the user reviews before accept. Same hunk-review machinery as agent edits. [op-log-reextract-merge-deferred] |
| **Diff-and-prompt** (deferred) | Show the diff inline; let the user pick which hunks of the new extraction to apply. Built on `op-log-reextract-merge-deferred`. [op-log-reextract-diff-prompt-deferred] |

`Replace` (the default for previously-`linked` sidecars) and `Skip` (the default for previously-`unlinked` sidecars) ship first; the others land when needed.


## Embedding sync

Embeddings are derived data — same content + same model → same embedding (modulo floating-point variance). They don't have meaningful concurrent edits and don't belong in the CRDT. [op-log-embeddings-lww-cache]

```
vault/.hiker/embeddings/
  <model_version>/<content_hash[:2]>/<content_hash>.bin    # one vector per content-addressed key
  index.db                                                 # SQLite: (doc_id, chunk_index, content_hash, model_version) → blob path
```

Sync model:

- **Content-addressed.** Each embedding vector is keyed by `(content_hash, model_version)`. Same content + same model on two devices produces the same key — sync collapses to "send me blobs I don't have."
- **LWW per `(doc_id, chunk_index)` mapping.** When two devices both embed the same chunk with different `content_hash` (because they extracted different content), the latest writer's hash wins as the "current embedding" for that chunk. Stale embeddings GC when no chunk references them.
- **Each device can regenerate locally** if the cache is missing. The sync layer is best-effort; missing embeddings just trigger local re-indexing.
- **Not in `oplog_meta.db`.** Embedding sync is a separate transport. The Yrs Doc carries no embedding state.

This keeps the CRDT lean (Yrs Docs don't bloat with vector data) and makes the "compute embeddings on the cheap device, sync to the expensive one" workflow possible — desktop embeds, phone receives the vectors and can serve search without running the embedder locally.


## Module placement

- `core::oplog` — owns `<doc-id>.yrs` files, `<doc-id>.pending` files, and `oplog_meta.db`. Exposes `OpLog::open`, `OpLog::apply_user_edit`, `OpLog::stage_pending`, `OpLog::accept_pending`, `OpLog::reject_pending`, `OpLog::materialize_accepted`, `OpLog::materialize_pending_view`, `OpLog::query_metadata`. The Yrs dependency is confined to this crate; consumers see plain Rust types. [op-log-module]
- `core::ops` — wraps `OpLog` with the higher-level write paths (`write_file`, `agent_write_note`, `agent_edit_note`, `flip_op_status`). Producers don't talk to `OpLog` directly.
- `core::changes` — projection layer over `OpLog::query_metadata`; returns `ChangeRow` DTOs.
- `core::activity` — merged activity feed across accepted + pending. Pure projection over `OpLog`.
- `core::embed` — embedding sync layer; content-addressed blob store separate from `core::oplog`.
- `app` — the editor pane consumes `OpLog::materialize_pending_view`; per-hunk verbs route through `core::ops::flip_op_status`.


## `[op-log]` config section

Replaces the prior `[staging]` section. [op-log-config-section]

```toml
[op-log]
metadata_retention_days = 365      # GC threshold for op_metadata.status='accepted' rows
rejected_retention_days = 14       # GC threshold for op_metadata.status='rejected' rows
auto_reject_on_drift = false       # when a pending op's anchor no longer resolves, flip to rejected
review_required = true             # default status for agent writes; surface-specific overrides win
compact_threshold = 4              # rewrite <doc-id>.yrs as a fresh snapshot when its size > N× materialized
```

| Key | Type | Default | Scope | Notes |
| --- | ---- | ------- | ----- | ----- |
| `metadata_retention_days` | u32 | `365` | user + vault | GC age for accepted-op metadata rows. The Yrs Doc content lives forever (it's the document); only the side-table author/timestamp data is bounded. |
| `rejected_retention_days` | u32 | `14` | user + vault | Faster GC for rejected ops. |
| `auto_reject_on_drift` | bool | `false` | user + vault | Replaces the prior `[staging].auto_reject_on_conflict`. |
| `review_required` | bool | `true` | user + vault | Default `status` for agent-authored ops. Surface-specific overrides (`[mcp.tools].review_required`, `[llm.background].review_required`) apply. |
| `compact_threshold` | f32 | `4.0` | user + vault | Yrs Doc size multiple over materialized size that triggers compaction on vault open. |


## Sync substrate

The sync transport (deferred, per `design.md`'s "Ideas for integrated syncing") ships Yrs updates between replicas: [op-log-multi-device-sync-deferred]

- **Yrs's native update protocol.** `Doc::encode_state_as_update_v2(&peer_state_vector)` returns "ops since this watermark." Apply via `Doc::apply_update`. This is the standard Yjs sync pattern; battle-tested at Google-Docs scale.
- **Per-document streams.** Each `<doc-id>.yrs` syncs independently. State vectors per (doc_id, peer_device_id) tracked in a per-device sync watermark table.
- **Embedding sync rides alongside** but separate — content-addressed blob diff (see "Embedding sync" above).
- **Cluster trees sync as regular markdown documents** once trees move from `trees.db` to per-tree `.md` files (deferred work tracked separately). Until then, trees stay in `trees.db` and are simply not synced in v1.
- **Editorial metadata syncs as a CRDT-merged side stream.** `op_metadata` rows whose `(yrs_client_id, yrs_clock_lo, yrs_clock_hi)` range matches an op already in the synced Doc apply; rows whose range doesn't match yet wait. Concurrent metadata writes to the same op (e.g. two devices both reject) resolve LWW per `(op_id, timestamp_ms)`.

Sync transport, encryption, conflict copies, key management — all stay deferred. This doc specs the *substrate* the sync layer rides on.


## Out of scope

- **Sync transport.** Substrate only.
- **WASM source-type plugins.** Re-extraction policies are specced so the future WASM-extractor path slots in cleanly, but the plugin host itself is not part of this spec.
- **Cross-document atomic operations.** Each Yrs Doc is independent. Multi-file refactors are N independent op streams; no transactional grouping primitive.
- **Encryption at rest.** Orthogonal to the log shape.
- **Per-character review UI.** `op-log-per-op-status-flip` lands the primitive; surfacing as UI is deferred.


## Deferred

- `op-log-multi-device-sync-deferred` — sync transport spec.
- `op-log-reextract-merge-deferred` — pending-op-shaped re-extraction policy.
- `op-log-reextract-diff-prompt-deferred` — interactive re-extraction review.


## Forward refs

- `changes.md` — user-facing rollback / history / activity feed surfaces, queryable over `OpLog::query_metadata`.
- `patch-review.md` — per-hunk agent-edit review surface, built on the two-doc model.
- `diff.md` — `DiffLayer` primitive, unchanged.
- `design.md` "Source-derived notes" — sidecar architecture this composes with.
- `design.md` "Ideas for integrated syncing" — the sync layer this substrate enables.
- `mcp.md` — agent tool calls produce pending Yrs updates with `author=agent:<client-id>`.
- `suggestions.md` — triage auto-accepts ride `author=auto:triage`.
- `settings.md` — `[op-log]` config section above replaces the prior `[staging]` section.
- `cluster-editor.md` — trees will move from `trees.db` to per-tree `.md` files (separate spec work); once that lands they ride this substrate like any other markdown document.
