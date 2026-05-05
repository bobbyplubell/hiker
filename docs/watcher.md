# Watcher

Filesystem watcher that drives both the indexer and the editor's live-buffer drift detection. Built on the `notify` crate; runs in core, not in the Tauri layer. design.md names notify as the choice; this doc nails down event handling, debounce, ignored paths, and the dispatch path to frontend + indexer.


## Scope

One watcher per open vault, rooted at the vault path. Started when a vault is opened, stopped when the vault is closed or swapped. Recursive.

External-file ingestion (design.md:88) gets its own watcher instances per configured path/glob, but shares the same event-handling code. v1 watches only the vault root.


## Event sources and consumers

The watcher fans out events to two consumers:

1. **Indexer** — for any file matching `*.md` (and v2+: extractor-handled types). Triggers re-chunk/re-embed via the ingest pipeline in index.md.
2. **Editor frontend** — only for the *currently open file's* path. Drives the buffer drift behavior in editor.md:48 (silent reload if clean, conflict prompt if dirty).

Both consumers receive the same normalized event stream; each filters to the events it cares about.


## Event normalization

`notify` emits raw events that are platform-specific and noisy (multiple events per logical change, partial writes, editor swap-file dances). Normalize before fanning out.

Normalized event types:

```rust
enum FileEvent {
    Created { path: PathBuf },
    Modified { path: PathBuf },
    Deleted { path: PathBuf },
    Renamed { from: PathBuf, to: PathBuf },
}
```

Normalization steps:

- **Debounce** — coalesce raw events within a 200ms window keyed by path. The last event for a path during the window wins. Catches editor swap-file patterns (`vim`'s write-temp-then-rename, VSCode's atomic save) where a logical save produces 3–5 raw events.
- **Rename pairing** — `notify` emits paired `RenameFrom`/`RenameTo` events on Linux/macOS. Pair them within the debounce window into a single `Renamed`. Unpaired `RenameFrom` after timeout → `Deleted`; unpaired `RenameTo` → `Created`.
- **Drop self-writes** — when the indexer or the editor's save path writes a file, mark the path in a short-lived "we just wrote this" set (TTL 500ms). Drop normalized events for those paths to avoid an infinite re-index loop. Implementation: indexer/save call `watcher.suppress(path)` immediately before the write.
- **Drop ignored paths** — see below.

Dispatch order: debounce window closes → normalize → check ignore list → check suppression set → fan out to consumers.


## Ignored paths

Hard-coded ignores (never reach consumers):

- Anything under `.hiker/` (our own state — index.db, refs/, proposals/, reconcile-history.yaml)
- Anything under `.git/` if present
- Dotfiles at vault root by default (`.DS_Store`, `.obsidian/` if a vault is migrating from Obsidian, etc.)
- Files matching `*.tmp`, `*.swp`, `*~`, `4913` (vim's permission-probe file), `.#*` (emacs lock files)

Configurable ignores (later):

- `vault/.hiker/ignore` — a gitignore-style file. v1 ships without it; the hard-coded list is enough for personal use.

Indexer's own writes to `.hiker/index.db` would loop forever without the `.hiker/` ignore, so this is load-bearing, not a nicety.


## Dispatch to consumers

Core exposes a broadcast channel (`tokio::sync::broadcast::channel<FileEvent>`) that anyone can subscribe to. Two subscribers in v1:

1. **Indexer task** — filters to `*.md` events, sends matching `IndexJob`s into its work queue. See index.md.
2. **Frontend bridge** — a Tauri command-layer task that subscribes and forwards events to the webview via `emit("hiker:file-changed", payload)`. The frontend filters to the open-buffer path; everything else is dropped client-side. (Could filter server-side instead, but pushing all events keeps the bridge stateless and a future tree-view-refresh consumer can use the same stream without a second subscription.)

Event payload to frontend:

```ts
type FileChangedEvent = {
  kind: "created" | "modified" | "deleted" | "renamed";
  path: string;          // vault-relative
  from?: string;         // for renamed only
};
```


## Editor integration

When the frontend receives `hiker:file-changed` for the active buffer's path:

- `kind: "modified"` and buffer is **clean** → fetch fresh contents + hash via `read_file_with_hash`, dispatch a doc-replace transaction, update `loadedHash`. Silent reload.
- `kind: "modified"` and buffer is **dirty** → fire the same conflict modal used by the pre-write drift check (Keep mine / Take theirs / Cancel). "Take theirs" reloads from disk; "Keep mine" leaves the buffer as-is — the next save will re-trigger the drift check, since the on-disk hash will differ from `loadedHash`.
- `kind: "deleted"` and buffer is clean → close the buffer, clear the editor, surface a non-blocking toast ("file removed externally").
- `kind: "deleted"` and buffer is dirty → keep the buffer open, surface a toast ("file removed; save to recreate"). User's edits live only in memory until they save.
- `kind: "renamed"` from the active buffer's path → update `currentPath` to the new path silently. The buffer follows.

The pre-write drift check stays in place as a final guard. The watcher reduces but doesn't eliminate the stale-buffer window — events can be missed on network filesystems, dropped when notify's queue overflows, or simply arrive after the user has already hit save.


## Indexer integration

Indexer subscribes to the same broadcast and queues `IndexJob`s:

- `Created`, `Modified` for `*.md` → upsert job
- `Deleted` for `*.md` → delete job (remove note + chunks + vecs)
- `Renamed` for `*.md` → rename job (update `notes.path` and `path_ids`, no re-embed if content hash unchanged)

Non-markdown events are ignored by the v1 indexer; v2+ extractor types subscribe to additional patterns through the same dispatch.


## Lifecycle

- **Vault open** — spawn the watcher, start the indexer task, kick a startup scan in parallel.
- **Vault close / swap** — drop the watcher (tokio task aborts when its handle drops), close the indexer's connection, flush any in-flight broadcast events.
- **App shutdown** — same as close; no special teardown needed.

The watcher handle lives in the `HikerCore` state struct (`tauri::State<Arc<HikerCore>>` per design.md:362). Vault-swap drops the old core and constructs a new one — clean separation, no risk of cross-vault event leakage.


## Failure modes

- **notify queue overflow** — long bursts (mass file copy, git checkout) can exceed notify's internal buffer. Detect via the `Error::PathNotFound` / `Error::Generic` paths and surface a "watcher fell behind, scanning" event; the indexer triggers a startup-style rescan to catch up. Frontend just shows a brief progress indicator.
- **Network filesystem** — notify's events are unreliable on NFS/SMB. v1 keeps the pre-write drift check as the trustworthy fallback. Long-term: a polling fallback mode for paths flagged as networked.
- **Permissions** — file becomes unreadable mid-watch (parent dir chmod 000). Watcher emits an error; indexer marks the note as `orphaned` (per the design.md missing-source convention) and stops trying until the next event.
- **Symlink loops** — `notify` follows symlinks by default. v1 disables symlink-following on the watcher to avoid surprise. External-file ingestion in v2 will revisit this with an explicit allowlist.


## Out of scope for v1

- External-path watchers (configured globs outside vault root)
- Polling fallback for network filesystems
- User-configurable ignore file
- Cross-vault event routing (multi-vault support is a v3+ concern)
- Tree-view auto-refresh on watcher events (the editor consumer is the only frontend subscriber in v1; the file tree gets a manual refresh button until v2)
