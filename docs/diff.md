# Diff

A unified diff primitive plus the editor surfaces that consume it. Every diff in hiker — uncommitted-buffer diff, plain-file snapshot history, pending agent edits, git-revision review — is the same `DiffLayer` rendered against an editor tab.

- **Diff is a mode of the editor tab, not a tab kind.** A tab carries an optional `diff: DiffSource`; when set, it renders the buffer's `current` decorated by `DiffLayer(resolve(diff), current)`. Toggle is in the editor toolbar; no separate `BufferDiff` / `SnapshotPreview` / `StagingPreview` / `TrashPreview` tab kinds. [diff-as-mode]
status:: done
note:: the editor tab carries `Option<DiffSource>`; `diff_overlay::compute` resolves the source and pushes a `DiffLayer`-derived layer onto the same editor widget. Diff toggle in the toolbar flips the active tab's `diff` field; no separate diff-tab kind
- **One diff engine, rendering in the editor crate.** `editor_core::diff::lines(left, right)` is the single line-diff engine (with a word-level intraline pass); `editor-diff` turns its hunks into a `DecorationSet`. There is no separate `core::diff` — the vestigial `hiker_core::diff` line-differ was deleted and its two callers moved onto this engine. (The engine `diff-core-module` and the renderer `diff-renderer` are defined under Module placement below.)


## DiffLayer

The one primitive: two text inputs in, hunks + decoration set out, owner-agnostic. Hosted in the editor crate. [diff-layer]
status:: done
note:: `editor/editor-diff/src/lib.rs` (`DiffLayer`, `DiffOwner`). Constructor takes base + current + owner; `decorations(line_height, theme, intraline)` runs the existing `unified_decorations_opts` and returns a `DecorationSet`. Every diff path in the app routes through this — `panels/buffer/diff_overlay.rs` (diff is a mode of the editor tab via `DiffSource` in `app/src/tab.rs`)

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

The input contract is `{ before, after }` — two ropes in (`app/src/tab.rs`'s `DiffSource` resolves the `before`; the buffer supplies the `after`). Banner actions + close move out of the renderer's contract since each consumer's preview pane owns its own banner + lifecycle. [diff-viewer-input-shape]
status:: done

Owners drive UI affordances, not rendering — the decoration set is identical across owners; only the per-hunk verbs differ (above), riding as overlay widgets on the hunk. [diff-layer-owner]
status:: done
note:: `DiffOwner::{ Index, Pending, Agent, HistoryVersion, Manual }` (`editor/editor-diff/src/lib.rs`). `diff_overlay::compute` picks the owner from buffer state (Agent for hydrated proposals) or the DiffSource shape. Agent owner additionally emits per-hunk Accept/Reject ActionRow widgets via `attach_agent_hunk_widgets`

The layer recomputes hunks each frame from `(base, current)`. Cheap because the inputs are ropes and diff is line-based. No anchor bookkeeping across edits — the diff *is* the state.

The decoration set the layer emits:

- **Line decorations.** Pale-red background for `base`-only lines (deletion), pale-green for `current`-only lines (insertion), default for equal lines. Whole-file paint: the renderer paints full file context with the changes inline (red/green per-line decorations over `editor_core::diff::lines`), not just the changed hunks; the between-hunks `⋯` code path is in place but unreachable until [[spec:diff-viewer-grouped-hunks]] lands. [diff-viewer-line-unified]
  status:: done
- **View zones.** Removed lines from `base` are injected as phantom lines above their successor in `current` (block decorations that affect line height). Same mechanism the editor already uses for chunk-boundary widgets.
- **Intraline marks.** For each paired delete/insert line, a second pass (in the editor renderer over `editor_core::diff`, not `core::diff`) emits `Decoration::mark` ranges with saturated red/green over the pale line background. Controlled by the per-vault `view.intraline_diff` toggle. [diff-viewer-intraline]
status:: done
note:: character-level highlights inside paired delete/insert lines. Renderer adds intra mark decorations on top of the line backgrounds
  - The renderer emits `.cm-diff-add-intra` / `.cm-diff-del-intra` mark decorations keyed by line + UTF-8 byte offsets on top of the line decorations. [diff-intraline-render-marks]
    status:: done
- **Gutter markers.** `DiffAdded` / `DiffRemoved` / `DiffModified` glyphs per hunk. The only thing `Index` ownership emits.
- **Overlay widgets (per hunk).** Owner-driven action buttons positioned at the hunk's first visible line. [diff-layer-hunk-widgets]
status:: done
touches:: [[code:hiker/panels/buffer/diff_overlay]]
note:: `app/src/panels/buffer/diff_overlay.rs` — `attach_agent_hunk_widgets` (Accept/Reject per agent hunk) + `attach_history_version_hunk_widgets` (Restore per history-version hunk). Click ids map through `DiffOverlay.click_map` to `HunkAction::{Accept, Reject, Restore}`. Buffer panel dispatches: `handle_hunk_accept` / `handle_hunk_reject` walk the contributing proposals; `handle_hunk_restore` splices the history version's byte-range text into the on-disk content via `vault.write_file_checked` and appends a `Modified` audit row


## DiffSource

`DiffSource` enumerates where `base` comes from. Each variant resolves to a `Rope` directly — no URI scheme indirection — synchronously off existing services (`vault.read_file`, `app.buffers.get(path)`, `op_writes::content_at_snapshot(path, snapshot_id)`, `vault.read_trash(path)`, the git engine's `show` pass-through); no async, no caching layer; resolved each time the tab activates or its source is invalidated. [diff-source-enum]
status:: done
note:: `app/src/tab.rs` (`DiffSource::{ Disk { path }, Empty, HistoryVersion { op_id, path }, PendingProposal { proposal_id }, GitRef { rev, path } }`). Each variant resolves to a base rope in `diff_overlay::resolve_source_text` (on-disk read, a plain-file snapshot via `op_writes::content_at_snapshot`, git `show`, or empty)

```rust
pub enum DiffSource {
    Disk(PathBuf),                          // on-disk text at read time
    LiveBuffer(PathBuf),                    // another open buffer's current rope
    HistoryVersion { path: PathBuf, snapshot_id: String }, // content_at_snapshot for the snapshot id
    PendingProposal { proposal_id: String },// proposed content for a whole-file agent proposal
    GitRef { rev: String, path: PathBuf },  // the file's content at a git revision
    Trash(PathBuf),                         // trashed file content
    Empty,                                  // empty rope (for diff-against-nothing)
}
```

`LiveBuffer` is the "diff against another open buffer" affordance. The dirty-buffer diff is `base = Disk(path), current = live buffer`.

`GitRef` resolves the file's content at a git revision through the git engine's pass-through to `GitBackend::show` (`git.md` [[spec:git-backend-trait]]; present only when `[git] enabled`). A path absent at the rev resolves to an *empty* base — the whole file reads as added — rather than suppressing the overlay. Owner is `Manual`: viewer only, no per-hunk verbs. `TabKind::git_diff_preview(path, rev)` is the constructor the diff-summary panel's row-open uses (buffer `Vault { path }`, diff `GitRef`), alongside the existing `version_preview` / `pending_preview` helpers. [diff-source-git-ref]
status:: done
implements:: [[code:hiker/tab/impl#[TabKind]git_diff_preview]], [[code:hiker/panels/buffer/diff_overlay/impl#[`Compute<'a>`]resolve_source_text]]
note:: resolution arm in `diff_overlay.rs::resolve_source_text` via `GitSyncEngine::show_at`; the rev is anything `git rev-parse` accepts (`HEAD`, full/short shas, ref names)

`Empty` lets trash entries — which have no current on-disk counterpart — open in editor mode with `diff = Some(Empty)` greyed out, rather than special-casing the tab.


## Editor tab integration

A tab is `Editor { buffer, diff: Option<DiffSource> }` (the only buffer-backed kind; see `editor.md` [[spec:tab-kinds]]). The renderer:

1. Mounts the buffer's editor widget against `buffer.current`.
2. If `diff` is `Some`, resolves the `DiffSource` to a `Rope` `base`, constructs a `DiffLayer { base, current: buffer.current, owner: owner_for(diff) }`, and pushes its decoration set onto the editor's decoration stack.
3. Renders the toolbar's diff toggle as pressed when `diff.is_some()`. Right-clicking the toggle opens a source picker (per `editor.md` `editor-diff-target-picker`).

The toolbar diff toggle is greyed when the buffer is clean *and* the `DiffSource` is `Disk(path)` *and* the path exists. Click toggles the tab's diff mode against the current `DiffSource` (default `Disk(path)`); right-click opens the source picker — `Diff against on-disk`, `Show changes…` (submenu), future sources. [editor-diff-vs-disk-toggle]
status:: done

The buffer's `current` is whatever the buffer normally holds — its rope is unchanged by entering diff mode. Diff is a content lens, not a buffer swap. Cursor and selection survive toggling on and off. Because entering diff mode is not a buffer swap — the tab's `current` is unchanged and decorations layer on top — [[spec:file-switch-guard-dirty]] doesn't fire and dirty-buffer state is preserved across mode toggles. [diff-viewer-respects-dirty-source]
status:: done

`owner_for(diff)` maps: `Disk` / `LiveBuffer` / `GitRef` / `Trash` / `Empty` → `Manual`; `HistoryVersion` → `HistoryVersion` (a snapshot); `PendingProposal` → `Agent`. (Verbs per the `DiffOwner` variants above.)

`Agent` ownership is the inline patch-review path described in `patch-review.md`: the diff source isn't a single proposal — it's the buffer's `agent_base` (the materialized `accepted + working` at the moment proposals were hydrated). That's not a `DiffSource` variant because it's a property of the buffer itself, not a chosen comparison target.

Read-only vs editable is independent of diff mode. History-version / trash sources mark the buffer read-only (no save path). The dirty-buffer diff and the agent diff leave the buffer editable.


## Show changes (snapshot history browser)

Right-clicking inside an editor buffer opens a context menu whose "Show changes…" entry lists recent snapshots for the buffer's path (via `op_writes::snapshot_history`), newest first. Selecting a row switches the active tab into diff mode with `diff = Some(HistoryVersion { path, snapshot_id })`. [editor-show-changes-menu]
status:: done
touches:: [[code:hiker/panels/buffer/show_changes]]
note:: `app/src/panels/buffer/show_changes.rs` (`show_diff_source_menu`). Right-click on the diff toolbar button opens "Diff against on-disk" + "Show changes…" submenu of up to 20 recent snapshots for the path (timestamp, via `op_writes::snapshot_history`). Selecting opens (or focuses) a history-version tab keyed on the snapshot id. Restore-this-hunk verb on HistoryVersion hunks pending ([[spec:diff-layer-hunk-widgets]])

- **Submenu shape.** Up to N=20 recent rows. Each row shows the snapshot timestamp (relative + absolute on hover). Last entry: `Browse all… → ` opens the per-path snapshot history on the home page.
- **No URI scheme.** `DiffSource::HistoryVersion` resolves directly through `op_writes::content_at_snapshot(path, snapshot_id)`. The editor crate doesn't know about URI providers.
- **Per-hunk restore.** Hunks carry a `Restore this hunk` verb that writes `base` (the snapshot's text) back into the buffer for that range and saves (forward-correct — a fresh save / snapshot). Restore-all stays as the row-level restore action. [diff-layer-hunk-widgets]
- **Read-only on history side.** The displayed diff is `base = the snapshot`, `current = live buffer`. The user can keep editing `current` while a historical diff is shown; the diff updates each frame.


## Git diff summary (changed-files viewer)

A read-only viewer over the vault's git history, pairing the summary ("which files changed") with the existing per-tab overlay ("what changed inside"). A singleton `GitDiff` tab holds a base-rev picker fed by the backend `log`, a head-rev pick defaulting to the **working tree**, and the changed-path list from `git.md` [[spec:diff-paths-trait-method]] with per-path Added / Modified / Deleted / Renamed status. Clicking a row opens the file as a normal editor tab with `diff = Some(GitRef { rev: base, path })` — the summary shows *where*, the overlay shows the hunks. Viewer only: no merge / branch / PR verbs (deferred with the PR/merge surface, `git.md`). Rows follow the interaction grammar ([[spec:click-opens]], [[spec:modclick-sticky]], [[spec:rightclick-menu-always]], [[spec:hover-open-signal]]): hover wash + pointer signal the open, click opens into the preview slot, mod-click opens sticky, right-click is a menu (Open / Open diff / Copy path). A path absent from the working tree (deleted, or renamed away) has no buffer to open into: its row keeps only the menu, with the open verbs greyed out carrying the reason. [diff-summary-panel]
status:: done
implements:: [[code:hiker/panels/git_diff/show]], [[code:hiker/panels/git_diff/open_diff_tab]]
note:: `app/src/panels/git_diff.rs`; registered like its sibling singleton tabs — `TabKind::GitDiff`, persist key `:git_diff`, action `vault.open_git_diff` (registry → palette), hamburger row beside Changes. Commit/file lists are cached in `PanelStates::git_diff` and recomputed on pick change or Refresh, not per frame. The right side of a row-open diff is always the live buffer — for a rev↔rev pick the overlay still compares the working file against the base rev (v1 simplification); when no engine is configured the tab renders the how-to-enable hint, same posture as the Sync page


## Module placement

- `editor_core::diff` (editor-core crate) — the single diff engine: `lines(left, right) -> Vec<Hunk>` with line hunks (`HunkKind::{Context,Added,Removed,Modified}`, 0-based `left_lines`/`right_lines` ranges) plus a word-level intraline pass. `similar` confined here. There is no `core::diff`; the former `hiker_core::diff` line-differ was deleted and its callers (the sync fork-diff, the dirty-state gutter) moved onto this engine. [diff-core-module]
status:: done
note:: `core/src/diff.rs` (`compute`, `DiffResult`, `DiffHunk`, `DiffLine`, `DiffOp`). Pure text → diff using `similar`'s `TextDiff::from_lines` + `grouped_ops(3)`; `similar` confined to the module
  - The dead intraline-IPC scaffolding in `hiker_core::diff` (zero callers) was deleted per `scratch/substrate_decision.md`'s safe-now cleanup. Intraline highlighting lives in the editor renderer ([[spec:diff-intraline-render-marks]]) over `editor_core::diff`, which is unaffected. [diff-intraline-core-pair]
    status:: removed
  - Superseded with [[spec:diff-intraline-core-pair]] — `IntralineSpan` deleted from `hiker_core::diff`. [diff-intraline-char-level-v1]
    status:: removed
  - Superseded with [[spec:diff-intraline-core-pair]] — `Line::intraline_spans` field + the `intraline` flag arg deleted from `hiker_core::diff`. [diff-intraline-ipc-flag]
    status:: removed
- `editor::diff` (editor crate, `editor-diff` module) — `DiffLayer`, `unified_decorations`, the intraline mark pass, view-zone construction for removed lines, gutter markers, hunk overlay widgets. Consumes `editor_core::diff` output. [diff-renderer]
- `app/src/panels/buffer/` — the editor tab body. Owns `Editor { buffer, diff }` rendering, the toolbar diff toggle and source picker, the right-click "Show changes" menu, and per-owner hunk-verb dispatch.
- CLI: `hiker diff <path> [<frame-id>]` calls `editor_core::diff::lines` directly and prints a unified diff. [cli-diff]
status:: planned
note:: `hiker diff <path> [<change-id>]` over `editor_core::diff::lines`; lands when the CLI is fleshed out


## Out of scope

- **Side-by-side view.** Layout option on `DiffLayer` (two editor widgets sharing the same hunks). Lands when a user prefers it over unified. [diff-viewer-split-view]
status:: planned
note:: side-by-side layout option on `DiffLayer` (two editor widgets sharing the same hunks). Same primitive; different layout
- **Three-way merge.** Three-rope input + merge resolution UI; the git conflict-marker resolver (`git.md`) and [[spec:drift-conflict-modal]]'s deferred "open diff" verb are the consumers. [diff-viewer-three-way]
status:: planned
note:: third rope slot on `DiffLayer` for merge / drift-conflict resolution; anchors [[spec:drift-conflict-modal]]'s deferred "open diff" option
- **Image / binary diff.** Text only.
- **Export as patch.** "Copy as unified diff" affordance. [diff-viewer-export-patch]
status:: planned
note:: "copy as unified-diff patch" affordance
- **MCP diff tool.** Agents reason over two blobs via existing read tools; a dedicated diff tool is an optimization. [mcp-tool-diff]
status:: planned
note:: reserved as deferred — agents can already retrieve two blobs (`get_note` + `change_content`) and reason over them


## Deferred

- [[spec:diff-viewer-split-view]] — side-by-side layout option on `DiffLayer`.
- [[spec:diff-viewer-three-way]] — third rope input for merge resolution (the git conflict-marker resolver, `git.md`).
- **Ignore whitespace.** A toggle on the `compute` call to normalize whitespace before diffing. [diff-viewer-ignore-whitespace]
  status:: planned
- [[spec:diff-viewer-export-patch]] — "copy as unified diff" affordance.
- **Grouped hunks.** A future `core::diff` entry point emits multiple hunks with bounded context (~3 lines per side); `⋯` separators + click-to-expand land then. Triggered by large-file ergonomics or [[spec:mcp-tool-diff]]. The wire format already accommodates a list of hunks. [diff-viewer-grouped-hunks]
  status:: planned
- [[spec:snapshot-diff-between-versions]] — multi-select two snapshot rows + "Diff selected" opens a tab with `diff = Some(HistoryVersion { path, snapshot_id: a })` against a buffer whose content is `content_at_snapshot(path, b)`.
- **Activity-detail diff between versions.** Multi-select two activity rows + "Diff selected"; opens a `buffer` tab with `current = content_at(id_a)`, `diff = Some(HistoryVersion { op_id: id_b })`; depends on activity-detail multi-select. [activity-detail-diff-between-versions]
  status:: planned
- [[spec:mcp-tool-diff]] — agent-facing diff IPC.
- **Two-sided rev-vs-rev diff in the summary panel.** v1 ([[spec:diff-summary-panel]]) always diffs against the live buffer on row-open even when both picks are revisions; the true rev-vs-rev view needs a read-only buffer materialized at the head rev. [diff-git-two-sided]
status:: planned
