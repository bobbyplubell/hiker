# Files

Hiker's file tree — the vault's notes browser — is the content of an `egui_workbench` primary-side-bar **Files** activity. The side-bar / accordion mechanics (sections, headers, collapse, resize, drag-to-add, persistence) belong to `egui_workbench` (`egui-workbench/SPEC.md`); this doc owns hiker's tree content: what the rows are, how they're created / moved / renamed / deleted, the trash, the per-row index-state markers, source visibility, and note multi-select.

Headline decisions:

- **`core::vault` owns the verbs.** `create_note` / `move_note` / `delete_note` (+ restore) are the single source of truth for note lifecycle — the tree's UI actions and the CLI (`hiker new` / `mv` / `rm` / `trash …`) call the same core commands.
- **Delete is soft.** Deleting a note moves it into a per-vault trash directory; restorable until the trash is emptied.
- **Explicit mutation, not inferred.** Tree moves/creates/deletes register a watcher suppression around their own writes so they aren't re-enqueued as redundant index jobs.
- **Drag-and-drop moves; double-click renames; right-click is the verb menu.**
- **Persistent multi-note selection** drives bulk verbs and feeds clustering / cluster-tree authoring.


## File tree

In Files mode the side bar hosts the file tree, including drag-and-drop note moves — the drop calls a single core `move_note` command that does the fs rename and updates the index path in one step, so the move is recorded explicitly rather than being inferred from watcher events. Same code path is exposed as a `hiker mv` CLI command. [drag-and-drop-move]

### Files-mode header actions

The Files panel header carries the `+` (new item) and `⋯` (actions menu) affordances. No labels — icon-only.

- **`+` is the new-item button.** Left-click creates the active surface's primary item: a note in the Files panel. Right-click opens a popover that lets the user pick any item type regardless of current surface (New note / New cluster tree / New trail), so creating a trail while browsing files doesn't require switching surfaces. [sidebar-new-item-button]
- **`⋯` menu's contents are filetree actions in the Files panel**: Refresh tree / Reindex all / Reindex this file / Sort by. [sidebar-toolbar-actions-menu]

- **New note** (Files-mode `+` left-click): creates a numbered `new-note-N.md` in the currently-selected folder (vault root if nothing's selected) via a `create_note(rel_path)` core command. `N` is the lowest positive integer that doesn't collide with an existing file in the target folder — `new-note-1.md` first, then `new-note-2.md`, and so on. The new file opens in the editor immediately, and the tree row enters inline-rename mode with the `new-note-N` basename pre-selected (extension excluded from selection so users can type a new name and hit Enter without re-typing `.md`). Submit renames via the same `move_note` path; Esc keeps the default name. [sidebar-new-item-button]
- **`⋯` menu** (Files mode) opens a small popover with the v1 entries below. Adding new entries is intentionally low-friction — the menu is the catch-all for low-frequency filetree actions, so future verbs slot in here rather than growing the header row.
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
  - **Properties** — opens a `properties`-kind tab for the note (per `tab-kinds` and the "Note properties tab" section in `editor.md`). Read-only inspector of every piece of data hiker stores about the note across `index.db` and the op log. [tree-context-properties]

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

### Trash panel

Trash is its own activity-bar panel (`feature-trash-panel`), not pinned inside the file tree. The body lists trashed items (basename + deletion time) with per-row Restore / Purge actions; "Empty trash" is in the panel header's right-click menu (disabled when empty). Like any feature panel it can be split alongside others in the accordion. [feature-trash-panel]

Planned richer interactions (the bullets below describe the intended surface; the current panel implements the disk listing + Restore / Purge / Empty):

- **Disk is the source of truth for what's in the bin.** The panel is built by walking `<vault>/.hiker/trash/` directly — every file there shows up. The manifest is consulted for *original path* and *deletion time* only, and only on a per-entry basis. Files dropped into `.hiker/trash/` by hand, or entries whose manifest row got corrupted, still appear and can still be emptied. The manifest is a hint, not a gate. [tree-trash-disk-listing]
- **Flat list, sorted by deletion time descending.** No reconstruction of the original folder structure inside the bin. Trash is a recovery surface ("the thing I deleted ten minutes ago"), not a working tree. Each row shows the basename, a relative-time hint (`5m ago`, `yesterday`, `Mar 12`), and the original path as muted secondary text. Folder entries get a `▸` glyph and a `(N notes)` count derived from the manifest's `members` (or `?` if the entry is orphaned and we can't tell). [tree-trash-flat-by-deleted]
- **Click → read-only preview.** Single click on a trash row opens the file in the editor in a non-editable mode (read-only editor mode via `ViewState.read_only` plus a banner across the top: "Trash preview · Restore to edit"). The buffer's `path` is set to the on-disk trash location, `loadedHash` is set, but `isDirty` is forced false and the save button hides. Switching away from a trash preview discards nothing — there's nothing to discard. [tree-trash-preview]
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
- Frontmatter-editing-aware preview (the preview is read-only; richer trash inspection waits for the properties tab landing on trash entries — see `note-properties-tab`).

### Tree-row index-state markers

Beyond the dirty-suffix dot (`dirty-tree-dot`), each tree row reflects its file's index state with at most one small marker rendered as a suffix glyph (right of the filename, on the same side as the dirty dot). One marker per row, mutually exclusive across the three states. The dirty dot and the index marker paint at distinct positions in the row, so a row can carry both ("dirty *and* queued") without collision. Indexed-and-clean — the common case — shows nothing on either, keeping the tree visually quiet.

- **Unsupported** — hollow grey dot. The file's extension has no chunker (anything outside `.md`, `.markdown`, `.txt` in v1). Derivable client-side from the path; no index lookup needed. [tree-row-unsupported-marker]
- **Skipped** — amber filled dot. The indexer attempted ingest and refused (>5MB sanity cap, UTF-8 decode failure, future: corrupted source). Reason string from the indexer (`"file too large"`, `"not UTF-8"`) shown in the row's `title=` tooltip. [tree-row-skipped-marker]
- **Queued / mid-index** — pulsing accent dot. Transient; clears when the file's index job completes. Driven by indexer-progress events so no polling is needed. [tree-row-queued-marker]

State is supplied by `cmd-file-index-state` (see `index.md`), called lazily for visible rows on render and refreshed in place when index events fire. Folders are never marked — too noisy. The status-bar-side mirror of these states is `status-bar-active-file-index-state` (see `editor.md`).

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


## Multi-select

A persistent multi-note selection model in the file tree: a `selected: HashSet<vault-rel-path>` that survives folder expand/collapse and is the input set for bulk file-tree verbs and for clustering build scope. The file tree already carries single-row selection + drag-drop plumbing; multi-select extends that, it doesn't replace it. [note-multi-select]

**Gesture split.** Mirrors the cluster editor's already-shipped row gestures (`cluster-editor-multi-select-shift-range` in `cluster-editor.md`) so the file-manager convention is identical in both surfaces:

- **Plain click.** Clears any existing multi-selection and re-anchors on the clicked row. The row's primary affordance (open the note) still fires — clicking a row is a "use this row" gesture, not a bare "select this row" gesture.
- **Cmd-click / Ctrl-click.** Toggles the clicked row in the selection set and re-anchors on it, so subsequent shift-clicks pivot off the just-toggled row.
- **Shift-click.** Replaces the selection with the range from the current anchor through the clicked row in current display order (top-to-bottom walk of currently-rendered rows respecting expand/collapse), inclusive. Range membership is computed on the rendered tree at click time; expanding a folder after a shift-click range was set doesn't grow the existing selection. With no anchor (first interaction), a shift-click is treated as a single-row range and sets the anchor.

The anchor lives on the file-tree UI state and is cleared when the vault swaps. Selection survives folder expand/collapse — collapsing a folder whose children are selected keeps them in the set; they re-render as selected on re-expand.

**What it powers:**

- **Bulk file-tree verbs.** The selection set is the target for multi-note actions — move (one `move_note` per selected path under a single transaction, same shape as folder drag), delete (one `delete_note` per path into trash), add-to-board, and add-to-tree-cluster. The right-click context menu (`tree-context-menu`) shows the bulk forms of its verbs when more than one row is selected ("Move N notes to trash?" reuses the folder-delete confirm copy shape). [note-multi-select-bulk-verbs]
- **Selected-notes clustering build scope.** When notes are multi-selected, the clustering build-scope picker (`cluster-editor-build-scope-picker` in `cluster-editor.md`) defaults to `BuildScope::Notes` (per `cluster-build-scope` in `clustering.md`) — the scope already exists; the selection feeds it the set of note ids.
- **Drag-into-cluster authoring.** A multi-selected set can be dragged into a cluster in the cluster editor's graphical surface to author membership by example (`tree-author-blank` in `cluster-editor.md`).
