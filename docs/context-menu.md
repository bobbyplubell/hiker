# Context menus

A generalized right-click menu layer. Every menu is a declarative `Menu<A>` value
built by a per-item-kind builder, rendered by one shared renderer, with the chosen
action returned to the caller for dispatch. Right-clicking the same kind of item
anywhere yields the same base options; a host surface (a canvas, a board) appends its
own contextual entries.

Key decisions, each detailed in its own section below: menus are data not imperative `ui` calls (`ctxmenu-menu-spec`); one shared renderer (`ctxmenu-renderer`); the core lives in `egui_workbench::menu`, the home for shared egui primitives (`ctxmenu-crate`); one builder per item kind (`ctxmenu-target-builder`); contextual entries compose onto the base (`ctxmenu-contextual-extend`); dispatch stays per-domain, no global action enum (`ctxmenu-deferred-dispatch`); the lean `canvas-view` widget stays menu-library-free behind a host seam (`ctxmenu-canvas-seam`).


## The `Menu<A>` model

A menu is a list of sections; sections render with separators between them. Building a
`Menu<A>` is pure data construction — no `ui` borrow, so builders are unit-testable. [ctxmenu-menu-spec]

```rust
pub struct Menu<A> { sections: Vec<Vec<Entry<A>>> }

pub enum Entry<A> {
    Action  { label: Cow<'static, str>, icon: Option<Icon>, enabled: Enabled, shortcut: Option<String>, action: A },
    Toggle  { label: Cow<'static, str>, checked: bool, action: A },
    Submenu { label: Cow<'static, str>, menu: Menu<A> },
    Custom(Box<dyn FnOnce(&mut egui::Ui) -> Option<A>>),
}
```

Builder surface (fluent, mirrors the data): `Menu::new()`, `.action(label, a)`,
`.toggle(label, checked, a)`, `.submenu(label, sub)`, `.section()` to start a new
separator-delimited group, and `.extend(other)` to splice a contextual section.

- **Entry kinds.** `Action` is the common case. `Toggle` carries a checkmark. `Submenu`
  nests a `Menu<A>`. `Custom` is the escape hatch for the rare entry that must render a
  live widget (the canvas zoom row, a WIP-limit radio) and yields an `A` when used. [ctxmenu-entry-kinds]
- **Disabled with a reason.** `Enabled` is `Yes | No(reason)`; a disabled entry renders
  greyed with the reason as its hover tooltip, so the menu teaches *why* an action is
  unavailable instead of hiding it. [ctxmenu-disabled-reason]
- **Icons** are an egui-agnostic `Icon` newtype (glyph string for now); the renderer
  draws them in a fixed leading slot so labels align.

`A` is the caller's own verb type (`FileVerb`, `CardAction`, `EditOp`, …). The core
crate never names a concrete action — it is generic end to end.


## The renderer

`menu::show(ui: &mut Ui, menu: Menu<A>) -> Option<A>` walks the spec, emits one egui
widget per entry, and returns the action of whichever entry was activated (`None`
otherwise). It is the single place that calls `ui.button` / `ui.close` / `ui.menu_button`. [ctxmenu-renderer]

- Sections render top-to-bottom with `ui.separator()` between non-empty groups; empty
  sections are skipped so composition never leaves a dangling rule.
- Submenus recurse via the same `show`; the chosen action bubbles up. Closing on click,
  nested-submenu state, and escape handling ride egui's built-in menu machinery
  (`containers/menu.rs`) — the renderer does not reimplement them.
- The renderer is pure: it performs no mutation and holds no app reference, which is
  what lets it live as a generic module in `egui_workbench::menu`.

Attaching a menu to a widget at the call site (egui's `context_menu` closure must
return `()`, so the chosen action is captured out):

```rust
let mut chosen = None;
response.context_menu(|ui| chosen = menu::show(ui, build_file_menu(&cx, rel)));
if let Some(verb) = chosen {
    ctx.defer(move |app| apply_file_verb(app, verb, &rel));
}
```


## Builders: one per item kind

Each kind of right-clickable item has exactly one builder that returns its base menu.
This is the "same item kind → same options" guarantee: the options live in one function
(`build_file_menu`, `build_search_hit_menu`, `build_canvas_node_menu`, …, each returning a
`Menu<DomainVerb>`), not copied across surfaces. [ctxmenu-target-builder]

- **Context is gathered when the menu opens, not per frame.** Builders receive the
  context they need (board membership, available canvases, active trail) computed lazily
  inside the `context_menu` closure, matching today's file-tree behavior. Items reflect
  current state because they are rebuilt on each open. [ctxmenu-build-on-open]
- **A `MenuCx` carries the read-only handles** a builder needs (vault, trees, active
  trail). It is a borrow bundle, distinct from the app's mutable dispatch context.

### Contextual composition

A host surface starts from the shared base and appends its own section: [ctxmenu-contextual-extend]

```rust
let mut m = build_canvas_node_menu(id);   // Delete, Rename — shared with anywhere a node appears
m.section();
m.extend(canvas_node_extras(id));         // Zoom in/out, Reset zoom — canvas-only (Custom entries)
menu::show(ui, m)
```

The same `File` right-clicked in the tree versus dropped on a canvas yields an identical
base with a different tail. Hosts never re-list the base entries.


## The shared base for sidebar list items

Most rows in hiker's sidebar lists — file tree, trails, cluster-tree leaves, search hits,
vault-view leaves, related, backlinks — reference a note/path. They all share one base
menu, defined once and reused, so right-clicking a note anywhere offers the same core
actions. [ctxmenu-item-base]

- **One builder, one dispatch.** `item_menu::note_item_base(path, opts, wrap)` returns the
  base section — **Open · Reveal in file tree · Copy path · Properties** — and
  `item_menu::apply_item_action(app, action, path)` applies it. Both live in
  `app/src/item_menu.rs`; every list routes through them. [ctxmenu-item-base, ctxmenu-item-base-apply]
- **Composition, not duplication.** A surface with extra actions adds a `Base(ItemAction)`
  variant to its verb enum, prepends the base, then `.section()` + its own items:
  ```rust
  let mut m = note_item_base(path, BaseOpts { reveal: true }, FileVerb::Base);
  m.section();
  m.extend(file_specific_items(rel, …));   // Rename, Duplicate, Delete, Add to trail/board/canvas …
  ```
  On dispatch, `FileVerb::Base(a) => apply_item_action(app, a, rel)`. Surfaces that have no
  extra actions show the base directly as `Menu<ItemAction>`.
- **`BaseOpts.reveal`** drops "Reveal in file tree" when the list *is* the file tree (it
  would be a no-op there).
- **Copy path** is a `Custom` entry (it copies via `ui.ctx().copy_text` at render time,
  since the deferred `&mut AppState` dispatch path has no egui context) — the one base
  action that doesn't go through `apply_item_action`.

Adding a base action later (e.g. "Add to trail") is a one-line edit to `note_item_base` +
`apply_item_action` and it appears on every list at once. [ctxmenu-item-base-adoption]


## Dispatch

The renderer returns the chosen action; applying it is the caller's concern, because the
two menu hosts have different mutation models. [ctxmenu-deferred-dispatch]

- **`app` surfaces** funnel every action through `ctx.defer(|app| …)` so mutation happens
  at frame end, never mid-render. This is the single app-side dispatch path; the
  hand-rolled mix of direct mutation, method calls, viewport commands, and inline config
  writes collapses into it.
- **The canvas widget** (`canvas-view`) keeps its menu *action enums* (plain data) and its
  effect application — applying the returned action inside its existing `EditOp` + undo
  pipeline, or setting a request flag on `CanvasResponse` for actions the host must
  complete (new note, link prompt, insert-from-vault). It does not build menus itself: a
  host-supplied `CanvasMenuRenderer` seam (passed into `show`, alongside the existing
  `NodeContentRenderer`) returns the chosen action per right-clicked target, and the
  widget applies it. The hiker app implements that seam in `app/src/panels/canvas/menu.rs`
  using `egui_workbench::menu` (the seam is why `canvas-view` depends on neither the menu
  primitive nor the workbench, per `ctxmenu-canvas-seam` above). [ctxmenu-canvas-seam]

## Adoption

Every existing hand-rolled menu moves to a builder + `menu::show`. Targets, each its own
builder returning a domain action type:

| Surface | Builder | Action type | Slug |
| --- | --- | --- | --- |
| Search result card | `build_search_hit_menu` | `CardAction` | [ctxmenu-search] |
| File / folder tree row | `build_file_menu` | `FileVerb` | [ctxmenu-files] |
| Board column | `build_board_column_menu` | `BoardAction` | [ctxmenu-board] |
| Canvas node / edge / empty | `build_canvas_*_menu` | canvas `NodeAction`/… | [ctxmenu-canvas] |
| Cluster tree node | `build_cluster_node_menu` | cluster verb | [ctxmenu-clusters] |
| Trail waypoint card | `build_waypoint_menu` | trail verb | [ctxmenu-trails] |
| Editor clipboard | `build_clipboard_menu` | clipboard verb | [ctxmenu-editor-clipboard] |
| Changes row | `build_changes_row_menu` | changes verb | [ctxmenu-changes] |
| Diff-source button | `build_diff_source_menu` | diff verb | [ctxmenu-diff-source] |
| Toolbar vault label | `build_vault_label_menu` | toolbar verb | [ctxmenu-toolbar] |

Search, file tree, and board already return verb enums, so they convert near-mechanically
and are the reference conversions. Canvas proves contextual composition and the `Custom`
escape hatch. The direct-mutation surfaces (trails, changes, zim, clipboard, vault label)
swap inline mutation for a returned verb applied via `ctx.defer`.


## Deferred

- **`when`-clause conditions.** A shared `Condition` vocabulary (`has_selection`,
  `canvas_focused`, …) driving `Enabled` and, later, keybinding availability, instead of
  inline booleans in each builder. Sketched in `scratch/future_abstractions.md`. [ctxmenu-conditions]
- **Shortcut display from a command layer.** Once a unified command enum exists, entries
  show their bound keybind via a reverse keymap. Depends on the command work in
  `scratch/future_abstractions.md`; the `shortcut` field on `Action` is reserved for it. [ctxmenu-shortcut-display]
- **Command-palette reuse.** The same builders/verbs feeding a fuzzy command palette is a
  command-layer concern, not this doc's. [ctxmenu-palette-reuse]


## Out of scope

- **The toolbar customize menus** (`BarOp`, add-action picker) stay on egui `menu_button`
  for now; they are left-click dropdowns and toolbar-edit affordances, not item context
  menus. The `Menu<BarOp>` shape would fit them, but converting them is not part of this
  unification.
- **Native OS menus.** Hiker renders its own menus through egui; no `muda`-style native
  menu path.
- **A global action enum / contribution registry.** Per-domain verbs stay; plugin-style
  menu contribution is a plugin-era concern (`scratch/plugins.md`).
