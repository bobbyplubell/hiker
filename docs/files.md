# Files

Hiker's file tree — the vault's notes browser — is the content of an `egui_workbench` primary-side-bar **Files** activity. The side-bar / accordion mechanics (sections, headers, collapse, resize, drag-to-add, persistence) belong to `egui_workbench` (`egui-workbench/SPEC.md`); this doc owns hiker's tree content: what the rows are, how they're created / moved / renamed / deleted, the trash, the per-row index-state markers, source visibility, and note multi-select.

The dedicated three-button Files/Clusters/Trails switcher row that once sat above the tree is superseded by the egui-workbench activity-bar + multi-region sidebar (`egui-workbench/`, `app/src/workbench_host.rs`): each surface (Files / Clusters / Trails / Trash / Vault) is now an independent dockable panel reached from the activity bar, and per-vault layout persists via the workbench panel set (`app/src/side_panel_persist.rs`) rather than the old `vault.sidebar_mode` row. [sidebar-mode-switcher]
status:: superseded
touches:: [[code:hiker/side_panel_persist]], [[code:hiker/workbench_host]]

One collapse toggle is shared across every surface: the sidebar collapse hides the whole sidebar regardless of which panel is active, so the modes share a single collapse state by virtue of operating on the same sidebar. [sidebar-mode-shared-collapse]
status:: done
note:: evidence: `app/src/sidebar/files.rs` (sidebar toggle unchanged across modes)

## File tree

In Files mode the side bar hosts the file tree, including drag-and-drop note moves — the drop calls a single core `move_note` command that does the fs rename and updates the index path in one step, so the move is recorded explicitly rather than being inferred from watcher events. Same code path is exposed as a `hiker mv` CLI command. [drag-and-drop-move]
status:: done
implements:: [[code:hiker/ops/file/move_folder]]
note:: file DnD calls `move_note`; folder DnD calls `move_folder` → `core::ops::move_folder` (owns watcher suppression + `IndexJob::MoveFolder` send/await) → indexer-side `core::vault::move_folder` (single fs rename + bulk index path remap via `Store::rename_notes_by_paths`). Empty subfolders move with the rename for free. Buffer follows when the open file is inside the moved subtree · evidence: `app/src/sidebar/files.rs` (drag-and-drop handling), `core/src/ops.rs` (`move_folder`)
implements:: [[code:hiker/files/sidebar/move_into_folder]], [[code:hiker/files/sidebar/impl#[`FilesCtx<'_, '_>`]move_into_folder]], [[code:hiker/files/sidebar/impl#[`FilesCtx<'_, '_>`]root_drop_strip]]

### Files-mode header actions

The Files panel header carries the `+` (new item) and `⋯` (actions menu) affordances. No labels — icon-only.

- **`+` is the new-item split-button ([[spec:split-add-button]]).** A primary `+` butted against a caret. Clicking `+` creates the active surface's primary item — a note in the Files panel. Clicking the caret opens a dropdown to pick any document type regardless of current surface (New note / New board / New canvas), so creating a board or canvas while browsing files doesn't require switching surfaces. [sidebar-new-item-button]
status:: done
touches:: [[code:hiker/sidebar]], [[code:hiker/widgets/split_button]], [[code:hiker/workbench_host]]
note:: Files-mode `+` split-button ([[spec:split-add-button]]): the primary `+` creates a numbered `new-note-N.md` (opens it + enters inline-rename via `create_with_suffix`); the caret dropdown offers New note / New board / New canvas · evidence: `app/src/workbench_host.rs` (`side_bar_action_buttons`), `app/src/widgets/split_button.rs`, `app/src/sidebar/mod.rs` (`new_note`/`new_board`/`new_canvas`)
- **[[spec:split-add-button]] is one reusable widget** — primary action button + caret-dropdown; also drives the canvas create toolbar ([[spec:canvas-node-create]]) and the Clusters new-tree control ([[spec:cluster-editor-new-tree-action]]). The dropdown, not a right-click popover, is the visible affordance for the secondary options. [split-add-button]
status:: done
touches:: [[code:hiker/widgets/split_button]]
note:: reusable small `+` split-button — one rounded button with a built-in caret segment (shared outer outline, hairline seam) that opens a dropdown menu; drives the Files header ([[spec:sidebar-new-item-button]]), the canvas create toolbar ([[spec:canvas-node-create]]), and the Clusters new-tree control ([[spec:cluster-editor-new-tree-action]])
- **`⋯` menu's contents are filetree actions in the Files panel**: Refresh tree / Reindex all / Reindex this file / Sort by. [sidebar-toolbar-actions-menu]
status:: partial
note:: `⋯` icon button in the unified sidebar top row, persistent across every mode; opens the actions menu. Files-mode entries: Refresh tree / Reindex all / Reindex this file / Sort by. **Partial**: visibility is now persistent across modes per spec, but the menu's *contents* are still filetree-only. Cluster trees → [[spec:cluster-editor-mode-menu]] entries land with the cluster editor; Trails → trail-scoped entries land with trails · evidence: `app/src/sidebar/files.rs` (`sort_header()` actions menu)

- **New note** (Files-mode `+` left-click): creates a numbered `new-note-N.md` in the currently-selected folder (vault root if nothing's selected) via a `create_note(rel_path)` core command. `N` is the lowest positive integer that doesn't collide with an existing file in the target folder — `new-note-1.md` first, then `new-note-2.md`, and so on. The new file opens in the editor immediately, and the tree row enters inline-rename mode with the `new-note-N` basename pre-selected (extension excluded from selection so users can type a new name and hit Enter without re-typing `.md`). Submit renames via the same `move_note` path; Esc keeps the default name. [sidebar-new-item-button]
- **`⋯` menu** (Files mode) opens a small popover with the v1 entries below. Adding new entries is intentionally low-friction — the menu is the catch-all for low-frequency filetree actions, so future verbs slot in here rather than growing the header row.
  - **Refresh tree** — re-reads the directory and rebuilds the tree from disk, restoring the active highlight with expansion state preserved across the refresh. With the v1 watcher, the tree should mostly stay in sync on its own — refresh is a backstop for the watcher's known failure modes (notify queue overflow during big git checkouts, NFS/network filesystems, missed events) and for the "did I really just save that" sanity case. Auto-refresh from watcher events is a v2 add per `watcher.md`; refresh stays even after that lands. [tree-refresh-manual]
status:: done
note:: evidence: `app/src/sidebar/files.rs` ("Refresh tree" entry)
  - **Auto-refresh on watcher events** — a 200ms-debounced tree refresh on created/deleted/renamed events; modified events are no-ops (tree shape unchanged), and the manual [[spec:tree-refresh-manual]] refresh stays as a backstop. Lifted from v2 → v1; the `watcher.md` "Out of scope for v1" entry naming this is now stale. [tree-refresh-watcher]
status:: done
note:: evidence: `app/src/sidebar/files.rs` (watcher file events handling)
  - **Reindex all** — full-vault reindex via [[spec:reindex-all-action]] (see `index.md`). No confirm modal: re-embedding identical content is non-destructive, and the user opted in by clicking.
  - **Reindex this file** — single-file reindex via [[spec:reindex-current-file-action]]; greyed when no file is active.
  - **Sort by ▸** — submenu of mutually-exclusive sort orders applied to the file tree (folders always grouped first; the chosen order applies within folders and within files). v1 entries: **Name (A→Z)** (default), **Name (Z→A)**, **Modified (newest first)**, **Modified (oldest first)**. Selection persists in memory only for v1; per-vault persistence is a `settings.md` concern when that surface lands. Modified time comes from the filesystem's mtime — same field the watcher and indexer already use, no new metadata. [tree-sort-options]
status:: done
implements:: [[code:hiker/vault/impl#[Vault]resolve]]
note:: Folders grouped first; chosen order applies within each group. mtime sourced from filesystem metadata in `list_dir` (best-effort: a failed stat falls back to 0). In-memory state per spec; persistence waits for `settings.md`. The current order is surfaced in the parent menu entry's label · evidence: `app/src/sidebar/files.rs` (`sort_header()`), `core/src/vault.rs` (`DirEntryDto.mtime`)
implements:: [[code:hiker/files/sidebar/impl#[`FilesCtx<'_, '_>`]sort_header]]
verifies:: [[code:hiker/files/sidebar/sort_label_tests/every_variant_has_a_label]], [[code:hiker/files/sidebar/sort_label_tests/labels_are_distinct]]

  A destructive **Reindex (rebuild)** verb — drops and recreates the schema before reindexing — is deferred to the settings page (`settings.md`). The CLI counterpart [[spec:cli-reindex-rebuild]] covers the operational case in the meantime.

### API & edge cases

Both `create_note` and `move_note` live in `core::vault` and are the single source of truth for creating and relocating notes — UI tree actions and CLI commands (`hiker new`, `hiker mv`) call them unchanged.

- `create_note(rel) -> Result<String>` — creates an empty file at `rel`, returns the actual path used (auto-suffix may have changed it). The button passes a `new-note-N.md` candidate; the CLI passes the requested name verbatim and errors on collision rather than auto-suffixing (CLI explicit, UI forgiving). [create-note-core-cmd]
status:: done
implements:: [[code:hiker/ops/file/create_with_suffix]], [[code:hiker/ops/file/create_at]], [[code:hiker/vault/companion_folder_for]]
note:: empty file, errors on collision (auto-suffix is the caller's job) · evidence: `core/src/vault.rs` (`Vault::create_note`)
- `move_note(from, to) -> Result<()>` — atomic fs rename + index update. Order: suppress watcher events for both paths (see below), fs rename, update `notes.path` + `path_ids` in one transaction, release suppression. If the index update fails the fs rename is rolled back (`to` → `from`) before returning the error. [move-note-core-cmd]
status:: done
implements:: [[code:hiker/ops/file/move_note]], [[code:hiker/vault/move_note]]
note:: `core::ops::move_note` owns watcher suppression + `IndexJob::Move` send/await; indexer-side `core::vault::move_note` runs the atomic fs rename + index update on the owned store. Folder walk lives in [[spec:drag-and-drop-move]] · evidence: `core/src/ops.rs` (`move_note`), `core/src/vault.rs` (`move_note`)
- **Target collision** — `move_note` errors and leaves the source untouched. No overwrite, no auto-suffix; the caller decides what to do (the tree DnD shows a toast, the CLI prints an error).
- **Source is the currently-open buffer** — `move_note` operates on disk only; the buffer's `currentPath` keeps pointing at the old path, so the next save fails the drift check (file missing) and prompts the user. Buffer-follows-rename can come later.
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

`move_note` and `create_note` both perform writes the watcher would otherwise observe and re-enqueue as redundant index jobs (with a small race window where the watcher's rename pairing could disagree with the explicit move). The [[spec:watcher-suppress-self-writes]] feature in `watcher.md` is a prerequisite — build it first so the explicit-mutation path can register a short-lived suppression set around its writes. `delete_note` (below) needs the same suppression.

### Tree interactions

Beyond drag-and-drop and the toolbar buttons, the file tree supports two more interactions:

- **Double-click on a tree row** → enters inline-rename mode for that note. Same UX as the post-create rename: the basename is pre-selected with the extension excluded, Enter or focus loss submits via `move_note` (focus-loss commits per [[spec:inline-edit-lifecycle]] in `interaction.md`, matching the board card editor), Esc is the only cancel and reverts. Double-clicking a folder enters inline-rename for the folder name (recursive move under the hood — the same code path the folder-drag case uses). [tree-double-click-rename]
status:: done
note:: dblclick on file → inline rename via `move_note`; dblclick on folder → inline rename via `move_folder`. Commit on Enter OR focus loss (`rename_edit_outcome`, unit-tested); Esc is the only cancel — was cancel-on-focus-loss until `bug-rename-focus-loss-cancels` (fixed 2026-06-11). An unchanged or empty draft commits as a no-op. Single-click handlers skip the second click of the dbl so it doesn't toggle/open. Folder rename remaps expansion prefixes so expansion state survives, and the open buffer follows when its path is inside the renamed subtree · evidence: `app/src/files/rename.rs` (`rename_text_edit`, `rename_edit_outcome`)
implements:: [[code:hiker/files/rename/commit_rename]], [[code:hiker/files/rename/rename_text_edit]], [[code:hiker/files/rename/start_rename]], [[code:hiker/files/sidebar/impl#[`FilesCtx<'_, '_>`]rename_row]]
- **Right-click on a tree row** → opens a context menu. v1 entries: [tree-context-menu]
status:: done
note:: row menu: Open / Rename / Delete / Properties (greyed); empty-space menu: New note here · evidence: `app/src/sidebar/files.rs` (`FileVerb` context menu)
implements:: [[code:hiker/files/sidebar/impl#[`FilesCtx<'_, '_>`]file_row_menu]], [[code:hiker/files/sidebar/impl#[`FilesCtx<'_, '_>`]run_file_verb]]

  - **Open** — opens the note in the editor (same as a single click; included for discoverability and to give right-click a non-destructive default).
  - **Rename** — enters inline-rename mode (same as double-click).
  - **Delete** — calls `delete_note` after a confirm modal. Delete is *not* permanent: the file is moved into the vault's trash (see "Delete semantics" below). Modal text reflects this: "Move `<path>` to trash?" for files; "Move `<path>` and N notes inside it to trash?" for folders. Two buttons: Cancel (default focus) and Move to trash (red-ish, but not as alarming as a true delete). No "don't ask again" bypass — keep the friction since most people deleting a note from a tree mean to. [tree-context-delete]
status:: done
note:: confirm modal (Cancel default-focus, danger Move-to-trash); folder copy includes recursive note count; closes open buffer if deleted; toast confirmation · evidence: `app/src/sidebar/files.rs` (delete verb)
  - **Properties** — opens a `properties`-kind tab for the note (per [[spec:tab-kinds]] and the "Note properties tab" section in `editor.md`). Read-only inspector of every piece of data hiker stores about the note across `index.db` and the op log. [tree-context-properties]
status:: done
touches:: [[code:hiker/panels/properties]]
note:: menu entry now live — opens a `properties`-kind tab for the note per [[spec:tab-kinds]]. Done per S4a wiring. · evidence: `app/src/sidebar/files.rs` (`FileVerb` Properties entry), `app/src/panels/properties.rs`
implements:: [[code:hiker/files/sidebar/open_properties]]

  Right-click on **empty space below the tree** opens a smaller menu with one entry — **New note here** — which is equivalent to clicking the toolbar's + New note while no folder is selected.

### Duplicate [tree-context-duplicate]

The context menu's **Duplicate** verb copies a file's bytes into a fresh sibling in the same folder. The target name is the first free `<stem>-copy-N.<ext>` (scanning N upward against the folder's current listing), and the copy is written through the same indexer-driven `create_at` op the `+` button uses — watcher suppression plus an upsert index job — so the duplicate is indexed without a redundant watcher round-trip. [tree-context-duplicate]
implements:: [[code:hiker/files/sidebar/duplicate_file]], [[code:hiker/files/sidebar/pick_copy_target]]

### Delete semantics

Delete is a soft delete — the file is moved into a per-vault trash directory, not removed from disk. Restorable until the trash is emptied.

`delete_note(rel) -> Result<()>` lives in `core::vault` next to `create_note` and `move_note`. Order: suppress watcher events for the source path, fs rename into trash (collision-suffixed; see below), update store (`store::delete_note` cascades chunks + vec rows + path_ids per `index.md`) so the note stops appearing in search/related, append a trash-manifest entry recording the original path, release suppression. [delete-note-core-cmd]
status:: done
implements:: [[code:hiker/ops/file/delete]], [[code:hiker/vault/delete_note]]
note:: files + folders; `core::ops::delete` owns watcher suppression + `IndexJob::DeleteNote` send/await; indexer-side `core::vault::delete_note` runs the trash move + index cascade on the owned store and stamps the file entry's path so restore can rebind to its path-keyed snapshot history; rollback on store failure · evidence: `core/src/ops.rs` (`delete`), `core/src/vault.rs` (`delete_note`)

**Trash location:** `vault/.hiker/trash/`. Per-vault rather than per-user so the safety net travels with the vault under Syncthing/git/etc., and so two vaults' deletions don't collide.

**Trash naming:** when moving a file in, prefix the filename with the deletion timestamp to avoid collisions across multiple deletes of the same path: `vault/.hiker/trash/2026-05-06T14-22-31_myNote.md`. Folder deletes recreate the relative folder structure under a single timestamped root: `vault/.hiker/trash/2026-05-06T14-22-31_<foldername>/...`. Manifest at `vault/.hiker/trash/manifest.yaml` records each entry's original path, original mtime, deletion time, and a stable id for restore. [vault-trash]
status:: done
note:: `<vault>/.hiker/trash/` + `manifest.yaml`; collision suffix `_N`; folder moves preserve relative tree; serde_yml + time deps · evidence: `core/src/trash.rs`

**Restore (`hiker trash restore <id|path>`)** — moves the file back to its original path via `move_note` (so the index re-picks it up cleanly). If the original path is now occupied, restore fails and the user picks a new target. [vault-trash-restore]
status:: done
implements:: [[code:hiker/indexer/jobs/impl#[`UpsertCtx<'a>`]handle_restore_from_trash]], [[code:hiker/indexer/jobs/handle_simple_job]], [[code:hiker/indexer/jobs/record_oplog_restore]], [[code:hiker/oplog/writes/restore]], [[code:hiker/oplog/lifecycle/impl#[OpLog]restore_document]], [[code:hiker/ops/file/restore]], [[code:hiker/vault/restore_note]]
note:: `core::ops::restore` resolves the trash entry up front for pre-suppression, then sends `IndexJob::RestoreFromTrash`; indexer-side `core::vault::restore_note` does the fs rename + manifest remove, then `record_oplog_restore` clears the tombstone (`OpLog::restore_document`) so the document comes back at its path (its path-keyed snapshot history is still in `.hiker/history/`), before the inline re-ingest. Errors if original path now occupied; recreates missing parent; CLI not yet wired · evidence: `core/src/ops.rs` (`restore`), `core/src/vault.rs` (`restore_note`), `core/src/oplog/lifecycle.rs` (`restore_document`), `core/src/indexer/jobs.rs` (`record_oplog_restore`), `app/src/sidebar/files.rs` (toast Undo button)

**Empty (`hiker trash empty`)** — permanent deletion of all trash entries. Confirm prompt; this *is* the irrecoverable operation. No automatic emptying in v1 (no TTL, no size cap); auto-empty policies (`trash.retention_days`, `trash.max_size_mb`) can land later as a setting. [vault-trash-empty]
status:: done
note:: confirm modal, no auto-empty in v1; CLI not yet wired · evidence: `core/src/trash.rs` (`Trash::empty`), `app/src/sidebar/files.rs` (header right-click)

The watcher ignores `vault/.hiker/trash/` via the existing `.hiker/` ignore (`watcher.md`) — noted explicitly since trash entries *are* `.md` files a less-careful ignore would re-index.

Edge cases:

- **Currently-open buffer** — moving the file out from under the buffer closes the buffer. The editor clears (or the next file in the tree opens, picked by an "open neighbor" rule); a non-blocking toast confirms the move and offers an Undo for ~5 seconds (Undo calls `hiker trash restore` for the entry just created — cheaper than re-typing the path). If the buffer is dirty, the modal copy adjusts: "Move `<path>` to trash? Unsaved changes will be discarded." Discard is real — the file in trash reflects what was on disk, not the dirty buffer state.
- **Folder delete** — recursive. Walk the folder, move each file into the timestamped trash subtree preserving relative paths, then `std::fs::remove_dir_all` the now-empty source shell. Single transaction across all the store updates and a single manifest entry covers the whole folder, so restore can put the entire subtree back atomically.
- **Source missing** — error. Same reasoning as the move case.
- **Trash itself missing** — auto-create on first delete (`std::fs::create_dir_all`).
- **Trash entry collision** — should be impossible thanks to the timestamp prefix, but if two deletes land in the same second on the same path the second one gets a `_2`, `_3`, ... suffix.
- **CLI parity** — `hiker rm <path>` invokes the same core command. `--yes` skips the confirm prompt. `hiker trash list`, `hiker trash restore <id>`, `hiker trash empty` round out the CLI surface.

### Trash panel

Trash is its own activity-bar panel ([[spec:feature-trash-panel]]), not pinned inside the file tree. The body lists trashed items (basename + deletion time) with per-row Restore / Purge actions; "Empty trash" is in the panel header's right-click menu (disabled when empty). Like any feature panel it can be split alongside others in the accordion. [feature-trash-panel]
status:: done
implements:: [[code:hiker/workbench_host/impl#[`HikerWbBehavior<'a>`][`Host<HikerWbTab, _>`]side_bar_actions_menu]]
note:: Trash is a standalone activity-bar panel (folder/trash icon), not pinned inside Files. Body lists trashed items (name + deleted-at + Restore / Purge) read from `hiker_core::trash::Trash`; empty shows "Trash is empty". The batch "Empty trash" verb lives in the header right-click menu (`Host::side_bar_actions_menu` for `Trash`, disabled when empty) and routes through the `EmptyTrash` confirm modal. Removed from the Files panel (`sidebar/files.rs` no longer has `trash_bin`/`TrashTimeFmt`) · evidence: `app/src/sidebar/trash.rs` (`TrashView`), `panels_registry.rs` (`PANEL_TRASH` / `P_TRASH`), `workbench_host.rs` (`HikerMode::Trash` + label/panel_id/icon/`all`), `state.rs` (`ConfirmIntent::EmptyTrash`)

The earlier in-tree trash bin — a row pinned at the bottom of the file tree — is superseded by the standalone panel above (`app/src/sidebar/trash.rs`); the bin no longer lives in the file tree. [tree-trash-bin]
status:: superseded

Planned richer interactions (the bullets below describe the intended surface; the current panel implements the disk listing + Restore / Purge / Empty):

- **Disk is the source of truth for what's in the bin.** The panel walks `<vault>/.hiker/trash/` directly — every file shows up. The manifest is consulted per-entry for *original path* and *deletion time* only. Hand-dropped files or entries with a corrupted manifest row still appear and can still be emptied — the manifest is a hint, not a gate. [tree-trash-disk-listing]
status:: done
implements:: [[code:hiker/trash/impl#[Trash]list_from_disk]]
note:: walks `.hiker/trash/`; manifest joined per-entry; orphans flagged · evidence: `core/src/trash.rs` (`Trash::list_from_disk`)
- **Flat list, sorted by deletion time descending.** No reconstruction of the original folder structure inside the bin. Trash is a recovery surface ("the thing I deleted ten minutes ago"), not a working tree. Each row shows the basename, a relative-time hint (`5m ago`, `yesterday`, `Mar 12`), and the original path as muted secondary text. Folder entries get a `▸` glyph and a `(N notes)` count derived from the manifest's `members` (or `?` if the entry is orphaned and we can't tell). [tree-trash-flat-by-deleted]
status:: done
note:: sorted desc by `deleted_at`; basename + rel-time + muted orig path; folder rows show `(N notes)` or `(?)` · evidence: `app/src/sidebar/files.rs` (trash-bin rendering)
- **Click → read-only preview.** Single click on a trash row opens the file in the editor in a non-editable mode (read-only editor mode via `ViewState.read_only` plus a banner across the top: "Trash preview · Restore to edit"). The buffer's `path` is set to the on-disk trash location, `loadedHash` is set, but `isDirty` is forced false and the save button hides. Switching away from a trash preview discards nothing — there's nothing to discard. [tree-trash-preview]
status:: done
note:: trash preview opens as `TabKind::Editor { buffer: Trash{...}, diff: None }`. `ensure_readonly_buffer_loaded` reads the trashed file's bytes into a read-only `Buffer`; `render_readonly_source_toolbar` shows the muted "In trash · read-only" label; no save, no diff
- **Right-click → Restore / Delete permanently.** Per-row context menu has two entries. Restore calls [[spec:vault-trash-restore]] and re-ingests the note (see below). Delete permanently removes that single entry from disk + manifest, with a confirm modal that says "Permanently delete `<original_path>`? This cannot be undone." Same `confirmDanger` modal pattern the soft-delete uses. [tree-trash-restore-action]
status:: done
note:: row right-click → Restore (greyed for orphans) / Delete permanently with confirm · evidence: `app/src/sidebar/files.rs` (trash row menu), `core/src/trash.rs` (`Trash::permanent_delete`)
- **Top-level right-click → Empty trash.** Right-clicking the `🗑 Trash` header itself opens a single-entry menu: "Empty trash (N entries)". Calls [[spec:vault-trash-empty]] after the same `confirmDanger` modal. Disabled when `N == 0`. [tree-trash-empty-action]
status:: done
note:: header right-click → "Empty trash (N entries)" with confirm; disabled when N == 0 · evidence: `app/src/sidebar/files.rs` (trash header context menu)

#### Restore semantics

Restore is a `move_note` from the trash entry's on-disk location to its `original_path` (from the manifest), followed by a re-ingest so search/related see it again. `move_note` already routes through the indexer's owned store connection and emits the correct watcher suppression, so restore inherits that path — no separate code, no second writer.

The document's local history survives the round trip: history is the plain-file snapshot tree keyed by path (`op-log.md` "Local history"), which is not moved into trash on delete and is still keyed by `original_path`; restoring to that path means the snapshot history is right there. (When git is integrated, the commit graph is the durable history.) The store re-ingests fresh chunks + embeddings keyed by the restored path ([[spec:store-path-is-identity]]), so search rebinds to the restored identity automatically.

Edge cases:

- **Original path now occupied** — restore fails with a clear message ("`<original_path>` already exists; rename it first or restore to a new location"). v1 doesn't offer an in-app target picker; the workaround is to rename the conflicting file in the tree, then retry restore. CLI has the same constraint per [[spec:vault-trash-restore]].
- **Original parent directory missing** — auto-create on restore. Different from the explicit-mutation `move_note` rule (which errors on missing parent) because the user's intent here is unambiguous: put it back where it was. If the parent was itself deleted into the trash, a cascade restore is *not* attempted — the user restores the parent first. We surface this as the same "not found" error.
- **Orphaned entry (no manifest row)** — restore is unavailable for that row; the menu entry is greyed with tooltip "No original location recorded — drag out of `.hiker/trash/` manually". Empty trash and Delete permanently still work. [tree-trash-orphan-recovery]
status:: done
note:: orphans listed (italic, muted); Restore disabled with explanation; Empty + Delete permanently still work via `trashed_name` identifier · evidence: `core/src/trash.rs` (`list_from_disk`), `app/src/sidebar/files.rs` (trash row menu)
- **Folder entry restore** — restores the entire trashed subtree to `original_path` via a recursive `move_note`-equivalent walk, then re-ingests every `.md` in the manifest's `members`. Single transaction across the store updates so search either sees all of it or none.

#### Interactions and constraints

- **No drag in or out of the trash row.** Restore is an explicit verb, not a DnD gesture. Dragging a regular tree note onto the trash header could plausibly be a delete shortcut, but the existing right-click → Delete plus the confirm modal already covers that path; adding a second route doubles the surface for accidents.
- **Default state: collapsed.** First open of a vault shows the trash row collapsed regardless of count. Persistence of the expanded/collapsed state across launches is deferred to `settings.md`.
- **Refresh.** The manual refresh button ([[spec:tree-refresh-manual]]) re-walks the trash dir alongside the vault. Trash entries don't auto-refresh on filesystem events (the watcher's `.hiker/` ignore); the panel re-reads itself after each Hiker action, and manual edits to the trash dir surface on refresh.
- **Index isolation.** Trash entries are never indexed, never appear in search/related, never count toward `Indexed (N notes)` — covered by the watcher's and walker's `.hiker/` skip; called out here so future indexer changes don't accidentally include trash content.

#### Out of scope (deferred)

- In-app target picker for restore-into-occupied-path conflicts (CLI workaround is fine for v1).
- Auto-empty policies (`trash.retention_days`, `trash.max_size_mb`) — same as the existing [[spec:vault-trash-empty]] deferral.
- Drag-out-of-trash to a specific tree location (would need a target-picker UX too; restore-to-original covers the common case).
- Frontmatter-editing-aware preview (the preview is read-only; richer trash inspection waits for the properties tab landing on trash entries — see [[spec:note-properties-tab]]).

### Tree-row index-state markers

Beyond the dirty-suffix dot ([[spec:dirty-tree-dot]]), each tree row reflects its file's index state with at most one small marker rendered as a suffix glyph (right of the filename, on the same side as the dirty dot). One marker per row, mutually exclusive across the three states. The dirty dot and the index marker paint at distinct positions in the row, so a row can carry both ("dirty *and* queued") without collision. Indexed-and-clean — the common case — shows nothing on either, keeping the tree visually quiet.

- **Unsupported** — hollow grey dot. The file's extension has no chunker (anything outside `.md`, `.markdown`, `.txt` in v1). Derivable client-side from the path; no index lookup needed. [tree-row-unsupported-marker]
status:: done
note:: hollow grey suffix dot from `index_state_for` (returns `Unsupported` for paths outside `core::indexer::indexable_extensions`); the prior client-side `isIndexableExt` predicate was deleted with `bug-is-indexable-extension-duplicated-in-ui` so the rule lives in one place · evidence: `app/src/sidebar/files.rs` (index-state marker rendering)
- **Skipped** — amber filled dot. The indexer attempted ingest and refused (>5MB sanity cap, UTF-8 decode failure, future: corrupted source). Reason string from the indexer (`"file too large"`, `"not UTF-8"`) shown in the row's `title=` tooltip. [tree-row-skipped-marker]
status:: done
note:: amber suffix dot from `index_state_for`; reason in tooltip · evidence: `app/src/sidebar/files.rs` (index-state marker rendering)
implements:: [[code:hiker/files/sidebar/impl#[`FilesCtx<'_, '_>`]index_state_marker]]
- **Queued / mid-index** — pulsing accent dot. Transient; clears when the file's index job completes. Driven by indexer-progress events so no polling is needed. [tree-row-queued-marker]
status:: done
note:: pulsing accent suffix dot driven by indexer-progress events · evidence: `app/src/sidebar/files.rs` (queued-state marker driven by indexer-progress events)

State is supplied by [[spec:cmd-file-index-state]] (see `index.md`), called lazily for visible rows on render and refreshed in place when index events fire. Folders are never marked — too noisy. The status-bar-side mirror of these states is [[spec:status-bar-active-file-index-state]] (see `editor.md`).

### Tree source visibility

The file tree shows every note at its real on-disk path, including the subsystem note collections that carry user-authored bodies — chat sessions and trail waypoints — which live in **visible vault folders** (`chats/`, a trail-doc's companion folder), not hidden under `.hiker/` (per [[spec:subsystem-notes-visible]] in `design.md`). They are ordinary folders/notes in the tree; their clean grouping/labelling is Vault mode's job ([[spec:vault-view-source-groups]]).

A registry remains for any *genuinely hidden* source category that might surface later (one whose payload isn't a user-authored note): it names the category, an optional `vault.show_<category>_in_tree` toggle, the path it covers, and a group label, consulted by the tree renderer when assembling the top-level list. Categories whose toggle is off skip rendering but stay indexed and search-reachable. **v1 seeds no categories** — sessions and waypoints are visible notes, and sidecars-next-to-source hide via their own rule (`extract-sidecar-tree-hidden`), so nothing currently rides this registry; it's the hook for future hidden categories. [tree-source-visibility-toggles]
status:: planned
note:: registry hook for any *genuinely hidden* future source category (optional `vault.show_<category>_in_tree` toggle + virtual group). v1 seeds **no** categories — sessions/waypoints are visible notes ([[spec:subsystem-notes-visible]]), sidecars hide via `extract-sidecar-tree-hidden`. The retired `vault.show_sessions_in_tree` toggle was removed alongside the sessions→`chats/` migration

Search and related-notes are independent of any such toggle — a category hidden from the tree is never removed from search. The toggles are navigation chrome, not data scoping. [tree-source-visibility-orthogonal-to-search]
status:: planned
note:: search and related-notes ignore any visibility toggle — hiding a category from the tree doesn't drop it from search


## Companion folders

A note can own a sibling folder of child notes: a note at `<dir>/<name>.md` pairs with a folder `<dir>/<name>/` holding notes that logically belong to it. Used by trail waypoints ([[spec:trail-storage-layout]] in `trails.md`); a general primitive, not specific to trails. [note-companion-folder]
status:: partial
implements:: [[code:hiker/indexer/jobs/handle_simple_job]], [[code:hiker/indexer/jobs/update_trail_waypoints_if_relevant]], [[code:hiker/trails/waypoints_dir_for_doc]], [[code:hiker/trails/ops/create_trail]], [[code:hiker/trails/ops/append_waypoint]], [[code:hiker/trails/ops/delete_trail]], [[code:hiker/trails/ops/on_note_moved]], [[code:hiker/trails/ops/impl#[`RewriteCtx<'a>`]fan_out_waypoint_moved]], [[code:hiker/trails/ops/impl#[`RewriteCtx<'a>`]rewrite_own_waypoint_paths_on_trail_doc_move]], [[code:hiker/vault/move_note]]
verifies:: [[code:hiker/vault/tests/move_note_moves_companion_folder_and_returns_members]], [[code:hiker/vault/tests/move_folder_renames_dir_and_remaps_indexed_members]]
note:: core primitive done + tested: `companion_folder_for(rel)` computes `<dir>/<name>.md` → `<dir>/<name>/`; `move_note` fs-renames the companion folder + bulk-remaps members in the same op and returns `(old,new)` pairs so the rename-rewrite pass covers the children; lazy creation (folder made on first child write, not note creation — see trails `append_waypoint`). `hiker.parent` / the waypoint tree stays the nesting authority (not folder membership). GAP: the Vault-mode "nest folder contents under the note" render + the general `hiker.parent` child-stamp consumer (crawl/feed captures) aren't built yet — trails is the only producer wired so far · evidence: `core/src/vault.rs` (`companion_folder_for`, `move_note` companion-folder pairing + returned member pairs); `core/src/indexer/jobs.rs` (`IndexJob::Move` handler fans `on_note_moved` over companion members)

- **Pairing rule.** A folder whose name exactly matches a sibling `.md` basename is that note's companion folder. The Files tree renders it as an ordinary folder (real bytes, real path); Vault mode collapses its contents *as children of the note* (`vault-view.md`).
- **`hiker.parent` is the nesting authority, not folder membership.** Child notes stamp `hiker.parent` (or, for trails, ride the `hiker.waypoints` tree). A file dropped into the folder *without* a parent stamp is a plain file, not a logical child — so the physical folder is a convenience/discoverability home, and the metadata is the truth. This keeps the two decoupled: stray files never become false children.
- **Rename keeps the pair in sync.** Renaming `<name>.md` renames `<name>/` in the same `move_note` transaction (and rewrites child→parent references via [[spec:wikilink-rename-rewrite]]). The companion folder is cosmetic — losing or mismatching it never breaks nesting, which resolves through the parent stamp regardless. [note-companion-folder]
- **Creation is lazy.** The folder is created on first child write, not when the note is created. A capture note with `fill_body: true` (single clip) or zero children has no companion folder.


## Multi-select

A persistent multi-note selection model in the file tree: a `selected: HashSet<vault-rel-path>` that survives folder expand/collapse and is the input set for bulk file-tree verbs and for clustering build scope. The file tree already carries single-row selection + drag-drop plumbing; multi-select extends that, it doesn't replace it. [note-multi-select]
status:: planned
note:: persistent multi-note selection model in the file tree: `selected: HashSet<vault-rel-path>` with plain-click (clear + select), Cmd/Ctrl-click (toggle + re-anchor), Shift-click (range from anchor through clicked row in display order). Mirrors the cluster editor's gesture split ([[spec:cluster-editor-multi-select-shift-range]]). Selection survives folder expand/collapse. Feeds bulk file-tree verbs ([[spec:note-multi-select-bulk-verbs]]), the Selected-notes clustering build scope (`BuildScope::Notes` per [[spec:cluster-build-scope]]), and drag-into-cluster authoring ([[spec:tree-author-blank]]). Extends the file tree's existing single-row selection + drag-drop plumbing
implements:: [[code:hiker/files/sidebar/impl#[`FilesCtx<'_, '_>`]handle_select_click]]

**Gesture split.** Mirrors the cluster editor's already-shipped row gestures ([[spec:cluster-editor-multi-select-shift-range]] in `cluster-editor.md`) so the file-manager convention is identical in both surfaces:

- **Plain click.** Clears any multi-selection and re-anchors on the clicked row. The row's primary affordance (open the note) still fires — a click is "use this row," not bare "select this row."
- **Cmd-click / Ctrl-click.** Toggles the clicked row in the selection set and re-anchors on it, so subsequent shift-clicks pivot off it.
- **Shift-click.** Replaces the selection with the range from the current anchor through the clicked row in current display order (top-to-bottom walk of rendered rows, respecting expand/collapse), inclusive. Range membership is computed on the rendered tree at click time — expanding a folder afterward doesn't grow the selection. With no anchor (first interaction), it's a single-row range that sets the anchor.

The anchor lives on the file-tree UI state and clears on vault swap. Selection survives folder expand/collapse — collapsed children stay in the set and re-render selected on re-expand.

**What it powers:**

- **Bulk file-tree verbs.** The selection set is the target for multi-note actions — move (one `move_note` per path under a single transaction, same shape as folder drag), delete (one `delete_note` per path into trash), add-to-board, add-to-canvas ([[spec:canvas-add-to-canvas-verb]] in `canvas.md` — each path inserted as a pointer node), and add-to-tree-cluster. The right-click menu ([[spec:tree-context-menu]]) shows bulk forms when more than one row is selected ("Move N notes to trash?" reuses the folder-delete confirm copy). [note-multi-select-bulk-verbs]
status:: planned
note:: the multi-selection set is the target for bulk file-tree actions — move (one `move_note` per path, single transaction, same shape as folder drag), delete (one `delete_note` per path into trash), add-to-board, add-to-tree-cluster. The right-click context menu ([[spec:tree-context-menu]]) shows the bulk forms of its verbs when more than one row is selected
- **Drag-into-canvas (pending).** A file row or multi-selection dragged onto an open canvas should drop pointer nodes at the drop point — the deferred [[spec:canvas-dnd-add]] (`canvas.md`), riding the uniform vault-path drag payload (`design.md` [[spec:trails-dnd-ingestion]]). Until it lands, the **Add to canvas** verb and the canvas **Insert from vault** picker cover insertion. [note-multi-select-bulk-verbs]
- **Selected-notes clustering build scope.** Multi-selection defaults the clustering build-scope picker ([[spec:cluster-editor-build-scope-picker]] in `cluster-editor.md`) to `BuildScope::Notes` (per [[spec:cluster-build-scope]] in `clustering.md`), feeding it the set of note ids.
- **Drag-into-cluster authoring.** A multi-selected set can be dragged into a cluster in the cluster editor to author membership by example ([[spec:tree-author-blank]] in `cluster-editor.md`).
