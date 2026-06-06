# Patch review

In-editor surface for reviewing pending agent ops on the active buffer. Built on the op log (per `op-log.md`) and the `DiffLayer` primitive (per `diff.md`): the buffer renders the merge of the document's three op layers; the diff between the user's current view and that view plus the agent's pending ops renders as hunks; accept/reject flips op status.

The buffer renders `working`; the agent's proposal is a read-only overlay. The buffer's `current` is `materialize(accepted + working)` — the user's own text, edited directly, with cursor and edits in one coordinate space. The agent's `pending` proposals are *not* in the buffer text; the review is `DiffLayer { base: agent_base, current: buffer, owner: Agent }` against `materialize(accepted + working + pending(session))`, rendering pending ops as a suggestion overlay — additions as phantom blocks, deletions struck through (per `op-log-layered-model`). Same primitive that powers snapshot and history diff; no anchor tracker, recomputed from the two ropes each frame. [patch-review-buffer-state, patch-review-diff-layer]


## Buffer state

```rust
pub struct Buffer {
    pub doc_id: DocId,
    pub current: Rope,                       // materialize(accepted + working + pending(session))
    pub agent_base: Option<Rope>,            // materialize(accepted + working); None when no pending ops in scope
    pub loaded_content: Rope,                // disk text at last save/load; used by drift checks
    pub active_session: Option<SessionId>,   // which agent session's pending ops are in scope
    // ...other existing fields
}
```

`agent_base` feeds the inline `DiffLayer`. It clears when every pending op is resolved or when the buffer's active session changes. `loaded_content` retains its role as the save-path drift baseline — the editor's existing `pre-write-drift-check` keys off it, not off `agent_base`.

The op log is hot storage; the buffer is a view. Pending ops persist in `<doc-id>.pending` regardless of whether a tab is open; opening a tab materializes and renders, closing discards the buffer rope but leaves the queue untouched, re-opening rehydrates fresh. [patch-review-buffer-is-view]

When the user types, the editor binding applies the edit as a `user` op on the `working` layer (per `op-log-editor-binding`); `current` and `agent_base` recompute next frame. Because both include `working`, user edits sit on both sides of the diff base and never render as a hunk against themselves — only the agent's pending ops produce hunks. Save (`commit_working`) folds `working` into `accepted`. Accept / reject calls `core::ops::flip_op_status(op_ids, new_status)`; both ropes recompute next frame. [patch-review-coexisting-edits]


## Inline rendering

When `agent_base.is_some()` and the active tab is the buffer:

- The editor pushes a `DiffLayer { base: agent_base, current, owner: Agent }` onto the buffer's decoration stack. Pale-red line backgrounds for `base`-only lines, pale-green for `current`-only, intraline marks if the View toggle is on (per `diff-viewer-intraline`).
- The buffer remains fully editable. Typing produces new `user` ops on the `working` layer; the diff recomputes; pending-op hunks shift their `current_range` to track the user's typing (the CRDT positions move under the text edits).
- Each hunk's overlay widget carries `Accept ✓` and `Reject ✗` icon buttons. Same primitive snapshot diff uses (per `diff-layer-hunk-widgets`).
- Hunks contributed by ops sharing a `metadata.batch_id` (a single `edit_note` call's multiple `Replace` ops, say) are connected by a thin gutter marker. No coupled behavior — each is independently accept/rejectable. [patch-review-batch-grouping]


## Accept / reject

The hunk's `(base_range, current_range)` mapping comes from `DiffLayer`. Accept/reject resolves ops, not text. [patch-review-per-hunk-accept]

**Accept:**
1. Query the queue for pending ops in `active_session` whose CRDT position falls inside `current_range`. (Position-overlap query — `core::oplog::ops_in_range(doc_id, session, range)`.)
2. `core::ops::flip_op_status(op_ids, Accepted)` applies the contributing ops' queued updates to `accepted`, removes them from the queue, writes their `op_metadata` rows, and triggers a save-to-disk per `op-log-atomic-write`.
3. `agent_base` recomputes on the next frame and now includes the just-accepted content. The hunk disappears.

**Reject:**
1. Same lookup.
2. `flip_op_status(op_ids, Rejected)` drops them from the queue and writes a rejected audit row to `op_metadata`. They never reach `accepted`.
3. `current` recomputes; the hunk disappears.

When every pending op for `active_session` is resolved, `agent_base` clears to `None` and the file pill disappears.

Accept and reject operate only on the agent's `pending` ops — the user's uncommitted `working` edits survive both, and neither reloads the buffer from disk, so unsaved typing is never discarded.

**Accept-all / Reject-all** apply the per-hunk verbs sequentially over every pending op for the session. Drifted ops are skipped by Accept-all and resolved by Reject-all (the file pill's `(M drifted)` count covers them).


## Conflicts

`working` ops and `pending` ops merge by position (per `op-log.md`'s "Merge and conflicts"):

- **Disjoint regions** — the user edits one part of the document while the agent edits another. The merge is automatic; both render in the buffer and no prompt fires. [op-log-merge-auto]
- **Overlapping region** — a `working` edit and a `pending` edit touch the same line region. The overlap surfaces as a conflict hunk in the inline review with per-hunk **Keep mine** / **Keep theirs** / **Keep both**: keep-mine rejects the pending op over that span, keep-theirs accepts it and drops the user's overlapping edit, keep-both takes the positional merge. [op-log-merge-conflict]

A conflict hunk routes through the same `flip_op_status` machinery as a plain hunk; keep-theirs additionally discards the overlapping `working` op before applying the pending one.


## Save semantics

Save is `commit_working` (per `op-log.md`'s "Disk write invariant"): it folds the `working` layer into `accepted` and writes the result to disk per `op-log-atomic-write`. Pending ops are untouched — they stay in the queue and continue to show as hunks; the save path can't carry one to disk because pending lives outside both `working` and `accepted`. No pending op reaches disk without being explicitly accepted.

Reject is independent of save — it only mutates the log; disk content is unchanged.


## Drift

A pending op can become inconsistent with the current accepted state — e.g., a `Replace` whose `AnchorHint` (per `op-log-op-shape`) points at a body region a later user op has changed. Hiker re-derives drift by trying to apply each queued update to a clone of current `accepted` on every relevant change event:

- **Drifted ops** surface in the file pill's `(M drifted)` suffix. Click expands a popover listing each with `[Reject]` and `View` (opens the op's proposed content in diff mode against `Empty`). [patch-review-conflicted-hunk-display]
- **In the file pill popover** and the activity-detail page, Accept is greyed for drifted ops with the drift reason as tooltip. Reject stays active. [patch-review-conflicted-accept-disabled]
- **Auto-reject on drift** is opt-in via `[op-log] auto_reject_on_drift` per `op-log.md`'s config section. When set, drifted ops flip to `rejected` immediately rather than surfacing.


## File pill

A thin strip directly below the editor toolbar whenever the active buffer has `agent_base.is_some()`. [patch-review-file-pill]

- **Label.** `N hunks` from the live `DiffLayer`. Adds `(M drifted)` suffix when drifted ops exist for the active session on this document.
- **Accept all.** Sequentially accepts every hunk's contributing ops.
- **Reject all.** Sequentially rejects every pending op for the session (covers drifted ops too). Always confirms.
- **Next hunk.** Scrolls to the next hunk by document order, wrapping.
- **Visual family.** Same minimal chrome as `write-note-pending-banner`; muted background tint, single-line height.


## Whole-file and structured op review

Some op kinds don't compose into the per-hunk view:

- **Whole-body `Replace { range=entire_doc, content }`** (the shape `write_note` MCP calls emit). [write-note-review-surface]
- **`SetFrontmatter` patches.**
- **`Create` ops** for paths that don't yet exist on disk.

These open the editor tab in diff mode: the buffer holds the proposed content read-only; the diff layer shows `base = materialize(accepted + working)` (or `Empty` for `Create` against a non-existent path), `current = proposed content`. Toolbar mode-controls slot carries `Review rewrite` / `Review new note` plus Accept / Reject verbs. [write-note-review-mode-label]

- **Accept** folds the proposal into `accepted` (preserving `working` like the inline path). A whole-body rewrite replaces the body region; where it overlaps uncommitted `working` edits, the overlap surfaces as a conflict hunk with **Keep mine / Keep theirs / Keep both** (per `op-log-merge-conflict`); disjoint `working` edits merge automatically. Then navigates to the target note as a preview tab. [write-note-review-blocks-on-dirty, staging-accept-navigates-to-preview]
- **Drifted whole-file ops.** Accept disabled with reason as tooltip; proposed content still renders. Reject works. [write-note-review-conflicted-display]


## Open routing and precedence

On `openFile(rel)`, the whole-file review surface takes precedence over plain editing: a path with at least one pending whole-file op opens in the whole-file surface (most recent by `timestamp_ms`; older ones via the status-bar version dropdown), while a path with only hunk-shaped `Replace` ops opens in plain editing with the inline diff layer and file pill. When both shapes exist, accepting/rejecting the whole-file surface returns to plain editing where the hunk-shaped ops remain. [note-open-routes-to-pending-review]

When a tab is in plain editing but its path has a pending whole-file op, a thin **Pending-rewrite banner** above the editor reads `Pending rewrite for this note` (or `Pending new-note proposal` when the path doesn't exist) with a `Review` button into the whole-file surface; it stacks above the inline file pill when both shapes are pending. [write-note-pending-banner]


## Module placement

- `core::oplog` — `ops_in_range(doc_id, session, range)`, `flip_op_status(op_ids, status)`, materialization. Owns the substrate per `op-log.md`.
- `core::ops` — high-level write wrappers (`write_file_checked`, `agent_edit_note`, `flip_op_status`). The host calls these; nothing in `app/` reaches into `core::oplog` directly.
- `app/src/panels/editor/` — the unified editor tab body. Owns the diff toolbar toggle, source picker, "Show changes" menu, hunk overlay widget dispatch (per `DiffOwner`).
- `app/src/panels/editor/patch_review.rs` — buffer state mapping (`agent_base`, `current`, `active_session`); per-hunk accept/reject dispatch that maps hunks → contributing pending ops → `flip_op_status` calls.
- `app/src/panels/editor/patch_review_pill.rs` — the file pill component.
- `app/src/panels/changes.rs` — unified Changes tab: lists pending ops (with drifted status) and accepted op history. The cold-storage management surface.
- `editor::diff` — `DiffLayer` primitive, decoration emission, overlay widget hooks. Owner-aware widget content injected by the consumer via a render callback.


## Editor integration

- The editor widget remains editable for any buffer with `agent_base.is_some()`. Decorations are visual; user input is unaffected.
- `DiffLayer` decorations are pushed onto the same decoration stack as markdown styling, wikilinks, etc. View-zone insertions for removed lines participate in line-height calculation via the existing `DecorationLayers::push_with_heights` path.
- Hunk overlay widgets use the existing clickable-widget primitive (`ClickAction::WidgetClick`); widget ids namespaced from `WIDGET_ID_BASE`.
- Cursor and selection are unaffected by diff recomputation. Frame-level diff cost is bounded by `core::diff::compute(agent_base, current)` over the in-memory ropes — cheap for note-sized files; no incremental diff cache required.


## Pane integration

The whole-file review surface is identified by `Editor { buffer, diff: Some(PendingOp(_)) }`. Inline patch-review is identified by `agent_base.is_some()` on the buffer; the tab itself is plain `Editor { buffer, diff: None }`. No new tab kinds.

Navigation history:

- Entering the whole-file review surface pushes onto history.
- Per-hunk accept/reject in the inline view does *not* push — it's a plain-editing operation on the live buffer.
- Exiting the whole-file review surface pops back to plain editing on the same path.


## Multi-session

When more than one agent session has pending ops on the same document, the file pill shows one row per session: `Session foo: 3 hunks` / `Session bar: 1 hunk`. Clicking a row sets `active_session` and recomputes `agent_base` / `current` against just that session's ops. The "diff against my session's view" property the agent enjoys (per `op-log.md`'s read-after-write story) is per-session by construction. [patch-review-multi-session]


## Out of scope (this surface)

- **Cross-file proposal review.** Each inline view is scoped to the active buffer's document. Multi-file workflows (rename a function + update call sites) are N independent op streams; aggregation across documents is the activity-detail page's job. [patch-review-cross-file-deferred]
- **Rich three-way merge UI for whole-file accept-over-`working`.** Same-region overlap resolves through the per-hunk Keep mine / Keep theirs / Keep both conflict hunks (per `op-log-merge-conflict`); a richer three-way merge view rides `diff-viewer-three-way`. [patch-review-three-way-deferred]
- **Per-agent-attribution badges within a hunk.** `metadata.batch_id` grouping covers the common case. [patch-review-per-agent-attribution-deferred]
- **Keyboard navigation between hunks beyond the pill's Next button.** [patch-review-hunk-keybind-deferred]
- **Per-character review surface.** `op-log-per-op-status-flip` is the substrate; surfacing it as UI is deferred.


## Forward refs

- `op-log.md` — the substrate.
- `diff.md` (`diff-layer`, `diff-as-mode`, `diff-source-enum`, `diff-layer-hunk-widgets`) — the primitive and tab integration.
- `mcp.md` (`mcp-tool-edit-note`, `mcp-edit-note-validation`) — the producer side. `edit_note` calls emit `Replace` ops per `op-log-op-shape`.
- `editor.md` (`tab-kinds`, `editor-toolbar-mode-controls`, `editor-show-changes-menu`, `status-bar-version-dropdown`) — tab kinds, toolbar slot, status-bar dropdown.
- `op-log.md` "History materialization" (`changes-query-api`, `activity-feed-*`) — the projection layer pending ops surface through.
- `settings.md` — `[op-log]` config section (per `op-log-config-section`) replaces the prior `[staging]`.
