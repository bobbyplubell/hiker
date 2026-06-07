# Editor

Hiker embeds `egui_editor` — the embeddable text-editor **widget** (`editor/SPEC.md`) — as its `buffer` tab kind inside `egui_workbench` — the IDE **shell** (`egui-workbench/SPEC.md`). This doc covers hiker's *integration* of those two crates: the wiring, policy, and hiker-specific surfaces around them — not the generic editing / shell behavior the crate specs own.

Where this doc points elsewhere:

- The widget's editing model — multi-cursor, selection, decorations, markdown live preview, diff view, find+replace, IME, minimap, the view toggles — is `egui_editor` (`editor/SPEC.md`).
- The shell's chrome — activity bar, side bars, editor groups + splits, tab mechanics, panel area, status-bar chrome, layout persistence — is `egui_workbench` (`egui-workbench/SPEC.md`).
- Hiker's file tree / Files activity content is `files.md`; cluster trees are `cluster-editor.md`.

Transactions, decorations, and selections referenced below are the widget's types.


## Buffer model

One open buffer at a time in v0. Switching files replaces the buffer's contents via a single `dispatch` that swaps the entire doc.

The buffer is the editor's rope — the user's own text, `accepted + working` (per `op-log-layered-model`). User typing lands at plain byte offsets; the host mirrors each editor change set into the `working` layer (per `op-log-working-layer`), and Save commits that layer. An agent's `pending(session)` proposals render *on top* as an editor-native anchored overlay — a `DiffLayer` recomputed from two ropes (per `op-log-three-way-overlay`), reviewed via `patch-review.md`.

State tracked per buffer:

- `path` — vault-relative; null when no file is open [buffer-path-tracking]
- `loadedHash` — hash (or full string) of the contents most recently read from / written to disk
- `isDirty` — derived: current doc text !== loadedHash

`isDirty` is the single source of truth for save state — computed lazily from the editor doc and `loadedHash`, no separate flag that can desync. Cleared by re-reads and successful writes; set implicitly by any edit. [buffer-dirty-derived]

Multi-buffer / tabs deferred. When tabs land, the same per-buffer state moves into a `Buffer[]` keyed by path, with the active buffer driving the editor view.

### `transactions_out` seam

The editor widget exposes the change sets it applied from user input — the per-edit `ChangeSet` (retain / delete / insert over byte ranges). The host drains this stream each frame and mirrors each change set into the `working` layer (per `op-log-working-layer`), so the editor stays the source of *what changed* rather than the host re-diffing. The same change sets remap the pending overlay's anchors via the editor's `map_pos` (exact, not fuzzy), keeping the agent's anchored ranges in place as the user types (per `op-log-three-way-overlay`). Reverse-direction edits (an accepted/pending/external change applied back into the editor) carry a sync origin and are not re-emitted, so the mirror can't echo. The seam is editor-crate-owned and host-agnostic — it feeds any consumer needing a precise edit log. [editor-transactions-out]


### Embedded buffer view (one note, many places)

A reusable primitive for rendering an **editable** view of a vault note *anywhere* in the UI — not only in its dedicated buffer tab — so the same note can appear in two places at once (a canvas file-node card, a board's Markdown view, a split pane) and "typing on a note shows up wherever the note is." The model is **one shared editor, many views**: [embedded-buffer-view]

- **Shared (one per path):** the note's `Editor` — document + selection/cursor + undo history — lives in the single `session.buffers[path]` buffer. There is never a second dirty copy of a note; loading a note that already has a dirty buffer just attaches to that buffer. Cursor and undo are shared across every view, because they live on the one `Editor`.
- **Per-view (one per embedding site):** each host owns its own `ViewState` + `PaintCache` (scroll offset, content zoom, wrap, viewport, galley cache). So a 300px canvas card and a full-height tab of the same note scroll and zoom independently while showing the same text.
- **Host-agnostic render call:** a single helper renders `session.buffers[path]`'s `Editor` through the editor widget against a caller-supplied `(ViewState, PaintCache)` at the caller's rect, drains `editor-transactions-out`, and mirrors the change sets into the `working` layer (`op-log-working-layer`) for that path. Because the mirror runs from *whichever host drew the editor this frame*, edits reach `working` even when no buffer tab is open — so save / autosave / agent-review / dirty-tracking work identically regardless of where the editing happened. (Only the focused view receives keystrokes per frame, so the mirror is driven by one host at a time; views render sequentially, never holding two `&mut Editor` borrows at once.)
- **Lifecycle.** Buffer eviction is reference-counted across *all* hosts, not just tabs: a note kept open only by a canvas card (no tab) stays loaded, dirty-tracked, and autosaved until the last host releases it. The tab-only "drop when no tab references this path" rule generalizes to "drop when no tab **or embed** references it." [embedded-buffer-view-lifecycle]

Consumers: the canvas inline editor (`canvas-inline-edit`), and — as they adopt it — the board Markdown view (`board-view-toggle`) and editor split panes, which today each load the buffer but render their own editor. The buffer tab panel is the reference renderer; the helper is the extracted, chrome-free core it and every embed share.


## Save UX

Save action: commits the buffer's `working` layer (`commit_working`, per `op-log.md`'s "Disk write invariant"), which folds the user's uncommitted edits into `accepted` and materializes that to `currentPath`. On success, updates `loadedHash` to the new doc text, which clears `isDirty`. On error, surfaces a non-blocking error toast and leaves the dirty state alone (so the user can retry).

Triggers (all funnel into the same save function):

| Trigger | Binding / location |
| --- | --- |
| Keybind | Mod-S [save-keybind-mod-s] |
| Toolbar button | Floppy-disk icon left of View options; always visible; disabled when no file is open or not dirty [save-button] |

Save writes the user's file. Crash-recovery autosave (sidecar shadow copies of dirty buffers, NPP shape) is a separate mechanism — see `autosave.md`. The two paths don't overlap: saving clears the autosave sidecar for that path, autosave never touches the user's file.

Dirty indicator:

- Window title shows `• Hiker — <path>` when dirty, `Hiker — <path>` when clean. [dirty-window-title]
- Active file in the tree shows a small dot suffix when its buffer is dirty. The active tab in the strip shows the same dot. [dirty-tree-dot]
- The save button carries no dirty marker — its enabled/disabled state is the signal, and the tab + tree dots cover the rest.

File-switch guard fires on **explicit close** of a dirty tab (× / middle-click / `tab.close` keybind) — a confirm dialog with three options: Save & close, Discard & close, Cancel. Cancel keeps the tab open. Switching away from a dirty tab (tab click / file-tree click / search-result click) does *not* fire the guard — the buffer stays dirty in memory. Window close has no dirty-buffer modal; see `## Multi-buffer model`. [file-switch-guard-dirty]

External changes: a file edited on disk outside hiker reconciles into the `accepted` layer as an `external` op (per `op-log-external-edit-sync`). Because the buffer materializes `accepted + working`, an external change and the user's uncommitted `working` edits merge by position: disjoint regions auto-merge with no prompt, and an overlapping region surfaces as a conflict hunk with **Keep mine / Keep theirs / Keep both** — the same model agent proposals use (per `op-log-merge-auto`, `op-log-merge-conflict`).

Save does **not** re-read disk or compare hashes — it commits `working` directly (per the Save action above). Disk drift is reconciled at **open-time** and via the **watcher** (per `op-log.md`), not at save time:

- Open-time reconcile folds any on-disk delta into `accepted` before the buffer shows.
- Watcher integration: the notify-based watcher pushes file-change events for the open file. Buffer clean → silently reload, `loadedHash` updates. Buffer with `working` edits → the same conflict-hunk reconciliation, proactive on the event.

A save-time drift check / `DiskDrift` modal is specced-but-dormant — tracked as bug `bug-editor-no-save-time-drift-check` in `bug_tracking.md`.


## Keybind registry

Two scopes. **Window-level chords** (`app/src/keybinds.rs`) fire regardless of focus; `Keybinds::handle_keybinds(ctx)` runs once per frame before the editor widget and consumes each chord via `ctx.input_mut(|i| i.consume_key(...))`, so a matched window-level chord never reaches the buffer. **Buffer-local chords** (`editor-view::command::handle`) only fire when the editor has focus. The split between the two registries *is* the scope today (a future `scope` field could refine it). Goals: discoverable, overridable (later), conflict-detectable. [keybind-registry]

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

- left: a **version dropdown** for the active buffer's file. Closed-state label is the basename plus a mode qualifier when a non-current version is selected (e.g. `note.md`, `note.md — Snapshot 2m ago`, `note.md — Staging · chat`). The full vault-relative path stays in the `title=` tooltip on hover. See `## Version dropdown` below. [status-bar-version-dropdown]
- center: index status label (v1+) — short text reflecting indexer state. Concretely: `Model loading…` while the embedder loads, `Indexing X/Y` while jobs flow (X = remaining queue depth, Y = total since last idle), `Indexed (N notes)` when idle, `Index error` (with last_error in title attribute) when the indexer reports a failure. Plain text, no icons in v1; styling can come later. [status-bar-index-label]

  When the *active buffer*'s file is in a non-indexed state (per `cmd-file-index-state` in `index.md`), the center label is replaced for that file's lifetime as the active buffer with a file-specific message: `Not indexed (unsupported filetype)` for unsupported extensions, `Skipped — <reason>` for skipped files (reason string straight from the indexer), `Queued for indexing` while the file's job is pending. Reverts to the aggregate label once the file becomes indexed (or another file opens). [status-bar-active-file-index-state]
- right: line:col, word count, file type badge (`md`)

Click targets:

- dropdown → opens the version list (see below).
- right-click on the dropdown's closed-state label → context menu with "Reveal in file manager" (Finder on macOS, File Explorer on Windows, default file manager on Linux; via the OS shell/opener). Suppressed for trash-preview / snapshot / staging buffers so internal `.hiker/` paths don't leak. [status-bar-path-reveal]
- line:col → opens a goto-line input (deferred; click is a no-op in v0) [status-bar-goto-line]


### Version dropdown

The left region of the status bar is a single dropdown that lists every addressable version of the active buffer's file. Selecting an entry switches the editor view to that version (live editor, snapshot preview, or staging preview), without changing tabs or pane state. [status-bar-version-dropdown]

Entries, in fixed group order, newest within each group:

1. **Current** — the live, editable on-disk version. Always present, always the first entry. Selecting it exits any snapshot / staging preview the buffer is in and returns to the editable buffer (same code path as the existing exit-preview transitions).
2. **Snapshots** — every `.ops` history frame for this path within retention (`op_writes::path_history`), one entry per frame. Label: `Snapshot · <relative-time> · <author> · <op>`. Selecting an entry enters `snapshot-preview-mode` against that frame (same code path as clicking a row on the activity detail page). The current on-disk state already appears as the top "Current" entry, so the most-recent snapshot row is *not* hidden — it represents the saved version, which may diverge from the live buffer if the user has unsaved edits.
3. **Pending proposals** — every pending whole-file proposal whose `target_path` equals this file (`op_writes::list_whole_file_proposals`). Label: `Proposal · <surface> · <relative-time>`. Selecting an entry opens the proposal's content as a read-only proposal preview (same code path as clicking a proposal row on the activity detail page).

The selected entry reflects what's currently in view. Closed-state label mirrors that selection (e.g. `note.md — Snapshot 2m ago · agent:claude`), so the user can tell at a glance which version the editor is showing without opening the dropdown. Mode-specific verbs (Restore, Accept / Reject, Diff toggle) stay in the editor toolbar's `#mode-controls` slot per `editor-toolbar-mode-controls`; the dropdown is purely a version selector. [status-bar-version-dropdown-selection]

The dropdown is buffer-only — it hides for non-buffer tab kinds the same way the rest of the status bar does (`tab-kinds`). For a buffer whose file does not yet exist on disk (newly-created, never saved), only the "Current" entry appears.

Population:

- Snapshots and staging entries come from `core::activity::list_for_path(path, filter)` (see `op-log.md` "Unified activity feed"), so the dropdown shares the merged-feed type with the activity detail page. [status-bar-version-dropdown-uses-unified-feed]
- The list refreshes on op-log append events and staging-snapshot updates for events touching the active buffer's path; debounced consistent with the activity widget. [status-bar-version-dropdown-live-refresh]

Trash entries are out of scope — a trash entry *is* a different file on disk (different path), not a version of the open buffer; surfaced via `tree-trash-preview`.


### Sibling protection (overflow rule)

Every status-bar region — and any horizontal toolbar / strip elsewhere in the app — truncates user-derived content (file names, error messages, status labels) with an ellipsis so a long string in one region can't push its siblings off-screen. [ui-no-sibling-pushout]


## Layout

The four-region workbench shell — activity bar, side bars (as accordion sections), editor groups + splits + resizable splitters, panel area, status-bar chrome, and layout persistence — is `egui_workbench` (`egui-workbench/SPEC.md` §1–§8). This section covers only hiker's wiring of that shell: which activities mount, what fills the editor toolbar, and the hiker-specific panels.

Hiker's region map:

- **Top strip**: a single horizontal strip across the full width of the window — leading cluster of icon buttons (Back / Forward / Home / Queue / Settings / Open vault) plus the active vault path label, then the tab strip filling the rest. Hiker-specific buttons + behavior are in `## Top strip` below. [top-strip-layout]
- **Left (primary side bar)**: hosts the Files / Cluster-trees / Trails activities. The file tree is `files.md`; cluster trees are `cluster-editor.md`. The side-bar / accordion mechanics (sections, headers, collapse, resize, drag-to-add, persistence) are `egui_workbench`. The sidebar collapse toggle is `sidebar-toggle-icon` below. [four-region-layout]
- **Center (editor area)**: the editor pane — a thin toolbar strip across its top, the `egui_editor` widget below, the status bar beneath. Toolbar contents are hiker-specific; see the editor-toolbar wiring below.
- **Right (discovery panel)**: related-notes panel. Renders `RelatedHit[]` from `related_notes(currentPath)`, updated on file-open and on save (debounced 500ms per `index.md`). [related-notes-panel-ui]

### Editor-toolbar wiring

The editor pane's toolbar (hiker chrome, not the widget's) holds, left-to-right: the sidebar toggle, Save (floppy icon), the dirty-buffer Diff toggle (`editor-diff-vs-disk-toggle`), a centered `#mode-controls` slot between two flex spacers (see `## Mode controls slot`), then the View menu button (eye icon, `## View options menu`), the Mutations menu (wand, `## Note-mutations menu`), and the discovery-panel toggle. The two panel-toggle buttons are always visible; their pressed/unpressed state reflects whether the corresponding side panel is open. [panel-toggle-buttons]

- **Sidebar toggle icon.** A safe-dial / ship-wheel glyph (round with spokes) inside a rounded-square frame. Tooltip "Toggle sidebar." [sidebar-toggle-icon]
- **Discovery toggle icon.** A magnifying glass (the panel's primary surface is search-driven retrieval, per `search.md`). Tooltip "Toggle discovery panel." [discovery-toggle-icon]

Default state on first launch: tree open, related panel collapsed. Persistence of these toggles rides the workbench layout persistence (per-vault); side-column resize, min/max clamps, and the resize handles are `egui_workbench` mechanics. [side-panel-resize]


## Mode controls slot

The editor toolbar reserves a centered `#mode-controls` slot between two flex spacers. The slot is empty during normal editing; entering a read-only preview mode populates it with mode-specific icon-only buttons plus a short text label naming the mode. One slot, one render path, per-mode populators. [editor-toolbar-mode-controls]

What lands in the slot:

- **Icon-only action buttons** for the mode's verbs: Diff toggle (`editor-diff-vs-disk-toggle` below), Restore, Apply, Reject, Close — whichever the active mode exposes. Icons match the toolbar palette; stateful icons reflect toggle state. The mode qualifier naming which non-current version is in view sits in the version dropdown's closed-state label (`status-bar-version-dropdown`), keeping the toolbar compact.

The render path reads buffer state (mode kind, dirty flag, diff-active) and rebuilds the slot on every affecting transition (buffer swap, mode entry/exit, dirty toggle, diff on/off). Per-mode populators (snapshot, trash, dirty-buffer, future staging) each render label + icons and mutate nothing directly — state changes go through the buffer/preview API.

### Dirty-buffer Diff toggle

A diff toggle lives in the editor toolbar (just right of Save). Greyed when the buffer is clean *and* no other diff source is selected (nothing to diff against). Click toggles the editor tab's `diff` mode against the current `DiffSource` (see `diff.md` `diff-as-mode` and `diff-source-enum`); the default source is `Disk(path)` — the live buffer vs. last-loaded content. The flip is non-destructive: the buffer's `current` is unchanged, decorations are layered on top; toggling off restores cursor + selection. **Right-click opens a source picker** — a small context menu offering: `Diff against on-disk`, `Show changes…` (submenu of recent op-log rows for this path), and future sources (snapshot, another open buffer). Selecting a source switches the tab's `DiffSource` and turns diff mode on. [editor-diff-vs-disk-toggle, editor-show-changes-menu]

Constraints:

- **Disabled when there's nothing to diff.** Buffer clean *and* `DiffSource` is `Disk(path)` *and* the path exists on disk → toggle is disabled with tooltip "No changes to show."
- **Newly-created buffer (file not on disk yet).** Toggle is disabled with tooltip "Save first to diff against disk." The source picker still works for non-disk sources (e.g. another open buffer).

### Show changes menu

The right-click context menu on the diff toggle (and the buffer's body, when no selection is active) carries a `Show changes…` entry whose submenu lists recent `.ops` history frames for the active buffer's path (via `op_writes::path_history`), newest first. Selecting a row sets the tab's `DiffSource = HistoryVersion { path, frame_id }` and turns diff mode on; the buffer's `current` text stays put, and `agent_base` (if any) is unaffected. [editor-show-changes-menu]

- **Submenu shape.** Up to 20 recent rows. Each row shows timestamp (relative + absolute on hover), op kind (`created` / `modified` / `deleted` / `renamed`), and author. Final row: `Browse all… → ` opens the `home-detail { which: activity-row { path } }` tab filtered to this path (per `vault-home-recent-activity-detail`).
- **Per-hunk restore.** When the diff source is `HistoryVersion { path, frame_id }`, hunks carry a `Restore this hunk` overlay verb (owner `Snapshot` per `diff.md`'s `diff-layer-owner`). Restore writes the historical text for that hunk's range into `current` and lets the user save through the normal path. Full-snapshot restore stays on the row-level surface (`vault-home-recent-activity-detail`), unchanged.
- **No URI scheme.** The diff resolves directly through `oplog::materialize_at(path, frame_id)`; the editor crate doesn't go through a custom URI provider.


## Find in note

In-buffer find / replace is the `egui_editor` search panel (`editor/SPEC.md` §6, §9.13) — triggered by Mod-F, with case / whole-word / regex / in-selection toggles, match highlights, and gutter + minimap match ticks. Hiker enables that panel on the buffer tab kind; it doesn't re-implement it. [editor-find-in-note]

Hiker boundary: in-buffer find is "jump to this string in *this* file." Cross-file find is the discovery panel's job (per `search.md`); the in-buffer bar must not grow into a second search surface.


## Reader / focus mode

A workbench-level focus mode that hides all chrome except the global top bar and focuses the active tab full-window — aimed at distraction-free reading and the long-form writing case. A single session-level flag on the workbench, not per-buffer, so it works on any focused tab. Not persisted. [view-reader-mode]

- **Trigger.** Ctrl+R (Cmd+R) and a global book-icon button on the top strip. Also reachable from the global eye-icon View menu (`global-view-menu`) and the editor toolbar menu as a regular toggle row. **Right-clicking the book icon** opens the reader-view-specific options (the hide toggles below) as a context menu, so they're reachable straight from the reader icon.
- **Exit.** The same toggle, or Esc.
- **What's hidden by default.** Every workbench chrome region — activity bar, both side bars, status bar, panel area — plus the editor's own status bar / gutter / minimap. The active tab fills the window. The global top bar, the tab strip, and each view's in-tab toolbar all stay by default; three opt-in toggles hide them.
- **Optional hide toggles.** Independent reader-mode settings, all shown in the eye View menu, the book-icon right-click menu, and Settings; each takes effect next frame, vault-scoped:
  - **Top bar** — `ui.reader_hide_top_bar` hides the global top bar entirely (custom titlebar or native top toolbar). In frameless mode the window resize grips remain and Ctrl+R is the exit. [view-reader-hide-top-bar]
  - **Tabs** — `ui.reader_hide_tabs` suppresses the editor-area tab strip via the workbench's `hide_tab_strip` render-time gate (`tab_bar_height` → 0 + a no-paint `tab_ui`); the tabs and layout are untouched, so the strip returns when cleared. [view-reader-hide-tabs]
  - **Toolbar** — `ui.reader_hide_toolbar` hides each view's in-tab toolbar (the canvas create toolbar, the editor toolbar). Gated through `AppState::reader_hides_view_toolbar`. [view-reader-hide-toolbar]
- **Scope.** Switching tabs stays in reader mode and focuses the new tab. The flag gates chrome at render time only — the user's collapse choices and layout persistence are untouched.


## Command palette

Fuzzy-search popover over every registered keybind action — the discoverability surface for the keybind registry (`keybind-registry`). [command-palette]

- **Trigger.** Keybind `vault.commandPalette` = Mod-Shift-P (reserved in `keybind-registry`'s "Reserved IDs" table; this spec lights it up), and a top-strip icon when wired.
- **Surface.** A centered overlay popover above the editor pane: a text input at the top, a scrollable result list below, footer hint listing accept / dismiss bindings.
- **Action source.** The keybind registry is the source of truth — every entry in `Keybinds::known_keybindings()` is a palette row. Adding a registry entry adds a palette row for free.
- **Row shape.** Action title (the registry's human label), source area as a small badge ("editor" / "tab" / "navigation" / "vault" / etc.) inferred from the action's id prefix, and the bound chord on the right (or `Unbound` when no chord is set). Greyed rows when the action isn't currently dispatchable (e.g. `editor.save` when no buffer is open).
- **Ranking.** Fuzzy match on the human label first, then on the action id. Recent invocations float up via a small per-session MRU list — same shape as the chat `@`-autocomplete recency tiebreaker, in-memory only.
- **Invocation.** Enter (or click) fires the action through the same dispatch path the keybind handler uses — palette is a discovery surface, not a parallel runtime. Esc dismisses.
- **No payload prompting in v1.** Actions that take arguments (a future "Open file by name" action) aren't in the palette until their entry-point becomes a side-effect-free invocation; palette rows are zero-argument verbs. Picker-driven actions (open vault, open recent) plug in by registering a no-arg "open the picker" verb, not by spawning their UI from the palette.
- **AI-touching actions are hidden under `[llm] enabled = false`** (per `llm-features-disable-entirely`). The filter runs at render time so a flip applies live.
- **Module placement.** Popover lives in `app/src/panels/command_palette.rs`; the action list it reads is `Keybinds::known_keybindings()` plus per-action metadata (label, area badge, dispatchable predicate).

The palette coexists with right-click context menus and the View menu — keyboard-first answer, menus stay mouse-first.


## Click selection patterns

Double/triple-click word/line selection is `egui_editor` (`editor/SPEC.md` §2.2). Hiker layers one thing on top: **what** a click selects is regex-configurable via `[editor]` config — each click runs `view.{double,triple}_click_re` against the clicked line and selects the match whose span contains the cursor column. There is no separate "built-in" path; the historic Unicode-word / whole-line behavior is just the default regex. An empty config value resets to the default; an invalid regex logs once and falls back to the default, so a typo can never break selection. [click-select-pattern]

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

The editor pane's top toolbar (`panel-toggle-buttons`) gains a View menu button alongside the tree- and related-panel toggles. The menu hosts display-only toggles — flips that change how the active note is rendered without touching the file or the index. Sibling to the deferred `note-mutations-menu`; the split is clean: View changes pixels, Mutations changes bytes. [editor-view-options-menu]

**Icon.** Eye glyph, no text label, no chevron — matches the icon-only treatment of the other toolbar buttons (sidebar wheel, discovery magnifying glass). Tooltip "View options" handles discoverability for the icon-only form. The click target opens the menu directly, same shape the other icon buttons use to open their popovers. [view-menu-icon]


### Toolbar icon palette

The editor pane's top toolbar is converging on icon-only affordances; each menu / button gets a single distinctive glyph in the same visual family (line-weight, frame, sizing). Reserved glyphs:

| Affordance                  | Glyph                                  | Slug                  | Status   |
| --------------------------- | -------------------------------------- | --------------------- | -------- |
| Sidebar toggle              | safe-dial / ship-wheel (rounded-square frame, circle with spokes) | `sidebar-toggle-icon` | landed   |
| Discovery toggle            | magnifying glass                       | `discovery-toggle-icon` | landed |
| View menu                   | eye                                    | `view-menu-icon`      | landed   |
| Mutations menu              | wand                                   | `mutations-menu-icon` | landed (live via `note-mutations-menu`; mutation roster grows under that slug) |

Sidebar-scoped icons (the `+` new-item button, `⋯` actions menu) live in the side-bar panel header, not the editor toolbar — see `files.md`. The activity switcher between Files / Cluster trees / Trails is `egui_workbench`'s activity bar (`egui-workbench/SPEC.md` §1).

Each entry is a checkable item — checkmark when active, click flips it, menu closes on click. State is in-memory only for v1; persistence is a `settings.md` concern when that surface lands.

### v1 entries

- **Show chunk boundaries** — overlays a thin horizontal rule between chunks (pale reddish-orange) and the chunk index in the gutter at each chunk's start line. Backed by `cmd-chunks-for-path` (`index.md`). Refreshes on save (debounced 500ms, same cadence as the related-notes panel). When the file isn't indexed (unsupported / skipped / queued per `cmd-file-index-state`), toggling on shows nothing and a faint gutter hint explains why. Editor integration: a decoration provider (`chunk_boundary_decorations`) emitting the rule + gutter index onto the buffer view's decoration set. A debugging-grade view of the chunker's output. [view-show-chunk-boundaries]

- **Hide frontmatter** — visually collapse the leading `---\n…\n---\n` YAML block into a single placeholder line (`▸ frontmatter (N lines)`) without touching the file. Detection mirrors `core::frontmatter::split` exactly — the block must start at byte 0 with `---\n` and have a closing `---\n` line before any body content; an unterminated or non-leading block is ignored. Editor integration: a block replace decoration (`frontmatter_fold`) over the byte range, recomputed off the document so edits update the placeholder line count immediately. Default off; persistence via `editor.hide_frontmatter` (`settings-section-editor`). [view-hide-frontmatter-toggle]

- **Intraline diff highlights** — augments the line-level red/green diff with character-level highlights inside paired delete/insert lines. Affects every consumer that calls `editor.renderDiff` (snapshot preview, dirty-buffer diff, write-note review). Default off; persistence via `editor.intraline_diff` (`settings-section-editor`). Flipping while a diff is displayed re-renders it with the new style. Does *not* affect the patch-review agent-diff surface (own rules in `patch-review.md`). Full rendering contract in `diff.md`'s "Diff style" section. [view-intraline-diff-toggle]

### Reserved entries (greyed in v1, enabled when their backing feature lands)

These appear in the menu now so the surface is predictable, but render greyed-out with a tooltip naming the dependency.

- **Live preview** — hide/show markdown syntax markers on cursor-out. Specced in `live-preview.md`; entry becomes live (default on) when that ships. [view-live-preview-toggle]
- **Render .txt as markdown** — session-scope override of `txt-render-as-markdown-default` (flip the vault default for the current app session; no file mutation, no persistence in v1). Greyed until `settings-vault-config-toml` lands a per-vault default loader; see `txt-ingest.md`. [view-render-txt-as-markdown-toggle]
- **`egui_editor` feature toggles** — these rows are session/vault-scope flips of the corresponding `egui_editor` features (see `editor/SPEC.md`): Word wrap (§3.8), Show whitespace (special-character rendering, §9.16), Highlight trailing whitespace (§9.17 — quiet enough to leave on for code, noisy on prose, so opt-in; default off, persisted per-vault), Show line numbers (gutter, §3.7). The menu rows are hiker chrome; the rendering is the widget's. [view-word-wrap-toggle, view-show-whitespace-toggle, view-highlight-trailing-whitespace-toggle, view-line-numbers-toggle]
- **Show heading breadcrumb** — overlays each chunk with its `heading_path` (already stored on chunks). Pairs with chunk boundaries; defer until both have a real user. [view-heading-breadcrumb-toggle]

### Out of scope (this menu)

- Content-mutating actions — those live in `note-mutations-menu`.
- Per-file scoped toggles. The menu's scope is "active buffer at most"; per-file persistence is a frontmatter concern that doesn't exist in v1.
- Theme / font / color-scheme — those belong in settings, not a quick toggle.


## Note-mutations menu

A top-bar button on the editor pane hosting content-mutation actions on the active note. Sibling to View options (`editor-view-options-menu`); the split is clean — View changes pixels, Mutations changes bytes. Icon-only button using the wand glyph (`mutations-menu-icon`). Click opens a popover listing the mutations applicable to the active buffer. [note-mutations-menu]

Mutations are LLM-driven content rewrites of the active note. Single-note user-initiated mutations apply **as buffer edits** — there is no separate review surface, no derived file, no explicit Apply/Reject verbs. Save accepts, Ctrl-Z reverts, the existing dirty-buffer + changes-log machinery handles everything else. The shape is uniform across all current and future mutations:

1. The user clicks a mutation entry. Hiker submits a `Direct`-shape task to `core::tasks` (per `task-queue.md`) at `High` priority — the user is watching. The task carries the buffer's *live* text (not last-saved, same rule as `chat-active-note-context-injection`) so the mutation operates on what the user sees. The buffer is set read-only for the duration of the task, and the source tab is pinned (a preview tab promotes to sticky on submit per `editor-preview-tab-promotion` so a preview-slot swap can't displace the buffer the result needs to land on). [note-mutation-buffer-ro-while-in-flight]
2. The queue's direct-LLM worker drains the task by calling `core::llm::chat` with the mutation's prompt. External MCP-attached clients can also drain the task per the queue's worker rules. The home-page Task queue widget (`task-queue-home-widget`) is the in-flight progress surface — no per-mutation toast.
3. On `TaskCompleted`: the result replaces the source buffer's content as a single editor transaction, the buffer's read-only flag clears, and the buffer becomes dirty. Works whether the source tab is the active one (dispatch through the live editor view) or a background tab (rewrite the tab's saved editor state in place via a transaction off the existing state, preserving history so Ctrl-Z reverts the whole replacement as one undo step on activation). The user reviews by reading the buffer; the dirty-buffer Diff toggle (`editor-diff-vs-disk-toggle`) flips the editor view to a line-level diff against on-disk content for explicit comparison. **Save** writes the mutated content through the regular save path (which appends a `'modified'` history frame). **Ctrl-Z** reverts the mutation as a single undo step. If the user closed the source tab mid-flight (only possible from the explicit close path, since the tab is RO + pinned during the flight), the result is dropped silently — no toast, no held state. [note-mutation-applies-as-buffer-edit]
4. On `TaskFailed`, the buffer's read-only flag clears and a toast surfaces the error. No content change. On `TaskCancelled` (user cancels via the queue widget), the buffer's read-only flag clears, no content change, no toast.

[note-mutations-menu-task-shape]

**Changes-log lineage.** When a mutation lands on the buffer, hiker stashes a `pending_changes_metadata` field carrying `{ mutation: "<kind>" }`. The next save consumes this stash: the resulting `'modified'` row's `metadata` carries `mutation: "<kind>"` so the recent-activity widget and future filters can identify mutation-derived edits. A one-shot stamp — subsequent saves don't carry the tag. [note-mutation-stash-changes-tag]

### v1 mutation: Reformat as markdown

The first concrete mutation: reformat the active note's content as clean markdown. Useful for `.txt` files (per `txt-ingest.md`'s LLM-rewrite option) and for `.md` files whose markup has rotted (uneven heading levels, broken list nesting, inconsistent emphasis). [note-mutation-reformat-as-markdown]

Submits a task with `kind: NoteMutation { mutation: ReformatAsMarkdown, source_path }` and `payload` carrying the buffer's live text + the source extension. The prompt template lives at the user/vault prompt-store path `note_mutation_reformat_as_markdown.md` (per `llm-prompts-file-store`); the bundled default is registered in `core::prompts::bundled_defaults()`.

### Mutations-menu button states

- **Enabled** when the active buffer is an editable note (`mode.kind` is `File`) of an indexable extension (`.md` / `.markdown` / `.txt`) and has at least one byte of content.
- **Disabled** during read-only preview modes (trash / snapshot / staging review) — mutating from inside a review surface would be confusing. Tooltip explains why.
- **Disabled with "Mutation in progress…" tooltip** when there is an active or leased task whose `kind: NoteMutation { source_path }` matches the active buffer's path. The buffer is RO during this window for the same reason. Only one in-flight mutation per source path (`note-mutation-one-in-flight-per-path`).

- **Pending-background-mutation indicator.** When the active buffer has any pending background mutation job (a `NoteMutation`-kind task in non-terminal state whose `source_path` matches), the Mutations menu trigger renders a small pulsing accent-color dot on its icon (same `@keyframes` pulse as `tree-row-queued-marker`). Distinct from the `#mode-controls` "Reformatting…" pill, which names the single in-flight in-buffer mutation; the dot signals presence-of-any-pending and stays lit across multiple queued or batch-flight jobs (`note-mutation-batch-via-staging`). [note-mutations-menu-pending-indicator]

When only one mutation entry is enabled (the v1 case), the popover still opens. As more mutations land, they slot in alphabetically.

### Batch mutations

**Batch mutations** (e.g., "reformat every `.txt` in `inbox/`") fan out N tasks the user can't watch one-by-one, so results route through the **staging surface** (per `settings.md`'s staging review section): the activity detail page's Pending filter with per-row [Accept] [Reject] and [Accept all], plus the editor toolbar pill when the affected file is open.

Batch entry points are deferred to v2; they slot into:

- **A folder-context bulk action** invoked from the file tree (`note-mutation-batch-from-folder`, deferred).
- **A search-result bulk action** alongside the already-reserved `search-bulk-action-tag` / `search-bulk-action-move` (`note-mutation-batch-from-search`, deferred).
- **A CLI command** (`hiker mutate <kind> <glob>`, deferred).

All three converge on the same staging-driven flow; no batch-specific review surface. [note-mutation-batch-via-staging]


## Vault home page

When no note is open, the editor pane shows a vault home page in place of the editor — a lightweight overview of the vault rather than empty space. Default landing surface on vault open (assuming no auto-resume of last-open buffer); reappears when the user closes the active buffer without opening another. [vault-home-screen]

Three widgets, in this vertical order:

- **Vault stats.** Total notes, total chunks, breakdown by index state (indexed / queued / skipped / unsupported), maybe disk usage of the vault directory. Pulled cheaply from the existing index store via a single command. Live-updates via the existing indexer-progress events so the counts reflect ongoing work. [vault-home-stats-widget]
- **Recently modified.** Top N (default 10) notes by filesystem mtime (reuses the `DirEntryDto` mtime field, `tree-sort-options`). Each row shows basename + relative path + relative time. Click → open in editor. [vault-home-recent-modified]
- **Recently accessed.** Top N notes by user-open time; same row shape and click behavior as recently-modified. [vault-home-recent-accessed]

Note access tracking is independent infrastructure (later consumers: search ranking, an "activity" view, etc.) and rides its own slug:

- **Note access tracking.** Add `last_accessed_at INTEGER` to the `notes` row; bump the schema-version constant (same fail-loud + reindex contract as `store-version-fail-loud`). Written when a file becomes the active buffer (tree / search / recents open). Read by the recents widget and future consumers. [note-access-tracking]

Refresh shape: the home page subscribes to indexer-progress events for live stat updates and to watcher file events for recent-modified updates. The recently-accessed list updates on each open without watcher involvement (the writer is hiker itself).

UI scope: minimal. Header with vault root path, three widgets stacked, no charts / graphs, no per-source-type breakdowns yet. A "New note here" button at the top — same call as the sidebar's `sidebar-new-item-button`.

Out of scope for v1 of the home page: pinned/landmark notes, active-trail display, search shortcuts, discovery hints from clustering, recent-searches list, vocabulary stats, sync status. All slot in as additional widgets as their backing features land.

### Recent activity widget (lands with `core::activity`)

A fourth widget appears on the home page once the op log's accepted-op feed (`core::activity`, per `op-log.md` "History materialization") has any rows — i.e. as soon as any save / rename / delete has happened in this vault. Hidden when the feed is empty so a fresh vault doesn't show a confusing zero-count tile. [vault-home-recent-activity-widget]

Preview content (the home tile):

- Header: "Recent activity" + count of recent rows.
- Top 3–5 most recent change events: timestamp, path, op (created / modified / deleted / renamed), author class. Click → detail view (see below).
- Mixed-author by default — user saves and (when MCP lands) agent writes share the stream. Not agent-specific; the agent-activity use case is a filter preset, not a separate surface.

Refresh: subscribes to an op-log change event emitted whenever a history frame is appended. Light debounce (a few hundred ms) so save bursts don't repaint per keystroke.


### Detail views

Vault home widget tiles support a drill-in pattern. **Click on a widget's tile or header → home view body swaps to a detail view for that widget.** No back button affordance within the home view itself — clicking the Home button in the top strip always returns to the home overview, regardless of whether you're in the overview or a detail view. Clicking a note row in any detail view exits home and opens the editor on that note (same shape as `openFile` already exits home view today). [vault-home-detail-views]

Detail views replace the home overview body, not the editor. `#editor-pane` has four states — editor, home overview, home detail, and the settings surface (`settings-pane-mode`). The gear (`vault-bar-settings-icon`) toggles editor ↔ settings; widget-tile clicks go home overview → home detail.

Read-only review surfaces (trash, snapshot, staging review previews) are sub-modes of the editor state, sharing the editor view; the `#mode-controls` slot lights up with mode-specific buttons + label (see `## Mode controls slot`).

Per-widget detail views, in roughly the order they earn their keep:

- **`vault-home-stats-detail`** — each Stats tile (Notes / Indexed / Chunks / Queued / Skipped) drills in to a list view:
    - **Notes** — full list of all notes, paginated, sortable by mtime / access / path.
    - **Indexed** — same shape, filtered to indexed-only.
    - **Chunks** — per-note chunk count, sortable; flags pathologies (>100 chunks, 0 chunks). A surface for spotting chunker pathology, ahead of the deferred `eval-sanity-stats` work.
    - **Queued** — live list of notes currently in the indexer's pending set (`is_pending` per `cmd-file-index-state`). Updates on every indexer-progress event.
    - **Skipped** — list of skipped notes with their reasons (already tracked via `notes.skipped` + `notes.skip_reason`). Per-row "retry" affordance reroutes through `IndexJob::Upsert` with `force=true` so users can manually retry after fixing the underlying issue (file size, encoding).
- **`vault-home-recent-activity-detail`** — full list from `core::activity` (`recent`), all author classes. Mental model: **each row is a saved version of the file.** Row layout: op label · path · author · time-ago, plus a `current` badge on the most recent row per path and a `↩ restored` badge on rows that were themselves a Restore. Filter pills (author class) live in the header. [vault-home-recent-activity-detail]

    The interaction shape:

    - **Click a row** → opens that snapshot read-only in the editor. Reuses the same `readOnlyCompartment` + banner pattern as `tree-trash-preview`; the banner reads `Snapshot of <path> · <when> · <author> · <op>` with `[Restore this version]` and `[Close preview]` actions. Closing returns to the activity detail view.
    - **Per-row `[Restore this version]`** → for power-user single-click without previewing first. Hidden on the `current` row (restoring the current state is a tautology) and on `'deleted'` rows (no content blob to write).
    - **No separate "Open" button.** Click-the-row → snapshot preview is the only path; the live file is reached via the tree, search, or recently-modified.
    - **No separate "Rollback to before this" button.** The row *is* the version (the content blob lives on it); `Restore this version` is the verb — what you click is what you get.

    Restore reads the version's content (`op_writes::content_at_op`) and writes it back via `op_writes::user_save` — a fresh `user` op that becomes the newest accepted version (command `restore_snapshot`). The change-shaped flavor (`rollback_change`) stays available for the agent-rollback consumer per `mcp.md`; both coexist on the same op-log primitives (`op-log.md` "History materialization" → "Rollback").

    - **Filter pills — three independent toggles.** Default-all-on; state persists per-vault. Each toggle gates a distinct row population, so two-of-three off is a meaningful filter (e.g. "show only pending agent reviews"). [vault-home-recent-activity-filter-pills]
        - **Show staging** — pending staging proposals (rows that route to a review surface on click). Off → backend query switches `source` from `Merged` to `ChangesOnly`. Tooltip "Show pending agent reviews."
        - **User** — committed change rows with `author_class == "user"`. Tooltip "Show user activity." [recent-activity-human-icon]
        - **Agent** — committed change rows with `author_class == "agent"` (agent writes already landed on disk — staged-and-accepted or direct-mode; distinct from show-staging, which covers proposals not yet landed). Tooltip "Show agent activity." Future author classes (sync, import) join as additional pills. [recent-activity-agent-icon]
    - **Un-rollback affordance** — append-only log + per-row content blob means *every* prior state stays restorable, including states that were themselves a Restore. "Un-rollback" is just Restore on a more recent prior version — same primitive. UX: rows tagged `metadata.restored_from` show a `↩ restored` badge; immediately after a Restore, the row that *was* the current state for that path gets a soft highlight + "← previous state — click Restore to undo" caption (a hint, not a separate primitive). [vault-home-recent-activity-unrollback]
    - **Snapshot read-only preview.** Reuses the trash-preview machinery: `setReadOnly(true, "snapshot")` swaps in the snapshot banner, suppresses the save button + dirty marker, and the dirty-switch guard treats it like a trash preview (nothing to discard). The buffer carries `snapshotPreview: true` and `snapshotChangeId` so the banner's Restore can write back without a re-lookup. Banner is amber (not trash's red) — informational, not a recovery surface. [snapshot-preview-mode]
- **`vault-home-recents-detail`** (lower priority) — full-list versions of Recently Modified / Recently Accessed; adds filtering / longer history. Each preview row already has click-to-open, so this isn't load-bearing.

The Stats subviews (Notes / Indexed / Chunks / Queued / Skipped) share the one `vault-home-stats-detail` slug, parameterized by which tile launched them — new tiles add parameter values, not slugs. [vault-home-stats-detail]

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
- **Forward button.** Same. [top-strip-forward-button]
- **Home button.** House glyph. Toggles the editor pane to the vault home page — a view toggle, not a buffer close (the active buffer stays in memory; clicking any tree row, recents entry, search result, or tab restores the editor onto it). Tooltip "Vault home." Reserves keybind id `vault.go-home`. [vault-home-button]
- **Queue button.** List-with-pulse glyph. Opens the shared queue detail page (`task-queue.md`'s `queue-detail-shared-page`). A small superimposed indicator shows the `Queued + Leased` count (hidden when zero); the icon pulses when anything is `Leased`. Tooltip "Background work" (or "Background work (N active)"). [vault-bar-queue-button]
- **Settings button.** Gear glyph. Toggles the editor pane to the settings surface (`settings-pane-mode`); same view-toggle behavior as Home. Pressed/unpressed state reflects whether the settings pane is visible. Tooltip "Settings." Keybind `settings.open` — Cmd-, / Ctrl-,. [vault-bar-settings-icon]
- **Open-vault button.** Folder glyph. Triggers the JS dialog → `open_vault_at` flow per `settings.md`'s default-vault-autoopen story. Tooltip "Open vault…". [vault-bar-open-vault-icon]

The vault-path label sits to the right of the icon cluster, before the tab strip — same shape it has today, just relocated. Truncates with ellipsis when space is tight (per `ui-no-sibling-pushout`).

### Tab strip

The tab strip itself — per-group tabs, active/inactive shading, dirty-dot↔close-× swap, overflow scrolling + dropdown, middle-click close, drag-between-groups, the preview/pinned visual states — is `egui_workbench` (`egui-workbench/SPEC.md` §5). Hiker fills it with the tab kinds below and wires these hiker-specific behaviors: [editor-tab-strip]

- **Tab content / disambiguation.** Tab label is the open buffer's basename. When two open buffers share a basename, both render with a folder hint (`notes.md (research/)` vs `notes.md (inbox/)`); tooltip shows the full vault-relative path. [editor-tab-disambiguation]
- **Tab keybinds**, all reserved in `keybind-registry`: `tab.close` = Cmd/Ctrl-W, `tab.next` = Cmd/Ctrl-Tab, `tab.previous` = Cmd/Ctrl-Shift-Tab, `tab.jump-N` = Cmd/Ctrl-1..9 (9 jumps to the last tab). Hiker binds these into the workbench's tab actions. [editor-tab-keybinds]
- **Right-click context menu.** Hiker adds a **Reveal in tree** verb (selects the tab's note in the file tree, expanding parent folders) alongside the workbench's Close / Close others / Close to the right. [editor-tab-context-menu]
- **No `+` button.** New notes use the file tree's `+ New note` affordance.

The active/inactive shading and dirty-marker rendering slugs map onto the workbench tab states. [editor-tab-active-state, editor-tab-dirty-marker, editor-tab-overflow]

### Tab strip behavior with the rest of the app

- **File-tree click on an already-open file** switches to its tab rather than reloading. Click on a not-yet-open file opens a new tab and switches to it. [multi-buffer-tree-click-switches-tab]
- **Search-result, recents, wikilink, and any other "open this note" entry point** behave the same: existing tab → switch; not yet open → new tab.
- **Mode-controls slot, View menu, Mutations menu, chat "active note" injection, navigation history** all operate on the active tab.
- **In-flight-mutation RO** (`note-mutation-buffer-ro-while-in-flight`) applies to the source tab whether or not it's active. Its dirty marker reads as a normal dirty dot; the queue widget / inline indicator is the source-of-truth for background work.

### Linked tabs (drive / follow)

By default every viz tab is self-contained: a graph or canvas tab opens clicked notes into its own preview slot and highlights whichever note is globally active. **Linking** wires a viz tab to another editor group so the two coordinate explicitly, generalizing v1's "Related stays bound to the active editor" (`search-related-stays-bound`) into per-tab source/target wiring. Two independent directions:

- **DRIVE (target).** When a viz tab targets a group, clicking a node opens the note into *that* group instead of the viz tab's own preview slot. A thin sibling of the one `open_file` chokepoint (`open_file_in_group`) points the workbench's focused group at the target before delegating; an already-open note is focused in place. [tab-link-drive]
- **FOLLOW (source).** When a viz tab follows a group, each frame it reads that group's active tab, resolves its note path, and highlights / brings into view the matching node — the graph accents the node, the canvas single-selects and centers the file-node referencing it (deduped so the camera moves only when the followed note changes). Polled per frame off the stable group handle; no event bus. [tab-link-follow]
- **Link control.** Each viz tab's header carries a small **Link** control opening a picker over the current editor groups — a "Follow" list and a "Drive" list, each with a clear option, labelled by the group's active-tab title. The tab's own group is excluded (self-link is a no-op loop). [tab-link-control]
- **Reference + persistence.** A link references a group by the per-window group handle (`GroupId`), which is **not** restart-stable, so v1 links are **in-session only** — they don't ride the autosave tab-state snapshot. A re-resolvable form (persist the linked group's active-tab `persist_key`, re-resolve after layout restore) is the planned follow-up. [tab-link-persist]

The wiring lives entirely in the app layer (a per-tab source/target link referencing tabs or groups, the `open_file_in_group` / `active_tab_in_group` seams, the per-frame follow read). The workbench gains only narrow group accessors; no core involvement, since linking neither mutates the vault nor touches the indexer. Extends to the cluster vector visualization (`cluster-vector-viz`) once that lands. [tab-link-model]

### Multi-buffer model

The editor-group + tab container is `egui_workbench` (§4–§5). Hiker's policy on top of it:

- **In-memory while the vault is open; tab state restores on next open.** The set of open buffers is in-memory state during a session — closes, switches, and dirty content all live in RAM. The autosave layer (`autosave.md`) round-trips a tab-state snapshot (open paths + active path + preview-slot path) to `.hiker/autosave/index.json`, so the next vault open silently reopens the same set of tabs. Per-buffer dirty content recovery rides the same store, prompting via the recovery modal. [multi-buffer-in-memory-only]
- **No max open count / no retention timer.** Tabs stay until the user closes them; a user with 50 tabs gets the workbench's overflow handling.
- **`file-switch-guard-dirty` is close-time only.** Navigating *to* a dirty tab is fine — the dirty buffer stays dirty in memory. The save/discard/cancel modal only fires when the user closes the tab (× / middle-click / Cmd-W). The existing nav-time fire is dropped. [multi-buffer-no-switch-guard]
- **Window close has no dirty-buffer modal.** Quitting flushes every dirty buffer through the autosave pipeline and pushes the open-tab snapshot, then destroys the window — no prompt. Next launch auto-restores the workspace as dirty tabs (`autosave-recovery-auto-restore` + `autosave-tab-state-silent-restore`); the user saves or reverts via the existing affordances. This parks the work — the user's actual files are unchanged on exit. [autosave-close-no-modal]
- **Navigation history stays unified** across all tabs (one stack per vault). Back/forward navigates between content surfaces regardless of which tab they were in; the corresponding tab activates as part of the back/forward action.


### Tab kinds

A tab is a `(kind, payload)` pair. The kind names *what* the tab renders; the payload identifies *which one*.

**Umbrella term: "app pages."** Every non-`buffer` kind below (`home`, `home-detail`, `queue`, `settings`, `properties`, `agent`, `graph`) is collectively an *app page* — a tab that renders an in-app surface rather than user-authored content. The `TabKind` discriminator on the wire stays per-kind (`home`, `queue`, …) — "app page" is umbrella vocabulary, not a runtime category.

- `buffer` — payload is a vault-relative file path plus an optional `DiffSource` (per `diff.md` `diff-source-enum`). Renders the editor widget for that file; when `diff` is set, layers a `DiffLayer` over the same widget — diff is a mode of this tab, not a separate kind. Snapshot review, trash preview, staging-proposal review, dirty-buffer diff, history diff (right-click → Show changes) are all `buffer` tabs with different `DiffSource` selections. All current tab semantics (preview slot, dirty marker, close guard, autosave participation, tree-click activation, search-result-click activation, navigation-history entries) describe this kind.
- `agent` — payload is a chat session id; renders the chat surface as the tab's content (per `chat-panel-expand-to-editor`). The discovery-panel's bottom-docked chat region collapses while an agent tab is open since the surface lives in the tab; closing the agent tab restores the docked region.
- `graph` — payload is the graph view's state (filter set, selection); renders a graph-view canvas (per `design.md`'s graph-view future bullet).
- `home` — vault home overview (per `vault-home-screen`); renders the home page as the tab's content.
- `home-detail` — payload is the detail-view kind (`stats` | `recent-activity` | `recent-modified` | `recent-accessed`); renders the home page's drill-in view.
- `queue` — task queue + indexer detail view (per `task-queue-home-detail-view`).
- `settings` — settings pane (per `settings-pane-mode`).
- `properties` — payload is a vault-relative note path; renders the read-only properties inspector for that note (per `note-properties-tab`). One properties tab per note path; opening Properties on a path that already has a tab open switches to it rather than spawning a duplicate.
- `cluster-review` — payload is a `ClusterReviewState` (purpose `new-tree` | `recluster-subtree` | `rebuild`, plus the in-flight build config and any in-memory structural result). Renders the clustering review surface (`cluster-review-tab` in `cluster-editor.md`) — configure → run → review → confirm. On Confirm it transitions in place to `cluster-batch-review` for the newly-persisted tree.

Tab-strip rendering is kind-aware: a small leading icon distinguishes the kind (per the toolbar icon palette), and the label is whatever the kind chooses (basename for `buffer`, session preview for `agent`, "Graph" / "Home" / "Queue" / "Settings" etc. for app pages).

**App-page tabs default-land in the preview slot.** Clicking the Home / Queue / Settings buttons opens the corresponding tab as a *preview*, replacing whatever preview was there (same one-preview-at-a-time rule as `editor-preview-tab`). Promotion to sticky uses the same affordances as buffer previews (right-click "Keep open", or a tab-body interaction signalling "I'm staying" — per-kind: home-detail clicks within the page promote; settings flips do not).

**Buffer-scoped chrome hides when the active tab is non-buffer.** The editor toolbar's buffer-scoped controls (View menu, Save button, Diff button, Mutations menu, the mode-controls slot) and the bottom status bar (line:col, index-state label, file-path) are buffer-only — they hide entirely when the active tab is `agent`, `graph`, `home`, `home-detail`, `queue`, or `settings`. The sidebar / discovery toggle icons stay visible regardless because they control the side panels independently of the center pane. Each non-buffer kind brings its own chrome (or none) inside the tab body — settings has its scope toggle and refresh button in its own header, home has its overview/detail toggle, etc.

**Kind-aware predicates.** Existing tab semantics that assume "every tab is a file buffer" gate on kind:

- **Preview slot** (`editor-preview-tab`) — buffer-only on the *contents-tracking* side (paths replace each other in the slot). App-page tabs use the same one-slot-per-strip rule; opening an app-page tab evicts whatever was previewed before (buffer or app page).
- **Dirty marker** (`editor-tab-dirty-marker`) is `buffer`-only — non-buffer tabs have no dirty concept.
- **Close guard** (`file-switch-guard-dirty`) only fires when closing a `buffer` tab whose buffer is dirty.
- **Autosave tab-state** (`autosave-tab-state-store`) records `(kind, payload)` per open tab; restore reopens each kind through its own mount path.
- **Reveal in tree** (`editor-tab-context-menu`) only applies to `buffer` tabs.

[tab-kinds]


### Note properties tab

Right-click → Properties on a tree row opens a `properties`-kind tab for that note — a read-only inspector of every piece of state hiker tracks for the note across `index.db` and the op log ("what does hiker actually know about this file"). Useful for debugging skip reasons, embedder-version drift, the change log, and trail / cluster membership. Frontmatter editing is **not** part of this tab (`tree-context-properties-frontmatter-editing` is a separate future surface).

- **One properties tab per note path.** Opening Properties on a path that already has one switches to it rather than duplicating — same shape as the file-tree click rule for buffer tabs. [note-properties-tab]
- **Read-only data view, no editor chrome.** Non-buffer per `tab-kinds`, so the editor toolbar and status bar hide on activation. The tab body owns its own header (basename + relative path). No save button, no dirty marker. [note-properties-tab-no-editor-chrome]
- **Preview-slot rule applies on open.** Default-lands in the preview slot like `home` / `queue` / `settings` (per `tab-kinds`); a second Properties open replaces the preview, with standard promotion paths. [note-properties-tab-preview-slot]
- **Live-refreshing.** Subscribes to indexer-progress events (notes-row / chunks), op-log append events (changes section), and watcher file events (mtime / size). No manual refresh button. [note-properties-tab-live-refresh]

#### Sections rendered

Each section is a labeled block stacked vertically; sections render in order regardless of whether they have content (a missing row shows an empty-state line). [note-properties-tab-content]

- **Identity.** Path, note ULID, and the `path_ids` row id. Calls out a `notes.id` ↔ `path_ids[path]` mismatch if one exists (shouldn't, but the user should see it).
- **File state.** mtime, size, `content_hash` (full blake3 hex, copyable), extension, and whether the path is open in the buffer set / another tab.
- **Index state.** `indexed_at`, `embedder_version`, `skipped` flag + `skip_reason`, and the runtime classification (`Indexed` / `Skipped` / `Queued` / `Unsupported`) — same surface that drives the tree row markers and `status-bar-active-file-index-state`.
- **Chunks.** Total count plus a compact per-chunk list (index, byte range, `heading_path`, ~80-char snippet). Long lists virtualize; debugging aid, not a search UI.
- **Access tracking.** `last_accessed_at` (per `note-access-tracking`), relative time with absolute on hover.
- **Changes.** Total `changes` rows for this path, breakdown by `author_class`, and the most recent N rows (timestamp, op, author, metadata summary). Each row click opens the change-row detail in `snapshot-preview-mode`, sharing the recent-activity detail code path (`vault-home-recent-activity-detail`).
- **Trail / cluster membership.** Trails containing this note (via `core::trails::trails_containing_note_with_paths`) and clusters it belongs to (placeholder when no clustering data).

#### Behavior details

- **Open paths.** Right-click → Properties (`tree-context-properties`) is the canonical entry; a `Show properties` verb in the buffer tab context menu (`editor-tab-context-menu`) and a future buffer-body entry are the others. Programmatic `openProperties(rel)` skips the preview slot per the directed-action rule.
- **Path doesn't resolve.** If the path no longer exists on disk when the tab activates (deleted / moved externally), the tab renders a "Note not found at `<path>`" empty state but still shows whatever the index and changes db know — exactly the case the inspector exists to surface.
- **Trash entries.** Right-clicking a trash row → Properties opens the same tab kind for the trashed note (trash-relative path). "Index state" shows `Skipped`; "Changes" shows the row recorded at delete time. [note-properties-tab-trash]
- **Autosave tab-state.** Properties tabs participate in `autosave-tab-state-store` like every kind — open at quit, reopens at the same path on next launch.
- **Reveal in tree.** Tab right-click → Reveal in tree highlights the note in the file tree.
- **No write affordances in v1.** Frontmatter editing, force-reindex, change-row restore are follow-up candidates; v1 is strictly read-only. Force-reindex is the likely first write addition (`note-properties-force-reindex`).

#### Out of scope (deferred)

- **In-place frontmatter editing.** Tracked under `tree-context-properties-frontmatter-editing`.
- **Force-reindex this note.** A button submitting a single-note `IndexJob::Reindex`. [note-properties-force-reindex]
- **Restore-from-this-row inline.** Redundant — the changes section already opens each row in `snapshot-preview-mode`, which carries Restore.
- **Properties for non-note paths** (folders, non-`.md`/`.txt` trash entries). Folder properties are a different surface (recursive note count, total bytes). [note-properties-tab-folder-deferred]
- **Comparison view across two notes.**


### Preview tabs

The preview-tab mechanic — at most one preview slot, italic title, replace-in-place on the next preview-open, promote-to-sticky on edit / double-click / drag / "Keep open" — is `egui_workbench` (`egui-workbench/SPEC.md` §5.3). Hiker wires which callsites open preview and how directed actions opt out:

- **Every click-driven open-note callsite uses the preview slot by default.** File-tree click, search-result click, related-notes click, recents click, wikilink click, chat note-link click, `@`-mention click — all route through `openFile(rel, { preview: true })`. Uniform on purpose: "click is preview, Mod-click is sticky." [editor-preview-tab-from-open-callsites]
- **Mod-click on any open-note callsite forces a sticky tab.** Skips the preview slot, opens directly into a new sticky tab. Drag-from-tree (when that's a thing) is also implicitly sticky. [editor-preview-tab-mod-click-sticky]
- **Programmatic opens skip preview.** Restore-from-trash, new-note creation, the right-click "Open" tree verb, mutation-apply, and any other non-user-click path open sticky — these are directed actions, not browsing. `openFile` is `{ preview: false }` (or omitted) at those callsites.
- **Edit-as-promotion keeps preview tabs never dirty** — the moment the user types, the tab is sticky, so the dirty-buffer machinery (`file-switch-guard-dirty`, `autosave-close-no-modal`) never has to know about preview tabs. [editor-preview-tab, editor-preview-tab-promotion]
- **Tree double-click stays bound to inline rename** per `tree-double-click-rename` — promotion via double-click on a *tree row* would conflict; tab double-click covers the canonical promote gesture.
- **Pending agent proposals route the open into review mode.** When `openFile(rel)` resolves a path with one or more pending staging proposals, the buffer lands in patch-review or write-note review per `note-open-routes-to-pending-review` (in `patch-review.md`). The preview-vs-sticky distinction is preserved; the review state rides on `buffer.mode`, not the tab kind.


## Navigation (back / forward)

Browser-style back/forward navigation across editor-pane states. Each user-initiated transition between distinct content surfaces (see `### What pushes onto the stack`) pushes onto a per-vault history stack. Back and forward navigate that stack via the top strip's leading-cluster buttons, trackpad two-finger horizontal swipe, mouse side buttons, and keybinds.

- **History is a per-vault in-memory stack of editor-pane content states.** Cleared on vault swap; not persisted across restarts. [navigation-history-stack]
- **Back and forward buttons live in the top strip's leading cluster** (leftmost, before Home / Queue / Settings / Open). Icon-only, disabled when no history exists in that direction. [top-strip-back-button, top-strip-forward-button]
- **Trackpad swipe** — see `### Trackpad swipe shape` below. [navigation-trackpad-swipe]
- **Keybind registry entries** reserve `navigation.back` and `navigation.forward`: Cmd/Ctrl-[ back, Cmd/Ctrl-] forward; Alt-Left/Right as additional bindings on Linux/Windows. [navigation-keybind]
- **Mouse side buttons** (mouse-button-3 back / mouse-button-4 forward) trigger back/forward by default. Detection via window-level `mousedown` / `auxclick` reading `event.button`, calling the same `navigation.back` / `navigation.forward` handlers as the keybind and swipe paths. Default-on; rebinding deferred until the registry grows mouse-button support (keyboard-chord-only today). [navigation-mouse-buttons]
- **Dirty-buffer protection** is moot for back/forward — navigating activates a different tab without closing the prior one, so the dirty buffer stays dirty in memory (per `multi-buffer-no-switch-guard`, `autosave-close-no-modal`).


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

Edge cases worth pinning:

- **Inside the editor.** Horizontal-scroll deltas reach the pane's swipe handler when content isn't horizontally scrollable. When a line *is* horizontally scrolled (code blocks), the swipe still triggers once the horizontal delta substantially exceeds the line's scrollable extent.
- **Inside scrollable detail-view lists.** Same shape — the list scrolls on `deltaY`, so horizontal swipes pass through.
- **Touchscreen devices.** Trackpad-only for v1; touch swipe gestures are a separate slug if a touchscreen variant ever ships.

Closing the vault while history exists drops the entire stack — no warning, no save protection beyond what already gates vault swap.

Out of scope: persisting history across restarts (per-session), and a rich history menu (right-click → list of last N pages, deferred).


## Fonts

Three configurable font slots, all per-vault settings (per `settings-section-editor`). One default per slot; the user picks any installed system font. [editor-three-fonts]

| Slot | Setting key | Default | Used by |
| ---- | ----------- | ------- | ------- |
| System | `editor.system_font` | platform UI default | every non-editor chrome surface (toolbars, menus, sidebar, status bar, tabs) |
| Editor | `editor.editor_font` | platform default proportional | the editor canvas's prose body — plain paragraphs, headings (size still set by the heading style), list items |
| Code | `editor.code_font` | platform default monospace | fenced code blocks **and** frontmatter blocks (per `editor-frontmatter-rendering-fix` in `live-preview.md`); inline code; the diff layer's code-shaped hunks |

- **Selection.** Settings pane row per slot — a font-picker dropdown enumerating installed fonts via the OS font enumeration. Strict-load rejects a name that doesn't resolve at startup; the pane shows the picked-but-missing name in red with a note ("Font not installed").
- **Scope.** Vault-scope by default (matches the rest of `[editor]`). User-scope works via the section's scope toggle for users who want "code font everywhere I open."
- **Live-applied.** Flipping any of the three reflows the affected surfaces on the next frame — no relaunch.
- **No size knob in v1.** Sizing rides the existing theme tokens; a per-slot size knob lands when there's a real ask.

The code font's secondary job — being the frontmatter font — is what gets frontmatter rendering off heading-size and back to body-size; the rule itself lives in `live-preview.md`.


## Editor layer order

The decoration / extension ordering that sets precedence for overlapping ranges is `egui_editor`'s extension surface (`editor/SPEC.md` §9, §13) — the view aggregates decoration providers (pure `&Editor → Set` functions) onto `ViewState` in apply order. Hiker registers the providers it owns onto that pipeline: chunk-boundary decorations (`view-show-chunk-boundaries`), the frontmatter-fold block (`view-hide-frontmatter-toggle`), the trailing-whitespace decoration gate (`view-highlight-trailing-whitespace-toggle`), and the find-match decorations — layered after the widget's built-in gutters / markdown styling and before the theme. Markdown language selection is per-buffer, so opening a non-markdown sidecar swaps styling without rebuilding the editor state. [editor-layer-order]

The editor state is created once at startup and reused across buffer switches; switching files dispatches a doc-replacement transaction, never reconstructs the view. [editor-instance-reuse]


## Out of scope (deferred)

- Live-preview decorations (syntax-marker hiding on cursor-out) — specced in `live-preview.md`
- Wikilink rendering and autocomplete
- Widget-based rendering (LaTeX math, Mermaid, tables, images) — specced in `editor-widgets.md`
- Multi-buffer / tabs / split panes
- Vim/Emacs keymaps
- User keybind overrides (the registry supports it; the loader is later)
- External-change watcher integration (v1)
