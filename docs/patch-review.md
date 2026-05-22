# Patch review

In-editor surface for reviewing pending agent ops on the active buffer. Built on the op log (per `op-log.md`) and the `DiffLayer` primitive (per `diff.md`): pending ops for the open document materialize into the buffer view; the diff between accepted-only and accepted-plus-pending materializations renders as hunks; accept/reject flips op status.

The headline decisions:

- **The buffer renders `materialize(pending_view(session))`.** Opening a document with pending ops doesn't apply them to a separate base; the buffer's `current` rope is the pending-view materialization (`accepted` cloned + the session's queued pending updates applied on top). The accepted materialization (= what's on disk) is `agent_base`. The diff renders both. [patch-review-buffer-state]
- **Inline review is `DiffLayer { base: agent_base, current: buffer, owner: Agent }`.** Same primitive that powers snapshot diff and history diff. No anchor tracker; the diff recomputes from the two ropes each frame. [patch-review-diff-layer]
- **Per-hunk accept/reject resolves ops, not text.** Accept finds the pending ops contributing to the hunk's `current_range`, applies their queued updates to `accepted`, removes them from the queue, and re-runs the save-to-disk projection. Reject drops them from the queue (writing a rejected audit row). The buffer's `current` recomputes from the new materialization on the next frame. [patch-review-per-hunk-accept]
- **The op log is hot storage; the buffer is a view.** Pending ops persist in `<doc-id>.pending` regardless of whether a tab is open. Opening a tab is just "materialize and render"; closing a tab discards the buffer rope but leaves the queue untouched. Re-opening rehydrates fresh. [patch-review-buffer-is-view]
- **User edits go to `accepted`; agent edits wait in the queue.** Typing into a buffer with pending ops applies new `user` ops directly to the `accepted` Doc. They show up in `current` immediately (because `pending_view` is `accepted` + the queue) but produce no diff hunks against themselves — the diff is `accepted` vs `pending_view(session)`, so user edits are on both sides. Only the agent's pending ops produce hunks. [patch-review-coexisting-edits]
- **The file pill carries bulk verbs and hunk navigation.** Thin strip above the editor when the open document has any pending ops in the active agent session: `N hunks (M drifted)` plus `[Accept all] [Reject all] [Next hunk]`. [patch-review-file-pill]
- **Whole-file proposals (`write_note` shape, `SetFrontmatter`) open in diff mode against the accepted materialization.** Per-hunk doesn't compose with a single-op whole-document replacement; the editor tab opens with `diff = Some(PendingOp(op_id))`, owner `Agent`, and Accept / Reject in the toolbar's mode-controls slot. [write-note-review-surface]


## Buffer state

```rust
pub struct Buffer {
    pub doc_id: DocId,
    pub current: Rope,                       // materialize(pending_view(session))
    pub agent_base: Option<Rope>,            // materialize(accepted); None when no pending ops in scope
    pub loaded_content: Rope,                // disk text at last save/load; used by drift checks
    pub active_session: Option<SessionId>,   // which agent session's pending ops are in scope
    // ...other existing fields
}
```

`agent_base` is set only when at least one pending op for `active_session` exists on the document. It feeds the inline `DiffLayer`. It clears when every pending op is resolved (accepted or rejected) or when the buffer's active session changes.

`loaded_content` retains its role as the save-path drift baseline — the editor's existing `pre-write-drift-check` keys off it, not off `agent_base`.

When the user types, the editor applies `Insert` / `Delete` ops with `author=user` directly to the `accepted` Doc; `current` recomputes on the next frame.

When the user accepts / rejects a hunk, the editor calls `core::ops::flip_op_status(op_ids, new_status)`; `current` and `agent_base` recompute on the next frame.


## Inline rendering

When `agent_base.is_some()` and the active tab is the buffer:

- The editor pushes a `DiffLayer { base: agent_base, current, owner: Agent }` onto the buffer's decoration stack. Pale-red line backgrounds for `base`-only lines, pale-green for `current`-only, intraline marks if the View toggle is on (per `diff-viewer-intraline`).
- The buffer remains fully editable. Typing produces new accepted `user` ops; the diff recomputes; pending-op hunks shift their `current_range` to track the user's typing (the CRDT positions move under the text edits).
- Each hunk's overlay widget carries `Accept ✓` and `Reject ✗` icon buttons. Same primitive snapshot diff uses (per `diff-layer-hunk-widgets`).
- Hunks contributed by ops sharing a `metadata.batch_id` (a single `edit_note` call's multiple `Replace` ops, say) are connected by a thin gutter marker. No coupled behavior — each is independently accept/rejectable. [patch-review-batch-grouping]


## Accept / reject

The hunk's `(base_range, current_range)` mapping comes from `DiffLayer`.

**Accept:**
1. Query the queue for pending ops in `active_session` whose CRDT position falls inside `current_range`. (Position-overlap query — `core::oplog::ops_in_range(doc_id, session, range)`.)
2. `core::ops::flip_op_status(op_ids, Accepted)` applies the contributing ops' queued updates to `accepted`, removes them from the queue, writes their `op_metadata` rows, and triggers a save-to-disk per `op-log-atomic-write`.
3. `agent_base` recomputes on the next frame and now includes the just-accepted content. The hunk disappears.

**Reject:**
1. Same lookup.
2. `flip_op_status(op_ids, Rejected)` drops them from the queue and writes a rejected audit row to `op_metadata`. They never reach `accepted`.
3. `current` recomputes; the hunk disappears.

When every pending op for `active_session` is resolved, `agent_base` clears to `None` and the file pill disappears.

**Accept-all / Reject-all** apply the per-hunk verbs sequentially over every pending op for the session. Drifted ops are skipped by Accept-all and resolved by Reject-all (the file pill's `(M drifted)` count covers them).


## Save semantics

Saving runs through `core::ops::write_file_checked`, which calls `OpLog::materialize_accepted` and writes the result to disk per `op-log-atomic-write`. The save path doesn't care about pending ops — they're queued, not in `accepted`, and so don't reach disk.

When the user types in a buffer with pending ops, their typing applies new `user` ops to `accepted`; the on-disk markdown advances accordingly. The pending agent ops continue to wait in the queue and continue to show as hunks. The "you saved without reviewing" failure mode is gone — there is no path by which a pending op reaches disk without being explicitly accepted.

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

These open the editor tab in diff mode: the buffer holds the proposed content read-only; the diff layer shows `base = materialize(accepted)` (or `Empty` for `Create` against a non-existent path), `current = proposed content`. Toolbar mode-controls slot carries `Review rewrite` / `Review new note` plus Accept / Reject verbs. [write-note-review-mode-label]

- **Accept blocks while the live buffer for the path (if any) is dirty.** Modal: "Your buffer has unsaved changes. Save or revert before accepting this rewrite." Reject works regardless. Whole-file accept has no per-hunk story with the user's pending typing. [write-note-review-blocks-on-dirty]
- **Accept navigates to the target note** as a preview tab. [staging-accept-navigates-to-preview]
- **Drifted whole-file ops.** Accept disabled with reason as tooltip; proposed content still renders. Reject works. [write-note-review-conflicted-display]


## Auto-routing on open

When `openFile(rel)` resolves a path that has at least one pending whole-file op, the open lands in the whole-file review surface rather than plain editing. The most recent such op by `timestamp_ms` is the one shown; older pending ops are accessible via the status-bar version dropdown. [note-open-routes-to-pending-review]

When a path has pending `Replace` ops (hunk-shaped) but no whole-file op, the open lands in plain editing with the inline diff layer and file pill visible.

When both shapes exist, the whole-file surface takes precedence on open. Accepting or rejecting it returns the user to plain editing where the hunk-shaped ops remain.


## Pending-rewrite banner

When a buffer tab is in plain editing (not the whole-file review surface) and its path has at least one pending whole-file op, a thin banner above the editor reads `Pending rewrite for this note` (or `Pending new-note proposal` when the path doesn't exist), with a `Review` button that switches the tab into the whole-file review surface. [write-note-pending-banner]

Stacks with the inline file pill — a path with both shapes pending renders pill above banner.


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
- **Three-way merge for whole-file accept-while-dirty.** Blocked-and-tell is the posture. Real three-way merge rides `diff-viewer-three-way`. [patch-review-three-way-deferred]
- **Per-agent-attribution badges within a hunk.** `metadata.batch_id` grouping covers the common case. [patch-review-per-agent-attribution-deferred]
- **Keyboard navigation between hunks beyond the pill's Next button.** [patch-review-hunk-keybind-deferred]
- **Per-character review surface.** `op-log-per-op-status-flip` is the substrate; surfacing it as UI is deferred.


## Forward refs

- `op-log.md` — the substrate.
- `diff.md` (`diff-layer`, `diff-as-mode`, `diff-source-enum`, `diff-layer-hunk-widgets`) — the primitive and tab integration.
- `mcp.md` (`mcp-tool-edit-note`, `mcp-edit-note-validation`) — the producer side. `edit_note` calls emit `Replace` ops per `op-log-op-shape`.
- `editor.md` (`tab-kinds`, `editor-toolbar-mode-controls`, `editor-show-changes-menu`, `status-bar-version-dropdown`) — tab kinds, toolbar slot, status-bar dropdown.
- `changes.md` (`changes-query-api`, `activity-feed-*`) — the projection layer pending ops surface through.
- `settings.md` — `[op-log]` config section (per `op-log-config-section`) replaces the prior `[staging]`.
