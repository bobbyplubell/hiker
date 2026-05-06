# Status

Implementation status of features specced in the other docs. One row per feature, identified by a stable kebab-case slug. Slugs are positional-free — they name the feature, not its location in the spec — so reorganizing sections of `editor.md`, `index.md`, etc. doesn't break references. Code or commit messages can cite a slug (e.g. "implements `pre-write-drift-check`") and the link survives doc reshuffles.

Status values:

- **done** — implemented and exercised
- **partial** — present but incomplete; the gap is named in the notes column
- **planned** — specced, not started

When a feature is reorganized, renamed, or split, update its row here first — this file is the registry. Audited 2026-05-05.


## How to use this file

- **When code implements (or starts implementing) a slug, tag it in a comment.** A short marker near the relevant entry point is enough: `// status: pre-write-drift-check` in Rust, `// status: drag-and-drop-move` in TS. The goal is grep-ability — `rg "status: drag-and-drop-move"` should land you on the implementation.
- **Don't go overboard.** One tag per feature, near the most natural anchor (the public function, the event handler, the top of the relevant module). Don't sprinkle the slug across every helper. If a feature spans several files, tag the obvious entry point and let `status.md` be the index.
- **When you write a new spec, add its features here.** New rows go in the section for the spec doc that owns them, with a slug, status (`planned` to start), and a one-line note. If a feature crosses doc boundaries (e.g. a core command shared by UI and CLI), put it under the spec that defines its semantics and reference the slug from the others.
- **When a feature is renamed, split, or merged, edit `status.md` first**, then update tags in code and references in other docs. Treat the slug like a function name: stable until you deliberately rename it.

### Open meta-tasks

- [ ] Backfill spec docs (`editor.md`, `index.md`, `watcher.md`, `settings.md`, `qa.md`, `design.md`) with inline `[slug]` markers next to each feature definition. One-time mechanical pass, ~75 annotations. Keeps spec → status → code traceable in both directions.


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
| `file-switch-guard-dirty` | done | `ui/src/main.ts:177` | 3-way confirm via window.prompt (rough) |
| `window-close-guard-dirty` | done | `ui/src/main.ts:319` | |
| `pre-write-drift-check` | done | `core/src/vault.rs:99` | re-reads + hashes before write |
| `drift-conflict-modal` | done | `ui/src/main.ts:150` | keep/take/cancel; no diff option |
| `status-bar-layout` | done | `ui/index.html`, `ui/src/style.css` | three regions |
| `status-bar-index-label` | done | `ui/src/main.ts:450` | model loading / indexing / indexed / error |
| `status-bar-path-copy` | planned | — | path-click → clipboard not wired |
| `status-bar-goto-line` | planned | — | line:col is display-only |
| `three-column-layout` | done | `ui/index.html`, `ui/src/style.css` | grid, sides collapsible |
| `panel-toggle-buttons` | done | `ui/index.html:19` | sidebar + related toggles |
| `cm6-extension-order` | done | `ui/src/main.ts:113` | basicSetup → lang → save tracking → keymap |
| `cm6-editor-reuse` | done | `ui/src/main.ts:194` | doc replaced via dispatch on switch |
| `drag-and-drop-move` | planned | — | tree DnD → core `move_note` → fs rename + index update |
| `create-note-button` | planned | — | wide "+ New note" at top of tree; creates `new_note.md` in selected folder, opens immediately, inline-rename with basename pre-selected |
| `tree-refresh-manual` | planned | — | small icon button at top of tree; re-reads dir from disk; backstop for watcher misses |
| `tree-refresh-watcher` | planned | — | auto-refresh tree from watcher events (v2 per watcher.md) |
| `confirm3-real-modal` | planned | — | replace `window.prompt` 3-way with proper modal |
| `help-panel-keybinds` | planned | — | enumerate keybinds.list() |


## Index (index.md)

| Slug | Status | Evidence | Notes |
| ---- | ------ | -------- | ----- |
| `store-sqlite-vec-static` | done | `core/src/store.rs` | bundled rusqlite + sqlite-vec |
| `store-schema-v1` | done | `core/src/store.rs` | notes / chunks / chunk_vecs / path_ids |
| `store-version-fail-loud` | done | `core/src/store.rs:17` | no auto-migrate |
| `store-wal-mode` | partial | `core/src/indexer.rs:154` (comment) | PRAGMA not visibly set in code path |
| `store-module-discipline` | done | `core/src/store.rs` | rusqlite confined to one module |
| `chunker-heading-bounded` | done | `core/src/chunker.rs:32` | pulldown-cmark walk |
| `chunker-soft-size-1200` | done | `core/src/chunker.rs:16` | |
| `chunker-code-blocks-whole` | done | `core/src/chunker.rs:98` | |
| `chunker-frontmatter-strip` | done | `core/src/chunker.rs:152` | |
| `chunker-heading-path` | done | `core/src/chunker.rs:93` | breadcrumb stored, not yet used in ranking |
| `embedder-fastembed-bge-small` | done | `core/src/embed.rs` | bge-small-en-v1.5, 384 dims |
| `embedder-version-tag` | done | `core/src/embed.rs:18` | embedder_version on notes row |
| `embedder-spawn-blocking` | done | `core/src/indexer.rs:194` | model load + embed off async pool |
| `embedder-batch-64` | partial | `core/src/indexer.rs` | batching exists; size not configurable |
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
| `related-notes-panel-ui` | partial | `ui/index.html`, `ui/src/main.ts` | panel exists; verify file-open + debounced-save refresh |
| `tauri-cmd-related-notes` | done | `ui/src-tauri/src/lib.rs:195` | |
| `tauri-cmd-index-status` | done | `ui/src-tauri/src/lib.rs:188` | |
| `tauri-cmd-index` | done | `ui/src-tauri/src/lib.rs:173` | All / Path scopes |
| `walker-symlink-policy` | partial | not visible in code | spec says don't follow; configuration not confirmed |
| `move-note-core-cmd` | planned | — | atomic fs rename + index path update; backs `drag-and-drop-move` and `hiker mv` |


## Watcher (watcher.md)

| Slug | Status | Evidence | Notes |
| ---- | ------ | -------- | ----- |
| `watcher-per-vault` | done | `core/src/watcher.rs:52` | recursive, lifecycle bound to vault |
| `watcher-debounce-200ms` | done | `core/src/watcher.rs:23` | |
| `watcher-event-normalized` | done | `core/src/watcher.rs:28` | Created/Modified/Deleted/Renamed |
| `watcher-rename-pairing` | done | `core/src/watcher.rs:106` | unpaired → Created/Deleted |
| `watcher-suppress-self-writes` | planned | — | spec says TTL-500ms suppression set; not implemented |
| `watcher-ignore-hardcoded` | done | `core/src/watcher.rs:143` | .hiker/, .git/, dotfiles, swap files |
| `watcher-symlink-policy` | partial | not visible in code | spec disables symlink-following on watcher |
| `watcher-broadcast-channel` | done | `core/src/watcher.rs:54` | tokio broadcast |
| `watcher-bridge-to-indexer` | done | `ui/src-tauri/src/lib.rs:114` | |
| `watcher-bridge-to-frontend` | done | `ui/src-tauri/src/lib.rs:122` | hiker:file-changed |
| `watcher-editor-reload-clean` | done | `ui/src/main.ts:524` | silent reload |
| `watcher-editor-conflict-dirty` | planned | — | dirty buffer should fire conflict modal proactively |
| `watcher-editor-deleted-buffer` | planned | — | close-or-toast behavior on deletion |
| `watcher-editor-renamed-followup` | planned | — | update currentPath when active path is renamed |
| `watcher-overflow-rescan` | planned | — | detect notify queue overflow, trigger rescan |
| `watcher-config-ignore-file` | planned | — | `vault/.hiker/ignore` (deferred per spec) |


## Settings (settings.md)

Whole surface is planned. Slugs reserved so they can be cited from code as features land.

| Slug | Status | Notes |
| ---- | ------ | ----- |
| `settings-user-config-toml` | planned | per-user `~/.config/hiker/config.toml` |
| `settings-vault-config-toml` | planned | per-vault `vault/.hiker/config.toml` overrides |
| `settings-section-indexing` | planned | model selection, reindex, batch size, ignores |
| `settings-section-keymap` | planned | overrides for keybind-registry by id |
| `settings-section-editor` | planned | tab size, wrap, theme, font, autosave |
| `settings-section-vault` | planned | recent vaults, default-on-startup |
| `settings-schema-version` | planned | top-of-file version, additive migrations |


## Clustering (clustering.md)

All planned. Lands post-v1, alongside the curated-tree placement feature.

| Slug | Status | Notes |
| ---- | ------ | ----- |
| `cluster-build-recursive` | planned | RAPTOR-shaped: cluster → summarize → embed summary → recurse |
| `cluster-note-embeddings` | planned | mean-pool of chunk embeddings, cached on `notes` row |
| `cluster-hdbscan` | planned | HDBSCAN over GMM; outlier-handling + determinism |
| `cluster-algorithm-selectable` | planned | per-vault `cluster.algorithm` config: `hdbscan` / `gmm` / `hybrid` |
| `cluster-hybrid-outlier-recovery` | planned | HDBSCAN clusters + GMM on outliers; soft-member tagging |
| `cluster-place-greedy-descent` | planned | online per-note placement (also specced in design.md:252) |
| `cluster-placement-provenance` | planned | `hiker.placement: manual / auto:vN / confirmed` written to frontmatter |
| `cluster-manual-via-tree-dnd` | planned | drag-and-drop-move sets `manual` placement, never auto-overridden |
| `cluster-confirm-promotion` | planned | UI to promote `auto:vN` → `confirmed` |
| `cluster-chunk-thread-hint` | planned | secondary: cross-note chunk clusters surface as "thread" hints to user (not auto-trails) |
| `cluster-chunk-multitopic-flag` | planned | secondary: chunks scattered across clusters → split candidate |
| `cluster-summarize-llm` | planned | one LLM call per cluster per level; small local model OK |
| `cluster-name-from-summary` | planned | LLM proposes 3–6 word name + 1–3 sentence summary + confidence |
| `cluster-stable-identity` | planned | Jaccard ≥0.7 on member sets carries cluster id across runs |
| `cluster-history-yaml` | planned | `vault/.hiker/cluster-history.yaml` for cross-run identity |
| `cluster-summarize-fallback-tfidf` | planned | template-based naming if LLM unavailable |
| `cluster-trigger-reconcile-only` | planned | runs on `hiker reconcile`; never automatic in v1 |
| `cluster-tree-output` | planned | `ClusterTree` shape consumed by reconcile flow |
| `cluster-module-discipline` | planned | `core::cluster` + `core::summarize` modules; trait-bounded swaps |


## CLI (no spec doc yet)

The CLI is a stub today. Slugs reserved for what's implied by other docs.

| Slug | Status | Notes |
| ---- | ------ | ----- |
| `cli-mv` | planned | shares `move-note-core-cmd` with tree DnD |
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
