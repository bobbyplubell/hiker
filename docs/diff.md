# Diff

A unified diff primitive plus the editor surfaces that consume it. Every diff in hiker — uncommitted-buffer diff, `.ops` history (via `materialize_at`), pending agent edits, snapshot review — is the same `DiffLayer` rendered against an editor tab.

- **Diff is a mode of the editor tab, not a tab kind.** A tab carries an optional `diff: DiffSource`; when set, it renders the buffer's `current` decorated by `DiffLayer(resolve(diff), current)`. Toggle is in the editor toolbar; no separate `BufferDiff` / `SnapshotPreview` / `StagingPreview` / `TrashPreview` tab kinds. [diff-as-mode]
- **One diff engine, rendering in the editor crate.** `editor_core::diff::lines(left, right)` is the single line-diff engine (with a word-level intraline pass); `editor-diff` turns its hunks into a `DecorationSet`. There is no separate `core::diff` — the vestigial `hiker_core::diff` line-differ was deleted and its two callers moved onto this engine. [diff-core-module, diff-renderer]


## DiffLayer

The one primitive: two text inputs in, hunks + decoration set out, owner-agnostic. Hosted in the editor crate. [diff-layer]

```rust
pub struct DiffLayer {
    pub base: Rope,
    pub current: Rope,
    pub owner: DiffOwner,
}

pub enum DiffOwner {
    Index,          // gutter-only; no inline decorations, no controls
    Agent,          // per-hunk accept/reject of pending agent edits
    HistoryVersion, // per-hunk restore (writes that hunk's base text back as a fresh frame)
    Manual,         // no controls (user-initiated diff, e.g. between two versions)
}
```

Owners drive UI affordances, not rendering — the decoration set is identical across owners; only the per-hunk verbs differ (above), riding as overlay widgets on the hunk. [diff-layer-owner]

The layer recomputes hunks each frame from `(base, current)`. Cheap because the inputs are ropes and diff is line-based. No anchor bookkeeping across edits — the diff *is* the state.

The decoration set the layer emits:

- **Line decorations.** Pale-red background for `base`-only lines (deletion), pale-green for `current`-only lines (insertion), default for equal lines.
- **View zones.** Removed lines from `base` are injected as phantom lines above their successor in `current` (block decorations that affect line height). Same mechanism the editor already uses for chunk-boundary widgets.
- **Intraline marks.** For each paired delete/insert line, a second pass (in the editor renderer over `editor_core::diff`, not `core::diff`) emits `Decoration::mark` ranges with saturated red/green over the pale line background. Controlled by the per-vault `view.intraline_diff` toggle. [diff-viewer-intraline]
- **Gutter markers.** `DiffAdded` / `DiffRemoved` / `DiffModified` glyphs per hunk. The only thing `Index` ownership emits.
- **Overlay widgets (per hunk).** Owner-driven action buttons positioned at the hunk's first visible line. [diff-layer-hunk-widgets]


## DiffSource

`DiffSource` enumerates where `base` comes from. Each variant resolves to a `Rope` directly — no URI scheme indirection — synchronously off existing services (`vault.read_file`, `app.buffers.get(path)`, `oplog.materialize_at(path, frame_id)`, `vault.read_trash(path)`); no async, no caching layer; resolved each time the tab activates or its source is invalidated. [diff-source-enum]

```rust
pub enum DiffSource {
    Disk(PathBuf),                          // on-disk text at read time
    LiveBuffer(PathBuf),                    // another open buffer's current rope
    HistoryVersion { path: PathBuf, frame_id: String }, // materialize_at the given .ops frame
    PendingProposal { proposal_id: String },// proposed content for a whole-file agent proposal
    Trash(PathBuf),                         // trashed file content
    Empty,                                  // empty rope (for diff-against-nothing)
}
```

`LiveBuffer` is the "diff against another open buffer" affordance. The dirty-buffer diff is `base = Disk(path), current = live buffer`.

`Empty` lets trash entries — which have no current on-disk counterpart — open in editor mode with `diff = Some(Empty)` greyed out, rather than special-casing the tab.


## Editor tab integration

A tab is `Editor { buffer, diff: Option<DiffSource> }` (the only buffer-backed kind; see `editor.md` `tab-kinds`). The renderer:

1. Mounts the buffer's editor widget against `buffer.current`.
2. If `diff` is `Some`, resolves the `DiffSource` to a `Rope` `base`, constructs a `DiffLayer { base, current: buffer.current, owner: owner_for(diff) }`, and pushes its decoration set onto the editor's decoration stack.
3. Renders the toolbar's diff toggle as pressed when `diff.is_some()`. Right-clicking the toggle opens a source picker (per `editor.md` `editor-diff-target-picker`).

The buffer's `current` is whatever the buffer normally holds — its rope is unchanged by entering diff mode. Diff is a content lens, not a buffer swap. Cursor and selection survive toggling on and off.

`owner_for(diff)` maps: `Disk` / `LiveBuffer` / `Trash` / `Empty` → `Manual`; `HistoryVersion` → `HistoryVersion`; `PendingProposal` → `Agent`. (Verbs per the `DiffOwner` variants above.)

`Agent` ownership is the inline patch-review path described in `patch-review.md`: the diff source isn't a single proposal — it's the buffer's `agent_base` (the materialized `accepted + working` at the moment proposals were hydrated). That's not a `DiffSource` variant because it's a property of the buffer itself, not a chosen comparison target.

Read-only vs editable is independent of diff mode. History-version / trash sources mark the buffer read-only (no save path). The dirty-buffer diff and the agent diff leave the buffer editable.


## Show changes (.ops history browser)

Right-clicking inside an editor buffer opens a context menu whose "Show changes…" entry lists recent frames for the buffer's path (via `core::ops::path_history`), newest first. Selecting a row switches the active tab into diff mode with `diff = Some(HistoryVersion { path, frame_id })`. [editor-show-changes-menu]

- **Submenu shape.** Up to N=20 recent rows. Each row shows timestamp (relative + absolute on hover), change kind, author class. Last entry: `Browse all… → ` opens the `history` app page filtered to this path.
- **No URI scheme.** `DiffSource::HistoryVersion` resolves directly through `oplog::materialize_at(path, frame_id)`. The editor crate doesn't know about URI providers.
- **Per-hunk restore.** Hunks carry a `Restore this hunk` verb that writes `base` (the historical text) back into the buffer for that range and saves (forward-correct — a fresh frame). Restore-all stays as the existing `restore_snapshot` row-level action on the activity surface. [diff-layer-hunk-widgets]
- **Read-only on history side.** The displayed diff is `base = historical`, `current = live buffer`. The user can keep editing `current` while a historical diff is shown; the diff updates each frame.


## Module placement

- `editor_core::diff` (editor-core crate) — the single diff engine: `lines(left, right) -> Vec<Hunk>` with line hunks (`HunkKind::{Context,Added,Removed,Modified}`, 0-based `left_lines`/`right_lines` ranges) plus a word-level intraline pass. `similar` confined here. There is no `core::diff`; the former `hiker_core::diff` line-differ was deleted and its callers (the sync fork-diff, the dirty-state gutter) moved onto this engine. [diff-core-module]
- `editor::diff` (editor crate, `editor-diff` module) — `DiffLayer`, `unified_decorations`, the intraline mark pass, view-zone construction for removed lines, gutter markers, hunk overlay widgets. Consumes `editor_core::diff` output. [diff-renderer]
- `app/src/panels/buffer/` — the editor tab body. Owns `Editor { buffer, diff }` rendering, the toolbar diff toggle and source picker, the right-click "Show changes" menu, and per-owner hunk-verb dispatch.
- CLI: `hiker diff <path> [<frame-id>]` calls `editor_core::diff::lines` directly and prints a unified diff. [cli-diff]


## Out of scope

- **Side-by-side view.** Layout option on `DiffLayer` (two editor widgets sharing the same hunks). Lands when a user prefers it over unified. [diff-viewer-split-view]
- **Three-way merge.** Three-rope input + merge resolution UI; required by the unified conflict surface (`sync.md`) and `drift-conflict-modal`'s deferred "open diff" verb. [diff-viewer-three-way]
- **Image / binary diff.** Text only.
- **Export as patch.** "Copy as unified diff" affordance. [diff-viewer-export-patch]
- **MCP diff tool.** Agents reason over two blobs via existing read tools; a dedicated diff tool is an optimization. [mcp-tool-diff]


## Deferred

- `diff-viewer-split-view` — side-by-side layout option on `DiffLayer`.
- `diff-viewer-three-way` — third rope input for merge resolution (anchors the unified conflict surface).
- `diff-viewer-ignore-whitespace` — toggle on the compute call.
- `diff-viewer-export-patch` — "copy as unified diff" affordance.
- `activity-detail-diff-between-versions` — multi-select two history rows + "Diff selected" opens a tab with `diff = Some(HistoryVersion { path, frame_id: a })` against a buffer whose content is `materialize_at(path, b)`.
- `mcp-tool-diff` — agent-facing diff IPC.
