# hiker-lite

A lightweight, Notepad++-style text editor built on hiker's editor stack. Ships alongside hiker proper. Targets native desktop today, wasm/OPFS browser next.

## Goals

- A standalone, usable text editor: open files, edit, save, search, hex-view.
- Reuse hiker's editor stack (`editor-core`, `editor-view`, `editor-egui`, `editor-md`, `egui_workbench`) without depending on the hiker `core` crate (vault, oplog, sync, indexer, plugins).
- Same source compiles to native desktop and to wasm running in a browser tab.
- Forcing function: a second consumer surfaces accidental coupling in the existing app's panels, paving the way for shared panel crates.

## Non-goals

- Wikilinks, backlinks, trails, clusters, boards, sync, agents, plugins, embeddings — none of it.
- Per-language syntax highlighting everywhere. Markdown via `editor-md` is the floor.
- Live-editing the user's real filesystem from the browser. OPFS only; import/export is the bridge.

## Architecture

### Crate layout

New crate `hiker-lite/` in the workspace, depending on:

- `editor-core`, `editor-view`, `editor-egui`, `editor-md` (workspace)
- `egui_workbench` (workspace)
- `eframe` (native + wasm)
- Native-only: `rfd` (native dialog), `walkdir` or `ignore` (gitignore-aware walk)
- Wasm-only: `web-sys` (OPFS), `wasm-bindgen-futures`, `js-sys`

### The Vfs seam

One trait, two backends, gated by `cfg`. All panels code against `Vfs`; nothing else touches `std::fs` or OPFS directly.

```rust
#[async_trait]
trait Vfs {
    async fn read(&self, path: &VfsPath) -> Result<Vec<u8>>;
    async fn write(&self, path: &VfsPath, bytes: &[u8]) -> Result<()>;
    async fn list(&self, dir: &VfsPath) -> Result<Vec<DirEntry>>;
    async fn remove(&self, path: &VfsPath) -> Result<()>;
}
```

- **Native** (`#[cfg(not(target_arch = "wasm32"))]`): `std::fs` under a user-chosen root.
- **Wasm** (`#[cfg(target_arch = "wasm32")]`): OPFS via `web-sys`. All ops async on main thread.

### App shell

- `eframe::App` impl owning a `Workbench`, a `Vfs` handle, an open-files map (`HashMap<VfsPath, EditorState>` with dirty flags), and a status bar.
- Workbench hosts the panels; tabs hold editor instances.

## Phasing

### Phase 1 — Native skeleton (current)

1. Create the crate; wire into workspace.
2. `Vfs` trait + native `std::fs` backend.
3. `eframe::App` + `Workbench` mount.
4. Editor host: open file → load `Rope` → display via `editor-egui` → Cmd-S save → dirty tracking.
5. Filetree panel (against Vfs, native dialog to pick root).
6. Filename fuzzy search (Cmd-P).
7. Find/replace in active file (Cmd-F / Cmd-H).
8. Hex view panel (binary file detect or explicit open-as-hex).

Outcome: a usable desktop text editor.

### Phase 2 — Wasm / OPFS

1. OPFS backend implementing `Vfs`.
2. `eframe` wasm build setup (trunk).
3. Import/export panel (drag-drop folder → OPFS; "Export" downloads zip), gated `#[cfg(target_arch = "wasm32")]`.
4. Audit and async-ify any sync I/O paths that snuck in.

Outcome: same app, in a browser tab.

### Phase 3 — Modularization back into hiker proper

1. Identify panels hiker would benefit from sharing: filetree, filename search, find/replace, hex view.
2. Extract into `hiker-panels-common/` (or one crate per panel).
3. Delete hiker's copies; depend on shared crate.
4. Iterate the `Vfs` trait if hiker's vault wants to plug in as a third backend.

### Phase 4 — Project-wide contents search

`rg`-via-subprocess on native; deferred on wasm (no obvious clean solution).

## Conventions

- Zero dependency on the hiker `core` crate.
- No reference to vault, oplog, wikilinks, trails, clusters, boards in hiker-lite code or docs.
- Async `Vfs` is the law; no sync I/O in shared panel code.
- Don't fork `editor-egui` or `egui_workbench` from inside hiker-lite. If something's missing, raise it; fix upstream.
