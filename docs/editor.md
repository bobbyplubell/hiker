# Editor

In-tree widget over egui. Live-preview decorations and widget rendering: see `design.md`.

- `editor-core` — `Rope`, `EditorState`, `Selection`/`SelRange`/`Anchor`, `Transaction`, `Decoration`/`DecorationSet`/`RangeSet`. Pure data.
- `editor-view` — `command::handle(state, view, event)`, decoration providers, `ViewState`, `CompletionSource` trait.
- `editor-egui` — input translation, painter, gutter, selection, scroll.

Transactions, decorations, and selections referenced below are the types from those crates.


## Buffer model

One open buffer at a time in v0. Switching files replaces the buffer's contents via a single `dispatch` that swaps the entire doc.

State tracked per buffer:

- `path` — vault-relative; null when no file is open [buffer-path-tracking]
- `loadedHash` — hash (or full string) of the contents most recently read from / written to disk
- `isDirty` — derived: current doc text !== loadedHash

`isDirty` is the single source of truth for save state — computed lazily from the editor doc and `loadedHash`, no separate flag that can desync. Cleared by re-reads and successful writes; set implicitly by any edit. [buffer-dirty-derived]

Multi-buffer / tabs deferred. When tabs land, the same per-buffer state moves into a `Buffer[]` keyed by path, with the active buffer driving the editor view.


## Save UX

Save action: writes current doc to `currentPath` via the `write_file` core command. On success, updates `loadedHash` to the new doc text, which clears `isDirty`. On error, surfaces a non-blocking error toast and leaves the dirty state alone (so the user can retry).

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

External changes: two mechanisms.

- Pre-write drift check (v0). Every save re-reads the file and compares its hash to `loadedHash` before writing. [pre-write-drift-check, drift-conflict-modal]
    - match — write proceeds; `loadedHash` updates.
    - file missing — prompt: write anyway (re-creates) / cancel.
    - hash mismatch — conflict prompt: keep mine (overwrite) / take theirs (discard buffer, reload) / open diff (deferred — falls back to keep/take in v0).
    - Catches the "I edited the file in vim while it was open in Hiker" case without a watcher.

- Watcher integration (v1). The notify-based watcher (lands with the indexer) pushes file-change events for the open file.
    - buffer clean — silently reload; `loadedHash` updates.
    - buffer dirty — same conflict prompt, but proactive (on event, not at save time).
    - Reduces the stale-buffer window; pre-write check remains as final guard since watchers miss events (network filesystems, rapid changes, event/save races).

Both mechanisms reduce to the same conflict-resolution UI; only the trigger differs.


## Keybind registry

Window-level chords: `app/src/keybinds.rs`, intercepted via `ctx.input_mut(|i| i.consume_key(...))` before the editor sees them. Buffer-local chords: `editor-view::command::handle`. `known_keybindings()` returns the flat window-level list the F1 overlay enumerates. Goals: discoverable, overridable (later), conflict-detectable. [keybind-registry]

Shape:

```ts
interface Binding {
  id: string;            // "editor.save", "editor.toggleBold"
  keys: string;          // CM6 chord syntax: "Mod-s", "Mod-Shift-p"
  label: string;         // human-readable for help panel
  run: (view: EditorView) => boolean;   // returns true if handled
}
```

Compilation: `registry.toCMKeymap()` returns a CM6 extension built from `keymap.of(bindings.map(b => ({key: b.keys, run: b.run})))`. The editor wires this in once at startup.

Validation: a `registry.validate()` pass at startup logs and throws on duplicate `id` or duplicate `keys`. No silent overrides.

Scope: v0 has one scope — the editor. Bindings only fire when the editor has DOM focus. When a future binding needs to fire outside the editor (e.g. `Mod-P` quick-open from any pane), reuse CM6's exported `keyName` parser in a window-level `keydown` handler — never roll a custom chord parser. Add a `scope` field then; until then, omit it.

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

Why basename rather than full path: the file tree shows location, the window title carries disambiguation, and full paths overflow on deep vaults. Tooltip + tree cover "where does it live."

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

- Snapshots and staging entries come from `core::activity::list_for_path(path, filter)` (see `changes.md` `## Unified activity feed`) so the dropdown shares the merged-feed type with the activity detail page rather than calling two separate APIs and reconciling them in the UI. [status-bar-version-dropdown-uses-unified-feed]
- The list refreshes on `hiker:changes-appended` and `hiker:staging-changed` for events that touch the active buffer's path; debounced consistent with the activity widget. [status-bar-version-dropdown-live-refresh]

Why a dropdown rather than a static label: the prior label only surfaced *which* preview was active, and only one preview was reachable from outside the editor at a time. A per-buffer version picker makes the status bar the canonical place to ask "what other versions of this file exist?" without leaving the editor. [status-bar-version-dropdown]

Trash entries are out of scope for the dropdown — a trash entry *is* a different file on disk (different path), not a version of the open buffer; surfaced via the existing `tree-trash-preview` path.


### Sibling protection (overflow rule)

Every status-bar region — and any other horizontal toolbar / strip elsewhere in the app — must use `min-width: 0` and `flex-shrink: 1` so a long string in one region cannot push siblings off-screen. The basename + tooltip change above fixes the common case for the path region; the rule generalizes. Anywhere a region's content is user-derived (file names, error messages, status labels reflecting external state), the same `min-width: 0` + ellipsis combo applies. Tracked as `ui-no-sibling-pushout` so the rule has a slug to cite from CSS comments and code review. [ui-no-sibling-pushout]


## Layout (v1)

Four regions: top strip across the window, then three columns below it (sidebar / editor / discovery), both side columns collapsible. [four-region-layout]

- **Top**: a single horizontal strip across the full width of the window. Leading cluster of icon buttons on the left (Back / Forward / Home / Queue / Settings / Open vault) plus the active vault path label, then the tab strip filling the rest of the row. See `## Top strip` below. [top-strip-layout]
- **Left**: sidebar. Collapsible. Mode-switchable (Files / Cluster trees / Trails). In Files mode it hosts the file tree, including drag-and-drop note moves — the drop calls a single core `move_note` command that does the fs rename and updates the index path in one step, so the move is recorded explicitly rather than being inferred from watcher events. Same code path is exposed as a `hiker mv` CLI command. [drag-and-drop-move]

  ### Sidebar mode switcher

  The sidebar's content is mode-switchable. Three modes:

  - **Files** (default) — file tree as described in this section. The `+ New note` / `…` actions row is filetree-specific chrome; the Trash bin pinned at the bottom is shared across modes (trash is multimodal — may hold notes, trails, cluster trees).
  - **Cluster trees** — switches the sidebar body to the cluster editor (per `cluster-editor.md`); filetree chrome hides; the cluster editor brings its own header (tree-name selector, "Suggest reorganization" action, mode-specific `…` menu). The Trash bin stays pinned regardless of mode.
  - **Trails** — reserved slot, greyed in v1; lands when trails do.

  The top of the sidebar is a single uniform icon-only row, persistent across all three modes: three mode buttons on the left (Files / Cluster trees / Trails — pressed-state on the active mode), then a divider, then a `+` (new note) and a `⋯` (actions menu) on the right. No labels anywhere. Clicking a mode icon switches the sidebar body in place; the editor pane is unaffected, the active buffer stays loaded, the discovery panel keeps its state. Mode is persisted per-vault under `vault.sidebar_mode` via the existing `set_setting` plumbing (eligible-key set grows by one). [sidebar-mode-switcher]

  The sidebar's collapse toggle (`sidebar-toggle-icon`) keeps its existing behavior — it hides the whole sidebar regardless of mode. Modes share one collapse state. [sidebar-mode-shared-collapse]

  The `+` and `⋯` buttons sit in the same top row regardless of mode, so the row layout never shifts when the user switches modes. Their *behavior* is mode-aware:

  - **`+` is the new-item button.** Left-click creates the active mode's primary item: a note in Files mode, a cluster tree in Cluster trees mode, a trail in Trails mode. Right-click opens a popover that lets the user pick any item type regardless of current mode (New note / New cluster tree / New trail), so creating a trail while browsing files doesn't require a mode switch. [sidebar-new-item-button]
  - **`⋯` menu's contents swap per mode**: filetree actions in Files mode (Refresh tree / Reindex all / Reindex this file / Sort by); the `cluster-editor-mode-menu` entries in Cluster trees mode; trail-scoped entries in Trails mode when trails land. [sidebar-toolbar-actions-menu]

  - **New note** (Files-mode `+` left-click): creates a numbered `new-note-N.md` in the currently-selected folder (vault root if nothing's selected) via a `create_note(rel_path)` core command. `N` is the lowest positive integer that doesn't collide with an existing file in the target folder — `new-note-1.md` first, then `new-note-2.md`, and so on. The new file opens in the editor immediately, and the tree row enters inline-rename mode with the `new-note-N` basename pre-selected (extension excluded from selection so users can type a new name and hit Enter without re-typing `.md`). Submit renames via the same `move_note` path; Esc keeps the default name. [sidebar-new-item-button]
  - **`⋯` menu** (Files mode) opens a small popover with the v1 entries below. Adding new entries is intentionally low-friction — the menu is the catch-all for low-frequency filetree actions, so future verbs slot in here rather than growing the top row. (For Cluster trees mode the menu hosts `cluster-editor-mode-menu` entries; for Trails mode, trail-scoped entries when trails land.)
    - **Refresh tree** — re-reads the directory and rebuilds the tree from disk. With the v1 watcher, the tree should mostly stay in sync on its own — refresh is a backstop for the watcher's known failure modes (notify queue overflow during big git checkouts, NFS/network filesystems, missed events) and for the "did I really just save that" sanity case. Auto-refresh from watcher events is a v2 add per `watcher.md`; refresh stays even after that lands. [tree-refresh-manual, tree-refresh-watcher]
    - **Reindex all** — full-vault reindex via `reindex-all-action` (see `index.md`). No confirm modal: re-embedding identical content is non-destructive, and the user opted in by clicking.
    - **Reindex this file** — single-file reindex via `reindex-current-file-action`; greyed when no file is active.
    - **Sort by ▸** — submenu of mutually-exclusive sort orders applied to the file tree (folders always grouped first; the chosen order applies within folders and within files). v1 entries: **Name (A→Z)** (default), **Name (Z→A)**, **Modified (newest first)**, **Modified (oldest first)**. Selection persists in memory only for v1; per-vault persistence is a `settings.md` concern when that surface lands. Modified time comes from the filesystem's mtime — same field the watcher and indexer already use, no new metadata. [tree-sort-options]

    A destructive **Reindex (rebuild)** verb — drops and recreates the schema before reindexing — is deferred to the settings page (`settings.md`). The CLI counterpart `cli-reindex-rebuild` covers the operational case in the meantime.

  ### API & edge cases

  Both `create_note` and `move_note` live in `core::vault` and are the single source of truth for creating and relocating notes — UI tree actions and CLI commands (`hiker new`, `hiker mv`) call them unchanged.

  - `create_note(rel: &str) -> Result<String>` — creates an empty file at `rel`, returns the actual path used (since auto-suffix may have changed it from the requested name). The button always passes a `new-note-N.md` candidate; the CLI passes the user's requested name verbatim and errors on collision rather than auto-suffixing (CLI behavior is explicit; UI behavior is forgiving). [create-note-core-cmd]
  - `move_note(from: &str, to: &str) -> Result<()>` — atomic fs rename + index update. Order: suppress watcher events for both paths (see below), fs rename, update `notes.path` + `path_ids` in a single transaction, release suppression. If the index update fails the fs rename is rolled back (rename `to` → `from`) before returning the error. [move-note-core-cmd]
  - **Target collision** — `move_note` errors and leaves the source untouched. No overwrite, no auto-suffix; the caller decides what to do (the tree DnD shows a toast, the CLI prints an error).
  - **Source is the currently-open buffer** — `move_note` operates on disk only and doesn't touch the buffer. The buffer's `currentPath` keeps pointing at the old path; the next save will fail the drift check (file missing) and prompt the user. Acceptable for v1 — buffer-follows-rename can come later if it proves annoying.
  - **Source missing** — error.
  - **Target parent directory missing** — error rather than auto-create. Only reachable via CLI typo (`hiker mv a.md sub/dir/that/doesnt/exist/a.md`); UI drops are always onto an existing tree node.
  - **Folder drag** — moving a folder moves all contained notes recursively. Implementation: walk the folder, call `move_note` per file in a single transaction so the whole move succeeds or fails atomically. Empty subfolders move with the rename.

  ### Drop targets

  - Drop onto a **folder** → move into that folder.
  - Drop onto a **file** → move into the file's parent folder (treats the row as "this folder, near this file").
  - Drop onto **empty space below the tree** → move to vault root.
  - Drop onto the **same parent** → no-op (don't even call `move_note`).
  - Drop into a folder that contains a same-named file → error per the collision rule.

  ### Prerequisite

  `move_note` and `create_note` both perform writes the watcher would otherwise observe and re-enqueue as redundant index jobs (with a small race window where the watcher's rename pairing could disagree with the explicit move). The `watcher-suppress-self-writes` feature in `watcher.md` is a prerequisite — build it first so the explicit-mutation path can register a short-lived suppression set around its writes. `delete_note` (below) needs the same suppression.

  ### Tree interactions

  Beyond drag-and-drop and the toolbar buttons, the file tree supports two more interactions:

  - **Double-click on a tree row** → enters inline-rename mode for that note. Same UX as the post-create rename: the basename is pre-selected with the extension excluded, Enter submits via `move_note`, Esc cancels and reverts. Double-clicking a folder enters inline-rename for the folder name (recursive move under the hood — the same code path the folder-drag case uses). [tree-double-click-rename]
  - **Right-click on a tree row** → opens a context menu. v1 entries: [tree-context-menu]

    - **Open** — opens the note in the editor (same as a single click; included for discoverability and to give right-click a non-destructive default).
    - **Rename** — enters inline-rename mode (same as double-click).
    - **Delete** — calls `delete_note` after a confirm modal. Delete is *not* permanent: the file is moved into the vault's trash (see "Delete semantics" below). Modal text reflects this: "Move `<path>` to trash?" for files; "Move `<path>` and N notes inside it to trash?" for folders. Two buttons: Cancel (default focus) and Move to trash (red-ish, but not as alarming as a true delete). No "don't ask again" bypass — keep the friction since most people deleting a note from a tree mean to. [tree-context-delete]
    - **Properties** — opens a `properties`-kind tab for the note (per `tab-kinds` and the "Note properties tab" section below). Read-only inspector of every piece of data hiker stores about the note across `index.db` and `changes.db`. [tree-context-properties]

    Right-click on **empty space below the tree** opens a smaller menu with one entry — **New note here** — which is equivalent to clicking the toolbar's + New note while no folder is selected.

  ### Delete semantics

  Delete is a soft delete — the file is moved into a per-vault trash directory, not removed from disk. Restorable until the trash is emptied. This trades a small amount of disk overhead for a real safety net against the worst tree-action mistake (deleting the wrong note).

  `delete_note(rel: &str) -> Result<()>` lives in `core::vault` next to `create_note` and `move_note`. Order: suppress watcher events for the source path, fs rename into trash (collision-suffixed; see below), update store (`store::delete_note` cascades chunks + vec rows + path_ids per `index.md`) so the note stops appearing in search/related, append a metadata entry to the trash manifest so restore knows the original path, release suppression. [delete-note-core-cmd]

  **Trash location:** `vault/.hiker/trash/`. Per-vault rather than per-user so the safety net travels with the vault under Syncthing/git/etc., and so two vaults' deletions don't collide.

  **Trash naming:** when moving a file in, prefix the filename with the deletion timestamp to avoid collisions across multiple deletes of the same path: `vault/.hiker/trash/2026-05-06T14-22-31_myNote.md`. Folder deletes recreate the relative folder structure under a single timestamped root: `vault/.hiker/trash/2026-05-06T14-22-31_<foldername>/...`. Manifest at `vault/.hiker/trash/manifest.yaml` records each entry's original path, original mtime, deletion time, and a stable id for restore. [vault-trash]

  **Restore (`hiker trash restore <id|path>`)** — moves the file back to its original path via `move_note` (so the index re-picks it up cleanly). If the original path is now occupied, restore fails and the user picks a new target. [vault-trash-restore]

  **Empty (`hiker trash empty`)** — permanent deletion of all entries in the trash. Confirm prompt; this *is* the irrecoverable operation. No automatic emptying in v1 (no TTL, no size cap) — disk is cheap, surprise is expensive. Auto-empty policies can come later as a setting (`trash.retention_days`, `trash.max_size_mb`) when there's a real ask. [vault-trash-empty]

  Watcher must include `vault/.hiker/trash/` in its hard-coded ignore list (it's already covered by the existing `.hiker/` ignore in `watcher.md`, but worth noting explicitly because trash entries *are* `.md` files and a less-careful ignore would re-index them).

  Edge cases:

  - **Currently-open buffer** — moving the file out from under the buffer closes the buffer. The editor clears (or the next file in the tree opens, picked by an "open neighbor" rule); a non-blocking toast confirms the move and offers an Undo for ~5 seconds (Undo calls `hiker trash restore` for the entry just created — cheaper than re-typing the path). If the buffer is dirty, the modal copy adjusts: "Move `<path>` to trash? Unsaved changes will be discarded." Discard is real — the file in trash reflects what was on disk, not the dirty buffer state.
  - **Folder delete** — recursive. Walk the folder, move each file into the timestamped trash subtree preserving relative paths, then `std::fs::remove_dir_all` the now-empty source shell. Single transaction across all the store updates and a single manifest entry covers the whole folder, so restore can put the entire subtree back atomically.
  - **Source missing** — error. Same reasoning as the move case.
  - **Trash itself missing** — auto-create on first delete (`std::fs::create_dir_all`).
  - **Trash entry collision** — should be impossible thanks to the timestamp prefix, but if two deletes land in the same second on the same path the second one gets a `_2`, `_3`, ... suffix.
  - **CLI parity** — `hiker rm <path>` invokes the same core command. `--yes` skips the confirm prompt. `hiker trash list`, `hiker trash restore <id>`, `hiker trash empty` round out the CLI surface.

  ### Trash bin in tree

  The trash needs a visible surface or users will lose track of what they've deleted. v1 puts a pinned `🗑 Trash (N)` row at the **bottom** of the file tree, below the regular vault entries. The bottom position keeps the trash present-but-out-of-the-way: tree scrolling lands on real notes first, and the deletion surface doesn't compete for visual priority with the working set. Expand the row to see deleted notes; collapse it to make it disappear. `N` is the count of entries currently in the trash; `Trash` (no count) when empty. [tree-trash-bin]

  Headline decisions:

  - **Disk is the source of truth for what's in the bin.** The panel is built by walking `<vault>/.hiker/trash/` directly — every file there shows up. The manifest is consulted for *original path* and *deletion time* only, and only on a per-entry basis. Files dropped into `.hiker/trash/` by hand, or entries whose manifest row got corrupted, still appear and can still be emptied. The manifest is a hint, not a gate. [tree-trash-disk-listing]
  - **Flat list, sorted by deletion time descending.** No reconstruction of the original folder structure inside the bin. Trash is a recovery surface ("the thing I deleted ten minutes ago"), not a working tree. Each row shows the basename, a relative-time hint (`5m ago`, `yesterday`, `Mar 12`), and the original path as muted secondary text. Folder entries get a `▸` glyph and a `(N notes)` count derived from the manifest's `members` (or `?` if the entry is orphaned and we can't tell). [tree-trash-flat-by-deleted]
  - **Click → read-only preview.** Single click on a trash row opens the file in the editor in a non-editable mode (CodeMirror `EditorState.readOnly.of(true)` plus a banner across the top: "Trash preview · Restore to edit"). The buffer's `path` is set to the on-disk trash location, `loadedHash` is set, but `isDirty` is forced false and the save button hides. Switching away from a trash preview discards nothing — there's nothing to discard. [tree-trash-preview]
  - **Right-click → Restore / Delete permanently.** Per-row context menu has two entries. Restore calls `vault-trash-restore` and re-ingests the note (see below). Delete permanently removes that single entry from disk + manifest, with a confirm modal that says "Permanently delete `<original_path>`? This cannot be undone." Same `confirmDanger` modal pattern the soft-delete uses. [tree-trash-restore-action]
  - **Top-level right-click → Empty trash.** Right-clicking the `🗑 Trash` header itself opens a single-entry menu: "Empty trash (N entries)". Calls `vault-trash-empty` after the same `confirmDanger` modal. Disabled when `N == 0`. [tree-trash-empty-action]

  #### Restore semantics

  Restore is a `move_note` from the trash entry's on-disk location to its `original_path` (looked up from the manifest), followed by a re-ingest so search/related see it again. Because `move_note` already routes through the indexer's owned store connection and emits the correct watcher suppression, restore inherits that path for free — no separate code, no second writer. The store-side effect of restore is identical to a fresh import: a new ulid, fresh chunks, fresh embeddings. We do *not* try to preserve the pre-delete note id; chunk ids and the note id were freed by the original `delete_note` cascade and the v1 stable-id story doesn't extend across the trash boundary. Worth revisiting if/when MCP agents start pinning to chunk ids and a delete+restore round trip needs to look like a no-op.

  Edge cases:

  - **Original path now occupied** — restore fails with a clear message ("`<original_path>` already exists; rename it first or restore to a new location"). v1 doesn't offer an in-app target picker; the workaround is to rename the conflicting file in the tree, then retry restore. CLI has the same constraint per `vault-trash-restore`.
  - **Original parent directory missing** — auto-create on restore. Different from the explicit-mutation `move_note` rule (which errors on missing parent) because the user's intent here is unambiguous: put it back where it was. If the parent was itself deleted into the trash, a cascade restore is *not* attempted — the user restores the parent first. We surface this as the same "not found" error.
  - **Orphaned entry (no manifest row)** — restore is unavailable for that row; the menu entry is greyed with tooltip "No original location recorded — drag out of `.hiker/trash/` manually". Empty trash and Delete permanently still work. [tree-trash-orphan-recovery]
  - **Folder entry restore** — restores the entire trashed subtree to `original_path` via a recursive `move_note`-equivalent walk, then re-ingests every `.md` in the manifest's `members`. Single transaction across the store updates so search either sees all of it or none.

  #### Interactions and constraints

  - **No drag in or out of the trash row.** Restore is an explicit verb, not a DnD gesture. Dragging a regular tree note onto the trash header could plausibly be a delete shortcut, but the existing right-click → Delete plus the confirm modal already covers that path; adding a second route doubles the surface for accidents.
  - **Default state: collapsed.** First open of a vault shows the trash row collapsed regardless of count. Persistence of the expanded/collapsed state across launches is deferred to `settings.md`.
  - **Refresh.** The manual refresh button (`tree-refresh-manual`) re-walks the trash dir alongside the vault. The watcher's `.hiker/` ignore stays in place, so trash entries do not auto-refresh on filesystem events; this is intentional — trash is changed only by Hiker actions, and after each one the panel re-reads itself. If a user manually edits the trash dir, refresh picks it up.
  - **Index isolation.** Trash entries are never indexed, never appear in search/related, never count toward `Indexed (N notes)` in the status bar. Already covered by the watcher's `.hiker/` ignore and the walker's startup-scan path skipping `.hiker/`; restated here so future indexer changes don't accidentally include trash content.

  #### Out of scope (deferred)

  - In-app target picker for restore-into-occupied-path conflicts (CLI workaround is fine for v1).
  - Auto-empty policies (`trash.retention_days`, `trash.max_size_mb`) — same as the existing `vault-trash-empty` deferral.
  - Drag-out-of-trash to a specific tree location (would need a target-picker UX too; restore-to-original covers the common case).
  - Bulk select + restore/delete (multi-row selection isn't a v1 tree feature anywhere else).
  - Frontmatter-editing-aware preview (the preview is read-only; richer trash inspection waits for the properties tab landing on trash entries — see `note-properties-tab`).

  ### Tree-row index-state markers

  Beyond the dirty-suffix dot (`dirty-tree-dot`), each tree row reflects its file's index state with at most one small marker rendered as a suffix glyph (right of the filename, on the same side as the dirty dot). One marker per row, mutually exclusive across the three states. The two suffix glyphs use distinct DOM slots — the dirty dot is a `li::after` pseudo-element, the index marker is a child `.ix-marker` span — so a row can carry both ("dirty *and* queued") without colliding for the single `::after` slot. Indexed-and-clean — the common case — shows nothing on either, keeping the tree visually quiet.

  - **Unsupported** — hollow grey dot. The file's extension has no chunker (anything outside `.md`, `.markdown`, `.txt` in v1). Derivable client-side from the path; no index lookup needed. [tree-row-unsupported-marker]
  - **Skipped** — amber filled dot. The indexer attempted ingest and refused (>5MB sanity cap, UTF-8 decode failure, future: corrupted source). Reason string from the indexer (`"file too large"`, `"not UTF-8"`) shown in the row's `title=` tooltip. [tree-row-skipped-marker]
  - **Queued / mid-index** — pulsing accent dot. Transient; clears when the file's index job completes. Driven by `hiker:reindex-progress` events so no polling is needed. [tree-row-queued-marker]

  State is supplied by `cmd-file-index-state` (see `index.md`), called lazily for visible rows on render and refreshed in place when index events fire. Folders are never marked — too noisy. The status-bar-side mirror of these states is `status-bar-active-file-index-state` above.

  ### Tree source visibility

  The file tree shows regular vault notes by default. Other source categories — chat sessions, imported sessions from other agents, future categories — are hidden by default and opt in via per-category vault settings, each surfacing its category as a virtual top-level group in the tree.

  Mechanism: a small registry that names each visible-in-tree source category, the setting key that controls it, the on-disk path the category covers, and the group label. The registry is consulted by the tree renderer when assembling the top-level list; categories whose toggles are off skip the rendering pass but stay indexed and search-reachable. Each source-providing spec adds a row to the registry — `llm.md` adds the native-sessions row and the imported-sessions row; future categories (e.g., snapshots, derived files if they ever surface) plug in the same way. [tree-source-visibility-toggles]

  Default values for every category are `false` — tree starts quiet, the user opts each category in. Settings live under `vault.show_<category>_in_tree` and ride the existing eligibility model (per `settings-vault-config-toml`) so they persist per-vault. The settings UI gets a small "Tree visibility" group under the vault settings section listing every registered category as a checkbox. [tree-source-visibility-settings-ui]

  Search and related-notes are independent of these toggles — a category being hidden from the tree never removes it from search. The toggles are about navigation chrome, not data scoping. [tree-source-visibility-orthogonal-to-search]

  v1 categories at registry seeding:

  | Category           | Setting key                          | Path                              | Owning spec |
  | ------------------ | ------------------------------------ | --------------------------------- | ----------- |
  | Native sessions    | `vault.show_sessions_in_tree`        | `.hiker/sessions/`                | `llm.md` (`chat-session-show-in-tree-toggle`) |
  | Imported sessions  | `vault.show_imported_sessions_in_tree` | `.hiker/sessions/imported/`     | `llm.md` (`chat-session-imported-show-in-tree-toggle`) |

  Future categories slot in by adding a row, an eligibility entry, and a one-line render rule. No changes to the tree-rendering code beyond the registry pull.

- **Center**: editor pane with a thin toolbar strip across its top, then the editor below, then the existing status bar. Toolbar holds two toggle buttons — left button toggles the tree/sidebar, right button toggles the discovery panel. Both buttons are always visible; their pressed/unpressed state reflects whether the corresponding panel is open. The same toolbar hosts Save (floppy icon), the dirty-buffer Diff toggle (`editor-diff-vs-disk-toggle`), the View menu button (eye icon, see `## View options menu`), and the Mutations menu (wand, see `## Note-mutations menu`). Between the left toggle and the right-hand cluster sits a centered `#mode-controls` slot (between two flex spacers) that lights up with mode-specific icon buttons + a label — read-only preview modes (trash / snapshot / staging review) populate it with their verbs. See `## Mode controls slot` below. Empty when the buffer is in plain editing mode. Icons: [panel-toggle-buttons]
  - **Sidebar toggle icon.** A safe-dial / ship-wheel glyph (round with spokes) inside a rounded-square frame — riffs on the project's "vault" vocabulary. Distinct enough from generic file-tree icons that it doesn't read as just-another-folder. Tooltip "Toggle sidebar." [sidebar-toggle-icon]
  - **Discovery toggle icon.** A magnifying glass — the panel's primary surface is search-driven retrieval (per `search.md`). Tooltip "Toggle discovery panel." Naming aside (the panel hosts search results *and* related-notes *and* future surfaces), the magnifying glass is the most recognizable retrieval glyph users have. [discovery-toggle-icon]
- **Right**: related-notes panel. Collapsible. Renders `RelatedHit[]` from `related_notes(currentPath)`. Updated on file-open and on save (debounced 500ms per index.md). [related-notes-panel-ui]

Default state on first launch: tree open, related panel collapsed. Persistence of these toggles across launches is a settings concern (see settings.md) — for v1 the state lives in-memory only.

CSS: a 3-column grid where the side columns collapse to width 0 (or `display: none`) when toggled. Editor column is `1fr`; sides are fixed widths. Toolbar lives inside the editor column so the buttons sit where the user's eyes naturally are.

### Resizable side columns

Both side columns are user-resizable horizontally via a drag handle on the inner edge — the boundary between the sidebar and the editor, and the boundary between the editor and the discovery panel. Hovering the boundary swaps the cursor to the standard horizontal-resize affordance (`col-resize`); dragging adjusts that column's width live. The editor column stays `1fr` and absorbs the slack, so resizing one side never compresses the other side. [side-panel-resize]

Constraints:

- **Min / max widths.** Each side column has a min width (~160px for the sidebar so file names stay readable; ~220px for the discovery panel so search-result snippets don't wrap into uselessness) and a max width (~50% of the window so the editor column can't be squeezed to nothing). Drags clamp at the bounds; the cursor stays as `col-resize` so the user sees they've hit the limit.
- **Collapse interaction.** The toggle buttons (`panel-toggle-buttons`) still hide / show the column wholesale — collapse is `display: none`, not "drag width to 0." Re-opening restores the last user-set width.
- **Persistence.** The two widths persist per-vault via `settings-write-back` to `vault.sidebar_width` / `vault.discovery_width` (eligible-key set grows by two). Defaults match the existing fixed widths so users who never drag see no change.
- **Implementation.** Plain pointer-event drag on a 4-px-wide handle element absolutely positioned over the column's inner edge. No third-party splitter library; CM6 reflows on the editor column resize for free. The handle is purely visual on hover (subtle accent) — no persistent divider chrome, matches the rest of the UI's "quiet by default" treatment.

The same handle slot exists on both sides regardless of whether the discovery panel is currently showing search-results, related notes, or the chat surface (`chat-panel-pinned-bottom`) — width is a panel-level affordance, not a section-level one.


## Mode controls slot

The editor toolbar reserves a centered `#mode-controls` slot between two flex spacers. The slot is empty during normal editing; entering a read-only preview mode populates it with mode-specific icon-only buttons plus a short text label naming the mode. One slot, one render function (`renderModeControls`), per-mode populators. [editor-toolbar-mode-controls]

Why a single toolbar slot rather than per-mode banners:

- **Consistent visual language.** Icons match the rest of the toolbar palette (line-weight, sizing, hover). Per-mode banners would be a separate visual family that fights the surrounding chrome.
- **Less DOM and CSS.** No separate banner elements, no per-mode show/hide. Slot is `replaceChildren()`-rebuilt every transition. Idempotent.
- **Discoverable once.** "Label + icons in toolbar center = something special is going on" carries across snapshot / trash / staging / dirty-buffer-diff / future modes.

What lands in the slot:

- **Icon-only action buttons** for the mode's verbs: Diff toggle (see `editor-diff-vs-disk-toggle` below), Restore, Apply, Reject, Close — whichever the active mode exposes. Icons match the toolbar palette; pressed/unpressed states reflect toggle state for stateful icons. The mode qualifier that names which non-current version is in view sits in the status-bar left region's version dropdown closed-state label (see `status-bar-version-dropdown` above) so the toolbar stays compact and the user's eye finds the context in the same place it finds the file name.

`renderModeControls()` reads the current buffer state (`buffer.mode.kind`, `isDirty()`, etc.) and the diff-active flag and rebuilds the slot's children. Called on every transition that affects the slot — buffer swap, mode entry/exit, dirty toggling, diff on/off.

Per-mode populators live in `ui/src/main.ts` — `renderSnapshotControls(diffActive: bool)`, `renderTrashControls()`, `renderDirtyBufferControls(diffActive: bool)`, and (future) `renderStagingControls()`. Each appends label + icons; none mutates state directly. State changes go through the existing buffer/preview API.

### Dirty-buffer Diff toggle

A diff toggle lives in the editor toolbar (just right of Save). Greyed when the buffer is clean *and* no other diff source is selected (nothing to diff against). Click toggles the editor tab's `diff` mode against the current `DiffSource` (see `diff.md` `diff-as-mode` and `diff-source-enum`); the default source is `Disk(path)` — the live buffer vs. last-loaded content. The flip is non-destructive: the buffer's `current` is unchanged, decorations are layered on top; toggling off restores cursor + selection. **Right-click opens a source picker** — a small context menu offering: `Diff against on-disk`, `Show changes…` (submenu of recent `changes.db` rows for this path), and future sources (snapshot, another open buffer). Selecting a source switches the tab's `DiffSource` and turns diff mode on. [editor-diff-vs-disk-toggle, editor-show-changes-menu]

Why this lives here, not in some mutation-specific surface: any time the user wants to compare the buffer against some other version of itself — current vs. disk, current vs. an earlier snapshot, current vs. another open buffer — the same affordance covers it. One toggle, one source picker, every comparison.

Constraints:

- **Disabled when there's nothing to diff.** Buffer clean *and* `DiffSource` is `Disk(path)` *and* the path exists on disk → toggle is disabled with tooltip "No changes to show."
- **Newly-created buffer (file not on disk yet).** Toggle is disabled with tooltip "Save first to diff against disk." The source picker still works for non-disk sources (e.g. another open buffer).

### Show changes menu

The right-click context menu on the diff toggle (and the buffer's body, when no selection is active) carries a `Show changes…` entry whose submenu lists recent `changes.db` rows for the active buffer's path, newest first. Selecting a row sets the tab's `DiffSource = ChangesDb(change_id)` and turns diff mode on; the buffer's `current` text stays put, and `agent_base` (if any) is unaffected. [editor-show-changes-menu]

- **Submenu shape.** Up to 20 recent rows. Each row shows timestamp (relative + absolute on hover), op (`saved` / `agent-applied` / `restored` / `import` / etc.), and author. Final row: `Browse all… → ` opens the `home-detail { which: activity-row { path } }` tab filtered to this path (per `vault-home-recent-activity-detail`).
- **Per-hunk restore.** When the diff source is `ChangesDb(id)`, hunks carry a `Restore this hunk` overlay verb (owner `Snapshot` per `diff.md`'s `diff-layer-owner`). Restore writes the historical text for that hunk's range into `current` and lets the user save through the normal path. Full-snapshot restore stays on the row-level surface (`vault-home-recent-activity-detail`), unchanged.
- **No URI scheme.** The diff resolves directly through `core::changes::content_at(change_id)`; the editor crate doesn't go through a custom URI provider.


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

Sidebar-scoped icons (Files / Cluster trees / Trails switcher, `+` new note, `⋯` actions menu) live in the sidebar's top row, not the editor toolbar — see the Sidebar mode switcher section above.

Each entry is a checkable item — checkmark when active, click flips it, menu closes on click. State is in-memory only for v1. Persistence (per-vault, per-user, or both) is a `settings.md` concern when that surface lands; users will expect toggle state to survive a relaunch, so this menu is one of the first hooks the settings work picks up.

### v1 entries

- **Show chunk boundaries** — overlays the editor with a thin horizontal rule between chunks (pale reddish-orange — visible against prose without competing for attention) and the chunk index (`0`, `1`, `2`, ...) in the gutter at each chunk's start line. Backed by `cmd-chunks-for-path` (see `index.md`) which returns the active note's chunk bounds. Refreshes on save (debounced 500ms, same cadence as the related-notes panel). When the file isn't indexed (unsupported / skipped / queued per `cmd-file-index-state`), toggling on shows nothing and a faint hint in the gutter explains why. CodeMirror integration: a `StateField<DecorationSet>` plus a `gutter` extension; sits in its own slot in the CM6 extension order (after language, before keymap). [view-show-chunk-boundaries]

  This is genuinely a debugging-grade view of the chunker's output — useful while txt-ingest is hardening, and useful long after as a sanity check when chunker behavior changes.

- **Hide frontmatter** — visually collapse the leading `---\n…\n---\n` YAML block into a single placeholder line (`▸ frontmatter (N lines)`) without touching the file. Detection mirrors `core::frontmatter::split` exactly — the block must start at byte 0 with `---\n` and have a closing `---\n` line before any body content; an unterminated or non-leading block is ignored. CodeMirror integration: a `Decoration.replace({block: true})` over the byte range, recomputed off `state.doc` so edits inside or around the block update the placeholder line count immediately. Default off; persistence via `editor.hide_frontmatter` (`settings-section-editor`). Motivated by agent-stamped frontmatter (`mcp-tool-set-frontmatter`, `mcp-tool-apply-tag-remove-tag`) accumulating into a tall block that pushes the actual prose off screen — flipping this on lets the user read the body without manually scrolling past metadata that's already visible elsewhere (the activity widget, file detail views). [view-hide-frontmatter-toggle]

- **Intraline diff highlights** — augments the line-level red/green diff with character-level highlights inside paired delete/insert lines. Affects every consumer that calls `editor.renderDiff` (snapshot preview, dirty-buffer diff, write-note review). Default off; persistence via `editor.intraline_diff` (`settings-section-editor`). Flipping the toggle while a diff is currently displayed re-renders the active diff with the new style. Does *not* affect the patch-review agent-diff surface — that renders span-anchored hunks as widgets on the live doc and is governed by its own rules in `patch-review.md`. See `diff.md`'s "Diff style" section for the full rendering contract. [view-intraline-diff-toggle]

### Reserved entries (greyed in v1, enabled when their backing feature lands)

These appear in the menu now so the surface is predictable, but render greyed-out with a tooltip naming the dependency. Putting the slot up front is also a forcing function for designing each backing feature with the toggle in mind.

- **Live preview** — hide/show markdown syntax markers on cursor-out. Specced in `live-preview.md`; entry becomes live (default on) when that ships. [view-live-preview-toggle]
- **Render .txt as markdown** — session-scope override of `txt-render-as-markdown-default`. Greyed until `settings-vault-config-toml` lands and gives the per-vault default a real loader; see `txt-ingest.md`. Different scope from the per-note override that doc explicitly rejects — this one is "for the current app session, flip the vault default," no file mutation, no persistence in v1. [view-render-txt-as-markdown-toggle]
- **Word wrap** — session-scope override of `settings-section-editor`'s wrap default. [view-word-wrap-toggle]
- **Show whitespace** — toggles CM6's whitespace-rendering extension. [view-show-whitespace-toggle]
- **Highlight trailing whitespace** — paints a faint red background over runs of `' '` / `'\t'` that sit between the last non-blank character on a line and the line terminator. Independent of `view-show-whitespace-toggle` (which renders every whitespace glyph): this one only marks the trailing run and only as a background, so it's quiet enough to leave on for code but is noisy enough on prose / `.txt` notes that it must be opt-in. Default off; persisted per-vault. The decoration provider is `editor_view::trailing_whitespace_decorations`; gate the call in the buffer panel on this flag rather than baking it into the always-on decoration stack. [view-highlight-trailing-whitespace-toggle]
- **Show line numbers** — toggles the line-number gutter. [view-line-numbers-toggle]
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
3. On `TaskCompleted`: the result replaces the source buffer's content as a single CM6 transaction, the buffer's read-only flag clears, and the buffer becomes dirty. Works whether the source tab is the active one (dispatch through the live editor view) or a background tab (rewrite the tab's saved CM6 state in place via a transaction off the existing state, preserving history so Ctrl-Z reverts the whole replacement as one undo step on activation). The user reviews by reading the buffer; the dirty-buffer Diff toggle (`editor-diff-vs-disk-toggle`) flips the editor view to a line-level diff against on-disk content for explicit comparison. **Save** writes the mutated content through the regular save path (which handles `pre-write-drift-check` + appends a `'modified'` row to `core::changes`). **Ctrl-Z** reverts the mutation as a single undo step. If the user closed the source tab mid-flight (only possible from the explicit close path, since the tab is RO + pinned during the flight), the result is dropped silently — no toast, no held state. [note-mutation-applies-as-buffer-edit]
4. On `TaskFailed`, the buffer's read-only flag clears and a toast surfaces the error. No content change. On `TaskCancelled` (user cancels via the queue widget), the buffer's read-only flag clears, no content change, no toast.

[note-mutations-menu-task-shape]

**Changes-log lineage.** When a mutation lands on the buffer, hiker stashes a `pending_changes_metadata` field on the buffer carrying `{ mutation: "<mutation-kind>" }`. The next save consumes this stash: the resulting `'modified'` row's `metadata` field carries `mutation: "<kind>"` so the recent-activity widget and any future filter can identify mutation-derived edits. Subsequent saves don't carry the tag — it's a one-shot stamp on the save that accepts the mutation. [note-mutation-stash-changes-tag]

**The user can keep editing during in-flight only by *not* triggering RO** — but the buffer is RO, so they can read and scroll but can't type. Switching buffers is fine; the in-flight mutation locks only its source buffer. If the user closes the source buffer mid-flight, the task continues and the result lands via the toast in step 4.

### v1 mutation: Reformat as markdown

The first concrete mutation: reformat the active note's content as clean markdown. Useful for `.txt` files (per `txt-ingest.md`'s LLM-rewrite option) and for `.md` files whose markup has rotted (uneven heading levels, broken list nesting, inconsistent emphasis). [note-mutation-reformat-as-markdown]

Submits a task with `kind: NoteMutation { mutation: ReformatAsMarkdown, source_path }` and `payload` carrying the buffer's live text + the source extension. The prompt template lives at the user/vault prompt-store path `note_mutation_reformat_as_markdown.md` (per `llm-prompts-file-store`); the bundled default is registered in `core::prompts::bundled_defaults()`.

### Mutations-menu button states

- **Enabled** when the active buffer is an editable note (`buffer.mode.kind === "file"`) of an indexable extension (`.md` / `.markdown` / `.txt`) and has at least one byte of content.
- **Disabled** during read-only preview modes (trash / snapshot / staging review) — mutating from inside a review surface would be confusing. Tooltip explains why.
- **Disabled with "Mutation in progress…" tooltip** when there is an active or leased task whose `kind: NoteMutation { source_path }` matches the active buffer's path. The buffer is RO during this window for the same reason. Only one in-flight mutation per source path (`note-mutation-one-in-flight-per-path`).

- **Pending-background-mutation indicator.** When the active buffer has at least one pending background mutation job in the queue (any `NoteMutation`-kind task in non-terminal state whose `source_path` matches the buffer), the Mutations menu trigger renders a small pulsing accent-color dot in the corner of its icon. Same `@keyframes` pulse as `tree-row-queued-marker` so the visual vocabulary stays uniform. Distinct from the "Reformatting…" pill in `#mode-controls` (which names the single in-flight in-buffer mutation): the pill belongs to single-note in-buffer flight, the dot is the presence-of-any-pending indicator for the per-note menu and stays lit across multiple queued or batch-flight jobs (`note-mutation-batch-via-staging`). Driven by the same `hiker:queue-event` subscription the menu already maintains. [note-mutations-menu-pending-indicator]

When only one mutation entry is enabled (the v1 case), the popover still opens — clicks-to-action stay one shape so users learn it once. As more mutations land, they slot in alphabetically.

### Batch mutations

Single-note in-buffer is the right shape when the user is watching one note land. **Batch mutations** (e.g., "reformat every `.txt` in `inbox/`") fan out N tasks; the user can't watch N buffers at once. Batch results route through the **staging surface** (per `settings.md`'s staging review section). Results appear on the activity detail page's Pending filter with per-row [Accept] [Reject] and [Accept all], plus on the editor toolbar pill when the affected file is open.

Batch entry points are deferred to v2; they slot into:

- **A folder-context bulk action** invoked from the file tree (`note-mutation-batch-from-folder`, deferred).
- **A search-result bulk action** alongside the already-reserved `search-bulk-action-tag` / `search-bulk-action-move` (`note-mutation-batch-from-search`, deferred).
- **A CLI command** (`hiker mutate <kind> <glob>`, deferred).

All three converge on the same staging-driven flow; no batch-specific review surface. [note-mutation-batch-via-staging]


## Vault home page

When no note is open, the editor pane shows a vault home page in place of the CM6 editor — a lightweight overview of the vault rather than empty space. Default landing surface on vault open (assuming no auto-resume of last-open buffer); reappears when the user closes the active buffer without opening another. [vault-home-screen]

Three widgets, in this vertical order:

- **Vault stats.** Total notes, total chunks, breakdown by index state (indexed / queued / skipped / unsupported), maybe disk usage of the vault directory. Pulled cheaply from the existing index store via a single command. Live-updates via the existing `hiker:reindex-progress` events so the counts reflect ongoing work. [vault-home-stats-widget]
- **Recently modified.** Top N (default 10) notes by filesystem mtime. Reuses the mtime field on `DirEntryDto` (`tree-sort-options`); ordering is just `ORDER BY mtime DESC LIMIT N` against the store's notes rows. Each row shows basename + relative path + relative time ("2 hours ago"). Click → open in editor. [vault-home-recent-modified]
- **Recently accessed.** Top N notes by user-open time. Requires a new `last_accessed_at` column on the `notes` row, written from the open-file command path; same row shape and click behavior as recently-modified. [vault-home-recent-accessed]

The new column rides a small slug of its own since the tracking is independent infrastructure (later consumers could include search ranking, habits-of-association, an "activity" view, etc.):

- **Note access tracking.** Add `last_accessed_at INTEGER` to the `notes` row; bump the schema-version constant (same fail-loud + reindex contract as the existing `store-version-fail-loud` / schema bump pattern). Written when a file becomes the active buffer (open from tree, search-result click, recents click, etc.). Read by the recents widget and any future consumer. [note-access-tracking]

Refresh shape: the home page subscribes to `hiker:reindex-progress` for live stat updates and to `hiker:file-changed` for recent-modified updates. The recently-accessed list updates on each open without watcher involvement (the writer is hiker itself).

UI scope: minimal. Header with vault root path, three widgets stacked, no charts / graphs, no per-source-type breakdowns yet (those land when source-derived notes are real). A "New note here" button at the top is an obvious affordance to keep — same call as the sidebar's `sidebar-new-item-button`.

Out of scope for v1 of the home page: pinned/landmark notes, active-trail display, search shortcuts, discovery hints from clustering, recent-searches list, vocabulary stats, sync status. All slot in as additional widgets as their backing features land.

### Recent activity widget (lands with `core::changes`)

A fourth widget appears on the home page once `core::changes` (per `changes.md`) has any rows — i.e. as soon as any save / rename / delete has happened in this vault since the v3 schema bump. Hidden when the changelog is empty so a fresh post-upgrade vault doesn't show a confusing zero-count tile. [vault-home-recent-activity-widget]

Preview content (the home tile):

- Header: "Recent activity" + count of recent rows.
- Top 3–5 most recent change events: timestamp, path, op (created / modified / deleted / renamed), author class. Click → detail view (see below).
- Mixed-author by default — user saves and (when MCP lands) agent writes appear in the same stream. The widget is *not* agent-specific; the agent-activity use case is a filter preset within the same widget rather than a separate surface.

Refresh: subscribes to a new `hiker:changes-appended` event emitted whenever the indexer task appends a row to `core::changes`. Same shape as `hiker:reindex-progress`. Light debounce (a few hundred ms) so save bursts don't repaint per keystroke.


### Detail views

Vault home widget tiles support a drill-in pattern. **Click on a widget's tile or header → home view body swaps to a detail view for that widget.** No back button affordance within the home view itself — clicking the Home button in the top strip always returns to the home overview, regardless of whether you're in the overview or a detail view. Clicking a note row in any detail view exits home and opens the editor on that note (same shape as `openFile` already exits home view today). [vault-home-detail-views]

Detail views replace the home overview body, not the editor. `#editor-pane` has four states — editor, home overview, home detail, and the settings surface (`settings-pane-mode`, see `settings.md` `## Settings UI shell`).

Transitions:
- Home button toggles editor ↔ home overview.
- Widget-tile clicks: home overview → home detail.
- Gear (`vault-bar-settings-icon`) toggles editor ↔ settings.
- Back: Home button (→ overview), note-row click (→ editor), gear (→ editor).

Read-only review surfaces (trash, snapshot, staging review previews) are sub-modes of the editor state — they share the CM6 view, and the toolbar's `#mode-controls` slot lights up with mode-specific icon buttons + label (see `## Mode controls slot`). Where applicable, a Diff toggle in that slot flips between the consumer's content and the line-level diff (see `diff.md`). The dirty-buffer Diff toggle (`editor-diff-vs-disk-toggle`) lives in the editor toolbar instead — always visible alongside Save, greyed when no diff target applies.

Per-widget detail views, in roughly the order they earn their keep:

- **`vault-home-stats-detail`** — each Stats tile (Notes / Indexed / Chunks / Queued / Skipped) drills in to a list view:
    - **Notes** — full list of all notes, paginated, sortable by mtime / access / path.
    - **Indexed** — same shape, filtered to indexed-only.
    - **Chunks** — per-note chunk count, sortable; flags pathologies (notes with >100 chunks, notes with 0 chunks). Ties into the deferred `eval-sanity-stats` work — gives a real surface for spotting chunker pathology before the formal eval framework lands.
    - **Queued** — live list of notes currently in the indexer's pending set (`is_pending` per `cmd-file-index-state`). Updates on every `hiker:reindex-progress` event.
    - **Skipped** — list of skipped notes with their reasons (already tracked via `notes.skipped` + `notes.skip_reason`). Per-row "retry" affordance reroutes through `IndexJob::Upsert` with `force=true` so users can manually retry after fixing the underlying issue (file size, encoding).
- **`vault-home-recent-activity-detail`** — full list from `core::changes::recent`, all author classes. Mental model: **each row is a saved version of the file.** Row layout: op label · path · author · time-ago, plus a `current` badge on the most recent row per path and a `↩ restored` badge on rows that were themselves a Restore. Filter pills (author class) live in the header. [vault-home-recent-activity-detail]

    The interaction shape:

    - **Click a row** → opens that snapshot read-only in the editor. Reuses the same `readOnlyCompartment` + banner pattern as `tree-trash-preview`; the banner reads `Snapshot of <path> · <when> · <author> · <op>` with `[Restore this version]` and `[Close preview]` actions. Closing returns to the activity detail view.
    - **Per-row `[Restore this version]`** → for power-user single-click without previewing first. Hidden on the `current` row (restoring the current state is a tautology) and on `'deleted'` rows (no content blob to write).
    - **No separate "Open" button.** That was confusing in an earlier iteration — users expected "open" to show the historic state, not the live file. Click-the-row → snapshot preview is the only path; the live file is reached via the tree, search, or recently-modified.
    - **No separate "Rollback to before this" button.** That phrasing was confusing because the row IS the version (the content blob lives on the row), and "before this" implied off-by-one mental gymnastics. The `Restore this version` semantics are honest: what you click is what you get.

    Restore writes the row's `content_at(id)` blob back to disk via `vault.write_file_checked`, then appends a new `'modified'` row stamped `metadata.restored_from = id`. Command: `restore_snapshot`. The change-shaped flavor (`rollback_change`, walks `previous_content_for_path`) stays available for the agent-rollback consumer per `mcp.md` — both flavors coexist on the same log primitives, see `changes.md` "Rollback".

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

Browser-style multi-buffer tabs. Each tab represents one open buffer; click switches to it; × closes it (with the existing dirty-buffer save/discard/cancel modal). The strip fills the remaining horizontal space after the leading cluster + vault path. [editor-tab-strip]

- **Tab content.** Basename of the open buffer's path. When two open buffers share a basename, both render with a folder hint (`notes.md (research/)` vs `notes.md (inbox/)`). Tooltip on hover shows the full vault-relative path. [editor-tab-disambiguation]
- **Active vs inactive.** Active tab has a distinct background; inactive tabs are visually muted with × revealed on hover. The active tab is the one whose buffer the editor pane is currently showing. [editor-tab-active-state]
- **Dirty marker.** A small colored dot appears on dirty tabs in place of the default × glyph; on hover the dot becomes the close × so the user can still close from the dirty state (which fires the existing save/discard/cancel modal). [editor-tab-dirty-marker]
- **Overflow.** Browser pattern — tabs shrink to a minimum width before any overflow handling fires; once shrunk to minimum, the strip becomes horizontally scrollable with chevron buttons at each edge; a "more (N)" dropdown lists tabs that scrolled off. The active tab always stays visible (auto-scrolls into view on activation). [editor-tab-overflow]
- **Keybinds**, all reserved in `keybind-registry`:
    - `tab.close` = Cmd/Ctrl-W — close active tab.
    - `tab.next` = Cmd/Ctrl-Tab — cycle to next tab.
    - `tab.previous` = Cmd/Ctrl-Shift-Tab — cycle to previous tab.
    - `tab.jump-N` = Cmd/Ctrl-1 through Cmd/Ctrl-9 — jump to tab at that 1-indexed position; Cmd/Ctrl-9 jumps to the last tab regardless of count (browser convention).
    - Middle-click on any tab also closes (browser convention).
    [editor-tab-keybinds]
- **Right-click context menu.** Verbs: Close / Close others / Close all to the right / Reveal in tree. The reveal-in-tree action selects the tab's note in the file tree, expanding parent folders as needed. [editor-tab-context-menu]
- **No `+` button.** New notes have a clear home in the sidebar's `+ New note` affordance (and any future "new tab" verb in keybinds); duplicating it in the tab strip splits the surface for no gain.

### Tab strip behavior with the rest of the app

- **File-tree click on an already-open file** switches to its tab rather than reloading. Click on a not-yet-open file opens a new tab and switches to it. [multi-buffer-tree-click-switches-tab]
- **Search-result, recents, wikilink, and any other "open this note" entry point** behave the same: existing tab → switch; not yet open → new tab.
- **Mode-controls slot, View menu, Mutations menu, chat panel "active note" injection, navigation history** all operate on the active tab, no change.
- **In-flight-mutation RO** (`note-mutation-buffer-ro-while-in-flight`) applies to the source tab regardless of whether it's currently active. The dirty marker on a tab whose buffer is mid-mutation reads as a normal dirty dot; users learn the queue widget / inline indicator as the source-of-truth for "what's working in the background."

### Multi-buffer model

- **In-memory while the vault is open; tab state restores on next open.** The set of open buffers is in-memory state during a session — closes, switches, and dirty content all live in RAM. The autosave layer (`autosave.md`) round-trips a tab-state snapshot (open paths + active path + preview-slot path) to `.hiker/autosave/index.json`, so the next vault open silently reopens the same set of tabs. Per-buffer dirty content recovery rides the same store, prompting via the recovery modal. [multi-buffer-in-memory-only]
- **No max open count.** A user with 50 tabs gets browser-style overflow; that's a UX signal, not a system constraint.
- **No max retention timer.** Tabs stay until the user closes them.
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

Right-click → Properties on a tree row opens a `properties`-kind tab for that note. The tab is a read-only inspector of every piece of state hiker tracks for the note across `index.db` and `changes.db` — the answer to "what does hiker actually know about this file." Useful for debugging skip reasons, sanity-checking embedder version drift, auditing the change log without opening recent activity, and confirming trail / cluster membership. Frontmatter editing is **not** part of this tab; the in-place frontmatter editor (`tree-context-properties-frontmatter-editing`) is a separate future surface that will eventually layer in as a section once a frontmatter-editing primitive exists.

The headline decisions:

- **One properties tab per note path.** Opening Properties on a path that already has a properties tab open switches to it instead of spawning a duplicate — same shape as the file-tree click rule for buffer tabs. [note-properties-tab]
- **Read-only data view, no editor chrome.** The tab is non-buffer per `tab-kinds`, so the editor toolbar and bottom status bar hide on activation. The tab body owns its own header (note basename + relative path). No save button, no dirty marker, no preview-slot promotion path — clicking Properties from the tree always opens sticky (it's a directed action, like restore-from-trash). [note-properties-tab-no-editor-chrome]
- **App-page preview-slot rule still applies on open.** Properties tabs default-land in the preview slot — same rule as `home` / `queue` / `settings` (per `tab-kinds`). Clicking Properties on a second note replaces the preview; promotion paths are the standard ones (right-click "Keep open", drag, etc.). [note-properties-tab-preview-slot]
- **Live-refreshing.** The tab subscribes to the same event surfaces the rest of the UI rides — `hiker:reindex-progress` (notes-row / chunks data refreshes when a re-ingest finishes), `hiker:changes-appended` (changes-section refreshes on every new change row for this path), `hiker:file-changed` (mtime / size refresh on external edits). No manual refresh button; the data is always current. [note-properties-tab-live-refresh]

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

VSCode-style "preview" tab. Single-clicking a note from any browse-y entry point (tree, search results, related notes, recents, wikilink, chat note-link, `@`-mention click) opens it in a single shared preview slot rather than spawning a new tab; clicking the next note replaces the preview's contents in place. The user can browse through ten search hits without ending up with ten tabs. The preview tab promotes to a regular sticky tab the moment the user signals intent — by editing, by double-clicking the tab, by dragging it, or by picking "Keep open" from the tab context menu.

The headline decisions:

- **At most one preview tab exists at a time.** Opening another note while a preview tab is active replaces that preview's buffer in place — same tab slot, same tab DOM node, just different contents. The replacement is a single doc swap (same as today's tab-switch path), not a close-then-open. [editor-preview-tab]
- **Visual treatment is italic title only.** No different background, no border, no extra glyph — preview tabs render exactly like sticky tabs except the title text is italicized. Active vs inactive shading and the dirty marker rules are unchanged (preview tabs are never dirty — see promotion). The italic is the only visual signal because it's the only one users actually need: "this tab will go away if I open another file." [editor-preview-tab]
- **Every click-driven open-note callsite uses the preview slot by default.** File-tree click, search-result click, related-notes click, recents click, wikilink click (when wikilinks land), chat note-link click, `@`-mention click in the chat panel — all route through `openFile(rel, { preview: true })`. The set is uniform on purpose; carving exceptions per surface ("recents always sticky," "wikilinks always preview") would be a worse mental model than "click is preview, Mod-click is sticky." [editor-preview-tab-from-open-callsites]
- **Mod-click on any open-note callsite forces a sticky tab.** Skips the preview slot, opens directly into a new sticky tab. Mirrors the browser convention "Mod-click opens in new tab"; same gesture meaning here. Drag-from-tree (when that's a thing) is also implicitly sticky — drag intent is more directed than click intent. [editor-preview-tab-mod-click-sticky]
- **Promotion paths.** Edit the buffer, double-click the tab, drag the tab to reorder, or pick "Keep open" from the tab right-click menu. Save is implicit (saving requires dirty, which requires edit, which already promoted). Each promotion clears the italic and removes the tab from the preview slot; the tab keeps its position in the strip. **Edit-as-promotion is what makes preview tabs never dirty** — the moment the user types, the tab is sticky, so the existing dirty-buffer machinery (`file-switch-guard-dirty`, `autosave-close-no-modal`) doesn't need to know about preview tabs at all. [editor-preview-tab-promotion]

Behavior details:

- **Replacing a preview is not "closing" it.** No dirty guard fires (preview is never dirty), the tab DOM node persists, only the buffer behind it changes. The replaced buffer is dropped from `openBuffers` since it has no tab anymore.
- **Activating a preview tab from a different sticky tab** is a normal tab switch, not a re-open. The italic stays — the preview is still a preview until promoted.
- **Closing a preview tab** uses the same close path as any tab. No dirty guard fires (it's never dirty), the slot is empty afterward and the next click-open creates a fresh preview.
- **Keybinds.** No new keybinds. `tab.close`, `tab.next`, `tab.previous`, `tab.jump-N` all operate on the active tab regardless of preview state.
- **Tab right-click menu** gains one verb when the active tab is the preview: **Keep open** (promotes to sticky). Greyed when the tab is already sticky. The other verbs (Close / Close others / Close all to the right / Reveal in tree) are unchanged.
- **Bulk close verbs** treat the preview tab like any other tab — "Close others" closes the preview if it isn't the target.
- **No persistence across vault re-open.** Tabs are already in-memory only per `multi-buffer-in-memory-only`; preview state is too. Vault swap clears the preview slot along with everything else.
- **Tree double-click stays bound to inline rename** per `tree-double-click-rename`. Promoting via double-click on the *tree row* would conflict; the tab double-click covers the canonical VSCode gesture.
- **Programmatic opens skip preview.** Restore-from-trash, new-note creation, the right-click "Open" tree verb, and any other non-user-click path open sticky — these are directed actions, not browsing. The `openFile` parameter is `{ preview: false }` (or omitted) at those callsites.
- **Pending agent proposals route the open into review mode.** When `openFile(rel)` resolves a path with one or more pending staging proposals, the buffer lands in patch-review or write-note review per `note-open-routes-to-pending-review` (in `patch-review.md`). The preview-vs-sticky distinction is preserved; the review state rides on `buffer.mode`, not the tab kind.

Out of scope for this feature:

- **Pin a sticky tab to never auto-close.** Hiker has no auto-close behavior to begin with; pinning is a VSCode artifact of split-view + restore semantics that don't apply here.
- **Multiple preview slots.** Single slot is the point. Two preview tabs would lose the "click another to replace" mental model.
- **Hover-to-preview from the tree.** VSCode doesn't do this either; the click-is-preview rule already gives users a cheap way to peek.


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

- **Inside CodeMirror.** CM6 doesn't intercept horizontal trackpad scroll by default for content that isn't horizontally scrollable, so wheel events with `deltaX` bubble up to the pane handler naturally. If a markdown-source line is horizontally scrolled (rare for prose; possible in code blocks), the swipe should still trigger navigation when `deltaX` substantially exceeds the line's scrollable extent.
- **Inside scrollable detail-view lists.** Same shape — the list scrolls on `deltaY`, so horizontal swipes pass through.
- **Touchscreen devices.** v1 of this feature targets trackpads only. Touch swipe gestures via `touchstart`/`touchend` are a separate slug if the project ever ships a touchscreen-friendly variant.


### Dirty-buffer interaction

Back/forward navigation under multi-buffer doesn't need a dirty-buffer guard — the dirty buffer stays in its tab, the navigation just activates a different tab (or pane state). The save/discard/cancel modal only fires on explicit tab close + window close.

Closing the vault while history exists drops the entire stack — no warning, no save protection beyond what already gates vault swap.


### Out of scope (this feature)

- **Persisting history across restarts.** Browser-shaped feature: history is per-session.
- **Tab-style multi-buffer history.** Hiker is single-buffer in v1; if tabs ever land, each tab gets its own history stack.
- **Touchscreen swipe gestures.** Trackpad-only for v1.
- **Rich history menu (right-click → list of last N pages).** Browser-shaped polish, deferred.


## Extension load order (CM6)

Order matters in CM6 — earlier extensions take precedence for keymaps and overlap-able decorations. Canonical order: [cm6-extension-order]

1. `basicSetup` — gutters, history, default keymap
2. `EditorState.tabSize.of(2)`
3. `EditorView.lineWrapping`
4. language compartment (`markdown()`) — swappable later when we add other langs
5. `saveTracking` extension — updates dirty state, fires title-bar update
6. `keybinds.editorKeymap()` — our registry's editor-scope bindings
7. (future) `livePreview()` — syntax-marker hiding decorations
8. (future) `wikilinks()` — `[[id]]` parser extension + decorations
9. (future) `widgets()` — images, math, transclusions
10. theme

The `language` slot uses a `Compartment` so it can be reconfigured per-buffer without rebuilding the whole state (e.g. opening a `.json` sidecar would swap to JSON mode). Same pattern for `theme` later.

Editor instance is created once at startup and reused across buffer switches; switching files dispatches a doc-replacement transaction, never reconstructs the view. [cm6-editor-reuse]


## Out of scope (deferred)

- Live-preview decorations (syntax-marker hiding on cursor-out) — specced in `live-preview.md`
- Wikilink rendering and autocomplete
- Widget-based rendering (images, math, embeds, callouts)
- Multi-buffer / tabs / split panes
- Vim/Emacs keymaps
- User keybind overrides (the registry supports it; the loader is later)
- External-change watcher integration (v1)
