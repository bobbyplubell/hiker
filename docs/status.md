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
| `drag-and-drop-move` | done | `ui/src/main.ts` (`attachDnd`, `performDrop`) | file DnD calls Tauri `move_note`; folder DnD calls Tauri `move_folder` → `core::vault::move_folder` (single fs rename + bulk index path remap via `Store::rename_notes_by_paths`). Empty subfolders move with the rename for free. Buffer follows when the open file is inside the moved subtree |
| `create-note-button` | done | `ui/src/main.ts` (`new-note-btn` handler) | auto-suffix `new-note-N.md` via Tauri `create_note`, opens + inline-renames |
| `tree-refresh-manual` | done | `ui/src/main.ts` (`tree-actions-btn` "Refresh tree" entry) | re-reads dir; restores active highlight; expansion state preserved across refresh via `expandedFolders` set; lives inside the `…` actions menu |
| `tree-refresh-watcher` | done | `ui/src/main.ts` (`scheduleTreeRefreshFromWatcher`, `hiker:file-changed` listener) | 200ms-debounced `refreshTree` on created/deleted/renamed events; modified events are no-ops (tree shape unchanged); manual `tree-refresh-manual` stays as a backstop. Lifted from v2 → v1; watcher.md "Out of scope for v1" entry now stale |
| `tree-double-click-rename` | done | `ui/src/main.ts` (`renderDir` dblclick handler, `beginInlineRename`) | dblclick on file → inline rename via `move_note`; dblclick on folder → inline rename via `move_folder`. Single-click handlers skip when `event.detail >= 2` so the second click of the dbl doesn't toggle/open. Folder rename remaps `expandedFolders` prefixes so expansion state survives, and the open buffer follows when its path is inside the renamed subtree |
| `tree-context-menu` | done | `ui/src/main.ts` (`attachContextMenu`, `openContextMenu`) | row menu: Open / Rename / Delete / Properties (greyed); empty-space menu: New note here |
| `tree-context-delete` | done | `ui/src/main.ts` (`deleteFromTree`) | confirm modal (Cancel default-focus, danger Move-to-trash); folder copy includes recursive note count; closes open buffer if deleted; toast confirmation |
| `tree-context-properties` | partial | `ui/src/main.ts` (`attachContextMenu`) | menu entry stub disabled; opens nothing until frontmatter editing lands |
| `delete-note-core-cmd` | done | `core/src/vault.rs` (`delete_note`), `ui/src-tauri/src/lib.rs` (`delete_note` Tauri cmd) | files + folders; routes through `IndexJob::DeleteNote` so writes flow through the indexer's owned store; rollback on store failure |
| `vault-trash` | done | `core/src/trash.rs` | `<vault>/.hiker/trash/` + `manifest.yaml`; collision suffix `_N`; folder moves preserve relative tree; serde_yml + time deps |
| `vault-trash-restore` | done | `core/src/vault.rs` (`restore_note`), `ui/src-tauri/src/lib.rs` (`restore_trash_entry`), `ui/src/main.ts` (toast Undo button) | reverses delete via fs rename + manifest remove + inline re-ingest; errors if original path now occupied; recreates missing parent; CLI not yet wired |
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
| `embedder-module-discipline` | done | `core/src/embed.rs` | trait Embedder; no fastembed leakage |
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
| `move-note-core-cmd` | done | `core/src/vault.rs` (`move_note`) | atomic fs rename + index update with watcher suppression; folder walk lives in `drag-and-drop-move` |
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
| `watcher-overflow-rescan` | planned | — | detect notify queue overflow, trigger rescan |
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
| `settings-default-vault-autoopen` | done | `core/src/config.rs` (`Config::user_default_vault`), `ui/src-tauri/src/lib.rs` (`try_open_default_vault`, `open_vault_at`), `ui/src/main.ts` (`bootstrapDefaultVault`, called at module init) | reads `vault.default` from user TOML at app bootstrap; missing path warns and falls back to picker without clearing the setting |
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
| `cli-eval` | planned | runs golden-set, reports recall@5/@10/MRR (qa.md) |
| `cli-stats` | planned | sanity dashboards (qa.md) |


## QA (qa.md)

All planned; build only when there are real notes to evaluate against.

| Slug | Status | Notes |
| ---- | ------ | ----- |
| `eval-golden-set` | planned | `vault/.hiker/eval.yaml` + `hiker eval` |
| `eval-thumbs-feedback` | planned | per-row up/down → feedback.jsonl |
| `eval-sanity-stats` | planned | chunk-distribution, mean top-1 sim, orphans, mutual-top-1 |
| `eval-synthetic-corpus` | planned | LLM-generated topical notes for bootstrap benchmark |
| `eval-auto-org` | planned | manual-placement holdout + reconcile-history regression |
