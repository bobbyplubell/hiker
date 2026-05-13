# Patch review

In-editor surface for reviewing agent-staged content changes. Two flavors riding the same toolbar affordances and mode-controls slot: *patch-review mode* for span-anchored `edit_note` proposals (per-hunk accept/reject with inline overlays on the live file), and *write-note review mode* for full-file `write_note` proposals (read-only buffer with diff toggle, proposal-level accept/reject). The two flavors are entered through the same agent-diff toolbar toggle and share the staging-state machinery from `settings.md`.

The headline decisions:

- **One new editor mode for per-hunk review (`patch-review-mode`).** CM6 view stays on the live on-disk file; pending `edit_note` hunks render as widget decorations (struck-through original lines for deletions, inserted green-tinted lines for additions) with per-hunk gutter accept/reject buttons. Mode-controls slot carries Accept-all / Reject-all. Exiting the mode is the agent-diff toolbar toggle's job — it doubles as the entry and exit affordance, so a redundant Exit verb in the slot would just clutter it. [patch-review-mode]
- **Toolbar gains a second diff toggle.** The existing dirty-buffer Diff toggle (`editor-diff-vs-disk-toggle`) gets a small user badge in the corner; a new agent-diff toggle with a robot badge sits next to it. The agent toggle is the entry point into patch-review mode. Both toggles use the same icon family for muscle memory and are mutually exclusive at runtime — turning one on turns the other off. [patch-review-agent-diff-toggle, patch-review-toggles-mutually-exclusive]
- **`edit_note` accept composes with dirty buffers via a transactional patch apply.** Accept applies the same span-anchored patch to both disk and the in-memory buffer in one move; if the user's edits have clobbered the patch's anchor in the buffer, accept refuses with a clear message rather than silently merging. Disk gets `apply(edit, current_disk)`; buffer gets `apply(edit, current_buffer)`; both succeed or neither does. [patch-review-dirty-buffer-transactional-accept]
- **Conflicted proposals stay visible.** When the staging-side re-check (per `staging-drift-eager-recheck`) flips a proposal to `conflicted`, its hunk renders greyed with a warning glyph and a tooltip naming the conflict reason. Accept on that hunk is disabled; Reject still works. Auto-reject is opt-in via `[staging].auto_reject_on_conflict` (per `staging-auto-reject-on-conflict`). [patch-review-conflicted-hunk-display]
- **Unanchorable hunks pin to end-of-file.** A proposal can be `applyable` on the staging side (disk's anchor still resolves) and yet have no place to render inline if the user's dirty buffer no longer contains `old_str`. Rather than silently drop the hunk, the view appends a single block widget at end-of-doc listing each unanchored proposal — `?` glyph, `new_str` preview, per-row Reject. No Accept (nothing to anchor to). [patch-review-unanchored-hunk-pin]
- **`write_note` proposals get a parallel "rewrite / new-note review" surface.** Whole-file proposals don't compose with the live file the way patches do, so they open in a read-only buffer with the existing diff toggle, framed in the mode-controls label as "Review rewrite" or "Review new note" depending on whether the target path exists. Accept-while-dirty is blocked (per the existing `diff-viewer-respects-dirty-source` rule); the user resolves their dirty buffer first. [write-note-review-surface]
- **Opening a note with pending agent proposals enters review by default.** When the user clicks into a note whose path has at least one pending staging proposal (any of `edit_note` / `write_note` / `set_frontmatter` / `apply_tag`), `openFile` lands in the appropriate review mode instead of plain editing: patch-review mode when *any* `edit_note` proposals target the path; write-note review mode otherwise. Clicking the agent-diff toolbar toggle exits the review back to plain editing for that session without rejecting anything; the next open re-evaluates. The two diff toggles, the editor toolbar pill, and the status-bar version dropdown remain available as override surfaces. [note-open-routes-to-pending-review]
- **Plain editing shows a thin banner when a write-shaped proposal is pending for the active path.** One-line strip directly below the editor toolbar: `Pending rewrite for this note` (or `Pending new-note proposal` when the target path doesn't exist) plus a single `Review` button that enters write-note review mode. No accept / reject in the banner, no diff preview. Visible only when the active tab is a buffer tab in plain editing mode and the buffer's path matches the pending write-shaped proposal; hidden on every other tab (home, queue, settings) and on any other open buffer. [write-note-pending-banner]


## The two toolbar toggles

The editor toolbar's right-side cluster (per `panel-toggle-buttons` in `editor.md`) reserves two diff-toggle slots: [patch-review-toolbar-buttons]

- **User-diff toggle** — the existing `editor-diff-vs-disk-toggle`, repainted with a small user badge in the corner of the diff glyph. Behavior unchanged: greys when the buffer is clean or the file isn't on disk; click toggles between the live editable buffer and a read-only line-level diff (live buffer vs last-loaded content); right-click opens the target-picker menu.
- **Agent-diff toggle** — new icon, same diff glyph with a small robot badge in the corner. Greys when there are no pending `edit_note` proposals for the active file. Click enters patch-review mode against the union of all `edit_note` proposals targeting this path. Click again exits back to plain editing.

**Greying, not hiding.** Both toggles keep their position in the toolbar regardless of state. A greyed toggle renders with reduced opacity and a tooltip explaining why ("No unsaved edits to diff" / "No pending agent edits"). Hiding would shift surrounding icons every time a proposal lands or accepts, churning muscle memory. [patch-review-toggles-grey-when-empty]

**Mutual exclusion.** Turning the agent toggle on forces the user toggle off, and vice versa. Both toggles being on would mean overlaying interactive per-hunk affordances on top of a read-only diff doc — ambiguous about which decorations are clickable. The implementation: each toggle's on-click handler calls the other's `forceOff()` before flipping itself on. State changes route through `renderModeControls()` so the mode-controls slot rebuilds idempotently. [patch-review-toggles-mutually-exclusive]


## Patch-review mode

Entered by clicking the agent-diff toggle (or via any "Review change" affordance that targets an `edit_note`-shaped proposal — chat card row, tree context menu, activity detail row). [patch-review-mode]

### What the editor view shows

The CM6 view stays on the live on-disk file content (with the user's dirty edits if any). For each pending `edit_note` proposal whose `target_path` matches the active buffer, the renderer emits a CM6 widget block at the proposal's anchor location:

- **Original-span lines** (matched by `old_str`) get a strike-through line decoration plus pale-red background.
- **New-span lines** (from `new_str`) are inserted *below* the original span as widget block lines with pale-green background and a "+" gutter marker. They are non-editable; click-through goes through to the live document underneath them.
- **Gutter buttons.** Each hunk gets two icon-only buttons in the gutter at the hunk's top line: Accept (check) and Reject (×). Same affordance family as the existing chunk-boundary gutter widgets (`view-show-chunk-boundaries`). Per-hunk accept/reject calls `core::staging::accept(id)` / `reject(id)` for that proposal alone. [patch-review-per-hunk-accept]
- **Hunk grouping by `batch_id`.** When multiple proposals share a `batch_id` (one originating `edit_note` tool call split into N), a thin connecting marker in the gutter visually associates them. No behavior coupling — each is still independently accept/rejectable — but the visual hint tells the reviewer "these came together as one agent intent." [patch-review-batch-grouping]

The editor is otherwise quiescent: typing is suppressed (the view is `EditorState.readOnly.of(true)` during patch-review mode), search and selection still work for reading. The mode is read + decide, not edit. Exiting the mode returns the view to plain editing in the same state the user left it. [patch-review-readonly-while-active]

### Mode-controls slot

While patch-review mode is active, the `#mode-controls` slot (per `editor-toolbar-mode-controls`) renders: [patch-review-mode-controls]

- **Label** — `Review agent edits · <N hunks>` where N counts only `applyable` proposals (conflicted ones are excluded from the verb count but still rendered).
- **Accept all** — calls `core::staging::accept_all({ by_path, state: applyable })`. Skips conflicted proposals. Confirm dialog when N > 5 to avoid accidental bulk-accepts.
- **Reject all** — calls `core::staging::reject_all({ by_path })`. Confirms regardless of count — Reject is destructive of agent work.

Exiting patch-review mode is done by clicking the (pressed) agent-diff toolbar toggle. No separate Exit verb in the slot — the toggle already serves as a toggle, and a second exit affordance was redundant chrome.

The per-hunk gutter buttons and the slot-level Accept-all / Reject-all share the same staging API; the slot-level verbs are just sugar over a filter.

### Conflicted-proposal display

A proposal whose `state == "conflicted"` (per `staging-proposal-state`) renders distinctly in patch-review mode: [patch-review-conflicted-hunk-display]

- The original-span line decoration switches from strike-through-red to **dashed-grey outline** so the user can still see which span the agent intended to touch.
- The new-span widget block renders **muted** (lower opacity) and is annotated with a small warning glyph plus a tooltip naming the conflict reason (`anchor_missing`, `anchor_not_unique`, `target_missing`, `hash_changed`).
- The gutter Accept button is **disabled** with a tooltip explaining why ("Anchor lost — your edits no longer contain the text the agent wanted to change"). Reject stays active. [patch-review-conflicted-accept-disabled]
- The label in `#mode-controls` adds a "(M conflicted)" suffix when M > 0.

When `[staging].auto_reject_on_conflict = true`, conflicted state transitions auto-reject the proposal before the user ever sees a conflicted hunk; the display rules above only apply to proposals whose flag was off when they transitioned.

### Unanchored-hunk pin

Distinct from the staging-side `conflicted` flag: a proposal whose `old_str` doesn't match the *current buffer text* has no place to render inline, even when the staging side still considers it `applyable` (disk content hasn't drifted, the agent edit is still valid on disk — the user's dirty buffer is what diverged). The fix is to pin those hunks at the bottom of the view rather than silently omit them. [patch-review-unanchored-hunk-pin]

- One block-level widget anchored at end-of-doc collects every unanchored proposal for the active path; it rebuilds whenever the proposal snapshot changes (on `hiker:staging-changed`) or when the doc changes (so a buffer revert lifts pinned hunks back into the inline view).
- Each row collapses by default and expands on click. [patch-review-unanchored-hunk-expand]
  - **Collapsed row:** a `?` glyph (tooltip names the reason — `anchor_not_in_buffer` for the buffer-only case, or the staging conflict reason when the proposal is *also* `conflicted`), a single-line preview of `new_str` truncated to ~80 chars (full text in `title`), a small chevron affordance indicating expandability, and a Reject button. No Accept — there's no in-buffer span to compose against.
  - **Expanded row:** the collapsed header stays visible (chevron rotates to "open" state) and the row reveals two labeled blocks underneath — `Anchor` showing the full `old_str` and `Replacement` showing the full `new_str`. Both render as pre-wrapped monospace text so whitespace and newlines are visible; this is the same content the agent intended to match against and substitute. Empty `new_str` (a pure deletion) renders as a muted `(empty)` marker so the user can distinguish "deletes the anchor" from "no replacement field." Empty `old_str` should not occur (validated upstream) but renders the same way for safety.
  - **Toggle scope.** Click anywhere on the collapsed header row (glyph, preview, or chevron) toggles expansion. The Reject button stops propagation so clicking it doesn't also expand. Expansion state is per-row and persists across decoration rebuilds within the same review session (proposal snapshot updates and doc edits don't collapse rows the user opened); it resets when patch-review mode exits or the buffer closes. Rows whose proposal is removed from the snapshot (accepted or rejected elsewhere) shed their expansion state automatically.
- Block header: `Unanchored agent edits (K) — your buffer no longer contains the text these edits target`. The user's options are spelled out by the surface itself: Reject the row, expand to inspect the anchor and replacement the agent intended, or resolve the buffer (save / revert) so the next snapshot re-resolves the anchor and the hunk lifts into the inline view.
- The `#mode-controls` label is unchanged; the pinned block carries its own count. Adding a `(K unanchored)` suffix would lie on buffer edits since the slot only rebuilds on staging changes, not doc changes.


## Accept-while-dirty: transactional patch apply

The interesting case: the user has unsaved edits in the buffer and accepts an `edit_note` proposal whose anchor still matches against the buffer. The accept must update both surfaces — disk and the in-memory buffer — coherently or refuse. [patch-review-dirty-buffer-transactional-accept]

Flow for `accept(id)` on an `edit_note` proposal:

1. **Compute `disk' = apply(edit, current_disk)`** using the proposal's `old_str` / `new_str`. Re-resolves the anchor against current disk content (lazy re-check per `staging-drift-eager-recheck`); on failure return `AcceptOutcome::Conflicted { reason }` without touching anything.
2. **Compute `buffer' = apply(edit, current_buffer)`** using the same `old_str`. Re-resolves against the dirty in-memory buffer.
   - If the buffer's anchor doesn't match (the user's edits clobbered the bytes the agent wanted to change), return `AcceptOutcome::AnchorConflict { reason: "user_edits_clobber_anchor" }`. The UI surfaces this as a non-destructive toast or modal: "Your edits conflict with this proposal — save or revert first to accept." Disk and buffer are both untouched; the proposal stays pending. [patch-review-anchor-conflict]
3. **If both succeed:** write `disk'` to disk via the existing `core::vault::write_file_checked` path. Append a `core::changes` row tagged `author='agent:<client-id>'` with `metadata.staging_proposal_id` + `metadata.batch_id` (when present). Remove staging files.
4. **Update the buffer transactionally.** In one CM6 dispatch: replace the buffer's text with `buffer'`, update `loadedContent = disk'`, update `loadedHash = hash(disk')`. The dirty flag re-derives from `hash(buffer') !== loadedHash`:
   - **Buffer was clean before accept** → `buffer' == disk'` → dirty flag clears (buffer matches the new loaded content).
   - **Buffer had user edits** → `buffer'` has user edits + agent edit, `disk'` has only the agent edit → still dirty.
   - The user's later save = standard `write_file_checked` against the updated `loadedHash`; standard `changes.db` row authored `user`. No drift modal, no clobbered work.
5. **Mode-controls + decorations refresh.** `renderModeControls()` rebuilds, `hiker:staging-changed` fires, the per-hunk decorations for this proposal disappear (proposal is gone from staging), remaining proposals stay in place. [patch-review-cm6-transactional]

**For other write tools (`write_note`, `set_frontmatter`, `apply_tag`):** the same compose-with-dirty machinery doesn't apply — there's no patch to apply to the buffer. `write_note` proposals open in write-note review mode (next section) which blocks accept while dirty. `set_frontmatter` / `apply_tag` are merge-into-frontmatter operations, not span replacements; their accept-while-dirty path is identical to `write_note`'s and uses the same write-note review surface. [patch-review-restricted-to-edit-note]


## Write-note review mode

`write_note`-shaped proposals (plus `set_frontmatter`, `apply_tag`, and any other whole-file write that lands in staging) open in *write-note review mode* — a read-only buffer view of the proposed content with the existing diff toggle. [write-note-review-surface]

### What the editor view shows

- The buffer's CM6 view is replaced with the proposal's full content as read-only text. Same machinery the existing `snapshot-preview-mode` uses.
- The mode-controls slot's label reads `Review new note` when the target path doesn't exist on disk, `Review rewrite` when it does, plus the surface origin (`· chat`, `· batch`, `· trail`) as a muted suffix. [write-note-review-mode-label]
- The mode-controls slot's verbs: **Diff toggle** (flips between proposed content and a unified diff against current disk, or against an empty buffer for new-note proposals), **Accept**, **Reject**. Same icon palette as the snapshot-preview verbs. Exit is via the agent-diff toolbar toggle (same rule as patch-review per `patch-review.md:52` — a separate Exit verb is redundant chrome).
- The agent-diff toolbar toggle is *pressed* (visually, this is a write-note review session, not edit_note patch-review). Click again to exit, same as patch-review.

### Accept-while-dirty: blocked

If the active buffer is dirty when the user clicks Accept, the accept refuses with a clear modal — "Your buffer has unsaved changes. Save or revert before accepting this rewrite." Same rule as `diff-viewer-respects-dirty-source`. Reject is allowed regardless of dirty state. [write-note-review-blocks-on-dirty]

Why blocked rather than transactional: whole-file rewrites have no anchor to compose with the user's edits. Merging would either drop the user's edits (silently) or produce an ambiguous merged content the user didn't ask for. Block-and-tell is the honest posture; the user resolves their buffer first.

### Conflicted-proposal display

Same flag as patch-review but rendered at the proposal level rather than per-hunk: the Accept verb in the mode-controls slot is disabled with a tooltip naming the conflict reason. The proposed content still renders (so the user can see what the agent wanted) but can't be applied as-is. Reject still works. [write-note-review-conflicted-display]

For `write_note` proposals, the conflict reason is typically `hash_changed` — the file's content drifted past propose-time because another write landed (user save, another agent write, accepted `edit_note`). The user's options are Reject + ask the agent to re-issue, or Reject + accept whatever they actually want manually.


## Auto-routing on open

When `openFile(rel, opts)` resolves a path that has one or more pending staging proposals (`core::staging::list_for_path(rel).len() > 0`), the open lands in a review mode rather than plain editing: [note-open-routes-to-pending-review]

- **Any `edit_note` proposal present** → patch-review mode is the landing state for the new buffer. The agent-diff toolbar toggle paints as pressed; the mode-controls slot renders the patch-review controls; CM6 is read-only per the standard patch-review rules. Hunks from every pending `edit_note` proposal targeting this path render together (including conflicted ones with the standard greyed display).
- **No `edit_note` proposals, but a `write_note` / `set_frontmatter` / `apply_tag` proposal present** → write-note review mode is the landing state. The most recent proposal by `created_at` is the one shown; older proposals against the same path are accessible via the status-bar version dropdown.
- **No pending proposals** → plain editing, as today.

Auto-routing respects the existing `openFile` preview-vs-sticky distinction: preview opens land in review-preview (closing the preview clears the review along with everything else); sticky opens land in review-sticky. The review state is part of `buffer.mode`, not a separate tab kind, so navigation history sees one entry (matching how snapshot-preview rides the same buffer).

**Show-live-file affordance.** Clicking the (pressed) agent-diff toolbar toggle exits the review for this open session and switches to plain editing on the same buffer. Doesn't accept or reject anything; the proposals stay pending and remain reachable via the two diff toggles, the editor toolbar pill, the status-bar version dropdown, and the activity-detail surface. The escape is per-open: re-opening the path from the tree re-enters review (the user did not signal "stop reviewing this," only "let me see the live file right now"). The two diff toggles' mutual-exclusion rules apply unchanged.

**Why default-to-review.** A pending agent edit on a note the user opens is almost always the reason the user opened the note — they want to look at the proposed change. Landing in review eliminates the extra click through the toolbar pill or status-bar dropdown for the common case, and the agent-diff toggle keeps the rarer "show me the live file" case (user wants to keep working on their own edits while a proposal sits) reachable in one click.


## Pending-rewrite banner

When the active tab is a buffer tab in plain editing mode and the buffer's path has at least one pending write-shaped proposal (`write_note` / `set_frontmatter` / `apply_tag`), a thin banner renders directly below the editor toolbar. [write-note-pending-banner]

- **Single line.** Label `Pending rewrite for this note` when the target path exists on disk; `Pending new-note proposal` when it doesn't (matches the `write-note-review-mode-label` framing). A muted origin suffix follows the same mapping as the write-note-review label: `· chat` for agent chat-tool writes (`surface = "chat"`), `· batch` for user-triggered batch note-mutation jobs (`surface = "batch-mutation"`), `· trail` for trail-driven writes (`surface = "trails"`); other surfaces render no suffix. A single `Review` button on the right enters write-note review mode on the active buffer via the same `openFile`-time auto-routing path. No accept / reject, no count, no diff preview — those live in the review surface the button opens.
- **Conditions for display.** Banner is visible iff (1) the active tab is a buffer tab (hidden on home / queue / settings / properties / any non-buffer kind), (2) the buffer's mode is plain editing (not patch-review, not write-note review, not snapshot-preview, not trash-preview), and (3) `pendingWriteProposalsForPath(buffer.path).length > 0`. A pending proposal against some *other* path never surfaces here — it shows up on that file's tab when the user opens it, on the recent-activity widget, and on the status-bar version dropdown. Rebuilds on `hiker:staging-changed`, on buffer switch, and on mode entry/exit.
- **Edit-note proposals don't trigger the banner.** Patch-review mode is its own affordance with its own pressed-state toolbar toggle; layering a banner on top would duplicate the signal. The banner is purely the write-shape counterpart to that toggle.
- **Multiple proposals collapse to one banner.** The button opens the same write-note review surface as `note-open-routes-to-pending-review` (most recent by `created_at`); older proposals against the same path remain reachable via the status-bar version dropdown.
- **Visual family.** Minimal: muted background tint (informational, same amber family as `snapshot-preview-mode`'s banner color), single-line height, no border-radius beyond the surrounding pane. Lives in `ui/src/editorPane/` as part of the editor pane chrome rather than the toolbar — the toolbar's `editor-toolbar-mode-controls` slot is reserved for active read-only modes, not plain-editing notifications.

**Why a banner here and not the mode-controls slot.** The mode-controls slot lights up *during* a review mode; plain editing leaves it empty by design (per `editor.md`'s "Empty when the buffer is in plain editing mode"). A pending write-shaped proposal in plain editing is a state the slot doesn't represent, and overloading it would conflict with the existing empty-when-plain rule. A thin banner is the smallest addition that signals "there's a review waiting" without competing with the mode-controls discipline.


## Module placement

- **`core::patch`** — new pure module. `Patch::apply(before, &[Edit]) -> Result<String, PatchError>` is the single entry point; supports the per-edit anchor / overlap / uniqueness rules of `mcp-edit-note-validation`. Same module-discipline as `core::diff` and `core::frontmatter`: confined dependency, plain-Rust input and output, unit tests alongside.
- **`core::staging`** — gains the `recheck` / `propose_batch` APIs per the updated module surface in `settings.md`. The patch payload (`EditPayload { old_str, new_str, replace_all }`) is stored on the proposal row alongside `target_path` and used by `recheck` to re-anchor.
- **`ui/src/patchReview/`** — new directory hosting the mode: `mountPatchReview(view, deps)` enters the mode against the active CM6 view + staging API; `renderHunks(view, proposals)` emits widget decorations and per-hunk gutter buttons; `exitPatchReview(view)` tears down. Same shape as `ui/src/snapshotPreview/`.
- **`ui/src/diff/`** — gains the agent-diff toggle button next to the existing user-diff toggle. The button is a thin component delegating to `mountPatchReview` / `exitPatchReview`. The two-button mutual-exclusion shim lives here.
- **`ui/src/modeControls/`** — gains `renderPatchReviewControls(applyableCount, conflictedCount)` and `renderWriteNoteReviewControls(rewriteOrNew, conflictReason?)` populators, called from the existing `renderModeControls()` dispatcher when the active mode is patch-review or write-note review.


## CM6 integration

- **Widget decorations.** Original-span deletions use `Decoration.line({ class: "patch-review-removed" })` plus a `Decoration.replace({ block: false, widget: StrikeThroughWidget })` over the matched byte range. Inserted spans use `Decoration.widget({ widget: InsertedBlockWidget, block: true, side: 1 })` anchored just past the matched range's end. Gutter buttons use CM6's `gutter` API with a per-hunk marker class.
- **Read-only enforcement.** Patch-review mode reconfigures the relevant compartment with `EditorState.readOnly.of(true)`. The dirty-buffer Diff toggle's existing live-preview + hide-frontmatter no-op compartment-reconfig (per `diff.md`'s "Markdown-rendering coupling") applies here too — the inserted widget blocks aren't real markdown, and live-preview decorations on them would render synthesized "edit content" with the wrong styling.
- **Idempotent rebuild.** All decorations come from a single `StateField<DecorationSet>` derived from the current staging snapshot for this path. `hiker:staging-changed` fires `view.dispatch({ effects: setProposals.of(newSnapshot) })`; the state field recomputes the decoration set. No incremental decoration patching; same pattern as the existing diff renderer.
- **Cursor preservation across modes.** Entering patch-review mode snapshots the user-diff toggle's saved selection + viewport; exit restores them. Same shape as the existing user-diff toggle's save-restore pattern. [patch-review-cursor-preserve]


## Pane integration

Patch-review and write-note review are *editor modes*, not pane states (same rule as snapshot / trash / staging preview). The pane-state list stays at three: editor / vault-home-overview / vault-home-detail, plus the settings sub-mode. Patch-review is identified by `buffer.mode.kind === "patch-review"`; write-note review by `"write-note-review"`. [patch-review-as-mode-not-pane]

Navigation history (`navigation-history-stack`):

- Entering patch-review mode or write-note review mode pushes onto history (matches snapshot-preview behavior).
- Toggling individual hunks' accept/reject within patch-review mode does *not* push — the mode is one history step.
- Exiting either mode pops back to plain editing on the same buffer.

Dirty-buffer protection on mode entry:

- **Patch-review entry never blocks.** The user can be dirty; the transactional accept handles the conflict surface itself.
- **Write-note review entry never blocks either.** The user can be dirty *while reviewing*; accept is what blocks (per `write-note-review-blocks-on-dirty`).


## Phases / dependencies

Concrete landing order so each piece can ship independently:

1. **`mcp-tool-edit-note` + `staging-per-edit-proposals`** — the tool itself plus the split-on-receive proposal shape. Once these land, `edit_note` calls produce per-edit staging rows that flow through the *existing* staging review surfaces (activity detail page, file tree, editor pill, chat card) the same way `write_note` proposals do today. The user can already accept/reject each edit individually; per-hunk inline review is a UI upgrade, not a prerequisite.
2. **`staging-proposal-state` + `staging-drift-eager-recheck`** — wires the `applyable` / `conflicted` derivation. Existing surfaces start rendering conflicted-proposal greying. No new UI needed; the existing accept verbs check state and refuse when conflicted.
3. **`staging-config-section` + `staging-auto-reject-on-conflict`** — the new `[staging]` TOML section with both keys. Can land before or after the patch-review UI; independent feature.
4. **`patch-review-mode` + `patch-review-agent-diff-toggle` + `patch-review-per-hunk-accept`** — the inline review UI. Depends on phase 1; benefits from phases 2 + 3 but doesn't require them.
5. **`patch-review-dirty-buffer-transactional-accept`** — the transactional accept handler. Lands with phase 1 (the direct-write path of `edit_note` also needs it for dirty-buffer-aware writes-without-review).
6. **`write-note-review-surface`** — relabels and remodels the existing snapshot-preview-shaped staging review path for `write_note` proposals. Lands when the patch-review mode is real, to keep the asymmetric framing consistent.


## Out of scope (this surface)

- **Editing-while-reviewing.** Patch-review mode is read-only while active. Hand-editing the buffer with pending hunks visible is a power-user surface that adds state (live anchor re-resolution as the user types) without clear payoff; the workflow is review-then-edit, not review-while-editing. Revisit if a real user asks. [patch-review-edit-while-reviewing-deferred]
- **Cross-file proposal review.** Each invocation of patch-review mode is scoped to the active buffer's file. Multi-file `edit_note` workflows (one call spanning N paths) aren't supported by the tool itself (`edit_note` takes a single `rel_path`); a future `multi_edit_note` would change the math, deferred. [patch-review-cross-file-deferred]
- **Three-way merge for `write_note` accept-while-dirty.** Blocked-and-tell is the v1 posture. Real three-way merge waits on `diff-viewer-three-way`. [patch-review-three-way-deferred]
- **Per-edit attribution badges showing which agent issued which hunk.** Useful only when multiple distinct agents have proposals on the same file simultaneously — rare in practice. Existing `batch_id` grouping covers the more common "these N hunks came from one tool call" case. [patch-review-per-agent-attribution-deferred]
- **Keyboard navigation between hunks.** Cycle next/previous hunk in the active patch-review session via a registered keybind. Worth doing once muscle memory builds; not load-bearing for v1. [patch-review-hunk-keybind-deferred]


## Forward refs

- `mcp.md` (`mcp-tool-edit-note`, `mcp-edit-note-validation`) — the producer side. Tool validates and splits before staging hands proposals to this review surface.
- `settings.md` (`staging-per-edit-proposals`, `staging-proposal-state`, `staging-drift-eager-recheck`, `staging-auto-reject-on-conflict`, `staging-config-section`) — the substrate this surface reads.
- `diff.md` (`diff-renderer`, `mode-controls-diff-toggle`, `editor-diff-vs-disk-toggle`) — the diff primitive patch-review reuses for write-note review's diff toggle, plus the icon-family conventions for the two diff toggles.
- `editor.md` (`editor-toolbar-mode-controls`, `panel-toggle-buttons`, `status-bar-version-dropdown`) — toolbar slot, mode-controls slot, and the per-buffer version surface where staging proposals already appear.
- `changes.md` (`changes-write-path`) — accepted edits append rows tagged `author='agent:<client-id>'` with `metadata.batch_id` for grouping.
