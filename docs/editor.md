# Editor

CodeMirror 6 inside the Tauri webview. This document specs the editor surface itself — buffer model, save UX, keybinds, status bar, and the extension layout that future features (live preview, wikilinks, autocomplete) will slot into. Live-preview decorations and widget rendering are out of scope here; see design.md.


## Buffer model

One open buffer at a time in v0. The buffer is identified by its vault-relative path (`currentPath`). Switching files replaces the buffer's contents via a single `dispatch` that swaps the entire doc.

State tracked per buffer:

- `path` — vault-relative; null when no file is open
- `loadedHash` — hash (or full string) of the contents most recently read from / written to disk
- `isDirty` — derived: current doc text !== loadedHash

`isDirty` is the single source of truth for save state. Computed lazily from the editor doc and `loadedHash` — no separate "dirty flag" that can desync. Cleared by re-reads and successful writes; set implicitly by any edit.

Multi-buffer / tabs are deferred. The model above keeps single-buffer simple but generalizes — when tabs land, the same per-buffer state moves into a `Buffer[]` keyed by path, with the active buffer driving the editor view.


## Save UX

Save action: writes current doc to `currentPath` via the `write_file` core command. On success, updates `loadedHash` to the new doc text, which clears `isDirty`. On error, surfaces a non-blocking error toast and leaves the dirty state alone (so the user can retry).

Triggers (all funnel into the same save function):

- Mod-S keybind
- Save button in the status bar (visible always; disabled when no file is open or when not dirty)
- Future: autosave on idle / on blur (deferred — opt-in setting later)

Dirty indicator:

- Window title shows `• Hiker — <path>` when dirty, `Hiker — <path>` when clean.
- Status bar save button shows a filled-dot icon when dirty, empty when clean.
- Active file in the tree shows a small dot suffix when its buffer is dirty.

File-switch guard: clicking another file while the current buffer is dirty pops a confirm dialog with three options — Save & switch, Discard & switch, Cancel. Cancel keeps the current buffer active. The same guard applies to closing the window: a `before-close` listener on the Tauri window cancels the close if dirty and prompts; user choice (save / discard / cancel) decides whether the close proceeds.

External changes: two mechanisms, layered.

- Pre-write drift check (v0). Every save re-reads the file from disk and compares its hash to `loadedHash` before writing. Three outcomes:
    - match — write proceeds normally; on success `loadedHash` updates to the new doc text.
    - file missing — prompt: write anyway (re-creates) / cancel.
    - hash mismatch — conflict prompt: keep mine (overwrite, lose disk version) / take theirs (discard buffer, reload from disk) / open diff (deferred — falls back to keep/take in v0).

    This catches the common "I edited the file in vim while it was open in Hiker" case without needing a watcher.

- Watcher integration (v1). When the notify-based watcher lands with the indexer, it pushes file-change events to the frontend for the currently open file. Behavior:
    - buffer clean — silently reload from disk; `loadedHash` updates.
    - buffer dirty — same conflict prompt as above, but proactive (fired on the change event, not deferred to save time).

    The watcher reduces the window where the user can edit a stale buffer; the pre-write check remains as a final guard since the watcher can miss events (network filesystems, rapid changes, race between event and save).

The pre-write check and the watcher both reduce to the same conflict-resolution UI; only the trigger differs.


## Keybind registry

A single module owns all keybindings as a flat list. The registry is an introspection layer, not a translator — CM6's `keymap.of([...])` is the only sink in v0. Goals: discoverable (a help panel can enumerate `list()`), overridable (user config later), conflict-detectable.

Shape:

```ts
interface Binding {
  id: string;            // "editor.save", "editor.toggleBold"
  keys: string;          // CM6 chord syntax: "Mod-s", "Mod-Shift-p"
  label: string;         // human-readable for help panel
  run: (view: EditorView) => boolean;   // returns true if handled
}
```

Compilation: `registry.toCMKeymap()` returns a CM6 extension built from `keymap.of(bindings.map(b => ({key: b.keys, run: b.run})))`. The editor wires this in once at startup.

Validation: a `registry.validate()` pass at startup logs and throws on duplicate `id` or duplicate `keys`. No silent overrides.

Scope: v0 has one scope — the editor. Bindings only fire when the editor has DOM focus. When a future binding needs to fire outside the editor (e.g. `Mod-P` quick-open from any pane), reuse CM6's exported `keyName` parser in a window-level `keydown` handler — never roll a custom chord parser. Add a `scope` field then; until then, omit it.

v0 bindings:

| ID            | Keys  | Action              |
| ------------- | ----- | ------------------- |
| `editor.save` | Mod-S | save current buffer |

Reserved IDs (real impls later, not registered as no-ops in v0):

| ID                     | Keys        | Action                    |
| ---------------------- | ----------- | ------------------------- |
| `vault.openFile`       | Mod-P       | quick-open by filename    |
| `vault.commandPalette` | Mod-Shift-P | open command palette      |
| `editor.toggleBold`    | Mod-B       |                           |
| `editor.toggleItalic`  | Mod-I       |                           |

Override mechanism (deferred): a user keybind file (`vault/.hiker/keybinds.toml`) overrides any binding's `keys` by `id`. The registry's flat-list shape supports this trivially; the loader is later.


## Status bar

Bottom strip across the editor pane only (not under the tree). Three regions:

- left: save button + dirty dot, current file path (relative to vault root)
- center: index status label (v1+) — short text reflecting indexer state. Concretely: `Model loading…` while the embedder loads, `Indexing X/Y` while jobs flow (X = remaining queue depth, Y = total since last idle), `Indexed (N notes)` when idle, `Index error` (with last_error in title attribute) when the indexer reports a failure. Plain text, no icons in v1; styling can come later.
- right: line:col, word count, file type badge (`md`)

Click targets:

- save button → save action
- file path → copy path to clipboard
- line:col → opens a goto-line input (deferred; click is a no-op in v0)


## Layout (v1)

Three columns, both sides collapsible:

- **Left**: file tree (existing `#sidebar`). Collapsible. Supports drag-and-drop to move notes between folders — the drop calls a single core `move_note` command that does the fs rename and updates the index path in one step, so the move is recorded explicitly rather than being inferred from watcher events. Same code path is exposed as a `hiker mv` CLI command.

  Tree toolbar at the top of the sidebar: a wide **+ New note** button and a small refresh icon next to it. The asymmetry is the point — new-note is a frequent action, refresh is a sanity-check fallback.

  - **New note** creates `new_note.md` in the currently-selected folder (vault root if nothing's selected) via a `create_note(rel_path)` core command. The new file opens in the editor immediately, and the tree row enters inline-rename mode with the `new_note` basename pre-selected (extension excluded from selection so users can type a new name and hit Enter without re-typing `.md`). Submit renames via the same `move_note` path; Esc keeps the default name.
  - **Refresh** re-reads the directory and rebuilds the tree from disk. With the v1 watcher, the tree should mostly stay in sync on its own — refresh is a backstop for the watcher's known failure modes (notify queue overflow during big git checkouts, NFS/network filesystems, missed events) and for the "did I really just save that" sanity case. Auto-refresh from watcher events is a v2 add per `watcher.md`; the manual button stays even after that lands.
- **Center**: editor pane with a thin toolbar strip across its top, then the editor below, then the existing status bar. Toolbar holds two toggle buttons (VSCode-style icons or simple labels) — left button toggles the tree, right button toggles the related panel. Both buttons are always visible; their pressed/unpressed state reflects whether the corresponding panel is open.
- **Right**: related-notes panel. Collapsible. Renders `RelatedHit[]` from `related_notes(currentPath)`. Updated on file-open and on save (debounced 500ms per index.md).

Default state on first launch: tree open, related panel collapsed. Persistence of these toggles across launches is a settings concern (see settings.md) — for v1 the state lives in-memory only.

CSS: a 3-column grid where the side columns collapse to width 0 (or `display: none`) when toggled. Editor column is `1fr`; sides are fixed widths. Toolbar lives inside the editor column so the buttons sit where the user's eyes naturally are.


## Extension load order (CM6)

Order matters in CM6 — earlier extensions take precedence for keymaps and overlap-able decorations. Canonical order:

1. `basicSetup` — gutters, history, default keymap
2. `EditorState.tabSize.of(2)`
3. `EditorView.lineWrapping`
4. language compartment (`markdown()`) — swappable later when we add other langs
5. `saveTracking` extension — updates dirty state, fires title-bar update
6. `keybinds.editorKeymap()` — our registry's editor-scope bindings
7. (future) `livePreview()` — syntax-marker hiding decorations
8. (future) `wikilinks()` — `[[id]]` parser extension + decorations
9. (future) `widgets()` — images, math, transclusions
10. theme

The `language` slot uses a `Compartment` so it can be reconfigured per-buffer without rebuilding the whole state (e.g. opening a `.json` sidecar would swap to JSON mode). Same pattern for `theme` later.

Editor instance is created once at startup and reused across buffer switches; switching files dispatches a doc-replacement transaction, never reconstructs the view.


## Out of scope (deferred)

- Live-preview decorations (syntax-marker hiding on cursor-out)
- Wikilink rendering and autocomplete
- Widget-based rendering (images, math, embeds, callouts)
- Multi-buffer / tabs / split panes
- Autosave timer
- Vim/Emacs keymaps
- User keybind overrides (the registry supports it; the loader is later)
- External-change watcher integration (v1)
