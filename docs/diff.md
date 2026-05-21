# Diff

A unified diff primitive plus the editor surfaces that consume it. Every diff in hiker — uncommitted-buffer diff, changes.db history, pending agent edits, snapshot review — is the same `DiffLayer` rendered against an editor tab.

The headline decisions:

- **One primitive: `DiffLayer { base, current, owner }`.** Two text inputs in; hunks + intraline spans + decoration set out. The renderer is owner-agnostic. Hosted in the editor crate alongside the existing decoration primitives. [diff-layer]
- **Diff is a mode of the editor tab, not a tab kind.** An editor tab carries an optional `diff: DiffSource`; when set, the tab renders the buffer's `current` text decorated by `DiffLayer(resolve(diff), current)`. Toggle is in the editor toolbar; no separate `BufferDiff` / `SnapshotPreview` / `StagingPreview` / `TrashPreview` tab kinds. [diff-as-mode]
- **`DiffSource` enumerates where `base` comes from.** `Disk(path)`, `LiveBuffer(doc_id)`, `ChangesDb(change_id)`, `StagingProposal(proposal_id)`, `Trash(trash_path)`, `Empty`. Each variant resolves to a `Rope` directly — no URI scheme indirection. [diff-source-enum]
- **Owners drive UI affordances, not rendering.** The decoration set is identical across owners; what differs is per-hunk verbs (accept/reject for `Agent` and `Staging`, restore for `Snapshot`, none for `Index` or `Manual`). Verbs ride as overlay widgets on the hunk. [diff-layer-owner]
- **Computation is pure; rendering is in the editor crate.** `core::diff::compute(before, after)` returns hunks with optional intraline spans. `editor-diff` turns hunks into a `DecorationSet` (line backgrounds, removed-line view zones, intraline marks). Side-by-side layout is a future view option on the same primitive, not a separate tab. [diff-core-module, diff-renderer, diff-viewer-split-view]


## DiffLayer

```rust
pub struct DiffLayer {
    pub base: Rope,
    pub current: Rope,
    pub owner: DiffOwner,
}

pub enum DiffOwner {
    Index,     // gutter-only; no inline decorations, no controls
    Staging,   // per-hunk accept/reject
    Agent,     // per-hunk accept/reject (same render as Staging; distinct telemetry)
    Snapshot,  // per-hunk restore (writes that hunk's base text back to disk)
    Manual,    // no controls (user-initiated diff, e.g. between two snapshots)
}
```

The layer recomputes hunks each frame from `(base, current)`. Cheap because the inputs are ropes and diff is line-based. No anchor bookkeeping across edits — the diff *is* the state.

The decoration set the layer emits:

- **Line decorations.** Pale-red background for `base`-only lines (deletion), pale-green for `current`-only lines (insertion), default for equal lines.
- **View zones.** Removed lines from `base` are injected as phantom lines above their successor in `current` (block decorations that affect line height). Same mechanism the editor already uses for chunk-boundary widgets.
- **Intraline marks.** For each paired delete/insert line, a second pass emits `Decoration::mark` ranges with saturated red/green over the pale line background. Controlled by the per-vault `view.intraline_diff` toggle. [diff-viewer-intraline]
- **Gutter markers.** `DiffAdded` / `DiffRemoved` / `DiffModified` glyphs per hunk. The only thing `Index` ownership emits.
- **Overlay widgets (per hunk).** Owner-driven action buttons positioned at the hunk's first visible line. [diff-layer-hunk-widgets]


## DiffSource

```rust
pub enum DiffSource {
    Disk(PathBuf),                  // on-disk text at read time
    LiveBuffer(DocId),              // another open buffer's current rope
    ChangesDb(ChangeId),            // content_at(change_id) from changes.db
    StagingProposal(ProposalId),    // proposal's stored before-text
    Trash(PathBuf),                 // trashed file content
    Empty,                          // empty rope (for diff-against-nothing)
}
```

Each variant resolves to a `Rope` synchronously off existing services (`vault.read_file`, `app.buffers.get(doc_id)`, `changes.content_at(id)`, `staging.proposal(id)`, `vault.read_trash(path)`). No async, no caching layer; resolved each time the tab activates or its source is invalidated.

`LiveBuffer` is the "diff against another open buffer" affordance — used by the dirty-buffer toggle (`base = LiveBuffer(self)` is wrong; the dirty-buffer diff is `base = Disk(path), current = live buffer`).

`Empty` exists so trash entries — which have no current on-disk counterpart — can open in editor mode with `diff = Some(Empty)` greyed out, rather than special-casing the tab.


## Editor tab integration

A tab is `Editor { buffer, diff: Option<DiffSource> }` (the only buffer-backed kind; see `editor.md` `tab-kinds`). The renderer:

1. Mounts the buffer's editor widget against `buffer.current`.
2. If `diff` is `Some`, resolves the `DiffSource` to a `Rope` `base`, constructs a `DiffLayer { base, current: buffer.current, owner: owner_for(diff) }`, and pushes its decoration set onto the editor's decoration stack.
3. Renders the toolbar's diff toggle as pressed when `diff.is_some()`. Right-clicking the toggle opens a source picker (per `editor.md` `editor-diff-target-picker`).

The buffer's `current` is whatever the buffer normally holds — its rope is unchanged by entering diff mode. Diff is a content lens, not a buffer swap. Cursor and selection survive toggling on and off.

`owner_for(diff)` maps:
- `Disk` / `LiveBuffer` → `Manual` (no per-hunk controls; user is just looking)
- `ChangesDb` → `Snapshot` (per-hunk restore verb)
- `StagingProposal` → `Staging` (per-hunk accept/reject)
- `Trash` → `Manual`
- `Empty` → `Manual` (diff is empty anyway)

`Agent` ownership is the inline patch-review path described in `patch-review.md`: the diff source isn't a single proposal — it's the buffer's `agent_base` (the disk text at the moment proposals were hydrated). That's not a `DiffSource` variant because it's a property of the buffer itself, not a chosen comparison target.

Read-only vs editable is independent of diff mode. Snapshot / trash sources mark the buffer read-only (no save path). The dirty-buffer diff and the agent diff leave the buffer editable.


## Show changes (changes.db browser)

Right-clicking inside an editor buffer opens a context menu whose "Show changes…" entry lists recent `changes.db` rows for the buffer's path, newest first. Selecting a row switches the active tab into diff mode with `diff = Some(ChangesDb(change_id))`. [editor-show-changes-menu]

- **Submenu shape.** Up to N=20 recent rows. Each row shows timestamp (relative + absolute on hover), op, author. Last entry: `Browse all… → ` opens the `history` app page filtered to this path.
- **No URI scheme.** `DiffSource::ChangesDb` resolves directly through `core::changes::content_at(change_id)`. The editor crate doesn't know about URI providers.
- **Per-hunk restore.** Hunks carry a `Restore this hunk` verb that writes `base` (the historical text) back into the buffer for that range and saves. Restore-all stays as the existing `restore_snapshot` row-level action on the activity surface. [diff-layer-hunk-widgets]
- **Read-only on history side.** The displayed diff is `base = historical`, `current = live buffer`. The user can keep editing `current` while a historical diff is shown; the diff updates each frame.


## Module placement

- `core::diff` — pure `compute(before, after) -> DiffResult` with hunks + intraline spans. `similar` crate confined to this module. [diff-core-module]
- `editor::diff` (editor crate, `editor-diff` module) — `DiffLayer`, `unified_decorations`, intraline mark pass, view-zone construction for removed lines, gutter markers, hunk overlay widgets. Consumes `core::diff` output. [diff-renderer]
- `app/src/panels/editor/` — the editor tab body. Owns `Editor { buffer, diff }` rendering, the toolbar diff toggle and source picker, the right-click "Show changes" menu, and per-owner hunk-verb dispatch.
- CLI: `hiker diff <path> [<change-id>]` calls `core::diff::compute` directly and prints a unified diff. [cli-diff]


## Out of scope

- **Side-by-side view.** Layout option on `DiffLayer` (two editor widgets sharing the same hunks). Lands when a user prefers it over unified. [diff-viewer-split-view]
- **Three-way merge.** Three-rope input + merge resolution UI; required by `drift-conflict-modal`'s deferred "open diff" verb. [diff-viewer-three-way]
- **Image / binary diff.** Text only.
- **Export as patch.** "Copy as unified diff" affordance. [diff-viewer-export-patch]
- **MCP diff tool.** Agents reason over two blobs via existing read tools; a dedicated diff tool is an optimization. [mcp-tool-diff]


## Deferred

- `diff-viewer-split-view` — side-by-side layout option on `DiffLayer`.
- `diff-viewer-three-way` — third rope input for merge resolution.
- `diff-viewer-ignore-whitespace` — toggle on the compute call.
- `diff-viewer-export-patch` — "copy as unified diff" affordance.
- `activity-detail-diff-between-versions` — multi-select two `changes.db` rows + "Diff selected" opens a tab with `diff = Some(ChangesDb(id_a))` against a buffer whose content is `content_at(id_b)`.
- `mcp-tool-diff` — agent-facing diff IPC.
