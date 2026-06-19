# Interaction grammar

The cross-surface conventions for how hiker signals that options exist and what each
input primitive does. One grammar, learned once: a user who has learned any list,
board, or graph has learned them all. `style.md` owns visual tokens; this doc owns
*behavior*. New surfaces cite this doc instead of restating conventions; divergences
in existing surfaces are tracked as `bug-…` rows in [`bug_tracking.md`](bug_tracking.md)
(grammar audit: `scratch/interaction-audit-6-11.md`, 2026-06-11).

The headline decisions:

- **Click opens; named surfaces are the only exceptions.** Primary click on a
  note-like item opens it. Editing canvases click-select; focus-nav graphs
  click-drill — and both keep double-click/menu as the open path. [click-opens]
- **Double-click takes ownership.** It edits text you own (rename, title, card
  text) or promotes a preview tab to sticky. It never means "open harder."
  [dblclick-take-ownership]
- **Right-click is a menu, always.** Never a direct action, never the only path
  to a primary action. Menus compose from per-item-kind base builders
  ([[spec:ctxmenu-target-builder]]). [rightclick-menu-always]
- **One signal per meaning.** "Click acts here" = hover wash + pointer cursor,
  everywhere. Any note reference hover-previews. [hover-open-signal]
- **Focus-loss commits, Esc cancels** — every inline editor, no exceptions.
  [inline-edit-lifecycle]
- **Destruction lives in the menu**, behind a confirm. Removing a *reference*
  (a card, a waypoint) may be a hover-revealed control; destroying *data* may not.
  [destructive-verbs-in-menu]

## The primitive ladder [interaction-primitive-ladder]
status:: partial
note:: normative summary; per-rule conformance below — after the 2026-06-12 cross-cutting batch (signal/sticky/drag/destructive/esc-back) the sole remaining partial is [keyboard-esc-ladder]'s deferred list arrow-nav parity

| primitive | meaning |
|---|---|
| hover | signal actionability (wash + pointer) + preview after dwell |
| click | open (exceptions: select on canvas, drill on focus-nav graphs) |
| mod-click | open sticky (bypass the preview-tab slot) |
| double-click | take ownership: inline-edit owned text / promote preview tab |
| right-click | the item's full menu — always a menu, never a direct action |
| drag | carry the item (notes: vault-relative path payload) to a container |
| Esc | cancel edit → up one level → close popup/panel (first that applies) |
| Enter | commit edit / run selected |
| middle-click | close (tabs); otherwise unbound |

## Click opens [click-opens]
status:: done
note:: lists, cluster graph nodes, board note-cards all open on click; the vault graph opens on click in its overview (and the click also anchors the scope dial) — in hops scope it drills, the focus-nav exception below ([[spec:graph-nav-extract]])

Primary click on a note-like item opens it in the preview tab slot
(`tab-preview-model`). Selection is not a separate step on list rows — the row
press that opens also selects.

### Named exceptions [click-exceptions]
status:: done
note:: both exceptions keep an open path: canvas double-click activates; focus-nav graphs keep Open on the node menu — the vault graph joined the code graph here when it gained hops scope ([[spec:graph-nav-extract]]; drill applies only while focused, overview clicks still open)

- **Editing canvases** (`canvas`): click selects — spatial editing needs
  selection as the primary gesture. Double-click activates/edits.
- **Focus-nav graphs** (`code-graph`, and any future graph with drill): click
  drills (re-centers the neighborhood). Opening is the menu's job.

## Mod-click opens sticky [modclick-sticky]
status:: done
note:: `bug-mod-click-sticky-incomplete` fixed 2026-06-12 — the branch is single-sourced (`widgets::note_row::open_sticky`, Cmd/Ctrl) and wired at every open site: vault-view rows, wikilink pills, search cards + chunk rows, backlinks, related, appears-in, board note-cards (plain click now lands in the preview slot per [click-opens]), git-diff rows · one named exception below

Anywhere a click opens a note, mod-click opens it sticky. The tab system already
distinguishes preview/sticky globally; the modifier must work at every open site.

**Named exception:** in lists with multi-select (the files tree), Cmd/Ctrl-click
is the selection *toggle* (`files.md` [[spec:note-multi-select]] — the
Finder/VS Code convention) and therefore cannot also mean open-sticky; plain
click still opens into the preview slot there.

## Double-click takes ownership [dblclick-take-ownership]
status:: done
note:: files rename, board title/column rename, canvas node edit, tab promote all conform

The unifying reading: double-click makes the thing *yours* — it enters
inline-edit on text you own, or promotes a preview tab to sticky. It is never an
alternate open gesture; surfaces must not bind it to navigation.

## Right-click is a menu, always [rightclick-menu-always]
status:: done
note:: holds everywhere an item kind renders — the seven note-list surfaces, tabs, canvas, columns, board cards, all three graphs (latched popup over the engine pane), and the boards-index / trash / projects / queue / changes rows (menu-cluster bugs resolved 2026-06-12) · sole deliberate carve-out: the cluster review preview's unresolvable leaves get no dead menu, same gating as its find popup

Every item kind right-clicks to a `Menu<A>` composed from its kind's base
builder plus host-contextual entries ([[spec:ctxmenu-contextual-extend]]). Two hard
rules: right-click never performs a direct action (a menu with one entry is
still a menu), and a *primary* action is never menu-only — the menu is the
complete verb list, not the only door.

## Hover: one signal, then preview [hover-open-signal] [hover-preview-universal]
status:: done
note:: signal divergence fixed 2026-06-12 (`bug-open-signal-inconsistent`) — the wash is one policy fn (`hiker_theme::open_signal_wash`) + PointingHand everywhere: board cards dropped the bespoke accent overlay (the red × keeps its distinct destructive-hover wash), trash rows gained wash+pointer, files/git-diff rows route through the helper, search cards wash on hover, button rows inherit the same `hover_bg` from the theme and gained the pointer · preview covers files-tree note rows, board note-cards, trail waypoints alongside the original list surfaces (`bug-files-tree-no-hover-preview` fixed 2026-06-11)

- **Signal** [hover-open-signal]: "click acts here" is shown by the standard
  hover wash (row/card background) plus `CursorIcon::PointingHand`. Not a
  hand-rolled accent wash, not a tooltip standing in for a wash, not cursor-only.
- **Preview** [hover-preview-universal]: any element referencing a note shows
  the shared preview card after hover dwell (`register_note_hover`); canvases
  show the spatial thumbnail. Applies to list rows, board cards, waypoints,
  graph nodes (where the panel's preview toggle is on), wikilink pills.

## Drag carries the note [drag-note-payload]
status:: done
note:: `bug-note-rows-not-drag-sources` fixed 2026-06-12 — arming + ghost chip extracted to `widgets::note_row::note_drag_source` and wired on search cards/chunk rows, backlinks, related, vault-view rows in all lenses (smart-folder member rows included), and git-diff rows; payload stays the bare vault-relative `String`, so every existing drop target accepts the new sources unchanged

The drag payload for a note item is its vault-relative path (`String`) —
already the convention. Any note row is a drag source; containers (folders,
board lanes, canvases, trails when editable) are the drop targets. An item
kind's capability does not vary by which list happens to render it.

## Destructive verbs live in the menu [destructive-verbs-in-menu]
status:: done
note:: boards-index bare Delete button fixed 2026-06-12 (`bug-boards-index-bare-delete` — menu entry → shared confirm modal) · the last divergence, the trash row's persistent Purge button, fixed the same day (`bug-trash-purge-no-confirm`): Purge is menu-only behind the house confirm (`ConfirmIntent::PurgeTrashItem`), the bare button is gone, the non-destructive Restore button stays

Distinguish two severities:

- **Remove a reference** (card off a board, waypoint off a trail): may be a
  small hover-revealed control on the item (the card ×), and also appears in
  the menu. No confirm needed — the note is untouched.
- **Destroy or trash data** (delete note, delete board/trail): menu-only, with
  a confirm step. Never a bare persistent button on a row — a misclick on a
  list must not be able to destroy anything.

## Inline-edit lifecycle [inline-edit-lifecycle]
status:: done
note:: files rename (`rename_text_edit`), board card/title/column edits all commit on focus loss; Esc is the only cancel

- **Triggers**: double-click (canonical), the menu's Rename/Edit verb, Enter/F2
  on a sole-selected editable item.
- **Commit**: Enter, or focus loss. Focus-loss *commits* everywhere (matches
  platform convention: Finder, VS Code); a half-typed commit is visible and
  undoable, a silently discarded edit is not.
- **Cancel**: Esc, exactly and only.

## Keyboard: the Esc ladder and parity [keyboard-esc-ladder]
status:: partial
note:: registry + palette core is strong; Ctrl+F find now on all three graphs · middle rung wired 2026-06-12 (`bug-esc-no-back-in-focus-nav`): code-graph Esc pops hops focus to overview (find popup / node menu consume Esc first), ZIM viewer gained a per-pane history with toolbar back/forward + Esc-as-back · the vault graph gained the same middle rung with its hops scope ([[spec:graph-nav-extract]] — one shared gate, `graph_nav::esc_pops_focus`, drives both graphs) · stays partial for the deferred sibling-parity gap: list arrow-nav beyond search

- **Esc resolves top-down**: cancel the active inline edit → step up one level
  (pop graph focus to overview, leave a drill) → close the popup/panel. A
  surface with a level structure must wire the middle rung.
- **Parity across siblings**: a capability one sibling surface has (Ctrl+F find
  in graphs, arrow-nav in lists), its siblings have. List arrow-nav beyond
  search is deferred but the rule names the direction.
- Global chords stay in the action registry so the palette lists them with
  hints (`keybinds.rs` / `command_palette`).

## New-item placement [new-item-placement]
status:: done
note:: every surface's primary creation is a persistent header affordance — Files/boards/projects "+ New …", and the trails header now carries a "+" beside its ⋯ menu

Creating the surface's primary object is a persistent header "+" affordance
("+ New board", "+ Add card", "+ New project"). Secondary creations live in
the overflow/context menu. A surface's *primary* creation verb is never
menu-only.

## Out of scope

- Visual styling of washes, menus, badges — `style.md`.
- The menu system's mechanics — `context-menu.md`.
- Tab preview/sticky semantics — the workbench/tabs spec (this doc only binds
  gestures to them).
