# Layered editing model

The local editing model every write rides on. A document is a plain `.md` file
on disk (canonical); its identity is its vault-relative path. Editing is a plain
text buffer — no CRDT, no op-log database. There is no bespoke history substrate:
local version history is plain-file **snapshots** (below), with optional
user-driven git (`git.md`) as the richer, shareable history.

Orientation: there is no on-disk frame log. Three in-memory views of one file
compose at edit time — `accepted` (the last committed `.md`, lazy-loaded from
disk), `working` (the editor buffer, uncommitted), and `pending(session)` (an
agent session's proposed edits, anchored against `accepted`). Saving commits
`working` into `accepted`, writes the `.md` atomically, and drops a plain-file
snapshot; accepting folds a `pending` hunk into `working`. Each behavior is
specced below.

## The headline decisions

- **Editing is plain text + editor-native anchored ranges** — no CRDT, no fuzzy
  patch matching. The buffer is the editor's rope; the user's edits land at plain
  byte offsets. Agent edits are anchored ranges (position + replacement vs
  `accepted`) remapped through the user's edits via the editor's own
  `ChangeSet`/`map_pos` — exact position tracking, not context-fuzzy.
  [op-log-layered-model]
status:: partial
implements:: [[code:hiker/oplog/working/impl#[OpLog]materialize_working]], [[code:hiker/oplog/working/impl#[OpLog]materialize_review]]
touches:: [[code:hiker/panels/buffer/editor_binding]]
note:: the editable buffer is `accepted + working`; the agent's `pending` renders on top as the review overlay. `materialize_working`/`materialize_review` seams + `app/src/panels/buffer/editor_binding.rs`. Making the overlay editor-native (anchored ranges via `map_pos`) is [[spec:op-log-three-way-overlay]]
- **Identity is the vault path** — no stable doc-id, no path↔id table. History
  (snapshots, git) follows content across a rename because hiker moves the
  snapshot directory with the file. [op-log-path-identity]
status:: partial
implements:: [[code:hiker/oplog/lifecycle/impl#[OpLog]create_document]], [[code:hiker/oplog/impl#[OpLog]doc_id_for_path]], [[code:hiker/oplog/impl#[OpLog]path_for_doc]]
note:: identity is the vault path; the per-doc `.pending` queue and the `.hiker/history/<path>/` snapshot dir follow the path, renames move them
- **Agent edits are an anchored-range overlay, accepted per-hunk** — proposed as a
  diff vs `accepted`, rendered inline, folded into the buffer on accept; rejected
  by dropping the range. [op-log-pending-patch]
status:: partial
note:: agent edits staged as a per-session set anchored against `accepted`; accept folds a hunk into `working`, reject drops it. `stage_pending`/`accept_pending`/`reject_pending`. The anchoring becomes editor-native under [[spec:op-log-three-way-overlay]]
- **Local history is plain-file snapshots** — each save writes a whole-`.md` copy
  under `.hiker/history/<rel-path>/<ts>.md` (no deltas, no codec, `cat`-able),
  capped by `[history]` count/age. Disposable cache: `rm -rf .hiker/history`
  loses only local history, nothing canonical. Optional git (`git.md`) is the
  richer, shareable parallel history. [plain-file-snapshots]
status:: done
implements:: [[code:hiker/snapshot/snapshot]], [[code:hiker/snapshot/list_snapshots]], [[code:hiker/snapshot/prune]], [[code:hiker/snapshot/move_snapshots]]
verifies:: [[code:hiker/snapshot/tests/snapshot_writes_plain_md_and_round_trips]], [[code:hiker/snapshot/tests/prune_drops_by_count]], [[code:hiker/snapshot/tests/prune_drops_by_age]]
note:: `core/src/snapshot.rs` — whole-`.md` snapshots under `.hiker/history/<rel>/<ts_ms>.md`, written on every save (identical-content save is a no-op), pruned by `RetentionPolicy{max_snapshots,max_age_days}` on each write; a rename moves the dir
- **The only database is the search index** (`index.db`, per `index.md`), fully
  regenerable, opened by `core::store` alone (a single writer). The op-log no
  longer touches it. [op-log-no-oplog-db]


## Document identity

A document is a markdown file; its identity is its vault-relative path.
[op-log-path-identity]

| Source location | Source type | Document is | Identity |
| --------------- | ----------- | ----------- | -------- |
| Vault-internal | markdown | the `.md` file itself | its vault path |
| Vault-internal | non-md | the sidecar `<src>.md` | sidecar's vault path |

No stable id, no `path → id` table. A note's snapshot directory and its `.pending`
queue are keyed by path and move with the document on a rename, so history
survives a rename without any internal identifier. A rename is an observed
content-preserving move (see "Renames" below); wikilinks rewrite by path
(`wikilinks.md`).

Sidecars decouple user-edited content (the `<src>.md`) from a binary source (PDF,
image, audio); the sidecar is the tracked document, the source is not (`design.md`
"Source-derived notes"). [op-log-sidecar-document]
status:: done
touches:: [[code:hiker/ops/op_writes]]
note:: non-md sources are edited via their sidecar `<src>.md`; the source is read-only to the model. `core/src/ops/op_writes.rs` create/bootstrap treats the sidecar as the document

### Renames

Path is identity, so a rename moves the document's `.md` *and* its snapshot
directory together (`core::snapshot::move_snapshots`); local history follows with
certainty when hiker **observes** the move (the file tree, a `move_note` call).
When git is integrated the move is also committed as a content-preserving rename
so `git log --follow` matches it (`git.md`). A rename + heavy rewrite performed
outside hiker while it was closed is the irreducible hard case for any system
without a stored id: the snapshot history simply starts fresh at the new path —
content is never lost, only the local-history lineage link breaks.
[op-log-observed-move, op-log-rename-follow-heuristic]


## Layered document model

Three views of one file compose at edit time: [op-log-layered-model]

- **`accepted`** — the last committed `.md`, lazy-loaded from disk, the
  snapshot/git baseline. Holds every change authorized to reach the repo — saved
  user edits, observed external edits, accepted agent edits. [op-log-accepted-base]
- **`working`** — the editor buffer (the editor's rope): the user's uncommitted
  edits over `accepted`. Local to the buffer until Save; equals `accepted` when
  the buffer is clean. Save commits `working`. [op-log-working-layer]
status:: done
implements:: [[code:hiker/oplog/working/impl#[OpLog]apply_working_edit]], [[code:hiker/oplog/working/impl#[OpLog]replace_working]], [[code:hiker/oplog/working/impl#[OpLog]materialize_working]], [[code:hiker/oplog/working/impl#[OpLog]materialize_review]], [[code:hiker/oplog/working/impl#[OpLog]discard_working]], [[code:hiker/oplog/working/impl#[OpLog]has_working_edits]], [[code:hiker/oplog/impl#[OpLog]commit_working]]
verifies:: [[code:hiker/oplog/tests/working_layer/working_edit_shows_in_buffer_not_on_disk]], [[code:hiker/oplog/tests/working_layer/commit_working_folds_into_accepted]], [[code:hiker/oplog/tests/working_layer/discard_working_reverts_to_accepted]], [[code:hiker/oplog/tests/working_layer/review_view_overlays_pending_on_working]], [[code:hiker/oplog/tests/working_layer/accept_preserves_working_edits]], [[code:hiker/oplog/tests/working_layer/external_edit_preserves_working_edits]], [[code:hiker/oplog/tests/working_layer/commit_after_accept_lands_both]]
note:: the user's uncommitted edits over `accepted`; `core/src/oplog/working.rs` (`apply_working_edit`/`materialize_working`/`discard_working`/`has_working_edits`) + `commit_working` (Save folds into `accepted`). Tested `working_edit_shows_in_buffer_not_on_disk`, `commit_working_folds_into_accepted`
- **`pending(session)`** — a per-agent-session set of proposed edits, anchored
  against `accepted`, staged for review and not committed. Accept folds a hunk
  into `working`; reject drops it. [op-log-pending-patch]

The editable buffer is `accepted + working` — the user's own text, so typing lands
at plain byte offsets with no coordinate translation. The agent's pending edits
render *on top* as the inline review overlay — the diff toward
`accepted + working + pending(session)` (per `patch-review.md`) — so the user sees
proposals in place while editing their own regions. The overlay's anchors are
remapped through the user's edits via the editor's `ChangeSet`/`map_pos` (exact,
not fuzzy); disjoint edits compose by position; accept/reject rebases the overlay
without disturbing `working`. [op-log-three-way-overlay]
status:: planned
touches:: [[code:hiker/panels/buffer/diff_overlay]]
note:: the agent overlay anchors via the editor's own `ChangeSet`/`map_pos` (exact remap). Ride the existing diff-based overlay (`app/src/panels/buffer/diff_overlay.rs` re-diffs each frame); accept builds a `Set` applied as a `Transaction`; drift is a base-context re-diff check

Why `working` is its own layer rather than committing keystrokes straight to
`accepted`: unsaved work stays a real, mergeable edit — an agent edit elsewhere
neither displaces it nor is displaced by it — while disk writes stay gated behind
an explicit Save. Why `pending` stays separate rather than merging into `working`:
proposals are reviewed per-hunk and never leave the device — staging is local, not
collaborative. More than one agent session = more than one `pending(session)`
overlay; the file pill swaps the active one (`patch-review.md`).


## Agent edits

The agent perceives a coherent, live document regardless of accept status. There
is not one document with a "pending" flag; there are multiple materialized views
of one base, one per perspective: [op-log-agent-session-view]
status:: planned
note:: the agent reads/writes a session text (`accepted` + its own edits), materialized in memory; pending hunks derive on demand by diffing `accepted` vs the session text. Reads-after-writes consistent, dependent edits compose, never blocks on acceptance

| view | = | who sees it |
| ---- | - | ----------- |
| disk / accepted | the committed `.md` | canonical baseline |
| user / working | accepted + the user's uncommitted edits | the user's editing buffer |
| agent / session | accepted + *this session's* edits | the agent's reads and writes |

The agent reads and writes against its **session view**, which always reflects its
own edits; "pending" is purely how those edits appear in the user's review view,
invisible to the agent. The agent never blocks on acceptance — user accept/reject
happens asynchronously, possibly after the agent's turn ends. The mental model is a
per-session branch: the agent's reads see its branch; the user sees the base plus a
reviewable diff of the branch.

Mechanism — session text + derived hunks:

- The session holds a working **session text** (a plain `String`) — its
  materialized view, `accepted` + this session's edits. Each agent edit splices the
  byte range in memory immediately, so reads-after-writes are consistent and
  dependent edits compose.
- The **pending hunks shown to the user are derived on demand** by diffing
  `accepted` against the session text (`editor_core::diff`). The hunks are a view
  for the user, not what the agent operates on.
- Persisted to `.hiker/pending/<session>/<path>.pending` (gitignored, inspectable
  text — the session text plus its anchor metadata) so it survives restart until
  accepted or rejected. [op-log-pending-survives-restart]
status:: done
implements:: [[code:hiker/oplog/store/save_pending]], [[code:hiker/oplog/store/load_pending]]
verifies:: [[code:hiker/oplog/tests/pending_survives_restart]], [[code:hiker/oplog/tests/unreadable_pending_queue_is_tolerated]]
touches:: [[code:hiker/oplog/store]]
note:: `core/src/oplog/store.rs::{save_pending, load_pending}` persist the pending set to `<path>.pending`; `ensure_loaded` reconstitutes on reopen. Tested `pending_survives_restart`
- Session-scoped: each session is its own text; concurrent sessions don't see each
  other's uncommitted work.

Why a plain `String`, not the editor's rope: the agent path is a coarse, infrequent,
headless mutator (LLM-tool-call cadence, KB-sized docs, localized edits) — a splice
is a sub-µs memmove dwarfed by the model round-trip, and the diff operates natively
on `&str`. Streaming edits fall out naturally: mutate the `String` (debounced) as
tokens arrive, re-derive the hunks for display.


## Merge and conflicts

`working` and `pending` hunks merge by position, in memory:

- **Disjoint regions** — the user edits one part, the agent another: the merge is
  automatic, both render in the buffer, no prompt. [op-log-merge-auto]
status:: done
verifies:: [[code:hiker/oplog/tests/working_layer/user_edit_below_agent_line_both_survive_accept_and_commit]], [[code:hiker/oplog/tests/working_layer/user_edit_above_agent_line_both_survive_accept_and_commit]], [[code:hiker/oplog/tests/working_layer/user_edit_disjoint_line_with_agent_multi_line_edits]], [[code:hiker/oplog/tests/working_layer/user_deletes_line_while_agent_edits_another]]
note:: disjoint `working`/`pending` edits merge by position automatically (both render, no prompt). Tested `review_view_overlays_pending_on_working`, `accept_preserves_working_edits`
- **Overlapping region** — both change the same span: hiker does not silently
  interleave them. The overlap surfaces as a conflict hunk in the inline review
  with per-hunk **Keep mine** / **Keep theirs** / **Keep both**. [op-log-merge-conflict]
status:: done
implements:: [[code:hiker/panels/buffer/patch_review/impl#[AppState]apply_hunk_keep_theirs]]
note:: overlapping `working`+`pending` edits surface as a conflict hunk (Keep mine / Keep theirs / Keep both) rather than a silent interleave. Pure detection + revert args in `app/src/panels/buffer/conflict.rs` (tested); verbs wire through `diff_overlay.rs` (`conflict_row`) → `patch_review.rs`

A conflict on a contended region is the desired behavior, not a regression: for
notes you want a conflict there, not a character-level interleave. The same inline
conflict resolver also handles git merge markers when git is integrated (`git.md`).


## Drift

When `accepted` advances (user typing, an observed external edit, an earlier
pending hunk accepted), a queued pending hunk may no longer apply — the base
context its anchor expects no longer matches. Drift is a re-diff check: re-derive
the hunks against current `accepted` and verify the hunk's base context still
matches exactly; a mismatch means the hunk is *drifted*. Surface in the file
pill's `(M drifted)` count; Accept disabled, Reject active. `auto_reject_on_drift`
drops drifted hunks automatically. [op-log-drift]
status:: done
note:: a pending hunk whose base context no longer matches current `accepted` is drifted (Accept disabled, Reject active); `auto_reject_on_drift` drops them. `is_pending_drifted` + `op_writes::auto_reject_drifted` (config-gated, fired post-save). Tested `ops/tests.rs::auto_reject_on_drift_flips_a_drifted_op`
implements:: [[code:hiker/oplog/impl#[OpLog]op_drifted]]


## Per-hunk accept / reject

The review surface diffs the user view against the agent view: [op-log-hunk-view]
status:: done
implements:: [[code:hiker/ops/op_writes/review_materializations]]
note:: review diffs `materialize(accepted+working)` (base) against `+pending(session)` (current) → `DiffLayer{owner:Agent}`; user typing sits on both sides so only agent hunks render. `op_writes::review_materializations` + `editor_binding.rs`; the inline layer diffs in `diff_overlay.rs`. Tested `ops/tests.rs::hunk_accept_applies_only_overlapping_ops`

```rust
let base    = materialize(accepted + working);                    // the user's current view
let current = materialize(accepted + working + pending(session)); // with the agent's proposal
let layer   = DiffLayer { base, current, owner: DiffOwner::Agent };
```

Per-hunk **accept** applies the hunk into the `working` buffer: build a `ChangeSet`
for the hunk's base range → replacement text and apply it as a `Transaction`
against the buffer (the `git add -p` data model); thereafter it is a normal working
edit, committed on Save. Per-hunk **reject** drops the hunk from the pending set.
Because `base` already includes `working`, the user's own typing is on both sides of
the review diff and produces no hunks against itself — only the agent's pending
hunks do. [op-log-per-hunk-accept-reject]
status:: done
implements:: [[code:hiker/oplog/impl#[OpLog]ops_in_range]], [[code:hiker/ops/op_writes/ops_in_hunk]], [[code:hiker/panels/buffer/patch_review/impl#[`BufCtx<'_>`]apply_pill_action]], [[code:hiker/panels/buffer/patch_review/impl#[AppState]apply_hunk_accept]], [[code:hiker/panels/buffer/patch_review/impl#[AppState]apply_hunk_reject]]
verifies:: [[code:hiker/oplog/tests/working_layer/two_agent_ops_accept_one_reject_other_with_user_edit]]
touches:: [[code:hiker/panels/buffer/patch_review]]
note:: `op_writes::ops_in_hunk` resolves a hunk → contributing edits; `flip_op_status` accepts (into `working`) / rejects. Accept-all skips drifted, Reject-all covers them. `diff_overlay.rs::attach_agent_hunk_widgets` + `patch_review.rs::{apply_hunk_accept, apply_hunk_reject, apply_pill_action}`. Tested `ops/tests.rs::{hunk_accept_applies_only_overlapping_ops, hunk_reject_leaves_accepted_untouched}`

Whole-file and structured proposals that don't compose into per-hunk view (a
`write_note` whole-body rewrite, a `Create`/`Tombstone`/`Rename` lifecycle op) are
reviewed via the whole-file / confirm-card surfaces in `patch-review.md`.


## Save policy

Saves are driven programmatically; the user never types a storage command.
[op-log-save-policy]

- **Commit on Save** — Ctrl/Cmd-S folds `working` into `accepted`, writes the `.md`
  atomically, then writes a plain-file snapshot (below). When git is integrated and
  `auto_commit` is on, the save also drops a git commit (`git.md`). Snapshots
  coalesce: an identical-content save writes no new snapshot.
- An agent-accept is a normal working edit, committed on the next Save like any
  other.
- History is per-save, not per-keystroke — sub-save granularity stays in-session.


## Storage layout

```
vault/
  *.md                               # canonical content (accepted)
  .hiker/                            # local hiker state
    history/<rel-path>/<ts>.md       # plain-file snapshots (local history) — REGENERABLE CACHE
    pending/<session>/<path>.pending # un-accepted agent edits (inspectable text) — DURABLE
    refs/<rel-path>/                 # imported binary artifacts (import.md) — DURABLE
    index.db                         # search / vector index (index.md) — the only db, REGENERABLE
    autosave/                        # crash-recovery sidecars (autosave.md) — REGENERABLE
```

[op-log-store-layout]
status:: done
implements:: [[code:hiker/oplog/impl#[OpLog]open]]
touches:: [[code:hiker/oplog/store]], [[code:hiker/snapshot]]
note:: durable `.md` (canonical) + per-doc `.pending` queue + imported `refs/`; regenerable snapshot history, `index.db`, autosave. `core/src/oplog/store.rs` + `core/src/snapshot.rs`

Durability classes under `.hiker/`. **Durable** (not reconstructible from the `.md`
alone): the un-accepted `.pending` edits and the imported artifacts under `refs/`.
**Regenerable cache**: the snapshot history (lose it and you lose only *local*
version history — the canonical `.md` and any git history are untouched), `index.db`
(rebuilds from the notes), autosave (transient crash-recovery). So `rm -rf .hiker/`
loses only un-accepted pending edits and imported artifacts — never any canonical
`.md` content. (`design.md` "Sync / backup" carries the full backup-class table.)

When git is integrated (`git.md`), `.hiker/` is gitignored — git tracks only the
user's markdown and attachments, and supplies its own commit-graph history in
parallel to the snapshots.


## Disk write invariant

A commit into `accepted` — Save folding `working` in (`commit_working`), or accept
folding a pending hunk in — writes the `.md` atomically (temp + rename + fsync),
then writes the snapshot (best-effort, non-atomic; a snapshot is disposable cache).
[op-log-atomic-write]
status:: done
implements:: [[code:hiker/oplog/lifecycle/impl#[OpLog]tombstone_document]], [[code:hiker/oplog/lifecycle/impl#[OpLog]rename_document]], [[code:hiker/oplog/lifecycle/impl#[OpLog]restore_document]], [[code:hiker/oplog/impl#[OpLog]apply_user_edit]], [[code:hiker/oplog/impl#[OpLog]commit_working]], [[code:hiker/oplog/impl#[OpLog]commit_text_edit]], [[code:hiker/oplog/write_md_file]], [[code:hiker/oplog/store/write_atomic]]
verifies:: [[code:hiker/oplog/tests/working_layer/commit_working_folds_into_accepted]], [[code:hiker/oplog/tests/working_layer/commit_after_accept_lands_both]]
touches:: [[code:hiker/oplog/store]]
note:: save order: write the `.md` atomically (temp+rename+fsync via `store::write_atomic`) → write the snapshot. `core/src/oplog/mod.rs` (`apply_user_edit`/`commit_working`)

The `.md` is canonical. A crash after the file write but before the snapshot leaves
a junk-free, fully-correct `.md` — the snapshot is simply skipped that save.
Uncommitted `working` edits live in memory (crash-recovered from the autosave
sidecar, `autosave.md`); pending edits live in `.hiker/pending/`.


## Materialization

`materialize(accepted)` is the current `.md`; the editable buffer is
`accepted + working + pending(session)` composed in memory. There is no parse /
re-emit step — the bytes the user wrote are the bytes committed and re-read, so
opening and saving never rewrites a character the user didn't change.
[op-log-materialization, op-log-disk-canonical]


## Local history

Local version history is the plain-file snapshot tree, the trustworthy inverse of
a delta-chained codec: each snapshot is a whole `.md`, no chain, `cat`-able,
disposable. [plain-file-snapshots]

- **Layout & trigger.** `.hiker/history/<vault-relative-path>/<timestamp_ms>.md`,
  one whole-file snapshot per save (an identical-content save is a no-op, so a save
  that changed nothing mints no snapshot). A rename moves the directory with the
  note (`move_snapshots`).
- **Retention.** Capped by count **and** age via `[history]` (default keep-50 /
  30-days); pruned on every save. `0` on either knob disables that dimension. The
  prune count is logged (no silent truncation).
- **History listing** — `op_writes::snapshot_history(path)` lists a note's snapshots
  newest-first to drive the version dropdown and per-file history. The newest
  snapshot mirrors the current on-disk content.
- **Content at a version** — `op_writes::content_at_snapshot(path, snapshot_id)`
  reads a snapshot's whole-file content straight off disk (the id is the snapshot's
  millisecond timestamp).
- **Rollback** — "restore this version" reads a snapshot and writes it back as a
  normal save (forward-correct: the old content lands as a fresh edit, so the
  newest snapshot and any git history stay append-only). [changes-rollback-helper]
status:: done
implements:: [[code:hiker/ops/op_writes/snapshot_history]], [[code:hiker/ops/op_writes/content_at_snapshot]], [[code:hiker/ops/op_writes/previous_snapshot_content]]
touches:: [[code:hiker/panels/buffer]], [[code:hiker/panels/home]]
note:: `app/src/panels/home.rs::rollback_change` pulls the prior content via `op_writes::previous_snapshot_content` and writes it back through `op_writes::user_save` — a fresh save that becomes the newest snapshot. Version restore (`app/src/panels/buffer/mod.rs`) reads `content_at_snapshot` and re-saves through `user_save` the same way

The richer, shareable history is optional git (`git.md`): when integrated, every
save commits, and `git log --follow` / `git show <sha>:<path>` give a globally
ordered cross-device commit graph the local snapshots don't. Attribution (who
authored a change) survives only via git's `Hiker-Author` trailers when git is on;
there is no author-class side table.


## External edits

The `.md` is a write surface other tools own (a manual edit in another editor, a
`git pull` moving HEAD, a delete or rename elsewhere). With no frame to mint, the
reconcile collapsed to a simple rule: a clean buffer reloads from disk; `accepted`
is lazy-loaded from the `.md`, so the next read sees the external content.
[op-log-external-edit-sync]
status:: done
implements:: [[code:hiker/oplog/impl#[OpLog]ensure_loaded]]
note:: `accepted` is loaded lazily from the `.md`; a clean buffer reloads on a watcher event (`watcher.md`). `Watcher::suppress` keeps the indexer's/save's own writes from looping. There is no startup full-vault rehash and no offline-rename lineage machinery — those died with the `.ops` engine

- A **clean buffer** (no `working` edits) reloads from disk on a watcher event
  (`watcher.md`).
- A **dirty buffer** vs an external edit is the deferred conflict case: git's merge
  markers cover it when git is integrated (`git.md`); a simple prompt otherwise.
- `Watcher::suppress` self-write suppression is kept so the indexer's and save's
  own writes don't loop the indexer.

There is **no** startup full-vault rehash, **no** open-time per-doc fold, and **no**
offline-rename lineage machinery — all of that was the `.ops` reconcile, deleted
with the history engine.


## Module placement

- `core::oplog` — owns the `working`-layer verbs (`apply_working_edit` /
  `materialize_working` / `commit_working` / `discard_working`) and the pending
  verbs (`stage_pending` / `accept_pending` / `reject_pending`) over the in-memory
  edits + `.hiker/pending/`. No history engine, no `index.db` access. Plain Rust
  types cross the boundary. [op-log-module]
status:: done
implements:: [[code:hiker/oplog/OpLog]], [[code:hiker/oplog/impl#[OpLog]open]]
touches:: [[code:hiker/oplog]]
note:: `core::oplog::OpLog` — `open`/`apply_user_edit`/`stage_pending`/`accept_pending`/`reject_pending`/`materialize_accepted`/`materialize_pending_view` + substrate verbs (`create_document`/`tombstone_document`/`rename_document`/`pending_ops`/`is_pending_drifted`); plain Rust types cross the boundary (`DocContent`, `shapes::{PendingOp, Author, OpKind, AnchorHint}`, `error::Error`)
- `core::snapshot` — the plain-file snapshot store (`snapshot` / `list_snapshots` /
  `read` / `prune` / `move_snapshots`), config-free (callers pass a
  `RetentionPolicy`). [plain-file-snapshots]
- `core::ops` — the higher-level write paths (`user_save`, `stage_agent_edits`,
  `flip_op_status`) plus the snapshot-history seams (`snapshot_history`,
  `content_at_snapshot`, `previous_snapshot_content`).
- `app` — the editor pane runs the buffer; Save calls `commit_working`; per-hunk
  accept/reject route through `core::ops::flip_op_status`; the agent overlay renders
  the diff between the two ropes (`patch-review.md`).


## `[op-log]` config section

[op-log-config-section]
status:: done
implements:: [[code:hiker/config/sections/OpLogConfig]], [[code:hiker/config/Config#op_log]]
note:: `core/src/config/sections.rs::OpLogConfig` keys (`rejected_retention_days`/`auto_reject_on_drift`/`review_required`). The `[op-log]` section tunes only the in-memory pending layer now; snapshot retention is the separate `[history]` section (`settings.md`)

```toml
[op-log]
rejected_retention_days = 14    # GC dropped pending edits older than N days
auto_reject_on_drift   = false  # flip a drifted pending hunk to rejected
review_required        = true   # default status for agent writes; surface-specific overrides win
```

| Key | Type | Default | Scope | Notes |
| --- | ---- | ------- | ----- | ----- |
| `rejected_retention_days` | u32 | `14` | user + vault | GC age for dropped pending edits in `.hiker/pending/`. |
| `auto_reject_on_drift` | bool | `false` | user + vault | Auto-reject a pending hunk when it drifts against current `accepted`. |
| `review_required` | bool | `true` | user + vault | Default status for agent-authored writes. Surface overrides (`[mcp.tools]`, `[llm.background]`) apply. |

Snapshot retention lives in the `[history]` section (`settings.md`); git in `[git]`
(`git.md`).


## Out of scope

- **Multi-device sync.** Removed. The richer, shareable history is optional git
  (`git.md`); a third-party file sync of the vault folder works because the vault is
  just plain files.
- **Encryption at rest.** Orthogonal to the editing model.
- **External extraction / import tooling.** Hiker does no in-process extraction;
  the external producer + the manifest contract are `import.md`.
- **Cross-document atomic transactions.** A multi-file reorganization is N
  independent edits with no all-or-nothing guarantee (partial apply allowed); a
  batch is a display grouping for review, not a transaction. [op-log-reorg-batch]
status:: done
implements:: [[code:hiker/oplog/pending/impl#[`super::OpLog`]stage_pending_renames]], [[code:hiker/oplog/pending/impl#[`super::OpLog`]accept_batch]], [[code:hiker/oplog/pending/impl#[`super::OpLog`]reject_batch]], [[code:hiker/ops/op_writes/stage_reorg_batch]], [[code:hiker/suggest/apply_tree]], [[code:hiker/suggest/stage_moves]]
verifies:: [[code:hiker/oplog/tests/reorg_batch_stages_n_renames_sharing_a_batch_id]], [[code:hiker/oplog/tests/reorg_batch_accept_moves_each_file_on_disk]], [[code:hiker/oplog/tests/reorg_batch_partial_apply_skips_a_collision]]
note:: `stage_pending_renames` (one pending `Rename` per moved note, one cross-document `batch_id`) + `accept_batch`/`reject_batch` (partial apply — a target-occupied collision refuses just that move so the batch's others still land). Accept moves the file on disk per [[spec:op-log-atomic-write]]. Drives [[spec:cluster-editor-multi-select-stage-move]]


## Forward refs

- `patch-review.md` — per-hunk agent-edit review surface, built on the layered model and the pending overlay.
- `diff.md` — the `DiffLayer` primitive over `core::diff`.
- `git.md` — optional, user-driven git: commit-on-save, the `log`/`show`/`diff_paths` read API, the `Hiker-Author`/`Hiker-Rename` trailers, the conflict-marker resolver.
- `mcp.md` — agent tool calls produce pending edits with `Author::Agent(<client-id>)`.
- `watcher.md` — the live external-edit trigger and self-write suppression.
- `files.md` — trash routing for deletes.
- `design.md` "Source-derived notes" — the sidecar architecture this composes with; "Sync / backup" — the durability/backup classes.
- `settings.md` — the `[op-log]` and `[history]` config sections.
- `index.md` — the search index (`index.db`), keyed by path.
