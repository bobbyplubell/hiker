# Diff viewer

A read-only rendering primitive that shows the line-level difference between two text buffers. Hosted by the consumer (snapshot preview, dirty-buffer Diff toggle, staging review), surfaced as a toggle button in the editor toolbar's `#mode-controls` slot that flips the CM6 view between "the consumer's primary content" and "the diff."

The headline decisions:

- **Two buffers in, one diff out.** Inputs are a pair of labeled text buffers. The renderer doesn't know or care where the buffers come from — on-disk current, snapshot blobs, live editor buffers, staging proposals, future surfaces all fit. [diff-viewer-input-shape]
- **Rendering primitive, hosted by the consumer.** The diff is *not* its own pane state. Each consumer toggles the CM6 view between its primary content (e.g. snapshot blob, live editable buffer, staging proposal) and the diff rendering against the appropriate counterpart. [diff-renderer]
- **Unified red/green line diff for v1.** Removed lines red, added lines green, unchanged context grey. v1 paints the whole file with changes inline — `core::diff::compute` emits a single hunk containing every line so the user sees full file context regardless of where the changes are. Hunk grouping with `⋯` separators and click-to-expand context is a deferred follow-up (`diff-viewer-grouped-hunks`); the wire format already accommodates a list of hunks so the consumer entry point is the only thing that grows. Side-by-side and intraline highlighting are separate deferred follow-ups. [diff-viewer-line-unified]
- **Mode-specific controls live in the editor toolbar's center slot, not in banners.** Each consumer populates `#mode-controls` with a small text label naming the mode plus icon-only buttons for the mode's actions. The Diff toggle is one such icon button (pressed when diff is active); Restore / Close / Apply / Reject are siblings. No separate banner DOM elements — controls live in the same toolbar that hosts View / Mutations / Discovery toggles. [editor-toolbar-mode-controls, mode-controls-diff-toggle]


## Inputs

```ts
interface DiffInput {
  before: { label: string; content: string; meta?: object };
  after:  { label: string; content: string; meta?: object };
}
```

`label` is what the renderer shows in the gutter / chrome to identify each side ("`note.md` · disk" / "`note.md` · buffer"). `meta` is opaque to the renderer; consumers use it for their own bookkeeping (e.g. snapshot id, staging path).

A consumer that wants to render a diff calls `renderDiff(view: EditorView, input: DiffInput)` (or equivalent) — the renderer applies the diff doc + line decorations to the CM6 view it's handed. Toggling back to the plain view is the consumer's job (replace the doc with the plain `after.content` again, drop the decorations).

`actions` and `onClose` are *not* part of the renderer's contract. Each consumer's mode-controls render owns the buttons and lifecycle; the diff is a content lens, not a separate surface.


## Computation vs rendering

The split mirrors the rest of hiker's module discipline: domain logic in `core::*`, presentation in `ui/`.

- **`core::diff`** — pure function `compute(before: &str, after: &str) -> DiffResult` returning a list of hunks (each hunk is a list of `{ op: Equal | Insert | Delete, line: String, before_line_no: Option<u32>, after_line_no: Option<u32> }`). Backed by the [`similar`](https://crates.io/crates/similar) crate. Pure: no I/O, no async, no state — just text → diff. Testable in Rust without spinning up the UI. [diff-core-module]
- **Tauri command** `compute_diff(before: String, after: String) -> Result<DiffResult>` — thin wrapper over `core::diff::compute`. The `DiffResult` shape auto-exports as a TS type via `ts-rs` per design.md.
- **CLI parity** — `hiker diff <path> <snapshot-id>` (and `hiker diff <path-a> <path-b>`) calls the same `core::diff::compute`, prints unified-diff output to stdout. Lands when the CLI is fleshed out. [cli-diff]
- **MCP** — no tool surface in v1. Agents can already retrieve two blobs (`get_note` + `change_content`) and reason over them; an `mcp-tool-diff` would be an optimization, not a capability. Reserved as deferred. [mcp-tool-diff]


## Rendering

The renderer applies the `DiffResult` to the consumer's CodeMirror 6 `EditorView` with `EditorState.readOnly.of(true)` and line decorations driving the red/green coloring. Reusing CM6 means future syntax-highlighted diffs come for free (the same language compartment the editor already uses).

- **Removed lines** — pale red background, full-width line decoration.
- **Added lines** — pale green background, full-width line decoration.
- **Context lines** — default editor background.

v1 paints the whole file: `core::diff::compute` returns a single hunk containing every line (Equal / Insert / Delete), and the renderer flattens it into one continuous doc with per-line decorations. No `⋯` separators, no click-to-expand, no folding of unchanged regions — the user sees full file context with changes highlighted inline. The renderer's between-hunk `⋯` separator code path is unreachable in v1 by design; it stays in place so the grouped-hunks variant can drop in without UI rework.

The grouped variant (`diff-viewer-grouped-hunks`) is the deferred follow-up: a separate `compute` entry point in `core::diff` (or an option flag) emits multiple hunks with bounded context (~3 lines of leading/trailing per hunk), and the renderer's `⋯` separators light up between them. Lands when files large enough to make the full-file paint awkward become a real consumer concern, or when the MCP tool surface for diff (`mcp-tool-diff`) wants a more compact representation. Same wire format, same UI primitive, different `compute` call.

Whitespace handling: diff is computed on raw text. A toggle for ignore-whitespace lands when a consumer needs it. Deferred (`diff-viewer-ignore-whitespace`).

Staleness: the diff is computed once when the toggle flips on and is *not* auto-refreshed if `before` or `after`'s underlying source mutates while the toggle stays on (e.g. a `hiker:file-changed` event for the snapshot consumer's path). The user toggles off and back on to recompute. Deliberate non-feature for v1 — preview surfaces are short-lived review interactions, and pinning the diff to its initial inputs avoids surprising re-renders mid-review. If a future consumer needs live re-diffing it can call `renderDiff` again itself; the renderer stays stateless about freshness.

Markdown-rendering coupling: when the diff is shown inside a CM6 view that normally hosts markdown extensions (live preview, hide-frontmatter, etc.), the consumer must reconfigure those extensions to no-ops while the diff is active. The synthesized diff doc isn't markdown — its first `---` line shouldn't collapse under a frontmatter widget, a removed `# heading` shouldn't render as a styled heading, and emphasis markers shouldn't hide on cursor-out. Snapshot preview's `toggleSnapshotDiff` does this for the `livePreview` and `hideFrontmatter` compartments; future consumers follow the same pattern. Chunk-boundary gutters are already gated on read-only buffers and stay quiet for free.


## The mode-controls slot and the Diff toggle

The editor toolbar (`panel-toggle-buttons`) reserves a center slot, `#mode-controls`, between two flex spacers. The slot is empty in normal editing; entering a read-only preview mode populates it. [editor-toolbar-mode-controls]

Concretely the slot holds:

- A short text label naming the mode ("Snapshot preview" / "Trash preview" / "Diff · snapshot ↔ current" / "Diff · buffer ↔ disk" / "Staging review"). Title attribute carries metadata (path, timestamp, author, change id) so hover gives the user the full context without taking space.
- A row of small icon-only buttons matching the existing toolbar-button palette (same line-weight, sizing, hover treatment). Mode-specific verbs land here: Diff toggle, Restore, Apply, Reject, Close — whichever the active consumer exposes.

The Diff toggle is the icon button that flips the CM6 view between the consumer's primary content and the diff rendering: [mode-controls-diff-toggle]

- **Default ("the primary content")** — the consumer's primary content (snapshot blob, live editable buffer, staging proposal, etc.) is shown. Toggle button is unpressed; tooltip "Show diff vs current" (or whichever phrasing fits the consumer).
- **Toggled on ("the diff")** — the synthesized diff document with red/green line decorations against `before`. Toggle button is pressed; tooltip names the alternate state ("Hide diff").

Pressed/unpressed visual state reflects which view is active. Same affordance shape across consumers so users build muscle memory once.

Consumers that have nothing to diff against (e.g. an `op = "deleted"` activity row, or a newly-created buffer that hasn't been saved to disk yet) omit the Diff toggle entirely — its presence is the signal that "a diff exists for this view." The other action icons (Restore / Apply / Reject / Close) stay regardless of toggle state; the diff is a different lens on the same review, the verbs don't change.

Rebuild discipline: `renderModeControls()` is idempotent — every transition (buffer swap, mode entry/exit, diff on/off) calls it fresh and replaces the slot's children. No incremental DOM updates; same inputs produce the same DOM. Cheap because the slot is tiny.


## Consumers

### Snapshot preview

`snapshot-preview-mode` is the first and reference consumer. The mode-controls slot shows: label ("Snapshot preview" or "Diff · snapshot ↔ current" when toggled), Diff toggle icon, Restore icon, Close icon. Default view on snapshot open is the snapshot content; toggle flips to diff with `before = snapshot blob`, `after = current on-disk` (read via `read_file_with_hash`). The toggle is hidden for `op = "deleted"` rows (no current content to diff against). [snapshot-preview-diff-toggle]

### Dirty-buffer Diff toggle

Whenever the active buffer is `isDirty()` and not in any other read-only preview mode, the mode-controls slot shows the Diff toggle alone — no other verbs (Save / Ctrl-Z / Save As are the user's verbs, and they live in the regular editor surface). Default view: the live editable buffer. Toggle: diff with `before = last-loaded content`, `after = live buffer text`. Toggling back returns the user's cursor + selection — flipping to diff doesn't destroy editor state. [editor-diff-vs-disk-toggle]

This is the review surface for in-buffer mutations (per `editor.md` Note-mutations menu) — the user reads the post-mutation buffer, optionally toggles to compare against on-disk, then either Saves (accept) or Ctrl-Zs (revert). It's also a generally useful affordance for hand-edits before saving.

### Staging review (forward ref)

Staging proposals open for preview as a read-only buffer with the existing diff toggle — the same pattern `snapshot-preview-mode` already uses. Accept/reject lives on the calling surface's row (activity detail page, chat card, trails panel, tree context menu, editor toolbar pill), not in the editor toolbar mode-controls slot. See `settings.md`'s "Staging review" section for the full surface.

### Drift-conflict resolution (forward ref)

`drift-conflict-modal` currently offers keep-mine / take-theirs / cancel, with "open diff" explicitly deferred. When that lands, it'll use the same toggle pattern: the modal opens a temporary preview mode with `before = on-disk current`, `after = my buffer`, mode-controls icons = keep mine / take theirs / cancel + the Diff toggle. The third action is the structural change vs the two-action consumers. [diff-viewer-three-way]

### Diff between two snapshots

In the activity detail view, when multi-select lands the user can pick two snapshot rows and click "Diff selected." This *is* a pure-inspection surface (no Restore action makes sense — neither row is "current"), so the mode-controls slot carries just the label, Diff toggle, and Close — no action verbs. Pinned for when the multi-select shape lands. [activity-detail-diff-between-versions]


## Pane integration

The diff is *not* a separate `#editor-pane` state. Each consumer's preview surface shares the editor's CM6 view and is distinguished by what `#mode-controls` is currently rendering. The pane-state list stays at three: editor / vault home overview / vault home detail. Snapshot preview, staging preview are sub-modes of the editor pane state, identified by buffer-state flags (`buffer.mode.kind`) that drive `renderModeControls()`. The dirty-buffer Diff toggle is *not* a separate sub-mode — it's a content-lens on the regular editor pane state, and toggling it on/off doesn't push history.

Navigation history (`navigation-history-stack`) treats entering snapshot preview / staging preview as a content-surface change. Toggling within a preview between plain view and diff view does *not* push history — same as toggling the View menu's options doesn't push, the user is still on the same content surface, just rendered differently.

Dirty-buffer protection: snapshot and staging previews are read-only; entering one from a dirty editor buffer is the same code path as opening a trash preview today (no buffer swap, dirty state preserved on return). The staging-Apply action errors when the source has unsaved edits rather than dropping them. [diff-viewer-respects-dirty-source]


## Module placement

- `core::diff` — pure `compute(before, after) -> DiffResult`; `similar` crate confined to this module, mirroring the `rusqlite-only-in-store` / `fastembed-only-in-embed` pattern. Unit tests live alongside.
- Tauri command `compute_diff` in `ui/src-tauri/src/lib.rs` — ~10-line wrapper.
- `ui/src/diff/` — `renderDiff(view, input)` rendering helper + the toggle button component + CM6 line decorations consuming the `DiffResult`. Strings already exist UI-side; the IPC carries the diff *output*, not the inputs round-tripped.
- Each consumer (snapshot preview, dirty-buffer Diff toggle, staging preview) owns its own pane wiring and calls `renderDiff` against its own CM6 view when the toggle flips on.
- CLI: `hiker diff` lives in `cli/`, calls `core::diff::compute` directly, prints unified diff.


## Out of scope (v1)

- **Word- or character-level intraline diff.** Useful for prose where a single word changed inside a long line. [diff-viewer-intraline]
- **Side-by-side split view.** Standard alternate rendering; lands when a real user prefers it over unified. [diff-viewer-split-view]
- **Three-way merge view.** For actual conflict resolution; lands when `drift-conflict-modal`'s diff option is wired. [diff-viewer-three-way]
- **In-editor inline diff overlays on the live editor.** A different surface (decorations on top of the editing buffer, not a preview pane). Out of this doc.
- **Image / binary diff.** v1 is text-only.
- **Export as patch.** "Copy as unified diff" for paste-elsewhere workflows. [diff-viewer-export-patch]
- **Configurable color scheme.** Theming waits for the broader theme work.


## Deferred

Slugs registered as `planned`:

- `diff-viewer-split-view` — side-by-side rendering as a toggle option.
- `diff-viewer-intraline` — character-level highlights inside changed lines.
- `diff-viewer-three-way` — third buffer slot for merge / drift-conflict resolution.
- `diff-viewer-ignore-whitespace` — toggle when a consumer needs it.
- `diff-viewer-export-patch` — "copy as patch" affordance.
- `activity-detail-diff-between-versions` — multi-select two snapshot rows + "Diff selected".
- `staging-review-activity-detail-filter` — "Pending" filter pill on activity detail page; diff toggle reuses `snapshot-preview-diff-toggle` per `settings.md`.
