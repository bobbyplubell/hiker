# Op log

The single substrate every write rides on. One Yrs document per hiker document plus a side-table of editorial metadata (author, status, surface). Markdown on disk is the canonical materialization of *accepted* operations; pending agent operations are held in a per-document queue until the user accepts.

Orientation: each hiker document is one Yrs `Doc` (the whole `.md` file as one `Y.Text`) keyed by an internal `doc_id` ULID, with editorial metadata in a side table. The buffer renders three CRDT op layers — `accepted` (on disk, synced) + `working` (uncommitted user edits) + `pending` (agent proposals). The editor binds to the CRDT at the op level. External disk edits (including offline ones) reconcile back in as `author=external` ops. Embeddings sync as a separate content-addressed cache, not through the CRDT. Each unique behavior is specced in its section below.


## Document identity

A document is whatever owns one Yrs Doc + one pending queue. Three kinds:

| Source location | Source type | Document is             | User-facing identity | Internal Yrs Doc key |
| --------------- | ----------- | ----------------------- | -------------------- | ---------------------- |
| Vault-internal  | markdown    | the `.md` file itself   | vault path           | `doc_id` (ulid)        |
| Vault-internal  | non-md      | the sidecar `<src>.md`  | sidecar's vault path | sidecar's `doc_id`     |
| External        | any         | `.hiker/external/<id>--slug.md` | TODO              | sidecar's `doc_id`     |

[op-log-document-identity, op-log-sidecar-document]

Native markdown notes: the file on disk is the document. Users reference notes by their vault path; the op-log keeps a separate `path → doc_id` mapping (`doc-index.db`) so the Yrs Doc and its history survive renames without any user-facing identifier changing. Sidecars decouple user-edited content (the sidecar `.md`) from source (PDF, image, etc.); the Yrs Doc applies to the sidecar.

<!-- TODO: external-source documents currently identified by an internal `<id>--slug.md` filename + a separate `hiker.source_ref` external-handle scheme. Decision pending on whether to keep external sources in scope at all under the path-as-id model. Don't rework this row until that decision is made. -->

**Rename updates the mapping, not the doc_id.** When a note is renamed (`move-note-core-cmd`, `drag-and-drop-move`, external rename via watcher), `doc-index.db` rewrites the `path → doc_id` entry — the new path now maps to the same `doc_id`, and the Yrs Doc continues unchanged. The op-log records the rename as a logical op (`OpKind::Rename { from }`); no replay of history happens.

**External handles** for source pointers outside the vault use a logical scheme that survives device differences: [op-log-external-handle]

```yaml
hiker:
  source_ref: myrepo://docs/api.md
```

`myrepo` is a named root resolved per-device via user-scope `[external_roots]` config (not synced). The sidecar (and its Yrs Doc) syncs across devices; the root-resolution mapping is device-local.


## Yrs document shape

Each hiker document is one Yrs Doc with a two-field root layout:

```
Y.Doc
├── text: Y.Text     # the entire .md file, frontmatter fence and body, verbatim
└── meta: Y.Map      # internal document state, never written into the file
      ├── kind: string         ("note" | "sidecar" | "trail" | ...)
      ├── path: string         (vault-relative; the doc's current location)
      └── tombstone: bool      (true iff deleted)
```

[op-log-document-shape]

**Why the whole file as one `Y.Text`:** matches the user's mental model (a markdown file is a stream of text), supports fine-grained CRDT merge during concurrent edits, and lets the editor's existing rope-based buffer stay unchanged — the buffer reads `Y.Text` as a string, and each save is diffed against the accepted state into the minimal localized Yrs operations (a char-level diff; untouched bytes — including a concurrent remote edit elsewhere — are never rewritten). [op-log-yrs-backed] Frontmatter rides inside the same `Y.Text` as plain bytes; it gets no structural modeling. The cost: concurrent same-line `tags:` edits across devices merge as character-level text, not clean set-union — acceptable because that race only exists once sync ships, while frontmatter-as-`Y.Map` would make `materialize()` lossy against the user's authored bytes today (reordered keys, dropped comments, coerced scalars, phantom watcher diffs).

**Why `meta` stays a `Y.Map`:** `kind`, `path`, and `tombstone` are document state that never appears in the `.md` file, so they have no round-trip to corrupt. `tombstone` needs cross-device merge semantics (a delete on one device must merge against an edit on another), and `path` is what regenerates `doc-index.db` after a loss (per "Storage layout"). The sidecar's source handle is *not* duplicated here — it lives in the frontmatter text as `hiker.source_ref` (per "Document identity").

**Materialization writes the canonical `.md`:** `materialize(doc)` reads `text: Y.Text` as a string (verbatim bytes, no re-serialization). Pure function over Yrs state; no I/O. The returned string is byte-identical to what the user (or any external editor) last wrote — no parse/re-emit step to reorder keys, drop comments, or coerce scalars. (Full shape under "Materialization".)


## Layered document model

Each open document is the merge of three CRDT op layers over one logical text:

- **`accepted`** — the canonical CRDT state. Synced across devices, materialized to the on-disk `.md`. Holds every op authorized to reach disk: saved user edits, external edits, sync receives, accepted agent edits, accepted extractor re-runs.
- **`working`** — the user's *uncommitted* edits, as `user` ops on top of `accepted`. Local to this device until saved; empty when the buffer is clean. Save folds `working` into `accepted`; nothing in `working` reaches disk or syncs before then. [op-log-working-layer]
- **`pending(session)`** — a per-agent-session queue of proposed ops, staged for review, not yet in `accepted`. Accept folds one into `accepted`; reject drops it. [op-log-pending-queue]

The **editable** buffer is `materialize(accepted + working)` — the user's own text, committed plus uncommitted, so their typing applies at plain buffer offsets with no coordinate translation and `working` never entangles with pending content. The agent's `pending` ops surface *on top* as the inline review overlay — the diff toward `materialize(accepted + working + pending(session))` (per `patch-review.md`) — so the user sees the agent's proposals in place while editing their own regions. Because all three are CRDT op layers, edits in different regions merge by position; accepting or rejecting a pending op rebases the overlay without touching the user's `working` edits, and the user is never forced to switch between "my text" and "the review view". [op-log-layered-model]

Why `working` is its own layer rather than committing user typing straight to `accepted`: it keeps unsaved work as real, mergeable ops — an agent edit elsewhere in the file neither displaces it nor is displaced by it — while still gating disk writes behind an explicit Save. Why `pending` stays separate rather than merging into `working`: pending ops are editorial proposals reviewed per-op, and they never sync — staging is per-device, not collaborative. When more than one agent session has pending ops, each session is its own `pending(session)` overlay; the file pill swaps the active one.


## Editor binding

The editor and the document's CRDT stay in lockstep at the *operation* level — every edit is a CRDT op, both directions: [op-log-editor-binding]

The buffer is `materialize_working`; pending ops render as an overlay (the buffer diffed against `materialize_review = working + pending`: additions as phantom blocks, deletions struck through). This is the y-codemirror.next shape — the editor crate stays CRDT-agnostic; this binding is the only adapter — keeping the user's edits in one coordinate space (`working`) with no offset remapping.

- **Forward (editor → `working`).** The editor emits a change set — a list of retain / delete / insert ops over byte ranges — for each edit it applies. Because the buffer *is* `working`, the offsets are already working coordinates: each delete/insert is applied to the `working` layer as a `user` op directly, no translation.
- **Reverse (CRDT → editor).** When `working` advances without matching user typing — an accepted agent op replayed onto `working`, or an external edit — hiker pulls the new `working` into the buffer and **maps the selection through the change** (CodeMirror's `ChangeSet.mapPos` discipline), so an edit landing above the cursor carries the cursor with it rather than stranding it at a stale offset. Positions are mapped through changes, never clamped.
- **Overlay.** Each frame the binding stashes `materialize_review` as the buffer's `agent_proposal`; the inline review diffs the buffer (`working`) against it to render the pending ops. Cleared when there are no pending ops (`review == working`).
- **Origin tagging.** Host-applied edits (the reverse direction) bypass the widget's input path, so they never re-enter the forward sink — no echo loop, no explicit origin marker needed.
- **Fidelity invariant.** The forward mirror may not silently drop an edit: `materialize(working)` equals the editor's text at all times, asserted at Save. A failed forward apply is a hard error that forces a full resync of `working` from the editor's text, never a logged-and-ignored warning. Without this the reverse step — which treats `working` as the source of truth — would re-point the editor back and erase the user's keystroke on the next frame, and Save would commit the stale `working` while the dirty/clean check reads the editor text, both losing the edit and mis-flagging the buffer clean. [op-log-binding-fidelity-invariant]

Capturing the editor's own change set (rather than re-diffing the whole buffer to guess what changed) is what makes user typing a first-class CRDT op, so the `working` layer merges with `pending` and `accepted` exactly as agent and external ops do — every mutation in the system is a CRDT op (`user`, `agent:*`, `external`, `sync:*`), uniformly syncable.


## Merge and conflicts

`working` ops and `pending` ops merge by position:

- **Disjoint regions** — the user edits one part while the agent edits another: the merge is automatic; both render in the buffer, no prompt. [op-log-merge-auto]
- **Overlapping region** — both change the same span: hiker does *not* silently interleave them (a positional CRDT merge there is deterministic but can interleave into nonsense). The overlap surfaces as a conflict hunk in the inline review with per-hunk **Keep mine** / **Keep theirs** / **Keep both**: keep-mine rejects the agent op over that span, keep-theirs accepts it and drops the user's overlapping edit, keep-both takes the positional merge. Overlap is detected when a `working` edit and a `pending` edit touch the same line region. [op-log-merge-conflict]


## Pending queue

`<doc-id>.pending` holds one `PendingOp` row per pending update: `op_id` (ulid); `yrs_update` (serialized Yrs update bytes, applied against `accepted`'s current state); `op_kind` (logical shape, see "Op shapes"); `author` (`agent:<client-id>` | `auto:*` | `extractor:*`); `session_id`; `surface` (`"mcp-tool-call"` | `"triage"` | `"extractor"` | ...); `batch_id` (groups e.g. multi-edit `edit_note` calls); `created_at_ms`; `metadata`. [op-log-pending-queue]

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

`op_metadata` columns: `doc_id`; `op_id` (ulid; for accepted ops matches a Yrs op range); `yrs_client_id` + `yrs_clock_lo` (inclusive) + `yrs_clock_hi` (exclusive) — the Yrs op range this row describes; `author` (`'user'` | `'agent:<id>'` | `'external'` | `'extractor:<id>'` | `'auto:<producer>'` | `'sync:<device>'`); `op_kind` (`'replace'` | `'set_frontmatter'` | `'rename'` | `'create'` | `'tombstone'`); `rename_from` (prior path, non-NULL only when `op_kind = 'rename'`); `status` (`'accepted'` | `'rejected'`; pending lives in `<doc-id>.pending`); `timestamp_ms`; `content_hash` (blake3 of `materialize(accepted)` as of this op; sync's enrollment hash-classification reads it, `sync-content-hash-column`); `surface`; `session_id`; `batch_id`; `metadata` (default `'{}'`). Indexed on `(doc_id, timestamp_ms DESC)`, `(author, timestamp_ms DESC)`, and `(status, timestamp_ms DESC)`.

The `(yrs_client_id, yrs_clock_lo, yrs_clock_hi)` tuple identifies the Yrs op range this metadata row describes. Hiker logical ops can span multiple Yrs ops (e.g. `edit_note`'s `Replace { old_str, new_str }` = delete + insert in Yrs); one metadata row covers the whole range.

**Why a single side-table file for the whole vault** rather than per-document: cross-document queries (the activity feed, "show me everything agent claude did today") need to scan many documents. One indexed SQLite database serves these efficiently; per-document files would require scanning every file. The trade-off: opening a document needs both the Yrs Doc *and* a small metadata query — fine in practice.

`oplog_meta.db` is durable. Losing it loses authorship history but not document content (which lives in the Yrs Docs themselves).


## Op shapes

A Yrs update is an opaque position-delta; hiker layers a logical `OpKind` over each one so the activity feed, rollback, and agent introspection have a typed handle. The kind is born with the op — carried on `PendingOp.op_kind` while pending, copied to the `op_metadata` row on accept — so it's available before *and* after the op reaches `accepted`. [op-log-op-shape]

`OpKind` variants: `Replace { anchor: Option<AnchorHint> }` (a `text` Y.Text edit; `anchor` carries the `edit_note` old_str); `SetFrontmatter` (a `text` edit whose byte range falls inside the frontmatter fence); `Rename { from: String }` (a `meta.path` change; `from` is the prior vault-relative path); `Create` (the first op establishing a new document); `Tombstone` (sets `meta.tombstone = true`).

`SetFrontmatter` is a **logical label, not a distinct mechanism**: a frontmatter change is a `Replace` on the one `text: Y.Text` whose byte range lands inside the leading `---` fence. The producer tags it `SetFrontmatter` rather than `Replace` so the activity feed and `ChangeOp` projection can say "edited frontmatter" vs "edited body"; the underlying Yrs update is an ordinary text edit. (Mint the label by testing whether the edit's byte range falls before the closing `---`.)

One logical op = one Yrs update = one `op_metadata` row over one `(yrs_client_id, yrs_clock_lo, yrs_clock_hi)` range. A `Replace`'s delete+insert is two Yrs ops but one logical op (one row). Producer → op-kind mapping:

| Producer call | Op kind(s) |
| ------------- | ---------- |
| `edit_note([e1, e2, …])` | one `Replace` per edit, all sharing a `batch_id`; accept/reject each independently |
| `write_note` (whole body) | one `Replace` spanning the whole body region of `text` |
| `set_frontmatter` / `apply_tag` / `remove_tag` | one `SetFrontmatter` (a `Replace` inside the frontmatter fence) |
| `move_note` / rename | one `Rename { from }` |
| create a new note | one `Create`, then the content `Replace` |
| delete a note | one `Tombstone` |

**`AnchorHint`** is the `Replace`'s `old_str` kept as `{ hash, preview }` (not the full text). Two consumers re-check it against `materialize(accepted)`: drift detection (per "Pending queue") and `mcp.md`'s `get_pending_proposal` anchor-status. Whole-body `Replace`, `SetFrontmatter`, and `Rename` carry no anchor.

**Projections.** The "History materialization" `ChangeOp` is the coarse user-facing projection of `op_kind`: `Create → Created`, `Replace` / `SetFrontmatter → Modified`, `Tombstone → Deleted`, `Rename → Renamed` (with `rename_from` from the side-table column). `mcp.md`'s pending-op introspection reads `op_kind` (and `anchor`) directly off the `PendingOp`.


## Multi-file reorganization

A reorganization proposal — the cluster-apply / `core::suggest::stage_moves` flow — is a **batch of independent `Rename` ops**, one `Rename { from }` per moved note, sharing a cross-document `batch_id`. The batch is a review/display grouping, *not* a transaction: each `Rename` applies on its own, and partial apply is allowed (one move hitting a target collision doesn't block the rest). [op-log-reorg-batch]

The ops stage as `pending` with `author = auto:<producer>` (e.g. `auto:cluster`, `auto:triage` per "Author classes") and ride the same machinery as agent note edits — the review surface lists the batch; `flip_op_status` accepts or rejects it. Accept-batch applies each `Rename`, skipping any that fail; reject-batch drops the batch from the queue. This is the only place a `batch_id` spans documents — note-edit batches (`edit_note`) stay within one document.


## Storage layout

```
vault/.hiker/oplog/
  <doc-id>.yrs               # Yrs Doc base snapshot (binary, Yrs v2 full-state update)
  <doc-id>.yrslog            # append-only log of Yrs v2 deltas since the base
  <doc-id>.pending           # bincode-serialized Vec<PendingOp>
  <doc-id>.ops               # append-only accepted-op history (zstd keyframe + delta content frames)
  oplog_meta.db              # SQLite side table (one for the whole vault)
  doc-index.db               # SQLite: path → doc_id mapping
```

[op-log-store-layout]

- **`<doc-id>.yrs`** is the Yrs Doc's base snapshot — `Doc::encode_state_as_update_v2(&Default::default())` (full state), written atomically (temp + rename + fsync) on document create and on compaction only.
- **`<doc-id>.yrslog`** is an append-only log of the deltas since the base: each commit appends one length-prefixed `encode_state_as_update_v2(&persisted_state_vector)` frame and advances the in-memory `persisted_state_vector`. A save is therefore **O(edit), not O(doc size)** — the full base is never rewritten on a normal commit. Load applies the base then replays the log (`Doc::apply_update`, which is idempotent). A crash mid-append leaves a torn trailing frame that load stops at; the `.md` stays canonical so the dropped delta reconciles as an external edit. Sync delivers incremental `update_v2` payloads applied the same way. [op-log-yrs-delta-log]
- **`<doc-id>.pending`** is appended on agent op submit, rewritten on accept/reject. Pending ops persist here across restarts; reopening a document reconstitutes them. [op-log-pending-survives-restart]
- **`oplog_meta.db`** is one vault-wide SQLite database (WAL mode, `synchronous=NORMAL`).
- **`doc-index.db`** maps vault-relative paths to `doc_id` for path-based lookup. Regenerable by scanning Yrs Docs' `meta.path` field — losing it triggers a full rescan.

**Compaction.** The `.yrslog` grows one delta per commit, and Yrs state itself accumulates history (tombstones, position metadata). Schedule: on vault open, if the combined `<doc-id>.yrs` + `<doc-id>.yrslog` size exceeds 4× the materialized content size, fold the log into a fresh full base — write `<doc-id>.yrs = encode_state_as_update_v2(&Default::default())` of the replayed doc (atomic), then delete `<doc-id>.yrslog`. Base-first / log-second is crash-safe: a crash between leaves the now-redundant log, which replays idempotently onto the already-folded base. Side table's `status='rejected'` rows GC per `[op-log] rejected_retention_days`. [op-log-compaction]


## Bootstrap

Paths are the key in the vault; the op log keys on `doc_id`. The first open seeds the mapping and the substrate from the on-disk notes: [op-log-doc-id-bootstrap]

1. Walk the vault. For each existing `.md` note (and each sidecar), mint a ULID `doc_id` and write the `path → doc_id` row into `doc-index.db`.
2. Create the note's Yrs Doc: one `Create` op, then a `Replace` inserting the file's current on-disk bytes as the initial `text` state. Set `meta.kind` and `meta.path`. Author the seed ops `user` — the existing file is the user's accepted state.
3. Persist `<doc-id>.yrs` and the `op_metadata` rows. The on-disk `.md` is already equal to `materialize(accepted)` by construction, so no rewrite happens.

The seed is idempotent: a path already mapped in `doc-index.db` is skipped, so subsequent opens are a cheap walk. Seeding only mints docs for *new* paths — an already-mapped file whose disk bytes drifted (or which vanished) while hiker was closed is handled by the startup disk-reconcile pass (per "External-edit sync"), not here.

**Op-log bootstraps before the indexer.** `index.db` (search/embeddings, per `index.md`) is a separate, independently-regenerable store, but it shares the ULID space: when the indexer upserts a note, it reads `doc_id` from `doc-index.db` and uses it as `notes.id`. The indexer never mints document ids of its own. This is what enforces "one ULID per document" across the whole system. [op-log-bootstraps-first]


## Materialization

`materialize(doc) -> Materialized` returns `{ text, tombstone }` — `text` is `text: Y.Text` as a string (verbatim file bytes), `tombstone` is `meta.tombstone` (default false). [op-log-materialization]

Pure read over the Yrs Doc; `text` is the file's bytes verbatim, no parse/re-emit. Drives every diff render, save-to-disk, accept dry-run. The buffer renders from `materialize(accepted + working + pending(session))`; the canonical disk file equals `materialize(accepted)`. Because materialization is the identity over the stored text, `op-log-disk-canonical` holds byte-for-byte: opening and saving a note never rewrites a single character the user didn't change.


## History materialization

The op log is the changelog: it answers "who/what/when" (the side table) *and*
"what did this document look like then" (historical content).
[op-log-history-materialization]

- **Accepted-op retention.** Every accepted op appends a frame to a per-doc
  history log `<doc-id>.ops` (length-prefixed bincode), holding the op id, the
  tombstone flag, and the *materialized content* as of that op — stored as a
  **keyframe** (full text, zstd-compressed) or a **delta** (zstd-compressed
  against the *previous* frame's text as a dictionary, so an incremental edit
  costs roughly its own size). A keyframe is written for the first frame of a
  doc, on a tombstone, every `KEYFRAME_INTERVAL` (16) frames, and on the first
  write after a (re)open; the frames between are deltas. Storing content history
  (not full Yrs Doc state, which would carry the doc's *entire* op history in
  every frame) keeps the log linear in content, and delta-packing cuts it toward
  linear in *edit size*. Appended on create, user edit, agent-op accept,
  external edit, rename, and tombstone. [op-log-accepted-op-retention]
- **`materialize_at(doc_id, op_id)`** reconstructs an op's text by decoding from
  the nearest preceding keyframe forward (each delta decompressed with the
  running text as its dictionary) — never touching the live `accepted` Doc or
  decoding Yrs on the read path; `KEYFRAME_INTERVAL` bounds the walk. Returns
  `{ text, tombstone }` (so a point past a tombstone reconstructs the deleted
  state), or `None` when no frame matches (unknown op, pre-retention, or a
  lifecycle marker like the bare `Create` of a non-empty note, whose content
  frame is keyed to the content op).
- **History listing.** `doc_history(doc_id, limit)` / `vault_history(limit)`
  project the side table (`status = Accepted`, newest-first) — the version
  dropdown, per-file history, and recent-activity feed read these plus
  `materialize_at` for content. Producer-facing seams: `path_history`
  (version list), `content_at_op` (content of a version), and
  `previous_accepted_content` (the version before the latest, for "restore
  previous") in `core::ops::op_writes`.

A torn trailing `.ops` frame from a crash mid-append is tolerated — the reader
stops at the first short/undecodable frame, and `.yrs` stays canonical for
*current* state. A delta depends on its preceding frames, so a torn frame also
truncates the deltas after it — bounded by the keyframe cadence (the next
keyframe re-anchors); current state is never at risk.

Deferred: (1) **coalescing** — a frame is currently minted per accepted op,
including each autosave-driven commit; debouncing a burst of saves into one
history frame would mint far fewer. (2) a **retention bound** (drop frames older
than N days / keep last K), dropping whole keyframe→next-keyframe spans so no
delta is orphaned. [op-log-history-retention]


### Change-row projection

`core::activity` is the user-facing projection layer over the side table. It
returns the `ChangeRow` DTO — never raw op records — to the home-page
recent-activity widget, the per-file version dropdown, the activity-detail
page, and the author-attribution queries. [changes-query-api]

```rust
pub struct ChangeRow {
    pub id: i64,                       // = timestamp_ms; the stable handle is op_id (in metadata)
    pub timestamp_ms: i64,
    pub path: String,                  // resolved path-as-of-this-op
    pub op: ChangeOp,                  // coarse projection of op_kind
    pub author: String,
    pub surface: Option<String>,
    pub content_hash: Option<String>,
    pub rename_from: Option<String>,
    pub metadata: serde_json::Value,   // carries op_id + doc_id
    pub is_current: bool,
    pub author_class: AuthorClass,
}

pub enum ChangeOp { Created, Modified, Deleted, Renamed }
```

`AcceptedFeed` (`recent` / `recent_by_author` / `history_for_path`) projects
`status = Accepted` rows newest-first and stamps `is_current` on the newest op
per path in the window. The ulid `op_id` and `doc_id` ride in `metadata` (the
`id: i64` field holds `timestamp_ms`) so content / rollback consumers recover
the real handle. Content as-of an op comes from `content_at_op` (above), not
from a per-row blob.

The `author` field records who *authored* the change, not who accepted it: a
user accepting a staged agent proposal leaves `author = "agent:<client-id>"`.
Automation writes carry `author = "auto:<producer>"`. The class prefix supports
wildcard (`author LIKE 'agent:%'`) and exact (`author = 'agent:claude-code'`)
queries; `AuthorClass` surfaces the class half typed for filter pills.


### Unified activity feed

The activity-detail page, the editor status-bar version dropdown, and the
queue-bar pending count consume one merged feed: each op surfaces as one item,
with `status` distinguishing accepted history from pending proposals. The merge
happens in `core::activity`; consumers don't reconcile two lists.
[activity-feed-merged]

- **One `Item` per op**, tagged with `status`: accepted items wrap a
  `ChangeRow`, pending items wrap a `PendingItem` carrying producer metadata
  (`surface`, `action`, `target_path`, `session_id`, `content_hash`).
  [activity-feed-unified-item, activity-feed-staging-metadata]
- **Source filter is a first-class arg.** `Source::{ChangesOnly, PendingOnly,
  Merged}` keeps the home page accepted-only by default; the detail page flips
  to include pending. [activity-feed-source-filter]
- **Ordering is single-key.** Items sort by `timestamp_ms desc` with the op id
  as the deterministic tiebreaker. [activity-feed-merge-ordering]


### Rollback

"Undo this change" and "restore this version" both produce a *fresh* op against
the document — the op being undone stays in the log with its original status, so
the audit trail is append-only. [changes-rollback-helper]

- **Restore-this-version** reads `content_at_op` for the chosen op and writes it
  back via `user_save`, which materializes the document to that content as a new
  accepted `user` op.
- **Restore-previous** reads `previous_accepted_content` (the version before the
  latest accepted op) and writes it back the same way.

Both compose with arbitrary chains; neither mutates prior history.


## Disk write invariant

Any commit into `accepted` — Save folding the `working` layer in (`commit_working`), or accept folding a `pending` op in — runs in this order, all under one lock hold: [op-log-atomic-write]

1. Apply the committing ops to the `accepted` Doc via `Doc::transact_mut`.
2. Persist the updated Yrs state to `<doc-id>.yrs` (write-temp-then-rename + fsync) — *before* the metadata row, so a crash can't leave a row pointing at unpersisted state.
3. Persist the `op_metadata` row(s) and append the history frame.
4. Compute `materialize(accepted)` and atomically write the `.md` to disk (write-temp-then-rename + fsync).

If the process crashes between (2) and (4), the next open re-runs (4) — Yrs is the source of truth, the `.md` is its projection. If the process crashes during (2), the partial write is detected (Yrs's update format has integrity checks); the previous good state is preserved.

Uncommitted `working` edits and `pending` ops don't trigger steps (1)-(4): `working` lives in memory (crash-recovered from the autosave sidecar per `autosave.md`), and `pending` lives in `<doc-id>.pending`, until saved or accepted respectively.


## External-edit sync

The `.md` on disk is a projection of `materialize(accepted)`, but the disk is also a write surface other tools own (Syncthing receive, a manual edit, a delete or rename in another editor). Any divergence between disk and `accepted` is folded back into the CRDT as one `author=external` op. [op-log-external-edit-sync]

**The fold.** Given a tracked doc and its current disk bytes:

1. Read the file's current bytes.
2. Compute `materialize(accepted)`.
3. If they match, it's a self-write echo — no-op (the `watcher-suppress-self-writes` machinery is the first line; this hash check is the safety net).
4. If they differ, diff `materialize(accepted)` → disk_bytes and apply the text delta to `accepted`'s `text: Y.Text` in a transaction tagged `author=external`. Frontmatter and body share one `Y.Text`, so the whole reconciliation is a single text diff.

The fold is **hash-gated, not mtime-gated**: a file touched but byte-identical to `materialize(accepted)` produces no op and no rewrite, so `op-log-disk-canonical` holds and an idle reopen mints nothing.

### Three triggers

The same fold runs from three places:

| Trigger | When | Scope |
| ------- | ---- | ----- |
| Live (per `watcher.md`) | a watcher event for a tracked path while hiker runs | one doc |
| Startup reconcile | vault open, before bootstrap seeds new paths | every tracked doc [op-log-startup-disk-reconcile] |
| Open-time reconcile | a buffer opens | that one doc, before its text loads [op-log-open-time-disk-reconcile] |

The watcher only reports changes that happen *while it is watching* — it never replays edits made while hiker was closed. The **startup reconcile** closes that gap: it walks the tracked docs (an mtime/size pre-filter skips rehashing files that plainly didn't change; the commit decision is still the byte hash), and folds any drift. The **open-time reconcile** is the per-doc backstop for anything the watcher dropped in-session (a suppressed-write window, a notify-queue overflow) — the buffer's text loads from the now-reconciled `accepted`.

### Ordering invariant

At vault open the order is **reconcile, then bootstrap-seed, then the first sync round** — and that order is load-bearing in two ways. [op-log-startup-disk-reconcile]

- **Reconcile before bootstrap.** Bootstrap seeds an untracked path as a fresh document; reconcile must claim an offline rename's *new* path while it is still untracked, or the rename degrades into "tombstone the old doc, seed the new one as a fresh lineage" and the history is orphaned. Reconcile works off the prior session's persisted `doc-index.db` mapping, so it needs no seeding first; on a first-ever open the mapping is empty, reconcile is a no-op, and bootstrap seeds everything from disk.
- **Reconcile before sync.** A stale `accepted` is never pushed to a peer, and an offline disk edit is never overwritten by an inbound update that lands first. A reconcile-vs-inbound-sync race on the same doc resolves by reconcile-first ordering plus the standard three-way merge at the materialization layer.

At startup `working` is empty; a pending agent op re-anchors through the existing drift detection (per "Pending queue").

### Offline delete and rename

A tracked path *gone* from disk at reconcile time is an offline delete: tombstone the doc (`OpKind::Tombstone`, `author=external`) and route it through the trash (`delete-note-core-cmd` in `files.md`), with `materialize(accepted)` — the last known content — written as the recoverable trash artifact since the original bytes are gone. The doc's Yrs state and history (`<doc-id>.yrs`/`.yrslog`/`.ops` + metadata) are **retained, keyed by `doc_id`** rather than purged: the op-log is path-independent, so the history simply stays put and the trash entry references the `doc_id`. Restore (`vault-trash-restore`) rebinds `path → doc_id` and clears the tombstone, recovering content *and* full history — not a fresh import. Trash is where a tracked doc's history lives once its file is gone.

An offline rename (one tracked path gone, a new untracked path present) is recognized as the same lineage when the new path's content hash matches the gone doc's current or recent history — rebind `path → doc_id` and record an `OpKind::Rename { from }`, reusing the rename handling in "Document identity". No match → treat as delete-to-trash of the old path plus a fresh seed of the new one.

For sidecars, an external edit on the *source* (the PDF) triggers re-extraction, not a synthesized op on the sidecar — see "Re-extraction".

### Acceptance set

Integration tests over a real `OpLog` + temp vault (these are not retrieval-quality eval — see `qa.md` for that distinction):

1. Offline edit to a tracked file → reopen → `accepted` matches disk; the `.md` is byte-unchanged; `content_hash` advanced.
2. Offline edit, then a sync round → the edit propagates (proves reconcile-before-sync ordering).
3. No offline change → reopen → no-op (no op minted, no rewrite).
4. Touched-but-byte-identical file → no spurious commit (hash gate, not mtime).
5. New file added offline → still seeded by bootstrap (regression guard, unchanged behavior).
6. Offline edit with a pending agent op present → the disk edit folds in; the pending op drifts / re-anchors correctly.
7. Open-time path: a change the watcher missed in-session → opening the buffer reconciles it.
8. A reconcile op surfaces as `author=external` in the history / activity feed.
9. A torn trailing `.yrslog` frame plus an offline edit → still converges (crash-safety).
10. Large vault → unchanged docs are skipped by the pre-filter.
11. After reconcile + open → the editor shows the reconciled content and the buffer reports clean.
12. Offline delete → file in trash, Yrs history retained keyed by `doc_id`, doc tombstoned; restore recovers content and history.
13. Doc diverged on a peer *and* on disk → the offline disk edit (folded `author=external` before the round, per the ordering invariant) and the inbound peer edit merge as a Yrs three-way: disjoint edits both survive, both devices converge, and the disk edit propagates back to the peer.
14. A present-but-unreadable file (non-UTF-8, a permission error) → skipped, NOT mistaken for a delete; and one un-reconcilable doc doesn't abort the pass (best-effort per doc).
15. A tombstoned doc whose file reappears on disk → resurrected (tombstone cleared, current bytes folded, same `doc_id`) rather than left a ghost.
16. Three (or more) peers editing the *same line* concurrently → every replica converges to one deterministic text regardless of delivery order, and no peer's edit is lost.
17. Offline delete vs a concurrent peer edit → both replicas converge tombstoned (the delete wins; a concurrent edit doesn't silently resurrect it).

### Limitations

- **Same-region concurrent edits can't merge *semantically*** — no automatic merge of two contended prose edits recovers intent. Rather than silently interleaving, a same-region sync conflict Blocks the document for user resolution (`sync.md` `sync-conflict-detect-same-region`); disjoint-region edits still merge automatically.
- **Rename + heavy edit isn't recognized as a rename.** A file renamed *and* substantially edited while closed no longer content-hash-matches the gone doc's history, so it degrades to delete-old (to trash, recoverable) + seed-new (fresh lineage). No data is lost; the lineage splits. Same limitation as the sync layer's rename detection.
- **Live double-write race.** While hiker runs, a peer update writing the `.md` (a self-write the watcher suppresses) can race an external editor writing the same file; reconciliation then rests on self-write suppression + last-writer-wins at the materialization layer. The offline (startup) path is clean; this in-session corner is not specifically serialized.


## Author classes

The `author` field is recorded in `op_metadata` for every Yrs operation range hiker authors. Vocabulary: [op-log-author-classes]

- `user` — keystroke / save / direct UI action.
- `agent:<client-id>` — an MCP-attached agent's tool call. `<client-id>` from MCP handshake.
- `external` — file on disk changed outside hiker; reconciled via the external-edit-sync path.
- `extractor:<producer>` — a source-derived note was re-extracted / re-imported by an external producer.
- `auto:<producer>` — write from internal automation (`auto:triage` per `cluster-editor.md`); the producer is the author whether the write was unattended or user-reviewed (`metadata.auto_accepted` distinguishes them).
- `sync:<device-id>` — Yrs operations received from another device via the sync transport.

Class prefix supports wildcard (`author LIKE 'agent:%'`) and exact (`author = 'agent:claude-code'`) queries.


## Status states

Three states, tracked across the side table + pending queue: [op-log-status-states]

- **`accepted`** — the Yrs op is in `accepted`'s state; the side-table row's `status = 'accepted'`. Counted in `materialize(accepted)` and reaches disk.
- **`pending`** — the op is in `<doc-id>.pending` as a serialized update; *not yet* applied to `accepted`. Visible only as the producing session's `pending` overlay in the merged buffer view. No side-table row exists yet (the op has no Yrs client_id range until it lands in `accepted`).
- **`rejected`** — the op was previously pending, the user said no; an audit row is written to `op_metadata` with `status = 'rejected'` and the serialized update bytes stashed in the row's metadata for "what did the agent suggest that I said no to?" queries. The op never enters `accepted`.

GC removes `rejected` rows after `[op-log] rejected_retention_days`. Pending entries never auto-GC — they sit in the queue until the user resolves them.


## Hunk view

```rust
let base    = materialize(accepted + working).text;          // the user's current view
let current = materialize(accepted + working + pending(active_session)).text;
let layer   = DiffLayer { base, current, owner: DiffOwner::Agent };
```

[op-log-hunk-view]

Per-hunk accept queries the pending queue for ops whose `yrs_update` overlaps the hunk's `current_range` (resolved by applying the update to a clone and checking the affected position range). Applies those updates to `accepted`, removes them from `<doc-id>.pending`, writes side-table rows with `status='accepted'`. [op-log-per-hunk-accept-reject]

Per-hunk reject does the same lookup but writes side-table rows with `status='rejected'` instead of applying to `accepted`.

Per-op flip (sub-hunk granularity) is the underlying primitive; expose as UI later if real workflows need it. [op-log-per-op-status-flip]

**User's own typing** lands in the `working` layer as `user` ops (per "Editor binding"), not in `accepted` — so it stays uncommitted until Save. Because `base` already includes `working`, the user's edits are on both sides of the review diff and produce no hunks against themselves; only the agent's `pending` ops do. Save folds `working` into `accepted` (→ disk).


## Whole-file and structured proposals

Some agent operation shapes don't compose into the per-hunk view:

- **Whole-body rewrite** — `write_note` MCP calls produce a Yrs update that clears the body region and inserts new content. Reviewed via the whole-file review surface (`patch-review.md` `write-note-review-surface`), not per-hunk.
- **`SetFrontmatter` patches** — produce a `Replace` inside the frontmatter fence. Reviewed as ordinary text-diff hunks over the frontmatter lines, the same as any body edit; there is no structured key-by-key rendering (frontmatter is plain text).
- **`Create` / `Tombstone` / `Rename`** — top-level lifecycle operations; reviewed as confirm-style cards.

Yrs handles all of these uniformly as updates against the Doc; the review surface picks the right rendering per the op's `op_kind` (per "Op shapes", `op-log-op-shape`).


## Re-extraction

Sidecar documents whose source has changed (or whose extractor version has bumped) need a controlled way to re-pull extracted content. User picks the policy per re-run: [op-log-reextract-replace]

| Policy | Behavior |
| ------ | -------- |
| **Replace** | Apply a Yrs update to `accepted` that replaces the body region of `text` (everything after the frontmatter fence) with the new extraction. `author = extractor:<id>`, `status = accepted`. The update targets only the body byte range, so frontmatter is left untouched; concurrent user edits elsewhere merge via Yrs's text CRDT. |
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

- `core::oplog` — owns `<doc-id>.yrs` files, `<doc-id>.pending` files, and `oplog_meta.db`. Exposes `OpLog::open`; the `working`-layer verbs `apply_working_edit` / `materialize_working` / `commit_working` / `discard_working` (per "Editor binding"); the pending verbs `stage_pending` / `accept_pending` / `reject_pending`; plus `materialize_accepted` and `query_metadata`. The Yrs dependency is confined to this crate; consumers see plain Rust types. [op-log-module]
- `core::ops` — wraps `OpLog` with the higher-level write paths (`write_file`, `agent_write_note`, `agent_edit_note`, `flip_op_status`) plus the history seams (`path_history`, `content_at_op`, `previous_accepted_content`) in `core::ops::op_writes`. Producers don't talk to `OpLog` directly.
- `core::activity` — the projection layer over `OpLog::query_metadata`: the `ChangeRow` DTO + `AcceptedFeed` (accepted-op history) and the merged `Activity` feed across accepted + pending. Pure projection over `OpLog`.
- `core::embed` — embedding sync layer; content-addressed blob store separate from `core::oplog`.
- `app` — the editor pane runs the editor binding (per "Editor binding"): it feeds the editor's change sets to `apply_working_edit` (forward) and renders `materialize_working` back into the editor (reverse), origin-tagged. Save calls `commit_working`; per-hunk accept/reject route through `core::ops::flip_op_status`.


## `[op-log]` config section

[op-log-config-section]

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
| `auto_reject_on_drift` | bool | `false` | user + vault | Auto-reject a pending op when it drifts against current `accepted`. |
| `review_required` | bool | `true` | user + vault | Default `status` for agent-authored ops. Surface-specific overrides (`[mcp.tools].review_required`, `[llm.background].review_required`) apply. |
| `compact_threshold` | f32 | `4.0` | user + vault | Yrs Doc size multiple over materialized size that triggers compaction on vault open. |


## Sync substrate

Multi-device sync is the goal this substrate is built for. The sync transport (specced in `sync.md`) ships Yrs updates between replicas: [op-log-multi-device-sync]

- **Yrs's native update protocol.** `Doc::encode_state_as_update_v2(&peer_state_vector)` returns "ops since this watermark." Apply via `Doc::apply_update`. This is the standard Yjs sync pattern; battle-tested at Google-Docs scale.
- **Per-document streams.** Each `<doc-id>.yrs` syncs independently. State vectors per (doc_id, peer_device_id) tracked in a per-device sync watermark table.
- **Embedding sync rides alongside** but separate — content-addressed blob diff (see "Embedding sync" above).
- **Cluster trees sync as regular markdown documents.** Trees are per-tree `.md` files under `vault/.hiker/trees/` (per `cluster-editor.md`); their structure lives in frontmatter and every edit is a `SetFrontmatter` op, so they sync through this substrate exactly like any other markdown document.
- **Editorial metadata syncs as a CRDT-merged side stream.** `op_metadata` rows whose `(yrs_client_id, yrs_clock_lo, yrs_clock_hi)` range matches an op already in the synced Doc apply; rows whose range doesn't match yet wait. Concurrent metadata writes to the same op (e.g. two devices both reject) resolve LWW per `(op_id, timestamp_ms)`.

Sync transport, encryption, conflict copies, key management — the remaining sync implementation work. This doc specs the *substrate* the sync layer rides on.


## Out of scope

- **Sync transport design.** Lives in `sync.md`; this doc specs the substrate it rides on.
- **External extraction / import tooling.** Re-extraction versioning rides the op-log (an `extractor`-authored op), but the external producer / importer itself is out of this spec.
- **Cross-document atomic *transactions*.** Each Yrs Doc is independent; a multi-file refactor is N independent op streams with no all-or-nothing guarantee (partial apply is allowed). A `batch_id` *may* span documents to group a reorganization for review (per "Multi-file reorganization", `op-log-reorg-batch`) — but that grouping is display-only, not a transaction.
- **Encryption at rest.** Orthogonal to the log shape.
- **Per-character review UI.** `op-log-per-op-status-flip` lands the primitive; surfacing as UI is deferred.


## Deferred

- `op-log-reextract-merge-deferred` — pending-op-shaped re-extraction policy.
- `op-log-reextract-diff-prompt-deferred` — interactive re-extraction review.
- `op-log-tags-set-union-deferred` — if cross-device sync makes concurrent same-line `tags:` edits a real problem, promote *only* `tags` to a `Y.Array` for set-union merge; everything else stays plain text inside `text: Y.Text`. Localizes the one capability the plaintext model gives up.


## Forward refs

- `patch-review.md` — per-hunk agent-edit review surface, built on the layered model and the editor binding.
- `diff.md` — `DiffLayer` primitive, unchanged.
- `design.md` "Source-derived notes" — sidecar architecture this composes with.
- `sync.md` — the sync transport / enrollment / server layer this substrate enables.
- `mcp.md` — agent tool calls produce pending Yrs updates with `author=agent:<client-id>`.
- `cluster-editor.md` — triage auto-accepts ride `author=auto:triage`.
- `settings.md` — the `[op-log]` config section above.
- `cluster-editor.md` — cluster trees are per-tree `.md` files riding this substrate like any other markdown document; tree edits are `SetFrontmatter` ops on the tree doc.
