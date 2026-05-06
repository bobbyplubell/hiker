# Editor

CodeMirror 6 inside the Tauri webview. This document specs the editor surface itself — buffer model, save UX, keybinds, status bar, and the extension layout that future features (live preview, wikilinks, autocomplete) will slot into. Live-preview decorations and widget rendering are out of scope here; see design.md.


## Buffer model

One open buffer at a time in v0. The buffer is identified by its vault-relative path (`currentPath`). Switching files replaces the buffer's contents via a single `dispatch` that swaps the entire doc.

State tracked per buffer:

- `path` — vault-relative; null when no file is open
- `loadedHash` — hash (or full string) of the contents most recently read from / written to disk
- `isDirty` — derived: current doc text !== loadedHash

`isDirty` is the single source of truth for save state. Computed lazily from the editor doc and `loadedHash` — no separate "dirty flag" that can desync. Cleared by re-reads and successful writes; set implicitly by any edit.

Multi-buffer / tabs are deferred. The model above keeps single-buffer simple but generalizes — when tabs land, the same per-buffer state moves into a `Buffer[]` keyed by path, with the active buffer driving the editor view.


## Save UX

Save action: writes current doc to `currentPath` via the `write_file` core command. On success, updates `loadedHash` to the new doc text, which clears `isDirty`. On error, surfaces a non-blocking error toast and leaves the dirty state alone (so the user can retry).

Triggers (all funnel into the same save function):

- Mod-S keybind
- Save button in the status bar (visible always; disabled when no file is open or when not dirty)
- Future: autosave on idle / on blur (deferred — opt-in setting later)

Dirty indicator:

- Window title shows `• Hiker — <path>` when dirty, `Hiker — <path>` when clean.
- Status bar save button shows a filled-dot icon when dirty, empty when clean.
- Active file in the tree shows a small dot suffix when its buffer is dirty.

File-switch guard: clicking another file while the current buffer is dirty pops a confirm dialog with three options — Save & switch, Discard & switch, Cancel. Cancel keeps the current buffer active. The same guard applies to closing the window: a `before-close` listener on the Tauri window cancels the close if dirty and prompts; user choice (save / discard / cancel) decides whether the close proceeds.

External changes: two mechanisms, layered.

- Pre-write drift check (v0). Every save re-reads the file from disk and compares its hash to `loadedHash` before writing. Three outcomes:
    - match — write proceeds normally; on success `loadedHash` updates to the new doc text.
    - file missing — prompt: write anyway (re-creates) / cancel.
    - hash mismatch — conflict prompt: keep mine (overwrite, lose disk version) / take theirs (discard buffer, reload from disk) / open diff (deferred — falls back to keep/take in v0).

    This catches the common "I edited the file in vim while it was open in Hiker" case without needing a watcher.

- Watcher integration (v1). When the notify-based watcher lands with the indexer, it pushes file-change events to the frontend for the currently open file. Behavior:
    - buffer clean — silently reload from disk; `loadedHash` updates.
    - buffer dirty — same conflict prompt as above, but proactive (fired on the change event, not deferred to save time).

    The watcher reduces the window where the user can edit a stale buffer; the pre-write check remains as a final guard since the watcher can miss events (network filesystems, rapid changes, race between event and save).

The pre-write check and the watcher both reduce to the same conflict-resolution UI; only the trigger differs.


## Keybind registry

A single module owns all keybindings as a flat list. The registry is an introspection layer, not a translator — CM6's `keymap.of([...])` is the only sink in v0. Goals: discoverable (a help panel can enumerate `list()`), overridable (user config later), conflict-detectable.

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

Bottom strip across the editor pane only (not under the tree). Three regions:

- left: save button + dirty dot, current file **basename** (e.g. `note.md`), with the full vault-relative path in a `title=` tooltip on hover.
- center: index status label (v1+) — short text reflecting indexer state. Concretely: `Model loading…` while the embedder loads, `Indexing X/Y` while jobs flow (X = remaining queue depth, Y = total since last idle), `Indexed (N notes)` when idle, `Index error` (with last_error in title attribute) when the indexer reports a failure. Plain text, no icons in v1; styling can come later.
- right: line:col, word count, file type badge (`md`)

Why basename rather than full path: the file tree already shows location, the window title (`Hiker — <path>`) carries the disambiguation when needed, and full paths overflow the bar on deep vaults. Basename answers "what's open right now"; the tooltip + tree cover "where does it live." Once tabs land the per-tab basename label uses the same rule.

Click targets:

- save button → save action
- file basename → reveal the file in the system file explorer (Finder on macOS, File Explorer on Windows, default file manager on Linux). Implemented via Tauri's shell/opener API. Tracked as `status-bar-path-reveal`.
- line:col → opens a goto-line input (deferred; click is a no-op in v0)


### Sibling protection (overflow rule)

Every status-bar region — and any other horizontal toolbar / strip elsewhere in the app — must use `min-width: 0` and `flex-shrink: 1` so a long string in one region cannot push siblings off-screen. The basename + tooltip change above fixes the common case for the path region; the rule generalizes. Anywhere a region's content is user-derived (file names, error messages, status labels reflecting external state), the same `min-width: 0` + ellipsis combo applies. Tracked as `ui-no-sibling-pushout` so the rule has a slug to cite from CSS comments and code review.


## Layout (v1)

Three columns, both sides collapsible:

- **Left**: file tree (existing `#sidebar`). Collapsible. Supports drag-and-drop to move notes between folders — the drop calls a single core `move_note` command that does the fs rename and updates the index path in one step, so the move is recorded explicitly rather than being inferred from watcher events. Same code path is exposed as a `hiker mv` CLI command.

  Tree toolbar at the top of the sidebar: a wide **+ New note** button and a small refresh icon next to it. The asymmetry is the point — new-note is a frequent action, refresh is a sanity-check fallback.

  - **New note** creates a numbered `new-note-N.md` in the currently-selected folder (vault root if nothing's selected) via a `create_note(rel_path)` core command. `N` is the lowest positive integer that doesn't collide with an existing file in the target folder — `new-note-1.md` first, then `new-note-2.md`, and so on. The new file opens in the editor immediately, and the tree row enters inline-rename mode with the `new-note-N` basename pre-selected (extension excluded from selection so users can type a new name and hit Enter without re-typing `.md`). Submit renames via the same `move_note` path; Esc keeps the default name.
  - **Refresh** re-reads the directory and rebuilds the tree from disk. With the v1 watcher, the tree should mostly stay in sync on its own — refresh is a backstop for the watcher's known failure modes (notify queue overflow during big git checkouts, NFS/network filesystems, missed events) and for the "did I really just save that" sanity case. Auto-refresh from watcher events is a v2 add per `watcher.md`; the manual button stays even after that lands.

  ### API & edge cases

  Both `create_note` and `move_note` live in `core::vault` and are the single source of truth for creating and relocating notes — UI tree actions and CLI commands (`hiker new`, `hiker mv`) call them unchanged.

  - `create_note(rel: &str) -> Result<String>` — creates an empty file at `rel`, returns the actual path used (since auto-suffix may have changed it from the requested name). The button always passes a `new-note-N.md` candidate; the CLI passes the user's requested name verbatim and errors on collision rather than auto-suffixing (CLI behavior is explicit; UI behavior is forgiving).
  - `move_note(from: &str, to: &str) -> Result<()>` — atomic fs rename + index update. Order: suppress watcher events for both paths (see below), fs rename, update `notes.path` + `path_ids` in a single transaction, release suppression. If the index update fails the fs rename is rolled back (rename `to` → `from`) before returning the error.
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

  - **Double-click on a tree row** → enters inline-rename mode for that note. Same UX as the post-create rename: the basename is pre-selected with the extension excluded, Enter submits via `move_note`, Esc cancels and reverts. Double-clicking a folder enters inline-rename for the folder name (recursive move under the hood — the same code path the folder-drag case uses).
  - **Right-click on a tree row** → opens a context menu. v1 entries:

    - **Open** — opens the note in the editor (same as a single click; included for discoverability and to give right-click a non-destructive default).
    - **Rename** — enters inline-rename mode (same as double-click).
    - **Delete** — calls `delete_note` after a confirm modal. Delete is *not* permanent: the file is moved into the vault's trash (see "Delete semantics" below). Modal text reflects this: "Move `<path>` to trash?" for files; "Move `<path>` and N notes inside it to trash?" for folders. Two buttons: Cancel (default focus) and Move to trash (red-ish, but not as alarming as a true delete). No "don't ask again" bypass — keep the friction since most people deleting a note from a tree mean to.
    - **Properties** — deferred. Stubbed in the menu (greyed out) until frontmatter editing exists; the entry will eventually open a small panel showing the note's `hiker:` frontmatter, content_hash, indexed_at, etc. Tracked as `tree-context-properties`.

    Right-click on **empty space below the tree** opens a smaller menu with one entry — **New note here** — which is equivalent to clicking the toolbar's + New note while no folder is selected.

  ### Delete semantics

  Delete is a soft delete — the file is moved into a per-vault trash directory, not removed from disk. Restorable until the trash is emptied. This trades a small amount of disk overhead for a real safety net against the worst tree-action mistake (deleting the wrong note).

  `delete_note(rel: &str) -> Result<()>` lives in `core::vault` next to `create_note` and `move_note`. Order: suppress watcher events for the source path, fs rename into trash (collision-suffixed; see below), update store (`store::delete_note` cascades chunks + vec rows + path_ids per `index.md`) so the note stops appearing in search/related, append a metadata entry to the trash manifest so restore knows the original path, release suppression.

  **Trash location:** `vault/.hiker/trash/`. Per-vault rather than per-user so the safety net travels with the vault under Syncthing/git/etc., and so two vaults' deletions don't collide.

  **Trash naming:** when moving a file in, prefix the filename with the deletion timestamp to avoid collisions across multiple deletes of the same path: `vault/.hiker/trash/2026-05-06T14-22-31_myNote.md`. Folder deletes recreate the relative folder structure under a single timestamped root: `vault/.hiker/trash/2026-05-06T14-22-31_<foldername>/...`. Manifest at `vault/.hiker/trash/manifest.yaml` records each entry's original path, original mtime, deletion time, and a stable id for restore.

  **Restore (`hiker trash restore <id|path>`)** — moves the file back to its original path via `move_note` (so the index re-picks it up cleanly). If the original path is now occupied, restore fails and the user picks a new target.

  **Empty (`hiker trash empty`)** — permanent deletion of all entries in the trash. Confirm prompt; this *is* the irrecoverable operation. No automatic emptying in v1 (no TTL, no size cap) — disk is cheap, surprise is expensive. Auto-empty policies can come later as a setting (`trash.retention_days`, `trash.max_size_mb`) when there's a real ask.

  Watcher must include `vault/.hiker/trash/` in its hard-coded ignore list (it's already covered by the existing `.hiker/` ignore in `watcher.md`, but worth noting explicitly because trash entries *are* `.md` files and a less-careful ignore would re-index them).

  Edge cases:

  - **Currently-open buffer** — moving the file out from under the buffer closes the buffer. The editor clears (or the next file in the tree opens, picked by an "open neighbor" rule); a non-blocking toast confirms the move and offers an Undo for ~5 seconds (Undo calls `hiker trash restore` for the entry just created — cheaper than re-typing the path). If the buffer is dirty, the modal copy adjusts: "Move `<path>` to trash? Unsaved changes will be discarded." Discard is real — the file in trash reflects what was on disk, not the dirty buffer state.
  - **Folder delete** — recursive. Walk the folder, move each file into the timestamped trash subtree preserving relative paths, then `std::fs::remove_dir_all` the now-empty source shell. Single transaction across all the store updates and a single manifest entry covers the whole folder, so restore can put the entire subtree back atomically.
  - **Source missing** — error. Same reasoning as the move case.
  - **Trash itself missing** — auto-create on first delete (`std::fs::create_dir_all`).
  - **Trash entry collision** — should be impossible thanks to the timestamp prefix, but if two deletes land in the same second on the same path the second one gets a `_2`, `_3`, ... suffix.
  - **CLI parity** — `hiker rm <path>` invokes the same core command. `--yes` skips the confirm prompt. `hiker trash list`, `hiker trash restore <id>`, `hiker trash empty` round out the CLI surface.
- **Center**: editor pane with a thin toolbar strip across its top, then the editor below, then the existing status bar. Toolbar holds two toggle buttons (VSCode-style icons or simple labels) — left button toggles the tree, right button toggles the related panel. Both buttons are always visible; their pressed/unpressed state reflects whether the corresponding panel is open.
- **Right**: related-notes panel. Collapsible. Renders `RelatedHit[]` from `related_notes(currentPath)`. Updated on file-open and on save (debounced 500ms per index.md).

Default state on first launch: tree open, related panel collapsed. Persistence of these toggles across launches is a settings concern (see settings.md) — for v1 the state lives in-memory only.

CSS: a 3-column grid where the side columns collapse to width 0 (or `display: none`) when toggled. Editor column is `1fr`; sides are fixed widths. Toolbar lives inside the editor column so the buttons sit where the user's eyes naturally are.


## Extension load order (CM6)

Order matters in CM6 — earlier extensions take precedence for keymaps and overlap-able decorations. Canonical order:

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

Editor instance is created once at startup and reused across buffer switches; switching files dispatches a doc-replacement transaction, never reconstructs the view.


## Out of scope (deferred)

- Live-preview decorations (syntax-marker hiding on cursor-out)
- Wikilink rendering and autocomplete
- Widget-based rendering (images, math, embeds, callouts)
- Multi-buffer / tabs / split panes
- Autosave timer
- Vim/Emacs keymaps
- User keybind overrides (the registry supports it; the loader is later)
- External-change watcher integration (v1)
