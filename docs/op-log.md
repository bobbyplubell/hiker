# Op log

The local editing & history substrate every write rides on. A document is a plain `.md` file on disk (canonical); its identity is its vault-relative path. Editing is a plain text buffer — no CRDT. History is per-save **text frames** in a per-document `.ops` file (`materialize_at` reconstructs any version). Sync is a separate, pluggable concern (`sync.md`) that ships *files*, not ops — this doc owns the local substrate it rides on.

Orientation: there is no CRDT and no op-log database. Three views of one file compose at edit time — `accepted` (the last committed `.md`, on disk), `working` (the editor buffer, uncommitted), and `pending(session)` (an agent session's proposed edits, anchored against `accepted`). Saving commits `working` into `accepted` and appends a history frame; accepting folds a `pending` hunk into `working`. Authorship rides the history frame as an author class. Each behavior is specced below.

## The headline decisions:

- **Editing is plain text + editor-native anchored ranges** — no CRDT, no fuzzy patch matching. The buffer is the editor's rope; the user's edits land at plain byte offsets. Agent edits are anchored ranges (position + replacement vs `accepted`) remapped through the user's edits via the editor's own `ChangeSet`/`map_pos` — exact position tracking, not context-fuzzy. [op-log-layered-model]
- **Identity is the vault path** — no stable doc-id, no path↔id table. History follows content across renames via an observed content-preserving move (below). [op-log-path-identity]
- **Agent edits are an anchored-range overlay, accepted per-hunk** — proposed as a diff vs `accepted`, rendered inline, folded into the buffer on accept; rejected by dropping the range. [op-log-pending-patch]
- **History is per-save text frames** — a per-document `.ops` file (zstd keyframe + delta); `materialize_at` reconstructs any version. Durable, local, and sync-independent; synced-in changes append frames too, so they show in history. [op-log-history-materialization]
- **Authorship rides the history frame** — each frame carries an `Author` class (`user` / `agent:<id>` / `external` / …); the activity feed is a projection over the `.ops` frames. No metadata database. In git-transport mode the same class also rides a commit trailer (`git.md`). [op-log-attribution]
- **The only database is the search index** (`index.db`, per `index.md`), and it is fully regenerable; the durable state is the `.md` files plus the `.ops` history and the un-accepted `.pending` edits. [op-log-no-oplog-db]


## Document identity

A document is a markdown file; its identity is its vault-relative path. [op-log-path-identity]

| Source location | Source type | Document is | Identity |
| --------------- | ----------- | ----------- | -------- |
| Vault-internal | markdown | the `.md` file itself | its vault path |
| Vault-internal | non-md | the sidecar `<src>.md` | sidecar's vault path |

No stable id, no `path → id` table. The per-document `.ops` history file is keyed by path and moves with the document on a rename, so history survives a rename without any internal identifier. A rename is an observed content-preserving move (see "Renames" below); wikilinks rewrite by path (`wikilinks.md`). [op-log-rename-as-move]

Sidecars decouple user-edited content (the `<src>.md`) from a binary source (PDF, image, audio); the sidecar is the tracked document, the source is not (`design.md` "Source-derived notes"). [op-log-sidecar-document]

External-source pointer documents (a watched file outside the vault) are deferred under the path-as-identity model — the prior `<id>--slug.md` + `source_ref` scheme is not reworked here. [op-log-external-handle-deferred]

### Renames

Path is identity, so a rename moves the document's path *and* its `.ops` history file together; history follows with certainty when hiker **observes** the move. A move hiker performs (the file tree, a `move_note` call) is recorded as: [op-log-observed-move]

1. **The content-preserving move** — the `.md` and its `.ops` file relabel to the new path; the bytes are byte-identical to the old path's `accepted`. A history frame records the move with `Hiker-Rename: <from> -> <to>` so the activity feed has an authoritative record.
2. **An edit frame** — only if the content also changed: a normal modify frame at the new path, after the move.

**The fallback is content similarity** for a move hiker cannot observe *as* a move — a rename + heavy rewrite performed outside hiker while it was closed, where the startup reconcile sees only "old path gone, a very different new path present." If similarity can't bridge it, history splits at that point — content is never lost, only the lineage link breaks. This is the irreducible case for any system without a stored id. (In git-transport mode the move is also a content-preserving rename commit so `git log --follow` matches it; see `git.md`.) [op-log-rename-follow-heuristic]


## Layered document model

Three views of one file compose at edit time: [op-log-layered-model]

- **`accepted`** — the last committed `.md`, on disk, the history/sync baseline. Holds every change authorized to reach the repo — saved user edits, reconciled external edits, accepted agent edits, accepted re-extractions, merged sync receives. [op-log-accepted-base]
- **`working`** — the editor buffer (the editor's rope): the user's uncommitted edits over `accepted`. Local to this device until Save; equals `accepted` when the buffer is clean. Save commits `working`. [op-log-working-layer]
- **`pending(session)`** — a per-agent-session set of proposed edits, anchored against `accepted`, staged for review and not committed. Accept folds a hunk into `working`; reject drops it. [op-log-pending-patch]

The editable buffer is `accepted + working` — the user's own text, so typing lands at plain byte offsets with no coordinate translation. The agent's pending edits render *on top* as the inline review overlay — the diff toward `accepted + working + pending(session)` (per `patch-review.md`) — so the user sees proposals in place while editing their own regions. The overlay's anchors are remapped through the user's edits via the editor's `ChangeSet`/`map_pos` (exact, not fuzzy); disjoint edits compose by position; accept/reject rebases the overlay without disturbing `working`, so the user is never forced to switch between "my text" and "the review view". [op-log-three-way-overlay]

Why `working` is its own layer rather than committing keystrokes straight to `accepted`: unsaved work stays a real, mergeable edit — an agent edit elsewhere neither displaces it nor is displaced by it — while disk writes stay gated behind an explicit Save. Why `pending` stays separate rather than merging into `working`: proposals are reviewed per-hunk and never sync — staging is per-device, not collaborative. More than one agent session = more than one `pending(session)` overlay; the file pill swaps the active one (`patch-review.md`).


## Agent edits

The agent perceives a coherent, live document regardless of accept status. There is not one document with a "pending" flag; there are multiple materialized views of one base, one per perspective: [op-log-agent-session-view]

| view | = | who sees it |
| ---- | - | ----------- |
| disk / accepted | the committed `.md` | canonical baseline |
| user / working | accepted + the user's uncommitted edits | the user's editing buffer |
| agent / session | accepted + *this session's* edits | the agent's reads and writes |

The agent reads and writes against its **session view**, which always reflects its own edits; "pending" is purely how those edits appear in the user's review view, invisible to the agent. The agent never blocks on acceptance — user accept/reject happens asynchronously, possibly after the agent's turn ends. The mental model is a per-session branch: the agent's reads see its branch; the user sees the base plus a reviewable diff of the branch.

Mechanism — session text + derived hunks:

- The session holds a working **session text** (a plain `String`) — its materialized view, `accepted` + this session's edits. Each agent edit splices the byte range in memory immediately, so reads-after-writes are consistent and dependent edits compose (each reads the real result of the prior).
- The **pending hunks shown to the user are derived on demand** by diffing `accepted` against the session text (`editor_core::diff`). The hunks are a view for the user, not what the agent operates on.
- Persisted to `.hiker/pending/<session>/<path>.pending` (gitignored, inspectable text — the session text plus its anchor metadata) so it survives restart until accepted or rejected. [op-log-pending-survives-restart]
- Session-scoped: each session is its own text; concurrent sessions don't see each other's uncommitted work.

Why a plain `String`, not the editor's rope: the agent path is a coarse, infrequent, headless mutator (LLM-tool-call cadence, KB-sized docs, localized edits) — a splice is a sub-µs memmove dwarfed by the model round-trip, and the diff operates natively on `&str`. The rope/editor stack carries cursor/viewport/selection/decoration/undo machinery the agent has none of. Streaming edits fall out naturally: mutate the `String` (debounced) as tokens arrive, re-derive the hunks for display.


## Merge and conflicts

`working` and `pending` hunks merge by position, in memory:

- **Disjoint regions** — the user edits one part, the agent another: the merge is automatic, both render in the buffer, no prompt. [op-log-merge-auto]
- **Overlapping region** — both change the same span: hiker does not silently interleave them. The overlap surfaces as a conflict hunk in the inline review with per-hunk **Keep mine** / **Keep theirs** / **Keep both**, routed through the one unified conflict surface (`sync.md` "Conflicts"). [op-log-merge-conflict]

A conflict on a contended region is the desired behavior, not a regression: for notes you want a conflict there, not a character-level interleave.


## Drift

When `accepted` advances (user typing, external edit, sync receive, an earlier pending hunk accepted), a queued pending hunk may no longer apply — the base context its anchor expects no longer matches. Drift is a re-diff check: re-derive the hunks against current `accepted` and verify the hunk's base context still matches exactly; a mismatch means the hunk is *drifted*. Surface in the file pill's `(M drifted)` count; Accept disabled, Reject active. `auto_reject_on_drift` drops drifted hunks automatically. [op-log-drift]


## Per-hunk accept / reject

The review surface diffs the user view against the agent view: [op-log-hunk-view]

```rust
let base    = materialize(accepted + working);                    // the user's current view
let current = materialize(accepted + working + pending(session)); // with the agent's proposal
let layer   = DiffLayer { base, current, owner: DiffOwner::Agent };
```

Per-hunk **accept** applies the hunk into the `working` buffer: build a `ChangeSet` for the hunk's base range → replacement text and apply it as a `Transaction` against the buffer (the `git add -p` data model); thereafter it is a normal working edit, committed on Save. Per-hunk **reject** drops the hunk from the pending set. Because `base` already includes `working`, the user's own typing is on both sides of the review diff and produces no hunks against itself — only the agent's pending hunks do. [op-log-per-hunk-accept-reject]

Whole-file and structured proposals that don't compose into per-hunk view (a `write_note` whole-body rewrite, a `Create`/`Tombstone`/`Rename` lifecycle op) are reviewed via the whole-file / confirm-card surfaces in `patch-review.md`.


## Storage layout

```
vault/
  *.md                               # canonical content (accepted)
  .hiker/                            # local hiker state
    ops/<path>.ops                   # per-document history frames (zstd keyframe + delta) — DURABLE
    pending/<session>/<path>.pending # un-accepted agent edits (inspectable text) — DURABLE
    index.db                         # search / vector index (index.md) — the only db, REGENERABLE
    autosave/                        # crash-recovery sidecars (autosave.md)
    embeddings/                      # content-addressed embedding cache (below)
    config.toml                      # per-vault config (settings.md)
```

[op-log-store-layout]

Two durability classes under `.hiker/`. **Durable** (not reconstructible from the `.md` alone): the `.ops` history (past versions) and the un-accepted `.pending` edits. **Regenerable**: `index.db` rebuilds from the notes, autosave is transient crash-recovery, the embedding cache re-derives. So `rm -rf .hiker/` loses nothing canonical except the `.ops` version history and any un-accepted `.pending` edits — the current `.md` content is untouched. [op-log-no-oplog-db]

`.hiker/` is gitignored in git-transport mode (`git.md`) — git tracks only the user's markdown and attachments, and supplies its own commit-graph history in parallel. The cutover starts fresh; there is no migration of any prior on-disk layout (pre-1.0, no live vaults).


## Attribution

Each history frame carries an author class, so "who changed this, and how" is answerable without a side database: [op-log-attribution]

```rust
pub enum Author {
    User,                 // "user"
    Agent(String),        // "agent:<client-id>"
    External,             // "external"
    Extractor(String),    // "extractor:<producer>"
    Auto(String),         // "auto:<producer>"
    Sync(String),         // "sync:<device-id>"
}
```

The logical shape for the activity feed (Created / Modified / Renamed / Deleted) is derived from the frame's effect on the document (first frame / modify / move / tombstone) plus the author class — no separate op-kind store. The activity feed is a projection of the `.ops` frames. In git-transport mode the same class is mirrored onto a `Hiker-Author` commit trailer so the git history is self-describing (`git.md`). [op-log-status-states]

### Author classes

`Author::as_wire()` renders `class[:identifier]`; `Author::parse` round-trips it. [op-log-author-classes]

- `user` — keystroke / save / direct UI action.
- `agent:<client-id>` — an MCP-attached agent's tool call; `<client-id>` from the MCP handshake.
- `external` — file changed on disk outside hiker; reconciled via external-edit reconcile.
- `extractor:<producer>` — a source-derived note re-extracted / re-imported.
- `auto:<producer>` — internal automation (`auto:triage` per `cluster-editor.md`); `metadata` distinguishes unattended vs user-reviewed.
- `sync:<device-id>` — a frame received from another device.

The class prefix supports wildcard (`agent:*`) and exact (`agent:claude-code`) queries; it records who *authored* the change, not who accepted it (a user accepting an agent proposal leaves `agent:<id>`).


## Save policy

Saves are driven programmatically; the user never types a storage command. [op-log-save-policy]

- **Commit on Save** — Ctrl/Cmd-S → write the `.md` → append a history frame. Debounced / idle-coalesced so a burst of saves does not mint a frame per keystroke-burst. An agent-accept can be its own frame. After the frame is written, the sync transport (if any) is poked (`sync.md`).
- History is per-save, not per-keystroke — desired (sub-save granularity stays ephemeral / in-session).
- Frames keyframe periodically and delta-compress between keyframes (below); a periodic GC trims old delta chains where configured.


## Disk write invariant

A commit into `accepted` — Save folding `working` in (`commit_working`), or accept folding a pending hunk in — writes the `.md` atomically (temp + rename + fsync), then appends the history frame with its `Author` class. [op-log-atomic-write]

The `.md` is canonical. A crash after the file write but before the frame append leaves an uncommitted working-tree edit, reconciled as an external edit on next open — nothing lost. Uncommitted `working` edits live in memory (crash-recovered from the autosave sidecar, `autosave.md`); pending edits live in `.hiker/pending/`.


## Materialization

`materialize(accepted)` is the current `.md`; the editable buffer is `accepted + working + pending(session)` composed in memory. There is no parse / re-emit step — the bytes the user wrote are the bytes committed and re-read, so opening and saving never rewrites a character the user didn't change. [op-log-materialization, op-log-disk-canonical]


## History

The `.ops` history is the changelog — it answers who/what/when and "what did this document look like then". A per-document `.ops` file stores **text frames**: a `RetainedOp::Full` keyframe (the whole materialized text, zstd-compressed) every N frames (and on tombstone / after reopen), and `RetainedOp::Delta` frames in between (zstd against the previous text). Reconstruction is linear from the nearest keyframe. [op-log-history-materialization]

- **History listing** — the `.ops` frames for a path (newest-first) drive the version dropdown, per-file history, and the recent-activity feed; the frame's author class supplies attribution. Rename-resilient: an observed move relabels the `.ops` file to the new path so history follows (see "Renames"); an unobserved offline rename+rewrite is best-effort.
- **Content at a version** — `OpLog::materialize_at(path, frame_id)` reconstructs the text at any frame. Delta frames keep the file compact; no whole-file copy per save.
- **Rollback** — "restore this version" reads `materialize_at` and writes it back as a new frame (forward-correct: the old content lands as a fresh edit, so it is sync-safe and the audit trail stays append-only). [changes-rollback-helper]

### Change-row projection

`core::activity` is the user-facing projection over the `.ops` frames. It returns the `ChangeRow` DTO — never raw frames — to the recent-activity widget, the per-file version dropdown, the activity-detail page, and author-attribution queries. The fast author/timestamp lookups it serves are backed by a **regenerable** query-index built from `.ops` and held in `index.db` (rebuilt by replaying the frames), not by a durable side table. [changes-query-api]

```rust
pub struct ChangeRow {
    pub frame_id: String,
    pub timestamp_ms: i64,
    pub path: String,                  // path as of this frame
    pub op: ChangeOp,                  // Created | Modified | Renamed | Deleted
    pub author: String,                // the Author class, wire form
    pub rename_from: Option<String>,
    pub is_current: bool,
    pub author_class: AuthorClass,
}

pub enum ChangeOp { Created, Modified, Deleted, Renamed }
```

### Unified activity feed

The activity-detail page, the version dropdown, and the queue-bar pending count consume one merged feed: each change surfaces as one item, with a status distinguishing committed history (a `ChangeRow` over a frame) from pending proposals (a `PendingItem` over a `.hiker/pending` edit carrying `surface`, `session_id`, `target_path`). The merge happens in `core::activity`; consumers don't reconcile two lists. Source filter is a first-class arg (`ChangesOnly` / `PendingOnly` / `Merged`); ordering is `timestamp_ms desc` with the frame / pending id as tiebreaker. [activity-feed-merged, activity-feed-unified-item, activity-feed-source-filter, activity-feed-merge-ordering]


## Diff rendering

Frames store whole-file snapshots (reconstructed via `materialize_at`); line-granularity is only the default rendering, not a storage constraint. Diffs render in tiers, all pure-Rust: [op-log-diff-tiers]

1. **Intra-line word/char highlight** — `core::diff` hunks for structure, a char/token diff within changed lines for the precise span (the GitHub / VS Code shape).
2. **Better hunk alignment** — histogram / patience over default Myers.
3. **Structural / AST diff** (deferred) — diff the markdown AST via tree-sitter so reflow / list-renumbering shows as no structural change. [op-log-diff-structural-deferred]


## External-edit reconcile

The `.md` is a write surface other tools own (a manual edit in another editor, a sync receive, a delete or rename elsewhere). Any divergence between the on-disk file and `accepted` is the user's accepted state advancing — folded back in as a frame authored `external`. [op-log-external-edit-sync]

The fold, given a tracked file:

1. Read the file's current bytes.
2. Compare to `accepted` (hash).
3. Identical → no-op (a self-write echo; `watcher-suppress-self-writes` is the first line, this hash check the safety net).
4. Differ → append a frame for the on-disk change with `Author::External`.

Hash-gated, not mtime-gated: a touched-but-byte-identical file mints no frame, so `op-log-disk-canonical` holds and an idle reopen commits nothing.

### Three triggers

| Trigger | When | Scope |
| ------- | ---- | ----- |
| Live (`watcher.md`) | a watcher event for a tracked path while hiker runs | one doc |
| Startup reconcile | vault open, before anything else commits | every tracked doc [op-log-startup-disk-reconcile] |
| Open-time reconcile | a buffer opens | that one doc, before its text loads [op-log-open-time-disk-reconcile] |

The watcher only reports changes that happen while it is watching; the **startup reconcile** closes the gap for edits made while hiker was closed (an mtime/size pre-filter skips rehashing files that plainly didn't change; the commit decision is still the byte hash). The **open-time reconcile** is the per-doc backstop for anything the watcher dropped in-session.

**Ordering at vault open: reconcile, then the first sync round.** A stale `accepted` is never pushed; an offline disk edit is never overwritten by an inbound merge that lands first. At startup `working` is empty; a pending edit re-anchors through drift detection.

### Offline delete and rename

A tracked path gone from disk at reconcile time is an offline delete: route it through the trash (`delete-note-core-cmd`, `files.md`); the file's `.ops` history is retained regardless, so restore recovers content and history. An offline rename detected at reconcile (a gone path's content matches a new untracked path) is recorded as a content-preserving move (`op-log-observed-move`) and links rewrite by path; a rename+heavy-rewrite done while hiker was closed may not be recognizable as a move, where content similarity is the fallback (`op-log-rename-follow-heuristic`). A present-but-unreadable file (non-UTF-8, permission error) is skipped, not mistaken for a delete; one un-reconcilable doc doesn't abort the pass (best-effort per doc).


## Sync substrate

Multi-device sync is a separate, pluggable transport that ships *files* (canonical `.md` + a version hash), never ops — specced in `sync.md`, with the git transport in `git.md`. This doc owns the local substrate sync rides on. [op-log-sync-substrate]

- The substrate is transport-agnostic: it produces and consumes whole-file content + version metadata. Transports (libp2p file-blob, integrated git, manual git, none) are swappable behind one seam and feed one 3-way text merge + one unified conflict surface (`sync.md`).
- Concurrent cross-device edits reconcile via that 3-way text merge: disjoint edits merge, same-region contention surfaces as a conflict for the user (the same conflict surface as the local user-vs-agent overlap, `op-log-merge-conflict`). No common base → fork conflict, never a silent interleave.
- The in-session three-way (the agent overlay) is the same text-diff machinery in memory; the transport only moves and merges *committed* text. So the transport's maturity never gates the editing engine.
- The embedding cache rides alongside but separate (below).


## Embedding cache

Embeddings are derived data — same content + same model → the same vector — so they aren't part of history or sync. A content-addressed cache under `.hiker/embeddings/` is keyed by `(content_hash, model_version)` and indexed in `index.db`. Each device regenerates locally if the cache is missing; any cross-device transfer is a separate content-addressed blob diff ("send me the vectors I don't have"), not part of the content stream. [op-log-embeddings-cache]


## Re-extraction

A sidecar whose source changed re-pulls extracted content as a frame on the sidecar: [op-log-reextract-replace]

| Policy | Behavior |
| ------ | -------- |
| **Replace** (default for linked sidecars) | Commit the new extraction over the body region, `Author::Extractor(<id>)`; frontmatter untouched (body-range write). Prior body stays in `.ops` history. |
| **Skip** (default for unlinked sidecars) | Don't run the extractor. |
| **Merge** (deferred) | Stage the new extraction as a pending edit for review — same hunk-review machinery as agent edits. [op-log-reextract-merge-deferred] |
| **Diff-and-prompt** (deferred) | Show the diff inline; let the user pick hunks. [op-log-reextract-diff-prompt-deferred] |


## Module placement

- `core::oplog` — owns the `working`-layer verbs (`apply_working_edit` / `materialize_working` / `commit_working` / `discard_working`), the pending verbs (`stage_pending` / `accept_pending` / `reject_pending`) over the in-memory edits + `.hiker/pending/`, the `.ops` history (`materialize_at`, `retain_frame`, the `RetainedOp` keyframe/delta encoding), and the reconcile seams. Plain Rust types cross the boundary. [op-log-module]
- `core::ops` — the higher-level write paths (`write_file`, `agent_write_note`, `agent_edit_note`, `flip_op_status`) plus the history seams (`path_history`, `content_at`, `previous_accepted_content`).
- `core::activity` — the projection over the `.ops` frames: the `ChangeRow` DTO + the merged accepted/pending feed.
- `app` — the editor pane runs the buffer; Save calls `commit_working`; per-hunk accept/reject route through `core::ops::flip_op_status`; the agent overlay renders the diff between the two ropes (`patch-review.md`).


## `[op-log]` config section

[op-log-config-section]

```toml
[op-log]
rejected_retention_days = 14    # GC dropped pending edits older than N days
auto_reject_on_drift   = false  # flip a drifted pending hunk to rejected
review_required        = true   # default status for agent writes; surface-specific overrides win
commit_debounce_ms     = 1500   # coalesce a burst of saves into one history frame
```

| Key | Type | Default | Scope | Notes |
| --- | ---- | ------- | ----- | ----- |
| `rejected_retention_days` | u32 | `14` | user + vault | GC age for dropped pending edits in `.hiker/pending/`. |
| `auto_reject_on_drift` | bool | `false` | user + vault | Auto-reject a pending hunk when it drifts against current `accepted`. |
| `review_required` | bool | `true` | user + vault | Default status for agent-authored writes. Surface overrides (`[mcp.tools]`, `[llm.background]`) apply. |
| `commit_debounce_ms` | u32 | `1500` | user + vault | Debounce window coalescing rapid saves into one history frame. |


## Out of scope

- **Sync transport design.** Lives in `sync.md` (transport seam + file sync) and `git.md` (the git transport); this doc specs the local substrate it rides on.
- **Encryption at rest.** Orthogonal to the log shape.
- **External extraction / import tooling.** Re-extraction rides the substrate (an `extractor`-authored frame); the external producer itself is `import.md`.
- **Cross-document atomic transactions.** A multi-file reorganization is N independent frames with no all-or-nothing guarantee (partial apply allowed); a batch is a display grouping for review, not a transaction. [op-log-reorg-batch]


## Deferred

- `op-log-reextract-merge-deferred` — pending-edit-shaped re-extraction policy.
- `op-log-reextract-diff-prompt-deferred` — interactive re-extraction review.
- `op-log-diff-structural-deferred` — AST/structural diff rendering tier.
- `op-log-external-handle-deferred` — external-source pointer documents under path-as-identity.


## Forward refs

- `patch-review.md` — per-hunk agent-edit review surface, built on the layered model and the pending overlay.
- `diff.md` — the `DiffLayer` primitive over `core::diff`.
- `sync.md` — the pluggable file-sync transport seam, the 3-way merge, the unified conflict surface.
- `git.md` — the git transport (integrated + manual), commit policy, the `Hiker-Author` / `Hiker-Rename` trailers.
- `mcp.md` — agent tool calls produce pending edits with `Author::Agent(<client-id>)`.
- `watcher.md` — the live external-edit trigger and self-write suppression.
- `files.md` — trash routing for deletes.
- `design.md` "Source-derived notes" — the sidecar architecture this composes with.
- `settings.md` — the `[op-log]` config section above.
- `index.md` — the search index (`index.db`), keyed by path; the embedding cache; the regenerable history query-index.
