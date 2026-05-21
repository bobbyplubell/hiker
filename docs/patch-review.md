# Patch review

In-editor surface for reviewing pending agent-staged content changes. Built on the `DiffLayer` primitive from `diff.md`: every pending proposal for an open buffer manifests as hunks in a diff between the buffer's `agent_base` (disk content at hydration) and the buffer's live text. Accept/reject acts on the diff, not on an anchor.

The headline decisions:

- **Buffer hydration absorbs pending proposals on open.** When an editor tab opens a path that has pending `edit_note` proposals in `staging.db`, the buffer applies them in order to the disk content and snapshots the pre-apply disk text as `agent_base`. The live buffer text *is* "disk + pending proposals." [patch-review-buffer-hydration]
- **Inline review is `DiffLayer { base: agent_base, current: buffer, owner: Agent }`.** The same primitive that powers snapshot diff and history diff renders the pending edits. No separate anchor tracker; the diff recomputes from the two ropes each frame. [patch-review-diff-layer]
- **Per-hunk accept/reject mutates the diff.** Accept advances `agent_base` toward `current` for the hunk's range. Reject splices `agent_base` back into `current` for the hunk's range. Both shrink the diff. Both resolve the underlying `staging.db` proposal(s) on the same dispatch: accept removes the proposal and appends a `changes.db` row tagged `author='agent:<client-id>'`; reject removes the proposal. [patch-review-per-hunk-accept]
- **`staging.db` is cold storage; the buffer is hot storage.** Proposals persist in `staging.db` while no tab is open for the path. Opening the tab hydrates into the buffer. Closing the tab dehydrates: any unresolved hunks correspond to proposals still in `staging.db`; the buffer state is discarded. Re-opening rehydrates fresh. [patch-review-hydrate-dehydrate]
- **User edits and agent edits coexist in the same `current`.** Typing into the live buffer updates `current`; the diff updates each frame. There is no "anchor conflict" state — if the user types over an agent-proposed range, the hunk's `current` side simply reflects both. Accept means "the current state is what I want"; reject means "throw away everything in this range and revert to `agent_base`." [patch-review-coexisting-edits]
- **The file pill carries bulk verbs and hunk navigation.** Thin strip above the editor when `agent_base.is_some()`: `N hunks (M conflicted)` plus `[Accept all] [Reject all] [Next hunk]`. Same chrome family as the write-note pending banner. [patch-review-file-pill]
- **`write_note`-shaped proposals open in diff mode against the live file.** Whole-file proposals don't compose with per-hunk hydration; they open the editor tab with `diff = Some(StagingProposal(id))`, owner `Staging`, and Accept / Reject in the toolbar's mode-controls slot. Hydration applies only to `edit_note` proposals. [write-note-review-surface]


## Buffer state

```rust
pub struct Buffer {
    pub current: Rope,                       // live editable text
    pub loaded_content: Rope,                // disk text at last save/load
    pub agent_base: Option<Rope>,            // disk text at hydration; None unless proposals applied
    pub hydrated_proposals: Vec<ProposalId>, // proposals whose edits were applied at hydration
    // ...other existing fields
}
```

`agent_base` is set only when at least one pending `edit_note` proposal was applied at open. It feeds the inline `DiffLayer`. It clears when every hydrated proposal is resolved or when the buffer closes.

`loaded_content` retains its role as the save-path base: drift detection (`pre-write-drift-check`), dirty-flag derivation, autosave comparisons all key off `loaded_content`, not `agent_base`.

Accept updates `agent_base` toward `current` for the hunk's range and resolves the contributing proposals. Reject reverts `current` toward `agent_base` for the hunk's range and resolves the contributing proposals. Neither path touches `loaded_content` — the user's subsequent save dispatches through `vault.write_file_checked` against `loaded_content` as usual.


## Hydration

`openFile(rel, opts)` for a path with pending `edit_note` proposals:

1. Read disk → `disk_text`.
2. Set `agent_base = Some(disk_text.clone())`. Set `loaded_content = disk_text.clone()`.
3. For each pending `edit_note` proposal in `staging.db` ordered by `created_at`: apply the proposal's edit to the running text using `core::patch::apply`. Record the proposal id in `hydrated_proposals`. If apply fails, the proposal is skipped and marked `conflicted` in `staging.db`; hydration continues with the rest.
4. Set `current` to the post-apply text.

The buffer is now "dirty against disk" by virtue of containing the pending agent edits; the standard dirty marker reflects this. The user saves to write the hydrated state to disk; this acts as a bulk accept of every applied proposal *only if* every proposal has already been individually accepted (see Save semantics below).

`write_note` / `set_frontmatter` / `apply_tag` proposals do not participate in hydration. They route to the diff-mode review surface (see "Whole-file review" below).


## Inline rendering

When `agent_base.is_some()` and the active tab is the buffer:

- The editor pane pushes a `DiffLayer { base: agent_base, current, owner: Agent }` onto the buffer's decoration stack. The layer's emitted decorations — pale-red lines for removed text (the `agent_base` content displayed via view zones above its successor in `current`), pale-green lines for added text, intraline marks if the View toggle is on (per `diff-viewer-intraline`) — render inline in the live editable buffer.
- The buffer remains fully editable. Typing into a hunk updates `current`; the diff recomputes; the hunk's shape shifts. The user can edit anywhere — inside, around, or unrelated to a hunk.
- Each hunk's overlay widget carries `Accept ✓` and `Reject ✗` icon buttons, rendered as inline widgets at the hunk's first visible line. Same widget primitive snapshot diff uses (per `diff-layer-hunk-widgets`). [patch-review-per-hunk-accept]
- Hunks originating from the same `batch_id` are connected by a thin gutter marker. No coupled behavior — each is independently accept/rejectable. [patch-review-batch-grouping]


## Accept / reject

Per-hunk verbs operate on the hunk's `(base_range, current_range)` mapping returned by `DiffLayer`.

**Accept:**
1. Determine which proposals contributed to the hunk's `base_range` — the proposals in `hydrated_proposals` whose applied edit overlaps `base_range`. For each: append a `changes.db` row tagged `author='agent:<client-id>'` with `metadata.staging_proposal_id` + `metadata.batch_id`; remove the proposal from `staging.db`; remove its id from `hydrated_proposals`.
2. Splice `current[current_range]` into `agent_base[base_range]`. The hunk disappears from the next diff recompute.

**Reject:**
1. Resolve the contributing proposals as in accept, but append no `changes.db` row. Remove them from `staging.db` and from `hydrated_proposals`.
2. Splice `agent_base[base_range]` into `current[current_range]`. The hunk disappears from the next diff recompute. User edits that fell inside `current_range` are discarded along with the agent's.

When the last hydrated proposal is resolved, `agent_base` clears to `None` and the file pill disappears. The buffer remains dirty against `loaded_content` until the user saves.

**Accept-all / Reject-all** apply the per-hunk verbs sequentially across every applyable hunk in one batched dispatch. Conflicted proposals are skipped by Accept-all (they aren't in the inline view) and removed by Reject-all.


## Save semantics

Saving a buffer with `agent_base.is_some()` requires that every hydrated proposal has been individually accepted: if `hydrated_proposals` is non-empty, the save refuses with a modal — "This buffer has unresolved agent proposals. Accept or reject each hunk before saving, or use Accept all." Reject is allowed on individual hunks regardless of dirty state; only the save-to-disk verb is gated.

This avoids the silent "the user saved without reviewing → agent edits land on disk without a `changes.db` audit row" failure mode. The Accept verb is what writes `changes.db`; bypassing it would corrupt history.

When `hydrated_proposals` is empty (every proposal was accepted or rejected), the save path is identical to any other dirty buffer — write through `vault.write_file_checked`, advance `loaded_content`. Reject does not require a save; it only mutates in-memory state plus removes the proposal from `staging.db`. The buffer can stay dirty after rejects if the user has other unsaved edits.


## Conflicted proposals

A proposal becomes `conflicted` only at hydration time, when its edit can't be applied to the partially-applied disk text. Once hydrated, a proposal is reified as part of the diff and no longer has an independent anchor that can become stale.

Conflicted proposals are surfaced two ways:

- **File pill suffix `(M conflicted)`.** Click expands a popover listing each conflicted proposal with `[Reject]` and `View` (opens the proposal in diff mode against `Empty`). [patch-review-conflicted-hunk-display]
- **Bulk `Changes` tab.** Conflicted proposals show as a distinct row class with the reason inline. Reject works; Accept is greyed. [patch-review-conflicted-accept-disabled]

Auto-reject on conflict is opt-in via `[staging].auto_reject_on_conflict`. When set, hydration removes conflicted proposals from `staging.db` without surfacing them.


## Dehydration

Closing the editor tab discards `current` and `agent_base`. Unresolved hunks correspond to proposals still present in `staging.db`; they survive intact. Re-opening the path rehydrates from `staging.db` and produces the same diff (modulo any concurrent disk changes that arrive in the meantime via `staging-drift-eager-recheck`, which still owns the disk-side state machine for proposals between creation and hydration).

User edits that the user made inside the buffer but didn't save are lost on close — same behavior as any unsaved buffer. The close-dirty guard fires first; the user dismisses it explicitly.


## File pill

A thin strip directly below the editor toolbar whenever the active buffer has `agent_base.is_some()`. [patch-review-file-pill]

- **Label.** `N hunks` from the live `DiffLayer`. Adds `(M conflicted)` suffix when conflicted proposals exist for this path.
- **Accept all.** Sequentially accepts every hunk. Confirm dialog when N > 5.
- **Reject all.** Sequentially rejects every hunk and removes conflicted proposals' staging entries. Always confirms — reject discards agent work.
- **Next hunk.** Scrolls to the next hunk by document order, wrapping. Cursor lands on the hunk's first line; selection unchanged.
- **Visual family.** Same minimal chrome as `write-note-pending-banner`; muted background tint, single-line height. Painted by the buffer panel above the editor widget.


## Whole-file review

`write_note` / `set_frontmatter` / `apply_tag` proposals don't compose into the inline view — there's no per-hunk story for a whole-file rewrite. They open the editor tab in diff mode: [write-note-review-surface]

- **Tab state.** `Editor { buffer: <ephemeral proposal buffer>, diff: Some(StagingProposal(id)) }`. The buffer holds the proposed full-file content read-only; the diff layer shows `base = current disk text` (or `Empty` if the path doesn't exist), `current = proposal content`.
- **Toolbar mode-controls slot.** Label `Review rewrite` (target path exists) or `Review new note` (target path doesn't), plus muted origin suffix (`· chat` / `· batch` / `· trail`). Verbs: Accept, Reject. The diff toggle exits to the proposed content rendered plain. [write-note-review-mode-label]
- **Accept blocks while the live buffer for the path (if any) is dirty.** Modal: "Your buffer has unsaved changes. Save or revert before accepting this rewrite." Reject works regardless. Whole-file accept has no anchor to compose with the user's edits. [write-note-review-blocks-on-dirty]
- **Accept navigates to the freshly-written note** via `editor_pane::open_file(target, sticky=true)` after the staging-preview tab closes. [staging-accept-navigates-to-preview]
- **Conflicted whole-file proposals.** Accept is disabled with a tooltip naming the reason; the proposed content still renders so the user can see it. Reject works. [write-note-review-conflicted-display]


## Auto-routing on open

When `openFile(rel, opts)` resolves a path that has at least one pending whole-file proposal (`write_note` / `set_frontmatter` / `apply_tag`), the open lands in the whole-file review surface rather than plain editing. The most recent proposal by `created_at` is the one shown; older proposals are accessible via the status-bar version dropdown. [note-open-routes-to-pending-review]

When a path has pending `edit_note` proposals but no whole-file proposal, the open lands in plain editing with hydration applied — the diff and file pill are visible, the buffer is editable.

When both kinds exist, the whole-file surface takes precedence on open. Accepting / rejecting it returns the user to plain editing where the `edit_note` hunks remain.


## Pending-rewrite banner

When a buffer tab is in plain editing (not the whole-file review surface) and its path has at least one pending whole-file proposal, a thin banner above the editor reads `Pending rewrite for this note` (or `Pending new-note proposal` when the target path doesn't exist), with a `Review` button that switches the tab into the whole-file review surface. [write-note-pending-banner]

Stacks with the edit-note file pill — a path with both pending kinds renders pill above banner.


## Module placement

- **`core::staging`** — owns `list_pending(path)`, `accept(id)`, `reject(id)`, `recheck_disk(path)` (the eager-recheck path for drift between proposal creation and hydration per `staging-drift-eager-recheck`).
- **`core::patch`** — `apply(text, &Edit) -> Result<String, PatchError>`. Used by hydration only; not called per-frame.
- **`app/src/panels/editor/`** — the unified editor tab body. Owns the diff toolbar toggle, source picker, "Show changes" menu, hunk overlay widget dispatch (per `DiffOwner`).
- **`app/src/panels/editor/patch_review.rs`** — hydration + dehydration helpers; per-hunk accept/reject dispatch that maps hunks → contributing proposals → `staging.db` mutations + `changes.db` appends.
- **`app/src/panels/editor/patch_review_pill.rs`** — the file pill component.
- **`app/src/panels/changes.rs`** — the unified `Changes` tab. Lists pending proposals (with conflicted status) and committed `changes.db` rows. The cold-storage management surface.
- **`editor::diff`** — `DiffLayer` primitive, decoration emission, overlay widget hooks. Owner-aware widget content is injected by the consumer (the editor panel) via a render callback.


## Editor integration

- The editor widget remains `ViewState::read_only = false` for any buffer with `agent_base.is_some()`. Decorations are visual; user input is unaffected.
- `DiffLayer` decorations are pushed to the same decoration stack as markdown styling, wikilinks, etc. View-zone insertions for removed lines participate in line-height calculation via the existing `DecorationLayers::push_with_heights` path.
- Hunk overlay widgets use the existing clickable-widget primitive (`ClickAction::WidgetClick`); widget ids are namespaced from `WIDGET_ID_BASE` so they can't collide with fold ids.
- Cursor and selection are unaffected by diff recomputation. Frame-level diff cost is bounded by `core::diff::compute(agent_base, current)` over the in-memory ropes — cheap for note-sized files; no incremental diff cache required.


## Pane integration

The whole-file review surface is identified by `Editor { buffer, diff: Some(StagingProposal(_)) }`. Inline patch-review is identified by `agent_base.is_some()` on the buffer state; the tab itself is plain `Editor { buffer, diff: None }`. No new tab kinds.

Navigation history (`navigation-history-stack`):

- Entering the whole-file review surface pushes onto history.
- Per-hunk accept/reject in the inline view does *not* push — it's a plain-editing operation on the live buffer.
- Exiting the whole-file review surface pops back to plain editing on the same path.


## Out of scope (this surface)

- **Cross-file proposal review.** Each inline view is scoped to the active buffer's file. Multi-file `edit_note` workflows aren't supported by the tool itself; a future `multi_edit_note` would change the math. [patch-review-cross-file-deferred]
- **Three-way merge for whole-file accept-while-dirty.** Blocked-and-tell is the posture. Real three-way merge rides `diff-viewer-three-way`. [patch-review-three-way-deferred]
- **Per-edit attribution badges showing which agent issued which hunk.** Existing `batch_id` grouping covers the common case. [patch-review-per-agent-attribution-deferred]
- **Keyboard navigation between hunks beyond the pill's Next button.** [patch-review-hunk-keybind-deferred]
- **Separating user edits from agent edits in the same hunk.** Hunks reflect `agent_base` vs `current`; mixed user+agent edits land in one hunk. A "blame within a hunk" view is conceivable but not pursued.


## Forward refs

- `diff.md` (`diff-layer`, `diff-as-mode`, `diff-source-enum`, `diff-layer-hunk-widgets`) — the primitive and tab integration.
- `mcp.md` (`mcp-tool-edit-note`, `mcp-edit-note-validation`) — the producer side.
- `settings.md` (`staging-per-edit-proposals`, `staging-proposal-state`, `staging-drift-eager-recheck`, `staging-auto-reject-on-conflict`, `staging-config-section`) — the staging substrate.
- `editor.md` (`tab-kinds`, `editor-toolbar-mode-controls`, `editor-show-changes-menu`, `status-bar-version-dropdown`) — tab kinds, toolbar slot, status-bar dropdown.
- `changes.md` (`changes-write-path`) — accepted edits append rows tagged `author='agent:<client-id>'` with `metadata.batch_id`.
