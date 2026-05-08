# Diff viewer

A read-only editor-pane surface that shows the line-level difference between two text buffers. Built first for the note-mutation accept/decline flow and the snapshot-vs-current activity view; extensible to other consumers as they appear.

The headline decisions:

- **Two buffers in, one diff out.** Inputs are a pair of labeled text buffers (label, content, optional source metadata). The viewer doesn't know or care where the buffers come from — derived files, on-disk current, snapshot blobs, future surfaces all fit. [diff-viewer-input-shape]
- **Pane state, not modal.** The editor pane gains a fourth state alongside editor / home-overview / home-detail. A banner across the top mirrors the trash and snapshot preview banners (label of each buffer, action buttons, close). [diff-viewer-pane]
- **Unified red/green line diff for v1.** Removed lines red, added lines green, unchanged context grey. Standard git-style hunk separators; line numbers in the gutter where applicable. Side-by-side and intraline highlighting are deferred follow-ups. [diff-viewer-line-unified]
- **Action buttons are consumer-defined.** The viewer reserves a slot in the banner for one or two action buttons whose label and behavior are passed in by the caller (e.g. "Replace original" / "Discard derived" for mutation; "Restore this version" for snapshot diff). The viewer renders + dispatches; the action's effect is the consumer's concern. [diff-viewer-banner-actions]


## Inputs

```ts
interface DiffInput {
  before: { label: string; content: string; meta?: object };
  after:  { label: string; content: string; meta?: object };
  actions?: DiffAction[];   // up to 2; rendered as buttons in the banner
}

interface DiffAction {
  label: string;            // "Replace original", "Restore this version", ...
  variant?: "primary" | "danger" | "default";
  run: () => Promise<void>; // consumer's effect; viewer awaits + closes on success
}
```

`label` is what the banner shows above each side ("`note.md` · current" / "`note.md` · reformatted"). `meta` is opaque to the viewer; consumers use it for their own bookkeeping (e.g. snapshot id, derived-file path).


## Rendering

Line diff via the standard Myers algorithm — JS-side using `diff` (jsdiff), no Rust round-trip. The viewer renders into a CodeMirror 6 `EditorView` with `EditorState.readOnly.of(true)` and line decorations driving the red/green coloring. Reusing CM6 means future syntax-highlighted diffs come for free (the same language compartment the editor already uses).

- **Removed lines** — pale red background, full-width line decoration.
- **Added lines** — pale green background, full-width line decoration.
- **Context lines** — default editor background.
- **Hunk separators** — a thin muted divider between non-adjacent hunks; click to expand context (defaults to ~3 lines of leading/trailing context).

Banner layout: left side shows the two buffer labels stacked or joined ("`note.md` · current ↔ derived"); center reserved for view-mode toggle (none in v1, lands with `diff-viewer-split-view`); right side renders the consumer-supplied action buttons plus a Close button.

Whitespace handling: diff is computed on raw text. A banner toggle for ignore-whitespace lands when a consumer needs it (likely the mutation flow if reformat-style prompts shuffle whitespace heavily). Deferred.


## Consumers

### Note-mutation accept/decline

When `note-mutations-menu` runs a mutation, the result lands at `.hiker/derived/<rel-path>.md` per the existing never-mutate-source rule. The pane immediately swaps to the diff viewer with `before = current source`, `after = derived output`, and two banner actions:

- **Replace original** — writes `after.content` to the source path via `vault.write_file_checked` (drift-checked against `before`'s hash so concurrent edits don't get clobbered), appends a `'modified'` row to `core::changes` tagged `metadata.mutation = "<mutation-name>"`, deletes the derived file, returns the pane to the editor on the now-mutated source. The activity widget picks up the change for free via the existing `hiker:changes-appended` flow. [note-mutation-replace-original]
- **Discard derived** — deletes the derived file, returns the pane to the editor on the unchanged source. The activity log isn't touched (no change occurred). [note-mutation-discard-derived]

The derived file is *not* shown to the user as a separate buffer in v1 — the diff is the only review surface. A "keep derived alongside source" option is a banner action we can add later if the workflow earns it. [note-mutation-diff-review]

The mutation feature itself stays deferred (same status as `note-mutations-menu`); this section pins how it'll plug into the diff viewer when it lands. The viewer + the mutation routing land in the same change.

### Snapshot diff in activity detail

The recent-activity detail view (`vault-home-recent-activity-detail`) currently offers two row affordances: click to open snapshot read-only (`snapshot-preview-mode`), per-row `[Restore this version]`. Two diff-driven additions:

- **Show diff vs current** — banner action on the existing snapshot read-only preview. Swaps the pane to the diff viewer with `before = snapshot blob`, `after = current on-disk`, and the snapshot's `[Restore this version]` action moved into the diff banner unchanged. Restore from the diff view does the same `restore_snapshot` write-back that the per-row Restore does. The snapshot read-only preview stays as the absolute view; diff is the comparative one — both are valid, the user picks per task. [snapshot-preview-diff-toggle]
- **Diff between two snapshots** — multi-select two rows in the activity detail view, click "Diff selected"; pane swaps with both buffers being snapshot blobs. No action buttons — pure inspection. Depends on a multi-select shape in the activity detail that doesn't exist in v1; deferred until that lands. [activity-detail-diff-between-versions]

### Drift-conflict resolution (forward ref)

`drift-conflict-modal` (in `editor.md`) currently offers keep-mine / take-theirs / cancel, with "open diff" explicitly deferred. When that lands, it uses this viewer with `before = on-disk current`, `after = my buffer`, and three actions: keep mine / take theirs / cancel. The viewer's two-action-button budget grows to three for that case, or the third action lives as a secondary affordance — pinned when the work happens. [diff-viewer-drift-conflict]


## Pane integration

The pane states (per editor.md's `## Layout` and the home overview/detail discussion):

- editor (CM6 buffer)
- vault home overview
- vault home detail
- **diff viewer** (new)

Same swap pattern: `#editor-pane` has a class controlling which child is visible. The diff viewer's CM6 instance is constructed lazily on first open and reused across diffs (state replaced via `dispatch`, same shape as the main editor's buffer-switch).

Navigation history (`navigation-history-stack`) treats entering the diff view as a content-surface change — pushes onto the stack the same way opening a snapshot preview does. Back returns to wherever the user came from (the snapshot preview, the activity detail, the editor on the source file, etc.).

Closing the diff view (banner Close button) returns to the previous pane state, not always to the editor.

Dirty-buffer protection: the diff view itself isn't a buffer — there's nothing to be dirty. Entering the diff view from a dirty editor buffer doesn't fire the file-switch guard (the source file isn't being switched, just hidden). The buffer's dirty state is preserved on return. The mutation Replace-original action does need to think about dirty state: if the source file has unsaved edits when Replace fires, the action errors with a clear message ("Save or discard pending edits before replacing"); we don't silently drop the buffer's edits. [diff-viewer-respects-dirty-source]


## Module placement

- `ui/src/diff/` — viewer component, banner, jsdiff wiring, CM6 line decorations.
- Consumers (mutation flow, snapshot preview, drift-conflict modal when it lands) call into `openDiffView(input: DiffInput)` and pass their actions.
- No Rust-side work for v1 — every input string already exists on the TS side (current buffer text, derived file content via `read_file`, snapshot blobs via `change_content`).


## Out of scope (v1)

- **Word- or character-level intraline diff.** Useful for prose where a single word changed inside a long line; planned for v2 of the viewer. [diff-viewer-intraline]
- **Side-by-side split view.** Standard alternate rendering; lands when a real user prefers it over unified. [diff-viewer-split-view]
- **Three-way merge view.** For actual conflict resolution; lands when `drift-conflict-modal`'s diff option is wired. The third buffer slot is the only structural change. [diff-viewer-three-way]
- **In-editor inline diff overlays.** A different surface (CM6 decorations on top of the live editor view, not a pane swap). Useful for "show me what just changed" inside the buffer; out of this doc.
- **Image / binary diff.** v1 is text-only.
- **Export as patch.** "Copy as unified diff" for paste-elsewhere workflows; cheap to add when needed. [diff-viewer-export-patch]
- **Configurable color scheme.** Reds and greens are the defaults; theming waits for the broader theme work.


## Deferred

Slugs registered as `planned`:

- `diff-viewer-split-view` — side-by-side rendering as a banner-anchored mode toggle.
- `diff-viewer-intraline` — character-level highlights inside changed lines.
- `diff-viewer-three-way` — third buffer slot for merge / drift-conflict resolution.
- `diff-viewer-ignore-whitespace` — banner toggle when a consumer needs it.
- `diff-viewer-export-patch` — "copy as patch" affordance.
- `activity-detail-diff-between-versions` — multi-select two snapshot rows + "Diff selected".
- `note-mutation-keep-derived` — third banner action for "keep derived alongside source" if the workflow earns it.
