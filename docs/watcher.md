# Watcher

Filesystem watcher that drives both the indexer and the editor's live-buffer drift detection. Built on the `notify` crate; runs in core, not in the host. This doc nails down event handling, debounce, ignored paths, and the dispatch path to frontend + indexer.


## Scope

One watcher per open vault, rooted at the vault path. Started when a vault is opened, stopped when the vault is closed or swapped. Recursive. [watcher-per-vault]
status:: done
touches:: [[code:hiker/watcher]]
note:: recursive, lifecycle bound to vault

External-file ingestion (design.md, External-file ingestion) gets its own watcher instances per configured path/glob, but shares the same event-handling code. v1 watches only the vault root.


## Event sources and consumers

The watcher fans out events to two consumers:

1. **Indexer** — for any file matching `*.md` (and v2+: extractor-handled types). Triggers re-chunk/re-embed via the ingest pipeline in index.md.
2. **Editor frontend** — only for the *currently open file's* path. Drives clean-buffer reload (see Editor integration); a *dirty* buffer is left untouched (the dirty-buffer-vs-external-edit conflict is the deferred case — git's merge markers cover it when git is integrated, a prompt otherwise; `op-log.md` "External edits").

Both consumers receive the same normalized event stream; each filters to the events it cares about.


## Event normalization

`notify` emits raw events that are platform-specific and noisy (multiple events per logical change, partial writes, editor swap-file dances). Normalize before fanning out. [watcher-event-normalized]
status:: done
touches:: [[code:hiker/watcher]]
note:: Created/Modified/Deleted/Renamed

Normalized event types:

```rust
enum FileEvent {
    Created { path: PathBuf },
    Modified { path: PathBuf },
    Deleted { path: PathBuf },
    Renamed { from: PathBuf, to: PathBuf },
    Overflow,
}
```

Normalization steps:

- **Debounce** — coalesce raw events within a 200ms window keyed by path. The last event for a path during the window wins. Catches editor swap-file patterns (`vim`'s write-temp-then-rename, VSCode's atomic save) where a logical save produces 3–5 raw events. [watcher-debounce-200ms]
status:: done
touches:: [[code:hiker/watcher]]
- **Rename pairing** — `notify` emits paired `RenameFrom`/`RenameTo` events on Linux/macOS. Pair them within the debounce window into a single `Renamed`. Unpaired `RenameFrom` after timeout → `Deleted`; unpaired `RenameTo` → `Created`. [watcher-rename-pairing]
status:: done
touches:: [[code:hiker/watcher]]
note:: unpaired → Created/Deleted
- **Drop self-writes** — when the indexer or the editor's save path writes a file, mark the path in a short-lived "we just wrote this" set (TTL 2s). Drop normalized events for those paths to avoid an infinite re-index loop. Indexer/save call `watcher.suppress(path)` immediately before the write and again after it completes, so the TTL window starts close to when notify surfaces the event. [watcher-suppress-self-writes]
status:: done
implements:: [[code:hiker/watcher/impl#[Watcher]suppress]]
note:: TTL-500ms map; bridge thread filters events whose path is currently suppressed · evidence: `core/src/watcher.rs` (`Watcher::suppress`)
- **Drop ignored paths** — see below.

Dispatch order: debounce window closes → normalize → check ignore list → check suppression set → fan out to consumers.


## Ignored paths

Hard-coded ignores (never reach consumers): [watcher-ignore-hardcoded]
status:: done
touches:: [[code:hiker/watcher]]
note:: .hiker/, .git/, dotfiles, swap files

- Anything under `.hiker/` (our own state — index.db, history/ snapshots, pending/, refs/, agent-log/, autosave/)
- Anything under `.git/` if present
- Dotfiles at vault root by default (`.DS_Store`, config dirs left by other markdown apps, etc.)
- Files matching `*.tmp`, `*.swp`, `*~`, `4913` (vim's permission-probe file), `.#*` (emacs lock files)

Configurable ignores:

- A vault-root **`.hikerignore`** (plus the project's own `.gitignore`, and `[indexing] ignored_paths` from config) layers gitignore semantics — nesting, negation (`!`), anchoring — on top of the hard-coded list, via the composed matcher in `core::ignore` (`Matcher` / `is_ignored_in`). **Note-protection invariant:** layers 2–4 only ever exclude NON-note files — a `.md`/`.markdown` note can be excluded only by the hard-coded `.git/`/`.hiker/` internals, never by an ignore file. This is what makes a code repo safe to keep inside a vault: build trees, vendored deps, and test-fixture `.txt` are excluded while authored notes are protected. [watcher-config-ignore-file]
status:: done
implements:: [[code:hiker/ignore/Matcher]], [[code:hiker/ignore/impl#[Matcher]is_ignored]], [[code:hiker/ignore/is_ignored_in]], [[code:hiker/ignore/register]], [[code:hiker/oplog/lifecycle/impl#[OpLog]forget_document]]
verifies:: [[code:hiker/ignore/tests/gitignored_code_excluded_note_protected_hardlist_excluded]], [[code:hiker/ignore/tests/hikerignore_layer_applies]], [[code:hiker/ignore/tests/nested_repo_allowlist_keeps_docs_excludes_rest]], [[code:hiker/vault/tests/walk_indexable_files_honors_composed_ignore_matcher]]
note:: shipped as vault-root `.hikerignore` (not the originally-planned `vault/.hiker/ignore`), composed with `.gitignore` + config `ignored_paths` in `core/src/ignore.rs` over the `ignore` crate's `Gitignore`. The composed matcher is the **single ignore policy across every ingest seam** (the seam-unification pass): the indexer full-scan walk (`indexer/mod.rs::walk_vault`), the watcher registration walk (`should_watch`), the watcher **event** path (`normalize`/`normalize_rename_fallback` via `event_ignored`), `vault::list_dir`, `vault::walk_indexable_files` (the op-log seed / reconcile / bulk-op seam), and the `process_upsert` guard all consult `ignore::is_ignored_in`. The legacy hard-coded-only `watcher::is_ignored` survives only as layer 1 inside the matcher. Four behaviors round out the feature: **(1) allowlist** — "keep a nested repo's docs but exclude the rest" uses gitignore allowlist form `hiker/*` + `!hiker/docs/` (use `dir/*`, NOT `dir/`/`dir/**`, so re-inclusion isn't blocked); this prunes sibling subtrees at the directory level (watcher doesn't watch them, indexer doesn't descend) while keeping docs, both ends satisfied without a separate allowlist feature. **(2) `.txt` is reference content, NOT note-protected** — only `.md`/`.markdown` carry the protection invariant, so `.txt` (e.g. golden test fixtures) CAN be excluded by an ignore file; a genuine `.txt` note can therefore be ignored away, by design. **(3) live-refresh** — editing the vault-root `.hikerignore`/`.gitignore` rebuilds the matcher (`ignore::register`) and kicks a non-forced rescan, wired in the app's `drain_fs_events`. **(4) retroactive prune** — at indexer startup `prune_ignored_tracked_docs` untracks docs seeded before an ignore rule existed: for each tracked op-log doc whose file still exists but is now ignored, `OpLog::forget_document` drops the in-memory doc state + its `.pending` queue (NOT a tombstone-to-trash; the file is left untouched) and the search-index rows are deleted

Indexer's own writes to `.hiker/index.db` would loop forever without the `.hiker/` ignore, so this is load-bearing, not a nicety.


## Dispatch to consumers

Core exposes a broadcast channel (`tokio::sync::broadcast::channel<FileEvent>`) that anyone can subscribe to. Two subscribers in v1: [watcher-broadcast-channel]
status:: done
touches:: [[code:hiker/watcher]]
note:: tokio broadcast

1. **Indexer task** — filters to `*.md` events, sends matching `IndexJob`s into its work queue. See index.md. [watcher-bridge-to-indexer]
status:: done
2. **Frontend bridge** — a host task that subscribes and forwards events to the app via `emit("watcher file events", payload)`. The frontend filters to the open-buffer path; everything else is dropped client-side. Pushing all events keeps the bridge stateless so a future tree-view-refresh consumer reuses the same stream. [watcher-bridge-to-frontend]
status:: done
note:: watcher file events

Wire shape to the frontend: the normalized `FileEvent` serialized with a `kind` discriminant, a vault-relative `path`, and a `from` (renamed only).


## Editor integration

What's built: clean-buffer reload only. On a `Modified` event for an open buffer that is **clean**, the buffer reloads from disk silently (`maybe_reload_clean_buffer`); a **dirty** buffer is left untouched (the dirty-vs-external conflict is deferred — `op-log.md` "External edits"). `Overflow` only clears the dir cache. Deleted and renamed buffers are not acted on. [watcher-editor-reload-clean]
status:: done
touches:: [[code:hiker/panels/buffer]]
note:: silent reload via `read_file_with_hash` when fresh hash differs · evidence: `app/src/panels/buffer/mod.rs` (watcher file events handling, modified+clean branch)

The fuller editor-conflict matrix is now wired:

- **Modified + dirty → conflict modal.** A proactive Keep / Take / Cancel modal; Keep and Cancel leave the buffer alone (the next save re-prompts via [[spec:pre-write-drift-check]]); a re-entry guard prevents stacked modals. [watcher-editor-conflict-dirty]
status:: done
touches:: [[code:hiker/panels/buffer]]
note:: evidence: `app/src/panels/buffer/mod.rs` (watcher conflict handling)
- **Deleted → toast.** A clean buffer closes with a "removed externally" toast; a dirty buffer is kept with a "save to recreate" toast. [watcher-editor-deleted-buffer]
status:: done
touches:: [[code:hiker/panels/buffer]]
note:: evidence: `app/src/panels/buffer/mod.rs` (watcher file events handling, deleted branch)
- **Renamed → silent path follow-up.** The buffer's path is updated silently to follow the new path; the tree row stays stale until a manual refresh / [[spec:tree-refresh-watcher]]. [watcher-editor-renamed-followup]
status:: done
touches:: [[code:hiker/panels/buffer]]
note:: evidence: `app/src/panels/buffer/mod.rs` (watcher file events handling, renamed branch)

**Still deferred (not built)** — `bug-watcher-editor-conflict-matrix-unbuilt`. An `Overflow` "watcher fell behind, scanning" toast.


## Indexer integration

Indexer subscribes to the same broadcast and queues `IndexJob`s:

- `Created`, `Modified` for `*.md` → upsert job
- `Deleted` for `*.md` → delete job (remove note + chunks + vecs)
- `Renamed` for `*.md` → rename job (update `notes.path` and `path_ids`, no re-embed if content hash unchanged)

Non-markdown events are ignored by the v1 indexer; v2+ extractor types subscribe to additional patterns through the same dispatch.


## Lifecycle

- **Vault open** — spawn the watcher, start the indexer task, kick a startup scan in parallel. The op log does **no** startup full-vault rehash: `accepted` is lazy-loaded from each `.md` on first open, so an edit made while hiker was closed is simply read fresh (the `.ops` startup-reconcile machinery was deleted with the history engine — see `op-log.md` "External edits").
- **Vault close / swap** — drop the watcher (tokio task aborts when its handle drops), close the indexer's connection, flush any in-flight broadcast events.
- **App shutdown** — same as close; no special teardown needed.

The watcher handle lives in the per-vault state held by the host. Vault-swap drops the old core and constructs a new one — clean separation, no risk of cross-vault event leakage.


## Failure modes

- **notify queue overflow** — long bursts (mass file copy, git checkout) can exceed notify's internal buffer. The indexer should trigger a startup-style rescan to catch up; today `FileEvent::Overflow` only clears the dir cache (the rescan + "watcher fell behind" toast are deferred, `bug-watcher-editor-conflict-matrix-unbuilt`). [watcher-overflow-rescan]
status:: done
touches:: [[code:hiker/indexer]], [[code:hiker/panels/buffer]], [[code:hiker/watcher]]
note:: kernel-level rescan flag (Linux Q_OVERFLOW / macOS MustScan / Windows buffer overrun) surfaces as `FileEvent::Overflow`; indexer kicks a non-forced full scan, frontend shows "watcher fell behind — rescanning…" toast and the existing reindex-progress events drive the status bar from there · evidence: `core/src/watcher.rs` (`FileEvent::Overflow`, `need_rescan` branch in bridge thread), `core/src/indexer.rs` (`route_watcher_events` Overflow → `IndexJob::FullScan`), `app/src/panels/buffer/mod.rs` (toast handling)
- **Network filesystem** — notify's events are unreliable on NFS/SMB. v1 keeps the pre-write drift check as the trustworthy fallback. Long-term: a polling fallback mode for paths flagged as networked.
- **Permissions** — file becomes unreadable mid-watch (parent dir chmod 000). Watcher emits an error; indexer marks the note as `orphaned` (per the design.md missing-source convention) and stops trying until the next event.
- **Symlink loops** — `notify` follows symlinks by default. v1 disables symlink-following on the watcher to avoid surprise. External-file ingestion in v2 will revisit this with an explicit allowlist. [watcher-symlink-policy]
status:: done
touches:: [[code:hiker/watcher]]
note:: events whose path has a symlink ancestor under the canonical vault root are dropped at the normalize step, so the indexer never sees content reached through an in-vault symlink regardless of how notify resolves it on the host platform · evidence: `core/src/watcher.rs` (`has_symlink_ancestor`, called from `normalize`)


## Out of scope for v1

- External-path watchers (configured globs outside vault root)
- Polling fallback for network filesystems
- User-configurable ignore file
- Cross-vault event routing (multi-vault support is a v3+ concern)
