# Editor

CodeMirror 6 inside the Tauri webview. This document specs the editor surface itself — buffer model, save UX, keybinds, status bar, and the extension layout that future features (live preview, wikilinks, autocomplete) will slot into. Live-preview decorations and widget rendering are out of scope here; see design.md.


## Buffer model

One open buffer at a time in v0. The buffer is identified by its vault-relative path (`currentPath`). Switching files replaces the buffer's contents via a single `dispatch` that swaps the entire doc.

State tracked per buffer:

- `path` — vault-relative; null when no file is open [buffer-path-tracking]
- `loadedHash` — hash (or full string) of the contents most recently read from / written to disk
- `isDirty` — derived: current doc text !== loadedHash

`isDirty` is the single source of truth for save state. Computed lazily from the editor doc and `loadedHash` — no separate "dirty flag" that can desync. Cleared by re-reads and successful writes; set implicitly by any edit. [buffer-dirty-derived]

Multi-buffer / tabs are deferred. The model above keeps single-buffer simple but generalizes — when tabs land, the same per-buffer state moves into a `Buffer[]` keyed by path, with the active buffer driving the editor view.


## Save UX

Save action: writes current doc to `currentPath` via the `write_file` core command. On success, updates `loadedHash` to the new doc text, which clears `isDirty`. On error, surfaces a non-blocking error toast and leaves the dirty state alone (so the user can retry).

Triggers (all funnel into the same save function):

- Mod-S keybind [save-keybind-mod-s]
- Save button in the status bar (visible always; disabled when no file is open or when not dirty) [save-button]
- Future: autosave on idle / on blur (deferred — opt-in setting later)

Dirty indicator:

- Window title shows `• Hiker — <path>` when dirty, `Hiker — <path>` when clean. [dirty-window-title]
- Status bar save button shows a filled-dot icon when dirty, empty when clean.
- Active file in the tree shows a small dot suffix when its buffer is dirty. [dirty-tree-dot]

File-switch guard: clicking another file while the current buffer is dirty pops a confirm dialog with three options — Save & switch, Discard & switch, Cancel. Cancel keeps the current buffer active. The same guard applies to closing the window: a `before-close` listener on the Tauri window cancels the close if dirty and prompts; user choice (save / discard / cancel) decides whether the close proceeds. [file-switch-guard-dirty, window-close-guard-dirty]

External changes: two mechanisms, layered.

- Pre-write drift check (v0). Every save re-reads the file from disk and compares its hash to `loadedHash` before writing. Three outcomes:
    - match — write proceeds normally; on success `loadedHash` updates to the new doc text.
    - file missing — prompt: write anyway (re-creates) / cancel.
    - hash mismatch — conflict prompt: keep mine (overwrite, lose disk version) / take theirs (discard buffer, reload from disk) / open diff (deferred — falls back to keep/take in v0).

    This catches the common "I edited the file in vim while it was open in Hiker" case without needing a watcher. [pre-write-drift-check, drift-conflict-modal]

- Watcher integration (v1). When the notify-based watcher lands with the indexer, it pushes file-change events to the frontend for the currently open file. Behavior:
    - buffer clean — silently reload from disk; `loadedHash` updates.
    - buffer dirty — same conflict prompt as above, but proactive (fired on the change event, not deferred to save time).

    The watcher reduces the window where the user can edit a stale buffer; the pre-write check remains as a final guard since the watcher can miss events (network filesystems, rapid changes, race between event and save).

The pre-write check and the watcher both reduce to the same conflict-resolution UI; only the trigger differs.


## Keybind registry

A single module owns all keybindings as a flat list. The registry is an introspection layer, not a translator — CM6's `keymap.of([...])` is the only sink in v0. Goals: discoverable (a help panel can enumerate `list()`), overridable (user config later), conflict-detectable. [keybind-registry]

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

- left: save button + dirty dot, current file **basename** (e.g. `note.md`), with the full vault-relative path in a `title=` tooltip on hover. [status-bar-path-basename-tooltip]
- center: index status label (v1+) — short text reflecting indexer state. Concretely: `Model loading…` while the embedder loads, `Indexing X/Y` while jobs flow (X = remaining queue depth, Y = total since last idle), `Indexed (N notes)` when idle, `Index error` (with last_error in title attribute) when the indexer reports a failure. Plain text, no icons in v1; styling can come later. [status-bar-index-label]

  When the *active buffer*'s file is in a non-indexed state (per `tauri-cmd-file-index-state` in `index.md`), the center label is replaced for that file's lifetime as the active buffer with a file-specific message: `Not indexed (unsupported filetype)` for unsupported extensions, `Skipped — <reason>` for skipped files (reason string straight from the indexer), `Queued for indexing` while the file's job is pending. Reverts to the aggregate label once the file becomes indexed (or another file opens). [status-bar-active-file-index-state]
- right: line:col, word count, file type badge (`md`)

Why basename rather than full path: the file tree already shows location, the window title (`Hiker — <path>`) carries the disambiguation when needed, and full paths overflow the bar on deep vaults. Basename answers "what's open right now"; the tooltip + tree cover "where does it live." Once tabs land the per-tab basename label uses the same rule.

Click targets:

- save button → save action
- file basename → reveal the file in the system file explorer (Finder on macOS, File Explorer on Windows, default file manager on Linux). Implemented via Tauri's shell/opener API. Tracked as `status-bar-path-reveal`. [status-bar-path-reveal]
- line:col → opens a goto-line input (deferred; click is a no-op in v0) [status-bar-goto-line]


### Sibling protection (overflow rule)

Every status-bar region — and any other horizontal toolbar / strip elsewhere in the app — must use `min-width: 0` and `flex-shrink: 1` so a long string in one region cannot push siblings off-screen. The basename + tooltip change above fixes the common case for the path region; the rule generalizes. Anywhere a region's content is user-derived (file names, error messages, status labels reflecting external state), the same `min-width: 0` + ellipsis combo applies. Tracked as `ui-no-sibling-pushout` so the rule has a slug to cite from CSS comments and code review. [ui-no-sibling-pushout]


## Layout (v1)

Three columns, both sides collapsible: [three-column-layout]

- **Left**: file tree (existing `#sidebar`). Collapsible. Supports drag-and-drop to move notes between folders — the drop calls a single core `move_note` command that does the fs rename and updates the index path in one step, so the move is recorded explicitly rather than being inferred from watcher events. Same code path is exposed as a `hiker mv` CLI command. [drag-and-drop-move]

  Tree toolbar at the top of the sidebar: a wide **+ New note** button and a small **`…`** actions menu next to it. The asymmetry is the point — new-note is a frequent action; the menu is the bucket for everything else. [tree-toolbar-actions-menu]

  - **New note** creates a numbered `new-note-N.md` in the currently-selected folder (vault root if nothing's selected) via a `create_note(rel_path)` core command. `N` is the lowest positive integer that doesn't collide with an existing file in the target folder — `new-note-1.md` first, then `new-note-2.md`, and so on. The new file opens in the editor immediately, and the tree row enters inline-rename mode with the `new-note-N` basename pre-selected (extension excluded from selection so users can type a new name and hit Enter without re-typing `.md`). Submit renames via the same `move_note` path; Esc keeps the default name. [create-note-button]
  - **`…` menu** opens a small popover with the v1 entries below. Adding new entries is intentionally low-friction — the menu is the catch-all for low-frequency tree-scoped actions, so future verbs slot in here rather than growing the toolbar.
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
    - **Properties** — deferred. Stubbed in the menu (greyed out) until frontmatter editing exists; the entry will eventually open a small panel showing the note's `hiker:` frontmatter, content_hash, indexed_at, etc. Tracked as `tree-context-properties`. [tree-context-properties]

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
  - Frontmatter-editing-aware preview (the preview is read-only; richer trash inspection waits for `tree-context-properties`).

  ### Tree-row index-state markers

  Beyond the dirty-suffix dot (`dirty-tree-dot`), each tree row reflects its file's index state with at most one small marker rendered as a suffix glyph (right of the filename, on the same side as the dirty dot). One marker per row, mutually exclusive across the three states. The two suffix glyphs use distinct DOM slots — the dirty dot is a `li::after` pseudo-element, the index marker is a child `.ix-marker` span — so a row can carry both ("dirty *and* queued") without colliding for the single `::after` slot. Indexed-and-clean — the common case — shows nothing on either, keeping the tree visually quiet.

  - **Unsupported** — hollow grey dot. The file's extension has no chunker (anything outside `.md`, `.markdown`, `.txt` in v1). Derivable client-side from the path; no index lookup needed. [tree-row-unsupported-marker]
  - **Skipped** — amber filled dot. The indexer attempted ingest and refused (>5MB sanity cap, UTF-8 decode failure, future: corrupted source). Reason string from the indexer (`"file too large"`, `"not UTF-8"`) shown in the row's `title=` tooltip. [tree-row-skipped-marker]
  - **Queued / mid-index** — pulsing accent dot. Transient; clears when the file's index job completes. Driven by `hiker:reindex-progress` events so no polling is needed. [tree-row-queued-marker]

  State is supplied by `tauri-cmd-file-index-state` (see `index.md`), called lazily for visible rows on render and refreshed in place when index events fire. Folders are never marked — too noisy. The status-bar-side mirror of these states is `status-bar-active-file-index-state` above.

- **Center**: editor pane with a thin toolbar strip across its top, then the editor below, then the existing status bar. Toolbar holds two toggle buttons — left button toggles the tree/sidebar, right button toggles the discovery panel. Both buttons are always visible; their pressed/unpressed state reflects whether the corresponding panel is open. The same toolbar hosts the View menu button (see `## View options menu`; eye-icon affordance per `view-menu-icon`) and reserves a slot for the deferred Mutations menu (see `note-mutations-menu` in "Out of scope" below). Icons: [panel-toggle-buttons]
  - **Sidebar toggle icon.** A safe-dial / ship-wheel glyph (round with spokes) inside a rounded-square frame — riffs on the project's "vault" vocabulary. Distinct enough from generic file-tree icons that it doesn't read as just-another-folder. Tooltip "Toggle sidebar." [sidebar-toggle-icon]
  - **Discovery toggle icon.** A magnifying glass — the panel's primary surface is search-driven retrieval (per `search.md`), so a search glyph is more honest than the generic circled-plus the panel previously used. Tooltip "Toggle discovery panel." Naming aside (the panel hosts search results *and* related-notes *and* future surfaces), the magnifying glass is the most recognizable retrieval glyph users have. [discovery-toggle-icon]
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


## View options menu

The editor pane's top toolbar (`panel-toggle-buttons`) gains a View menu button alongside the tree- and related-panel toggles. The menu hosts display-only toggles — flips that change how the active note is rendered without touching the file or the index. Sibling to the deferred `note-mutations-menu`; the split is clean: View changes pixels, Mutations changes bytes. [editor-view-options-menu]

**Icon.** Eye glyph, no text label, no chevron — matches the icon-only treatment of the other toolbar buttons (sidebar wheel, discovery magnifying glass). Tooltip "View options" handles discoverability for the icon-only form. The dropdown arrow that the previous `View ▾` text label implied isn't needed once the affordance is iconified — the click target opens the menu directly, same shape the other icon buttons use to open their popovers. [view-menu-icon]


### Toolbar icon palette

The editor pane's top toolbar is converging on icon-only affordances; each menu / button gets a single distinctive glyph in the same visual family (line-weight, frame, sizing). Reserved glyphs:

| Affordance                  | Glyph                                  | Slug                  | Status   |
| --------------------------- | -------------------------------------- | --------------------- | -------- |
| Sidebar toggle              | safe-dial / ship-wheel (rounded-square frame, circle with spokes) | `sidebar-toggle-icon` | landed   |
| Discovery toggle            | magnifying glass                       | `discovery-toggle-icon` | landed |
| View menu                   | eye                                    | `view-menu-icon`      | planned  |
| Mutations menu              | wand                                   | `mutations-menu-icon` | reserved (lands with `note-mutations-menu`) |
| Trails menu                 | squiggly trail (matches the file-tree trail-row icon seedling in `design.md`) | `trails-menu-icon`    | reserved (lands with the future trails UI) |
| Auto-tree-org (RAPTOR)      | tree                                   | `tree-org-menu-icon`  | reserved (lands with the auto-tree-org affordance from `clustering.md` / `suggestions.md`) |

The mutations/trails/tree-org rows are *icon reservations*, not full feature slugs — the parent UI surfaces (the menus themselves) are deferred to their respective feature specs. Pinning the glyph now keeps the visual language coherent so future spec writers don't have to relitigate the palette.

Each entry is a checkable item — checkmark when active, click flips it, menu closes on click. State is in-memory only for v1. Persistence (per-vault, per-user, or both) is a `settings.md` concern when that surface lands; users will expect toggle state to survive a relaunch, so this menu is one of the first hooks the settings work picks up.

### v1 entries

- **Show chunk boundaries** — overlays the editor with a thin horizontal rule between chunks (pale reddish-orange — visible against prose without competing for attention) and the chunk index (`0`, `1`, `2`, ...) in the gutter at each chunk's start line. Backed by `tauri-cmd-chunks-for-path` (see `index.md`) which returns the active note's chunk bounds. Refreshes on save (debounced 500ms, same cadence as the related-notes panel). When the file isn't indexed (unsupported / skipped / queued per `tauri-cmd-file-index-state`), toggling on shows nothing and a faint hint in the gutter explains why. CodeMirror integration: a `StateField<DecorationSet>` plus a `gutter` extension; sits in its own slot in the CM6 extension order (after language, before keymap). [view-show-chunk-boundaries]

  This is genuinely a debugging-grade view of the chunker's output — useful while txt-ingest is hardening, and useful long after as a sanity check when chunker behavior changes.

- **Hide frontmatter** — visually collapse the leading `---\n…\n---\n` YAML block into a single placeholder line (`▸ frontmatter (N lines)`) without touching the file. Detection mirrors `core::frontmatter::split` exactly — the block must start at byte 0 with `---\n` and have a closing `---\n` line before any body content; an unterminated or non-leading block is ignored. CodeMirror integration: a `Decoration.replace({block: true})` over the byte range, recomputed off `state.doc` so edits inside or around the block update the placeholder line count immediately. Default off; persistence via `editor.hide_frontmatter` (`settings-section-editor`). Motivated by agent-stamped frontmatter (`mcp-tool-set-frontmatter`, `mcp-tool-apply-tag-remove-tag`) accumulating into a tall block that pushes the actual prose off screen — flipping this on lets the user read the body without manually scrolling past metadata that's already visible elsewhere (the activity widget, file detail views). [view-hide-frontmatter-toggle]

### Reserved entries (greyed in v1, enabled when their backing feature lands)

These appear in the menu now so the surface is predictable, but render greyed-out with a tooltip naming the dependency. Putting the slot up front is also a forcing function for designing each backing feature with the toggle in mind.

- **Live preview** — hide/show markdown syntax markers on cursor-out. Specced in `live-preview.md`; entry becomes live (default on) when that ships. [view-live-preview-toggle]
- **Render .txt as markdown** — session-scope override of `txt-render-as-markdown-default`. Greyed until `settings-vault-config-toml` lands and gives the per-vault default a real loader; see `txt-ingest.md`. Different scope from the per-note override that doc explicitly rejects — this one is "for the current app session, flip the vault default," no file mutation, no persistence in v1. [view-render-txt-as-markdown-toggle]
- **Word wrap** — session-scope override of `settings-section-editor`'s wrap default. [view-word-wrap-toggle]
- **Show whitespace** — toggles CM6's whitespace-rendering extension. [view-show-whitespace-toggle]
- **Show line numbers** — toggles the line-number gutter. [view-line-numbers-toggle]
- **Show heading breadcrumb** — overlays each chunk with its `heading_path` (already stored on chunks). Pairs with chunk boundaries; defer until both have a real user. [view-heading-breadcrumb-toggle]

### Out of scope (this menu)

- Content-mutating actions — those live in `note-mutations-menu`.
- Per-file scoped toggles. The menu's scope is "active buffer at most"; per-file persistence is a frontmatter concern that doesn't exist in v1.
- Theme / font / color-scheme — those belong in settings, not a quick toggle.


## Vault home page

When no note is open, the editor pane shows a vault home page in place of the CM6 editor — a lightweight overview of the vault rather than empty space. Default landing surface on vault open (assuming no auto-resume of last-open buffer); reappears when the user closes the active buffer without opening another. [vault-home-screen]

Three widgets, in this vertical order:

- **Vault stats.** Total notes, total chunks, breakdown by index state (indexed / queued / skipped / unsupported), maybe disk usage of the vault directory. Pulled cheaply from the existing index store via a single Tauri command. Live-updates via the existing `hiker:reindex-progress` events so the counts reflect ongoing work. [vault-home-stats-widget]
- **Recently modified.** Top N (default 10) notes by filesystem mtime. Reuses the mtime field on `DirEntryDto` (`tree-sort-options`); ordering is just `ORDER BY mtime DESC LIMIT N` against the store's notes rows. Each row shows basename + relative path + relative time ("2 hours ago"). Click → open in editor. [vault-home-recent-modified]
- **Recently accessed.** Top N notes by user-open time. Requires a new `last_accessed_at` column on the `notes` row, written from the open-file Tauri command path; same row shape and click behavior as recently-modified. [vault-home-recent-accessed]

The new column rides a small slug of its own since the tracking is independent infrastructure (later consumers could include search ranking, habits-of-association, an "activity" view, etc.):

- **Note access tracking.** Add `last_accessed_at INTEGER` to the `notes` row; bump the schema-version constant (same fail-loud + reindex contract as the existing `store-version-fail-loud` / schema bump pattern). Written when a file becomes the active buffer (open from tree, search-result click, recents click, etc.). Read by the recents widget and any future consumer. [note-access-tracking]

Refresh shape: the home page subscribes to `hiker:reindex-progress` for live stat updates and to `hiker:file-changed` for recent-modified updates. The recently-accessed list updates on each open without watcher involvement (the writer is hiker itself).

UI scope: minimal. Header with vault root path, three widgets stacked, no charts / graphs, no per-source-type breakdowns yet (those land when source-derived notes are real). A "New note here" button at the top is an obvious affordance to keep — same Tauri call as the tree's existing `create-note-button`.

Out of scope for v1 of the home page: pinned/landmark notes, active-trail display, search shortcuts, discovery hints from clustering, recent-searches list, vocabulary stats, sync status. All slot in as additional widgets as their backing features land.

### Recent activity widget (lands with `core::changes`)

A fourth widget appears on the home page once `core::changes` (per `changes.md`) has any rows — i.e. as soon as any save / rename / delete has happened in this vault since the v3 schema bump. Hidden when the changelog is empty so a fresh post-upgrade vault doesn't show a confusing zero-count tile. [vault-home-recent-activity-widget]

Preview content (the home tile):

- Header: "Recent activity" + count of recent rows.
- Top 3–5 most recent change events: timestamp, path, op (created / modified / deleted / renamed), author class. Click → detail view (see below).
- Mixed-author by default — user saves and (when MCP lands) agent writes appear in the same stream. The widget is *not* agent-specific; the agent-activity use case is a filter preset within the same widget rather than a separate surface.

Refresh: subscribes to a new `hiker:changes-appended` event emitted whenever the indexer task appends a row to `core::changes`. Same shape as `hiker:reindex-progress`. Light debounce (a few hundred ms) so save bursts don't repaint per keystroke.


### Detail views

Vault home widget tiles support a drill-in pattern. **Click on a widget's tile or header → home view body swaps to a detail view for that widget.** No back button affordance — clicking the Home button in the vault bar always returns to the home overview, regardless of whether you're in the overview or a detail view. Clicking a note row in any detail view exits home and opens the editor on that note (same shape as `openFile` already exits home view today). [vault-home-detail-views]

Detail views replace the home overview body, not the editor. Same pane-mode framing: `#editor-pane` has four states — editor, home overview, home detail, and the diff viewer (`diff-viewer-pane`, see `diff.md`). The Home button toggles between editor and home overview; widget-tile clicks transition home overview → home detail; the diff viewer is entered from a snapshot preview's "Show diff vs current" action, from the note-mutation accept/decline flow, or from a future drift-conflict review; back transitions are via Home button (→ overview), note-row click (→ editor), or the diff viewer's Close button (→ wherever the user came from).

Per-widget detail views, in roughly the order they earn their keep:

- **`vault-home-stats-detail`** — each Stats tile (Notes / Indexed / Chunks / Queued / Skipped) drills in to a list view:
    - **Notes** — full list of all notes, paginated, sortable by mtime / access / path.
    - **Indexed** — same shape, filtered to indexed-only.
    - **Chunks** — per-note chunk count, sortable; flags pathologies (notes with >100 chunks, notes with 0 chunks). Ties into the deferred `eval-sanity-stats` work — gives a real surface for spotting chunker pathology before the formal eval framework lands.
    - **Queued** — live list of notes currently in the indexer's pending set (`is_pending` per `tauri-cmd-file-index-state`). Updates on every `hiker:reindex-progress` event.
    - **Skipped** — list of skipped notes with their reasons (already tracked via `notes.skipped` + `notes.skip_reason`). Per-row "retry" affordance reroutes through `IndexJob::Upsert` with `force=true` so users can manually retry after fixing the underlying issue (file size, encoding).
- **`vault-home-recent-activity-detail`** — full list from `core::changes::recent`, all author classes. Mental model: **each row is a saved version of the file.** Row layout: op label · path · author · time-ago, plus a `current` badge on the most recent row per path and a `↩ restored` badge on rows that were themselves a Restore. Filter pills (author class) live in the header. [vault-home-recent-activity-detail]

    The interaction shape:

    - **Click a row** → opens that snapshot read-only in the editor. Reuses the same `readOnlyCompartment` + banner pattern as `tree-trash-preview`; the banner reads `Snapshot of <path> · <when> · <author> · <op>` with `[Restore this version]` and `[Close preview]` actions. Closing returns to the activity detail view.
    - **Per-row `[Restore this version]`** → for power-user single-click without previewing first. Hidden on the `current` row (restoring the current state is a tautology) and on `'deleted'` rows (no content blob to write).
    - **No separate "Open" button.** That was confusing in an earlier iteration — users expected "open" to show the historic state, not the live file. Click-the-row → snapshot preview is the only path; the live file is reached via the tree, search, or recently-modified.
    - **No separate "Rollback to before this" button.** That phrasing was confusing because the row IS the version (the content blob lives on the row), and "before this" implied off-by-one mental gymnastics. The `Restore this version` semantics are honest: what you click is what you get.

    Restore writes the row's `content_at(id)` blob back to disk via `vault.write_file_checked`, then appends a new `'modified'` row stamped `metadata.restored_from = id`. Tauri command: `restore_snapshot`. The change-shaped flavor (`rollback_change`, walks `previous_content_for_path`) stays available for the agent-rollback consumer per `mcp.md` — both flavors coexist on the same log primitives, see `changes.md` "Rollback".

    - **Author-filter pills** — one pill per present author class. Default: all classes pressed (everything visible). User can flip filters; state persists per-vault. Pills only appear when their class has at least one row in the visible window. [vault-home-recent-activity-author-filter]
    - **`recent-activity-human-icon`** — human glyph (half-oval body + circle head) for the `user` filter pill. Same icon-only style as the editor toolbar palette. Tooltip "Show user activity." [recent-activity-human-icon]
    - **`recent-activity-agent-icon`** — simplified retro-robot glyph for the `agent:*` filter pill. Tooltip "Show agent activity." Future author classes (sync, import) get their own glyphs in the same family when they land. [recent-activity-agent-icon]
    - **Un-rollback affordance** — append-only log + per-row content blob means *every* prior state stays restorable, including states that were themselves the result of a Restore. Mechanically, "un-rollback" is just Restore on a more recent prior version — same primitive, no separate operation. UX: rows tagged `metadata.restored_from` show a `↩ restored` badge; immediately after a Restore action, the row that *was* the current state for that path gets a soft highlight + "← previous state — click Restore to undo" caption. The action is the regular `[Restore this version]` button on that row (no separate primitive); the caption is purely a hint. This is materially better than linear undo stacks where redo state vanishes after a subsequent edit; here, every row within retention is equally accessible as a Restore target. [vault-home-recent-activity-unrollback]
    - **Snapshot read-only preview.** Reuses the trash-preview machinery: `setReadOnly(true, "snapshot")` swaps in the snapshot banner, suppresses the save button + dirty marker, and the dirty-switch guard treats it the same as a trash preview (nothing to discard). The buffer carries `snapshotPreview: true` and `snapshotChangeId` so the banner's Restore action can write back without a re-lookup. Different banner color from trash (amber, not red) — informational, not a recovery surface. [snapshot-preview-mode]
- **`vault-home-recents-detail`** (lower priority — lands when needed) — full list versions of Recently Modified / Recently Accessed. Less urgent than the stats and agent-activity ones since each preview row already has a click-to-open affordance; the detail view adds filtering / longer history but isn't load-bearing.

Detail views don't get individual stub-slugs for each Stats subview (Notes / Indexed / Chunks / Queued / Skipped) — they're variations of the same `vault-home-stats-detail` slug parameterized by which tile launched them. Adding new tiles in the future just adds parameter values, not new slugs. [vault-home-stats-detail]

UI shape notes:

- Detail view header: tile name (e.g. "Skipped notes") + count.
- Body: paginated list, virtualized if needed (skipped/indexed lists could be thousands of rows on a large vault).
- Empty state: a brief "no items" message, since every detail view has a sensible empty case.
- Sort/filter affordances live in the detail view header, not the home overview tile.

### Vault bar affordances

Two small icon-only buttons live in the vault bar (the strip showing the current vault path). Both are vault-scoped, sit alongside the existing vault path display, and use icon-only styling for compactness; both carry `title=` tooltips so the icons remain discoverable.

- **Home button.** Icon-only (house glyph). Toggles the editor pane to the vault home page (described above). View toggle, not buffer close — the active buffer (if any) stays in memory; clicking any tree row, recents entry, or search result restores the editor onto whichever note. No save protection needed because nothing is closing. Tooltip "Vault home." Reserves the keybind id `vault.go-home` in `keybind-registry` (chord TBD; Cmd/Ctrl-Shift-H is unclaimed and pairs naturally with Ctrl-Space / Ctrl-Shift-F's "vault-level navigation" naming). [vault-home-button]
- **Open-vault button.** Replaces the existing "Open vault" text button with an icon (folder glyph). Same JS-dialog → `open_vault_at` flow per `settings.md`'s default-vault-autoopen story; this slug is purely the visual swap. Tooltip "Open vault…" preserves discoverability. [vault-bar-open-vault-icon]


## Navigation (back / forward)

Browser-style back/forward navigation across editor-pane states. Each user-initiated transition between distinct content surfaces — opening a note, going home, drilling into a home detail view, opening a trash preview — pushes onto a per-vault history stack. Back and forward navigate that stack via vault-bar buttons, trackpad two-finger horizontal swipe (matching browser convention), and a keybind registry entry.

The headline decisions:

- **History is a per-vault in-memory stack of editor-pane content states.** Cleared on vault swap. Not persisted across hiker restarts (matches browser per-window behavior). [navigation-history-stack]
- **Back and forward buttons live in the vault bar, pinned to the right** (trailing edge, after the vault path display). Icon-only, disabled when no history exists in that direction. [vault-bar-back-button, vault-bar-forward-button]
- **Two-finger horizontal trackpad swipe** triggers back/forward. Same UX as macOS Safari / Chrome / Firefox. Detection via wheel events with sustained `deltaX` past a threshold; right-swipe = back, left-swipe = forward (matches browser convention). [navigation-trackpad-swipe]
- **Keybind registry entries** reserve `navigation.back` and `navigation.forward` with platform-conventional chords: Cmd/Ctrl-[ for back, Cmd/Ctrl-] for forward; Alt-Left/Right as additional bindings on Linux/Windows for browser-keyboard parity. [navigation-keybind]
- **Dirty-buffer protection** integrates with the existing `file-switch-guard-dirty` modal — navigating back/forward into a different note from a dirty buffer fires the same Keep/Discard/Cancel modal save-on-switch already uses. [navigation-dirty-buffer-guard]


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


### Vault bar layout

The existing vault-bar order is preserved: Home button, Open-vault button, vault path display (left to right). Back and forward append at the trailing edge — after the vault path, pinned to the right. Browser convention puts navigation arrows leftmost, but the vault bar already has Home / Open-vault at that edge as vault-management controls, and pushing those right to make room for browser-style back/forward would shuffle muscle memory for affordances that are more frequently used. Trailing-edge placement keeps the existing left-edge cluster intact while still putting back/forward in a recognizable, dedicated zone.

Back/forward icon style matches the existing icon-only toolbar treatment (sidebar wheel, discovery magnifying glass, view eye, vault home button, etc.) — minimal arrow glyphs, tooltips, `aria-label`s. Disabled state styling for "no history that direction" should be visibly inert (greyed, no hover effect).


### Trackpad swipe shape

Browser convention: two-finger horizontal swipe on a trackpad triggers back/forward. macOS surfaces this as `wheel` events with `deltaX` accumulation; the editor pane's wheel handler watches for sustained horizontal scroll past a threshold (e.g. ~120px of accumulated `deltaX` over a short time window) and fires the navigation. Vertical swipes are ignored.

Optional polish (defer for v1 of the feature; nice-to-have): a small "←" or "→" overlay animation that previews the navigation while the swipe is in progress, cancels if the user reverses before the threshold. Not load-bearing.

Right-swipe = back. Left-swipe = forward. Same as every browser.

Edge cases worth pinning:

- **Inside CodeMirror.** CM6 doesn't intercept horizontal trackpad scroll by default for content that isn't horizontally scrollable, so wheel events with `deltaX` bubble up to the pane handler naturally. If a markdown-source line is horizontally scrolled (rare for prose; possible in code blocks), the swipe should still trigger navigation when `deltaX` substantially exceeds the line's scrollable extent.
- **Inside scrollable detail-view lists.** Same shape — the list scrolls on `deltaY`, so horizontal swipes pass through.
- **Touchscreen devices.** v1 of this feature targets trackpads only. Touch swipe gestures via `touchstart`/`touchend` are a separate slug if the project ever ships a touchscreen-friendly variant.


### Dirty-buffer interaction

Navigating away from a dirty buffer via back/forward fires the existing `file-switch-guard-dirty` modal (Keep / Discard / Cancel). Cancel aborts the navigation, history isn't mutated. Keep saves and proceeds. Discard reverts and proceeds. Same UX as switching files via the tree today.

Closing the vault while history exists drops the entire stack — no warning, no save protection beyond what already gates vault swap.


### Out of scope (this feature)

- **Persisting history across restarts.** Browser-shaped feature: history is per-session.
- **Tab-style multi-buffer history.** Hiker is single-buffer in v1; if tabs ever land, each tab gets its own history stack.
- **Visual swipe overlay animation.** Optional polish, deferred until the basic mechanism is real.
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
- Autosave timer
- Vim/Emacs keymaps
- User keybind overrides (the registry supports it; the loader is later)
- External-change watcher integration (v1)
- **Note-mutations menu** — a top-bar button on the editor pane hosting content-mutation actions on the active note. First candidate is markdown reformat (per `txt-ingest.md`'s deferred LLM-rewrite option) for `.txt` and messy `.md` content. Output goes to `.hiker/derived/<rel-path>.md` per the never-mutate-source rule; the source file is never touched until the user accepts. Other content-mutation actions slot into the same menu as they're specced. Not in v1; recorded here so the surface is reserved. **Routing (per `llm.md`):** this menu's actions are single-shot deterministic prompts — one click, one prompt, one derived file — so they use `core::llm` direct, *not* `core::agent` or `core::acp`. Provider (Ollama for local, OpenAI / Anthropic / etc. for cloud) is the user's `[llm]` config; no per-feature backend selection in this menu, no in-process model runtime. **Review surface (per `diff.md`):** when a mutation completes, the editor pane swaps to the diff viewer (`diff-viewer-pane`) with the source on the left and the derived output on the right. Two banner actions — Replace original (`note-mutation-replace-original`, drift-checked write to source + activity row + derived deletion) and Discard derived (`note-mutation-discard-derived`, no activity row, derived deleted). Diff is the only review surface in v1; the derived file isn't otherwise exposed as a buffer. [note-mutations-menu]
