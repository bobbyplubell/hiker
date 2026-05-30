# Editor

Hiker embeds `egui_editor` — the embeddable text-editor **widget** (`editor/SPEC.md`) — as its `buffer` tab kind inside `egui_workbench` — the IDE **shell** (`egui-workbench/SPEC.md`). This doc covers hiker's *integration* of those two crates: the wiring, policy, and hiker-specific surfaces around them — not the generic editing / shell behavior the crate specs own.

Where this doc points elsewhere:

- The widget's editing model — multi-cursor, selection, decorations, markdown live preview, diff view, find+replace, IME, minimap, the view toggles — is `egui_editor` (`editor/SPEC.md`).
- The shell's chrome — activity bar, side bars, editor groups + splits, tab mechanics, panel area, status-bar chrome, layout persistence — is `egui_workbench` (`egui-workbench/SPEC.md`).
- Hiker's file tree / Files activity content is `files.md`; cluster trees are `cluster-editor.md`.

Transactions, decorations, and selections referenced below are the widget's types.


## Buffer model

One open buffer at a time in v0. Switching files replaces the buffer's contents via a single `dispatch` that swaps the entire doc.

The buffer renders the merged working materialization of the document's op layers — `materialize(accepted + working + pending(session))` per `op-log-layered-model`. User typing doesn't write disk or the `accepted` layer directly; the editor binding (per `op-log-editor-binding`) turns each edit into a `user` op on the `working` layer, and Save commits that layer. An agent's `pending` proposals coexist in the same buffer, reviewed via `patch-review.md`.

State tracked per buffer:

- `path` — vault-relative; null when no file is open [buffer-path-tracking]
- `loadedHash` — hash (or full string) of the contents most recently read from / written to disk
- `isDirty` — derived: current doc text !== loadedHash

`isDirty` is the single source of truth for save state — computed lazily from the editor doc and `loadedHash`, no separate flag that can desync. Cleared by re-reads and successful writes; set implicitly by any edit. [buffer-dirty-derived]

Multi-buffer / tabs deferred. When tabs land, the same per-buffer state moves into a `Buffer[]` keyed by path, with the active buffer driving the editor view.

### `transactions_out` seam

The editor widget exposes the change sets it applied from user input — the per-edit list of retain / delete / insert ops over byte ranges, the forward half of the editor binding (per `op-log-editor-binding`). The host drains this stream each frame and mirrors each change set into the document's CRDT `working` layer as `user` ops, so the editor stays the source of *what changed* rather than the host re-diffing the whole buffer to guess. Reverse-direction edits (an accepted/pending/external change applied back into the editor) carry a sync origin and are not re-emitted on `transactions_out`, so the binding can't echo. The seam is editor-crate-owned and host-agnostic — the same exposed transactions feed any consumer that needs a precise edit log, not just the op log. [editor-transactions-out]


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
- The save button itself doesn't carry a dirty marker — its enabled/disabled state is the signal, and the tab + tree dots already cover the redundant case.

File-switch guard fires on **explicit close** of a dirty tab (× / middle-click / `tab.close` keybind) — a confirm dialog with three options: Save & close, Discard & close, Cancel. Cancel keeps the tab open. Switching away from a dirty tab via tab click / file-tree click / search-result click does *not* fire the guard — the buffer stays dirty in memory. Window close has **no** dirty-buffer modal: every dirty buffer flushes through the autosave pipeline and the open-tab snapshot is pushed, so next launch auto-restores the workspace as dirty tabs the user can save or revert via the existing affordances. [file-switch-guard-dirty, autosave-close-no-modal]

External changes: a file edited on disk outside hiker reconciles into the `accepted` layer as an `external` op (per `op-log-external-edit-sync`). Because the buffer materializes `accepted + working`, an external change and the user's uncommitted `working` edits merge by position: disjoint regions auto-merge with no prompt, and an overlapping region surfaces as a conflict hunk with **Keep mine / Keep theirs / Keep both** — the same model agent proposals use (per `op-log-merge-auto`, `op-log-merge-conflict`). Two mechanisms feed the reconciliation.

- Pre-write drift check (v0). Every save re-reads the file and compares its hash to `loadedHash` before writing. [pre-write-drift-check, drift-conflict-modal]
    - match — write proceeds; `loadedHash` updates.
    - file missing — prompt: write anyway (re-creates) / cancel.
    - hash mismatch — reconcile the disk delta into `accepted`; disjoint changes merge silently, an overlapping region opens conflict hunks. (A buffer with no `working` edits just reloads — there is nothing to conflict with.)
    - Catches the "I edited the file in vim while it was open in Hiker" case without a watcher.

- Watcher integration (v1). The notify-based watcher (lands with the indexer) pushes file-change events for the open file.
    - buffer clean — silently reload; `loadedHash` updates.
    - buffer with `working` edits — same reconciliation, but proactive (on event, not at save time).
    - Reduces the stale-buffer window; pre-write check remains as final guard since watchers miss events (network filesystems, rapid changes, event/save races).

Both mechanisms feed the same conflict-hunk reconciliation; only the trigger differs.


## Keybind registry

Window-level chords: `app/src/keybinds.rs`, intercepted via `ctx.input_mut(|i| i.consume_key(...))` before the editor sees them. Buffer-local chords: `editor-view::command::handle`. `known_keybindings()` returns the flat window-level list the F1 overlay enumerates. Goals: discoverable, overridable (later), conflict-detectable. [keybind-registry]

Shape:

Shape: window-level chords are a static `(chord, label)` table returned by `Keybinds::known_keybindings()` — e.g. `("Mod-S", "Save the active buffer")`, `("Ctrl-K", "Open the command palette")`. `Keybinds::handle_keybinds(ctx)` matches each chord by consuming the key combo from egui input (`ctx.input_mut(|i| i.consume_key(...))`) and runs the corresponding action.

Compilation: there's no separate keymap object — `handle_keybinds` runs once per frame before the editor widget, so a consumed window-level chord never reaches the buffer.

Validation: a startup-time test (`known_keybindings_has_no_duplicates`) rejects duplicate chords. No silent overrides.

Scope: two scopes today. Window-level chords (`app/src/keybinds.rs`) fire regardless of focus and are consumed before the editor sees them; buffer-local chords (`editor-view::command::handle`) only fire when the editor has focus. A future `scope` field could refine this further; until then the split between the two registries *is* the scope.

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
2. **Snapshots** — every `core::changes` row for this path within retention (`Changes::history_for_path`), one entry per row. Label: `Snapshot · <relative-time> · <author> · <op>`. Selecting an entry enters `snapshot-preview-mode` against that change-id (same code path as clicking a row on the activity detail page). The current on-disk state already appears as the top "Current" entry, so the most-recent snapshot row is *not* hidden — it represents the saved version, which may diverge from the live buffer if the user has unsaved edits.
3. **Staging proposals** — every `core::staging` proposal whose `target_path` equals this file (`Staging::list({by_path})`). Label: `Staging · <surface> · <relative-time>`. Selecting an entry opens the proposal's content as a read-only staging preview (same code path as clicking a proposal row on the activity detail page).

The selected entry reflects what's currently in view. Closed-state label mirrors that selection (e.g. `note.md — Snapshot 2m ago · agent:claude`), so the user can tell at a glance which version the editor is showing without opening the dropdown. Mode-specific verbs (Restore, Accept / Reject, Diff toggle) stay in the editor toolbar's `#mode-controls` slot per `editor-toolbar-mode-controls`; the dropdown is purely a version selector. [status-bar-version-dropdown-selection]

The dropdown is buffer-only — it hides for non-buffer tab kinds the same way the rest of the status bar does (`tab-kinds`). For a buffer whose file does not yet exist on disk (newly-created, never saved), only the "Current" entry appears.

Population:

- Snapshots and staging entries come from `core::activity::list_for_path(path, filter)` (see `op-log.md` "Unified activity feed") so the dropdown shares the merged-feed type with the activity detail page rather than calling two separate APIs and reconciling them in the UI. [status-bar-version-dropdown-uses-unified-feed]
- The list refreshes on op-log append events and staging-snapshot updates for events that touch the active buffer's path; debounced consistent with the activity widget. [status-bar-version-dropdown-live-refresh]

The dropdown is the canonical place to ask "what other versions of this file exist?" without leaving the editor. [status-bar-version-dropdown]

Trash entries are out of scope for the dropdown — a trash entry *is* a different file on disk (different path), not a version of the open buffer; surfaced via the existing `tree-trash-preview` path.


### Sibling protection (overflow rule)

Every status-bar region — and any other horizontal toolbar / strip elsewhere in the app — truncates user-derived content with an ellipsis so a long string in one region can't push its siblings off-screen. The basename + tooltip change above fixes the common case for the path region; the rule generalizes to any region whose content is user-derived (file names, error messages, status labels reflecting external state). Tracked as `ui-no-sibling-pushout` so the rule has a slug to cite in code review. [ui-no-sibling-pushout]


## Layout

The four-region workbench shell — activity bar, side bars (as accordion sections), editor groups + splits + resizable splitters, panel area, status-bar chrome, and layout persistence — is `egui_workbench` (`egui-workbench/SPEC.md` §1–§8). This section covers only hiker's wiring of that shell: which activities mount, what fills the editor toolbar, and the hiker-specific panels.

Hiker's region map:

- **Top strip**: a single horizontal strip across the full width of the window — leading cluster of icon buttons (Back / Forward / Home / Queue / Settings / Open vault) plus the active vault path label, then the tab strip filling the rest. Hiker-specific buttons + behavior are in `## Top strip` below. [top-strip-layout]
- **Left (primary side bar)**: hosts the Files / Cluster-trees / Trails activities. The file tree is `files.md`; cluster trees are `cluster-editor.md`. The side-bar / accordion mechanics (sections, headers, collapse, resize, drag-to-add, persistence) are `egui_workbench`. The sidebar collapse toggle is `sidebar-toggle-icon` below. [four-region-layout]
- **Center (editor area)**: the editor pane — a thin toolbar strip across its top, the `egui_editor` widget below, the status bar beneath. Toolbar contents are hiker-specific; see the editor-toolbar wiring below.
- **Right (discovery panel)**: related-notes panel. Renders `RelatedHit[]` from `related_notes(currentPath)`, updated on file-open and on save (debounced 500ms per `index.md`). [related-notes-panel-ui]

### Editor-toolbar wiring

The editor pane's toolbar (hiker chrome, not the widget's) holds, left-to-right: the sidebar toggle, Save (floppy icon), the dirty-buffer Diff toggle (`editor-diff-vs-disk-toggle`), a centered `#mode-controls` slot between two flex spacers (see `## Mode controls slot`), then the View menu button (eye icon, `## View options menu`), the Mutations menu (wand, `## Note-mutations menu`), and the discovery-panel toggle. The two panel-toggle buttons are always visible; their pressed/unpressed state reflects whether the corresponding side panel is open. [panel-toggle-buttons]

- **Sidebar toggle icon.** A safe-dial / ship-wheel glyph (round with spokes) inside a rounded-square frame — riffs on the project's "vault" vocabulary. Distinct enough from generic file-tree icons that it doesn't read as just-another-folder. Tooltip "Toggle sidebar." [sidebar-toggle-icon]
- **Discovery toggle icon.** A magnifying glass — the panel's primary surface is search-driven retrieval (per `search.md`). Tooltip "Toggle discovery panel." Naming aside (the panel hosts search results *and* related-notes *and* future surfaces), the magnifying glass is the most recognizable retrieval glyph users have. [discovery-toggle-icon]

Default state on first launch: tree open, related panel collapsed. Persistence of these toggles rides the workbench layout persistence (per-vault); side-column resize, min/max clamps, and the resize handles are `egui_workbench` mechanics. [side-panel-resize]


## Mode controls slot

The editor toolbar reserves a centered `#mode-controls` slot between two flex spacers. The slot is empty during normal editing; entering a read-only preview mode populates it with mode-specific icon-only buttons plus a short text label naming the mode. One slot, one render path, per-mode populators. [editor-toolbar-mode-controls]

What lands in the slot:

- **Icon-only action buttons** for the mode's verbs: Diff toggle (see `editor-diff-vs-disk-toggle` below), Restore, Apply, Reject, Close — whichever the active mode exposes. Icons match the toolbar palette; pressed/unpressed states reflect toggle state for stateful icons. The mode qualifier that names which non-current version is in view sits in the status-bar left region's version dropdown closed-state label (see `status-bar-version-dropdown` above) so the toolbar stays compact and the user's eye finds the context in the same place it finds the file name.

The mode-controls render path reads the current buffer state (mode kind, dirty flag, etc.) and the diff-active flag and rebuilds the slot. Called on every transition that affects the slot — buffer swap, mode entry/exit, dirty toggling, diff on/off.

Per-mode populators live in the egui toolbar / buffer-panel code — snapshot, trash, dirty-buffer, and (future) staging variants. Each renders label + icons; none mutates state directly. State changes go through the existing buffer/preview API.

### Dirty-buffer Diff toggle

A diff toggle lives in the editor toolbar (just right of Save). Greyed when the buffer is clean *and* no other diff source is selected (nothing to diff against). Click toggles the editor tab's `diff` mode against the current `DiffSource` (see `diff.md` `diff-as-mode` and `diff-source-enum`); the default source is `Disk(path)` — the live buffer vs. last-loaded content. The flip is non-destructive: the buffer's `current` is unchanged, decorations are layered on top; toggling off restores cursor + selection. **Right-click opens a source picker** — a small context menu offering: `Diff against on-disk`, `Show changes…` (submenu of recent op-log rows for this path), and future sources (snapshot, another open buffer). Selecting a source switches the tab's `DiffSource` and turns diff mode on. [editor-diff-vs-disk-toggle, editor-show-changes-menu]

Constraints:

- **Disabled when there's nothing to diff.** Buffer clean *and* `DiffSource` is `Disk(path)` *and* the path exists on disk → toggle is disabled with tooltip "No changes to show."
- **Newly-created buffer (file not on disk yet).** Toggle is disabled with tooltip "Save first to diff against disk." The source picker still works for non-disk sources (e.g. another open buffer).

### Show changes menu

The right-click context menu on the diff toggle (and the buffer's body, when no selection is active) carries a `Show changes…` entry whose submenu lists recent op-log rows for the active buffer's path (via `core::changes`), newest first. Selecting a row sets the tab's `DiffSource = ChangeRow(op_id)` and turns diff mode on; the buffer's `current` text stays put, and `agent_base` (if any) is unaffected. [editor-show-changes-menu]

- **Submenu shape.** Up to 20 recent rows. Each row shows timestamp (relative + absolute on hover), op kind (`created` / `modified` / `deleted` / `renamed`), and author. Final row: `Browse all… → ` opens the `home-detail { which: activity-row { path } }` tab filtered to this path (per `vault-home-recent-activity-detail`).
- **Per-hunk restore.** When the diff source is `ChangeRow(op_id)`, hunks carry a `Restore this hunk` overlay verb (owner `Snapshot` per `diff.md`'s `diff-layer-owner`). Restore writes the historical text for that hunk's range into `current` and lets the user save through the normal path. Full-snapshot restore stays on the row-level surface (`vault-home-recent-activity-detail`), unchanged.
- **No URI scheme.** The diff resolves directly through `core::changes::materialization_at(op_id)`; the editor crate doesn't go through a custom URI provider.


## Find in note

In-buffer find / replace is the `egui_editor` search panel (`editor/SPEC.md` §6, §9.13) — triggered by Mod-F, with case / whole-word / regex / in-selection toggles, match highlights, and gutter + minimap match ticks. Hiker enables that panel on the buffer tab kind; it doesn't re-implement it. [editor-find-in-note]

Hiker boundary: in-buffer find is "jump to this string in *this* file." Cross-file find is the discovery panel's job (per `search.md`); the in-buffer bar must not grow into a second search surface.


## Reader / focus view

A single keybind hides every chrome element and renders only the live-preview markdown of the active editor — top strip, sidebar, discovery panel, tab strip, editor toolbar, gutter, minimap, status bar all gone. Aimed at distraction-free reading and the long-form writing case. Per-session, not persisted. [editor-reader-view]

- **Trigger.** Keybind `editor.reader-view` (reserved in `keybind-registry`); also accessible from the View menu (per `editor-view-options-menu`) as a regular toggle row.
- **Exit.** The same keybind, the same View-menu toggle, or Esc when reader view has focus.
- **What's hidden.** Top strip + tab strip + sidebar + discovery panel + editor toolbar + status bar + gutter + minimap. Only the editor canvas is visible.
- **What renders.** The same live-preview markdown (per `live-preview.md`) the editor already draws — heading styling, fade-on-cursor-out, fenced code blocks, etc. Live preview's existing reveal rules still apply (cursor-on-line reveals markers), so typing in reader view is fine; the user can read or write without switching modes.
- **Scope.** Per-active-buffer view state, not a window-level mode. Switching tabs takes the new tab's buffer through the same reader view. Non-buffer tab kinds (per `tab-kinds`) ignore the toggle — reader view has nothing meaningful to do for the queue / settings / home pages.
- **State.** In-memory per session — not persisted to vault config in v1 (matches the rest of the view-options menu's persistence rule).
- **Implementation.** A boolean in `app::state` that the editor-pane layout reads; when set, the pane renders with all chrome regions hidden and the buffer centered. No new decoration layer — reader view is layout-only.


## Command palette

Fuzzy-search popover over every registered keybind action. The discoverability surface for the keybind registry (`keybind-registry`) so users don't have to memorize chords. Triggered by Mod-Shift-P and (when wired) by a top-strip icon. [command-palette]

- **Trigger.** Keybind `vault.commandPalette` = Mod-Shift-P. Already reserved in `keybind-registry`'s "Reserved IDs" table; this spec lights it up.
- **Surface.** A centered overlay popover above the editor pane: a text input at the top, a scrollable result list below, footer hint listing accept / dismiss bindings.
- **Action source.** The keybind registry is the source of truth — every entry in `Keybinds::known_keybindings()` is a palette row. Adding a registry entry adds a palette row for free.
- **Row shape.** Action title (the registry's human label), source area as a small badge ("editor" / "tab" / "navigation" / "vault" / etc.) inferred from the action's id prefix, and the bound chord on the right (or `Unbound` when no chord is set). Greyed rows when the action isn't currently dispatchable (e.g. `editor.save` when no buffer is open).
- **Ranking.** Fuzzy match on the human label first, then on the action id. Recent invocations float up via a small per-session MRU list — same shape as the chat `@`-autocomplete recency tiebreaker, in-memory only.
- **Invocation.** Enter (or click) fires the action through the same dispatch path the keybind handler uses — palette is a discovery surface, not a parallel runtime. Esc dismisses.
- **No payload prompting in v1.** Actions that take arguments (a future "Open file by name" action) aren't in the palette until their entry-point becomes a side-effect-free invocation; palette rows are zero-argument verbs. Picker-driven actions (open vault, open recent) plug in by registering a no-arg "open the picker" verb, not by spawning their UI from the palette.
- **AI-touching actions are hidden under `[llm] enabled = false`** (per `llm-features-disable-entirely`). The filter runs at render time so a flip applies live.
- **Module placement.** Popover lives in `app/src/panels/command_palette.rs`; the action list it reads is the existing `Keybinds::known_keybindings()` plus per-action metadata (label, area badge, dispatchable predicate).

Replaces no other surface; coexists with right-click context menus and the View menu. The palette is the keyboard-first answer; menus stay the mouse-first answer.


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

Each entry is a checkable item — checkmark when active, click flips it, menu closes on click. State is in-memory only for v1. Persistence (per-vault, per-user, or both) is a `settings.md` concern when that surface lands; users will expect toggle state to survive a relaunch, so this menu is one of the first hooks the settings work picks up.

### v1 entries

- **Show chunk boundaries** — overlays the editor with a thin horizontal rule between chunks (pale reddish-orange — visible against prose without competing for attention) and the chunk index (`0`, `1`, `2`, ...) in the gutter at each chunk's start line. Backed by `cmd-chunks-for-path` (see `index.md`) which returns the active note's chunk bounds. Refreshes on save (debounced 500ms, same cadence as the related-notes panel). When the file isn't indexed (unsupported / skipped / queued per `cmd-file-index-state`), toggling on shows nothing and a faint hint in the gutter explains why. Editor integration: a decoration provider (`chunk_boundary_decorations` in `app/src/panels/buffer/decorations.rs`) emitting the rule + gutter index, aggregated onto the buffer view's decoration set. [view-show-chunk-boundaries]

  This is genuinely a debugging-grade view of the chunker's output — useful while txt-ingest is hardening, and useful long after as a sanity check when chunker behavior changes.

- **Hide frontmatter** — visually collapse the leading `---\n…\n---\n` YAML block into a single placeholder line (`▸ frontmatter (N lines)`) without touching the file. Detection mirrors `core::frontmatter::split` exactly — the block must start at byte 0 with `---\n` and have a closing `---\n` line before any body content; an unterminated or non-leading block is ignored. Editor integration: a block replace decoration (`frontmatter_fold` in `editor/editor-md/src/meta.rs`) over the byte range, recomputed off the document so edits inside or around the block update the placeholder line count immediately. Default off; persistence via `editor.hide_frontmatter` (`settings-section-editor`). Motivated by agent-stamped frontmatter (`mcp-tool-set-frontmatter`, `mcp-tool-apply-tag-remove-tag`) accumulating into a tall block that pushes the actual prose off screen — flipping this on lets the user read the body without manually scrolling past metadata that's already visible elsewhere (the activity widget, file detail views). [view-hide-frontmatter-toggle]

- **Intraline diff highlights** — augments the line-level red/green diff with character-level highlights inside paired delete/insert lines. Affects every consumer that calls `editor.renderDiff` (snapshot preview, dirty-buffer diff, write-note review). Default off; persistence via `editor.intraline_diff` (`settings-section-editor`). Flipping the toggle while a diff is currently displayed re-renders the active diff with the new style. Does *not* affect the patch-review agent-diff surface — that renders span-anchored hunks as widgets on the live doc and is governed by its own rules in `patch-review.md`. See `diff.md`'s "Diff style" section for the full rendering contract. [view-intraline-diff-toggle]

### Reserved entries (greyed in v1, enabled when their backing feature lands)

These appear in the menu now so the surface is predictable, but render greyed-out with a tooltip naming the dependency. Putting the slot up front is also a forcing function for designing each backing feature with the toggle in mind.

- **Live preview** — hide/show markdown syntax markers on cursor-out. Specced in `live-preview.md`; entry becomes live (default on) when that ships. [view-live-preview-toggle]
- **Render .txt as markdown** — session-scope override of `txt-render-as-markdown-default`. Greyed until `settings-vault-config-toml` lands and gives the per-vault default a real loader; see `txt-ingest.md`. Different scope from the per-note override that doc explicitly rejects — this one is "for the current app session, flip the vault default," no file mutation, no persistence in v1. [view-render-txt-as-markdown-toggle]
- **`egui_editor` feature toggles** — these rows are session/vault-scope flips of the corresponding `egui_editor` features (see `editor/SPEC.md`): Word wrap (§3.8), Show whitespace (special-character rendering, §9.16), Highlight trailing whitespace (§9.17 — quiet enough to leave on for code, noisy on prose, so opt-in; default off, persisted per-vault), Show line numbers (gutter, §3.7). The menu rows are hiker chrome; the rendering is the widget's. [view-word-wrap-toggle, view-show-whitespace-toggle, view-highlight-trailing-whitespace-toggle, view-line-numbers-toggle]
- **Show heading breadcrumb** — overlays each chunk with its `heading_path` (already stored on chunks). Pairs with chunk boundaries; defer until both have a real user. [view-heading-breadcrumb-toggle]

### Out of scope (this menu)

- Content-mutating actions — those live in `note-mutations-menu`.
- Per-file scoped toggles. The menu's scope is "active buffer at most"; per-file persistence is a frontmatter concern that doesn't exist in v1.
- Theme / font / color-scheme — those belong in settings, not a quick toggle.


## Note-mutations menu

A top-bar button on the editor pane hosting content-mutation actions on the active note. Sibling to View options (`editor-view-options-menu`); the split is clean — View changes pixels, Mutations changes bytes. Icon-only button using the wand glyph (`mutations-menu-icon`). Click opens a popover listing the mutations applicable to the active buffer. [note-mutations-menu]

Mutations are LLM-driven content rewrites of the active note. Single-note user-initiated mutations apply **as buffer edits** — there is no separate review surface, no derived file, no explicit Apply/Reject verbs. Save accepts, Ctrl-Z reverts, the existing dirty-buffer + drift-check + changes-log machinery handles everything else. The shape is uniform across all current and future mutations:

1. The user clicks a mutation entry. Hiker submits a `Direct`-shape task to `core::tasks` (per `task-queue.md`) at `High` priority — the user is watching. The task carries the buffer's *live* text (not last-saved, same rule as `chat-active-note-context-injection`) so the mutation operates on what the user sees. The buffer is set read-only for the duration of the task, and the source tab is pinned (a preview tab promotes to sticky on submit per `editor-preview-tab-promotion` so a preview-slot swap can't displace the buffer the result needs to land on). [note-mutation-buffer-ro-while-in-flight]
2. The queue's direct-LLM worker drains the task by calling `core::llm::chat` with the mutation's prompt. External MCP-attached clients can also drain the task per the queue's worker rules. The home-page Task queue widget (`task-queue-home-widget`) is the in-flight progress surface — no per-mutation toast.
3. On `TaskCompleted`: the result replaces the source buffer's content as a single editor transaction, the buffer's read-only flag clears, and the buffer becomes dirty. Works whether the source tab is the active one (dispatch through the live editor view) or a background tab (rewrite the tab's saved editor state in place via a transaction off the existing state, preserving history so Ctrl-Z reverts the whole replacement as one undo step on activation). The user reviews by reading the buffer; the dirty-buffer Diff toggle (`editor-diff-vs-disk-toggle`) flips the editor view to a line-level diff against on-disk content for explicit comparison. **Save** writes the mutated content through the regular save path (which handles `pre-write-drift-check` + appends a `'modified'` row to `core::changes`). **Ctrl-Z** reverts the mutation as a single undo step. If the user closed the source tab mid-flight (only possible from the explicit close path, since the tab is RO + pinned during the flight), the result is dropped silently — no toast, no held state. [note-mutation-applies-as-buffer-edit]
4. On `TaskFailed`, the buffer's read-only flag clears and a toast surfaces the error. No content change. On `TaskCancelled` (user cancels via the queue widget), the buffer's read-only flag clears, no content change, no toast.

[note-mutations-menu-task-shape]

**Changes-log lineage.** When a mutation lands on the buffer, hiker stashes a `pending_changes_metadata` field on the buffer carrying `{ mutation: "<mutation-kind>" }`. The next save consumes this stash: the resulting `'modified'` row's `metadata` field carries `mutation: "<kind>"` so the recent-activity widget and any future filter can identify mutation-derived edits. Subsequent saves don't carry the tag — it's a one-shot stamp on the save that accepts the mutation. [note-mutation-stash-changes-tag]

**The user can keep editing during in-flight only by *not* triggering RO** — but the buffer is RO, so they can read and scroll but can't type. Switching buffers is fine; the in-flight mutation locks only its source buffer. If the user closes the source buffer mid-flight, the task continues and the result lands via the toast in step 4.

### v1 mutation: Reformat as markdown

The first concrete mutation: reformat the active note's content as clean markdown. Useful for `.txt` files (per `txt-ingest.md`'s LLM-rewrite option) and for `.md` files whose markup has rotted (uneven heading levels, broken list nesting, inconsistent emphasis). [note-mutation-reformat-as-markdown]

Submits a task with `kind: NoteMutation { mutation: ReformatAsMarkdown, source_path }` and `payload` carrying the buffer's live text + the source extension. The prompt template lives at the user/vault prompt-store path `note_mutation_reformat_as_markdown.md` (per `llm-prompts-file-store`); the bundled default is registered in `core::prompts::bundled_defaults()`.

### Mutations-menu button states

- **Enabled** when the active buffer is an editable note (`mode.kind` is `File`) of an indexable extension (`.md` / `.markdown` / `.txt`) and has at least one byte of content.
- **Disabled** during read-only preview modes (trash / snapshot / staging review) — mutating from inside a review surface would be confusing. Tooltip explains why.
- **Disabled with "Mutation in progress…" tooltip** when there is an active or leased task whose `kind: NoteMutation { source_path }` matches the active buffer's path. The buffer is RO during this window for the same reason. Only one in-flight mutation per source path (`note-mutation-one-in-flight-per-path`).

- **Pending-background-mutation indicator.** When the active buffer has at least one pending background mutation job in the queue (any `NoteMutation`-kind task in non-terminal state whose `source_path` matches the buffer), the Mutations menu trigger renders a small pulsing accent-color dot in the corner of its icon. Same `@keyframes` pulse as `tree-row-queued-marker` so the visual vocabulary stays uniform. Distinct from the "Reformatting…" pill in `#mode-controls` (which names the single in-flight in-buffer mutation): the pill belongs to single-note in-buffer flight, the dot is the presence-of-any-pending indicator for the per-note menu and stays lit across multiple queued or batch-flight jobs (`note-mutation-batch-via-staging`). Driven by the same queue events subscription the menu already maintains. [note-mutations-menu-pending-indicator]

When only one mutation entry is enabled (the v1 case), the popover still opens — clicks-to-action stay one shape so users learn it once. As more mutations land, they slot in alphabetically.

### Batch mutations

Single-note in-buffer is the right shape when the user is watching one note land. **Batch mutations** (e.g., "reformat every `.txt` in `inbox/`") fan out N tasks; the user can't watch N buffers at once. Batch results route through the **staging surface** (per `settings.md`'s staging review section). Results appear on the activity detail page's Pending filter with per-row [Accept] [Reject] and [Accept all], plus on the editor toolbar pill when the affected file is open.

Batch entry points are deferred to v2; they slot into:

- **A folder-context bulk action** invoked from the file tree (`note-mutation-batch-from-folder`, deferred).
- **A search-result bulk action** alongside the already-reserved `search-bulk-action-tag` / `search-bulk-action-move` (`note-mutation-batch-from-search`, deferred).
- **A CLI command** (`hiker mutate <kind> <glob>`, deferred).

All three converge on the same staging-driven flow; no batch-specific review surface. [note-mutation-batch-via-staging]


## Vault home page

When no note is open, the editor pane shows a vault home page in place of the editor — a lightweight overview of the vault rather than empty space. Default landing surface on vault open (assuming no auto-resume of last-open buffer); reappears when the user closes the active buffer without opening another. [vault-home-screen]

Three widgets, in this vertical order:

- **Vault stats.** Total notes, total chunks, breakdown by index state (indexed / queued / skipped / unsupported), maybe disk usage of the vault directory. Pulled cheaply from the existing index store via a single command. Live-updates via the existing indexer-progress events so the counts reflect ongoing work. [vault-home-stats-widget]
- **Recently modified.** Top N (default 10) notes by filesystem mtime. Reuses the mtime field on `DirEntryDto` (`tree-sort-options`); ordering is just `ORDER BY mtime DESC LIMIT N` against the store's notes rows. Each row shows basename + relative path + relative time ("2 hours ago"). Click → open in editor. [vault-home-recent-modified]
- **Recently accessed.** Top N notes by user-open time. Requires a new `last_accessed_at` column on the `notes` row, written from the open-file command path; same row shape and click behavior as recently-modified. [vault-home-recent-accessed]

The new column rides a small slug of its own since the tracking is independent infrastructure (later consumers could include search ranking, habits-of-association, an "activity" view, etc.):

- **Note access tracking.** Add `last_accessed_at INTEGER` to the `notes` row; bump the schema-version constant (same fail-loud + reindex contract as the existing `store-version-fail-loud` / schema bump pattern). Written when a file becomes the active buffer (open from tree, search-result click, recents click, etc.). Read by the recents widget and any future consumer. [note-access-tracking]

Refresh shape: the home page subscribes to indexer-progress events for live stat updates and to watcher file events for recent-modified updates. The recently-accessed list updates on each open without watcher involvement (the writer is hiker itself).

UI scope: minimal. Header with vault root path, three widgets stacked, no charts / graphs, no per-source-type breakdowns yet (those land when source-derived notes are real). A "New note here" button at the top is an obvious affordance to keep — same call as the sidebar's `sidebar-new-item-button`.

Out of scope for v1 of the home page: pinned/landmark notes, active-trail display, search shortcuts, discovery hints from clustering, recent-searches list, vocabulary stats, sync status. All slot in as additional widgets as their backing features land.

### Recent activity widget (lands with `core::changes`)

A fourth widget appears on the home page once the op log's accepted-op feed (`core::activity`, per `op-log.md` "History materialization") has any rows — i.e. as soon as any save / rename / delete has happened in this vault. Hidden when the feed is empty so a fresh vault doesn't show a confusing zero-count tile. [vault-home-recent-activity-widget]

Preview content (the home tile):

- Header: "Recent activity" + count of recent rows.
- Top 3–5 most recent change events: timestamp, path, op (created / modified / deleted / renamed), author class. Click → detail view (see below).
- Mixed-author by default — user saves and (when MCP lands) agent writes appear in the same stream. The widget is *not* agent-specific; the agent-activity use case is a filter preset within the same widget rather than a separate surface.

Refresh: subscribes to a new op-log append event emitted whenever the indexer task appends a row to `core::changes`. Same shape as indexer-progress events. Light debounce (a few hundred ms) so save bursts don't repaint per keystroke.


### Detail views

Vault home widget tiles support a drill-in pattern. **Click on a widget's tile or header → home view body swaps to a detail view for that widget.** No back button affordance within the home view itself — clicking the Home button in the top strip always returns to the home overview, regardless of whether you're in the overview or a detail view. Clicking a note row in any detail view exits home and opens the editor on that note (same shape as `openFile` already exits home view today). [vault-home-detail-views]

Detail views replace the home overview body, not the editor. `#editor-pane` has four states — editor, home overview, home detail, and the settings surface (`settings-pane-mode`, see `settings.md` `## Settings UI shell`).

Transitions:
- Home button toggles editor ↔ home overview.
- Widget-tile clicks: home overview → home detail.
- Gear (`vault-bar-settings-icon`) toggles editor ↔ settings.
- Back: Home button (→ overview), note-row click (→ editor), gear (→ editor).

Read-only review surfaces (trash, snapshot, staging review previews) are sub-modes of the editor state — they share the editor view, and the toolbar's `#mode-controls` slot lights up with mode-specific icon buttons + label (see `## Mode controls slot`). Where applicable, a Diff toggle in that slot flips between the consumer's content and the line-level diff (see `diff.md`). The dirty-buffer Diff toggle (`editor-diff-vs-disk-toggle`) lives in the editor toolbar instead — always visible alongside Save, greyed when no diff target applies.

Per-widget detail views, in roughly the order they earn their keep:

- **`vault-home-stats-detail`** — each Stats tile (Notes / Indexed / Chunks / Queued / Skipped) drills in to a list view:
    - **Notes** — full list of all notes, paginated, sortable by mtime / access / path.
    - **Indexed** — same shape, filtered to indexed-only.
    - **Chunks** — per-note chunk count, sortable; flags pathologies (notes with >100 chunks, notes with 0 chunks). Ties into the deferred `eval-sanity-stats` work — gives a real surface for spotting chunker pathology before the formal eval framework lands.
    - **Queued** — live list of notes currently in the indexer's pending set (`is_pending` per `cmd-file-index-state`). Updates on every indexer-progress event.
    - **Skipped** — list of skipped notes with their reasons (already tracked via `notes.skipped` + `notes.skip_reason`). Per-row "retry" affordance reroutes through `IndexJob::Upsert` with `force=true` so users can manually retry after fixing the underlying issue (file size, encoding).
- **`vault-home-recent-activity-detail`** — full list from `core::changes::recent`, all author classes. Mental model: **each row is a saved version of the file.** Row layout: op label · path · author · time-ago, plus a `current` badge on the most recent row per path and a `↩ restored` badge on rows that were themselves a Restore. Filter pills (author class) live in the header. [vault-home-recent-activity-detail]

    The interaction shape:

    - **Click a row** → opens that snapshot read-only in the editor. Reuses the same `readOnlyCompartment` + banner pattern as `tree-trash-preview`; the banner reads `Snapshot of <path> · <when> · <author> · <op>` with `[Restore this version]` and `[Close preview]` actions. Closing returns to the activity detail view.
    - **Per-row `[Restore this version]`** → for power-user single-click without previewing first. Hidden on the `current` row (restoring the current state is a tautology) and on `'deleted'` rows (no content blob to write).
    - **No separate "Open" button.** Click-the-row → snapshot preview is the only path; the live file is reached via the tree, search, or recently-modified.
    - **No separate "Rollback to before this" button.** The row *is* the version (the content blob lives on it); `Restore this version` is the verb — what you click is what you get.

    Restore reads the version's content via `op_writes::content_at_op` and writes it back through `op_writes::user_save` — a fresh `user` op that becomes the newest accepted version. Command: `restore_snapshot`. The change-shaped flavor (`rollback_change`, via `op_writes::previous_accepted_content`) stays available for the agent-rollback consumer per `mcp.md` — both flavors coexist on the same op-log primitives, see `op-log.md` "History materialization" → "Rollback".

    - **Filter pills — three independent toggles.** Default-all-on; state persists per-vault. Each toggle gates a distinct row population, so two-of-three off is a meaningful filter (e.g. "show only pending agent reviews"). The pills replace the earlier "author class + Pending" split — `Pending` is no longer a separate pill, the show-staging toggle owns that visibility. [vault-home-recent-activity-filter-pills]
        - **Show staging** — pending staging proposals (the rows that route to a review surface on click). Off → backend query switches `source` from `Merged` to `ChangesOnly`. Same icon family as the editor's agent-diff toggle; tooltip "Show pending agent reviews."
        - **User** — committed change rows with `author_class == "user"`. Tooltip "Show user activity." [recent-activity-human-icon]
        - **Agent** — committed change rows with `author_class == "agent"` (i.e. agent writes that have *already* landed on disk — staged-and-accepted, or direct-mode). Distinct from the show-staging pill, which covers proposals that haven't landed yet. Tooltip "Show agent activity." Future author classes (sync, import) join as additional pills in the same row. [recent-activity-agent-icon]
    - **Un-rollback affordance** — append-only log + per-row content blob means *every* prior state stays restorable, including states that were themselves the result of a Restore. Mechanically, "un-rollback" is just Restore on a more recent prior version — same primitive, no separate operation. UX: rows tagged `metadata.restored_from` show a `↩ restored` badge; immediately after a Restore action, the row that *was* the current state for that path gets a soft highlight + "← previous state — click Restore to undo" caption. The action is the regular `[Restore this version]` button on that row (no separate primitive); the caption is purely a hint. This is materially better than linear undo stacks where redo state vanishes after a subsequent edit; here, every row within retention is equally accessible as a Restore target. [vault-home-recent-activity-unrollback]
    - **Snapshot read-only preview.** Reuses the trash-preview machinery: `setReadOnly(true, "snapshot")` swaps in the snapshot banner, suppresses the save button + dirty marker, and the dirty-switch guard treats it the same as a trash preview (nothing to discard). The buffer carries `snapshotPreview: true` and `snapshotChangeId` so the banner's Restore action can write back without a re-lookup. Different banner color from trash (amber, not red) — informational, not a recovery surface. [snapshot-preview-mode]
- **`vault-home-recents-detail`** (lower priority — lands when needed) — full list versions of Recently Modified / Recently Accessed. Less urgent than the stats and agent-activity ones since each preview row already has a click-to-open affordance; the detail view adds filtering / longer history but isn't load-bearing.

Detail views don't get individual stub-slugs for each Stats subview (Notes / Indexed / Chunks / Queued / Skipped) — they're variations of the same `vault-home-stats-detail` slug parameterized by which tile launched them. Adding new tiles in the future just adds parameter values, not new slugs. [vault-home-stats-detail]

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
- **Home button.** House glyph. Toggles the editor pane to the vault home page. View toggle, not buffer close — the active buffer (if any) stays in memory; clicking any tree row, recents entry, search result, or tab restores the editor onto whichever note. Tooltip "Vault home." Reserves the keybind id `vault.go-home` in `keybind-registry`. [vault-home-button]
- **Queue button.** List-with-pulse glyph. Opens the shared queue detail page (per `task-queue.md`'s `queue-detail-shared-page`). A small indicator superimposed on the icon shows the count of `Queued + Leased` tasks; hidden when zero. The icon pulses subtly when anything is `Leased`. Tooltip "Background work" (or "Background work (N active)" when N > 0). [vault-bar-queue-button]
- **Settings button.** Gear glyph. Toggles the editor pane to the settings surface (`settings-pane-mode`). Same view-toggle behavior as Home — the active buffer stays in memory. Pressed/unpressed state reflects whether the settings pane is currently visible. Tooltip "Settings." Keybind `settings.open` — Cmd-, on macOS, Ctrl-, elsewhere. [vault-bar-settings-icon]
- **Open-vault button.** Folder glyph. Triggers the JS dialog → `open_vault_at` flow per `settings.md`'s default-vault-autoopen story. Tooltip "Open vault…". [vault-bar-open-vault-icon]

The vault-path label sits to the right of the icon cluster, before the tab strip — same shape it has today, just relocated. Truncates with ellipsis when space is tight (per `ui-no-sibling-pushout`).

The slug names retain the `vault-bar-` prefix even though there's no vault bar anymore — slugs name the *feature*, not the location, and the buttons themselves haven't fundamentally changed shape. (`vault-home-button` already has no prefix; the others stay as-is.)

### Tab strip

The tab strip itself — per-group tabs, active/inactive shading, dirty-dot↔close-× swap, overflow scrolling + dropdown, middle-click close, drag-between-groups, the preview/pinned visual states — is `egui_workbench` (`egui-workbench/SPEC.md` §5). Hiker fills it with the tab kinds below and wires these hiker-specific behaviors: [editor-tab-strip]

- **Tab content / disambiguation.** Tab label is the open buffer's basename. When two open buffers share a basename, both render with a folder hint (`notes.md (research/)` vs `notes.md (inbox/)`); tooltip shows the full vault-relative path. [editor-tab-disambiguation]
- **Tab keybinds**, all reserved in `keybind-registry`: `tab.close` = Cmd/Ctrl-W, `tab.next` = Cmd/Ctrl-Tab, `tab.previous` = Cmd/Ctrl-Shift-Tab, `tab.jump-N` = Cmd/Ctrl-1..9 (9 jumps to the last tab). Hiker binds these into the workbench's tab actions. [editor-tab-keybinds]
- **Right-click context menu.** Hiker adds a **Reveal in tree** verb (selects the tab's note in the file tree, expanding parent folders) alongside the workbench's Close / Close others / Close to the right. [editor-tab-context-menu]
- **No `+` button.** New notes have a clear home in the file tree's `+ New note` affordance; duplicating it in the tab strip splits the surface for no gain.

The active/inactive shading and dirty-marker rendering slugs map onto the workbench tab states. [editor-tab-active-state, editor-tab-dirty-marker, editor-tab-overflow]

### Tab strip behavior with the rest of the app

- **File-tree click on an already-open file** switches to its tab rather than reloading. Click on a not-yet-open file opens a new tab and switches to it. [multi-buffer-tree-click-switches-tab]
- **Search-result, recents, wikilink, and any other "open this note" entry point** behave the same: existing tab → switch; not yet open → new tab.
- **Mode-controls slot, View menu, Mutations menu, chat panel "active note" injection, navigation history** all operate on the active tab, no change.
- **In-flight-mutation RO** (`note-mutation-buffer-ro-while-in-flight`) applies to the source tab regardless of whether it's currently active. The dirty marker on a tab whose buffer is mid-mutation reads as a normal dirty dot; users learn the queue widget / inline indicator as the source-of-truth for "what's working in the background."

### Multi-buffer model

The editor-group + tab container is `egui_workbench` (§4–§5). Hiker's policy on top of it:

- **In-memory while the vault is open; tab state restores on next open.** The set of open buffers is in-memory state during a session — closes, switches, and dirty content all live in RAM. The autosave layer (`autosave.md`) round-trips a tab-state snapshot (open paths + active path + preview-slot path) to `.hiker/autosave/index.json`, so the next vault open silently reopens the same set of tabs. Per-buffer dirty content recovery rides the same store, prompting via the recovery modal. [multi-buffer-in-memory-only]
- **No max open count / no retention timer.** Tabs stay until the user closes them; a user with 50 tabs gets the workbench's overflow handling.
- **`file-switch-guard-dirty` is close-time only.** Navigating *to* a dirty tab is fine — the dirty buffer stays dirty in memory. The save/discard/cancel modal only fires when the user closes the tab (× / middle-click / Cmd-W). The existing nav-time fire is dropped. [multi-buffer-no-switch-guard]
- **Window close has no dirty-buffer modal.** Quitting the app or closing the window flushes every dirty buffer through the autosave pipeline and pushes the open-tab snapshot, then destroys the window — no prompt, no per-tab choice. Next launch's auto-restore reopens the workspace as dirty tabs (`autosave-recovery-auto-restore` + `autosave-tab-state-silent-restore`); the user saves or reverts then via the existing affordances. Note: this is "park the work," not "commit to disk" — the user's actual files are unchanged on exit. [autosave-close-no-modal]
- **Navigation history stays unified** across all tabs (one stack per vault). Back/forward navigates between content surfaces regardless of which tab they were in; the corresponding tab activates as part of the back/forward action.


### Tab kinds

A tab is a `(kind, payload)` pair. The kind names *what* the tab renders; the payload identifies *which one*.

**Umbrella term: "app pages."** Every non-`buffer` kind below (`home`, `home-detail`, `queue`, `settings`, `properties`, `agent`, `graph`) is collectively an *app page* — a tab that renders an in-app surface rather than user-authored content. "App-page tabs" is the form used where the tab-strip aspect matters; "app pages" is the bare noun. The term replaces the earlier inconsistent "page-kind tabs" / "meta pages" wording. The `TabKind` discriminator on the wire stays per-kind (`home`, `queue`, …) — "app page" is umbrella vocabulary, not a runtime category.

- `buffer` — payload is a vault-relative file path plus an optional `DiffSource` (per `diff.md` `diff-source-enum`). Renders the editor widget for that file; when `diff` is set, layers a `DiffLayer` over the same widget — diff is a mode of this tab, not a separate kind. Snapshot review, trash preview, staging-proposal review, dirty-buffer diff, history diff (right-click → Show changes) are all `buffer` tabs with different `DiffSource` selections. All current tab semantics (preview slot, dirty marker, close guard, autosave participation, tree-click activation, search-result-click activation, navigation-history entries) describe this kind.
- `agent` — payload is a chat session id; renders the chat surface as the tab's content (per `chat-panel-expand-to-editor`). The discovery-panel's bottom-docked chat region collapses while an agent tab is open since the surface lives in the tab; closing the agent tab restores the docked region.
- `graph` — payload is the graph view's state (filter set, selection); renders a graph-view canvas (per `design.md`'s graph-view future bullet).
- `home` — vault home overview (per `vault-home-screen`); renders the home page as the tab's content.
- `home-detail` — payload is the detail-view kind (`stats` | `recent-activity` | `recent-modified` | `recent-accessed`); renders the home page's drill-in view.
- `queue` — task queue + indexer detail view (per `task-queue-home-detail-view`).
- `settings` — settings pane (per `settings-pane-mode`).
- `properties` — payload is a vault-relative note path; renders the read-only properties inspector for that note (per `note-properties-tab`). One properties tab per note path; opening Properties on a path that already has a tab open switches to it rather than spawning a duplicate.
- `cluster-review` — payload is a `ClusterReviewState` (purpose: `new-tree` | `recluster-subtree { tree_id, node_id }` | `rebuild { tree_id }`, plus the in-flight build configuration and any in-memory structural result). Renders the clustering review surface per `cluster-review-tab` in `cluster-editor.md` — configure → run → review → confirm. On Confirm the tab transitions in place to `cluster-batch-review` for the newly-persisted tree.

Tab-strip rendering is kind-aware: a small leading icon distinguishes the kind (file glyph for `buffer`, chat glyph for `agent`, graph glyph for `graph`, house glyph for `home`/`home-detail`, list-with-pulse for `queue`, gear for `settings`); the label is whatever the kind chooses (basename for `buffer`, session preview for `agent`, "Graph" or filter summary for `graph`, "Home" / "Recent activity" / "Queue" / "Settings" / etc. for the app pages).

**App-page tabs default-land in the preview slot.** Clicking the Home / Queue / Settings buttons in the top strip's leading cluster opens the corresponding tab as a *preview*: it occupies the preview slot, replacing whatever preview was previously there (same one-preview-at-a-time rule as `editor-preview-tab`). The user can promote an app-page preview to sticky via the same affordances as buffer previews (right-click "Keep open", or any user interaction on the tab body that signals "I'm staying" — per-kind decision: home-detail clicks within the page count as promotion; settings flips do not, since the user is just toggling and leaving). This keeps the common case ("glance at home, then go back to my work") clutter-free while letting power users keep pages around.

**Buffer-scoped chrome hides when the active tab is non-buffer.** The editor toolbar's buffer-scoped controls (View menu, Save button, Diff button, Mutations menu, the mode-controls slot) and the bottom status bar (line:col, index-state label, file-path) are buffer-only — they hide entirely when the active tab is `agent`, `graph`, `home`, `home-detail`, `queue`, or `settings`. The sidebar / discovery toggle icons stay visible regardless because they control the side panels independently of the center pane. Each non-buffer kind brings its own chrome (or none) inside the tab body — settings has its scope toggle and refresh button in its own header, home has its overview/detail toggle, etc.

**Kind-aware predicates.** Existing tab semantics that assume "every tab is a file buffer" gate on kind:

- **Preview slot** (`editor-preview-tab`) — buffer-only on the *contents-tracking* side (paths replace each other in the slot). App-page tabs use the same one-slot-per-strip rule; opening an app-page tab evicts whatever was previewed before (buffer or app page).
- **Dirty marker** (`editor-tab-dirty-marker`) is `buffer`-only — non-buffer tabs have no dirty concept.
- **Close guard** (`file-switch-guard-dirty`) only fires when closing a `buffer` tab whose buffer is dirty.
- **Autosave tab-state** (`autosave-tab-state-store`) records `(kind, payload)` per open tab; restore reopens each kind through its own mount path.
- **Mode controls slot** (`editor-toolbar-mode-controls`) is buffer-only chrome and is hidden along with the rest of the editor toolbar on non-buffer tabs.
- **Reveal in tree** (`editor-tab-context-menu`) only applies to `buffer` tabs.

[tab-kinds]


### Note properties tab

Right-click → Properties on a tree row opens a `properties`-kind tab for that note. The tab is a read-only inspector of every piece of state hiker tracks for the note across `index.db` and the op log — the answer to "what does hiker actually know about this file." Useful for debugging skip reasons, sanity-checking embedder version drift, auditing the change log without opening recent activity, and confirming trail / cluster membership. Frontmatter editing is **not** part of this tab; the in-place frontmatter editor (`tree-context-properties-frontmatter-editing`) is a separate future surface that will eventually layer in as a section once a frontmatter-editing primitive exists.

The headline decisions:

- **One properties tab per note path.** Opening Properties on a path that already has a properties tab open switches to it instead of spawning a duplicate — same shape as the file-tree click rule for buffer tabs. [note-properties-tab]
- **Read-only data view, no editor chrome.** The tab is non-buffer per `tab-kinds`, so the editor toolbar and bottom status bar hide on activation. The tab body owns its own header (note basename + relative path). No save button, no dirty marker, no preview-slot promotion path — clicking Properties from the tree always opens sticky (it's a directed action, like restore-from-trash). [note-properties-tab-no-editor-chrome]
- **App-page preview-slot rule still applies on open.** Properties tabs default-land in the preview slot — same rule as `home` / `queue` / `settings` (per `tab-kinds`). Clicking Properties on a second note replaces the preview; promotion paths are the standard ones (right-click "Keep open", drag, etc.). [note-properties-tab-preview-slot]
- **Live-refreshing.** The tab subscribes to the same event surfaces the rest of the UI rides — indexer-progress events (notes-row / chunks data refreshes when a re-ingest finishes), op-log append events (changes-section refreshes on every new change row for this path), watcher file events (mtime / size refresh on external edits). No manual refresh button; the data is always current. [note-properties-tab-live-refresh]

#### Sections rendered

Each section is a labeled block stacked vertically; sections render in order regardless of whether they have content (a missing row shows an empty-state line). [note-properties-tab-content]

- **Identity.** Vault-relative path, note ULID (from `notes.id`), and the `path_ids` row's id. Calls out a mismatch between `notes.id` and `path_ids[path]` if it exists (shouldn't, but if it does the user should see it).
- **File state.** `mtime`, `size`, `content_hash` (full blake3 hex, copyable), filesystem extension, and whether the path is currently inside the open buffer set / open in another tab.
- **Index state.** `indexed_at`, `embedder_version`, `skipped` flag, `skip_reason` (if any), and the indexer's runtime classification (`Indexed` / `Skipped` / `Queued` / `Unsupported`) — same surface that drives `tree-row-unsupported-marker` / `tree-row-skipped-marker` / `tree-row-queued-marker` / `status-bar-active-file-index-state`.
- **Chunks.** Total count plus a compact list: `chunk_index`, byte range, `heading_path` (or `—`), and the first ~80 chars of the chunk text as a snippet. Long lists virtualize; the surface is a debugging aid, not a search UI.
- **Access tracking.** `last_accessed_at` from the notes row (per `note-access-tracking`), formatted as a relative time with absolute on hover.
- **Changes.** Total count of `changes` rows for this path, breakdown by `author_class` (user / agent / sync / import / other), and a chronological list of the most recent N rows (timestamp, op, author, metadata summary). Each row click opens the change-row detail in `snapshot-preview-mode` — same affordance the home page's recent-activity detail uses (`vault-home-recent-activity-detail`), so the inspector shares a code path rather than re-implementing snapshot preview.
- **Trail / cluster membership.** Trails containing this note (resolved via `core::trails::trails_containing_note_with_paths`, same query that powers `chat-tool-call-opens-touched-note` and the trails verbs) and clusters this note belongs to (when clustering data is available — placeholder otherwise).

#### Behavior details

- **Open paths.** Right-click → Properties (this slug, `tree-context-properties`) is the canonical entry; an editor-tab-strip right-click verb on `buffer` tabs (`Show properties` in the tab context menu, per `editor-tab-context-menu`) and a "Properties" entry on the buffer's right-click context menu in the future are the only other entries. Programmatic `openProperties(rel)` skips the preview slot per the directed-action rule.
- **Path doesn't resolve.** If the path no longer exists on disk by the time the tab activates (file deleted, moved by another process), the tab renders a "Note not found at `<path>`" empty state but still shows whatever the index and changes db know about the path — exactly the case the inspector exists to surface.
- **Trash entries.** Right-clicking a trash row → Properties opens the same tab kind for the trashed note (path is the trash-relative path). The "Index state" section shows `Skipped` (trash entries aren't indexed) and the "Changes" section shows the row recorded at delete time — useful for "what was this file's state when I deleted it." [note-properties-tab-trash]
- **Autosave tab-state.** Properties tabs participate in `autosave-tab-state-store` like every other kind — a properties tab open at quit reopens at the same path on next launch. Stable enough to ride the standard machinery; nothing tab-kind-specific to do.
- **Reveal in tree.** Tab right-click → Reveal in tree highlights the note in the file tree, same shape as the buffer-tab verb.
- **No write affordances in v1.** Editing frontmatter, force-reindexing this note, restoring a change row — all candidates for follow-up but the v1 surface is strictly read-only. Force-reindex of a single note is the most likely first write addition (slug-reserved as `note-properties-force-reindex` below).

#### Out of scope (deferred)

- **In-place frontmatter editing.** Tracked under `tree-context-properties-frontmatter-editing`; lands when a frontmatter-editing primitive exists.
- **Force-reindex this note.** A button that submits a single-note `IndexJob::Reindex`. Useful when debugging skip reasons or embedder drift; deferred until the surface has a real user. [note-properties-force-reindex]
- **Restore-from-this-row inline.** The changes section already opens each row in `snapshot-preview-mode` which carries Restore — duplicating the Restore button on every row in the inspector would be redundant.
- **Properties for non-note paths** (folders, trash entries other than `.md` / `.txt`). Folder properties are a different surface (recursive note count, total bytes); revisit when there's an ask. [note-properties-tab-folder-deferred]
- **Comparison view across two notes.** "Why does the indexer skip A but not B" is occasionally useful; defer until someone wants it.


### Preview tabs

The preview-tab mechanic — at most one preview slot, italic title, replace-in-place on the next preview-open, promote-to-sticky on edit / double-click / drag / "Keep open" — is `egui_workbench` (`egui-workbench/SPEC.md` §5.3). Hiker wires which callsites open preview and how directed actions opt out:

- **Every click-driven open-note callsite uses the preview slot by default.** File-tree click, search-result click, related-notes click, recents click, wikilink click (when wikilinks land), chat note-link click, `@`-mention click in the chat panel — all route through `openFile(rel, { preview: true })`. The set is uniform on purpose; carving exceptions per surface would be a worse mental model than "click is preview, Mod-click is sticky." [editor-preview-tab-from-open-callsites]
- **Mod-click on any open-note callsite forces a sticky tab.** Skips the preview slot, opens directly into a new sticky tab. Drag-from-tree (when that's a thing) is also implicitly sticky. [editor-preview-tab-mod-click-sticky]
- **Programmatic opens skip preview.** Restore-from-trash, new-note creation, the right-click "Open" tree verb, mutation-apply, and any other non-user-click path open sticky — these are directed actions, not browsing. `openFile` is `{ preview: false }` (or omitted) at those callsites.
- **Edit-as-promotion keeps preview tabs never dirty** — the moment the user types, the tab is sticky, so the dirty-buffer machinery (`file-switch-guard-dirty`, `autosave-close-no-modal`) never has to know about preview tabs. [editor-preview-tab, editor-preview-tab-promotion]
- **Tree double-click stays bound to inline rename** per `tree-double-click-rename` — promotion via double-click on a *tree row* would conflict; tab double-click covers the canonical promote gesture.
- **Pending agent proposals route the open into review mode.** When `openFile(rel)` resolves a path with one or more pending staging proposals, the buffer lands in patch-review or write-note review per `note-open-routes-to-pending-review` (in `patch-review.md`). The preview-vs-sticky distinction is preserved; the review state rides on `buffer.mode`, not the tab kind.


## Navigation (back / forward)

Browser-style back/forward navigation across editor-pane states. Each user-initiated transition between distinct content surfaces — opening a note, going home, drilling into a home detail view, opening a trash preview, switching tabs — pushes onto a per-vault history stack. Back and forward navigate that stack via the top strip's leading-cluster buttons, trackpad two-finger horizontal swipe (matching browser convention), and a keybind registry entry.

The headline decisions:

- **History is a per-vault in-memory stack of editor-pane content states.** Cleared on vault swap. Not persisted across hiker restarts (matches browser per-window behavior). [navigation-history-stack]
- **Back and forward buttons live in the top strip's leading cluster** (leftmost, before Home / Queue / Settings / Open). Icon-only, disabled when no history exists in that direction. Browser-leading-left placement keeps all vault-level navigation controls together; per-buffer navigation lives separately in the tab strip. [top-strip-back-button, top-strip-forward-button]
- **Two-finger horizontal trackpad swipe** triggers back/forward. Same UX as macOS Safari / Chrome / Firefox. Detection via wheel events with sustained `deltaX` past a threshold; right-swipe = back, left-swipe = forward (matches browser convention). [navigation-trackpad-swipe]
- **Keybind registry entries** reserve `navigation.back` and `navigation.forward` with platform-conventional chords: Cmd/Ctrl-[ for back, Cmd/Ctrl-] for forward; Alt-Left/Right as additional bindings on Linux/Windows for browser-keyboard parity. [navigation-keybind]
- **Mouse side buttons** (mouse-button-3 / mouse-button-4 — the thumb buttons on standard mice with a back/forward pair) trigger back/forward by default, matching every browser. Detection via window-level `mousedown` (or `auxclick`) listening for `event.button === 3` (back) and `event.button === 4` (forward); calls into the same `navigation.back` / `navigation.forward` action handlers as the keybind and trackpad-swipe paths so all three trigger surfaces converge. Default-on; rebinding deferred until the keybind registry grows mouse-button support (today's registry is keyboard-chord-only). [navigation-mouse-buttons]
- **Dirty-buffer protection** is moot for back/forward navigation under multi-buffer — navigating to a different tab leaves the prior tab dirty in memory rather than closing it. The save/discard/cancel modal only fires on explicit tab close (per `multi-buffer-no-switch-guard`); window close has no modal (`autosave-close-no-modal`).


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

When the user navigates back and then opens a new content surface, the forward stack is discarded — same shape as every browser. The history feels predictable.


### Trackpad swipe shape

Browser convention: two-finger horizontal swipe on a trackpad triggers back/forward. macOS surfaces this as `wheel` events with `deltaX` accumulation; the editor pane's wheel handler watches for sustained horizontal scroll past a threshold (e.g. ~120px of accumulated `deltaX` over a short time window) and fires the navigation. Vertical swipes are ignored.

**Visual feedback while swiping.** As `deltaX` accumulates past a small floor (~30px) but before the commit threshold, a directional indicator fades in on the swiped-toward edge of the editor pane — a chevron glyph (`‹` for back, `›` for forward) plus a thin progress bar whose fill tracks `|accumulated_deltaX| / threshold`. When the threshold trips, the indicator briefly snaps to fully-filled and fires the navigation; if the user reverses or the 250ms quiet-reset window expires, the indicator fades out without committing. Greyed (indicator visible but desaturated) when there's no history in that direction, so the user gets clear "would commit but nothing to navigate to" feedback rather than a silent no-op. [navigation-swipe-visual-feedback]

Right-swipe = back. Left-swipe = forward. Same as every browser.

Edge cases worth pinning:

- **Inside the editor.** The editor widget doesn't consume horizontal trackpad scroll for content that isn't horizontally scrollable, so horizontal-scroll deltas reach the pane's swipe handler naturally. If a markdown-source line is horizontally scrolled (rare for prose; possible in code blocks), the swipe should still trigger navigation when the horizontal delta substantially exceeds the line's scrollable extent.
- **Inside scrollable detail-view lists.** Same shape — the list scrolls on `deltaY`, so horizontal swipes pass through.
- **Touchscreen devices.** v1 of this feature targets trackpads only. Touch swipe gestures are a separate slug if the project ever ships a touchscreen-friendly variant.


### Dirty-buffer interaction

Back/forward navigation under multi-buffer doesn't need a dirty-buffer guard — the dirty buffer stays in its tab, the navigation just activates a different tab (or pane state). The save/discard/cancel modal only fires on explicit tab close + window close.

Closing the vault while history exists drops the entire stack — no warning, no save protection beyond what already gates vault swap.


### Out of scope (this feature)

- **Persisting history across restarts.** Browser-shaped feature: history is per-session.
- **Tab-style multi-buffer history.** Hiker is single-buffer in v1; if tabs ever land, each tab gets its own history stack.
- **Touchscreen swipe gestures.** Trackpad-only for v1.
- **Rich history menu (right-click → list of last N pages).** Browser-shaped polish, deferred.


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
- Widget-based rendering (images, math, embeds, callouts)
- Multi-buffer / tabs / split panes
- Vim/Emacs keymaps
- User keybind overrides (the registry supports it; the loader is later)
- External-change watcher integration (v1)
