# Context menus

A generalized right-click menu layer. Every menu is a declarative `Menu<A>` value
built by a per-item-kind builder, rendered by one shared renderer, with the chosen
action returned to the caller for dispatch. Right-clicking the same kind of item
anywhere yields the same base options; a host surface (a canvas, a board) appends its
own contextual entries.

Key decisions, each detailed in its own section below: menus are data not imperative `ui` calls ([[spec:ctxmenu-menu-spec]]); one shared renderer ([[spec:ctxmenu-renderer]]); the core lives in `egui_workbench::menu`, the home for shared egui primitives ([[spec:ctxmenu-crate]]); one builder per item kind ([[spec:ctxmenu-target-builder]]); contextual entries compose onto the base ([[spec:ctxmenu-contextual-extend]]); dispatch stays per-domain, no global action enum ([[spec:ctxmenu-deferred-dispatch]]); the lean `canvas-view` widget stays menu-library-free behind a host seam ([[spec:ctxmenu-canvas-seam]]).


## The `Menu<A>` model

A menu is a list of sections; sections render with separators between them. Building a
`Menu<A>` is pure data construction — no `ui` borrow, so builders are unit-testable. [ctxmenu-menu-spec]
status:: done
note:: `Menu<A>` declarative data (sections of `Entry<A>`), generic over the caller's action; built with no `ui` borrow so builders unit-test · evidence: `egui-workbench/src/menu/mod.rs`, `builder.rs`

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
status:: done
note:: `Entry<A>`: Action / Toggle / Submenu / Custom (live-widget escape hatch); fluent builder (`.action`/`.action_with`/`.toggle`/`.submenu`/`.custom`/`.section`/`.extend`) · evidence: `egui-workbench/src/menu/mod.rs` (`Entry`/`Action`/`Enabled`/`Icon`), `builder.rs`
- **Disabled with a reason.** `Enabled` is `Yes | No(reason)`; a disabled entry renders
  greyed with the reason as its hover tooltip, so the menu teaches *why* an action is
  unavailable instead of hiding it. [ctxmenu-disabled-reason]
status:: done
touches:: [[code:hiker/menu/render]]
note:: `Enabled::No(reason)` renders greyed with reason as tooltip (teaches why unavailable) · evidence: `egui-workbench/src/menu/render.rs`; files "Already in '…'" trail entry
- **Icons** are an egui-agnostic `Icon` newtype (glyph string for now); the renderer
  draws them in a fixed leading slot so labels align.

`A` is the caller's own verb type (`FileVerb`, `CardAction`, `EditOp`, …). The core
crate never names a concrete action — it is generic end to end.


## The renderer

`menu::show(ui: &mut Ui, menu: Menu<A>) -> Option<A>` walks the spec, emits one egui
widget per entry, and returns the action of whichever entry was activated (`None`
otherwise). It is the single place that calls `ui.button` / `ui.close` / `ui.menu_button`. [ctxmenu-renderer]
status:: done
touches:: [[code:hiker/menu/render]]
note:: single `show(ui, menu) -> Option<A>`; only code touching egui menus (separators, disabling, submenus, close); pure, no mutation · evidence: `egui-workbench/src/menu/render.rs` (`show`), `egui-workbench/tests/menu_render.rs`

- Sections render top-to-bottom with `ui.separator()` between non-empty groups; empty
  sections are skipped so composition never leaves a dangling rule.
- Submenus recurse via the same `show`; the chosen action bubbles up. Closing on click,
  nested-submenu state, and escape handling ride egui's built-in menu machinery
  (`containers/menu.rs`) — the renderer does not reimplement them.
- The renderer is pure: it performs no mutation and holds no app reference, which is
  what lets it live as a generic module in `egui_workbench::menu`. That generalized core is
  the generic egui-only `egui_workbench::menu` module — the home for shared egui UI
  primitives; `canvas-view` stays menu-lib-free via [[spec:ctxmenu-canvas-seam]]. [ctxmenu-crate]
status:: done
note:: evidence: `egui-workbench/src/menu/` (`pub mod menu`); `app` deps `egui_workbench` already

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
status:: done
touches:: [[code:hiker/panels/canvas/menu]]
note:: one builder fn per item kind returns its base `Menu<A>` — single source of truth for "what can you do to this kind of thing" · evidence: `build_search_hit_menu`, `build_file_menu`, `build_board_column_menu`, `app/src/panels/canvas/menu.rs`

- **Context is gathered when the menu opens, not per frame.** Builders receive the
  context they need (board membership, available canvases, active trail) computed lazily
  inside the `context_menu` closure, matching today's file-tree behavior. Items reflect
  current state because they are rebuilt on each open. [ctxmenu-build-on-open]
status:: done
touches:: [[code:hiker/files/sidebar]]
note:: builder context gathered lazily in the `context_menu` closure, not per frame; items reflect current state · evidence: `app/src/files/sidebar.rs` (lazy `picker_context_ctx`/`list_canvases` inside the menu closure)
- **A `MenuCx` carries the read-only handles** a builder needs (vault, trees, active
  trail). It is a borrow bundle, distinct from the app's mutable dispatch context.

### Contextual composition

A host surface starts from the shared base and appends its own section: [ctxmenu-contextual-extend]
status:: done
touches:: [[code:hiker/panels/canvas/menu]]
note:: host surfaces `.extend` the shared base with a contextual section (canvas node zoom); base never re-listed · evidence: `app/src/panels/canvas/menu.rs` (`build_node_menu` = zoom section `.extend` Delete base)

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
status:: done
touches:: [[code:hiker/item_menu]]
note:: shared base menu for any note/path-referencing list item — Open · Reveal in file tree · Copy path (Custom) · Properties — defined once, composed via `.extend`/prepend everywhere · evidence: `app/src/item_menu.rs` (`ItemAction`, `BaseOpts`, `note_item_base`)

- **One builder.** `item_menu::note_item_base(path, opts, wrap)` returns the
  base section — **Open · Reveal in file tree · Open in graph · Copy path · Properties** — living
  in `app/src/item_menu.rs`; every list routes through it.
- **One dispatch.** `item_menu::apply_item_action(app, action, path)` applies the base action
  (Open→`open_file`, Reveal→`reveal_in_files`, Properties→`open_properties`); Copy path copies at
  render via the Custom entry. [ctxmenu-item-base-apply]
status:: done
touches:: [[code:hiker/item_menu]]
note:: evidence: `app/src/item_menu.rs` (`apply_item_action`); `attach_note_item_menu` / `note_item_menu_response`
- **"Open in graph"** is a base entry on the note-item builder, so it appears everywhere a
  note item is right-clicked — file tree, search results, backlinks/related/appears-in,
  vault-view rows, board cards, trail waypoints, queue/changes/project rows, and the graph
  nodes themselves — with zero per-surface wiring (exactly the composition this section
  exists for). Dispatch: open/focus the singleton Graph tab on that note's neighbourhood at
  depth 2, the code view's default ([[spec:graph-tab-focus]] owns the open/seed mechanics;
  this doc only registers the entry). [open-in-graph]
status:: done
implements:: [[code:hiker/item_menu/note_item_base]], [[code:hiker/item_menu/apply_item_action]]
touches:: [[code:hiker/panels/graph/open_focused]]
note:: graph-unification-plan §4 Phase C; the one-line-edit adoption promise of [[spec:ctxmenu-item-base-adoption]] held — the entry + its dispatch arm were the only wiring, every base-composing surface picked it up unchanged
- **Container variants.** Where the *container* is the natural target, its host menu adds the
  container verb: the board title's right-click menu carries **"Open board in graph"**
  (focus = the board-doc node, its cards one membership edge away at depth 1) and the trail
  sidebar's header ⋯ menu carries **"Open trail in graph"** (the trail-doc node, its
  waypoints' source notes at depth 1). Menus are the universal path; no extra header buttons
  were added — graph-jumping is not the primary move on either surface
  (`interaction.md` [new-item-placement] reserves persistent header affordances for primary
  verbs). [open-in-graph-containers]
status:: done
implements:: [[code:hiker/panels/board/render_title]], [[code:hiker/trails/sidebar/overflow_menu]]
note:: the board title previously had no context menu (double-click renames); it now latches a one-verb menu per [rightclick-menu-always] — a menu, never a direct action
- **Scoped-graph variant on query-docs.** The smart-folder header row (Vault mode's
  query-doc row) composes **"Open in graph, scoped"** onto the base — the vault graph
  bounded to that query's match set. Dispatch and composition semantics live with
  [[spec:graph-scoped-query]] in `graph-view.md`; this doc only registers the entry
  (`app/src/vault_view/mod.rs::query_header_menu`, the [[spec:ctxmenu-contextual-extend]]
  shape — base first, contextual section after).
- **Spec-note variant on vault graph nodes.** A vault-graph node defining `[slug]` spec
  anchors composes an **"Open in code graph"** submenu (one entry per anchor) onto the
  note-item base — the jump that lights the spec on the code side
  ([[spec:vault-graph-spec-drift-badge]] owns the behavior; `app/src/panels/graph.rs::node_menu_ui`).
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
status:: done
note:: every note-referencing sidebar list shows the base + its own extras; the menu-less lists gained menus · evidence: files/trails/clusters-leaf/search-card compose the base (`*Verb::Base(ItemAction)`); vault-view/related/backlinks/search-chunks get the base directly


## Dispatch

The renderer returns the chosen action; applying it is the caller's concern, because the
two menu hosts have different mutation models. [ctxmenu-deferred-dispatch]
status:: done
note:: renderer returns chosen action; no global action enum — dispatch stays per-domain · evidence: `app` callers via `ctx.defer`; canvas via `EditOp`/undo + `CanvasResponse` flags

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
  primitive nor the workbench, per [[spec:ctxmenu-canvas-seam]] above). [ctxmenu-canvas-seam]
status:: done
touches:: [[code:hiker/panels/canvas/menu]]
note:: lean `canvas-view` exposes a menu host-seam (like `NodeContentRenderer`) and depends on neither the menu primitive nor `egui_workbench`; app supplies menus built on `egui_workbench::menu` · evidence: `hiker-canvas/view/src/menu.rs` (`CanvasMenuRenderer`), `widget.rs` (`show` takes `&mut dyn CanvasMenuRenderer`), `app/src/panels/canvas/menu.rs` (impl)

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

[ctxmenu-search]
status:: done
note:: search-result card menu; `allow_context` gating + four labels unchanged · evidence: `app/src/search/mod.rs` (`build_search_hit_menu`)

[ctxmenu-files]
status:: done
touches:: [[code:hiker/files/sidebar]]
note:: file/folder row menu; the two pickers ride `Custom` entries (live nested `menu_button` with per-board disabling + runtime canvas list) · evidence: `app/src/files/sidebar.rs` (`build_file_menu`)

[ctxmenu-board]
status:: done
touches:: [[code:hiker/panels/board]]
note:: board column menu; rename/delete-with-cards route via new `StartRenameColumn`/`RequestDeleteColumn` `BoardAction`s; WIP presets are `Toggle`s · evidence: `app/src/panels/board.rs` (`build_board_column_menu`, `build_wip_limit_menu`)

[ctxmenu-canvas]
status:: done
implements:: [[code:hiker/panels/canvas/render/canvas_body]]
touches:: [[code:hiker/panels/canvas/menu]]
note:: node/edge/empty menus; node menu proves [[spec:ctxmenu-contextual-extend]] · evidence: `app/src/panels/canvas/menu.rs` (builders + `CanvasMenus` impl), `hiker-canvas/view/src/menu.rs` (enums + seam), `widget.rs` (apply)

[ctxmenu-clusters]
status:: done
implements:: [[code:hiker/clusters/sidebar/node_menu/build_cluster_node_menu]]
note:: cluster-tree node menu; Policy / Move-to / Merge / Promote are `Custom` (live data + checkmark-with-tooltip rows); enabled-item hover tooltips on Split/Summarize/Merge-up dropped (renderer tooltips only on disabled) · evidence: `app/src/clusters/sidebar/node_menu.rs` (`build_cluster_node_menu`, `NodeVerb`), `tree.rs` (`apply_node_verb`)

[ctxmenu-trails]
status:: done
touches:: [[code:hiker/trails/sidebar]]
note:: waypoint card menu; applies to the same `TrailActions` fields after `show` · evidence: `app/src/trails/sidebar.rs` (`build_waypoint_menu`, `WaypointVerb`)

[ctxmenu-editor-clipboard]
status:: done
implements:: [[code:hiker/panels/buffer/clipboard_menu/attach]]
note:: cut/copy/paste/select-all; same focus + viewport-command / synthetic-key sequence after `show` · evidence: `app/src/panels/buffer/clipboard_menu.rs` (`build_clipboard_menu`, `ClipboardVerb`)

[ctxmenu-changes]
status:: done
touches:: [[code:hiker/panels/changes]]
note:: changes-row menu over the existing `Action` enum — composes the note-item base ([[spec:ctxmenu-item-base]]) + view history / rollback since 2026-06-12 · evidence: `app/src/panels/changes.rs` (`build_changes_row_menu`)

[ctxmenu-diff-source]
status:: done
touches:: [[code:hiker/panels/buffer/show_changes]]
note:: diff-source button; "Show changes…" is a data-driven `.submenu` (history gathered on open) · evidence: `app/src/panels/buffer/show_changes.rs` (`build_diff_source_menu`, `DiffSourceVerb`)

[ctxmenu-toolbar]
status:: done
touches:: [[code:hiker/toolbar]]
note:: vault-label "Set as default vault"; other toolbar menus left on `menu_button` (out of scope) · evidence: `app/src/toolbar.rs` (`build_vault_label_menu`, `VaultLabelVerb`)

Search, file tree, and board already return verb enums, so they convert near-mechanically
and are the reference conversions. Canvas proves contextual composition and the `Custom`
escape hatch. The direct-mutation surfaces (trails, changes, zim, clipboard, vault label)
swap inline mutation for a returned verb applied via `ctx.defer`.


## Deferred

- **`when`-clause conditions.** A shared `Condition` vocabulary (`has_selection`,
  `canvas_focused`, …) driving `Enabled` and, later, keybinding availability, instead of
  inline booleans in each builder. Sketched in `scratch/future_abstractions.md`. [ctxmenu-conditions]
status:: planned
note:: deferred: shared `Condition` vocabulary driving `Enabled` + keybind availability; see `scratch/future_abstractions.md`
- **Shortcut display from a command layer.** Once a unified command enum exists, entries
  show their bound keybind via a reverse keymap. Depends on the command work in
  `scratch/future_abstractions.md`; the `shortcut` field on `Action` is reserved for it. [ctxmenu-shortcut-display]
status:: planned
note:: deferred: entries show bound keybind via reverse keymap once a command layer exists; `shortcut` field reserved
- **Command-palette reuse.** The same builders/verbs feeding a fuzzy command palette is a
  command-layer concern, not this doc's. [ctxmenu-palette-reuse]
status:: planned
note:: deferred: same builders/verbs feeding a fuzzy command palette; command-layer concern


## Out of scope

- **The toolbar customize menus** (`BarOp`, add-action picker) stay on egui `menu_button`
  for now; they are left-click dropdowns and toolbar-edit affordances, not item context
  menus. The `Menu<BarOp>` shape would fit them, but converting them is not part of this
  unification.
- **Native OS menus.** Hiker renders its own menus through egui; no `muda`-style native
  menu path.
- **A global action enum / contribution registry.** Per-domain verbs stay; plugin-style
  menu contribution is a plugin-era concern (`scratch/plugins.md`).
