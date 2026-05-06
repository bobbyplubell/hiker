# Bug tracking

Known issues in the codebase. Each row is a kebab-case slug, a one-line description, optional file:line, and (where useful) a brief note on the intended fix. Same conventions as `status.md` — resolve by fixing in code, then strike the row (or remove it). If a bug ends up reshaping a feature, update the feature row in `status.md` instead of carrying both.

**No `// status: bug-…` markers in code.** Unlike feature slugs (which are stable references future readers may want to grep for), bug slugs are short-lived: once the row here is struck, the slug stops being meaningful. Tagging fixes inline just clutters the source. The git history and this file are enough.


## Open

| Slug | File | Notes |
| ---- | ---- | ----- |
| _none_ | | |


## Resolved

Fixed 2026-05-06:

| Slug | Fix |
| ---- | --- |
| `bug-save-button-double-listener` | dropped the early lambda; single click handler now lives at the related-notes refresh site |
| `bug-css-escape-incomplete` | replaced local helper with `CSS.escape()` |
| `bug-not-utf8-pins-index-error` | `process_upsert` returns `Skipped("not utf-8")` instead of `Error` |
| `bug-count-notes-swallows-errors` | seed and refresh paths surface `count_notes` failure as `last_error`; recovers when a later count succeeds |
| `bug-vault-resolve-accepts-absolute` | reject `RootDir`/`Prefix` components in `resolve` with a clear "expected vault-relative" message |
| `bug-queue-count-undercounts` | split `update_queue_count` into in-flight (`rx.len() + 1`) and idle (`rx.len()`) variants; main loop calls in-flight after recv, idle after job completes |
| `bug-progress-counter-unpaired-decrement` | unified `pendingCount + inFlightCount` into a single `outstandingCount`: `scan_complete` adds, every terminal event subtracts, `Started` is a no-op |
| `bug-vault-close-leaks-intervals` | `vaultIsOpen` flag + `startBackgroundIntervals` (called on each `openVault`) clears prior intervals and short-circuits when no vault is open |
| `bug-indexer-shutdown-broken` | `tx` wrapped in `Option<Sender>`; `shutdown` takes + drops it so the task's `recv()` returns `None` |
| `bug-watcher-suppress-ttl-too-short` | bumped `SUPPRESS_TTL` from 500ms → 2s, plus re-suppress after fs ops in `move_note` and `create_note` so the window starts close to when notify surfaces events |
| `bug-watcher-rename-pair-order-assumed` | branch on `RenameMode::From|To|Both` rather than trusting `paths[0]/paths[1]` ordering |
| `bug-watcher-unpaired-rename-as-modified` | `RenameMode::From` → `Deleted`, `RenameMode::To` → `Created`; only the genuine ambiguity case (Any/Other with one path) falls back to `Modified` |
| `bug-embedder-failure-counter-stuck` | embedder-failure drain now emits one `Error` per Upsert/Delete/Rename and replies `Err` to any pending `Move`, so the UI's outstanding counter actually reaches zero |
| `bug-pick-vault-blocks-tokio-worker` | replaced `std::sync::mpsc::recv()` with `tokio::sync::oneshot` so the file dialog suspends without parking a worker |
| `bug-vault-refuse-symlinks` | `Vault::resolve` walks each existing ancestor under the canonical root and rejects any symlink component (file *or* directory). 3 unit tests added |
| `bug-status-bar-path-overflow` | implemented `status-bar-path-basename-tooltip` (basename in `#status-path`, full path in `title=`) plus `ui-no-sibling-pushout` rule (`min-width: 0` + `flex-shrink: 1` on the status-bar regions) |
| `bug-move-note-routes-via-fresh-writer` | added `IndexJob::Move { from, to, reply }` + `IndexerHandle::move_note`; the Tauri move command now suppresses, sends the job via the indexer's owned writer, awaits the oneshot reply, and re-suppresses post-rename. No more parallel writer connection |
| `bug-sqlite-vec-init-transmute-fragile` | rewrote `register_vec_extension` to use rusqlite's typed `RawAutoExtension` alias and `register_auto_extension`; the transmute is still required (sqlite-vec ships an `extern "C" fn()` stub) but the destination type is now documented at the call site, and registration failures log instead of silently dropping |
| `bug-explicit-reindex-noops-on-unchanged-content` | added a `force: bool` field to `IndexJob::Upsert` / `IndexJob::FullScan`, plumbed through `process_upsert` and `run_full_scan`. The `index` Tauri command sets `force: true` so the menu's Reindex actions actually re-embed even when content_hash matches; watcher/startup paths keep `force: false` |
| `bug-related-panel-stale-on-vault-switch` | `openVault` now clears the related-notes panel via `refreshRelated(null)` on vault swap, alongside the existing `indexStateCache` / `inflightStateFetches` resets |
| `bug-too-large-errors-instead-of-skipping` | already fixed before this entry was filed: `process_upsert` at `core/src/indexer.rs:674` returns `UpsertOutcome::Skipped("file too large")` and persists the row via `Store::upsert_skipped`. `IndexerError::TooLarge` no longer exists in the enum. Bug-tracking entry was stale |
| `bug-index-status-shows-model-loading-with-no-vault` | `renderIndexStatus` now early-returns with a blank label + title when `vaultIsOpen` is false. `vaultIsOpen` was hoisted next to `indexStatus` so the early-init render path (CM6 view setup) can read it without a TDZ throw |
