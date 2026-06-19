# Editor

Hiker embeds `egui_editor` — the embeddable text-editor **widget** (`editor/SPEC.md`) — as its `buffer` tab kind inside `egui_workbench` — the IDE **shell** (`egui-workbench/SPEC.md`). This doc covers hiker's *integration* of those two crates: the wiring, policy, and hiker-specific surfaces around them — not the generic editing / shell behavior the crate specs own.

Where this doc points elsewhere:

- The widget's editing model — multi-cursor, selection, decorations, markdown live preview, diff view, find+replace, IME, minimap, the view toggles — is `egui_editor` (`editor/SPEC.md`).
- The shell's chrome — activity bar, side bars, editor groups + splits, tab mechanics, panel area, status-bar chrome, layout persistence — is `egui_workbench` (`egui-workbench/SPEC.md`).
- Hiker's file tree / Files activity content is `files.md`; cluster trees are `cluster-editor.md`.

Transactions, decorations, and selections referenced below are the widget's types.


## Buffer model

One open buffer at a time in v0. Switching files replaces the buffer's contents via a single `dispatch` that swaps the entire doc.

The buffer is the editor's rope — the user's own text, `accepted + working` (per [[spec:op-log-layered-model]]). User typing lands at plain byte offsets; the host mirrors each editor change set into the `working` layer (per [[spec:op-log-working-layer]]), and Save commits that layer. An agent's `pending(session)` proposals render *on top* as an editor-native anchored overlay — a `DiffLayer` recomputed from two ropes (per [[spec:op-log-three-way-overlay]]), reviewed via `patch-review.md`.

State tracked per buffer:

- `path` — vault-relative; null when no file is open [buffer-path-tracking]
status:: done
touches:: [[code:hiker/panels/buffer]], [[code:hiker/vault]]
- `loadedHash` — hash (or full string) of the contents most recently read from / written to disk
- `isDirty` — derived: current doc text !== loadedHash

`isDirty` is the single source of truth for save state — computed lazily from the editor doc and `loadedHash`, no separate flag that can desync. Cleared by re-reads and successful writes; set implicitly by any edit. [buffer-dirty-derived]
status:: done
touches:: [[code:hiker/panels/buffer]]
note:: isDirty computed from doc vs loadedText

Multi-buffer / tabs deferred. When tabs land, the same per-buffer state moves into a `Buffer[]` keyed by path, with the active buffer driving the editor view.

### `transactions_out` seam

The editor widget exposes the change sets it applied from user input — the per-edit `ChangeSet` (retain / delete / insert over byte ranges). The host drains this stream each frame and mirrors each change set into the `working` layer (per [[spec:op-log-working-layer]]), so the editor stays the source of *what changed* rather than the host re-diffing. The same change sets remap the pending overlay's anchors via the editor's `map_pos` (exact, not fuzzy), keeping the agent's anchored ranges in place as the user types (per [[spec:op-log-three-way-overlay]]). Reverse-direction edits (an accepted/pending/external change applied back into the editor) carry a sync origin and are not re-emitted, so the mirror can't echo. The seam is editor-crate-owned and host-agnostic — it feeds any consumer needing a precise edit log. [editor-transactions-out]
status:: done
note:: editor widget exposes the change sets (transactions) it applied from user input so the host mirrors them into the `working` layer and feeds the editor-native anchored overlay ([[spec:op-log-three-way-overlay]]); host-applied edits bypass the sink so reverse-direction edits aren't re-emitted · evidence: `editor-view::command::Action::Replace { state, tx }`; `editor-egui::Widget::with_transactions_sink`; test `editor-egui/tests/transactions_out.rs`


### Embedded buffer view (one note, many places)

A reusable primitive for rendering an **editable** view of a vault note *anywhere* in the UI — not only in its dedicated buffer tab — so the same note can appear in two places at once (a canvas file-node card, a board's Markdown view, a split pane) and "typing on a note shows up wherever the note is." The model is **one shared editor, many views**: [embedded-buffer-view]
status:: done
implements:: [[code:hiker/buffer_view/show_embedded_buffer]]
note:: Phase 1 (primitive only) — no canvas wiring yet. Chrome-free editor body: no minimap/gutter/scrollbar/diff/fold chrome. Markdown live-preview gated by `EmbedOpts.markdown`; `read_only` previews skip the sink + binding. Per-view decoration cache + doc mirror (keeps the unconditional index-diff layer empty since the embed has no gutter). Consumers ([[spec:canvas-inline-edit]], [[spec:board-view-toggle]], split panes) adopt it in Phase 2 · evidence: `app/src/buffer_view.rs`: `EmbeddedView` / `EmbedOpts` / `EmbedResponse` + `show_embedded_buffer`; loads the buffer (`ensure_vault_buffer_loaded`), renders the shared `Editor` via `editor_egui::Widget` against the embed's own `(ViewState, PaintCache)`, drains the transactions sink, runs `editor_binding::run` for `path` (now `pub(crate)`). Decoration rebuild reuses the tab's `panels::buffer::decorations::rebuild_editor_decorations` via a `pub(crate)` `DecoRebuildCtx`

- **Shared (one per path):** the note's `Editor` — document + selection/cursor + undo history — lives in the single `session.buffers[path]` buffer. There is never a second dirty copy of a note; loading a note that already has a dirty buffer just attaches to that buffer. Cursor and undo are shared across every view, because they live on the one `Editor`.
- **Per-view (one per embedding site):** each host owns its own `ViewState` + `PaintCache` (scroll offset, content zoom, wrap, viewport, galley cache). So a 300px canvas card and a full-height tab of the same note scroll and zoom independently while showing the same text.
- **Host-agnostic render call:** a single helper renders `session.buffers[path]`'s `Editor` through the editor widget against a caller-supplied `(ViewState, PaintCache)` at the caller's rect, drains [[spec:editor-transactions-out]], and mirrors the change sets into the `working` layer ([[spec:op-log-working-layer]]) for that path. Because the mirror runs from *whichever host drew the editor this frame*, edits reach `working` even when no buffer tab is open — so save / autosave / agent-review / dirty-tracking work identically regardless of where the editing happened. (Only the focused view receives keystrokes per frame, so the mirror is driven by one host at a time; views render sequentially, never holding two `&mut Editor` borrows at once.)
- **Lifecycle.** Buffer eviction is reference-counted across *all* hosts, not just tabs: a note kept open only by a canvas card (no tab) stays loaded, dirty-tracked, and autosaved until the last host releases it. The tab-only "drop when no tab references this path" rule generalizes to "drop when no tab **or embed** references it." [embedded-buffer-view-lifecycle]
status:: done
touches:: [[code:hiker/editor_pane]]
note:: Simplest correct rule (not a refcount): a dirty buffer kept open only by a non-tab host survives tab close; autosave commits it; once clean + tabless it's dropped on a later close. Canvas-only edits autosave for free — confirmed, no widening needed · evidence: `app/src/editor_pane.rs::close_tab` drops a tabless vault buffer only when `!still_open && !dirty`; autosave already iterates all `session.buffers` per dirty buffer (`main.rs::autosave_tick`) regardless of the active tab. Test: `smoke_tests::dirty_buffer_survives_tab_close_clean_one_is_dropped`

Consumers: the canvas inline editor ([[spec:canvas-inline-edit]]), and — as they adopt it — the board Markdown view ([[spec:board-view-toggle]]) and editor split panes, which today each load the buffer but render their own editor. The buffer tab panel is the reference renderer; the helper is the extracted, chrome-free core it and every embed share.


## Save UX

Save action: commits the buffer's `working` layer (`commit_working`, per `op-log.md`'s "Disk write invariant"), which folds the user's uncommitted edits into `accepted` and materializes that to `currentPath`. On success, updates `loadedHash` to the new doc text, which clears `isDirty`. On error, surfaces a non-blocking error toast and leaves the dirty state alone (so the user can retry).

Triggers (all funnel into the same save function):

| Trigger | Binding / location |
| --- | --- |
| Keybind | Mod-S [save-keybind-mod-s] |
| Toolbar button | Floppy-disk icon left of View options; always visible; disabled when no file is open or not dirty [save-button] |

[save-keybind-mod-s]
status:: done
touches:: [[code:hiker/keybinds]]

[save-button]
status:: done
touches:: [[code:hiker/panels/buffer]], [[code:hiker/toolbar]]
note:: floppy-disk icon, disabled when no file open / not dirty / RO. Lives in the editor toolbar (not the status bar — moved out so the dirty-marker dot didn't need to be duplicated alongside the tab + tree dots) · evidence: `app/src/toolbar.rs` (save action just left of the View-options entry); `app/src/panels/buffer/mod.rs` (save handler + enable/disable)

Save writes the user's file. Crash-recovery autosave (sidecar shadow copies of dirty buffers, NPP shape) is a separate mechanism — see `autosave.md`. The two paths don't overlap: saving clears the autosave sidecar for that path, autosave never touches the user's file.

Dirty indicator:

- Window title shows `• Hiker — <path>` when dirty, `Hiker — <path>` when clean. [dirty-window-title]
status:: done
touches:: [[code:hiker/titlebar]]
- Active file in the tree shows a small dot suffix when its buffer is dirty. The active tab in the strip shows the same dot. [dirty-tree-dot]
status:: done
note:: only updates while file is active buffer · evidence: `app/src/sidebar/files.rs`
- The save button carries no dirty marker — its enabled/disabled state is the signal, and the tab + tree dots cover the rest.

File-switch guard fires on **explicit close** of a dirty tab (× / middle-click / `tab.close` keybind) — a confirm dialog with three options: Save & close, Discard & close, Cancel. Cancel keeps the tab open. Switching away from a dirty tab (tab click / file-tree click / search-result click) does *not* fire the guard — the buffer stays dirty in memory. Window close has no dirty-buffer modal; see `## Multi-buffer model`. [file-switch-guard-dirty]
status:: done
touches:: [[code:hiker/workbench_host]]
note:: nav-time fire path dropped; switching tabs / opening a new file no longer prompts (per [[spec:multi-buffer-no-switch-guard]]). The guard fires only on explicit tab close (× / middle-click / `tab.close` keybind) and is invoked again from the multi-buffer window-close path · evidence: `app/src/workbench_host.rs` (close-tab runs the confirm modal on dirty tabs only)

External changes: a file edited on disk outside hiker reconciles into the `accepted` layer as an `external` op (per [[spec:op-log-external-edit-sync]]). Because the buffer materializes `accepted + working`, an external change and the user's uncommitted `working` edits merge by position: disjoint regions auto-merge with no prompt, and an overlapping region surfaces as a conflict hunk with **Keep mine / Keep theirs / Keep both** — the same model agent proposals use (per [[spec:op-log-merge-auto]], [[spec:op-log-merge-conflict]]).

Save does **not** re-read disk or compare hashes — it commits `working` directly (per the Save action above). Disk drift is reconciled at **open-time** and via the **watcher** (per `op-log.md`), not at save time:

- Open-time reconcile folds any on-disk delta into `accepted` before the buffer shows.
- Watcher integration: the notify-based watcher pushes file-change events for the open file. Buffer clean → silently reload, `loadedHash` updates. Buffer with `working` edits → the same conflict-hunk reconciliation, proactive on the event.

A save-time drift check / `DiskDrift` modal is specced-but-dormant — tracked as bug `bug-editor-no-save-time-drift-check` in `bug_tracking.md`.


## Keybind registry

Two scopes. **Window-level chords** (`app/src/keybinds.rs`) fire regardless of focus; `Keybinds::handle_keybinds(ctx)` runs once per frame before the editor widget and consumes each chord via `ctx.input_mut(|i| i.consume_key(...))`, so a matched window-level chord never reaches the buffer. **Buffer-local chords** (`editor-view::command::handle`) only fire when the editor has focus. The split between the two registries *is* the scope today (a future `scope` field could refine it). Goals: discoverable, overridable (later), conflict-detectable. [keybind-registry]
status:: done
touches:: [[code:hiker/keybinds]]
note:: flat list, validate() on duplicates

Shape: window-level chords are a static `(chord, label)` table from `Keybinds::known_keybindings()` (e.g. `("Mod-S", "Save the active buffer")`), which the F1 overlay enumerates. Validation: a startup test (`known_keybindings_has_no_duplicates`) rejects duplicate chords — no silent overrides.

v0 bindings:

| ID            | Keys  | Action              |
| ------------- | ----- | ------------------- |
| `editor.save` | Mod-S | save current buffer |

Reserved IDs (real impls later, not registered as no-ops in v0):

| ID                     | Keys        | Action                    |
| ---------------------- | ----------- | ------------------------- |
| `vault.openFile`       | Mod-P       | quick-open by filename    |
| `vault.commandPalette` | Mod-Shift-P | open command palette      |
| `editor.toggleBold`    | Mod-B       |                           |
| `editor.toggleItalic`  | Mod-I       |                           |

Override mechanism (deferred): a user keybind file (`vault/.hiker/keybinds.toml`) overrides any binding's `keys` by `id`. The registry's flat-list shape supports this trivially; the loader is later.


## Status bar

Bottom strip across the editor pane only (not under the tree). Three regions: [status-bar-layout]
status:: done
touches:: [[code:hiker/panels/buffer]]
note:: three regions

- left: a **version dropdown** for the active buffer's file. Closed-state label is the basename plus a mode qualifier when a non-current version is selected (e.g. `note.md`, `note.md — Snapshot 2m ago`, `note.md — Staging · chat`). The full vault-relative path stays in the `title=` tooltip on hover. See `## Version dropdown` below. [status-bar-version-dropdown]
status:: done
touches:: [[code:hiker/panels/buffer]]
note:: left click on the status path opens a popover listing Current + snapshot rows (via `op_writes::snapshot_history`) + pending proposals. Closed-state label is `basename` plus a `— <mode qualifier>` suffix when a non-current version is in view. Buffer-only — the popover is forced closed for non-buffer tab kinds and trash previews · evidence: `app/src/panels/buffer/mod.rs` (status-bar version dropdown)
- center: index status label (v1+) — short text reflecting indexer state. Concretely: `Model loading…` while the embedder loads, `Indexing X/Y` while jobs flow (X = remaining queue depth, Y = total since last idle), `Indexed (N notes)` when idle, `Index error` (with last_error in title attribute) when the indexer reports a failure. Plain text, no icons in v1; styling can come later. [status-bar-index-label]
status:: done
touches:: [[code:hiker/panels/buffer]]
note:: model loading / indexing / indexed / error

  When the *active buffer*'s file is in a non-indexed state (per [[spec:cmd-file-index-state]] in `index.md`), the center label is replaced for that file's lifetime as the active buffer with a file-specific message: `Not indexed (unsupported filetype)` for unsupported extensions, `Skipped — <reason>` for skipped files (reason string straight from the indexer), `Queued for indexing` while the file's job is pending. Reverts to the aggregate label once the file becomes indexed (or another file opens). [status-bar-active-file-index-state]
status:: done
touches:: [[code:hiker/panels/buffer]]
note:: center label swaps to "Not indexed (unsupported filetype)" / "Skipped — <reason>" / "Queued for indexing"; reverts to aggregate label when active buffer is Indexed or trash-preview · evidence: `app/src/panels/buffer/mod.rs` (index-status rendering)
- right: line:col, word count, file type badge (`md`)

Click targets:

- dropdown → opens the version list (see below).
- right-click on the dropdown's closed-state label → context menu with "Reveal in file manager" (Finder on macOS, File Explorer on Windows, default file manager on Linux; via the OS shell/opener). Suppressed for trash-preview / snapshot / staging buffers so internal `.hiker/` paths don't leak. [status-bar-path-reveal]
status:: done
touches:: [[code:hiker/panels/buffer]]
note:: macOS `open -R`, Windows `explorer /select,`, Linux `xdg-open <parent>` (Linux has no portable select-file verb). Right-click action on the version-dropdown closed-state label (moved off the basename click target by [[spec:status-bar-version-dropdown]]). Suppressed for trash-preview / snapshot / staging buffers so internal `.hiker/` paths don't leak · evidence: `app/src/panels/buffer/mod.rs` (status-path click handler)
- line:col → opens a goto-line input (deferred; click is a no-op in v0) [status-bar-goto-line]
status:: done
touches:: [[code:hiker/panels/buffer]]
note:: click line:col status indicator opens a "Go to line" popup menu with a numeric text edit (persisted per-path via `egui::Id::new(("goto-line", path))`); Enter on a valid `1..=total_lines` value moves the buffer's selection to `line_to_byte(n-1)`


### Version dropdown

The left region of the status bar is a single dropdown that lists every addressable version of the active buffer's file. Selecting an entry switches the editor view to that version (live editor, snapshot preview, or staging preview), without changing tabs or pane state. [status-bar-version-dropdown]

Entries, in fixed group order, newest within each group:

1. **Current** — the live, editable on-disk version. Always present, always the first entry. Selecting it exits any snapshot / staging preview the buffer is in and returns to the editable buffer (same code path as the existing exit-preview transitions).
2. **Snapshots** — every plain-file snapshot for this path within `[history]` retention (`op_writes::snapshot_history`), one entry per snapshot. Label: `Snapshot · <relative-time>`. Selecting an entry enters [[spec:snapshot-preview-mode]] against that snapshot. The current on-disk state already appears as the top "Current" entry, so the most-recent snapshot row is *not* hidden — it represents the saved version, which may diverge from the live buffer if the user has unsaved edits. (When git is integrated, git revisions are available too via the `Show changes` / git-diff surfaces, `git.md`.)
3. **Pending proposals** — every pending whole-file proposal whose `target_path` equals this file (`op_writes::list_whole_file_proposals`). Label: `Proposal · <surface> · <relative-time>`. Selecting an entry opens the proposal's content as a read-only proposal preview (same code path as clicking a row on the `PatchReview` tab).

The selected entry reflects what's currently in view. Closed-state label mirrors that selection (e.g. `note.md — Snapshot 2m ago · agent:claude`), so the user can tell at a glance which version the editor is showing without opening the dropdown. Mode-specific verbs (Restore, Accept / Reject, Diff toggle) stay in the editor toolbar's `#mode-controls` slot per [[spec:editor-toolbar-mode-controls]]; the dropdown is purely a version selector. [status-bar-version-dropdown-selection]
status:: done
touches:: [[code:hiker/panels/buffer]]
note:: Current → opens the file non-preview (exits any preview); snapshot row → opens snapshot preview; staging row → opens staging preview. Closed-state label mirrors the selection. Restore/Accept/Reject/Diff verbs unchanged — still live in the mode controls · evidence: `app/src/panels/buffer/mod.rs` (dropdown selection handlers)

The dropdown is buffer-only — it hides for non-buffer tab kinds the same way the rest of the status bar does ([[spec:tab-kinds]]). For a buffer whose file does not yet exist on disk (newly-created, never saved), only the "Current" entry appears.

Population:

- Snapshot entries come from `op_writes::snapshot_history(path)` (the plain-file snapshot tree, `op-log.md` "Local history"); pending-proposal entries from the op-log pending query. [status-bar-version-dropdown-uses-unified-feed]
status:: done
touches:: [[code:hiker/panels/buffer]]
note:: snapshot rows from `op_writes::snapshot_history(path)` + pending-proposal rows from the op-log pending query · evidence: `app/src/panels/buffer/mod.rs` (dropdown population), `core/src/snapshot.rs` (`list_snapshots`)
- The list refreshes on op-log append events and staging-snapshot updates for events touching the active buffer's path; debounced consistent with the activity widget. [status-bar-version-dropdown-live-refresh]
status:: done
touches:: [[code:hiker/panels/buffer]]
note:: re-fetch fires only when the event's path matches the active buffer; only re-renders if the popover is currently open · evidence: `app/src/panels/buffer/mod.rs` (dropdown refresh debounced 150ms; subscribes to op-log change events)

Trash entries are out of scope — a trash entry *is* a different file on disk (different path), not a version of the open buffer; surfaced via [[spec:tree-trash-preview]].


### Sibling protection (overflow rule)

Every status-bar region — and any horizontal toolbar / strip elsewhere in the app — truncates user-derived content (file names, error messages, status labels) with an ellipsis so a long string in one region can't push its siblings off-screen. [ui-no-sibling-pushout]
status:: done
touches:: [[code:hiker/panels/buffer]], [[code:hiker/titlebar]]
note:: applied to status-bar regions and vault-bar; rule documented in source comment · evidence: `app/src/panels/buffer/mod.rs` (status bar), `app/src/titlebar.rs` (vault bar)


## Layout

The four-region workbench shell — activity bar, side bars (as accordion sections), editor groups + splits + resizable splitters, panel area, status-bar chrome, and layout persistence — is `egui_workbench` (`egui-workbench/SPEC.md` §1–§8). This section covers only hiker's wiring of that shell: which activities mount, what fills the editor toolbar, and the hiker-specific panels.

Hiker's region map:

- **Top strip**: a single horizontal strip across the full width of the window — leading cluster of icon buttons (Back / Forward / Home / Queue / Settings / Open vault) plus the active vault path label, then the tab strip filling the rest. Hiker-specific buttons + behavior are in `## Top strip` below. [top-strip-layout]
status:: done
touches:: [[code:hiker/workbench_host]]
note:: leading-cluster icons (Back / Forward / Home / Queue / Settings / Open vault) + relocated vault-path + tab strip filling the rest. Replaces the prior vault-bar block at the top of the sidebar · evidence: `app/src/workbench_host.rs` (top strip + two-row layout)
- **Left (primary side bar)**: hosts the Files / Cluster-trees / Trails activities. The file tree is `files.md`; cluster trees are `cluster-editor.md`. The side-bar / accordion mechanics (sections, headers, collapse, resize, drag-to-add, persistence) are `egui_workbench`. The sidebar collapse toggle is [[spec:sidebar-toggle-icon]] below. [four-region-layout]
status:: done
touches:: [[code:hiker/workbench_host]]
note:: top strip across the full width holds leading-cluster icons + vault-path + tab strip; three-column grid below absorbs the rest. Sidebar / discovery collapse rules unchanged · evidence: `app/src/workbench_host.rs` (top strip + three-column layout below)
- **Center (editor area)**: the editor pane — a thin toolbar strip across its top, the `egui_editor` widget below, the status bar beneath. Toolbar contents are hiker-specific; see the editor-toolbar wiring below.
- **Right (discovery panel)**: related-notes panel. Renders `RelatedHit[]` from `related_notes(currentPath)`, updated on file-open and on save (debounced 500ms per `index.md`). [related-notes-panel-ui]
status:: done
note:: refresh wired on file-open, debounced-save, and explicit calls; cleared on vault swap · evidence: `app/src/panels/related.rs` (`State`, `View`, `show()`)

### Editor-toolbar wiring

The editor pane's toolbar (hiker chrome, not the widget's) holds, left-to-right: the sidebar toggle, Save (floppy icon), the dirty-buffer Diff toggle ([[spec:editor-diff-vs-disk-toggle]]), a centered `#mode-controls` slot between two flex spacers (see `## Mode controls slot`), then the View menu button (eye icon, `## View options menu`), the Mutations menu (wand, `## Note-mutations menu`), and the discovery-panel toggle. The two panel-toggle buttons are always visible; their pressed/unpressed state reflects whether the corresponding side panel is open. [panel-toggle-buttons]
status:: done
touches:: [[code:hiker/toolbar]]
note:: sidebar + related toggles

- **Sidebar toggle icon.** A safe-dial / ship-wheel glyph (round with spokes) inside a rounded-square frame. Tooltip "Toggle sidebar." [sidebar-toggle-icon]
status:: done
touches:: [[code:hiker/toolbar]]
note:: safe-dial glyph (rounded-square frame around a circle with spokes); "Toggle sidebar" tooltip; click handler unchanged · evidence: `app/src/toolbar.rs` (sidebar toggle)
- **Discovery toggle icon.** A magnifying glass (the panel's primary surface is search-driven retrieval, per `search.md`). Tooltip "Toggle discovery panel." [discovery-toggle-icon]
status:: done
touches:: [[code:hiker/toolbar]]
note:: magnifying-glass glyph; "Toggle discovery panel" tooltip (was "Toggle related notes"); click handler unchanged · evidence: `app/src/toolbar.rs` (discovery toggle)

Default state on first launch: tree open, related panel collapsed. Persistence of these toggles rides the workbench layout persistence (per-vault); side-column resize, min/max clamps, and the resize handles are `egui_workbench` mechanics. [side-panel-resize]
status:: done
implements:: [[code:hiker/config/sections/VaultConfig#sidebar_width]], [[code:hiker/config/sections/VaultConfig#discovery_width]]
touches:: [[code:hiker/workbench_host]]
note:: drag handle on inner edge of sidebar and discovery panel; `col-resize` cursor on hover; per-vault width persistence; min/max clamps; toggle still hides wholesale · evidence: `app/src/workbench_host.rs` (sidebar + discovery resize handles; live drags reflow the editor; collapse rules still pin the column to 0 so toggle is "hide wholesale" not "drag to 0"), `core/src/config.rs` (`VaultConfig.sidebar_width` / `discovery_width` `u32` with 280 / 320 defaults; eligible-key rows added as `PositiveInt`)


## Mode controls slot

The editor toolbar reserves a centered `#mode-controls` slot between two flex spacers. The slot is empty during normal editing; entering a read-only preview mode populates it with mode-specific icon-only buttons plus a short text label naming the mode. One slot, one render path, per-mode populators. [editor-toolbar-mode-controls]
status:: done
note:: centered slot between flex spacers; populated with mode-specific label + icon-only buttons whenever the active buffer's tab carries a `DiffSource` that owns verbs (snapshot restore, proposal accept/reject). Idempotent rebuild via `replaceChildren()` on every transition

What lands in the slot:

- **Icon-only action buttons** for the mode's verbs: Diff toggle ([[spec:editor-diff-vs-disk-toggle]] below), Restore, Apply, Reject, Close — whichever the active mode exposes. Icons match the toolbar palette; stateful icons reflect toggle state. The mode qualifier naming which non-current version is in view sits in the version dropdown's closed-state label ([[spec:status-bar-version-dropdown]]), keeping the toolbar compact.

The render path reads buffer state (mode kind, dirty flag, diff-active) and rebuilds the slot on every affecting transition (buffer swap, mode entry/exit, dirty toggle, diff on/off). Per-mode populators (snapshot, trash, dirty-buffer, future staging) each render label + icons and mutate nothing directly — state changes go through the buffer/preview API.

### Dirty-buffer Diff toggle

A diff toggle lives in the editor toolbar (just right of Save). Greyed when the buffer is clean *and* no other diff source is selected (nothing to diff against). Click toggles the editor tab's `diff` mode against the current `DiffSource` (see `diff.md` [[spec:diff-as-mode]] and [[spec:diff-source-enum]]); the default source is `Disk(path)` — the live buffer vs. last-loaded content. The flip is non-destructive: the buffer's `current` is unchanged, decorations are layered on top; toggling off restores cursor + selection. **Right-click opens a source picker** — a small context menu offering: `Diff against on-disk`, `Show changes…` (submenu of recent op-log rows for this path), and future sources (snapshot, another open buffer). Selecting a source switches the tab's `DiffSource` and turns diff mode on. [editor-diff-vs-disk-toggle, editor-show-changes-menu]

Constraints:

- **Disabled when there's nothing to diff.** Buffer clean *and* `DiffSource` is `Disk(path)` *and* the path exists on disk → toggle is disabled with tooltip "No changes to show."
- **Newly-created buffer (file not on disk yet).** Toggle is disabled with tooltip "Save first to diff against disk." The source picker still works for non-disk sources (e.g. another open buffer).

### Show changes menu

The right-click context menu on the diff toggle (and the buffer's body, when no selection is active) carries a `Show changes…` entry whose submenu lists recent plain-file snapshots for the active buffer's path (via `op_writes::snapshot_history`), newest first. Selecting a row sets the tab's `DiffSource = HistoryVersion { path, snapshot_id }` and turns diff mode on; the buffer's `current` text stays put, and `agent_base` (if any) is unaffected. [editor-show-changes-menu]

- **Submenu shape.** Up to 20 recent rows. Each row shows the snapshot timestamp (relative + absolute on hover). Final row: `Browse all… → ` opens the per-path snapshot history on the home page.
- **Per-hunk restore.** When the diff source is `HistoryVersion { path, snapshot_id }`, hunks carry a `Restore this hunk` overlay verb (owner `Snapshot` per `diff.md`'s [[spec:diff-layer-owner]]). Restore writes the snapshot's text for that hunk's range into `current` and lets the user save through the normal path. Full-snapshot restore stays on the row-level surface, unchanged.
- **No URI scheme.** The diff resolves directly through `op_writes::content_at_snapshot(path, snapshot_id)`; the editor crate doesn't go through a custom URI provider.


## Find in note

In-buffer find / replace is the `egui_editor` search panel (`editor/SPEC.md` §6, §9.13) — triggered by Mod-F, with case / whole-word / regex / in-selection toggles, match highlights, and gutter + minimap match ticks. Hiker enables that panel on the buffer tab kind; it doesn't re-implement it. [editor-find-in-note]
status:: done
touches:: [[code:hiker/keybinds]], [[code:hiker/panels/buffer/find]]
note:: Mod-F in-buffer search: forward/back step, case + regex toggles, match highlights; substring + case-insensitive defaults; no replace. **Gutter/minimap match ticks deferred.** · evidence: `app/src/panels/buffer/find.rs`, `app/src/keybinds.rs` (Mod-F), `editor-view::find`

Hiker boundary: in-buffer find is "jump to this string in *this* file." Cross-file find is the discovery panel's job (per `search.md`); the in-buffer bar must not grow into a second search surface.


## Reader / focus mode

A workbench-level focus mode that hides all chrome except the global top bar and focuses the active tab full-window — aimed at distraction-free reading and the long-form writing case. A single session-level flag on the workbench, not per-buffer, so it works on any focused tab. Not persisted. [view-reader-mode]
status:: done
touches:: [[code:hiker/actions]], [[code:hiker/icons]], [[code:hiker/keybinds]], [[code:hiker/workspace]]
note:: Workbench-level focus mode: a single session flag (not per-buffer) hides all chrome except the global top bar — activity bar / both side bars / status bar / panel area gated at render time in `Workbench::ui` (the `visible` booleans are never mutated, so collapse choices + persistence survive), plus the editor's own gutter / minimap / status bar. The active tab fills the window. The top bar, tab strip, and per-view toolbars stay by default — see the three opt-in `view-reader-hide-*` toggles. Toggled by Ctrl+R, the book-icon top-strip button, the eye-icon View menu, and the editor toolbar menu. Esc exits. Not persisted. Supersedes the old per-buffer `editor-reader-view` · evidence: `egui-workbench/src/workspace.rs` (`Workbench::reader_mode` flag + `toggle_reader_mode`, render-time chrome gate), `app/src/actions.rs` (`view.reader_mode`, `editor.reader_view` delegates), `app/src/keybinds.rs` (Ctrl+R dispatches `view.reader_mode`), `app/src/state.rs` (`view.reader_mode` book button in default top toolbar), `app/src/icons.rs` (`Icon::Book`)

- **Trigger.** Ctrl+R (Cmd+R) and a global book-icon button on the top strip. Also reachable from the global eye-icon View menu ([[spec:global-view-menu]]) and the editor toolbar menu as a regular toggle row. **Right-clicking the book icon** opens the reader-view-specific options (the hide toggles below) as a context menu, so they're reachable straight from the reader icon.
- **Exit.** The same toggle, or Esc.
- **What's hidden by default.** Every workbench chrome region — activity bar, both side bars, status bar, panel area — plus the editor's own status bar / gutter / minimap. The active tab fills the window. The global top bar, the tab strip, and each view's in-tab toolbar all stay by default; three opt-in toggles hide them.
- **Optional hide toggles.** Independent reader-mode settings, all shown in the eye View menu, the book-icon right-click menu, and Settings; each takes effect next frame, vault-scoped:
  - **Top bar** — `ui.reader_hide_top_bar` hides the global top bar entirely (custom titlebar or native top toolbar). In frameless mode the window resize grips remain and Ctrl+R is the exit. [view-reader-hide-top-bar]
status:: done
touches:: [[code:hiker/config/patch]], [[code:hiker/panels/settings]]
note:: `ui.reader_hide_top_bar` setting: when on, reader mode also hides the global top bar (custom titlebar or native top toolbar). In frameless mode the window resize grips stay and Ctrl+R is the exit. Vault+user scope; takes effect next frame (no restart). Toggleable from Settings, the View menu, and the book-icon right-click menu · evidence: `core/src/config/mod.rs` (`Ui::reader_hide_top_bar`), `core/src/config/patch.rs` (eligible bool both scopes + test), `app/src/main.rs` (top-bar suppression + resize grips kept in frameless), `app/src/panels/settings/mod.rs` (window section checkbox), `app/src/state.rs` (`UiState::reader_hide_top_bar`)
  - **Tabs** — `ui.reader_hide_tabs` suppresses the editor-area tab strip via the workbench's `hide_tab_strip` render-time gate (`tab_bar_height` → 0 + a no-paint `tab_ui`); the tabs and layout are untouched, so the strip returns when cleared. [view-reader-hide-tabs]
status:: done
touches:: [[code:hiker/config/patch]], [[code:hiker/editor_area]], [[code:hiker/panels/settings]], [[code:hiker/toolbar]], [[code:hiker/workspace]]
note:: `ui.reader_hide_tabs`: when on, reader mode also hides the editor-area tab strip via the workbench's render-time `hide_tab_strip` gate (tabs/layout untouched). Shown by default. Vault scope, next-frame. Toggleable from the eye View menu, the book-icon right-click, and Settings · evidence: `core/src/config/mod.rs` (`Ui::reader_hide_tabs`), `core/src/config/patch.rs` (eligible bool both scopes), `egui-workbench/src/workspace.rs` (`hide_tab_strip` field + `set_hide_tab_strip`), `egui-workbench/src/editor_area.rs` (`EditorBehavior::hide_tab_strip` → `tab_bar_height` 0 + no-paint `tab_ui`), `app/src/main.rs` (sets `hide_tab_strip = reader && setting`), `app/src/state.rs`, `app/src/panels/settings/mod.rs`, `app/src/toolbar.rs`
  - **Toolbar** — `ui.reader_hide_toolbar` hides each view's in-tab toolbar (the canvas create toolbar, the editor toolbar). Gated through `AppState::reader_hides_view_toolbar`. [view-reader-hide-toolbar]
status:: done
touches:: [[code:hiker/config/patch]], [[code:hiker/panels/buffer]], [[code:hiker/panels/canvas/render]], [[code:hiker/panels/settings]], [[code:hiker/toolbar]]
note:: `ui.reader_hide_toolbar`: when on, reader mode hides each view's in-tab toolbar (canvas create toolbar, editor toolbar) through the `reader_hides_view_toolbar` predicate. Shown by default. Vault scope, next-frame. Toggleable from the eye View menu, the book-icon right-click, and Settings · evidence: `core/src/config/mod.rs` (`Ui::reader_hide_toolbar`), `core/src/config/patch.rs` (eligible bool both scopes), `app/src/state.rs` (`AppState::reader_hides_view_toolbar` predicate), `app/src/panels/canvas/render.rs` (canvas create toolbar gated), `app/src/panels/buffer/mod.rs` (editor toolbar gated), `app/src/main.rs`, `app/src/toolbar.rs`, `app/src/panels/settings/mod.rs`
- **Scope.** Switching tabs stays in reader mode and focuses the new tab. The flag gates chrome at render time only — the user's collapse choices and layout persistence are untouched.


## Command palette

Fuzzy-search popover over every registered keybind action — the discoverability surface for the keybind registry ([[spec:keybind-registry]]). [command-palette]
status:: done
touches:: [[code:hiker/panels/command_palette]]
note:: fuzzy-search popover over the keybind registry's `known_keybindings()`. Opened by `Mod-Shift-P` / `Ctrl-K` or the `palette.open` action. Shows action title + area badge + bound chord; AI-touching actions hidden under [[spec:llm-features-disable-entirely]]. State lives on `UiState` (`palette_open` / `palette_query` / `palette_selected` / `palette_mru`); dispatch through the same path the keybind handler uses · evidence: `app/src/panels/command_palette.rs` (`AppState::command_palette`)

- **Trigger.** Keybind `vault.commandPalette` = Mod-Shift-P (reserved in [[spec:keybind-registry]]'s "Reserved IDs" table; this spec lights it up), and a top-strip icon when wired.
- **Surface.** A centered overlay popover above the editor pane: a text input at the top, a scrollable result list below, footer hint listing accept / dismiss bindings.
- **Action source.** The keybind registry is the source of truth — every entry in `Keybinds::known_keybindings()` is a palette row. Adding a registry entry adds a palette row for free.
- **Row shape.** Action title (the registry's human label), source area as a small badge ("editor" / "tab" / "navigation" / "vault" / etc.) inferred from the action's id prefix, and the bound chord on the right (or `Unbound` when no chord is set). Greyed rows when the action isn't currently dispatchable (e.g. `editor.save` when no buffer is open).
- **Ranking.** Fuzzy match on the human label first, then on the action id. Recent invocations float up via a small per-session MRU list — same shape as the chat `@`-autocomplete recency tiebreaker, in-memory only.
- **Invocation.** Enter (or click) fires the action through the same dispatch path the keybind handler uses — palette is a discovery surface, not a parallel runtime. Esc dismisses.
- **No payload prompting in v1.** Actions that take arguments (a future "Open file by name" action) aren't in the palette until their entry-point becomes a side-effect-free invocation; palette rows are zero-argument verbs. Picker-driven actions (open vault, open recent) plug in by registering a no-arg "open the picker" verb, not by spawning their UI from the palette.
- **AI-touching actions are hidden under `[llm] enabled = false`** (per [[spec:llm-features-disable-entirely]]). The filter runs at render time so a flip applies live.
- **Module placement.** Popover lives in `app/src/panels/command_palette.rs`; the action list it reads is `Keybinds::known_keybindings()` plus per-action metadata (label, area badge, dispatchable predicate).

The palette coexists with right-click context menus and the View menu — keyboard-first answer, menus stay mouse-first.


## Click selection patterns

Double/triple-click word/line selection is `egui_editor` (`editor/SPEC.md` §2.2). Hiker layers one thing on top: **what** a click selects is regex-configurable via `[editor]` config — each click runs `view.{double,triple}_click_re` against the clicked line and selects the match whose span contains the cursor column. There is no separate "built-in" path; the historic Unicode-word / whole-line behavior is just the default regex. An empty config value resets to the default; an invalid regex logs once and falls back to the default, so a typo can never break selection. [click-select-pattern]
status:: done
touches:: [[code:hiker/buffer]], [[code:hiker/command]], [[code:hiker/config/patch]], [[code:hiker/config/sections]], [[code:hiker/panels/settings]], [[code:hiker/viewport]]
note:: configurable double/triple-click via per-click regex (`[editor].double_click_pattern` / `triple_click_pattern`). **Regex everywhere — no separate built-in path:** defaults `\w+` / `.*\n?` reproduce the historic Unicode-word / whole-line-incl-newline behavior. cc=2 matches against `line_str(line)`; cc=3 matches against `slice(line_start..next_line_start)` so the line carries its trailing `\n`. Empty config value resets to default; invalid regex logs once and falls back to default. Settings panel shows the active values (defaults visible). Integration tests `double_click_pattern_includes_hyphen` + `triple_click_pattern_overrides_whole_line` in `editor/editor-view/tests/multiclick.rs` · evidence: `core/src/config/sections.rs` (`EditorConfig::{double,triple}_click_pattern` + `default_*_pattern` serde defaults), `core/src/config/patch.rs` (eligibility), `editor/editor-view/src/viewport.rs` (`DEFAULT_{DOUBLE,TRIPLE}_CLICK_PATTERN` + `LazyLock<Arc<Regex>>` defaults + `ViewState::{double,triple}_click_re: Arc<regex::Regex>`), `editor/editor-view/src/command.rs` (`pattern_span_at`, single-regex-path `mouse_down` cc=2/3), `app/src/buffer.rs` (`compile_click_pattern` with default fallback), `app/src/panels/settings/mod.rs` (settings rows)

```toml
[editor]
# Defaults (always valid, always shown in the settings panel):
double_click_pattern = "\\w+"     # Unicode word (foo-bar splits at "-")
triple_click_pattern = ".*\\n?"   # whole line incl. trailing newline

# Examples:
#   double_click_pattern = "[\\w-]+"   # select hyphenated words whole
#   double_click_pattern = "\\S+"      # select runs of non-whitespace
#   triple_click_pattern = ".*"        # line content WITHOUT the newline
```

- The double-click matcher runs against the line content (no trailing newline); the triple-click matcher runs against the line including its trailing `\n` when present — that's why the default `.*\n?` reproduces the previous whole-line-incl-newline behavior (`\n?` matches zero on the last line).
- Defaults live as `pub const`s + `LazyLock<Arc<Regex>>` in the editor view layer; the `core::config` serde defaults inline the same strings and are documented to stay in sync.
- Patterns compile once per buffer-open into `ViewState.{double,triple}_click_re: Arc<regex::Regex>` (non-`Option`). Changing the config and reopening the buffer picks up the new pattern.


## View options menu

The editor pane's top toolbar ([[spec:panel-toggle-buttons]]) gains a View menu button alongside the tree- and related-panel toggles. The menu hosts display-only toggles — flips that change how the active note is rendered without touching the file or the index. Sibling to the deferred [[spec:note-mutations-menu]]; the split is clean: View changes pixels, Mutations changes bytes. [editor-view-options-menu]
status:: done
touches:: [[code:hiker/panels/buffer]], [[code:hiker/toolbar]]
note:: View-options button on the editor toolbar between the sidebar and related toggles; opens the menu with checkable rows. State in-memory only per spec · evidence: `app/src/toolbar.rs` (View-options menu), `app/src/panels/buffer/mod.rs` (menu items)

**Icon.** Eye glyph, no text label, no chevron — matches the icon-only treatment of the other toolbar buttons (sidebar wheel, discovery magnifying glass). Tooltip "View options" handles discoverability for the icon-only form. The click target opens the menu directly, same shape the other icon buttons use to open their popovers. [view-menu-icon]
status:: done
touches:: [[code:hiker/toolbar]]
note:: eye glyph (outline + pupil) replaces the text-and-chevron View label; "View options" tooltip; click handler unchanged · evidence: `app/src/toolbar.rs` (View-options button)


### Toolbar icon palette

The editor pane's top toolbar is converging on icon-only affordances; each menu / button gets a single distinctive glyph in the same visual family (line-weight, frame, sizing). Reserved glyphs:

| Affordance                  | Glyph                                  | Slug                  | Status   |
| --------------------------- | -------------------------------------- | --------------------- | -------- |
| Sidebar toggle              | safe-dial / ship-wheel (rounded-square frame, circle with spokes) | [[spec:sidebar-toggle-icon]] | landed   |
| Discovery toggle            | magnifying glass                       | [[spec:discovery-toggle-icon]] | landed |
| View menu                   | eye                                    | [[spec:view-menu-icon]]      | landed   |
| Mutations menu              | wand                                   | [[spec:mutations-menu-icon]] | landed (live via [[spec:note-mutations-menu]]; mutation roster grows under that slug) |

Sidebar-scoped icons (the `+` new-item button, `⋯` actions menu) live in the side-bar panel header, not the editor toolbar — see `files.md`. The activity switcher between Files / Cluster trees / Trails is `egui_workbench`'s activity bar (`egui-workbench/SPEC.md` §1).

Each entry is a checkable item — checkmark when active, click flips it, menu closes on click. State is in-memory only for v1; persistence is a `settings.md` concern when that surface lands.

### v1 entries

- **Show chunk boundaries** — overlays a thin horizontal rule between chunks (pale reddish-orange) and the chunk index in the gutter at each chunk's start line. Backed by [[spec:cmd-chunks-for-path]] (`index.md`). Refreshes on save (debounced 500ms, same cadence as the related-notes panel). When the file isn't indexed (unsupported / skipped / queued per [[spec:cmd-file-index-state]]), toggling on shows nothing and a faint gutter hint explains why. Editor integration: a decoration provider (`chunk_boundary_decorations`) emitting the rule + gutter index onto the buffer view's decoration set. A debugging-grade view of the chunker's output. [view-show-chunk-boundaries]
status:: done
touches:: [[code:hiker/panels/buffer]]
note:: line-decoration boundary rule + dedicated gutter showing chunk indices. Toggled via View menu; default off. Refreshes on file-open, on save (500ms debounce, same cadence as related), and after watcher silent-reload. Faint gutter hint shown when the file is unsupported / skipped / queued / has zero chunks · evidence: `app/src/panels/buffer/mod.rs` (chunk-boundaries decoration + gutter)

- **Hide frontmatter** — visually collapse the leading `---\n…\n---\n` YAML block into a single placeholder line (`▸ frontmatter (N lines)`) without touching the file. Detection mirrors `core::frontmatter::split` exactly — the block must start at byte 0 with `---\n` and have a closing `---\n` line before any body content; an unterminated or non-leading block is ignored. Editor integration: a block replace decoration (`frontmatter_fold`) over the byte range, recomputed off the document so edits update the placeholder line count immediately. Default off; persistence via `editor.hide_frontmatter` ([[spec:settings-section-editor]]). [view-hide-frontmatter-toggle]
status:: done
touches:: [[code:hiker/panels/buffer]]
note:: egui decoration layer (`Decoration::Block`) over the leading `---\n…\n---\n` range with a `▸ frontmatter (N lines)` widget, recomputed off the document so live edits update the count. Detection caps the closing-`---` search at 1000 lines to bound the scan; unterminated blocks are no-op. Default off; persists via [[spec:settings-write-back]] · evidence: `app/src/panels/buffer/mod.rs` (hide-frontmatter toggle + View-menu entry), `core/src/config.rs` (`editor.hide_frontmatter`)

- **Intraline diff highlights** — augments the line-level red/green diff with character-level highlights inside paired delete/insert lines. Affects every consumer that calls `editor.renderDiff` (snapshot preview, dirty-buffer diff, write-note review). Default off; persistence via `editor.intraline_diff` ([[spec:settings-section-editor]]). Flipping while a diff is displayed re-renders it with the new style. Does *not* affect the patch-review agent-diff surface (own rules in `patch-review.md`). Full rendering contract in `diff.md`'s "Diff style" section. [view-intraline-diff-toggle]
status:: done
implements:: [[code:hiker/config/sections/EditorConfig#intraline_diff]]
note:: "Intraline diff highlights" View-menu entry flips `editor.intraline_diff`; affects every `DiffLayer` consumer. Toggling while a diff is rendered re-runs the compute

### Reserved entries (greyed in v1, enabled when their backing feature lands)

These appear in the menu now so the surface is predictable, but render greyed-out with a tooltip naming the dependency.

- **Live preview** — hide/show markdown syntax markers on cursor-out. Specced in `live-preview.md`; entry becomes live (default on) when that ships. [view-live-preview-toggle]
status:: done
touches:: [[code:hiker/panels/buffer]]
note:: toggles live preview; checkmark reflects the enabled flag; default on · evidence: `app/src/panels/buffer/mod.rs` ("Live preview" menu entry)
- **Render .txt as markdown** — session-scope override of [[spec:txt-render-as-markdown-default]] (flip the vault default for the current app session; no file mutation, no persistence in v1). Greyed until [[spec:settings-vault-config-toml]] lands a per-vault default loader; see `txt-ingest.md`. [view-render-txt-as-markdown-toggle]
status:: done
touches:: [[code:hiker/panels/buffer]]
note:: flips both the language and live-preview behavior for the active buffer; persists via [[spec:settings-write-back]] to `editor.render_txt_as_markdown` · evidence: `app/src/panels/buffer/mod.rs` ("Render .txt as markdown" menu entry)
- **`egui_editor` feature toggles** — these rows are session/vault-scope flips of the corresponding `egui_editor` features (see `editor/SPEC.md`): Word wrap (§3.8), Show whitespace (special-character rendering, §9.16), Highlight trailing whitespace (§9.17 — quiet enough to leave on for code, noisy on prose, so opt-in; default off, persisted per-vault), Show line numbers (gutter, §3.7). The menu rows are hiker chrome; the rendering is the widget's. [view-word-wrap-toggle, view-show-whitespace-toggle, view-highlight-trailing-whitespace-toggle, view-line-numbers-toggle]
- **Show heading breadcrumb** — overlays each chunk with its `heading_path` (already stored on chunks). Pairs with chunk boundaries; defer until both have a real user. [view-heading-breadcrumb-toggle]
status:: done
touches:: [[code:hiker/panels/buffer]]
note:: the toggle's menu-row stub (disabled with tooltip "Pairs with view-show-chunk-boundaries") is what this slug owns; the overlay it'll eventually flip is [[spec:view-heading-breadcrumb-overlay]] · evidence: `app/src/panels/buffer/mod.rs` (View-menu items)

### Out of scope (this menu)

- Content-mutating actions — those live in [[spec:note-mutations-menu]].
- Per-file scoped toggles. The menu's scope is "active buffer at most"; per-file persistence is a frontmatter concern that doesn't exist in v1.
- Theme / font / color-scheme — those belong in settings, not a quick toggle.


## Note-mutations menu

A top-bar button on the editor pane hosting content-mutation actions on the active note. Sibling to View options ([[spec:editor-view-options-menu]]); the split is clean — View changes pixels, Mutations changes bytes. Icon-only button using the wand glyph ([[spec:mutations-menu-icon]]). Click opens a popover listing the mutations applicable to the active buffer. [note-mutations-menu]
status:: done
touches:: [[code:hiker/panels/buffer]], [[code:hiker/toolbar]]
note:: wand-icon top-bar button; popover lists mutations applicable to the active buffer; v1 entry is "Reformat as markdown". Result lands as an in-buffer edit ([[spec:note-mutation-applies-as-buffer-edit]]); RO during in-flight per [[spec:note-mutation-buffer-ro-while-in-flight]]. The earlier derived-file flow (`replace_note_with_derived` / `delete_derived` / `list_pending_derived`, `note_mutation_replace_original` / `note_mutation_discard_derived` commands, the `BufferMode "mutation"` variant, the `.hiker/derived/` directory, mutation-completed events / `-cleared` events) was removed in this refactor · evidence: `app/src/toolbar.rs` (mutations menu trigger); `app/src/panels/buffer/mod.rs` (apply path); `submit_note_mutation` command

Mutations are LLM-driven content rewrites of the active note. Single-note user-initiated mutations apply **as buffer edits** — there is no separate review surface, no derived file, no explicit Apply/Reject verbs. Save accepts, Ctrl-Z reverts, the existing dirty-buffer + changes-log machinery handles everything else. The shape is uniform across all current and future mutations:

1. The user clicks a mutation entry. Hiker submits a `Direct`-shape task to `core::tasks` (per `task-queue.md`) at `High` priority — the user is watching. The task carries the buffer's *live* text (not last-saved, same rule as [[spec:chat-active-note-context-injection]]) so the mutation operates on what the user sees. The buffer is set read-only for the duration of the task, and the source tab is pinned (a preview tab promotes to sticky on submit per [[spec:editor-preview-tab-promotion]] so a preview-slot swap can't displace the buffer the result needs to land on). [note-mutation-buffer-ro-while-in-flight]
status:: done
touches:: [[code:hiker/panels/buffer]], [[code:hiker/toolbar]]
note:: populated from queue events `task_queued` events filtered to `kind: NoteMutation { source_path }`; cleared on terminal events (`task_completed` / `task_failed` / `task_cancelled`). Active buffer is RO whenever its path is in the set. The mode-controls "Reformatting…" pill surfaces the reason. Submit also pins the source tab so a preview-slot swap can't displace the buffer the result needs to land on · evidence: `app/src/panels/buffer/mod.rs` (in-flight mutation tracking, read-only mirror, source-tab pin), `app/src/toolbar.rs` (queue-event tracking)
2. The queue's direct-LLM worker drains the task by calling `core::llm::chat` with the mutation's prompt. External MCP-attached clients can also drain the task per the queue's worker rules. The home-page Task queue widget ([[spec:task-queue-home-widget]]) is the in-flight progress surface — no per-mutation toast.
3. On `TaskCompleted`: the result replaces the source buffer's content as a single editor transaction, the buffer's read-only flag clears, and the buffer becomes dirty. Works whether the source tab is the active one (dispatch through the live editor view) or a background tab (rewrite the tab's saved editor state in place via a transaction off the existing state, preserving history so Ctrl-Z reverts the whole replacement as one undo step on activation). The user reviews by reading the buffer; the dirty-buffer Diff toggle ([[spec:editor-diff-vs-disk-toggle]]) flips the editor view to a line-level diff against on-disk content for explicit comparison. **Save** writes the mutated content through the regular save path (which writes a plain-file snapshot). **Ctrl-Z** reverts the mutation as a single undo step. If the user closed the source tab mid-flight (only possible from the explicit close path, since the tab is RO + pinned during the flight), the result is dropped silently — no toast, no held state. [note-mutation-applies-as-buffer-edit]
status:: done
touches:: [[code:hiker/panels/buffer]]
note:: one edit so Ctrl-Z reverts the whole replacement as one undo step. Active tab applies through the live editor; background tab rewrites its saved state off the existing state (preserves history). `loadedText` stays at the pre-mutation content → buffer reads dirty → save accepts. Drift-checked at apply time against the buffer's loaded hash vs `source_hash_at_submit` (mismatch drops silently — RO+pin during flight makes drift rare). Source buffer closed mid-flight → result dropped silently (no toast, no held state) · evidence: `submit_note_mutation_inner` (mutation events emit on `TaskCompleted`), `app/src/panels/buffer/mod.rs` (mutation-apply path + mutation events handling)
4. On `TaskFailed`, the buffer's read-only flag clears and a toast surfaces the error. No content change. On `TaskCancelled` (user cancels via the queue widget), the buffer's read-only flag clears, no content change, no toast.

[note-mutations-menu-task-shape]
status:: done
note:: uniform shape per spec: submit `Direct`/`High` task with `kind: NoteMutation { mutation, source_path }`, await `TaskCompleted` / `Failed` / `Cancelled`, on Completed emit mutation events so the editor applies one edit (single undo step) · evidence: same evidence as [[spec:note-mutations-menu]]

**Mutation provenance.** When an LLM mutation lands on the buffer, the resulting frontmatter stamps `hiker.author: agent-authored` (and when git is integrated, the save's commit carries the agent `Hiker-Author` trailer, `git.md`) so mutation-derived edits are identifiable. There is no separate changes-log row to tag — the snapshot history carries no metadata beyond the timestamp. [note-mutation-stash-changes-tag]
status:: planned
note:: the mutation-tag stash rode the changelog row's `metadata` column; with the op log as the sole substrate, user saves apply through `op_writes::user_save` (whole-document, no per-write metadata). Re-landing the tag means threading op metadata through the user-save seam — deferred until a consumer needs the `{ mutation: "<kind>" }` provenance again

### v1 mutation: Reformat as markdown

The first concrete mutation: reformat the active note's content as clean markdown. Useful for `.txt` files (per `txt-ingest.md`'s LLM-rewrite option) and for `.md` files whose markup has rotted (uneven heading levels, broken list nesting, inconsistent emphasis). [note-mutation-reformat-as-markdown]
status:: done
implements:: [[code:hiker/prompts/bundled_defaults]]
note:: task submission + prompt are unchanged; the awaiter now emits mutation events carrying the result content + the source-hash captured at submit time · evidence: `core/prompts/note_mutation_reformat_as_markdown.md` (bundled prompt); `core/src/prompts.rs::bundled_defaults`; `submit_note_mutation_inner` builds the task

Submits a task with `kind: NoteMutation { mutation: ReformatAsMarkdown, source_path }` and `payload` carrying the buffer's live text + the source extension. The prompt template lives at the user/vault prompt-store path `note_mutation_reformat_as_markdown.md` (per [[spec:llm-prompts-file-store]]); the bundled default is registered in `core::prompts::bundled_defaults()`.

### Mutations-menu button states

- **Enabled** when the active buffer is an editable note (`mode.kind` is `File`) of an indexable extension (`.md` / `.markdown` / `.txt`) and has at least one byte of content.
- **Disabled** during read-only preview modes (trash / snapshot / staging review) — mutating from inside a review surface would be confusing. Tooltip explains why.
- **Disabled with "Mutation in progress…" tooltip** when there is an active or leased task whose `kind: NoteMutation { source_path }` matches the active buffer's path. The buffer is RO during this window for the same reason. Only one in-flight mutation per source path ([[spec:note-mutation-one-in-flight-per-path]]).

- **Pending-background-mutation indicator.** When the active buffer has any pending background mutation job (a `NoteMutation`-kind task in non-terminal state whose `source_path` matches), the Mutations menu trigger renders a small pulsing accent-color dot on its icon (same `@keyframes` pulse as [[spec:tree-row-queued-marker]]). Distinct from the `#mode-controls` "Reformatting…" pill, which names the single in-flight in-buffer mutation; the dot signals presence-of-any-pending and stays lit across multiple queued or batch-flight jobs ([[spec:note-mutation-batch-via-staging]]). [note-mutations-menu-pending-indicator]
status:: planned
note:: pulsing accent-color dot on the Mutations menu trigger when the active note has at least one pending background mutation job (i.e. any `NoteMutation`-kind task in the queue whose `source_path` matches the active buffer, in non-terminal state). Shape mirrors [[spec:tree-row-queued-marker]] (same `@keyframes` pulse). Distinct from [[spec:note-mutation-buffer-ro-while-in-flight]]'s "Reformatting…" pill — the pill names the single in-flight mutation on the active buffer; the dot is the per-note presence-of-any-pending indicator and stays lit across multiple queued or batch-flight jobs (pairs with [[spec:note-mutation-batch-via-staging]]). Driven by the same queue events subscription the menu already maintains for the disabled-state tooltip

When only one mutation entry is enabled (the v1 case), the popover still opens. As more mutations land, they slot in alphabetically.

### Batch mutations

**Batch mutations** (e.g., "reformat every `.txt` in `inbox/`") fan out N tasks the user can't watch one-by-one, so results route through the **staging surface** (per `settings.md`'s staging review section): the `PatchReview` tab with per-row [Accept] [Reject] and [Accept all], plus the editor toolbar pill when the affected file is open.

Batch entry points are deferred to v2; they slot into:

- **A folder-context bulk action** invoked from the file tree ([[spec:note-mutation-batch-from-folder]], deferred).
- **A search-result bulk action** alongside the already-reserved [[spec:search-bulk-action-tag]] / [[spec:search-bulk-action-move]] ([[spec:note-mutation-batch-from-search]], deferred).
- **A CLI command** (`hiker mutate <kind> <glob>`, deferred).

All three converge on the same staging-driven flow; no batch-specific review surface. [note-mutation-batch-via-staging]
status:: planned
note:: umbrella: batch mutation entry points (folder / search / CLI) fan out N tasks per `task-queue.md`; results route to staging and appear on the `PatchReview` tab + the editor toolbar pill for open files. Single-note stays in-buffer


## Vault home page

When no note is open, the editor pane shows a vault home page in place of the editor — a lightweight overview of the vault rather than empty space. Default landing surface on vault open (assuming no auto-resume of last-open buffer); reappears when the user closes the active buffer without opening another. [vault-home-screen]
status:: done
touches:: [[code:hiker/panels/home]]
note:: home page opens as a `home`-kind tab, so editor toolbar + status bar hide on activation per [[spec:tab-kinds]]. Migrated from CSS-class-based sub-mode to app-page tab (S2). · evidence: `app/src/panels/home.rs` (`show()`)

Three widgets, in this vertical order:

- **Vault stats.** Total notes, total chunks, breakdown by index state (indexed / queued / skipped / unsupported), maybe disk usage of the vault directory. Pulled cheaply from the existing index store via a single command. Live-updates via the existing indexer-progress events so the counts reflect ongoing work. [vault-home-stats-widget]
status:: done
implements:: [[code:hiker/store/dto/VaultStats]], [[code:hiker/store/notes/impl#[Store]vault_stats]]
touches:: [[code:hiker/panels/home]]
note:: five tiles: Notes / Indexed / Chunks / Queued / Skipped. Queued count rides the existing `IndexerHandle::status().queued`. Live-updates via debounced refresh on every terminal indexer-progress events. Unsupported / disk-usage breakdowns deliberately deferred — both need a vault walk · evidence: `core/src/store.rs` (`Store::vault_stats`, `VaultStats`), `app/src/panels/home.rs` (stats refresh)
- **Recently modified.** Top N (default 10) notes by filesystem mtime (reuses the `DirEntryDto` mtime field, [[spec:tree-sort-options]]). Each row shows basename + relative path + relative time. Click → open in editor. [vault-home-recent-modified]
status:: done
implements:: [[code:hiker/store/dto/RecentNote]], [[code:hiker/store/notes/impl#[Store]recent_notes_by_mtime]]
touches:: [[code:hiker/panels/home]]
note:: `ORDER BY mtime DESC LIMIT 10` over non-skipped notes. Refresh debounced (400ms) on watcher file events for any kind that can shift mtime ranking (created/deleted/renamed/modified). Click on a row opens the file · evidence: `core/src/store.rs` (`Store::recent_notes_by_mtime`, `RecentNote`), `app/src/panels/home.rs` (recent-modified refresh)
- **Recently accessed.** Top N notes by user-open time; same row shape and click behavior as recently-modified. [vault-home-recent-accessed]
status:: done
implements:: [[code:hiker/store/dto/RecentNote]], [[code:hiker/store/notes/impl#[Store]recent_notes_by_access]]
touches:: [[code:hiker/panels/home]]
note:: `ORDER BY last_accessed_at DESC` excluding NULL. Refreshes on full home re-render; the watcher doesn't drive these since hiker itself is the only writer of `last_accessed_at` and the writes happen via `note_accessed` on file open · evidence: `core/src/store.rs` (`Store::recent_notes_by_access`), `app/src/panels/home.rs` (recent-accessed refresh)

Note access tracking is independent infrastructure (later consumers: search ranking, an "activity" view, etc.) and rides its own slug:

- **Note access tracking.** Add `last_accessed_at INTEGER` to the `notes` row; bump the schema-version constant (same fail-loud + reindex contract as [[spec:store-version-fail-loud]]). Written when a file becomes the active buffer (tree / search / recents open). Read by the recents widget and future consumers. [note-access-tracking]
status:: done
implements:: [[code:hiker/indexer/jobs/handle_simple_job]], [[code:hiker/indexer/IndexJob#TouchAccess#ts]], [[code:hiker/indexer/impl#[Handle]touch_access]], [[code:hiker/store/notes/impl#[Store]touch_note_access]]
touches:: [[code:hiker/panels/buffer]]
note:: schema bumps to v4 (fail-loud + reindex per [[spec:store-version-fail-loud]]). Touch is fire-and-forget over the indexer mpsc so writes go through the indexer's owned writer. No-op when the note isn't yet indexed — the next ingest creates the row, and subsequent opens record · evidence: `core/src/store.rs` (`SCHEMA_VERSION = 4`, `notes.last_accessed_at`, `Store::touch_note_access`, `NoteRow.last_accessed_at`), `core/src/indexer.rs` (`IndexJob::TouchAccess`, `IndexerHandle::touch_access`, handler in `handle_simple_job`), `app/src/panels/buffer/mod.rs` (fire-and-forget access touch on file open)

Refresh shape: the home page subscribes to indexer-progress events for live stat updates and to watcher file events for recent-modified updates. The recently-accessed list updates on each open without watcher involvement (the writer is hiker itself).

UI scope: minimal. Header with vault root path, three widgets stacked, no charts / graphs, no per-source-type breakdowns yet. A "New note here" button at the top — same call as the sidebar's [[spec:sidebar-new-item-button]].

Out of scope for v1 of the home page: pinned/landmark notes, active-trail display, search shortcuts, discovery hints from clustering, recent-searches list, vocabulary stats, sync status. All slot in as additional widgets as their backing features land.

### Recent activity widget (removed)

The cross-note recent-activity feed widget was **removed** with `core::activity` and the `.ops` history engine (the rework retired the author-class activity feed and the `Changes` panel). There is no vault-wide activity stream. Per-note history is reachable through the status-bar version dropdown ([[spec:status-bar-version-dropdown]]) and the `Show changes` menu, both sourced from the plain-file snapshot tree (`op-log.md` "Local history"); pending agent edits are reviewed on the `PatchReview` tab (`settings.md`). [vault-home-recent-activity-widget]
status:: retired
note:: removed with `core::activity` / the `Changes` feed (the rework). Per-note history is the snapshot version dropdown; pending edits are the `PatchReview` tab.


### Detail views

Vault home widget tiles support a drill-in pattern. **Click on a widget's tile or header → home view body swaps to a detail view for that widget.** No back button affordance within the home view itself — clicking the Home button in the top strip always returns to the home overview, regardless of whether you're in the overview or a detail view. Clicking a note row in any detail view exits home and opens the editor on that note (same shape as `openFile` already exits home view today). [vault-home-detail-views]
status:: done
touches:: [[code:hiker/panels/home]]
note:: detail views become `home-detail`-kind tabs per [[spec:tab-kinds]]; activity-widget clicks open `home-detail` tabs. Migrated from swap-out sub-mode to app-page tab (S2). · evidence: `app/src/panels/home.rs` (`show_detail()`)

Detail views replace the home overview body, not the editor. `#editor-pane` has four states — editor, home overview, home detail, and the settings surface ([[spec:settings-pane-mode]]). The gear ([[spec:vault-bar-settings-icon]]) toggles editor ↔ settings; widget-tile clicks go home overview → home detail.

Read-only review surfaces (trash, snapshot, staging review previews) are sub-modes of the editor state, sharing the editor view; the `#mode-controls` slot lights up with mode-specific buttons + label (see `## Mode controls slot`).

Per-widget detail views, in roughly the order they earn their keep:

- **[[spec:vault-home-stats-detail]]** — each Stats tile (Notes / Indexed / Chunks / Queued / Skipped) drills in to a list view:
    - **Notes** — full list of all notes, paginated, sortable by mtime / access / path.
    - **Indexed** — same shape, filtered to indexed-only.
    - **Chunks** — per-note chunk count, sortable; flags pathologies (>100 chunks, 0 chunks). A surface for spotting chunker pathology, ahead of the deferred [[spec:eval-sanity-stats]] work.
    - **Queued** — live list of notes currently in the indexer's pending set (`is_pending` per [[spec:cmd-file-index-state]]). Updates on every indexer-progress event.
    - **Skipped** — list of skipped notes with their reasons (already tracked via `notes.skipped` + `notes.skip_reason`). Per-row "retry" affordance reroutes through `IndexJob::Upsert` with `force=true` so users can manually retry after fixing the underlying issue (file size, encoding).
- **[[spec:vault-home-recent-activity-detail]]** — *per-path snapshot history* (the vault-wide activity feed was removed with `core::activity`). For a given note, the list is its plain-file snapshots (`op_writes::snapshot_history`), newest first. Mental model: **each row is a saved version of the file.** Row layout: snapshot time-ago, plus a `current` badge on the most recent row. [vault-home-recent-activity-detail]
status:: done
touches:: [[code:hiker/panels/home]]
note:: per-path snapshot list (each row = saved version). Row click → opens the snapshot read-only in the editor ([[spec:snapshot-preview-mode]]). Per-row `[Restore this version]` writes that snapshot's `content_at_snapshot(id)` back via `user_save` (forward-correct). `current` badge marks the most recent row. The cross-note author-class feed + `core::activity` projection are gone

    The interaction shape:

    - **Click a row** → opens that snapshot read-only in the editor. Reuses the same `readOnlyCompartment` + banner pattern as [[spec:tree-trash-preview]]; the banner reads `Snapshot of <path> · <when> · <author> · <op>` with `[Restore this version]` and `[Close preview]` actions. Closing returns to the activity detail view.
    - **Per-row `[Restore this version]`** → for power-user single-click without previewing first. Hidden on the `current` row (restoring the current state is a tautology) and on `'deleted'` rows (no content blob to write).
    - **No separate "Open" button.** Click-the-row → snapshot preview is the only path; the live file is reached via the tree, search, or recently-modified.
    - **No separate "Rollback to before this" button.** The row *is* the version (the content blob lives on it); `Restore this version` is the verb — what you click is what you get.

    Restore reads the version's content (`op_writes::content_at_op`) and writes it back via `op_writes::user_save` — a fresh `user` op that becomes the newest accepted version (command `restore_snapshot`). The change-shaped flavor (`rollback_change`) stays available for the agent-rollback consumer per `mcp.md`; both coexist on the same op-log primitives (`op-log.md` "History materialization" → "Rollback").

    - **Author-class filter pills (removed).** The user/agent/show-staging author-class filter pills were removed with the `core::activity` feed and the `Changes` panel. Per-path snapshot history has no author dimension (attribution survives only in git's `Hiker-Author` trailers when git is integrated, `git.md`), so the detail view is a plain snapshot list. Pending agent edits are reviewed on the `PatchReview` tab (`settings.md`), not via a feed filter. [vault-home-recent-activity-filter-pills]
status:: retired
note:: removed with `core::activity` / the `Changes` panel; snapshots carry no author class, pending review moved to the `PatchReview` tab
    - **Un-rollback affordance** — every retained snapshot stays restorable, including a state that was itself a Restore. "Un-rollback" is just Restore on a more recent snapshot — same primitive. [vault-home-recent-activity-unrollback]
status:: done
touches:: [[code:hiker/panels/home]]
note:: every snapshot in retention is an addressable Restore target; the action is the same `[Restore this version]` button (forward-correct save). The append-only restore chain composes naturally
    - **Snapshot read-only preview.** Reuses the trash-preview machinery: `setReadOnly(true, "snapshot")` swaps in the snapshot banner, suppresses the save button + dirty marker, and the dirty-switch guard treats it like a trash preview (nothing to discard). The buffer carries `snapshotPreview: true` and `snapshotChangeId` so the banner's Restore can write back without a re-lookup. Banner is amber (not trash's red) — informational, not a recovery surface. [snapshot-preview-mode]
status:: done
note:: version review opens as `TabKind::Editor { buffer: HistoryVersion{...}, diff: Some(Disk{path}) }`. `ensure_readonly_buffer_loaded` reads `changes.content_at(id)` into a read-only `Buffer`; `render_readonly_source_toolbar` renders Restore + Show-diff buttons in the toolbar. Diff layer (owner `HistoryVersion`) emits per-hunk Restore widgets via `attach_history_version_hunk_widgets` ([[spec:diff-layer-hunk-widgets]]); whole-version Restore lives on the toolbar
- **[[spec:vault-home-recents-detail]]** (lower priority) — full-list versions of Recently Modified / Recently Accessed; adds filtering / longer history. Each preview row already has click-to-open, so this isn't load-bearing.

The Stats subviews (Notes / Indexed / Chunks / Queued / Skipped) share the one [[spec:vault-home-stats-detail]] slug, parameterized by which tile launched them — new tiles add parameter values, not slugs. [vault-home-stats-detail]
status:: planned
note:: per-tile detail views (Notes / Indexed / Chunks / Queued / Skipped) parameterized by source tile; Skipped row offers per-row retry via `IndexJob::Upsert force=true`

UI shape notes:

- Detail view header: tile name (e.g. "Skipped notes") + count.
- Body: paginated list, virtualized if needed (skipped/indexed lists could be thousands of rows on a large vault).
- Empty state: a brief "no items" message, since every detail view has a sensible empty case.
- Sort/filter affordances live in the detail view header, not the home overview tile.

## Top strip

A single horizontal strip across the very top of the window. Holds the vault-level icon-button cluster on the left, the vault path label, and the multi-buffer tab strip filling the rest of the row. Replaces the standalone vault bar — the four icon buttons that previously lived at the top of the sidebar (Home / Queue / Settings / Open vault) move out to this strip. [top-strip-layout]

### Top strip leading cluster

Icon-only buttons, left-to-right, in this order:

- **Back button.** Disabled when no back history. Standard arrow glyph. [top-strip-back-button]
status:: done
touches:: [[code:hiker/toolbar]]
note:: icon-only arrow in the leading cluster; disabled state mirrors stack emptiness · evidence: `app/src/toolbar.rs` (Back button), `app/src/state.rs` (back stack)
- **Forward button.** Same. [top-strip-forward-button]
status:: done
touches:: [[code:hiker/toolbar]]
note:: mirror of [[spec:top-strip-back-button]] · evidence: `app/src/toolbar.rs` (Forward button), `app/src/state.rs` (forward stack)
- **Home button.** House glyph. Toggles the editor pane to the vault home page — a view toggle, not a buffer close (the active buffer stays in memory; clicking any tree row, recents entry, search result, or tab restores the editor onto it). Tooltip "Vault home." Reserves keybind id `vault.go-home`. [vault-home-button]
status:: done
touches:: [[code:hiker/toolbar]]
note:: icon-only house glyph in the top strip's leading cluster (after Back/Forward, before Queue). Toggles the home view — view toggle, not buffer close, so the active tab stays in memory. Keybind `vault.go-home` still reserved but not yet registered (chord TBD) · evidence: `app/src/toolbar.rs` (Home button in the leading cluster)
- **Queue button.** List-with-pulse glyph. Opens the shared queue detail page (`task-queue.md`'s [[spec:queue-detail-shared-page]]). A small superimposed indicator shows the `Queued + Leased` count (hidden when zero); the icon pulses when anything is `Leased`. Tooltip "Background work" (or "Background work (N active)"). [vault-bar-queue-button]
status:: done
touches:: [[code:hiker/toolbar]]
note:: list-with-pulse glyph icon button + count indicator (hidden at zero) in the top strip's leading cluster between Home and Settings. Click opens the queue detail page; behavior unchanged from prior position. Slug name retains the `vault-bar-` prefix per the spec — the slug names the feature, not its location · evidence: `app/src/toolbar.rs` (Queue button + count indicator in the leading cluster)
- **Settings button.** Gear glyph. Toggles the editor pane to the settings surface ([[spec:settings-pane-mode]]); same view-toggle behavior as Home. Pressed/unpressed state reflects whether the settings pane is visible. Tooltip "Settings." Keybind `settings.open` — Cmd-, / Ctrl-,. [vault-bar-settings-icon]
- **Open-vault button.** Folder glyph. Triggers the JS dialog → `open_vault_at` flow per `settings.md`'s default-vault-autoopen story. Tooltip "Open vault…". [vault-bar-open-vault-icon]
status:: done
touches:: [[code:hiker/toolbar]]
note:: folder glyph in the top strip's leading cluster (after Settings). Click handler unchanged (`open_vault_at`); the slug retains `vault-bar-` per the spec convention · evidence: `app/src/toolbar.rs` (Open-vault button in the leading cluster)

The vault-path label sits to the right of the icon cluster, before the tab strip — same shape it has today, just relocated. Truncates with ellipsis when space is tight (per [[spec:ui-no-sibling-pushout]]).

### Tab strip

The tab strip itself — per-group tabs, active/inactive shading, dirty-dot↔close-× swap, overflow scrolling + dropdown, middle-click close, drag-between-groups, the preview/pinned visual states — is `egui_workbench` (`egui-workbench/SPEC.md` §5). Hiker fills it with the tab kinds below and wires these hiker-specific behaviors: [editor-tab-strip]
status:: done
touches:: [[code:hiker/workbench_host]]
note:: one tab per file-mode buffer; click switches (saves outgoing editor state and restores target's saved state so undo / selection / scroll persist); × closes; middle-click closes. History-version/trash previews don't get tabs — they're transient overlays on the active tab · evidence: `app/src/workbench_host.rs` (`HikerWbTab`, tab activation + close + render)

- **Tab content / disambiguation.** Tab label is the open buffer's basename. When two open buffers share a basename, both render with a folder hint (`notes.md (research/)` vs `notes.md (inbox/)`); tooltip shows the full vault-relative path. [editor-tab-disambiguation]
status:: done
touches:: [[code:hiker/workbench_host]]
note:: recomputes on every render: when two open buffers share a basename, both tabs render with a `(folder/)` hint; tooltip on hover always shows the full vault-relative path · evidence: `app/src/workbench_host.rs` (basename-count + folder-hint)
- **Tab keybinds**, all reserved in [[spec:keybind-registry]]: `tab.close` = Cmd/Ctrl-W, `tab.next` = Cmd/Ctrl-Tab, `tab.previous` = Cmd/Ctrl-Shift-Tab, `tab.jump-N` = Cmd/Ctrl-1..9 (9 jumps to the last tab). Hiker binds these into the workbench's tab actions. [editor-tab-keybinds]
status:: done
touches:: [[code:hiker/keybinds]]
note:: editor-focus case + cross-pane handler cover the rest (per `editor.md`'s "When a future binding needs to fire outside the editor" note). Middle-click also closes via the tab strip's aux-click handling · evidence: `app/src/keybinds.rs` (`tab.close` / `tab.next` / `tab.previous` / `tab.jump-1..9` plus a cross-pane handler)
- **Right-click context menu.** Hiker adds a **Reveal in tree** verb (selects the tab's note in the file tree, expanding parent folders) alongside the workbench's Close / Close others / Close to the right. [editor-tab-context-menu]
status:: done
touches:: [[code:hiker/workbench_host]]
note:: bulk-close paths abort if any individual close is cancelled (so a dirty-tab Cancel keeps the rest of the user's work intact). `Reveal in tree` delegates to the existing reveal-path path · evidence: `app/src/workbench_host.rs` (tab context menu: `Close` / `Close others` / `Close all to the right` / `Reveal in tree`)
- **No `+` button.** New notes use the file tree's `+ New note` affordance.

The active/inactive shading and dirty-marker rendering slugs map onto the workbench tab states. [editor-tab-active-state, editor-tab-dirty-marker, editor-tab-overflow]

### Tab strip behavior with the rest of the app

- **File-tree click on an already-open file** switches to its tab rather than reloading. Click on a not-yet-open file opens a new tab and switches to it. [multi-buffer-tree-click-switches-tab]
status:: done
touches:: [[code:hiker/workbench_host]]
note:: already-open files (tree click, search-result click, recents click, wikilink — once that lands) reuse the existing tab; not-yet-open files mint a new tab and activate it · evidence: `app/src/workbench_host.rs` (open-or-switch based on whether the buffer is already open)
- **Search-result, recents, wikilink, and any other "open this note" entry point** behave the same: existing tab → switch; not yet open → new tab.
- **Mode-controls slot, View menu, Mutations menu, the MCP `get_active_note` surface, navigation history** all operate on the active tab.
- **In-flight-mutation RO** ([[spec:note-mutation-buffer-ro-while-in-flight]]) applies to the source tab whether or not it's active. Its dirty marker reads as a normal dirty dot; the queue widget / inline indicator is the source-of-truth for background work.

### Linked tabs (drive / follow)

By default every viz tab is self-contained: a graph or canvas tab opens clicked notes into its own preview slot and highlights whichever note is globally active. **Linking** wires a viz tab to another editor group so the two coordinate explicitly, generalizing v1's "Related stays bound to the active editor" ([[spec:search-related-stays-bound]]) into per-tab source/target wiring. Two independent directions:

- **DRIVE (target).** When a viz tab targets a group, clicking a node opens the note into *that* group instead of the viz tab's own preview slot. A thin sibling of the one `open_file` chokepoint (`open_file_in_group`) points the workbench's focused group at the target before delegating; an already-open note is focused in place. [tab-link-drive]
status:: done
touches:: [[code:hiker/editor_pane]], [[code:hiker/panels/canvas/render]], [[code:hiker/panels/graph]]
note:: a viz tab with a `target` opens clicked notes into the linked group via the one `open_file` chokepoint (focus the target group, then delegate); already-open notes are focused in place · evidence: `app/src/editor_pane.rs` (`open_file_in_group`, `drive_target_group`), `app/src/panels/graph.rs` (node-click routing), `app/src/panels/canvas/render.rs` (`activate_node` routing)
- **FOLLOW (source).** When a viz tab follows a group, each frame it reads that group's active tab, resolves its note path, and highlights / brings into view the matching node — the graph accents the node, the canvas single-selects and centers the file-node referencing it (deduped so the camera moves only when the followed note changes). Polled per frame off the stable group handle; no event bus. [tab-link-follow]
status:: done
touches:: [[code:hiker/editor_pane]], [[code:hiker/panels/canvas/render]], [[code:hiker/panels/graph]]
note:: a viz tab with a `source` reads that group's active note each frame and highlights the matching node; canvas single-selects + centers the file-node (deduped on `Pane.followed` so the camera only moves on change) · evidence: `app/src/editor_pane.rs` (`followed_note_path`), `app/src/panels/graph.rs` (active-path override → accent highlight), `app/src/panels/canvas/render.rs` (`apply_follow`), `hiker-canvas/view/src/widget.rs` (`focus_node`), `hiker-canvas/view-core/src/camera.rs` (`center_on_point`)
- **Link control.** Each viz tab's header carries a small **Link** control opening a picker over the current editor groups — a "Follow" list and a "Drive" list, each with a clear option, labelled by the group's active-tab title. The tab's own group is excluded (self-link is a no-op loop). [tab-link-control]
status:: done
touches:: [[code:hiker/editor_pane]], [[code:hiker/panels/canvas/render]], [[code:hiker/panels/graph]]
note:: header button (a tab-with-link-in-the-corner icon on the canvas tab, pressed-state when linked) opens a Follow/Drive group picker (each with Clear); the tab's own group is excluded · evidence: `app/src/editor_pane.rs` (`link_menu_ui`, `group_label`), `app/src/panels/graph.rs` (`link_control`), `app/src/panels/canvas/render.rs` (`link_control`), `app/assets/icons/tab_link.svg`
- **Reference + persistence.** A link references a group by the per-window group handle (`GroupId`), which is **not** restart-stable, so v1 links are **in-session only** — they don't ride the autosave tab-state snapshot. A re-resolvable form (persist the linked group's active-tab `persist_key`, re-resolve after layout restore) is the planned follow-up. [tab-link-persist]
status:: partial
note:: links are **in-session only** for v1 — `GroupId`/`TileId` is not restart-stable and groups have no persisted identity. **Partial**: re-resolvable persistence (persist the linked group's active-tab `persist_key`, re-resolve after layout restore) is the planned follow-up · evidence: `app/src/tab.rs` (`persist_key` note + `TODO(tab-linking-persist)`)

The wiring lives entirely in the app layer (a per-tab source/target link referencing tabs or groups, the `open_file_in_group` / `active_tab_in_group` seams, the per-frame follow read). The workbench gains only narrow group accessors; no core involvement, since linking neither mutates the vault nor touches the indexer. Extends to the cluster vector visualization ([[spec:cluster-vector-viz]]) once that lands. [tab-link-model]
status:: done
touches:: [[code:hiker/workbench_host]], [[code:hiker/workspace]]
note:: per-tab source/target wiring, app-layer only; the workbench gains only narrow group accessors. Generalizes [[spec:search-related-stays-bound]]. Implemented for graph + canvas ([[spec:tab-link-drive]] / [[spec:tab-link-follow]]) · evidence: `app/src/tab.rs` (`Tab.link: TabLink { source, target }`, `LinkRef = Tab(TabId) | Group(GroupId)`), `egui-workbench/src/workspace.rs` (`active_tab_in_group` / `groups` / `group_of`), `app/src/workbench_host.rs` (`active_tab_in_group` / `group_of_tab` resolving workbench handles ↔ hiker `TabId`)

### Multi-buffer model

The editor-group + tab container is `egui_workbench` (§4–§5). Hiker's policy on top of it:

- **In-memory while the vault is open; tab state restores on next open.** The set of open buffers is in-memory state during a session — closes, switches, and dirty content all live in RAM. The autosave layer (`autosave.md`) round-trips a tab-state snapshot (open paths + active path + preview-slot path) to `.hiker/autosave/index.json`, so the next vault open silently reopens the same set of tabs. Per-buffer dirty content recovery rides the same store, prompting via the recovery modal. [multi-buffer-in-memory-only]
status:: done
touches:: [[code:hiker/workbench_host]]
note:: in-memory during a session; the autosave layer round-trips a `(open_paths, active_path, preview_path)` snapshot to `.hiker/autosave/index.json` ([[spec:autosave-tab-state-store]]) and the next vault open silently reopens those tabs ([[spec:autosave-tab-state-silent-restore]]) · evidence: `app/src/workbench_host.rs` (open-buffers set, vault-swap clear, autosave recovery reopens saved tabs from the autosave tab-state snapshot)
- **No max open count / no retention timer.** Tabs stay until the user closes them; a user with 50 tabs gets the workbench's overflow handling.
- **[[spec:file-switch-guard-dirty]] is close-time only.** Navigating *to* a dirty tab is fine — the dirty buffer stays dirty in memory. The save/discard/cancel modal only fires when the user closes the tab (× / middle-click / Cmd-W). The existing nav-time fire is dropped. [multi-buffer-no-switch-guard]
status:: done
touches:: [[code:hiker/workbench_host]]
note:: nav-time [[spec:file-switch-guard-dirty]] fire dropped; switching tabs / clicking another file leaves the prior tab dirty in memory. The guard now fires only on explicit close and on window close · evidence: `app/src/workbench_host.rs` (file-open no longer prompts; same-path short-circuit replaced with an open-or-switch path)
- **Window close has no dirty-buffer modal.** Quitting flushes every dirty buffer through the autosave pipeline and pushes the open-tab snapshot, then destroys the window — no prompt. Next launch auto-restores the workspace as dirty tabs ([[spec:autosave-recovery-auto-restore]] + [[spec:autosave-tab-state-silent-restore]]); the user saves or reverts via the existing affordances. This parks the work — the user's actual files are unchanged on exit. [autosave-close-no-modal]
status:: done
touches:: [[code:hiker/autosave]], [[code:hiker/workbench_host]]
note:: no dirty-buffer modal on app exit. Saved bytes are *not* written through to the user's files — the autosave sidecars persist and next launch's [[spec:autosave-tab-state-silent-restore]] + [[spec:autosave-recovery-auto-restore]] reopen the workspace with dirty tabs the user can save or revert via the existing affordances · evidence: `app/src/workbench_host.rs` (window-close flushes every dirty buffer then pushes the tab-state snapshot and destroys without prompting), `core/src/autosave.rs` (flush + snapshot-push)
- **Navigation history stays unified** across all tabs (one stack per vault). Back/forward navigates between content surfaces regardless of which tab they were in; the corresponding tab activates as part of the back/forward action.


### Tab kinds

A tab is a `(kind, payload)` pair. The kind names *what* the tab renders; the payload identifies *which one*.

**Umbrella term: "app pages."** Every non-`buffer` kind below (`home`, `home-detail`, `queue`, `settings`, `properties`, `patch-review`, `graph`) is collectively an *app page* — a tab that renders an in-app surface rather than user-authored content. The `TabKind` discriminator on the wire stays per-kind (`home`, `queue`, …) — "app page" is umbrella vocabulary, not a runtime category.

- `buffer` — payload is a vault-relative file path plus an optional `DiffSource` (per `diff.md` [[spec:diff-source-enum]]). Renders the editor widget for that file; when `diff` is set, layers a `DiffLayer` over the same widget — diff is a mode of this tab, not a separate kind. Snapshot review, trash preview, staging-proposal review, dirty-buffer diff, history diff (right-click → Show changes) are all `buffer` tabs with different `DiffSource` selections. All current tab semantics (preview slot, dirty marker, close guard, autosave participation, tree-click activation, search-result-click activation, navigation-history entries) describe this kind.
- (There is no `agent` tab — the in-app chat was removed in the core rework. External agents reach the vault via MCP, `mcp.md`.)
- `graph` — payload is the graph view's state (filter set, selection); renders a graph-view canvas (per `design.md`'s graph-view future bullet).
- `home` — vault home overview (per [[spec:vault-home-screen]]); renders the home page as the tab's content.
- `home-detail` — payload is the detail-view kind (`stats` | `recent-activity` | `recent-modified` | `recent-accessed`); renders the home page's drill-in view.
- `queue` — task queue + indexer detail view (per [[spec:task-queue-home-detail-view]]).
- `settings` — settings pane (per [[spec:settings-pane-mode]]).
- `properties` — payload is a vault-relative note path; renders the read-only properties inspector for that note (per [[spec:note-properties-tab]]). One properties tab per note path; opening Properties on a path that already has a tab open switches to it rather than spawning a duplicate.
- `cluster-review` — payload is a `ClusterReviewState` (purpose `new-tree` | `recluster-subtree` | `rebuild`, plus the in-flight build config and any in-memory structural result). Renders the clustering review surface ([[spec:cluster-review-tab]] in `cluster-editor.md`) — configure → run → review → confirm. On Confirm it transitions in place to `cluster-batch-review` for the newly-persisted tree.

Tab-strip rendering is kind-aware: a small leading icon distinguishes the kind (per the toolbar icon palette), and the label is whatever the kind chooses (basename for `buffer`, session preview for `agent`, "Graph" / "Home" / "Queue" / "Settings" etc. for app pages).

**App-page tabs default-land in the preview slot.** Clicking the Home / Queue / Settings buttons opens the corresponding tab as a *preview*, replacing whatever preview was there (same one-preview-at-a-time rule as [[spec:editor-preview-tab]]). Promotion to sticky uses the same affordances as buffer previews (right-click "Keep open", or a tab-body interaction signalling "I'm staying" — per-kind: home-detail clicks within the page promote; settings flips do not).

**Buffer-scoped chrome hides when the active tab is non-buffer.** The editor toolbar's buffer-scoped controls (View menu, Save button, Diff button, Mutations menu, the mode-controls slot) and the bottom status bar (line:col, index-state label, file-path) are buffer-only — they hide entirely when the active tab is `agent`, `graph`, `home`, `home-detail`, `queue`, or `settings`. The sidebar / discovery toggle icons stay visible regardless because they control the side panels independently of the center pane. Each non-buffer kind brings its own chrome (or none) inside the tab body — settings has its scope toggle and refresh button in its own header, home has its overview/detail toggle, etc.

**Kind-aware predicates.** Existing tab semantics that assume "every tab is a file buffer" gate on kind:

- **Preview slot** ([[spec:editor-preview-tab]]) — buffer-only on the *contents-tracking* side (paths replace each other in the slot). App-page tabs use the same one-slot-per-strip rule; opening an app-page tab evicts whatever was previewed before (buffer or app page).
- **Dirty marker** ([[spec:editor-tab-dirty-marker]]) is `buffer`-only — non-buffer tabs have no dirty concept.
- **Close guard** ([[spec:file-switch-guard-dirty]]) only fires when closing a `buffer` tab whose buffer is dirty.
- **Autosave tab-state** ([[spec:autosave-tab-state-store]]) records `(kind, payload)` per open tab; restore reopens each kind through its own mount path.
- **Reveal in tree** ([[spec:editor-tab-context-menu]]) only applies to `buffer` tabs.

[tab-kinds]
status:: done
implements:: [[code:hiker/autosave/TabState#open_tab_kinds]]
note:: `app/src/tab.rs` — one `TabKind::Editor { buffer: BufferSource, diff: Option<DiffSource> }` variant in place of the previous five buffer-shaped variants; `BufferSource::{ Vault, HistoryVersion, PendingProposal, Trash }` discriminates per-source loading + read/write posture. Dispatcher in `workbench_host.rs` routes by `BufferSource` to the existing per-kind body renderer (buffer / version_preview / pending_preview / trash_preview); diff (when set) layers via `diff_overlay` on the same widget


### Note properties tab

Right-click → Properties on a tree row opens a `properties`-kind tab for that note — a read-only inspector of every piece of state hiker tracks for the note across `index.db` and the op log ("what does hiker actually know about this file"). Useful for debugging skip reasons, embedder-version drift, the change log, and trail / cluster membership. Frontmatter editing is **not** part of this tab ([[spec:tree-context-properties-frontmatter-editing]] is a separate future surface).

- **One properties tab per note path.** Opening Properties on a path that already has one switches to it rather than duplicating — same shape as the file-tree click rule for buffer tabs. [note-properties-tab]
status:: partial
touches:: [[code:hiker/panels/properties]], [[code:hiker/store]]
note:: basic shell landed: Identity / File state / Index state / Chunks / Access tracking / History / Trail + cluster membership sections rendered. **Partial**: live-refresh (subscribing to events), per-chunk detail, trash-entry properties, and properties for non-note paths remain planned. · evidence: `core/src/store.rs` (`note_properties`), `app/src/panels/properties.rs` (`show()` — snapshot count via `op_writes::snapshot_history(..).len()`; Trail membership via `core::trails::containing_note_with_paths`; cluster membership via the persisted trees)
- **Read-only data view, no editor chrome.** Non-buffer per [[spec:tab-kinds]], so the editor toolbar and status bar hide on activation. The tab body owns its own header (basename + relative path). No save button, no dirty marker. [note-properties-tab-no-editor-chrome]
status:: done
touches:: [[code:hiker/panels/properties]], [[code:hiker/workbench_host]]
note:: properties pane shown/hidden by the workbench host; tab body owns its own header (note path); no save button, no dirty marker per [[spec:tab-kinds]]. · evidence: `app/src/panels/properties.rs` (`show()`); `app/src/workbench_host.rs` hides toolbar + status bar for non-buffer tab kinds
- **Preview-slot rule applies on open.** Default-lands in the preview slot like `home` / `queue` / `settings` (per [[spec:tab-kinds]]); a second Properties open replaces the preview, with standard promotion paths. [note-properties-tab-preview-slot]
status:: partial
touches:: [[code:hiker/panels/properties]]
note:: properties tabs open sticky (directed action, no preview). Spec wants preview-slot landing per [[spec:tab-kinds]] app-page rule; deferred. · evidence: `app/src/panels/properties.rs` (properties tabs open sticky)
- **Live-refreshing.** Subscribes to indexer-progress events (notes-row / chunks), op-log append events (changes section), and watcher file events (mtime / size). No manual refresh button. [note-properties-tab-live-refresh]
status:: planned
note:: sections refresh on indexer-progress events (notes-row + chunks), op-log append events (changes section), watcher file events (mtime / size). No manual refresh button

#### Sections rendered

Each section is a labeled block stacked vertically; sections render in order regardless of whether they have content (a missing row shows an empty-state line). [note-properties-tab-content]
status:: planned
implements:: [[code:hiker/store/dto/NoteProperties]]
touches:: [[code:hiker/store/notes]]
note:: sections rendered in order: Identity (the vault path), File state (mtime / size / `content_hash` / extension), Index state (`indexed_at`, `embedder_version`, `skipped`, `skip_reason`, runtime classification), Chunks (count + per-chunk index/byte-range/heading_path/snippet), Access tracking (`last_accessed_at`), History (snapshot count + recent snapshot rows; row click → [[spec:snapshot-preview-mode]]), Trail / cluster membership

- **Identity.** The vault path (identity is the path per [[spec:store-path-is-identity]]; no minted id, no `path → id` table).
- **File state.** mtime, size, `content_hash` (full blake3 hex, copyable), extension, and whether the path is open in the buffer set / another tab.
- **Index state.** `indexed_at`, `embedder_version`, `skipped` flag + `skip_reason`, and the runtime classification (`Indexed` / `Skipped` / `Queued` / `Unsupported`) — same surface that drives the tree row markers and [[spec:status-bar-active-file-index-state]].
- **Chunks.** Total count plus a compact per-chunk list (index, byte range, `heading_path`, ~80-char snippet). Long lists virtualize; debugging aid, not a search UI.
- **Access tracking.** `last_accessed_at` (per [[spec:note-access-tracking]]), relative time with absolute on hover.
- **History.** Snapshot count for this path and the most recent N snapshot rows (timestamp). Each row click opens the snapshot in [[spec:snapshot-preview-mode]], sharing the version-dropdown code path. (No author breakdown — snapshots carry no author class; git's `Hiker-Author` trailers are the attribution record when git is integrated.)
- **Trail / cluster membership.** Trails containing this note (via `core::trails::trails_containing_note_with_paths`) and clusters it belongs to (placeholder when no clustering data).

#### Behavior details

- **Open paths.** Right-click → Properties ([[spec:tree-context-properties]]) is the canonical entry; a `Show properties` verb in the buffer tab context menu ([[spec:editor-tab-context-menu]]) and a future buffer-body entry are the others. Programmatic `openProperties(rel)` skips the preview slot per the directed-action rule.
- **Path doesn't resolve.** If the path no longer exists on disk when the tab activates (deleted / moved externally), the tab renders a "Note not found at `<path>`" empty state but still shows whatever the index and changes db know — exactly the case the inspector exists to surface.
- **Trash entries.** Right-clicking a trash row → Properties opens the same tab kind for the trashed note (trash-relative path). "Index state" shows `Skipped`; "Changes" shows the row recorded at delete time. [note-properties-tab-trash]
status:: planned
note:: right-click on a trash row → Properties opens the same tab kind for the trashed note; Index state shows `Skipped`, Changes shows the row recorded at delete time
- **Autosave tab-state.** Properties tabs participate in [[spec:autosave-tab-state-store]] like every kind — open at quit, reopens at the same path on next launch.
- **Reveal in tree.** Tab right-click → Reveal in tree highlights the note in the file tree.
- **No write affordances in v1.** Frontmatter editing, force-reindex, change-row restore are follow-up candidates; v1 is strictly read-only. Force-reindex is the likely first write addition ([[spec:note-properties-force-reindex]]).

#### Out of scope (deferred)

- **In-place frontmatter editing.** Tracked under [[spec:tree-context-properties-frontmatter-editing]].
- **Force-reindex this note.** A button submitting a single-note `IndexJob::Reindex`. [note-properties-force-reindex]
status:: planned
note:: deferred — single-note force-reindex button inside the properties tab; submits `IndexJob::Reindex` for that path. Lights up when there's a real debugging use case
- **Restore-from-this-row inline.** Redundant — the changes section already opens each row in [[spec:snapshot-preview-mode]], which carries Restore.
- **Properties for non-note paths** (folders, non-`.md`/`.txt` trash entries). Folder properties are a different surface (recursive note count, total bytes). [note-properties-tab-folder-deferred]
status:: planned
note:: deferred — folder-shaped properties tab (recursive note count, total bytes, etc.). Different surface; revisit when there's an ask
- **Comparison view across two notes.**


### Preview tabs

The preview-tab mechanic — at most one preview slot, italic title, replace-in-place on the next preview-open, promote-to-sticky on edit / double-click / drag / "Keep open" — is `egui_workbench` (`egui-workbench/SPEC.md` §5.3). Hiker wires which callsites open preview and how directed actions opt out:

- **Every click-driven open-note callsite uses the preview slot by default.** File-tree click, search-result click, related-notes click, recents click, wikilink click — all route through `openFile(rel, { preview: true })`. Uniform on purpose: "click is preview, Mod-click is sticky." [editor-preview-tab-from-open-callsites]
status:: done
touches:: [[code:hiker/panels/home]], [[code:hiker/workbench_host]]
note:: uniform shape: every click-driven open reads the Mod modifier and inverts to `preview`. The right-click "Open" tree verb, new-note creation, mutation-apply, and trash-restore all stay sticky · evidence: `app/src/sidebar/files.rs` (single-click preview open), `app/src/panels/search.rs` (search hit click handlers) + `app/src/panels/related.rs` (related hit click handlers), `app/src/panels/home.rs` (recents click), `app/src/workbench_host.rs` (vault-home / discovery forward the opts)
- **Mod-click on any open-note callsite forces a sticky tab.** Skips the preview slot, opens directly into a new sticky tab. Drag-from-tree (when that's a thing) is also implicitly sticky. [editor-preview-tab-mod-click-sticky]
status:: done
note:: covers tree, search, related, and recents. Spec note: drag-from-tree is also implicitly sticky once it grows into a tab-spawning action; today drag fires `move_note`, so no preview wiring needed · evidence: same evidence as [[spec:editor-preview-tab-from-open-callsites]] — every click handler reads the Mod modifier and opens sticky when held
- **Programmatic opens skip preview.** Restore-from-trash, new-note creation, the right-click "Open" tree verb, mutation-apply, and any other non-user-click path open sticky — these are directed actions, not browsing. `openFile` is `{ preview: false }` (or omitted) at those callsites.
- **Edit-as-promotion keeps preview tabs never dirty** — the moment the user types, the tab is sticky, so the dirty-buffer machinery ([[spec:file-switch-guard-dirty]], [[spec:autosave-close-no-modal]]) never has to know about preview tabs. [editor-preview-tab, editor-preview-tab-promotion]
- **Tree double-click stays bound to inline rename** per [[spec:tree-double-click-rename]] — promotion via double-click on a *tree row* would conflict; tab double-click covers the canonical promote gesture.
- **Pending agent proposals route the open into review mode.** When `openFile(rel)` resolves a path with one or more pending staging proposals, the buffer lands in patch-review or write-note review per [[spec:note-open-routes-to-pending-review]] (in `patch-review.md`). The preview-vs-sticky distinction is preserved; the review state rides on `buffer.mode`, not the tab kind.


## Navigation (back / forward)

Browser-style back/forward navigation across editor-pane states. Each user-initiated transition between distinct content surfaces (see `### What pushes onto the stack`) pushes onto a per-vault history stack. Back and forward navigate that stack via the top strip's leading-cluster buttons, trackpad two-finger horizontal swipe, mouse side buttons, and keybinds.

- **History is a per-vault in-memory stack of editor-pane content states.** Cleared on vault swap; not persisted across restarts. [navigation-history-stack]
status:: done
touches:: [[code:hiker/workbench_host]]
note:: per-vault in-memory `back` / `forward` stacks driven by an inferred-vs-tracked-current dedup. Distinct content surfaces tracked: `tab(path)`, `home`, `home-detail{view}`, `queue-detail`, `settings`, `trash-preview{trashedName}`, `snapshot-preview{changeId,row}`. Restoration sets a `restoring` flag so apply-driven side effects (e.g. setting home visible during a back) don't echo onto the stack. Closed-tab entries are pruned (close + preview-replace); invalid restoration targets (closed tab, missing trash item, settings dirty-guard cancel) skip past via the `apply` loop. Cleared on vault swap · evidence: `app/src/state.rs` (back / forward stacks + checkpoint/prune/reset), `app/src/workbench_host.rs` (checkpoint calls on open / preview-replace / tab-activate / close, snapshot / trash open-close wrappers, reset on vault open, prune on tab close + preview-slot replace)
- **Back and forward buttons live in the top strip's leading cluster** (leftmost, before Home / Queue / Settings / Open). Icon-only, disabled when no history exists in that direction. [top-strip-back-button, top-strip-forward-button]
- **Trackpad swipe** — see `### Trackpad swipe shape` below. [navigation-trackpad-swipe]
status:: done
touches:: [[code:hiker/widgets/swipe_nav]]
note:: per-frame accumulator over `ctx.input(|i| i.smooth_scroll_delta.x)`; fires `editor_pane::nav_go(±1)` **the instant `|acc| ≥ 120px`** (2026-06-03: commit-on-threshold replaced the old arm-then-commit-on-release, which waited on the touchpad's momentum-scroll tail and felt laggy / hung), then locks 350ms so one gesture isn't double-counted. Skipped when any widget has focus or the pointer is over a registered `swipe_skip_rects` region — the editor body (`buffer/mod.rs`) and the **canvas viewport in scroll-to-pan mode** ([[spec:canvas-scroll-mode]], so two-finger pan isn't also nav). Horizontal-dominant gate (`|dx| > 1.5·|dy|`). Positive `dx` (swipe right) → back. Opt-out via `[ui].swipe_nav_enabled` ([[spec:navigation-swipe-disable]]). Browser-style mapping · evidence: `app/src/widgets/swipe_nav.rs` (`handle_swipe_nav`), `app/src/state.rs` (`swipe_accum_x`, `swipe_cooldown_until`, `swipe_skip_rects`)
- **Keybind registry entries** reserve `navigation.back` and `navigation.forward`: Cmd/Ctrl-[ back, Cmd/Ctrl-] forward; Alt-Left/Right as additional bindings on Linux/Windows. [navigation-keybind]
status:: done
touches:: [[code:hiker/keybinds]]
note:: editor-focus case + the cross-pane handler cover tree / sidebar / status-bar focus and Linux/Windows browser-conventional Alt-Left/Right · evidence: `app/src/keybinds.rs` (`navigation.back` = `Mod-[`, `navigation.forward` = `Mod-]`, plus `Mod-[` / `Mod-]` outside the editor and Alt-Left / Alt-Right on every platform)
- **Mouse side buttons** (mouse-button-3 back / mouse-button-4 forward) trigger back/forward by default. Detection via window-level `mousedown` / `auxclick` reading `event.button`, calling the same `navigation.back` / `navigation.forward` handlers as the keybind and swipe paths. Default-on; rebinding deferred until the registry grows mouse-button support (keyboard-chord-only today). [navigation-mouse-buttons]
status:: planned
note:: window-level `mousedown` / `auxclick` listener fires `navigation.back` on `event.button === 3` and `navigation.forward` on `event.button === 4` (the standard back/forward thumb buttons on side-button mice). Calls into the same action handlers as [[spec:navigation-keybind]] and [[spec:navigation-trackpad-swipe]] so all three trigger surfaces converge. Default-on; rebinding deferred until the keybind registry grows mouse-button support
- **Dirty-buffer protection** is moot for back/forward — navigating activates a different tab without closing the prior one, so the dirty buffer stays dirty in memory (per [[spec:multi-buffer-no-switch-guard]], [[spec:autosave-close-no-modal]]).


### What pushes onto the stack

A new history entry is appended for each *content-surface change* the user initiated:

- Opening a note (tree click, search-result click, recents click, drag-drop, wikilink click — when wikilinks land).
- Switching to vault home (Home button click, navigating to an empty editor state).
- Drilling into a home detail view (stats tile click, recent-activity tile click, etc.).
- Opening a trash preview.
- Returning from a detail view to home overview (Home button click while in detail).

Things that *don't* push:

- Editing a note (typing, save).
- Tree expand/collapse, panel toggles, filter pill changes within a detail view, search query typing.
- Buffer reload from a watcher event (the buffer's still on the same file).
- Programmatic restore from history (back/forward themselves don't push).

When the user navigates back and then opens a new content surface, the forward stack is discarded.


### Trackpad swipe shape

macOS surfaces the swipe as `wheel` events with `deltaX` accumulation; the editor pane's wheel handler watches for sustained horizontal scroll past a threshold (e.g. ~120px of accumulated `deltaX` over a short time window) and fires the navigation. Vertical swipes are ignored.

**Visual feedback while swiping.** As `deltaX` accumulates past a small floor (~30px) but before the commit threshold, a directional indicator fades in on the swiped-toward edge of the editor pane — a chevron glyph (`‹` for back, `›` for forward) plus a thin progress bar whose fill tracks `|accumulated_deltaX| / threshold`. When the threshold trips, the indicator briefly snaps to fully-filled and fires the navigation; if the user reverses or the 250ms quiet-reset window expires, the indicator fades out without committing. Greyed (indicator visible but desaturated) when there's no history in that direction, so the user gets clear "would commit but nothing to navigate to" feedback rather than a silent no-op. [navigation-swipe-visual-feedback]
status:: done
touches:: [[code:hiker/keybinds]], [[code:hiker/widgets/swipe_indicator]]
note:: rounded pill anchored on the swipe-toward edge (left for back, right for forward), with a chevron glyph and a progress-fill bar that grows with `|swipe_accum_x| / 120`. Greyed pill + chevron when there's no history in that direction (`nav_can_back` / `nav_can_forward`). On commit the accumulator is held at threshold through the 350ms cooldown so the fill flashes the accent colour; abandoned swipes decay exponentially toward 0 after 120ms of no input. Painted on `Order::Foreground` so it sits over all panels · evidence: `app/src/widgets/swipe_indicator.rs` (`show`), `app/src/main.rs` (call after panels), `app/src/state.rs` (`swipe_last_commit_dir`), `app/src/keybinds.rs` (decay + hold-at-threshold during cooldown)

Edge cases worth pinning:

- **Inside the editor.** Horizontal-scroll deltas reach the pane's swipe handler when content isn't horizontally scrollable. When a line *is* horizontally scrolled (code blocks), the swipe still triggers once the horizontal delta substantially exceeds the line's scrollable extent.
- **Inside scrollable detail-view lists.** Same shape — the list scrolls on `deltaY`, so horizontal swipes pass through.
- **Touchscreen devices.** Trackpad-only for v1; touch swipe gestures are a separate slug if a touchscreen variant ever ships.

Closing the vault while history exists drops the entire stack — no warning, no save protection beyond what already gates vault swap.

Out of scope: persisting history across restarts (per-session), and a rich history menu (right-click → list of last N pages, deferred).


## Fonts

Three configurable font slots, all per-vault settings (per [[spec:settings-section-editor]]). One default per slot; the user picks any installed system font. [editor-three-fonts]
status:: partial
implements:: [[code:hiker/panels/settings/render_font_preview]], [[code:hiker/HikerApp]], [[code:hiker/impl#[AppState]install_user_fonts]], [[code:hiker/impl#[HikerApp][App]on_exit]], [[code:hiker/impl#[HikerApp][App]update]]
touches:: [[code:hiker/config/sections]]
note:: three `[editor]` font slots (`font_system` / `font_editor` / `font_code`); code font also drives frontmatter + inline code + diff-layer code hunks; vault-scope default; live-applied; missing-font surfaced in red. **Partial**: the settings UI is free-text font-**path** fields (`font_row`); it should be a dropdown that lists the installed/available fonts — not yet implemented · evidence: `core/src/config/sections.rs` (`font_system` / `font_editor` / `font_code`), `app/src/main.rs` (live-apply), `app/src/panels/settings/mod.rs` (`font_row` + preview)

| Slot | Setting key | Default | Used by |
| ---- | ----------- | ------- | ------- |
| System | `editor.system_font` | platform UI default | every non-editor chrome surface (toolbars, menus, sidebar, status bar, tabs) |
| Editor | `editor.editor_font` | platform default proportional | the editor canvas's prose body — plain paragraphs, headings (size still set by the heading style), list items |
| Code | `editor.code_font` | platform default monospace | fenced code blocks **and** frontmatter blocks (per [[spec:editor-frontmatter-rendering-fix]] in `live-preview.md`); inline code; the diff layer's code-shaped hunks |

- **Selection.** Settings pane row per slot — a font-picker dropdown enumerating installed fonts via the OS font enumeration. Strict-load rejects a name that doesn't resolve at startup; the pane shows the picked-but-missing name in red with a note ("Font not installed").
- **Scope.** Vault-scope by default (matches the rest of `[editor]`). User-scope works via the section's scope toggle for users who want "code font everywhere I open."
- **Live-applied.** Flipping any of the three reflows the affected surfaces on the next frame — no relaunch.
- **No size knob in v1.** Sizing rides the existing theme tokens; a per-slot size knob lands when there's a real ask.

The code font's secondary job — being the frontmatter font — is what gets frontmatter rendering off heading-size and back to body-size; the rule itself lives in `live-preview.md`.


## Editor layer order

The decoration / extension ordering that sets precedence for overlapping ranges is `egui_editor`'s extension surface (`editor/SPEC.md` §9, §13) — the view aggregates decoration providers (pure `&Editor → Set` functions) onto `ViewState` in apply order. Hiker registers the providers it owns onto that pipeline: chunk-boundary decorations ([[spec:view-show-chunk-boundaries]]), the frontmatter-fold block ([[spec:view-hide-frontmatter-toggle]]), the trailing-whitespace decoration gate ([[spec:view-highlight-trailing-whitespace-toggle]]), and the find-match decorations — layered after the widget's built-in gutters / markdown styling and before the theme. Markdown language selection is per-buffer, so opening a non-markdown sidecar swaps styling without rebuilding the editor state. [editor-layer-order]
status:: done
touches:: [[code:hiker/panels/buffer]]
note:: decoration layer wiring order through `show_editor()`

The editor state is created once at startup and reused across buffer switches; switching files dispatches a doc-replacement transaction, never reconstructs the view. [editor-instance-reuse]
status:: done
touches:: [[code:hiker/panels/buffer]]
note:: doc replaced in place on buffer switch


## Out of scope (deferred)

- Live-preview decorations (syntax-marker hiding on cursor-out) — specced in `live-preview.md`
- Wikilink rendering and autocomplete
- Widget-based rendering (LaTeX math, Mermaid, tables, images) — specced in `editor-widgets.md`
- Multi-buffer / tabs / split panes
- Vim/Emacs keymaps
- User keybind overrides (the registry supports it; the loader is later)
- External-change watcher integration (v1)

## Registry imports (from status.md)

Entries imported from the retired status registry that had no anchor in this doc —
re-home them into the relevant sections as the doc evolves.

- **window-close-guard-dirty** — superseded by [[spec:autosave-close-no-modal]]: window close no longer prompts. Dirty buffers flush through autosave and the open-tab snapshot is pushed; next launch auto-restores the workspace as dirty tabs [window-close-guard-dirty]
  status:: removed
- **pre-write-drift-check** — re-reads + hashes before write [pre-write-drift-check]
  status:: done
  touches:: [[code:hiker/vault]]
- **drift-conflict-modal** — keep/take/cancel; no diff option [drift-conflict-modal]
  status:: done
  touches:: [[code:hiker/panels/buffer]]
- **tree-context-properties-frontmatter-editing** — future addition that layers in-place frontmatter editing as a section inside [[spec:note-properties-tab]]. Depends on a frontmatter-editing primitive that doesn't exist in v1; the read-only inspector lands first and frontmatter editing slots in once the primitive exists [tree-context-properties-frontmatter-editing]
  status:: planned
- **confirm3-real-modal** — modal dialog; used by the dirty guard and elsewhere [confirm3-real-modal]
  status:: done
  touches:: [[code:hiker/panels/buffer]]
  note:: evidence: `app/src/panels/buffer/mod.rs` (three-way confirm modal)
- **help-panel-keybinds** — enumerate keybinds.list() [help-panel-keybinds]
  status:: planned
- **note-mutation-one-in-flight-per-path** — menu entry shows "Mutation in progress…" tooltip + disabled when queue events shows an active `NoteMutation` task whose `source_path` matches the active buffer [note-mutation-one-in-flight-per-path]
  status:: done
  touches:: [[code:hiker/toolbar]]
  note:: evidence: `app/src/toolbar.rs` (in-flight set + disable-reason)
- **note-mutation-batch-from-folder** — deferred — folder-context bulk action invoked from the file tree; submits one `NoteMutation` task per eligible note; results land in staging [note-mutation-batch-from-folder]
  status:: planned
- **note-mutation-batch-from-search** — deferred — search-result bulk action sibling to [[spec:search-bulk-action-tag]] / [[spec:search-bulk-action-move]]; submits one task per result; results land in staging [note-mutation-batch-from-search]
  status:: planned
- **view-word-wrap-toggle** — reconfigures line wrapping in the editor crates (`editor-view` `ViewState`); persists via [[spec:settings-write-back]] to `editor.word_wrap` [view-word-wrap-toggle]
  status:: done
  touches:: [[code:hiker/panels/buffer]]
  note:: evidence: `app/src/panels/buffer/mod.rs` ("Word wrap" menu entry)
- **view-show-whitespace-toggle** — whitespace highlighting in the editor crates; default off; toggled via View menu. Persistence still pending [[spec:settings-section-editor]] [view-show-whitespace-toggle]
  status:: done
  touches:: [[code:hiker/panels/buffer]]
  note:: evidence: `app/src/panels/buffer/mod.rs` (whitespace highlight toggle)
- **view-highlight-trailing-whitespace-toggle** — Gates the red-background trailing-whitespace decoration behind a dedicated View-menu flip; default off; persists via [[spec:settings-write-back]] to `editor.highlight_trailing_whitespace` [view-highlight-trailing-whitespace-toggle]
  status:: done
  touches:: [[code:hiker/buffer]], [[code:hiker/config/sections]], [[code:hiker/panels/buffer]]
  note:: evidence: `app/src/panels/buffer.rs` (view-options menu entry + gating around `trailing_whitespace_decorations`), `app/src/buffer.rs` (`highlight_trailing_whitespace`), `core/src/config/sections.rs` (`editor.highlight_trailing_whitespace`)
- **view-line-numbers-toggle** — hides the line-number gutter; default visible. Persistence still pending [[spec:settings-section-editor]] [view-line-numbers-toggle]
  status:: done
  touches:: [[code:hiker/panels/buffer]]
  note:: evidence: `app/src/panels/buffer/mod.rs` (line-number visibility toggle)
- **view-heading-breadcrumb-overlay** — the actual heading-breadcrumb-per-chunk overlay (chunk's `heading_path` rendered above each chunk) that the toggle gates; pairs with [[spec:view-show-chunk-boundaries]] and lights up when both have a real user [view-heading-breadcrumb-overlay]
  status:: planned
- **mutations-menu-icon** — wand glyph (diagonal stick + sparkle); icon-only toolbar button; no click handler — icon reservation, lands with [[spec:note-mutations-menu]] [mutations-menu-icon]
  status:: done
  touches:: [[code:hiker/toolbar]]
  note:: evidence: `app/src/toolbar.rs` (Mutations button)
- **vault-home-recents-detail** — full-list versions of Recently Modified / Recently Accessed; lower priority since each preview row already opens on click [vault-home-recents-detail]
  status:: planned
- **editor-tab-active-state** — active tab gets distinct background + border; inactive tabs render muted [editor-tab-active-state]
  status:: done
  touches:: [[code:hiker/workbench_host]]
  note:: evidence: `app/src/workbench_host.rs` (active-tab styling)
- **editor-tab-dirty-marker** — dirty tabs render a small colored dot; on hover the dot is hidden and a close × is revealed in its place [editor-tab-dirty-marker]
  status:: done
  touches:: [[code:hiker/workbench_host]]
  note:: evidence: `app/src/workbench_host.rs` (dirty-dot / close-× swap)
- **editor-tab-overflow** — tabs shrink to min before the strip becomes horizontally scrollable; active tab auto-scrolls into view on activation. Chevron buttons at each edge + the "more (N)" dropdown are deferred polish — a native scrollbar surfaces in the meantime [editor-tab-overflow]
  status:: partial
  touches:: [[code:hiker/workbench_host]]
  note:: evidence: `app/src/workbench_host.rs` (tab sizing + scroll-into-view on activation)
- **multi-buffer-window-close-guard** — superseded by [[spec:autosave-close-no-modal]]: the multi-buffer close modal is gone. On window close the autosave layer flushes every dirty buffer and pushes the current tab-state snapshot, then the window destroys. Recovered tabs surface as dirty next launch [multi-buffer-window-close-guard]
  status:: removed
- **editor-preview-tab** — replace-in-place preserves the strip's render order (the new entry sits at the *end* of insertion order, matching how the user observes "the same tab kept moving"). Preview tabs are never dirty by construction — the first user-initiated edit promotes the tab, clearing the slot before any dirty check sees it [editor-preview-tab]
  status:: done
  touches:: [[code:hiker/workbench_host]]
  note:: evidence: `app/src/workbench_host.rs` (preview-tab state, replace-in-place swap, preview styling)
- **editor-preview-tab-promotion** — promotion gate distinguishes user typing/paste/delete from programmatic doc swaps (file open, mutation apply); without that gate the tab would promote on the very first edit from opening the file. Drag-to-reorder isn't a promotion path today because tabs don't reorder yet — the spec lists drag, the implementation slots it in when reorder lands. Tree double-click stays bound to inline rename per [[spec:tree-double-click-rename]] (called out in the spec) [editor-preview-tab-promotion]
  status:: done
  touches:: [[code:hiker/panels/buffer]], [[code:hiker/workbench_host]]
  note:: evidence: `app/src/panels/buffer/mod.rs` (first-user-edit promotion), `app/src/workbench_host.rs` (tab-targeted promotion; double-click on the tab promotes; right-click menu prepends "Keep open" when the tab is preview)
- **navigation-swipe-disable** — `[ui].swipe_nav_enabled` toggle to turn off two-finger swipe→Back/Forward (for users who hit false-triggers during ordinary horizontal scroll, e.g. over the tab strip). Default on [navigation-swipe-disable]
  status:: done
  touches:: [[code:hiker/panels/settings]], [[code:hiker/widgets/swipe_nav]]
  note:: evidence: `core/src/config/mod.rs` (`Ui::swipe_nav_enabled`, default true) + `patch.rs` (eligible bool, both scopes); gate in `app/src/widgets/swipe_nav.rs::handle_swipe_nav`; settings row `app/src/panels/settings/mod.rs::window_section`
- **canvas-scroll-mode** — `[ui].canvas_scroll_mode`: a plain scroll over empty canvas resolves to pan or zoom — **auto** (default) detects the device (mouse wheel zooms, touchpad pans), **pan** / **zoom** force one. Ctrl/Cmd+scroll and pinch always zoom regardless (pinch now on Linux/Wayland too via the winit fork). Scroll over a note card still scrolls the card [canvas-scroll-mode]
  status:: done
  implements:: [[code:hiker/panels/canvas/render/canvas_body]]
  verifies:: [[code:hiker/config/tests/write_back_canvas_scroll_mode_validates_allowed_values]]
  note:: evidence: `core/src/config/mod.rs` (`Ui::canvas_scroll_mode`: `CanvasScrollMode` enum Auto/Pan/Zoom, default Auto) + `patch.rs` (`ValueType::CanvasScrollMode`); `hiker-canvas/view-core/src/state.rs` (`ScrollMode` enum) + `hiker-canvas/view/src/widget.rs` (`set_scroll_mode` + `handle_zoom`: reads the scroll's `MouseWheelUnit` — `Line`=wheel, `Point`=touchpad — remembers it across egui's smoothing tail, pan-vs-zoom branch, `camera.pan_by_screen`); host maps config→view enum + registers the canvas swipe-skip when mode ≠ Zoom (`app/src/panels/canvas/render.rs::canvas_body`); shared `canvas_scroll_mode_selector` in the gear menu + `window_section`
- **global-view-menu** — Global eye-icon "View options" menu on the top strip. Popup currently holds a "Reader mode" toggle (dispatches `view.reader_mode`) and a "Hide top bar in reader mode" mirror of `ui.reader_hide_top_bar` (read + toggle + commit to vault scope). Room left for future global view options [global-view-menu]
  status:: done
  touches:: [[code:hiker/actions]], [[code:hiker/toolbar]]
  note:: evidence: `app/src/toolbar.rs` (`AppState::render_view_menu`, `commit_vault_bool`), `app/src/actions.rs` (`ID_VIEW_MENU` = `view.menu`, in `is_layout_id`), `app/src/state.rs` (`view.menu` in default top toolbar)
- **command-center-topbar** — VSCode-style "command center": a centered, clickable search box that opens the command palette (`palette.open`). In the default **frameless** mode it is overlaid centered in the merged titlebar (see [[spec:frameless-merged-titlebar]]); with native chrome it is overlaid centered on the first top toolbar (`render_toolbars(.., overlay_command_center)`, dedicated `command-center` bar only as a fallback). Suppressed in reader view. Shows a search icon + "Search commands" + a platform-appropriate ASCII chord hint (`Cmd+Shift+P` / `Ctrl+Shift+P`). The palette (`command_palette.rs`) dismisses on Esc or a pointer press outside its window. [[spec:command-center-topbar]] [command-center-topbar]
  status:: done
  touches:: [[code:hiker/command_center]], [[code:hiker/titlebar]], [[code:hiker/toolbar]]
  note:: evidence: `app/src/command_center.rs` (`AppState::command_center`), `app/src/titlebar.rs`, `app/src/toolbar.rs`, `app/src/main.rs`
- **frameless-merged-titlebar** — Frameless window (`with_decorations(false)`) is the default. A single 34px titlebar strip merges: the first top toolbar's actions (left, folded in via `toolbar::render_top_bar_inline`), the centered command center, and OS-style window controls (minimize / maximize / close, far right). Dragging uses **discrete drag zones in the empty gaps** (lapce/VSCode no-drag model): the toolbar keeps its layout (head left, spacer-anchored tail like the sidebar toggles right-aligned next to the controls), the command center centers, and `render_bar_items` returns `(head_right, tail_left)` so the titlebar places `click_and_drag` zones in the gaps *between* head, command center, and tail. They fire `ViewportCommand::StartDrag` on pointer-*down* (exact 1:1 tracking); no drag region ever overlaps a button, so clicks are never swallowed. Double-click a gap toggles maximize. Child regions render via `scope_builder` so their clicks take priority. `titlebar::window_resize_handles` adds invisible grips on the left/right/bottom edges + bottom corners that fire `ViewportCommand::BeginResize(dir)` with the matching resize cursor, restoring border resize that frameless windows otherwise lose. **Each grip is its own tiny foreground `Area`** (lapce pattern) so it masks lower-layer panel input only on its own thin strip — a single area spanning all edges blocks the whole window body (sidebar/editor clicks). Each area sets `constrain(false)` so edge-pinned grips aren't clipped/shifted inward (otherwise only the left grip, already at x=0, works). Grips stay below the titlebar so they never overlap its buttons — trade-off: no resize from the top edge or top corners. `main.rs` renders only the secondary (bottom/left/right) toolbars as panels when frameless (`render_secondary_toolbars`). Toggle off via Settings (`Ui::custom_titlebar = false`) to restore native chrome + the on-toolbar command center. **Note:** window-control commands depend on the compositor (Wayland/Asahi may vary) [frameless-merged-titlebar]
  status:: done
  touches:: [[code:hiker/titlebar]]
  note:: evidence: `app/src/titlebar.rs`, `app/src/main.rs`, `core/src/config/mod.rs` (`Ui::custom_titlebar` defaults true)
- **selection-autoscroll** — drag-selecting to (or past) the top/bottom viewport edge autoscrolls so the selection extends off-screen; linear + rectangular (Alt) drags. Backend-neutral mechanic in `editor-view`: a `distance^1.5` speed curve scaled in line-heights (≈½ line/frame at the edge, capped ≈1¼ lines/frame), driven per held frame so it continues while the pointer is still; stops at the band edge, at either document end, or on release. egui adapter only adds the keep-painting repaint signal. Horizontal + text-drag autoscroll deferred (§9.24). Tests: `editor/editor-view/tests/autoscroll.rs` (velocity curve + bottom/top/dead-zone/clamp/mouse-up drag integration) [selection-autoscroll]
  status:: done
  touches:: [[code:hiker/command]], [[code:hiker/viewport]]
  note:: evidence: `editor/SPEC.md` §9.24, `editor/editor-view/src/command.rs` (`selection_autoscroll_velocity` + `apply_selection_autoscroll` + `AUTOSCROLL_*` consts; called from `mouse_drag` `MaybeSelecting` / `RectangleSelecting`, cleared in `mouse_up`), `editor/editor-view/src/viewport.rs` (`ViewState::autoscroll_active`), `editor/editor-egui/src/widget.rs` (repaint while `autoscroll_active`)
