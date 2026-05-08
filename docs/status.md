# Status

Implementation status of features specced in the other docs. One row per feature, identified by a stable kebab-case slug. Slugs are positional-free — they name the feature, not its location in the spec — so reorganizing sections of `editor.md`, `index.md`, etc. doesn't break references. Code or commit messages can cite a slug (e.g. "implements `pre-write-drift-check`") and the link survives doc reshuffles.

Status values:

- **done** — implemented and exercised
- **partial** — present but incomplete; the gap is named in the notes column
- **planned** — specced, not started

When a feature is reorganized, renamed, or split, update its row here first — this file is the registry. Audited 2026-05-06.


## How to use this file

- **When code implements (or starts implementing) a slug, tag it in a comment.** A short marker near the relevant entry point is enough: `// status: pre-write-drift-check` in Rust, `// status: drag-and-drop-move` in TS. The goal is grep-ability — `rg "status: drag-and-drop-move"` should land you on the implementation.
- **Don't go overboard.** One tag per feature, near the most natural anchor (the public function, the event handler, the top of the relevant module). Don't sprinkle the slug across every helper. If a feature spans several files, tag the obvious entry point and let `status.md` be the index.
- **When you write a new spec, add its features here.** New rows go in the section for the spec doc that owns them, with a slug, status (`planned` to start), and a one-line note. If a feature crosses doc boundaries (e.g. a core command shared by UI and CLI), put it under the spec that defines its semantics and reference the slug from the others.
- **When a feature is renamed, split, or merged, edit `status.md` first**, then update tags in code and references in other docs. Treat the slug like a function name: stable until you deliberately rename it.

### Open meta-tasks

- [x] Backfilled spec docs with inline `[slug]` markers next to each feature definition — 2026-05-06.


### Bugs / known issues

Moved to [`bug_tracking.md`](bug_tracking.md). Same conventions (kebab-case slug, one-line note, optional file:line); this file stays focused on the feature registry.


## Editor (editor.md)

| Slug | Status | Evidence | Notes |
| ---- | ------ | -------- | ----- |
| `buffer-path-tracking` | done | `ui/src/main.ts`, `core/src/vault.rs` | |
| `buffer-dirty-derived` | done | `ui/src/main.ts:67` | isDirty computed from doc vs loadedText |
| `dirty-window-title` | done | `ui/src/main.ts:75` | |
| `dirty-tree-dot` | done | `ui/src/main.ts:90` | only updates while file is active buffer |
| `save-keybind-mod-s` | done | `ui/src/editor/keybinds.ts:102` | |
| `keybind-registry` | done | `ui/src/editor/keybinds.ts` | flat list, validate() on duplicates |
| `save-button` | done | `ui/src/main.ts:127` | disabled when no file or clean |
| `file-switch-guard-dirty` | done | `ui/src/main.ts:407` | uses `confirm3` real modal |
| `window-close-guard-dirty` | done | `ui/src/main.ts:319` | |
| `pre-write-drift-check` | done | `core/src/vault.rs:99` | re-reads + hashes before write |
| `drift-conflict-modal` | done | `ui/src/main.ts:150` | keep/take/cancel; no diff option |
| `status-bar-layout` | done | `ui/index.html`, `ui/src/style.css` | three regions |
| `status-bar-index-label` | done | `ui/src/main.ts:450` | model loading / indexing / indexed / error |
| `status-bar-path-basename-tooltip` | done | `ui/src/main.ts` (`updateStatus`) | basename in `#status-path`, full rel-path in `title=` |
| `status-bar-path-reveal` | done | `ui/src/main.ts` (statusPathEl click handler), `ui/src-tauri/src/lib.rs` (`reveal_in_file_manager`, `reveal_path` per-OS) | macOS `open -R`, Windows `explorer /select,`, Linux `xdg-open <parent>` (Linux has no portable select-file verb). Suppressed for trash-preview buffers so internal `.hiker/trash/` paths don't leak |
| `ui-no-sibling-pushout` | done | `ui/src/style.css` (`#status-bar`, `#vault-bar`) | applied to status-bar regions and vault-bar; rule documented in CSS comment at `#status-bar` |
| `status-bar-goto-line` | planned | — | line:col is display-only |
| `three-column-layout` | done | `ui/index.html`, `ui/src/style.css` | grid, sides collapsible |
| `panel-toggle-buttons` | done | `ui/index.html:19` | sidebar + related toggles |
| `cm6-extension-order` | done | `ui/src/main.ts:113` | basicSetup → lang → save tracking → keymap |
| `cm6-editor-reuse` | done | `ui/src/main.ts:194` | doc replaced via dispatch on switch |
| `drag-and-drop-move` | done | `ui/src/main.ts` (`attachDnd`, `performDrop`), `core/src/ops.rs` (`move_folder`) | file DnD calls Tauri `move_note`; folder DnD calls Tauri `move_folder` → `core::ops::move_folder` (owns watcher suppression + `IndexJob::MoveFolder` send/await) → indexer-side `core::vault::move_folder` (single fs rename + bulk index path remap via `Store::rename_notes_by_paths`). Empty subfolders move with the rename for free. Buffer follows when the open file is inside the moved subtree |
| `create-note-button` | done | `ui/src/main.ts` (`new-note-btn` handler), `core/src/ops.rs` (`create_with_suffix`) | name-template parameter (`"new-note"` from Tauri); op owns the suffix loop + watcher suppression + Upsert send |
| `tree-refresh-manual` | done | `ui/src/main.ts` (`tree-actions-btn` "Refresh tree" entry) | re-reads dir; restores active highlight; expansion state preserved across refresh via `expandedFolders` set; lives inside the `…` actions menu |
| `tree-refresh-watcher` | done | `ui/src/main.ts` (`scheduleTreeRefreshFromWatcher`, `hiker:file-changed` listener) | 200ms-debounced `refreshTree` on created/deleted/renamed events; modified events are no-ops (tree shape unchanged); manual `tree-refresh-manual` stays as a backstop. Lifted from v2 → v1; watcher.md "Out of scope for v1" entry now stale |
| `tree-double-click-rename` | done | `ui/src/main.ts` (`renderDir` dblclick handler, `beginInlineRename`) | dblclick on file → inline rename via `move_note`; dblclick on folder → inline rename via `move_folder`. Single-click handlers skip when `event.detail >= 2` so the second click of the dbl doesn't toggle/open. Folder rename remaps `expandedFolders` prefixes so expansion state survives, and the open buffer follows when its path is inside the renamed subtree |
| `tree-context-menu` | done | `ui/src/main.ts` (`attachContextMenu`, `openContextMenu`) | row menu: Open / Rename / Delete / Properties (greyed); empty-space menu: New note here |
| `tree-context-delete` | done | `ui/src/main.ts` (`deleteFromTree`) | confirm modal (Cancel default-focus, danger Move-to-trash); folder copy includes recursive note count; closes open buffer if deleted; toast confirmation |
| `tree-context-properties` | partial | `ui/src/main.ts` (`attachContextMenu`) | menu entry stub disabled; opens nothing until frontmatter editing lands |
| `delete-note-core-cmd` | done | `core/src/ops.rs` (`delete`), `core/src/vault.rs` (`delete_note`), `ui/src-tauri/src/lib.rs` (`delete_note` Tauri cmd) | files + folders; `core::ops::delete` owns watcher suppression + `IndexJob::DeleteNote` send/await; indexer-side `core::vault::delete_note` runs the trash move + index cascade on the owned store; rollback on store failure |
| `vault-trash` | done | `core/src/trash.rs` | `<vault>/.hiker/trash/` + `manifest.yaml`; collision suffix `_N`; folder moves preserve relative tree; serde_yml + time deps |
| `vault-trash-restore` | done | `core/src/ops.rs` (`restore`), `core/src/vault.rs` (`restore_note`), `ui/src-tauri/src/lib.rs` (`restore_trash_entry`), `ui/src/main.ts` (toast Undo button) | `core::ops::restore` resolves the trash entry up front for pre-suppression, then sends `IndexJob::RestoreFromTrash`; indexer-side `core::vault::restore_note` does the fs rename + manifest remove and the indexer task re-ingests inline. Errors if original path now occupied; recreates missing parent; CLI not yet wired |
| `vault-trash-empty` | done | `core/src/trash.rs` (`Trash::empty`), `ui/src-tauri/src/lib.rs` (`empty_trash`), `ui/src/main.ts` (header right-click) | confirm modal, no auto-empty in v1; CLI not yet wired |
| `tree-trash-bin` | done | `ui/index.html` (`#trash-bin`), `ui/src/main.ts` (`refreshTrashBin`, `renderTrashBin`) | pinned at bottom of sidebar; collapsed by default; chevron + count |
| `tree-trash-disk-listing` | done | `core/src/trash.rs` (`Trash::list_from_disk`), `ui/src-tauri/src/lib.rs` (`list_trash`) | walks `.hiker/trash/`; manifest joined per-entry; orphans flagged |
| `tree-trash-flat-by-deleted` | done | `ui/src/main.ts` (`renderTrashBin`, `relativeTime`) | sorted desc by `deleted_at`; basename + rel-time + muted orig path; folder rows show `(N notes)` or `(?)` |
| `tree-trash-preview` | done | `ui/src/main.ts` (`openTrashPreview`, `setReadOnly`), `ui/index.html` (`#trash-banner`) | reads `.hiker/trash/<name>` via `read_file_with_hash`; `readOnlyCompartment` toggles `EditorState.readOnly`; banner shown; save disabled; status bar shows "(in trash)" |
| `tree-trash-restore-action` | done | `ui/src/main.ts` (`openTrashRowMenu`), `ui/src-tauri/src/lib.rs` (`permanent_delete_trash_entry`), `core/src/trash.rs` (`Trash::permanent_delete`) | row right-click → Restore (greyed for orphans) / Delete permanently with confirm |
| `tree-trash-empty-action` | done | `ui/src/main.ts` (`trashHeaderEl` contextmenu) | header right-click → "Empty trash (N entries)" with confirm; disabled when N == 0 |
| `tree-trash-orphan-recovery` | done | `core/src/trash.rs` (`list_from_disk`), `ui/src/main.ts` (`openTrashRowMenu`) | orphans listed (italic, muted); Restore disabled with explanation; Empty + Delete permanently still work via `trashed_name` identifier |
| `confirm3-real-modal` | done | `ui/src/main.ts:1164` (`confirm3`) | overlay + `role=dialog` + `aria-modal`; used by `openFile` dirty guard and elsewhere |
| `help-panel-keybinds` | planned | — | enumerate keybinds.list() |
| `tree-toolbar-actions-menu` | done | `ui/index.html` (`#tree-actions-btn`), `ui/src/main.ts` (`treeActionsBtn` click handler) | `…` button next to + New note opens the existing `openContextMenu` popover; hosts Refresh tree / Reindex all / Reindex this file / Sort by |
| `tree-sort-options` | done | `ui/src/main.ts` (`treeSortOrder`, `sortTreeEntries`, `openSortByMenu`), `core/src/vault.rs` (`DirEntryDto.mtime`) | Folders grouped first; chosen order applies within each group. mtime sourced from filesystem metadata in `list_dir` (best-effort: a failed stat falls back to 0). In-memory state per spec; persistence waits for `settings.md`. Submenu rendered as a second flat `openContextMenu` invocation since the menu helper has no nested-submenu support — UX is fine, the current order is also surfaced in the parent entry's label |
| `tree-row-unsupported-marker` | done | `ui/src/main.ts` (`renderTreeRowLabel`, `isIndexableExt`) | hollow grey suffix dot derived client-side from extension; no Tauri round trip for non-md/txt rows |
| `tree-row-skipped-marker` | done | `ui/src/main.ts` (`renderTreeRowLabel`, `applyIndexMarker`), `ui/src/style.css` (`#tree li.ix-skipped > .ix-marker`) | amber suffix dot from `index_state_for`; reason in `title=` tooltip |
| `tree-row-queued-marker` | done | `ui/src/main.ts` (`updateIndexStateForPath` on `started`/`finished`/`skipped`), `ui/src/style.css` (`#tree li.ix-queued > .ix-marker`, `@keyframes ix-queued-pulse`) | pulsing accent suffix dot driven by `hiker:reindex-progress` events |
| `status-bar-active-file-index-state` | done | `ui/src/main.ts` (`renderIndexStatus`) | center label swaps to "Not indexed (unsupported filetype)" / "Skipped — <reason>" / "Queued for indexing"; reverts to aggregate label when active buffer is Indexed or trash-preview |
| `note-mutations-menu` | planned | — | deferred top-bar menu for content-mutation actions; first candidate is markdown reformat (local-CPU / local-API / cloud backend) |
| `editor-view-options-menu` | done | `ui/index.html` (`#view-menu-btn`), `ui/src/main.ts` (`buildViewMenuItems`, `viewMenuBtn` click handler) | `View ▾` button on the editor toolbar between the sidebar and related toggles; opens the existing `openContextMenu` popover with checkable rows. State in-memory only per spec |
| `view-show-chunk-boundaries` | done | `ui/src/editor/chunkBoundaries/index.ts`, `ui/src/main.ts` (`chunkBoundariesCompartment`, `setChunkBoundariesEnabled`, `fetchAndApplyChunkBounds`) | StateField + line-decoration boundary rule + dedicated gutter showing chunk indices. Toggled via View menu; default off. Refreshes on file-open, on save (500ms debounce, same cadence as related), and after watcher silent-reload. Faint gutter hint shown when the file is unsupported / skipped / queued / has zero chunks |
| `view-live-preview-toggle` | done | `ui/src/main.ts` (`buildViewMenuItems` "Live preview" entry) | wired to `setLivePreviewEnabled`; checkmark reflects `livePreviewEnabled`; default on |
| `view-render-txt-as-markdown-toggle` | done | `ui/src/main.ts` (`setRenderTxtAsMarkdown`, `buildViewMenuItems` "Render .txt as markdown" entry) | flips both `language` and `livePreview` compartments for the active buffer; persists via `settings-write-back` to `editor.render_txt_as_markdown` |
| `view-word-wrap-toggle` | done | `ui/src/main.ts` (`wordWrapCompartment`, `setWordWrapEnabled`, `buildViewMenuItems` "Word wrap" entry) | reconfigures CM6 `EditorView.lineWrapping` via its own compartment; persists via `settings-write-back` to `editor.word_wrap` |
| `view-show-whitespace-toggle` | done | `ui/src/main.ts` (`whitespaceCompartment`, `setWhitespaceEnabled`) | CM6's `highlightWhitespace` in its own compartment; default off; toggled via View menu. Persistence still pending `settings-section-editor` |
| `view-line-numbers-toggle` | done | `ui/src/main.ts` (`setLineNumbersVisible`), `ui/src/style.css` (`.cm-editor.hide-line-numbers`) | hides `.cm-gutter.cm-lineNumbers` from `basicSetup` via a class on the editor root rather than reconfiguring the extension stack; default visible. Persistence still pending `settings-section-editor` |
| `view-heading-breadcrumb-toggle` | partial | `ui/src/main.ts` (`buildViewMenuItems`) | menu entry stub disabled with tooltip "Pairs with view-show-chunk-boundaries" |
| `view-hide-frontmatter-toggle` | done | `ui/src/editor/hideFrontmatter/index.ts`, `ui/src/main.ts` (`hideFrontmatterCompartment`, `setHideFrontmatterEnabled`, View-menu entry), `core/src/config.rs` (`editor.hide_frontmatter`) | block-`Decoration.replace` over the leading `---\n…\n---\n` range with a `▸ frontmatter (N lines)` widget, recomputed off `state.doc` so live edits update the count. Detection caps the closing-`---` search at 1000 lines to bound the scan; unterminated blocks are no-op. Default off; persists via `settings-write-back` |
| `vault-home-screen` | done | `ui/index.html` (`#vault-home`), `ui/src/main.ts` (`refreshVaultHome` and helpers), `ui/src/style.css` (`#vault-home`, `.vault-home-*`) | header + three stacked widgets (stats, recently modified, recently accessed). `setVaultHomeVisible(true)` at the end of `applyOpenedVault` makes the home page the default landing surface on vault open per spec. New-note button calls existing `create_note` against vault root |
| `vault-home-stats-widget` | done | `core/src/store.rs` (`Store::vault_stats`, `VaultStats`), `ui/src-tauri/src/lib.rs` (`vault_home_stats` Tauri cmd, `VaultHomeStats`), `ui/src/main.ts` (`refreshVaultHomeStats`, `scheduleVaultHomeStatsRefresh`) | five tiles: Notes / Indexed / Chunks / Queued / Skipped. Queued count rides the existing `IndexerHandle::status().queued`. Live-updates via debounced refresh on every terminal `hiker:reindex-progress` event. Unsupported / disk-usage breakdowns deliberately deferred — both need a vault walk |
| `vault-home-recent-modified` | done | `core/src/store.rs` (`Store::recent_notes_by_mtime`, `RecentNote`), `ui/src-tauri/src/lib.rs` (`recent_notes_modified`), `ui/src/main.ts` (`refreshVaultHomeRecentModified`, `scheduleVaultHomeModifiedRefresh`) | `ORDER BY mtime DESC LIMIT 10` over non-skipped notes. Refresh debounced (400ms) on `hiker:file-changed` for any kind that can shift mtime ranking (created/deleted/renamed/modified). Click on a row opens via `openFile` |
| `vault-home-recent-accessed` | done | `core/src/store.rs` (`Store::recent_notes_by_access`), `ui/src-tauri/src/lib.rs` (`recent_notes_accessed`), `ui/src/main.ts` (`refreshVaultHomeRecentAccessed`) | `ORDER BY last_accessed_at DESC` excluding NULL. Refreshes on full home re-render; the watcher doesn't drive these since hiker itself is the only writer of `last_accessed_at` and the writes happen via `note_accessed` from `openFile` |
| `note-access-tracking` | done | `core/src/store.rs` (`SCHEMA_VERSION = 4`, `notes.last_accessed_at`, `Store::touch_note_access`, `NoteRow.last_accessed_at`), `core/src/indexer.rs` (`IndexJob::TouchAccess`, `IndexerHandle::touch_access`, handler in `handle_simple_job`), `ui/src-tauri/src/lib.rs` (`note_accessed` Tauri cmd), `ui/src/main.ts` (fire-and-forget `invoke("note_accessed", { rel })` at end of `openFile`) | schema bumps to v4 (fail-loud + reindex per `store-version-fail-loud`). Touch is fire-and-forget over the indexer mpsc so writes go through the indexer's owned writer. No-op when the note isn't yet indexed — the next ingest creates the row, and subsequent opens record |
| `vault-home-button` | done | `ui/index.html` (`#home-btn`), `ui/src/main.ts` (`setVaultHomeVisible`, click handler) | icon-only house glyph in vault bar; toggles `#editor-pane.home-view` which swaps `#editor` ↔ `#vault-home`. `openFile` and `openTrashPreview` exit home view so opening any note restores the editor. Default landing surface on vault open. Keybind id `vault.go-home` reserved per editor.md but not yet registered (chord TBD) |
| `vault-bar-open-vault-icon` | done | `ui/index.html` (`#pick-vault`) | inline-SVG folder glyph; `class="icon-btn"`; `title="Open vault…"` + `aria-label` preserve discoverability; click handler unchanged (`openVault` → `open_vault_at`) |
| `sidebar-toggle-icon` | done | `ui/index.html` (`#toggle-sidebar`) | inline-SVG safe-dial glyph (rounded-square frame around a circle with spokes); `title="Toggle sidebar"`; click handler unchanged |
| `discovery-toggle-icon` | done | `ui/index.html` (`#toggle-related`) | inline-SVG magnifying glass; `title="Toggle discovery panel"` (was "Toggle related notes"); click handler unchanged |
| `view-menu-icon` | done | `ui/index.html` (`#view-menu-btn`) | inline-SVG eye glyph (outline + pupil) replaces text-and-chevron `View ▾`; tooltip + `aria-label="View options"`; click handler unchanged |
| `mutations-menu-icon` | done | `ui/index.html` (`#mutations-menu-btn`) | inline-SVG wand glyph (diagonal stick + sparkle); icon-only `toolbar-btn`; no click handler — icon reservation, lands with `note-mutations-menu` |
| `trails-menu-icon` | done | `ui/index.html` (`#trails-menu-btn`) | inline-SVG squiggly-path glyph (sine-wave); icon-only `toolbar-btn`; no click handler — icon reservation, lands with the trails UI |
| `tree-org-menu-icon` | done | `ui/index.html` (`#tree-org-menu-btn`) | inline-SVG hierarchical-tree glyph (root + two children, connected); icon-only `toolbar-btn`; no click handler — icon reservation, lands with the menu itself |
| `vault-home-recent-activity-widget` | done | `ui/src/main.ts` (`refreshActivityWidget`, `buildActivityPreviewRow`), `ui/index.html` (`#vault-home-activity`), `ui/src-tauri/src/lib.rs` (`recent_changes`, `changes_count`) | hidden when `changes_count == 0`; preview is top-5 rows; click anywhere in section → detail view; subscribes to `hiker:changes-appended` (300ms debounce) for live refresh |
| `vault-home-detail-views` | done | `ui/src/main.ts` (`showHomeOverview`, `showHomeDetail`, `refreshVaultHome`), `ui/index.html` (`#vault-home-overview` / `#vault-home-detail`) | overview-vs-detail is a swap inside `#vault-home`; Home button click reruns `refreshVaultHome` which forces overview; note-row click in any detail view exits home to editor |
| `vault-home-stats-detail` | planned | per-tile detail views (Notes / Indexed / Chunks / Queued / Skipped) parameterized by source tile; Skipped row offers per-row retry via `IndexJob::Upsert force=true` |
| `vault-home-recent-activity-detail` | done | `ui/src/main.ts` (`renderActivityDetail`, `buildActivityDetailRow`, `openSnapshotPreview`, `doRestoreSnapshot`), `ui/src-tauri/src/lib.rs` (`restore_snapshot`, `change_content`, `rollback_change`) | mental model is version-list (each row = saved version). Row click → opens snapshot read-only in editor (`snapshot-preview-mode`). Per-row `[Restore this version]` writes that row's `content_at(id)` back via `restore_snapshot`, stamps `metadata.restored_from`. `current` badge marks each path's most recent row; `↩ restored` badge marks rows that were themselves a Restore. Hidden on `current` row (no-op) and `'deleted'` rows (no content). The change-shaped flavor (`rollback_change` walking `previous_content_for_path`) stays for MCP agent rollback per `mcp.md`. Inline diff deferred — open in RO covers the review need cleanly |
| `snapshot-preview-mode` | done | `ui/src/main.ts` (`openSnapshotPreview`, `exitSnapshotPreview`, `setReadOnly(ro, mode)`, `Buffer.snapshotPreview`/`snapshotChangeId`, `isReadOnlyBuffer`), `ui/index.html` (`#snapshot-banner`), `ui/src/style.css` (`#snapshot-banner`) | reuses trash-preview machinery with a separate banner element (amber, not red). Banner shows snapshot metadata + `[Restore this version]` + `[Close preview]`. `isReadOnlyBuffer()` helper extends prior `buffer.preview` checks to cover snapshot mode for save/dirty/watcher guards. Closing returns to the activity detail view. Snapshot-banner Restore writes the previewed version back via the same `doRestoreSnapshot` path as per-row Restore |
| `vault-home-recent-activity-author-filter` | done | `ui/src/main.ts` (`activeAuthorFilters`, filter-pill rendering inside `renderActivityDetail`) | pills appear only for author classes present in the visible window; default-all-on first time then preserves user toggles within the session; per-vault persistence via `settings-write-back` deferred — no `[home]` eligible-key set yet |
| `recent-activity-human-icon` | done | `ui/src/main.ts` (`authorPillIcon` user branch) | inline-SVG half-oval body + circle head glyph; rendered inside the `user` pill |
| `recent-activity-agent-icon` | done | `ui/src/main.ts` (`authorPillIcon` agent branch) | inline-SVG simplified-robot glyph; rendered for `agent:*` rows |
| `vault-home-recent-activity-unrollback` | done | `ui/src/main.ts` (`recentlyRestoredFromId`, highlight + caption rendering inside `buildActivityDetailRow`) | after a Restore the row that *was* the path's current state gets a soft highlight + "← previous state — click Restore to undo" caption. The action is the same `[Restore this version]` button as anywhere else (no separate primitive); the caption is purely a hint. Append-only chain composes naturally — every row in retention is an addressable Restore target |
| `vault-home-recents-detail` | planned | full-list versions of Recently Modified / Recently Accessed; lower priority since each preview row already opens on click |
| `navigation-history-stack` | planned | — | per-vault in-memory stack of editor-pane content states; cleared on vault swap; not persisted across restarts |
| `vault-bar-back-button` | planned | — | icon-only back button pinned to right edge of vault bar; disabled when no back history |
| `vault-bar-forward-button` | planned | — | icon-only forward button pinned to right edge of vault bar; disabled when no forward history |
| `navigation-trackpad-swipe` | planned | — | two-finger horizontal trackpad swipe triggers back/forward via wheel `deltaX` past threshold (~120px); right-swipe = back, left = forward |
| `navigation-keybind` | planned | — | reserves `navigation.back` (Cmd/Ctrl-[) and `navigation.forward` (Cmd/Ctrl-]) in keybind registry; Alt-Left/Right as additional Linux/Windows bindings |
| `navigation-dirty-buffer-guard` | planned | — | back/forward into a different note from a dirty buffer fires the existing `file-switch-guard-dirty` Keep/Discard/Cancel modal |


## Index (index.md)

| Slug | Status | Evidence | Notes |
| ---- | ------ | -------- | ----- |
| `store-sqlite-vec-static` | done | `core/src/store.rs` | bundled rusqlite + sqlite-vec |
| `store-schema-v1` | done | `core/src/store.rs` | notes / chunks / chunk_vecs / path_ids; bumped to `SCHEMA_VERSION = 2` to add `notes.skipped` + `notes.skip_reason` (per `tauri-cmd-file-index-state`). Slug name kept for stability; the schema-version constant tracks the actual on-disk version |
| `store-version-fail-loud` | done | `core/src/store.rs:17` | no auto-migrate |
| `store-wal-mode` | done | `core/src/store.rs:708` | `pragma_update(None, "journal_mode", "WAL")` plus `synchronous=NORMAL` |
| `store-module-discipline` | done | `core/src/store.rs` | rusqlite confined to one module |
| `chunker-heading-bounded` | done | `core/src/chunker.rs:32` | pulldown-cmark walk |
| `chunker-soft-size-1200` | done | `core/src/chunker.rs:16` | |
| `chunker-code-blocks-whole` | done | `core/src/chunker.rs:98` | |
| `chunker-frontmatter-strip` | done | `core/src/chunker.rs:152` | |
| `chunker-heading-path` | done | `core/src/chunker.rs:93` | breadcrumb stored, not yet used in ranking |
| `embedder-fastembed-bge-small` | done | `core/src/embed.rs` | bge-small-en-v1.5, 384 dims |
| `embedder-version-tag` | done | `core/src/embed.rs:18` | embedder_version on notes row |
| `embedder-spawn-blocking` | done | `core/src/indexer.rs:194` | model load + embed off async pool |
| `embedder-batch-64` | partial | `core/src/indexer.rs` | batching exists; the `[indexing].batch_size` config key (declared in `settings-section-indexing`) is not yet plumbed into the indexer task — the value is loaded but not consumed |
| `embedder-platform-data-dir` | done | `core/src/embed.rs:65` | `directories` crate |
| `embedder-module-discipline` | done | `core/src/embed.rs` | trait Embedder; no fastembed leakage; same boundary applies to `llm` crate when `embedder-llm-crate-backed` lands |
| `embedder-llm-crate-backed` | planned | `core::embed::LlmEmbedder` impl wrapping graniet/`llm`'s `EmbeddingProvider`; supports OpenAI / Ollama / Google / Cohere / Mistral / HuggingFace |
| `embedder-config-section` | planned | `[embedder]` config: `provider`, `model`, `api_key_env`, `base_url`; user/vault scoped, same shape as `[llm]` |
| `embedder-version-tag-includes-provider` | planned | `embedder_version` column keys off provider + model so switching provider triggers re-embed via existing fail-loud machinery |
| `embedder-first-run-nonblocking` | done | `core/src/indexer.rs` | vault opens; embed defers until model ready |
| `ingest-startup-scan` | done | `core/src/indexer.rs:303` | mtime/size precheck |
| `ingest-watcher-driven` | done | `core/src/indexer.rs:535` | broadcast → IndexJob |
| `ingest-manual-cli` | partial | `ui/src-tauri/src/lib.rs:174` | Tauri command exists; `hiker reindex` CLI not built |
| `ingest-tx-upsert` | done | `core/src/store.rs:206` | atomic chunks+vecs |
| `ingest-rename-preserve-id` | done | `core/src/indexer.rs:266` | path_ids lookup, no re-embed if hash same |
| `ingest-delete-cascade` | done | `core/src/store.rs:289` | chunks + vec rows + path_ids |
| `ingest-progress-events` | done | `core/src/indexer.rs:55` | hiker:reindex-progress |
| `related-notes-query` | done | `core/src/store.rs:334` | per-chunk KNN, exclude source, group by note |
| `related-notes-snippet` | done | `core/src/store.rs:403` | snippet + heading_path |
| `related-notes-panel-ui` | done | `ui/index.html`, `ui/src/main.ts` (`refreshRelated`, `scheduleRelatedRefresh`) | refresh wired on file-open (`:1411`), debounced-save (`:1526`), and explicit calls (`:353`); cleared on vault swap (`:793`) |
| `tauri-cmd-related-notes` | done | `ui/src-tauri/src/lib.rs:195` | |
| `tauri-cmd-index-status` | done | `ui/src-tauri/src/lib.rs:188` | |
| `tauri-cmd-index` | done | `ui/src-tauri/src/lib.rs:173` | All / Path scopes |
| `walker-symlink-policy` | done | `core/src/vault.rs:163`, `core/src/indexer.rs:792`, `core/src/trash.rs:159` | every `walkdir::WalkDir` call uses `.follow_links(false)` |
| `move-note-core-cmd` | done | `core/src/ops.rs` (`move_note`), `core/src/vault.rs` (`move_note`) | `core::ops::move_note` owns watcher suppression + `IndexJob::Move` send/await; indexer-side `core::vault::move_note` runs the atomic fs rename + index update on the owned store. Folder walk lives in `drag-and-drop-move` |
| `create-note-core-cmd` | done | `core/src/vault.rs` (`Vault::create_note`) | empty file, errors on collision (auto-suffix is the caller's job) |
| `tauri-cmd-file-index-state` | done | `ui/src-tauri/src/lib.rs` (`index_state_for`, `IndexState`), `core/src/indexer.rs` (`IndexerHandle::is_pending`) | Unsupported via `is_indexable_path`; Queued from indexer's pending-paths set; Skipped + Indexed from `notes` row. Schema bumped to v2 to add `notes.skipped` + `notes.skip_reason` (`store-schema-v1` row covers the v1 baseline; the v2 columns + persistence ride on this slug). Indexer now persists Skipped rows for "file too large" and "not UTF-8" branches in `process_upsert`; `Store::upsert_skipped` handles the row + chunk cleanup |
| `reindex-all-action` | done | `ui/src/main.ts` (tree-actions menu "Reindex all" entry) | calls `invoke("index", { scope: { kind: "all" } })`; no confirm modal |
| `reindex-current-file-action` | done | `ui/src/main.ts` (tree-actions menu "Reindex this file" entry) | calls `invoke("index", { scope: { kind: "path", rel: currentPath } })`; greyed when no real file is active (also when previewing a trash entry) |
| `reindex-rebuild-action` | planned | — | destructive UI rebuild (drop + recreate schema then reindex); deferred to settings page per `settings-section-indexing` |
| `tauri-cmd-chunks-for-path` | done | `ui/src-tauri/src/lib.rs` (`chunks_for`), `core/src/store.rs` (`ChunkBounds`, `Store::chunk_bounds_for`) | empty vec for unindexed / never-indexed paths; SELECT omits chunk text so the wire payload stays small |


## Live preview (live-preview.md)

| Slug | Status | Evidence | Notes |
| ---- | ------ | -------- | ----- |
| `live-preview-tier1-scope` | done | `ui/src/editor/livePreview/index.ts` | Tier-1 plugin walks the lang-markdown syntax tree and emits decorations; no widgets/media/math |
| `live-preview-cursor-line-reveal` | done | `ui/src/editor/livePreview/index.ts` (`computeActive`, `isRangeActive`) | active-lines set rebuilt per `selectionSet`; `isRangeActive` matches by line number first |
| `live-preview-selection-reveal-all` | done | `ui/src/editor/livePreview/index.ts` (`computeActive`, `isRangeActive`) | non-empty ranges tracked separately; selection-overlap check runs after the line check; multi-cursor unions naturally |
| `live-preview-default-on` | done | `ui/src/main.ts` (`livePreviewExtensionForPath`, `livePreviewEnabled`) | `livePreviewEnabled` defaults to `true`; the live-preview compartment activates whenever the active path is markdown. View menu's "Live preview" entry (`view-live-preview-toggle`) flips it via `setLivePreviewEnabled` |
| `live-preview-built-on-lang-markdown` | done | `ui/src/editor/livePreview/index.ts` | single-file plugin; only deps are `@codemirror/{language,view,state}` (already transitive) plus `@codemirror/lang-markdown` |
| `live-preview-disabled-non-md` | done | `ui/src/main.ts` (`languageExtensionForPath`) | bundled into the language compartment's extension array; non-md paths return `[]`, so the plugin reconfigures out as a side effect of language selection |
| `live-preview-marker-fade-inline` | done | `ui/src/editor/livePreview/index.ts` (StrongEmphasis/Emphasis/Strikethrough/InlineCode branch) | bold weight, italic, strike, monospace inline-code styling stays; `EmphasisMark`/`StrikethroughMark`/`CodeMark` children fade |
| `live-preview-link-url-fade` | done | `ui/src/editor/livePreview/index.ts` (Link branch) | text styled via `cm-lp-link`; brackets + url + parens fade as one span (`marks[2].from..marks[3].to`) |
| `live-preview-heading-style-fade-marker` | done | `ui/src/editor/livePreview/index.ts` (ATXHeading branch) | line decoration sets `cm-lp-h{1..6}`; HeaderMark + trailing space fade off-line; setext intentionally untouched per spec |
| `live-preview-code-fence-block-reveal` | done | `ui/src/editor/livePreview/index.ts` (FencedCode branch) | per-block reveal: `isRangeActive(node.from, node.to)` covers cursor-anywhere-inside *and* selection-overlap with the whole block |
| `live-preview-block-markers-keep` | done | `ui/src/editor/livePreview/index.ts` | blockquotes and lists are intentionally not visited; their markers render as raw source — no fade emitted |
| `live-preview-frontmatter-passthrough` | done | `ui/src/editor/livePreview/index.ts` (frontmatter detection block in `buildDecorations`) | detects leading `---` … `---`/`...` block and applies `cm-lp-frontmatter` line decorations (muted, monospace). 200-line scan cap; no marker fading; no kv parsing — all per spec. Avoids a custom `MarkdownConfig` since lang-markdown emits no FrontMatter node by default |


## Watcher (watcher.md)

| Slug | Status | Evidence | Notes |
| ---- | ------ | -------- | ----- |
| `watcher-per-vault` | done | `core/src/watcher.rs:52` | recursive, lifecycle bound to vault |
| `watcher-debounce-200ms` | done | `core/src/watcher.rs:23` | |
| `watcher-event-normalized` | done | `core/src/watcher.rs:28` | Created/Modified/Deleted/Renamed |
| `watcher-rename-pairing` | done | `core/src/watcher.rs:106` | unpaired → Created/Deleted |
| `watcher-suppress-self-writes` | done | `core/src/watcher.rs` (`Watcher::suppress`) | TTL-500ms map; bridge thread filters events whose path is currently suppressed |
| `watcher-ignore-hardcoded` | done | `core/src/watcher.rs:143` | .hiker/, .git/, dotfiles, swap files |
| `watcher-symlink-policy` | done | `core/src/watcher.rs` (`has_symlink_ancestor`, called from `normalize`) | events whose path has a symlink ancestor under the canonical vault root are dropped at the normalize step, so the indexer never sees content reached through an in-vault symlink regardless of how notify resolves it on the host platform |
| `watcher-broadcast-channel` | done | `core/src/watcher.rs:54` | tokio broadcast |
| `watcher-bridge-to-indexer` | done | `ui/src-tauri/src/lib.rs:114` | |
| `watcher-bridge-to-frontend` | done | `ui/src-tauri/src/lib.rs:122` | hiker:file-changed |
| `watcher-editor-reload-clean` | done | `ui/src/main.ts` (`hiker:file-changed` listener, modified+clean branch) | silent reload via `read_file_with_hash` when fresh hash differs |
| `watcher-editor-conflict-dirty` | done | `ui/src/main.ts` (`handleWatcherConflictDirty`) | proactive Keep/Take/Cancel modal; Keep+Cancel leave buffer alone (next save re-prompts via `pre-write-drift-check`); re-entry guard prevents stacked modals |
| `watcher-editor-deleted-buffer` | done | `ui/src/main.ts` (`hiker:file-changed` listener, deleted branch) | clean → close buffer + "removed externally" toast; dirty → keep buffer + "save to recreate" toast |
| `watcher-editor-renamed-followup` | done | `ui/src/main.ts` (`hiker:file-changed` listener, renamed branch) | silently sets `buffer.path = ev.to`; tree row stays stale until manual refresh / `tree-refresh-watcher` |
| `watcher-overflow-rescan` | done | `core/src/watcher.rs` (`FileEvent::Overflow`, `need_rescan` branch in bridge thread), `core/src/indexer.rs` (`route_watcher_events` Overflow → `IndexJob::FullScan`), `ui/src-tauri/src/lib.rs` (`hiker:watcher-overflow` emit), `ui/src/main.ts` (toast listener) | kernel-level rescan flag (Linux Q_OVERFLOW / macOS MustScan / Windows buffer overrun) surfaces as `FileEvent::Overflow`; indexer kicks a non-forced full scan, frontend shows "watcher fell behind — rescanning…" toast and the existing reindex-progress events drive the status bar from there |
| `watcher-config-ignore-file` | planned | — | `vault/.hiker/ignore` (deferred per spec) |


## Settings (settings.md)

v1 surface landed: TOML loader + auto-create + targeted write-back, editor / indexing / vault sections wired into the existing UI. No generalized settings UI in v1 — the existing in-app toggles persist via `settings-write-back`; everything else is hand-edited TOML and applied on restart. The keymap section stays planned.

| Slug | Status | Evidence | Notes |
| ---- | ------ | -------- | ----- |
| `settings-user-config-toml` | done | `core/src/config.rs` (`ConfigPaths::resolve`) | platform config dir via `directories`; auto-created with full defaults on first run |
| `settings-vault-config-toml` | done | `core/src/config.rs` (`ConfigPaths::resolve`, `Config::load`) | `vault/.hiker/config.toml`; deep-merged over user, vault wins on every key |
| `settings-load-once-at-startup` | done | `core/src/config.rs` (`Config::load`), `ui/src-tauri/src/lib.rs` (`pick_vault_inner` calls `Config::load`; `VaultSession.config: RwLock<Config>`) | one read per vault open; in-app writes update disk and the in-memory copy together |
| `settings-strict-load` | done | `core/src/config.rs` (every section uses `#[serde(deny_unknown_fields)]`; `Config::load` checks `schema_version` before deserialize, then validates `indexing.model` + `batch_size`) | unknown keys + type mismatches abort with file:line; mismatch path mirrors `store-version-fail-loud` |
| `settings-defaults-in-code` | done | `core/src/config.rs` (`Default` impl on `Config`, `EditorConfig`, `IndexingConfig`, `VaultConfig`, `TreeConfig`) | every field `serde(default)`-decorated; one `Default` impl per struct is the source of truth |
| `settings-auto-create-defaults` | done | `core/src/config.rs` (`read_or_create`, `write_defaults`, `atomic_write`) | missing TOML at load → atomic-write a fresh file with header comment + serialized defaults; tested at `auto_create_writes_defaults` |
| `settings-write-back` | done | `core/src/config.rs` (`Config::set`, `apply_patch`, eligible-key tables), `ui/src-tauri/src/lib.rs` (`set_setting` Tauri cmd), `ui/src/main.ts` (`persistSetting` plus call sites in View menu, sort menu, sidebar/related toggles, trash header) | closed eligible-key set; `toml_edit::DocumentMut` patches in place so user comments + unknown keys survive; tested at `write_back_patches_in_place_preserving_comments` |
| `settings-section-editor` | done | `core/src/config.rs` (`EditorConfig`) | `render_txt_as_markdown`, `live_preview`, `word_wrap`, `show_line_numbers`, `show_whitespace`, `show_chunk_boundaries`, `tab_size` |
| `settings-section-indexing` | done | `core/src/config.rs` (`IndexingConfig`) | `model`, `batch_size`, `ignored_paths` declared and validated; consumers (embedder, walker filter) still read in-code defaults — config keys exist but aren't yet plumbed through |
| `settings-section-vault` | done | `core/src/config.rs` (`VaultConfig`) | `recent`, `default`, `sidebar_open`, `related_open`, `trash_expanded`, `tree.sort_by` |
| `settings-default-vault-autoopen` | done | `core/src/config.rs` (`Config::user_default_vault`), `ui/src-tauri/src/lib.rs` (`get_default_vault`, `open_vault_at` — single shared open path; no backend dialog), `ui/src/main.ts` (`bootstrapDefaultVault` reads default, auto-opens; falls through to JS dialog via `@tauri-apps/plugin-dialog` on `HikerError::NotFound` with toast) | folder picker is a JS-only concern per spec; backend exposes `open_vault_at(path)` as the single shared entry point. Missing path returns `not_found` → toast + picker fall-through, never clears the setting |
| `settings-section-keymap` | planned | — | stub; loader for `keymap.<binding-id> = "<chord>"` deferred until first user remap |
| `settings-schema-version` | done | `core/src/config.rs` (`SCHEMA_VERSION`, mismatch check in `Config::load`) | top-level integer; mismatch hard-fails with "schema_version N, this binary expects M" |


## Clustering (clustering.md)

All planned. The build engine consumed by `suggestions.md`. Lands post-v1.

| Slug | Status | Notes |
| ---- | ------ | ----- |
| `cluster-build-recursive` | planned | RAPTOR-shaped: cluster → summarize → embed summary → recurse |
| `cluster-note-embeddings` | planned | mean-pool of chunk embeddings, cached on `notes` row |
| `cluster-hdbscan` | planned | HDBSCAN over GMM; outlier-handling + determinism |
| `cluster-algorithm-selectable` | planned | per-vault `cluster.algorithm` config: `hdbscan` / `gmm` / `hybrid` |
| `cluster-hybrid-outlier-recovery` | planned | HDBSCAN clusters + GMM on outliers; soft-member tagging |
| `cluster-place-greedy-descent` | planned | greedy centroid-descent classifier; engine for saved-tree triage in `suggestions.md` |
| `cluster-chunk-thread-hint` | planned | secondary: cross-note chunk clusters surface as "thread" hints to user (not auto-trails) |
| `cluster-chunk-multitopic-flag` | planned | secondary: chunks scattered across clusters → split candidate |
| `cluster-summarize-llm` | planned | one LLM call per cluster per level; small local model OK |
| `cluster-name-from-summary` | planned | LLM proposes 3–6 word name + 1–3 sentence summary + confidence |
| `cluster-summarize-fallback-tfidf` | planned | template-based naming if LLM unavailable |
| `cluster-tree-output` | planned | `ClusterTree` shape consumed by `suggestions.md` |
| `cluster-module-discipline` | planned | `core::cluster` + `core::summarize` modules; trait-bounded swaps |


## Suggestions (suggestions.md)

| Slug | Status | Notes |
| ---- | ------ | ----- |
| `suggestions-one-shot-flow` | planned | `hiker suggest` produces a markdown proposal; user reviews and applies; never auto-applies |
| `suggestions-proposal-md` | planned | proposal file is markdown-with-checkboxes at `.hiker/proposals/<ts>.md`; per-line granularity, hand-editable, becomes audit log |
| `suggestions-apply-cmd` | planned | `hiker suggest apply <proposal>` walks checked items; calls `move_note` / writes tag-frontmatter |
| `suggestions-rejection-history` | planned | `.hiker/suggestion-history.yaml`; per-(cluster-fingerprint, note, action) rows with TTL so rejected suggestions don't reappear |
| `suggestions-mode-move` | planned | apply suggestion as filesystem rename via `move_note`; auto-creates target folder |
| `suggestions-mode-tag` | planned | apply suggestion as a frontmatter tag write; no fs move |
| `suggestions-tag-field-configurable` | planned | `[suggestions] tag_field` config; default `hiker.suggested_tags`, can be set to `tags` to use the regular list |
| `triage-saved-tree` | planned | `hiker suggest save` persists centroids+names+actions to `.hiker/saved-tree.yaml`; one tree per vault |
| `triage-classifier-engine` | planned | greedy descent (`cluster-place-greedy-descent`) over the saved tree; cheap, no LLM, no re-cluster |
| `triage-confidence-tiers` | planned | high → auto-apply with Undo; medium → queue for review; low → leave in inbox; thresholds per-vault |
| `triage-auto-undo-toast` | planned | high-confidence auto-applies show toast with 10s Undo; Undo logs to rejection history |
| `triage-pending-review-panel` | planned | UI panel for medium-confidence triage suggestions; accept/reject per item |
| `suggestions-folder-pin` | planned | deferred; folder-level pin to exclude from suggestions and triage moves |


## Txt ingest (txt-ingest.md)

| Slug | Status | Evidence | Notes |
| ---- | ------ | -------- | ----- |
| `txt-extension-recognized` | done | `core/src/indexer.rs` (`is_indexable_path`, `process_upsert` chunker dispatch) | walker, watcher router, and per-file chunker dispatch all consult `is_indexable_path`; `Chunker` trait + `MarkdownChunker`/`TxtChunker` live under `core::chunker` |
| `txt-render-as-markdown-default` | done | `ui/src/main.ts` (`languageExtensionForPath`, `renderTxtAsMarkdown`, seeded in `openVault` from `get_settings`) | per-vault default loaded from `editor.render_txt_as_markdown`; session override is `view-render-txt-as-markdown-toggle` |
| `txt-chunker-paragraph-splits` | done | `core/src/chunker/txt.rs` (`chunk_txt`, `build_sections`) | Layer 1 baseline subsumed by Layer 3 sentence-packing within sections |
| `txt-chunker-structure-heuristics` | done | `core/src/chunker/txt.rs` (`detect_headings`, `is_setext_underline`, `looks_like_all_caps_heading`) | ALL-CAPS + setext `===`/`---`; lists/blockquotes flow as content per spec |
| `txt-chunker-sentence-pack` | done | `core/src/chunker/txt.rs` (`sentence_pack_range`, `segment_sentences`) | ~1200-char soft cap shared with markdown chunker |
| `txt-chunker-guardrails` | done | `core/src/chunker/txt.rs` (`detect_code_regions`, `last_caps_promotion` window) | code-region exclusion + max-one ALL-CAPS promotion per 5-line window; period+space rule lives in `segment_sentences` |
| `txt-abbreviation-allowlist` | done | `core/src/chunker/txt.rs` (`abbreviations::ALL`, `is_abbreviation_ending_at`) | `Mr.`/`Dr.`/`e.g.`/`i.e.`/`etc.`/... |


## Observability (observability.md)

v1 slice is just "init `tracing` and write to a file." Spans, in-app viewer, and the frontend bridge are deferred until file logs stop answering questions.

### v1

| Slug | Status | Notes |
| ---- | ------ | ----- |
| `obs-tracing-baseline` | done | `core/src/observability.rs` (`init_tracing`); `ui/src-tauri/src/lib.rs` (`pick_vault` calls it). All prior `eprintln!` in `core::indexer` / `core::store` / `core::watcher` converted to `tracing::{debug,info,warn,error}!` |
| `obs-log-files` | done | `core/src/observability.rs` (`init_tracing`) — file layer writes `<vault>/.hiker/logs/hiker.log` alongside the stderr layer |
| `obs-log-rotation` | done | `core/src/observability.rs` — `tracing_appender::rolling::Builder` with `Rotation::DAILY` + `max_log_files(7)` |
| `obs-error-context` | done | pattern applied throughout core — e.g. `core/src/indexer.rs` upsert err branch uses `error!(error = %e, path = %rel_path, ...)`; no string-interpolated context |
| `obs-no-content` | done | discipline-only; v1 events log paths/reasons/counts only, never note body text. Documented at the top of `core/src/observability.rs` |
| `obs-no-secrets` | done | discipline-only; nothing in v1 logs auth tokens or API keys (no such config exists yet). Documented at the top of `core/src/observability.rs` |

### Deferred (post-v1)

| Slug | Status | Notes |
| ---- | ------ | ----- |
| `obs-spans-pipeline` | planned | spans wrap pipeline stages, not individual fn calls; deferred — adopt when flat events get hard to correlate |
| `obs-env-filter` | planned | `HIKER_LOG` env var drives `EnvFilter`; defaults `info,hiker=debug`; deferred — v1 hardcodes the default |
| `obs-instrument-watcher` | planned | one span per debounced event; raw events at `trace!` only; deferred behind `obs-spans-pipeline` |
| `obs-instrument-indexer` | planned | top-level span per job; child spans for chunk / embed / store; deferred behind `obs-spans-pipeline` |
| `obs-instrument-embed` | planned | span with `batch_size`, elapsed; per-batch event; deferred behind `obs-spans-pipeline` |
| `obs-instrument-store` | planned | slow-query log at >100ms; no per-call span; deferred (slow-query log itself is a fine v1 add if needed) |
| `obs-instrument-cluster` | planned | top-level span on reconcile; per-level child spans; deferred until clustering lands |
| `obs-tauri-command-spans` | planned | `#[instrument]` on every `#[tauri::command]`; deferred behind `obs-spans-pipeline` |
| `obs-frontend-bridge` | planned | `log_from_frontend` Tauri command emits server-side `tracing` events; deferred until there's a real frontend error worth catching |
| `obs-log-tauri-channel` | planned | custom `tracing` layer fans events to a `tokio::broadcast`; Tauri emits `hiker:log-event`; deferred behind in-app viewer |
| `obs-log-ring-buffer` | planned | server-side ring (default 2000 events) for panel history; `get_log_buffer` Tauri cmd; deferred behind in-app viewer |
| `obs-log-viewer-panel` | planned | collapsible UI panel: live event stream, level + free-text filter, pause/resume, open-log-file button; deferred — file logs cover v1 |
| `obs-test-subscriber` | planned | `core::test_support::init_tracing()` per-test, no global init; deferred until a failing test wants logs |
| `obs-perf-flamegraph` | planned | deferred; one-line `tracing-flame` add when needed |


## Search (search.md)

v2 milestone. Vault-wide hybrid search (lexical FTS5 + semantic) hosted in the repurposed discovery panel; type-ahead with debounce + epoch-cancel; engine traits so tantivy can swap in later. Phases 1–4 landed: backend foundation, panel restructure + persistence, type-ahead + result rendering + click-to-chunk, Ctrl-Space focus + arrow/Enter/Tab/Esc nav. The deferred slugs at the bottom of the table are next; they're scoped follow-ups, not v2 blockers.

| Slug | Status | Notes |
| ---- | ------ | ----- |
| `search-discovery-panel` | done | `ui/index.html` (#discovery), `ui/src/style.css` (#discovery / .discovery-section / #app.related-collapsed), `ui/src/main.ts` (discovery panel block) — right panel renamed to Discovery with input + collapsible search/related sections; `panel-toggle-buttons` (existing) still flips the panel as a whole |
| `search-bar-input` | done | `ui/index.html` (#search-input) + `ui/src/style.css` (#search-input) |
| `search-mode-toggles` | done | `ui/index.html` (#toggle-mode-{semantic,lexical}), `ui/src/main.ts` (`setSearchMode{Semantic,Lexical}`) — S/L pills next to the input; visual `.active` class drives pressed state same as existing toolbar buttons |
| `search-modes-both-off-disabled` | done | `ui/src/main.ts` (`applySearchInputDisabledState`) — both toggles off → input `disabled` + placeholder swaps to "Enable Semantic or Lexical to search" |
| `search-mode-state-persisted` | done | `core/src/config.rs` (`SearchConfig`, `SearchModesConfig` defaults true/true) + eligible-key set entries `search.modes.{semantic,lexical}`; `ui/src/main.ts` seeds from `get_settings` on vault open and persists every flip via `persistSetting` |
| `search-typeahead-debounce` | done | `ui/src/main.ts` (`onSearchInput`, `runSearch`) — 250ms debounce + monotonically-increasing `searchEpoch`; stale responses dropped on the frontend before render. Empty query short-circuits without scheduling and bumps the epoch so any in-flight call drops |
| `search-empty-collapses-results` | done | `ui/src/main.ts` (`applySearchSectionVisibility`) — non-empty query reveals #search-section, empty hides it; related section keeps the panel content as before |
| `search-section-collapsible` | done | `ui/src/main.ts` (`applySectionCollapsed`, `setSearchSectionExpanded`, `setRelatedSectionExpanded`) + eligible keys `search.sections.{results_expanded,related_expanded}`; chevron rotates via `.collapsed` class |
| `search-section-counts` | done | `ui/src/main.ts` — `renderRelated` updates `#related-count`, `renderSearchResults` updates `#search-count` |
| `search-loading-shimmer` | done | `ui/index.html` (#search-spinner) + `ui/src/main.ts` (toggled by `onSearchInput` / `runSearch`) — minimal "…" spinner shown while a debounced query is in flight; styling can be upgraded later |
| `search-related-stays-bound` | done | `ui/src/main.ts` — search wiring leaves `refreshRelated` / `scheduleRelatedRefresh` untouched; the related section still updates only on file-open and debounced-save |
| `search-result-grouped-by-note` | done | `core/src/search.rs` (`group_by_note`) — chunk-level engine output is collapsed to one row per note before fusion, matching `design.md`'s fuse → group rule |
| `search-result-row` | done | `ui/src/main.ts` (`renderSearchResults`, `appendSnippetWithMarks`) + `ui/src/style.css` (`.search-mark`) — title + heading-path + snippet (literal `<mark>` substrings parsed into styled `<span class="search-mark">` nodes, never via innerHTML) + score |
| `search-result-click-opens-chunk` | done | `ui/src/main.ts` (`openSearchHit`, `byteOffsetToCharOffset`) — clicking a result row calls `openFile` then dispatches `EditorView.scrollIntoView` at the chunk's `byte_start`, converted UTF-8 byte → UTF-16 char via `TextEncoder`/`TextDecoder` |
| `search-result-budget` | done | `core/src/search.rs` — `PER_BACKEND_TOP_K = 25`, `FUSED_TOP_K = 20`; configurability deferred to MCP needs |
| `search-keybind-ctrl-space` | done | `ui/src/main.ts` — registered in CM6 keymap (`search.focusInput` / `Ctrl-Space`) so it wins over startCompletion inside the editor, plus a window-level capture-phase keydown listener for the global case (checks `ctrlKey && !metaKey`, so Cmd-Space on macOS stays Spotlight). Both call `focusSearchInput`, which expands the panel if collapsed and selects existing input contents. The keybind registry doesn't yet have a `scope` field — global half lives outside the registry until that refactor lands |
| `search-keyboard-nav` | done | `ui/src/main.ts` (`onResultListKeydown`, `setRovingTabIndex`, `focusRow`, search input keydown handler) + `ui/src/style.css` (`.related-item:focus`) — ↑/↓ within a list, vertical wrap between Search-bottom ↔ Related-top only; Enter triggers the row's click handler; Tab uses roving tabindex (one row per list is reachable) so input → search → related → out flows naturally; Esc in the input clears the query (or blurs if already empty), Esc on a row refocuses the input. ↓ from the input jumps to the first available result row |
| `search-engine-trait` | done | `core/src/search.rs` — `LexicalEngine` + `SemanticEngine` traits with concrete impls in same file; tantivy swap-point preserved |
| `search-fts5-lexical` | done | `core/src/search.rs` (`Fts5LexicalEngine`) |
| `search-fts5-schema` | done | `core/src/store.rs` (`ensure_schema`) — contentless `chunks_fts` + sync triggers on `chunks` (insert/update/delete); schema bumped to `SCHEMA_VERSION = 3` |
| `search-fts5-bm25-snippet` | done | `core/src/search.rs` (`Fts5LexicalEngine::query`) — `ORDER BY bm25` + `snippet(chunks_fts, 0, '<mark>', '</mark>', '…', 32)`; BM25 sign-flipped so higher = better matches the semantic side |
| `search-semantic-existing-vecs` | done | `core/src/search.rs` (`VecSemanticEngine`) — thin wrapper over `Store::knn_chunks_on` |
| `search-query-embed-spawn-blocking` | done | `ui/src-tauri/src/lib.rs` (`search_vault_inner`) — query string embed runs on `tokio::task::spawn_blocking` against the indexer's loaded `Arc<dyn Embedder>`, exposed via the new `IndexerHandle::embedder` accessor (filled by a `OnceCell` after the model loads) |
| `search-rrf-fusion` | done | `core/src/search.rs` (`rrf_fuse`) — k=60, applied when both modes on; group-by-note happens before fuse |
| `search-rebuild-on-schema-bump` | done | covered by `store-version-fail-loud`: opening a v2 db with this binary aborts with a version-mismatch error; user runs the existing reindex flow |
| `search-vault-scope-only` | done | `core/src/search.rs` — engine queries hit every non-skipped chunk in the vault; no scope filter; folder/tag/lifecycle filters stay deferred per spec |
| `search-tauri-cmd` | done | `ui/src-tauri/src/lib.rs` (`search_vault`) returning `SearchResponse { epoch, lexical_hits, semantic_hits, fused }`; both-modes-off / empty-query / model-not-ready short-circuit to empty buckets without erroring |
| `search-mode-options-menu` | planned | right-click on Semantic/Lexical toggle opens a popover with mode-specific options; reuses `openContextMenu`; left-click still toggles on/off |
| `search-lexical-options` | planned | umbrella for the lexical right-click menu's row set; flips persist via `settings-write-back` to `search.lexical.*` |
| `search-lexical-case-sensitive` | planned | post-filter pass on top-25 lexical hits; default off; FTS5 tokenizer stays case-folded |
| `search-lexical-diacritic-sensitive` | planned | post-filter pass; default off; tokenizer keeps `remove_diacritics 2` |
| `search-lexical-prefix-match` | planned | rewrite each query token to `token*` before FTS5 `MATCH`; default off |
| `search-lexical-phrase-mode` | planned | wrap whole query in double quotes for exact-phrase FTS5 match; default off |
| `search-semantic-options` | planned | umbrella for the semantic right-click menu's row set; flips persist via `settings-write-back` to `search.semantic.*` |
| `search-semantic-min-similarity` | planned | 0.00–0.95 slider; drops hits below the cosine floor before fusion; default 0.00 |
| `search-semantic-top-k-override` | planned | numeric override (5–100) of `PER_BACKEND_TOP_K` for the semantic side only; default 25 |
| `search-semantic-recency-bias` | planned | Off/Mild/Strong RRF blend of `notes.mtime` rank into the semantic score; default Off |
| `search-folder-scope` | planned | restrict to vault subtree; deferred |
| `search-lifecycle-filters` | planned | exclude/include archived/redacted/retired; waits on `design.md` lifecycle slugs |
| `search-tag-scope` | planned | filter by frontmatter tag; waits on auto-tag enrichment |
| `search-tantivy-swap` | planned | `LexicalEngine` impl over tantivy; triggered by ranking-quality complaints |
| `search-history` | planned | recent queries dropdown under input |
| `search-result-snippet-context` | planned | expand row to show surrounding chunks |
| `search-multi-vault` | planned | vault-level routing axis from `design.md`; needs multi-vault open first |
| `search-result-pin-as-collection` | planned | promote result set to a saved collection (`design.md` collections) |
| `search-result-multi-select` | planned | checkbox selection on result rows + select-all in section header; per-query state |
| `search-bulk-action-tag` | planned | apply/remove a tag across all results or the multi-select subset; depends on auto-tag enrichment landing first |
| `search-bulk-action-move` | planned | move all results (or multi-select subset) to a folder via `core::ops::move_note`; confirm-with-count |
| `search-authorship-filter` | planned | pill-row filter on user-authored/agent-authored/imported (`hiker.author:`); reads from Provenance index axis |
| `search-source-type-filter` | planned | pill-row filter on source type (md / trail / pdf / epub / image / audio / website / transcript); reads `hiker.type:` |


## LLM (llm.md)

All deferred. Lands with the v3.5 ACP-client + bundled-agent milestone in `design.md` build order. Architectural decisions (ACP-only, three feature types, ToS posture, etc.) live in the spec; the slugs below are concrete implementable features.

| Slug | Status | Notes |
| ---- | ------ | ----- |
| `llm-core-module` | planned | `core::llm` wraps graniet/`llm` crate; multi-provider via single trait + builder; module discipline (only place `llm` crate is imported) |
| `llm-providers-config` | planned | `vault/.hiker/llm.toml` + user-scope fallback; provider/model/api_key_env/base_url/limits; api keys via env vars |
| `llm-basic-agent-loop` | planned | `core::agent` — message-history + tool-dispatch loop on top of `core::llm`; default backend for the chat panel; bounded tool-call iterations |
| `llm-acp-client-optional` | planned | `core::acp` — optional client for external ACP agents (Claude Code / Goose / ...); chat-panel-only; never wired for background or fan-out |
| `llm-context-injection` | planned | when hiker has high-confidence relevant context for an interactive turn, inject it as Embedded Resource (ACP) or in-prompt context (basic agent loop) |
| `llm-disable-mode` | planned | `[llm] enabled = false` turns off background + fan-out + chat panel; MCP server stays available |
| `llm-feature-debounce` | planned | 1–2s coalesce window for save-driven LLM features so save bursts → one prompt |
| `llm-prompts-file-store` | planned | per-feature markdown files; two-tier (user + vault, vault wins); defaults written on first run |
| `llm-prompts-mustache-templating` | planned | `{{var}}` substitution; available placeholders documented per-feature in default file's comment header |
| `llm-prompts-staleness-on-upgrade` | planned | hash bundled defaults; mismatch flags drift in agent log + Prompts tab without clobbering user override |
| `llm-prompts-settings-tab` | planned | settings UI Prompts tab: editable text, default reference, reset, diff, test affordance |
| `llm-prompt-test-button` | planned | "test prompt with sample data" affordance in Prompts tab |
| `llm-audit-log` | planned | `vault/.hiker/agent-log/<YYYY-MM-DD>.jsonl`; one entry per LLM call (any module); daily rotation; full text gated on `[llm.audit] log_full_prompt = true` |
| `llm-cost-transparency` | planned | status-bar indicator of recent LLM activity; click opens audit log viewer |


## Changes log (changes.md)

All planned. Lands with v3 alongside MCP — agent rollback is the load-bearing first consumer; future per-file history view and the future sync layer also build on this substrate.

| Slug | Status | Notes |
| ---- | ------ | ----- |
| `changes-log-table` | done | `core/src/changes.rs` (`ensure_schema`, `SCHEMA_VERSION = 1`) | `.hiker/changes.db` with single `changes` table; indexes on `(path, ts)`, `(author, ts)`, `(ts)`; fail-loud on schema mismatch mirroring `store-version-fail-loud` |
| `changes-store-file` | done | `core/src/changes.rs` (`Changes::open`) | separate from `index.db`; opened at vault open in `ui/src-tauri/src/lib.rs` (`open_vault_at_inner`) |
| `changes-write-path` | done | `core/src/ops.rs` (`create_with_suffix`, `move_note`, `move_folder`, `delete`, `restore`) + `ui/src-tauri/src/lib.rs` (`write_file`, `write_file_checked`) | every UI-driven mutation appends one row through the shared `Arc<Changes>`. Watcher-driven external-write rows (`author='user'`) deferred — sync / external-edit ingestion isn't load-bearing for the v3 widget; the indexer route is the natural future home per spec |
| `changes-query-api` | done | `core/src/changes.rs` (`Changes::{recent, recent_by_author, history_for_path, content_at, previous_content_for_path, count}`) | DTO is `ChangeRow` (no rusqlite leakage); content blob fetched separately via `content_at` |
| `changes-rollback-helper` | done | `core/src/changes.rs` (`previous_content_for_path`, `content_at`) + `ui/src-tauri/src/lib.rs` (`rollback_change`, `restore_snapshot`) | two flavors riding the same primitives per `changes.md` "Rollback": `rollback_change` (change-shaped, walks `previous_content_for_path`, stamps `metadata.rolled_back_from`) for MCP agent rollback per `mcp.md`; `restore_snapshot` (version-shaped, reads `content_at(id)`, stamps `metadata.restored_from`) for the home-page recent-activity widget. Both append a new `'modified'` row; append-only preserved |
| `changes-baseline-on-first-mutation` | done | `core/src/changes.rs` (`Changes::ensure_baseline`, `has_any_for_path`), `ui/src-tauri/src/lib.rs` (called from `write_file` and `write_file_checked` before each save) | first-save edge case: pre-existing vault files have no prior row, so rollback finds nothing. The save path lazy-snapshots the pre-write content as a `'created'` row tagged `metadata.baseline = true` whenever the path has no rows yet. Idempotent — once any row exists, the call no-ops |
| `changes-retention` | done | `core/src/changes.rs` (`gc`) + `ui/src-tauri/src/lib.rs` (`open_vault_at_inner` calls `changes.gc(50)` on open) | per-`(path, author)` keep-N policy with `op='deleted'` rows preserved unconditionally; periodic GC task deferred — v1 runs once at vault open which bounds storage well enough for personal use. `[changes]` config section deferred (no eligible-key set yet) |
| `changes-content-zstd` | done | `core/src/changes.rs` (`SCHEMA_VERSION = 2`, `ZSTD_LEVEL = 3`, encode in `append`, `decode_blob` in `content_at` / `previous_content_for_path`, `migrate_v1_to_v2` runs in-place on open). Migration is a single-tx walk that re-encodes every non-NULL `content` BLOB; deleted rows (NULL content) untouched. Decode failure surfaces as `ChangesError::Corrupt { id, content_hash, message }`. Tests: round-trip with disk-side compression check, empty / NULL handling, v1→v2 migration with mixed (created / modified / deleted) rows |


## MCP (mcp.md)

All planned. v3 milestone — in-process MCP server in `mcp-server/` crate, Streamable HTTP transport, rmcp-backed, read + write tool surface, agent rollback via `core::changes`. Architectural decisions (in-process, decoupled crate, transport choice, localhost-trust auth, etc.) live in the spec; the slugs below are concrete implementable features.

| Slug | Status | Notes |
| ---- | ------ | ----- |
| `mcp-server-crate` | done | `mcp-server/` is now a library crate; `hiker_mcp::start(McpDeps) -> McpServerHandle` spawns an axum task wrapping `rmcp::StreamableHttpService`. `ui/src-tauri/src/lib.rs` (`open_vault_at_inner` → `start_mcp`) brings it up on vault open; the handle drops on session swap, cancelling the task and removing the discovery file. `mcp-server/tests/smoke.rs` exercises the full HTTP path |
| `mcp-config-section` | done | `core/src/config.rs` (`McpConfig`, `McpToolsConfig`, `McpAuditConfig`); strict-load validates the section. Defaults: enabled=true, port=0, discovery_file=`.hiker/mcp.json`, max_top_k=50, tools.writes_enabled=true, tools.allow_redacted_lookup=false, audit.log_full_input=false |
| `mcp-port-discovery` | done | `mcp-server/src/discovery.rs` writes `<vault>/.hiker/mcp.json` on bind; `McpServerHandle::shutdown` (and `Drop`) remove it. OS-assigned ephemeral by default; honors `[mcp].port` for a fixed port. Smoke test asserts both write and removal |
| `mcp-dynamic-capabilities` | partial | mechanism: tool list comes from the rmcp `tool_router`. v3 ships all seven tools unconditionally; the conditional-advertise hook lands when the first feature-gated tool does (trails/landmarks/collections/vision). No `is_available()` predicate yet |
| `mcp-error-model` | done | `mcp-server/src/handler.rs::translate_hiker_err` translates `HikerError` → JSON-RPC error codes. Hiker-specific: 1002 (`note_not_found`), 1003 (`drift`), 1004 (`disabled`), 1005 (`indexer_unavailable`). `1001 vault_not_open` not wired — the server only exists while a vault is open, so `vault_not_open` can't occur over the wire today; reserved for the future "MCP outlives session" mode |
| `mcp-lifecycle-aware` | partial | per-spec, the lifecycle fields (`hiker.archived` / `redacted` / `retired`) aren't yet implemented in hiker so the filter is a no-op. The redacted-body restriction lives on `core::store::get_note` rather than the MCP layer when it lands |
| `mcp-audit-log-mcp-calls` | done | `mcp-server/src/audit.rs::AuditLog` appends one JSONL row per call to `<vault>/.hiker/agent-log/<YYYY-MM-DD>.jsonl` with `surface="mcp-tool-call"`. When `[mcp.audit] log_full_input = false` (default), bulky fields (`content`, `query`, `fields`) are redacted to `{redacted: true, len: N}` |
| `mcp-tool-search-notes` | done | `mcp-server/src/handler.rs::HikerHandler::search_notes` wraps `core::search::query`; returns lexical_hits + semantic_hits + fused. `top_k` clamps the fused bucket to `[mcp] max_top_k`. Embedder unavailability surfaces as `1005 indexer_unavailable` |
| `mcp-tool-get-note` | done | `HikerHandler::get_note`; `detail = digest|snippet|full`. Snippet uses chunk 0 from the store when available, fallback head-of-file otherwise. Missing files return `1002 note_not_found` |
| `mcp-tool-related-notes` | done | `HikerHandler::related_notes` calls `Store::related_notes`; `top_k` capped by `[mcp] max_top_k`. Unindexed source returns an empty vec rather than erroring |
| `mcp-tool-write-note` | done | `core/src/ops.rs::agent_write_note` + `HikerHandler::write_note`. Watcher-suppresses, baselines pre-write content for rollback, writes via `vault.write_file_checked` when `expected_hash` is set, appends `author='agent:mcp'` changelog row carrying the post-op blob, enqueues `IndexJob::Upsert` |
| `mcp-tool-set-frontmatter` | done | new `core/src/frontmatter.rs` (split/merge/assemble); `core::ops::agent_set_frontmatter` reads existing, deep-merges patch via `merge_agent_patch`, stamps `hiker.author: agent-authored`, routes through `agent_write_note`. Errors `invalid_params` if `fields` isn't a JSON object |
| `mcp-ui-refresh-on-agent-write` | done | `ui/src/main.ts::handleAgentChange` — listener on `hiker:changes-appended` gates on `author.startsWith("agent:")` and runs the same tree-refresh / vault-home / active-buffer-reload sequence the watcher handler does. Dirty buffer is kept (toast surfaces the conflict) rather than prompting modally — agent writes are server-driven so an interrupt prompt would surprise the user |
| `mcp-tool-apply-tag-remove-tag` | done | `core::ops::agent_apply_tag` / `agent_remove_tag` thin wrappers over `agent_set_frontmatter` operating on the `tags` list. Idempotent (no-op when tag is already present / absent) |


## CLI (no spec doc yet)

The CLI is a stub today. Slugs reserved for what's implied by other docs.

| Slug | Status | Notes |
| ---- | ------ | ----- |
| `cli-mv` | planned | shares `move-note-core-cmd` with tree DnD |
| `cli-rm` | planned | shares `delete-note-core-cmd`; soft delete; `--yes` bypasses confirm |
| `cli-trash-list` | planned | enumerate trash manifest |
| `cli-trash-restore` | planned | restore by id or original path |
| `cli-trash-empty` | planned | permanent delete of all trash entries |
| `cli-reindex` | planned | spec'd in index.md ingest pipeline |
| `cli-reindex-rebuild` | planned | drop + recreate schema |
| `cli-eval` | dropped | superseded by external Python tool (`eval-synth-tool`); hiker exposes `cli-query` as the primitive the tool calls |
| `cli-query` | planned | thin CLI primitive that runs a single search/related query and prints results; consumed by the external eval tool until MCP is real |
| `cli-stats` | planned | sanity dashboards (qa.md) |


## QA (qa.md)

All planned; build only when there are real notes to evaluate against.

| Slug | Status | Notes |
| ---- | ------ | ----- |
| `eval-golden-set` | planned | `vault/.hiker/eval.yaml` + `hiker eval` |
| `eval-thumbs-feedback` | planned | per-row up/down → feedback.jsonl |
| `eval-sanity-stats` | planned | chunk-distribution, mean top-1 sim, orphans, mutual-top-1 |
| `eval-synthetic-corpus` | planned | LLM-generated topical notes for bootstrap benchmark; implementation lives in `eval-synth-tool` (external Python), not hiker |
| `eval-synth-tool` | partial | v0 (`gen` subcommand) lives at `tools/eval-synth/eval-synth.py`; topic taxonomy in `topics.yaml`, prompts in `prompts/note.md` + `prompts/note-txt.md`, syntax-paste fixtures in `pastes/<kind>/`. `.md` notes stamp `hiker.provenance: synthetic-corpus` + `hiker.author: imported` per design.md authorship trichotomy. Ground-truth manifest (canonical, including `.txt`) at `<out>/.synth/manifest.jsonl`. Knobs: `--txt-rate` writes a fraction as plain `.txt` (no frontmatter, per txt-ingest.md:105 leading-`---`-is-content rule); `--paste-rate` splices fixture syntax (sql/shell/json/python/tcpdump/regex) into eligible topics — fenced in `.md`, indented/raw/inline in `.txt` to exercise txt-ingest's code-region exclusion (`txt-chunker-guardrails`). Pathology mix (near-dup / topic-drift / very-short / very-long) at ~10%. Runner / scoring / recall@K still deferred until `cli-query` lands |
| `eval-auto-org` | planned | manual-placement holdout + reconcile-history regression |
