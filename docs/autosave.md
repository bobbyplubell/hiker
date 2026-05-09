# Autosave

Crash-recovery snapshots of dirty editor buffers, plus a tab-state restore on vault re-open. Written periodically from the frontend, owned by the backend, surfaced as a recovery modal on next launch when an autosaved buffer has unsaved deltas. Modeled on Notepad++: one snapshot per dirty buffer, overwritten in place each tick, separate from the actual file the user is editing.

Distinct from saving. Saving writes the *user's file* — autosave writes a *sidecar shadow copy* the user never sees unless we crash. Distinct from `changes.md`'s changelog — that records committed writes for agent rollback / future sync; autosave records *uncommitted* in-flight content for force-kill recovery. Different lifecycle, different consumers, different invariants; the two stores never share rows.

The headline decisions:

- **One sidecar file per dirty buffer, overwritten in place each tick.** No append-only history, no per-tick versioning. NPP shape: re-saves to the same file, the freshest tick is the only thing on disk per buffer. [autosave-one-per-buffer]
- **Backend owns storage, GC, and recovery; frontend ticks and pushes.** All filesystem touches are in `core::autosave`. The frontend's role collapses to "fire a 5s timer, push every dirty buffer's current text, prompt on recover hits." Live buffer text only exists in CM6, so the push direction is unavoidable; everything else stays in core. [autosave-backend-module]
- **Storage lives at `vault/.hiker/autosave/`.** Per-vault. One `<id>.md` per dirty buffer plus an `index.json` carrying the path↔id map, per-entry content hash, and an authoritative tab-state snapshot. [autosave-store-layout]
- **Recovery surfaces only buffers that genuinely have unsaved deltas.** On vault open, `autosave_recover()` returns entries whose autosaved `content_hash` differs from the live on-disk hash for the same path. Matches drop silently — they're stale snapshots from the last clean session. [autosave-recover-cmd]
- **Tab state restores silently; buffer recovery prompts.** Reopening tabs is a quality-of-life feature the user expects to "just work." Restoring buffer content is a destructive-feeling decision (your edits vs. what's on disk now) and gets a per-row modal. [autosave-tab-state-silent-restore, autosave-recovery-modal]
- **Not in `changes.db`.** The two stores have different lifecycles (autosave: ephemeral, GC'd on save; changes: durable, retention-bounded), different consumers, and conflating them would inflate changelog row counts by orders of magnitude.


## Storage layout

`vault/.hiker/autosave/` per vault:

```
.hiker/autosave/
  index.json
  01HRX3...--inbox-idea.md         # autosaved copy of inbox/idea.md
  01HRX4...--research-paper.md     # autosaved copy of research/paper.md
  ...
```

`<id>` is a ulid; the trailing slug is debuggable but not load-bearing (the `index.json` map is canonical). One file per dirty buffer; overwritten on each tick. The on-disk content is exactly what the buffer would write if Save were pressed *right now* — no diff-encoding, no compression. Markdown at personal-vault scale doesn't justify the ceremony, and recovery wants the cheapest possible read path.

`index.json`:

```json
{
  "version": 1,
  "entries": {
    "inbox/idea.md": {
      "autosave_id": "01HRX3...",
      "content_hash": "<blake3-of-autosaved-content>",
      "saved_at_ms": 1730000000000
    },
    "research/paper.md": { ... }
  },
  "tab_state": {
    "open_paths": ["inbox/idea.md", "research/paper.md", "personal/notes.md"],
    "active_path": "research/paper.md",
    "preview_path": "personal/notes.md",
    "saved_at_ms": 1730000000000
  }
}
```

Notes:

- **Path-keyed entries.** The vault-relative path is the primary key. One entry per buffer; the `autosave_id` is just the file naming.
- **`content_hash` is blake3 of the autosaved bytes.** Used by `autosave_recover()` to compare against the live on-disk hash and surface only the genuine deltas.
- **`tab_state` is a single snapshot.** Most-recently-pushed value wins; `autosave_save_tab_state` overwrites it in place. Same write path as the per-buffer entries — one `index.json` rewrite covers both.
- **Atomic writes only.** `index.json` updates use the `vim`-style write-temp-then-rename pattern so a crash mid-write leaves either the prior or new index, never a half-written one. Per-buffer `<id>.md` writes use the same pattern.

The autosave directory is in the `watcher-ignore-hardcoded` list (everything under `.hiker/` is). No `watcher-suppress-self-writes` dance needed — autosave writes never reach the watcher's normalization stage. [autosave-no-watcher-suppression]


## Backend module

`core::autosave::Autosave` exposes:

```rust
impl Autosave {
    pub fn open(vault_root: &Path) -> Result<Self, AutosaveError>;

    pub fn write(&self, path: &str, contents: &[u8], buffer_hash: &str)
        -> Result<(), AutosaveError>;

    pub fn clear(&self, path: &str) -> Result<(), AutosaveError>;

    pub fn save_tab_state(&self, state: TabState) -> Result<(), AutosaveError>;
    pub fn load_tab_state(&self) -> Result<Option<TabState>, AutosaveError>;

    pub fn recover(&self) -> Result<Vec<RecoveredEntry>, AutosaveError>;
    pub fn discard(&self, path: &str) -> Result<(), AutosaveError>;

    pub fn vault_swap_reset(&self) -> Result<(), AutosaveError>;
}
```

`RecoveredEntry`:

```rust
pub struct RecoveredEntry {
    pub path: String,
    pub autosave_id: String,
    pub autosaved_content: Vec<u8>,
    pub autosaved_hash: String,
    pub on_disk_hash: Option<String>,    // None when the file no longer exists on disk
    pub saved_at_ms: i64,
}
```

`recover()` walks the index, computes the live on-disk hash for each entry's path (or `None` when missing), and returns only entries whose `autosaved_hash != on_disk_hash` (or whose on-disk file is gone). Matches are dropped silently as part of the same call so the index file shrinks naturally. [autosave-recover-cmd]

Module discipline mirrors `core::store` and `core::changes` — `core::autosave` is the only module that touches `.hiker/autosave/`, returns plain Rust types (`RecoveredEntry`, `TabState`) not internal storage types, and exposes a narrow API the Tauri layer wraps in 5–15 lines per command. [autosave-backend-module]

The Tauri command surface matches the Rust API one-to-one: `autosave_write` / `autosave_clear` / `autosave_save_tab_state` / `autosave_load_tab_state` / `autosave_recover` / `autosave_discard`. Each command parses args → calls `Autosave::*` → translates errors → returns DTO.


## Frontend tick

Every ~5 seconds while any tab is dirty, the frontend pushes each dirty buffer's current `(path, contents, hash)` to `autosave_write`. Buffers that became clean since the last tick fire `autosave_clear(path)`. [autosave-write-tick]

- **Tick interval: 5s.** NPP defaults around 7s; we go slightly tighter since vaults are mostly markdown (cheap to write) and the cost of losing 5s of typing is annoying enough to justify the extra disk traffic.
- **Tick is suspended when no buffers are dirty.** No-op timers are wasteful; reactivate on first dirty transition.
- **Flush on window blur.** The OS gives us a focus-loss event before most graceful exits; the frontend fires an extra immediate tick on blur to shorten the worst-case loss window. [autosave-write-tick]
- **Read-only preview buffers (trash / snapshot) never autosave.** They're never dirty by construction — the autosave path filters them out at the source. [autosave-readonly-skipped]
- **In-flight-mutation buffers do autosave.** A buffer that's RO mid-mutation may still carry pre-mutation dirty content the user typed; that's exactly the case crash recovery exists for.
- **Concurrent ticks for the same path are serialized in the backend.** The frontend doesn't coordinate; rapid duplicate writes for the same path are fine because each one overwrites the same target file. [autosave-one-per-buffer]
- **Tab state pushes are event-driven, not on the timer.** Open tab / close tab / activate tab / preview-slot change all fire a debounced `autosave_save_tab_state` (~250ms). Cheaper than the full content push and orthogonal to dirty state. [autosave-tab-state-store]


## Save / close lifecycle

- **Successful save** → `autosave_clear(path)`. The on-disk file now matches what the buffer holds; the autosave sidecar is redundant and would otherwise resurface as a false-positive recovery on next open.
- **Tab close (any path)** → `autosave_clear(path)`. Whether the user picked Save, Discard, or the tab was clean to begin with, the autosave entry for that buffer is no longer relevant.
- **Window close, all dirty buffers handled** → after the existing `multi-buffer-window-close-guard` resolves, the frontend clears each handled path and pushes a final tab-state snapshot (which may be empty). The next launch's recovery returns nothing.
- **Force-kill / crash** → no cleanup runs. The next vault open finds the autosave directory populated and `autosave_recover()` does its filtering. This is the load-bearing path the whole feature exists for.
- **Watcher-driven external rename** → the buffer's path field follows the new name (per `watcher-editor-renamed-followup`); the frontend fires `autosave_clear(oldPath)` and the next tick writes against the new path naturally. [autosave-rename-clear-old]
- **Watcher-driven external delete while dirty** → buffer stays open per `watcher-editor-deleted-buffer`; the autosave entry persists. Recovery on next open compares against a now-missing on-disk file → `on_disk_hash = None` → entry surfaces as a recoverable buffer.


## Recovery modal

When `autosave_recover()` returns a non-empty list on vault open, the frontend renders a modal listing each entry. Same widget family as `multi-buffer-window-close-guard`'s save/discard list. [autosave-recovery-modal]

Per-row affordances:

- **Restore** — load the autosaved content into a buffer for that path, mark dirty, open as a sticky tab. The user can save (writes through to the actual file) or discard from the normal buffer-management surface afterward. Calls `autosave_clear(path)` once the buffer is open, since the autosaved copy is now live in memory.
- **Discard** — `autosave_discard(path)`. The autosaved sidecar is removed, no buffer is opened, the on-disk file (if it exists) is left alone.

Bulk affordances at the modal footer:

- **Restore all** — sequence per-row Restore for every entry.
- **Discard all** — sequence per-row Discard for every entry.

Cancel returns the user to the app with the modal dismissed but the autosave entries still on disk; the next vault open will surface them again. We deliberately don't auto-discard on Cancel — the user may have hit Cancel by accident, and the cost of a re-prompt is much smaller than the cost of silently dropping unsaved work.

Modal entry rows show: path (clickable preview-style row mirroring the recents-list shape), saved_at relative time (`2m ago`, `yesterday`), and a small `(deleted)` tag when `on_disk_hash` is `None` so the user knows Restore creates the file fresh. No diff preview in the modal itself — restoring opens the buffer, where the existing `editor-diff-vs-disk-toggle` answers "what changed?" if the user wants the comparison.


## Tab state restore

On vault open, after the recovery modal resolves (or if it had nothing to surface), the frontend silently calls `autosave_load_tab_state()` and reopens each path in `open_paths` as a sticky tab in order, then activates `active_path`. If `preview_path` is non-null and that path was *not* in `open_paths`, it opens as the preview slot. [autosave-tab-state-silent-restore]

Silent because:

- The set of open tabs is the user's working context. Restoring it without ceremony matches every other editor users have used (VSCode, Sublime, IntelliJ).
- The dirty-recovery prompt has already covered the destructive case (uncommitted edits). Tab restore is just "reopen what was open" — no mutation of the user's files.
- Failures (a path no longer exists on disk, or a tab whose buffer was a trash preview) are dropped silently from the restore list. The remaining tabs reopen normally; missing paths log to the obs stream per `obs-error-context`.

Tab state restore lifts the prior posture of `multi-buffer-in-memory-only` — open buffers now do persist across vault re-opens. The slug stays in `editor.md`'s multi-buffer section, restated to describe the new shape (in-memory plus a tab-state snapshot the autosave layer round-trips).


## Vault swap

Closing a vault flushes the in-memory autosave state via the same path that `multi-buffer-window-close-guard` runs through (clear each handled path, push final tab state) and then the new vault's `core::autosave` opens fresh against its own `.hiker/autosave/` directory. No cross-vault leakage; each vault's autosave state is local to that vault, exactly like `changes.db` and `index.db`. [autosave-vault-swap-clears]


## Backup classification

Per `design.md`'s three-class backup framing:

- The autosave directory's `<id>.md` files are **regenerable from running memory** — if the app is up and a tab is dirty, the next tick re-creates them. Lost only if the app exits cleanly with all buffers saved (in which case there's nothing to recover) or the user discards via the modal. Treat as **regenerable** for backup purposes; not worth syncing.
- The `index.json` is **durable** — it carries the tab-state snapshot, which isn't reconstructible after a clean shutdown. Worth keeping if a backup tool is already including the rest of `.hiker/`. The cost is trivial (one small JSON file per vault).

In practice, the simplest "back up the whole `.hiker/` directory" rule already covers both correctly. [autosave-backup-class]


## Settings

Autosave is on by default with a fixed 5s tick. No `[autosave]` config section in v1 — the tick interval is hard-coded, on/off is implicit, and there's no nob a normal user needs. If a real workflow asks for them, an `[autosave]` section can land later (`tick_secs`, `enabled`); the strict-load posture and write-back machinery in `settings.md` already cover the shape. The deferred row stays in `settings.md`'s `## Deferred`.


## Out of scope

- **Per-buffer history.** NPP shape is one snapshot per buffer overwritten in place. Multiple snapshots per buffer would re-invent `changes.db` for in-flight content; if a future workflow wants per-keystroke timeline replay, it'd build on `changes.db` (which already has per-row content) rather than autosave.
- **Compression.** Autosave files are markdown at personal-vault scale, written briefly, read once. zstd would save bytes but adds an encode step in the hot tick path for no measured win. `changes.md`'s `changes-content-zstd` lives where compression actually pays off (long retention, many rows).
- **Cross-device autosave sync.** Autosave is per-machine crash recovery, not a device-handoff feature. The future sync layer in `design.md` syncs *committed* writes (saved files); in-flight uncommitted content stays local. This deliberately matches every other editor's posture.
- **Per-keystroke continuous flush.** 5s tick + on-blur flush is the right ergonomic floor; flushing every keystroke would bury the disk for negligible recovery improvement.
- **Recovery diff preview inside the modal.** Restoring opens the buffer, where `editor-diff-vs-disk-toggle` already answers "what changed?" Inlining a second diff renderer in a modal duplicates that affordance.
- **Untitled / scratch buffers.** Hiker has no untitled-buffer concept today (`create-note-button` mints a path immediately). When and if scratch buffers land, autosave will need a path-less storage shape; not designed here.


## Forward refs

- `editor.md` "Multi-buffer model" — `multi-buffer-in-memory-only` is restated here in terms of "in-memory plus tab-state snapshot." The autosave layer is the persistence mechanism behind that restatement.
- `editor.md` "Save UX" — the deferred "Future: autosave on idle / on blur" entry is now this spec, with the explicit shift from "write through to the user's file on idle" to "write a sidecar shadow copy for crash recovery." The user-file save path is unchanged.
- `watcher.md` `watcher-ignore-hardcoded` — autosave writes don't reach the watcher because `.hiker/` is hard-coded ignored.
- `changes.md` — separate store, separate lifecycle. Autosave never writes a `changes.db` row; saving a recovered buffer routes through the existing save path, which writes one as usual.
- `design.md` "Sync / backup" — autosave directory falls into the regenerable bucket; the `index.json` is durable but trivial.
