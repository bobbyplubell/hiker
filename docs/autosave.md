# Autosave

Crash-recovery snapshots of dirty editor buffers, plus a tab-state restore on vault re-open. Written periodically from the frontend, owned by the backend, auto-restored as dirty sticky tabs on next launch when an autosaved buffer has unsaved deltas (no modal — the dirty-marker is the affordance). One snapshot per dirty buffer, overwritten in place each tick, separate from the actual file the user is editing.

Autosave is distinct from saving and from the op log. Saving writes the *user's file*; autosave writes a *sidecar shadow copy* the user never sees unless we crash. The op log (`op-log.md`) records *committed* writes (accepted ops) for agent rollback / future sync, is durable and retention-bounded; autosave records *uncommitted* in-flight per-keystroke content for force-kill recovery, is ephemeral and GC'd on save. Different lifecycle, different consumers, different invariants; the two stores never share state, and conflating them would inflate the op log by orders of magnitude. [autosave-one-per-buffer, autosave-backend-module, autosave-store-layout, autosave-recover-cmd, autosave-tab-state-silent-restore, autosave-recovery-auto-restore]


## Storage layout

`vault/.hiker/autosave/` per vault:

```
.hiker/autosave/
  index.json
  01HRX3...--inbox-idea.md         # autosaved copy of inbox/idea.md
  01HRX4...--research-paper.md     # autosaved copy of research/paper.md
  ...
```

`<id>` is a ulid; the trailing slug is debuggable but not load-bearing (the `index.json` map is canonical). One file per dirty buffer, overwritten each tick. The on-disk content is exactly what the buffer would write if Save were pressed *right now* — no diff-encoding, no compression.

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

`core::autosave::Autosave` exposes (all `-> Result<_, AutosaveError>`): `open(vault_root)`, `write(path, contents, buffer_hash)`, `clear(path)`, `save_tab_state(state)` / `load_tab_state() -> Option<TabState>`, `recover() -> Vec<RecoveredEntry>`, `discard(path)`, and `vault_swap_reset()`.

`RecoveredEntry`: `path`, `autosave_id`, `autosaved_content: Vec<u8>`, `autosaved_hash`, `on_disk_hash: Option<String>` (None when the file no longer exists on disk), `saved_at_ms`.

`recover()` walks the index, computes each entry's live on-disk hash (or `None` when missing), and returns only entries whose `autosaved_hash != on_disk_hash` (or whose file is gone). Matches drop silently in the same call so the index file shrinks. [autosave-recover-cmd]

Module discipline mirrors `core::store` and `core::activity` — `core::autosave` is the only module that touches `.hiker/autosave/`, returns plain Rust types (`RecoveredEntry`, `TabState`) not internal storage types, and exposes a narrow API the host wraps in 5–15 lines per command. [autosave-backend-module]

The host command surface matches the Rust API one-to-one: `autosave_write` / `autosave_clear` / `autosave_save_tab_state` / `autosave_load_tab_state` / `autosave_recover` / `autosave_discard`. Each command parses args → calls `Autosave::*` → translates errors → returns DTO.


## Frontend tick

Every ~5 seconds while any tab is dirty, the frontend pushes each dirty buffer's current `(path, contents, hash)` to `autosave_write`. Buffers that became clean since the last tick fire `autosave_clear(path)`. [autosave-write-tick]

- **Tick interval: 5s.** Vaults are mostly markdown (cheap to write) and losing 5s of typing is annoying enough to justify the disk traffic.
- **Tick is suspended when no buffers are dirty.** No-op timers are wasteful; reactivate on first dirty transition.
- **Flush on window blur.** The OS gives us a focus-loss event before most graceful exits; the frontend fires an extra immediate tick on blur to shorten the worst-case loss window. [autosave-write-tick]
- **Read-only preview buffers (trash / snapshot) never autosave.** They're never dirty by construction — the autosave path filters them out at the source. [autosave-readonly-skipped]
- **In-flight-mutation buffers do autosave.** A buffer that's RO mid-mutation may still carry pre-mutation dirty content the user typed; that's exactly the case crash recovery exists for.
- **Concurrent ticks for the same path are serialized in the backend.** The frontend doesn't coordinate; rapid duplicate writes for the same path are fine because each one overwrites the same target file. [autosave-one-per-buffer]
- **Tab state pushes are event-driven, not on the timer.** Open tab / close tab / activate tab / preview-slot change all fire a debounced `autosave_save_tab_state` (~250ms). Cheaper than the full content push and orthogonal to dirty state. [autosave-tab-state-store]


## Save / close lifecycle

- **Successful save** → `autosave_clear(path)`. The on-disk file now matches what the buffer holds; the autosave sidecar is redundant and would otherwise resurface as a false-positive recovery on next open.
- **Tab close (any path)** → `autosave_clear(path)`. Whether the user picked Save, Discard, or the tab was clean to begin with, the autosave entry for that buffer is no longer relevant.
- **Window close** → no dirty-buffer modal. The frontend awaits one final autosave flush (`flushAllAndWait`) and pushes the current tab-state snapshot (`pushTabStateNow`), then destroys the window. Sidecars are *not* cleared — next launch's `autosave-recovery-auto-restore` reopens every recoverable buffer as a dirty tab so the user can save or revert via the existing affordances. Note: autosave does *not* write through to the user's files on exit — exit means "park the work," not "commit to disk." [autosave-close-no-modal]
- **Force-kill / crash** → no cleanup runs. The next vault open finds the autosave directory populated and `autosave_recover()` does its filtering. This is the load-bearing path the whole feature exists for.
- **Watcher-driven external rename** → the buffer's path field follows the new name (per `watcher-editor-renamed-followup`); the frontend fires `autosave_clear(oldPath)` and the next tick writes against the new path naturally. [autosave-rename-clear-old]
- **Watcher-driven external delete while dirty** → buffer stays open per `watcher-editor-deleted-buffer`; the autosave entry persists. Recovery on next open compares against a now-missing on-disk file → `on_disk_hash = None` → entry surfaces as a recoverable buffer.


## Recovery (auto-restore)

When `autosave_recover()` returns a non-empty list on vault open, the frontend opens each entry as a sticky tab — no modal, no per-row prompt. [autosave-recovery-auto-restore]

Per-entry shape:

- **File still on disk.** Open the path normally (`open_for_edit` reads disk content into `loadedText`), then dispatch the autosaved bytes into the editor. The buffer reads dirty (autosaved bytes ≠ on-disk loadedText), so the user sees the unsaved work, the dirty marker on the tab, and can save (writes the autosaved bytes through to the file) or revert via the normal close-dirty / dirty-buffer-diff affordances.
- **File deleted on disk.** Write the autosaved bytes back to disk first (Restore creates the file fresh), then open normally. The buffer comes up clean — the user's work is preserved on disk; if they don't want it, normal delete works.

After the open, the autosave sidecar is dropped (`autosave_discard(path)`) since the autosaved copy is now live in memory.

The dirty marker on each restored tab is the affordance — it composes with the existing save / close-dirty / `editor-diff-vs-disk-toggle` surfaces, so no recovery modal is needed.

Worth pinning: the multi-tab restore order matches `tab_state.open_paths` (the silent tab-state restore runs alongside this auto-restore on vault open); the active tab on next open is `tab_state.active_path` if still resolvable, otherwise the most recent recovered tab.


## Tab state restore

On vault open, after the auto-restore loop finishes opening recovered buffers (or immediately if there were none), the frontend silently calls `autosave_load_tab_state()` and reopens each path in `open_paths` as a sticky tab in order, then activates `active_path`. If `preview_path` is non-null and that path was *not* in `open_paths`, it opens as the preview slot. [autosave-tab-state-silent-restore]

`open_paths`, `active_path`, and `preview_path` all use a tab's `persist_key` — the prefixed form for per-doc tabs (`canvas:<path>`, `board:<path>`) and a synthetic key for singleton page tabs (`:home`, `:graph`, …) — so the active tab restores for every persistable tab kind, not only plain vault-note tabs. A non-persistable active tab (trash/snapshot preview) leaves `active_path` empty and restore falls back to the first reopened tab.

Failures — a path no longer on disk, or a tab whose buffer was a trash preview — drop silently from the restore list; the remaining tabs reopen normally and missing paths log to the obs stream per `obs-error-context`.

Open buffers persist across vault re-opens via this tab-state snapshot (`multi-buffer-in-memory-only` in `editor.md`).


## Vault swap

Closing a vault flushes the in-memory autosave state — clearing each handled path and pushing the final tab state — and then the new vault's `core::autosave` opens fresh against its own `.hiker/autosave/` directory. No cross-vault leakage; each vault's autosave state is local to that vault, exactly like the op log and `index.db`. [autosave-vault-swap-clears]


## Backup classification

Per `design.md`'s three-class backup framing: the `<id>.md` sidecars are **regenerable** (the next tick re-creates any dirty buffer), not worth syncing; the `index.json` is **durable** (carries the tab-state snapshot, not reconstructible after a clean shutdown) but trivially small. The simplest "back up the whole `.hiker/` directory" rule covers both correctly. [autosave-backup-class]


## Settings

Autosave is on by default with a fixed 5s tick. No `[autosave]` config section in v1 — interval hard-coded, on/off implicit. An `[autosave]` section (`tick_secs`, `enabled`) can land later; `settings.md`'s strict-load + write-back machinery already covers the shape, and the deferred row lives in its `## Deferred`.


## Out of scope

- **Per-buffer history.** One snapshot per buffer, overwritten in place. Multiple snapshots would re-invent the op log for in-flight content; per-keystroke timeline replay, if ever wanted, builds on the op log, not autosave.
- **Compression.** Autosave files are markdown at personal-vault scale, written briefly, read once. zstd would save bytes but adds an encode step in the hot tick path for no measured win.
- **Cross-device autosave sync.** Autosave is per-machine crash recovery, not a device-handoff feature. The future sync layer in `design.md` syncs *committed* writes (saved files); in-flight uncommitted content stays local. This deliberately matches every other editor's posture.
- **Per-keystroke continuous flush.** 5s tick + on-blur flush is the right ergonomic floor; flushing every keystroke would bury the disk for negligible recovery improvement.
- **Recovery prompt / modal at vault open.** Auto-restore as dirty tabs is the design; the dirty-marker, the close-dirty modal, and `editor-diff-vs-disk-toggle` together cover everything a per-row Restore/Discard prompt would.
- **Untitled / scratch buffers.** Hiker has no untitled-buffer concept today (`sidebar-new-item-button` mints a path immediately). When and if scratch buffers land, autosave will need a path-less storage shape; not designed here.


## Forward refs

- `editor.md` "Multi-buffer model" — `multi-buffer-in-memory-only`; the autosave layer is the persistence mechanism behind it (in-memory plus a tab-state snapshot).
- `editor.md` "Save UX" — autosave-on-idle/blur writes a sidecar shadow copy for crash recovery; the user-file save path is unchanged.
- `watcher.md` `watcher-ignore-hardcoded` — autosave writes don't reach the watcher because `.hiker/` is hard-coded ignored.
- `op-log.md` — separate store, separate lifecycle. Autosave never appends to the op log; saving a recovered buffer routes through the existing save path, which appends accepted ops as usual.
- `design.md` "Sync / backup" — autosave directory falls into the regenerable bucket; the `index.json` is durable but trivial.
