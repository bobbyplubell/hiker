# Patch review

In-editor surface for reviewing an agent session's pending edits on the active buffer. Built on the layered document model (per `op-log.md`) and the `DiffLayer` primitive (per `diff.md`): the buffer renders the user's own text (`materialize(accepted + working)`); the agent's pending edits render on top as a read-only diff overlay; per-hunk accept folds a hunk into `working`, reject drops it from the pending set.

The buffer's `current` is `materialize(accepted + working)` — the user's own text, edited directly, with cursor and edits in one coordinate space. The agent's `pending` edits are *not* in the buffer text; the review is a `DiffLayer { base: agent_base = materialize(accepted + working), current = materialize(accepted + working + pending(session)), owner: Agent }`, rendering pending hunks as a suggestion overlay — additions as phantom blocks, deletions struck through (per [[spec:op-log-layered-model]]). Same primitive that powers snapshot and history diff; no anchor tracker — both ropes are recomputed each frame and the diff *is* the state. [patch-review-buffer-state, patch-review-diff-layer]


## Buffer state

```rust
pub struct Buffer {
    pub path: PathBuf,
    pub current: Rope,                       // materialize(accepted + working)
    pub agent_base: Option<Rope>,            // materialize(accepted + working); None when no pending edits in scope
    pub loaded_content: Rope,                // disk text at last save/load; used by drift checks
    pub active_session: Option<SessionId>,   // which agent session's pending edits are in scope
    // ...other existing fields
}
```

`agent_base` is the `base` side of the inline `DiffLayer`; the `current` side is `materialize(accepted + working + pending(session))`, derived by splicing the active session's anchored ranges over `current`. `agent_base` clears when every pending hunk is resolved or when the buffer's active session changes. `loaded_content` retains its role as the save-path drift baseline — the editor's existing [[spec:pre-write-drift-check]] keys off it, not off `agent_base`.

The buffer is a view; the durable state is the `.md` plus the un-accepted `.pending` edits. Pending edits persist in `.hiker/pending/<session>/<path>.pending` regardless of whether a tab is open; opening a tab materializes and renders, closing discards the buffer rope but leaves the pending set untouched, re-opening rehydrates fresh. [patch-review-buffer-is-view]
status:: done
note:: the buffer rope is a pure re-materialization off the substrate: open re-materializes via `editor_binding`'s `materialize_review`, close discards the rope (`close_tab` buffer drop), reopen re-materializes fresh

When the user types, the edit lands at plain byte offsets on the `working` layer (the buffer is the editor's rope); `current` recomputes and the session's anchored ranges remap through the edit via the editor's `ChangeSet`/`map_pos` (exact, not fuzzy). Because `current` and `agent_base` are both `materialize(accepted + working)`, user edits sit on both sides of the diff base and never render as a hunk against themselves — only the agent's pending edits produce hunks. Save (`commit_working`) folds `working` into `accepted`. Accept folds the hunk into `working`; reject drops it; both ropes recompute next frame. [patch-review-coexisting-edits]
status:: done
note:: user typing produces accepted ops; the diff `base` vs `current` excludes/includes them symmetrically so user typing produces no self-diff hunks — it sits on both sides of the diff base


## Inline rendering

When `agent_base.is_some()` and the active tab is the buffer:

- The editor pushes a `DiffLayer { base: agent_base, current, owner: Agent }` onto the buffer's decoration stack. Pale-red line backgrounds for `base`-only lines, pale-green for `current`-only, intraline marks if the View toggle is on (per [[spec:diff-viewer-intraline]]).
- The buffer remains fully editable. Typing lands as a `working` edit at byte offsets; the diff recomputes; pending hunks shift their `current_range` to track the user's typing as their anchored ranges remap through the user's `ChangeSet` via `map_pos`.
- Each hunk's overlay widget carries `Accept ✓` and `Reject ✗` icon buttons. Same primitive snapshot diff uses (per [[spec:diff-layer-hunk-widgets]]).
- Hunks contributed by edits sharing a `metadata.batch_id` (a single `edit_note` call's multiple replacements, say) are connected by a thin gutter marker. No coupled behavior — each is independently accept/rejectable. [patch-review-batch-grouping]
status:: planned
note:: ops sharing `metadata.batch_id` (e.g. multiple `Replace` ops from one `edit_note` call) get a thin gutter connector between sibling hunks. No coupled behavior; per-hunk accept/reject independent


## Accept / reject

The hunk's `(base_range, current_range)` mapping comes from `DiffLayer`. Accept/reject resolves the hunk against the pending set, then mutates text only on accept. [patch-review-per-hunk-accept]
status:: done
touches:: [[code:hiker/panels/buffer/patch_review]]
note:: `diff_overlay.rs::attach_agent_hunk_widgets` resolves each hunk's `current_range` → pending op ids via `op_writes::ops_in_hunk(rel, session, start, end)`; the Accept/Reject click routes through `app/src/panels/buffer/patch_review.rs::{apply_hunk_accept, apply_hunk_reject}` → `op_writes::flip_op_status`. After the flip the next-frame editor binding re-materializes the pending-view (`editor_binding::materialize_review`) so resolved hunks disappear

**Accept:**
1. Map the hunk to its contributing pending edits — the session's anchored ranges (vs `accepted`) whose remapped current position falls inside `current_range`. (Byte-range lookup over the in-memory anchored ranges — `core::oplog::ops_in_range(path, session, range)`.)
2. Fold the hunk into `working`: build a `ChangeSet` `Set` for the hunk's base range → replacement text and apply it as a `Transaction` against the buffer (the `git add -p` data model), then drop the contributing edits from the pending set and write the resolution to `.hiker/pending/`. Thereafter the change is a normal `working` edit, committed on the next Save per [[spec:op-log-atomic-write]].
3. `agent_base` recomputes on the next frame and now includes the just-accepted content (it is in `working`). The hunk disappears.

**Reject:**
1. Same lookup.
2. Drop the contributing edits from the pending set; write a rejected audit entry to `.hiker/pending/`. They never reach `working` or `accepted`.
3. `current` recomputes; the hunk disappears.

When every pending hunk for `active_session` is resolved, `agent_base` clears to `None` and the file pill disappears.

Accept and reject operate only on the agent's `pending` edits — the user's uncommitted `working` edits survive both, and neither reloads the buffer from disk, so unsaved typing is never discarded. Because accept routes through the `ChangeSet`/`Transaction` path rather than reloading, the user's cursor and concurrent edits are preserved.

**Accept-all / Reject-all** apply the per-hunk verbs sequentially over every pending hunk for the session. Drifted hunks are skipped by Accept-all and resolved by Reject-all (the file pill's `(M drifted)` count covers them).


## Conflicts

`working` edits and `pending` edits merge by position (per `op-log.md`'s "Merge and conflicts"):

- **Disjoint regions** — the user edits one part of the document while the agent edits another. The merge is automatic; both render in the buffer and no prompt fires. [op-log-merge-auto]
- **Overlapping region** — a `working` edit and a `pending` edit touch the same line region. The overlap surfaces as a conflict hunk in the inline review with per-hunk **Keep mine** / **Keep theirs** / **Keep both**: keep-mine rejects the pending hunk over that span, keep-theirs accepts it and drops the user's overlapping edit, keep-both takes the positional merge. Routed through the one unified conflict surface (per [[spec:op-log-merge-conflict]]). [op-log-merge-conflict]

A conflict hunk routes through the same accept/reject machinery as a plain hunk; keep-theirs additionally discards the overlapping `working` edit before applying the pending one as a `Transaction`.


## Save semantics

Save is `commit_working` (per `op-log.md`'s "Disk write invariant"): it folds the `working` layer into `accepted` and writes the result to disk per [[spec:op-log-atomic-write]]. Pending edits are untouched — they stay in the pending set and continue to show as hunks; the save path can't carry one to disk because pending lives outside both `working` and `accepted`. No pending edit reaches disk without being explicitly accepted (which first folds it into `working`).

Reject is independent of save — it only mutates the pending set in `.hiker/pending/`; disk content is unchanged.


## Drift

A pending hunk can become inconsistent with the current accepted state — e.g., an anchored range whose base context (per [[spec:op-log-op-shape]]) points at a body region a later user edit has changed. Drift is a re-diff check: re-derive the hunks against current `accepted` and verify the hunk's base context still matches exactly; an exact mismatch means the hunk is *drifted* (per [[spec:op-log-drift]]).

- **Drifted hunks** surface in the file pill's `(M drifted)` suffix. Click expands a popover listing each with `[Reject]` and `View` (opens the hunk's proposed content in diff mode against `Empty`). [patch-review-conflicted-hunk-display]
status:: planned
note:: conflicted proposals (hydration-time apply failures, or disk-side state from `staging-drift-eager-recheck` before hydration) surface via the file-pill's `(M conflicted)` suffix. They don't render as hunks in the inline view
- **In the file pill popover** and the activity-detail page, Accept is greyed for drifted hunks with the drift reason as tooltip. Reject stays active. [patch-review-conflicted-accept-disabled]
status:: partial
note:: inline hunk overlay: `diff_overlay.rs::attach_agent_hunk_widgets` greys the per-hunk Accept button (`enabled: false`) and labels the row `Hunk N (drifted)` when any contributing op is drifted (`OpLog::is_pending_drifted`); Reject stays active. The file pill surfaces the `(M drifted)` count. Gap: the file-pill popover + `Changes`-tab drift-reason tooltip are still planned
- **Auto-reject on drift** is opt-in via `[op-log] auto_reject_on_drift` per `op-log.md`'s config section. When set, drifted hunks are dropped from the pending set immediately rather than surfacing.


## File pill

A thin strip directly below the editor toolbar whenever the active buffer has `agent_base.is_some()`. [patch-review-file-pill]
status:: done
touches:: [[code:hiker/panels/buffer/patch_review_pill]]
note:: `app/src/panels/buffer/patch_review_pill.rs` consumes `Vec<HunkInfo>` from the diff overlay; renders `N agent hunks (M drifted)` + Accept-all / Reject-all / Next-hunk + per-session rows. Bulk verbs flip pending ops through `op_writes::flip_op_status` (Accept-all skips drifted, Reject-all covers them); the next-frame editor binding re-materializes via `editor_binding::materialize_review`. Drifted count from `is_pending_drifted`

- **Label.** `N hunks` from the live `DiffLayer`. Adds `(M drifted)` suffix when drifted hunks exist for the active session on this document.
- **Accept all.** Sequentially accepts every hunk into `working`.
- **Reject all.** Sequentially rejects every pending hunk for the session (covers drifted ones too). Always confirms.
- **Next hunk.** Scrolls to the next hunk by document order, wrapping.
- **Visual family.** Same minimal chrome as [[spec:write-note-pending-banner]]; muted background tint, single-line height.


## Whole-file and structured proposal review

Some proposals don't compose into the per-hunk view:

- **Whole-body rewrite** (the shape `write_note` MCP calls emit). [write-note-review-surface]
status:: done
implements:: [[code:hiker/oplog/impl#[OpLog]materialize_with_pending_op]], [[code:hiker/ops/op_writes/WholeFileProposal]], [[code:hiker/ops/op_writes/list_whole_file_proposals]], [[code:hiker/ops/op_writes/PendingProposal]], [[code:hiker/ops/op_writes/list_pending_proposals]], [[code:hiker/ops/op_writes/pending_op_count]], [[code:hiker/ops/op_writes/proposal_materializations]], [[code:hiker/panels/buffer/impl#[AppState]accept_staging_proposal]], [[code:hiker/panels/buffer/impl#[AppState]reject_staging_proposal]]
touches:: [[code:hiker/panels/patch_review]]
note:: whole-file proposals are pending whole-body `Replace{anchor:None}` / `Create` edits listed via `op_writes::list_whole_file_proposals` (per-frame cache `ui_cache.whole_file_proposals`). Open as `TabKind::Editor { buffer: PendingProposal{ proposal_id, target_path }, diff: Some(Disk{target_path}) }`; `editor_pane::ensure_readonly_buffer_loaded` + `diff_overlay::resolve_source_text` load the proposed content via `op_writes::proposal_materializations`. Accept/reject route through `op_writes::flip_op_status`
- **Frontmatter patches.**
- **New-note proposals** for paths that don't yet exist on disk.

These open the editor tab in diff mode: the buffer holds the proposed content read-only; the diff layer shows `base = materialize(accepted + working)` (or `Empty` for a new-note proposal against a non-existent path), `current = proposed content`. Toolbar mode-controls slot carries `Review rewrite` / `Review new note` plus Accept / Reject verbs. [write-note-review-mode-label]
status:: planned
note:: label reads `Review rewrite` (target path exists on disk) vs. `Review new note` (target path doesn't), with muted origin suffix (`· chat` / `· batch` / `· trail`)

- **Accept** folds the proposal into `working` (preserving the rest of `working` like the inline path), committed on the next Save. A whole-body rewrite replaces the body region; where it overlaps uncommitted `working` edits, the overlap surfaces as a conflict hunk with **Keep mine / Keep theirs / Keep both** (per [[spec:op-log-merge-conflict]]); disjoint `working` edits merge automatically. Then navigates to the target note as a preview tab. [write-note-review-blocks-on-dirty, staging-accept-navigates-to-preview]
- **Drifted whole-file proposals.** Accept disabled with reason as tooltip; proposed content still renders. Reject works. [write-note-review-conflicted-display]
status:: done
note:: drift is read off the op log (`OpLog::is_pending_drifted`, surfaced on `WholeFileProposal.drifted`). Accept is disabled with a drift reason in the tooltip on both the whole-file review toolbar (`render_readonly_source_toolbar`) and the pending-rewrite banner (`pending_rewrite_banner`); proposed content still renders; Reject + View stay active


## Open routing and precedence

On `openFile(rel)`, the whole-file review surface takes precedence over plain editing: a path with at least one pending whole-file proposal opens in the whole-file surface (most recent by `timestamp_ms`; older ones via the status-bar version dropdown), while a path with only hunk-shaped pending edits opens in plain editing with the inline diff layer and file pill. When both shapes exist, accepting/rejecting the whole-file surface returns to plain editing where the hunk-shaped edits remain. [note-open-routes-to-pending-review]
status:: planned
note:: when `openFile` resolves a path with at least one pending whole-file proposal, the open lands in the whole-file review surface (per [[spec:write-note-review-surface]]); pending `edit_note` proposals trigger hydration into plain editing

When a tab is in plain editing but its path has a pending whole-file proposal, a thin **Pending-rewrite banner** above the editor reads `Pending rewrite for this note` (or `Pending new-note proposal` when the path doesn't exist) with a `Review` button into the whole-file surface; it stacks above the inline file pill when both shapes are pending. [write-note-pending-banner]
status:: planned
note:: thin amber strip above the editor when a buffer tab is in plain editing and its path has a pending whole-file proposal. `Review` button switches the tab into the whole-file review surface


## Module placement

- `core::oplog` — `ops_in_range(path, session, range)`, the pending verbs (`stage_pending` / `accept_pending` / `reject_pending`), materialization. Owns the substrate per `op-log.md`.
- `core::ops` — high-level write wrappers (`write_file`, `agent_edit_note`, `flip_op_status`). The host calls these; nothing in `app/` reaches into `core::oplog` directly.
- `app/src/panels/editor/` — the unified editor tab body. Owns the diff toolbar toggle, source picker, "Show changes" menu, hunk overlay widget dispatch (per `DiffOwner`).
- `app/src/panels/editor/patch_review.rs` — buffer state mapping (`agent_base`, `current`, `active_session`); per-hunk accept/reject dispatch that maps hunks → contributing pending edits → the `ChangeSet`/`Transaction` fold (accept) or pending-set drop (reject).
- `app/src/panels/editor/patch_review_pill.rs` — the file pill component.
- `app/src/panels/changes.rs` — unified Changes tab: lists pending edits (with drifted status) and accepted history. The activity-management surface.
- `editor::diff` — `DiffLayer` primitive, decoration emission, overlay widget hooks. Owner-aware widget content injected by the consumer via a render callback.


## Editor integration

- The editor widget remains editable for any buffer with `agent_base.is_some()`. Decorations are visual; user input is unaffected.
- The editor exposes its change sets (per [[spec:editor-transactions-out]]): each user `Transaction`'s `ChangeSet` is what `map_pos` remaps the session's anchored ranges through, and is what the editor-native overlay (per [[spec:op-log-three-way-overlay]]) consumes to recompute the diff. The overlay is editor-native, not CRDT.
- `DiffLayer` decorations are pushed onto the same decoration stack as markdown styling, wikilinks, etc. View-zone insertions for removed lines participate in line-height calculation via the existing `DecorationLayers::push_with_heights` path.
- Hunk overlay widgets use the existing clickable-widget primitive (`ClickAction::WidgetClick`); widget ids namespaced from `WIDGET_ID_BASE`.
- Cursor and selection are unaffected by diff recomputation. Frame-level diff cost is bounded by `editor_core::diff::lines(agent_base, current)` over the in-memory ropes — cheap for note-sized files; no incremental diff cache required.


## Pane integration

The whole-file review surface is identified by `Editor { buffer, diff: Some(PendingProposal { .. }) }`. Inline patch-review is identified by `agent_base.is_some()` on the buffer; the tab itself is plain `Editor { buffer, diff: None }`. No new tab kinds.

Navigation history:

- Entering the whole-file review surface pushes onto history.
- Per-hunk accept/reject in the inline view does *not* push — it's a plain-editing operation on the live buffer.
- Exiting the whole-file review surface pops back to plain editing on the same path.


## Multi-session

When more than one agent session has pending edits on the same document, the file pill shows one row per session: `Session foo: 3 hunks` / `Session bar: 1 hunk`. Clicking a row sets `active_session` and recomputes `current` against just that session's edits. The "diff against my session's view" property the agent enjoys (per `op-log.md`'s agent-session-view story) is per-session by construction — each session has its own session text and derives its own hunks. [patch-review-multi-session]
status:: done
implements:: [[code:hiker/panels/buffer/patch_review/impl#[`BufCtx<'_>`]pill_meta]]
touches:: [[code:hiker/panels/buffer/patch_review]]
note:: `patch_review.rs::pill_meta` tallies pending ops per `session_id` off `OpLog::pending_ops`; when >1 session has pending ops the pill (`patch_review_pill.rs`) renders one selectable `Session <id>: N hunks` row each. Clicking sets `Buffer.active_session` and re-materializes the pending-view scoped to that session (`review_materializations` / `ops_in_hunk` take the session)


## Out of scope (this surface)

- **Cross-file proposal review.** Each inline view is scoped to the active buffer's document. Multi-file workflows (rename a function + update call sites) are N independent pending streams; aggregation across documents is the activity-detail page's job. [patch-review-cross-file-deferred]
status:: planned
note:: multi-file `edit_note` workflows; depends on a future `multi_edit_note` tool surface
- **Rich three-way merge UI for whole-file accept-over-`working`.** Same-region overlap resolves through the per-hunk Keep mine / Keep theirs / Keep both conflict hunks (per [[spec:op-log-merge-conflict]]); a richer three-way merge view rides [[spec:diff-viewer-three-way]]. [patch-review-three-way-deferred]
status:: planned
note:: true three-way merge for whole-file accept-while-dirty; rides [[spec:diff-viewer-three-way]]
- **Per-agent-attribution badges within a hunk.** `metadata.batch_id` grouping covers the common case. [patch-review-per-agent-attribution-deferred]
status:: planned
note:: per-edit badges showing which agent issued which hunk; useful only with concurrent multi-agent proposals
- **Keyboard navigation between hunks beyond the pill's Next button.** [patch-review-hunk-keybind-deferred]
status:: planned
note:: global keybind for next/previous hunk in the active buffer; the file pill's Next button covers the immediate need


## Forward refs

- `op-log.md` — the substrate (layered model, pending overlay, drift, attribution).
- `diff.md` ([[spec:diff-layer]], [[spec:diff-as-mode]], [[spec:diff-source-enum]], [[spec:diff-layer-hunk-widgets]]) — the primitive and tab integration.
- `mcp.md` ([[spec:mcp-tool-edit-note]], [[spec:mcp-edit-note-validation]]) — the producer side. `edit_note` calls emit anchored-range pending edits per [[spec:op-log-op-shape]].
- `editor.md` ([[spec:tab-kinds]], [[spec:editor-toolbar-mode-controls]], [[spec:editor-show-changes-menu]], [[spec:status-bar-version-dropdown]]) — tab kinds, toolbar slot, status-bar dropdown.
- `op-log.md` "History" ([[spec:changes-query-api]], `activity-feed-*`) — the projection layer pending edits surface through.
- `settings.md` — the `[op-log]` config section (per [[spec:op-log-config-section]]).

## Registry imports (from status.md)

Entries imported from the retired status registry that had no anchor in this doc —
re-home them into the relevant sections as the doc evolves.

- **patch-review-buffer-state** — `app/src/panels/buffer/editor_binding.rs` runs `log.materialize_review(rel)` off `op_writes::review_materializations` → sets `buffer.agent_proposal = review` and `agent_base = accepted`; clears `agent_base` to `None` when the two are equal (no pending ops in scope). Re-runs each frame the buffer is shown and after every flip. The `Buffer` carries `active_session` to scope the overlay [patch-review-buffer-state]
  status:: done
  touches:: [[code:hiker/panels/buffer/editor_binding]]
- **patch-review-diff-layer** — `app/src/panels/buffer/diff_overlay.rs::compute` builds `DiffLayer { base: agent_base, current, owner: Agent }` and pushes decorations onto the editor's decoration stack. Same primitive powers diff toggle and snapshot/history viewer [patch-review-diff-layer]
  status:: done
  touches:: [[code:hiker/panels/buffer/diff_overlay]]
- **patch-review-conflicted-list-popover** — clicking the `(M conflicted)` suffix on the file pill opens a popover listing each conflicted proposal with `[Reject]` and `View` (opens the proposal in diff mode against `Empty`) [patch-review-conflicted-list-popover]
  status:: planned
- **patch-review-decorations** — umbrella slug for the inline rendering of pending edits. Implementation now rides [[spec:patch-review-diff-layer]] rather than a custom decoration provider [patch-review-decorations]
  status:: planned
- **write-note-review-blocks-on-dirty** — Accept refuses with modal when the live buffer for the target path is dirty. Reject works regardless [write-note-review-blocks-on-dirty]
  status:: planned
- **editor-action-row-primitive** — `BlockKind::ActionRow { label, glyph, tone, buttons }` in `editor-core::decoration` with `ActionButton`, `ActionButtonStyle` (Primary/Danger/Neutral), `ActionTone` (Normal/Warning/Conflicted). Rendered by `paint_action_row` in `editor-egui`; each enabled button registers a per-rect `ClickZone` with `ClickAction::WidgetClick(button.id)`. Pure-data primitive used by [[spec:diff-layer-hunk-widgets]] [editor-action-row-primitive]
  status:: done
- **editor-inline-widget-display** — `InlineWidget` trait carries `fn display() -> Option<InlineWidgetDisplay>`. When `Some`, the egui painter renders the supplied `{ text, bg, fg, strikethrough }` as the segment galley + bg fill instead of the bordered placeholder [editor-inline-widget-display]
  status:: done
- **editor-end-of-line-widget** — `build_line_layout` collects `InlineWidget` decorations whose `range.start == line_byte_end` into a trailing-segment list rather than clipping them to zero width [editor-end-of-line-widget]
  status:: done
