# Kanban

A board view over a vault — columns of cards, where each card references a note and a "move" rewrites which column it sits in. A board is its own curated document, not a derived projection of note frontmatter: the board-doc owns the layout, the referenced notes stay untouched. A first-class core feature — it writes and rides the op-log + structured index directly.

## Board-doc shape

A board-doc is a regular markdown note at a user-chosen vault location (default `boards/`, configurable per `board-default-location`). Frontmatter:

```yaml
---
hiker:
  kind: board
  columns:                              # ordered; render left-to-right
    - name: Todo
      cards:                            # ordered; render top-to-bottom
        - { path: "research/raptor-paper.md" }
        - { path: "inbox/follow-up.md" }
    - name: Doing
      cards:
        - { path: "work/migration.md" }
    - name: Done
      cards: []                         # empty columns render
---
# Q3 Roadmap

Freeform prose. Why this board exists, what the columns mean, who it's for.
```

**Columns are arbitrary and explicit.** `hiker.columns` is the single source of truth for column order, membership, and within-column card order; an empty `cards:` list renders as an empty column (columns are never inferred from the cards present). Count and names are entirely user-defined per board — `Todo / Doing / Done` is only the default seed (`board-create`) — with no enum or fixed vocabulary. Because columns are an ordered frontmatter list, not the distinct values of a shared note field, two boards can have completely different column sets and renaming a column never touches any note. [board-column-model]

**Managing columns.** Add, rename, reorder, and delete columns from the board view (a column-header menu) or by hand-editing frontmatter in the markdown view — both are board-doc frontmatter edits on the same op-log path as a card move. Deleting a column that still holds cards prompts first (the cards' references would be dropped from the board; the notes are untouched). [board-column-management]

The body is freeform markdown, hand-authored and never overwritten — the board view renders columns from frontmatter, the body is prose framing. Body edits and card moves both ride the op-log granular-save path (`op-log-ops-producer-helpers`), so editing the description and moving a card never churn each other. [board-doc-shape]

The board-doc must have a `.md` extension to be recognized as a board. A note carrying `hiker.kind: board` but a non-`.md` extension is treated as a regular note — the discriminator alone isn't enough (same rule trails use).


## Card references

Every note card is `{ path: <vault-relative-path> }`. The path is the identity — no separate ID half. [board-card-references]

Resolution is a single path lookup against the indexer. A card whose path doesn't resolve (note deleted, path stale) renders greyed with a "broken reference" pill and stays in its column so the user decides whether to remove or repoint it.

**Auto-update on note move.** When a referenced note moves (`move-note-core-cmd`, `drag-and-drop-move`, or a watcher-detected external rename), the shared `wikilink-rename-rewrite` pass updates every affected `cards[].path` in board-docs in the same transaction as the move. The derived index below makes the affected-boards lookup cheap.


## Moving cards

A move is a board-doc frontmatter edit: the card entry is removed from its current column's `cards` array and inserted at the target position in the destination column's array. Reordering within a column is the same edit with source and destination column equal. The write goes through `op_writes::user_save` (`op-log-ops-producer-helpers`) — a normal versioned, undoable, syncable user edit. The referenced note is never read or written. [board-move]

Granular saves (`op-log` multi-span delta) localize the change to the touched frontmatter region, so a move produces a small op rather than a whole-document rewrite — which is what makes concurrent moves mergeable. Two devices moving different cards merge automatically; two devices moving the *same* card touch the same frontmatter region and surface as a conflict hunk with Keep mine / Keep theirs / Keep both (`op-log-merge-conflict`). No board-specific conflict mechanism is needed.


## Many boards, shared cards

Boards are independent documents; nothing constrains a note to one board. The same note can be a card on a roadmap board (column "Doing") and a personal board (column "Today") simultaneously — each board-doc holds its own reference and its own column placement. A note's membership across boards is a reverse lookup over the derived index (`boards_containing_note`), the symmetric query to trails' `trails_containing_note`. [board-many-to-many]


## Board view and the markdown toggle

A board-doc opens in a `board`-kind tab (per `tab-kinds`) that renders columns side by side; each card shows the referenced note's title and opens that note in the editor pane on click. The board tab is per-doc — opening two board-docs gives two board tabs (there is no singleton board tab). [board-view]

The pane carries a **"View as: Board / Markdown"** control (mirroring the cluster editor's view menu): **Board** (default) is the column render above; **Markdown** is the standard editor over the same note for hand-editing frontmatter + body, re-rendering on switch-back. It's a render choice over one op-log document, not two tabs. [board-view-toggle]

Board-docs render in the file tree at their natural location with a board glyph; clicking opens the board view by default. [board-view]


## Creating a board

A new board comes from a sidebar `+` action (and the cross-type new-item picker, `sidebar-new-item-button`) via a `core` create op that writes a board-doc with `hiker.kind: board` and a default column set (`Todo` / `Doing` / `Done`, editable). It opens in the board view with inline-rename active. [board-create]

**Default placement.** New boards land at `<vault>/<new_board_dir>/<name>.md`, `new_board_dir` configurable via `[boards] new_board_dir = "boards/"` (default `"boards/"`, vault-scope eligible per `settings-write-back`, auto-created on first board, empty string = vault root). Boards can be moved anywhere later via filetree drag-and-drop — the board carries its identity in frontmatter. [board-default-location]


## Boards index page

A singleton **Boards** page — a `boards-index`-kind app-page tab (per `tab-kinds`) reached from the toolbar actions menu — is the meta-surface for boards, since boards are per-doc and have no single home tab. It lists every board in the vault, each row showing title + column count + card count from `core::boards::list` (the same enumeration `boards_list` exposes to MCP); empty boards appear too, because the page enumerates board-docs, not just boards-with-cards. Clicking a row opens that board in its board view (`board-view`). The page carries a **New board** action (the `board-create` op) and a per-row **Delete** action (`board-delete`). [board-index-page]


## Deleting a board

Deleting a board moves its board-doc to `.hiker/trash/` via `core::ops::delete`, like any note — restorable, with the derived `board_cards` rows clearing on the delete-ingest. Referenced notes are never touched; only the board (its columns + card refs) goes away. Surfaced as the per-row Delete on the Boards index page, and — since a board-doc is an ordinary note — via the file tree's standard delete. A confirm step guards it (the layout is discarded, though trash makes it recoverable). [board-delete]


## Adding and removing cards

**Add a card.** A note becomes a card via a right-click "Add to board…" verb on indexable note rows in the file tree — it picks a target board and column, appends `{ path: <note path> }` to that column's `cards` array, and commits through the user-save path. Idempotent per board: a note already a card anywhere on the board disables the verb ("Already on this board"); the same note can still be added to a *different* board. [board-add-card]

**Remove a card.** A per-card "Remove from board" verb drops the card's entry from the board-doc frontmatter. The referenced note is untouched — removal is a board-membership edit, not a note deletion. [board-remove-card]

**Freeform cards.** A card need not reference a note. A **freeform card** carries its own text and a card-local id, with no note ref — for quick items, checklist entries, or placeholders not worth their own note. In `hiker.columns[].cards` it serializes as `{ card_id: <ulid>, text: "..." }`; presence of `text` (or `card_id` without `path`) discriminates from a note card's `{ path }`. The `card_id` is internal — it disambiguates two freeform cards with identical text for move/reorder/delete verbs and never surfaces to the user. Freeform cards have no resolution, no note link, and are **skipped by the derived `board_cards` index** — they aren't notes, so the reverse "boards containing note" lookup and rename-rewrite don't apply to them. Each column carries a **+ Add card** affordance that creates one inline (edit the text in place); editing rewrites the card's `text` through the board-doc user-save path like any other frontmatter edit. [board-freeform-card]


## Indexer integration

A derived `board_cards` table in `index.db` supports the reverse lookups and the auto-update-on-move path: which boards contain a given note, which cards belong to a given board, and where each card sits. Re-derived from each board-doc's frontmatter on ingest, fail-loud on schema bump, exactly like `trail-waypoints-derived-table`. Board-docs are visible notes under `vault/boards/`, so they ride the standard md ingest path with no watcher carve-out (unlike trails' hidden waypoint dir). [board-cards-derived-table]


## Deferred

- **Drag-and-drop between columns.** v1 moves a card via a per-card "Move to >" menu; DnD rides the uniform vault-path drag payload sketched in `design.md` (`trails-dnd-ingestion`) once it lands, and a card dragged from the file tree onto a column is the same gesture as the "Add to board" verb. [board-dnd]
- **Per-column WIP limits.** Column-level constraints (cap a column at N cards, flag overflow). Within-column ordering is already covered by the array model; only the limit is future. [board-wip-limits]
- **MCP board tools.** A read + curate surface so attached agents can read boards as context and reorganize them (read + write verbs gated by `agent-write-review-mode`); full surface in `mcp.md` §"Board tools". The active board is injected into the agent's turn context when a board tab is focused (`chat-active-note-context-injection`). [board-mcp-tools]


## Out of scope

- **Grouping notes by a frontmatter field.** Boards are curated reference lists, not a saved query over a `status:` field — a note's board membership and column are board-owned, not derived from the note's own metadata. A query-defined "smart board" is a different feature and not this doc's concern.
- **Cross-vault boards.** Boards are vault-scoped; paths aren't unique across vaults. Same boundary as trails.
