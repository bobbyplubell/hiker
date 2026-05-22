# Autosave

Crash-recovery snapshots of dirty editor buffers, plus a tab-state restore on vault re-open. Written periodically from the frontend, owned by the backend, auto-restored as dirty sticky tabs on next launch when an autosaved buffer has unsaved deltas (no modal — the dirty-marker is the affordance). Modeled on Notepad++: one snapshot per dirty buffer, overwritten in place each tick, separate from the actual file the user is editing.

Distinct from saving. Saving writes the *user's file* — autosave writes a *sidecar shadow copy* the user never sees unless we crash. Distinct from `changes.md`'s changelog — that records committed writes for agent rollback / future sync; autosave records *uncommitted* in-flight content for force-kill recovery. Different lifecycle, different consumers, different invariants; the two stores never share rows.

The headline decisions:

- **One sidecar file per dirty buffer, overwritten in place each tick.** No append-only history, no per-tick versioning. NPP shape: re-saves to the same file, the freshest tick is the only thing on disk per buffer. [autosave-one-per-buffer]
- **Backend owns storage, GC, and recovery; frontend ticks and pushes.** All filesystem touches are in `core::autosave`. The frontend's role collapses to "fire a 5s timer, push every dirty buffer's current text, prompt on recover hits." Live buffer text only exists in CM6, so the push direction is unavoidable; everything else stays in core. [autosave-backend-module]
- **Storage lives at `vault/.hiker/autosave/`.** Per-vault. One `<id>.md` per dirty buffer plus an `index.json` carrying the path↔id map, per-entry content hash, and an authoritative tab-state snapshot. [autosave-store-layout]
- **Recovery surfaces only buffers that genuinely have unsaved deltas.** On vault open, `autosave_recover()` returns entries whose autosaved `content_hash` differs from the live on-disk hash for the same path. Matches drop silently — they're stale snapshots from the last clean session. [autosave-recover-cmd]
- **Both tab state and buffer recovery restore silently.** Reopening tabs is the quality-of-life baseline; recovered buffers ride the same shape — each one auto-opens as a sticky tab carrying the autosaved content, dirty against disk, so the user sees the unsaved work and decides whether to save or revert via the normal save / discard surfaces. No prompt at vault open. [autosave-tab-state-silent-restore, autosave-recovery-auto-restore]
- **Not in the op log.** The two stores have different lifecycles (autosave: ephemeral, GC'd on save; op log: durable, retention-bounded), different consumers, and conflating them would inflate the op log by orders of magnitude. Autosave is in-flight per-keystroke buffer state; the op log is committed edit history.


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

Module discipline mirrors `core::store` and `core::changes` — `core::autosave` is the only module that touches `.hiker/autosave/`, returns plain Rust types (`RecoveredEntry`, `TabState`) not internal storage types, and exposes a narrow API the host wraps in 5–15 lines per command. [autosave-backend-module]

The host command surface matches the Rust API one-to-one: `autosave_write` / `autosave_clear` / `autosave_save_tab_state` / `autosave_load_tab_state` / `autosave_recover` / `autosave_discard`. Each command parses args → calls `Autosave::*` → translates errors → returns DTO.


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

No modal because:

- The dirty-marker on each restored tab is already a clear signal that recovered work is sitting in front of the user; it composes with every other dirty-buffer affordance the editor already has (save, the close-dirty modal, `editor-diff-vs-disk-toggle`).
- A modal at vault open is friction the user pays *every* recoverable session, even when the right answer is obviously "yes, give me my work back." The dirty-marker shape makes the same decision implicit and reversible.
- Force-kill recoveries are by definition unsaved work — defaulting to "preserve" matches the user's intent in every case the recovery exists for. If they really want to discard, the close-dirty modal already covers it.

Worth pinning: the multi-tab restore order matches `tab_state.open_paths` (the silent tab-state restore runs alongside this auto-restore on vault open); the active tab on next open is `tab_state.active_path` if still resolvable, otherwise the most recent recovered tab.


## Tab state restore

On vault open, after the auto-restore loop finishes opening recovered buffers (or immediately if there were none), the frontend silently calls `autosave_load_tab_state()` and reopens each path in `open_paths` as a sticky tab in order, then activates `active_path`. If `preview_path` is non-null and that path was *not* in `open_paths`, it opens as the preview slot. [autosave-tab-state-silent-restore]

Silent because:

- The set of open tabs is the user's working context. Restoring it without ceremony matches every other editor users have used (VSCode, Sublime, IntelliJ).
- The dirty-recovery prompt has already covered the destructive case (uncommitted edits). Tab restore is just "reopen what was open" — no mutation of the user's files.
- Failures (a path no longer exists on disk, or a tab whose buffer was a trash preview) are dropped silently from the restore list. The remaining tabs reopen normally; missing paths log to the obs stream per `obs-error-context`.

Tab state restore lifts the prior posture of `multi-buffer-in-memory-only` — open buffers now do persist across vault re-opens. The slug stays in `editor.md`'s multi-buffer section, restated to describe the new shape (in-memory plus a tab-state snapshot the autosave layer round-trips).


## Vault swap

Closing a vault flushes the in-memory autosave state — clearing each handled path and pushing the final tab state — and then the new vault's `core::autosave` opens fresh against its own `.hiker/autosave/` directory. No cross-vault leakage; each vault's autosave state is local to that vault, exactly like the op log and `index.db`. [autosave-vault-swap-clears]


## Backup classification

Per `design.md`'s three-class backup framing:

- The autosave directory's `<id>.md` files are **regenerable from running memory** — if the app is up and a tab is dirty, the next tick re-creates them. Lost only if the app exits cleanly with all buffers saved (in which case there's nothing to recover) or after the auto-restored buffer is in memory and the sidecar is dropped. Treat as **regenerable** for backup purposes; not worth syncing.
- The `index.json` is **durable** — it carries the tab-state snapshot, which isn't reconstructible after a clean shutdown. Worth keeping if a backup tool is already including the rest of `.hiker/`. The cost is trivial (one small JSON file per vault).

In practice, the simplest "back up the whole `.hiker/` directory" rule already covers both correctly. [autosave-backup-class]


## Settings

Autosave is on by default with a fixed 5s tick. No `[autosave]` config section in v1 — the tick interval is hard-coded, on/off is implicit, and there's no nob a normal user needs. If a real workflow asks for them, an `[autosave]` section can land later (`tick_secs`, `enabled`); the strict-load posture and write-back machinery in `settings.md` already cover the shape. The deferred row stays in `settings.md`'s `## Deferred`.


## Out of scope

- **Per-buffer history.** NPP shape is one snapshot per buffer overwritten in place. Multiple snapshots per buffer would re-invent the op log for in-flight content; if a future workflow wants per-keystroke timeline replay, it'd build on the op log (which already records every accepted op) rather than autosave.
- **Compression.** Autosave files are markdown at personal-vault scale, written briefly, read once. zstd would save bytes but adds an encode step in the hot tick path for no measured win. `changes.md`'s `changes-content-zstd` lives where compression actually pays off (long retention, many rows).
- **Cross-device autosave sync.** Autosave is per-machine crash recovery, not a device-handoff feature. The future sync layer in `design.md` syncs *committed* writes (saved files); in-flight uncommitted content stays local. This deliberately matches every other editor's posture.
- **Per-keystroke continuous flush.** 5s tick + on-blur flush is the right ergonomic floor; flushing every keystroke would bury the disk for negligible recovery improvement.
- **Recovery prompt / modal at vault open.** Auto-restore as dirty tabs is the design; the dirty-marker, the close-dirty modal, and `editor-diff-vs-disk-toggle` together cover everything a per-row Restore/Discard prompt would.
- **Untitled / scratch buffers.** Hiker has no untitled-buffer concept today (`sidebar-new-item-button` mints a path immediately). When and if scratch buffers land, autosave will need a path-less storage shape; not designed here.


## Forward refs

- `editor.md` "Multi-buffer model" — `multi-buffer-in-memory-only` is restated here in terms of "in-memory plus tab-state snapshot." The autosave layer is the persistence mechanism behind that restatement.
- `editor.md` "Save UX" — the deferred "Future: autosave on idle / on blur" entry is now this spec, with the explicit shift from "write through to the user's file on idle" to "write a sidecar shadow copy for crash recovery." The user-file save path is unchanged.
- `watcher.md` `watcher-ignore-hardcoded` — autosave writes don't reach the watcher because `.hiker/` is hard-coded ignored.
- `op-log.md` / `changes.md` — separate store, separate lifecycle. Autosave never appends to the op log; saving a recovered buffer routes through the existing save path, which appends accepted ops as usual.
- `design.md` "Sync / backup" — autosave directory falls into the regenerable bucket; the `index.json` is durable but trivial.
