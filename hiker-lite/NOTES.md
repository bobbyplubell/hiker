# hiker-lite Phase 1 — implementation notes

Living scratchpad for things that came up while building Phase 1 but
don't yet belong in PLAN.md. Phase 2 should reread this before starting.

## Upstream friction (opportunities for editor / workbench crates)

- `egui_workbench::editor_area::EditorArea::entries` is `pub(crate)`. The
  workbench *does* expose `get(handle) / get_mut(handle)` on the editor
  area, so we ended up fine — but the first sketch of "sync the tab's
  dirty bit from the host buffer each frame" naturally reaches for
  `entries.get_mut`. Worth checking whether the `Workbench` itself
  should expose a `tab_mut(TabId)` convenience to avoid hosts having to
  reach through `workbench.editor_area`.
- `editor_egui::widget::Widget::new` requires `&mut ViewState` even
  when the host's tab tree only owns the state immutably during render.
  Our `Buffers` map sidesteps it by handing the host a mutable borrow
  keyed by `TabId`, but it means the editor can't be embedded "by
  reference" inside an immutable rendering closure.
- No top-level search primitive in `editor-core` — we hand-rolled a
  naive substring scan in `panels/find_replace.rs`. A `Rope::find_iter`
  on the rope (or a thin search module beside `transaction.rs`) would
  be reusable across hosts.
- The `Vfs` trait was sized for Phase 2 (`remove`, `parse`, `parent`,
  `as_string`, `is_root`) — but trimmed for Phase 1 to keep clippy
  green under workspace `-D warnings`. Reintroduce them when the
  OPFS backend / file-management surface lands.

## Phase 2 prep

- The native backend already lives behind
  `#[cfg(not(target_arch = "wasm32"))]`. Adding `src/vfs/wasm.rs` and a
  `target.'cfg(target_arch = "wasm32")'.dependencies` stanza in
  Cargo.toml is the bulk of the work.
- The file-search panel walks `std::path::Path` via `walkdir`. On wasm
  it needs an async equivalent built on `Vfs::list` — straightforward.
- The hex view is purely string-formatting; nothing wasm-hostile.
- `rfd`'s native folder picker is the only piece that doesn't have a
  drop-in wasm equivalent — Phase 2 will replace it with drag-and-drop
  folder import.

## Quirks left in place

- Closing a dirty tab silently drops unsaved edits (the dirty bullet
  in the tab title is the only warning). A save-prompt modal is
  Phase 1.5.
- The filetree only re-lists directories via the explicit "Refresh"
  button; it does not auto-invalidate on file save. Fine for Phase 1
  but worth wiring once we add a "new file" command.
- `Cmd-O` opens a folder picker even though it's an undocumented
  shortcut — keep or remove in the polish pass.
