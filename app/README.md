# hiker-app

Native egui desktop app. Built around the local `editor/` widget crates
and `egui_dock` for the tab/split workspace.

## Run

```
cargo run -p hiker-app -- /path/to/vault
```

Without an argument, falls back to `$HIKER_VAULT` then the current
working directory.

## What works

- Three-panel layout (sidebar / dock / discovery), all collapsible and
  resizable
- File tree (Files mode) with click-to-open in preview slot, double-click
  to promote sticky, right-click context menu (Open / Properties /
  Delete)
- `+ New note` creates `new-note-N.md` in the selected folder
- egui_dock central workspace with custom tab semantics: kind-aware icons,
  preview-slot italics, dirty dots, drag-to-split, drag-between-leaves
- Buffer tabs: full markdown live preview via `editor/editor-md`
  (headings, bold/italic, lists, links, callouts, wikilinks,
  transclusion, math, mermaid, frontmatter folding) + toolbar + status bar
- `Cmd-S` / `Ctrl-S` save with drift check; dirty-buffer close dialog
- Modal + toast widgets
- Home / Queue / Settings / Properties / Graph / Agent tab kinds (most as
  placeholders pending feature port)
- Light theme matching `editor::light_default`

## What's still pending (rough order)

See the project-level task list. Major remaining ports:

- Settings UI (currently dumps the loaded TOML; real form ports per
  `docs/settings.md`)
- Chat panel + sessions (currently placeholder)
- Discovery panel search + related notes (currently placeholder)
- Patch-review + snapshot/staging-preview tab bodies
- Cluster editor body
- File tree DnD between folders + inline rename
- Trash bin actual listing + restore
- Autosave + tab-state persistence
- External-change watcher integration
- Real index status in the status bar
