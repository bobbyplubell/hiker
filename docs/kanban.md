# Kanban

A board view over a vault — columns of cards, where each card references a note and a "move" rewrites which column it sits in. A board is its own curated document, not a derived projection of note frontmatter: the board-doc owns the layout, the referenced notes stay untouched. A first-class core feature, **not** a plugin — it writes and rides the op-log + structured index directly; `plugins.md` ("What this does not try to be") records why a write-and-drag view belongs in core.

The headline decisions:

- **A board is a regular markdown note** at `vault/boards/<name>.md` with `hiker.kind: board` + a ULID `id`. Frontmatter holds an ordered list of arbitrary, user-named columns, each an ordered list of card references; the body is freeform prose the user authors (board description, framing). Searchable, linkable, syncable like any other note. [board-doc-shape, board-column-model]
- **Each card is a `{ id, path }` double-link to a real note** — the same stable-reference scheme trails use. Board operations never mutate the referenced note; the card's column and position live only in the board-doc. [board-card-references]
- **Moving or reordering a card edits the board-doc frontmatter** — a reference hops between (or within) column arrays — committed through the op-log user-save path. Within-column order is array position. Concurrent same-card moves from two devices are an ordinary same-region frontmatter edit, resolved by the existing conflict-hunk machinery (`op-log-merge-conflict`). [board-move]
- **A note can be a card on many boards**, in a different column on each — the motivation for board-owned layout over a single per-note field. [board-many-to-many]
- **The same board-doc opens as a board or as raw markdown, toggled in place** — a "View as: Board / Markdown" control, mirroring the cluster editor's view menu. The board view renders columns; the markdown view is the standard editor over the note's frontmatter + body. [board-view, board-view-toggle]
- **Card references self-heal** across note renames/moves and surface deleted notes as broken cards, reusing trails' reference-resolution + auto-update-on-move plus a derived index. [board-card-references, board-cards-derived-table]


## Board-doc shape

A board-doc is a regular markdown note at a user-chosen vault location (default `boards/`, configurable per `board-default-location`). Frontmatter:

```yaml
---
hiker:
  kind: board
  id: <ulid>
  columns:                              # ordered; render left-to-right
    - name: Todo
      cards:                            # ordered; render top-to-bottom
        - { id: <note ulid>, path: "research/raptor-paper.md" }
        - { id: <note ulid>, path: "inbox/follow-up.md" }
    - name: Doing
      cards:
        - { id: <note ulid>, path: "work/migration.md" }
    - name: Done
      cards: []                         # empty columns render
---
# Q3 Roadmap

Freeform prose. Why this board exists, what the columns mean, who it's for.
```

`hiker.columns` is the single source of truth for column order, column membership, and within-column card order. An empty `cards:` list renders as an empty column — columns are explicit, never inferred from the cards present. [board-column-model]

**Columns are arbitrary.** The count and names are entirely user-defined per board — `Todo / Doing / Done` is only the default seed a new board ships with (`board-create`). A board can have any number of columns with any names (`Backlog`, `Blocked`, `In review`, `Q3`, …); there is no enum or fixed vocabulary. Because columns are an ordered frontmatter list rather than the distinct values of a shared note field, two boards can have completely different column sets, and renaming a column never touches any note. [board-column-model]

**Managing columns.** Add, rename, reorder, and delete columns from the board view (a column-header menu) or by hand-editing frontmatter in the markdown view — both are board-doc frontmatter edits on the same op-log path as a card move. Deleting a column that still holds cards prompts first (the cards' references would be dropped from the board; the notes are untouched). [board-column-management]

The body is freeform markdown, hand-authored and never overwritten by hiker — the board view renders the columns from frontmatter, the body is the board's prose framing. Body edits are ordinary user text edits; card moves are localized frontmatter edits. Both ride the op-log granular-save path (`op-log-ops-producer-helpers`), so editing the description and moving a card never churn each other. [board-doc-shape]

The board-doc must have a `.md` extension to be recognized as a board. A note carrying `hiker.kind: board` but a non-`.md` extension is treated as a regular note — the discriminator alone isn't enough (same rule trails use).


## Card references

Every card is a **double-link** `{ id: <ulid>, path: <rel-path> }` to the note it represents. ULID is the canonical pointer that survives renames; rel-path keeps the board-doc legible when opened in any other markdown editor. Resolution and self-healing reuse the trails machinery wholesale (`trail-reference-resolution`): ULID wins on path drift (path rewritten in place, one op appended), a path-with-changed-identity surfaces a confirm modal, and a card whose note is gone renders as a broken card the user resolves. [board-card-references]

Because a card makes its note a *referent*, adding a card triggers ULID stamping under the vault's `note-id-stamping` policy (lazy mode stamps `hiker.id` onto the note the first time it's referenced) — boards join trails and future wikilinks as a stamping trigger.

**Auto-update on note move.** When a referenced note moves (`move-note-core-cmd`, `drag-and-drop-move`, or a watcher-detected external rename), board-docs referencing it get their stored `path` rewritten to match — same indexer-side hook trails use (`trail-auto-update-on-note-move`), driven off the derived index below. The ULID is unchanged; the move is path-only.

**Broken cards.** A card whose double-link resolves to nothing (note deleted, both halves stale) renders greyed with a "broken reference" pill and stays in its column so the user decides whether to remove it or repoint it. Boards have no frontmatter-model "free" referential integrity to fall back on — the card holds the only pointer — so the broken-card surface is the safety net. [board-card-references]


## Moving cards

A move is a board-doc frontmatter edit: the card's `{ id, path }` entry is removed from its current column's `cards` array and inserted at the target position in the destination column's array. Reordering within a column is the same edit with source and destination column equal. The write goes through `op_writes::user_save` (`op-log-ops-producer-helpers`) — a normal versioned, undoable, syncable user edit. The referenced note is never read or written. [board-move]

Granular saves (`op-log` multi-span delta) localize the change to the touched frontmatter region, so a move produces a small op rather than a whole-document rewrite — which is what makes concurrent moves mergeable. Two devices moving different cards merge automatically; two devices moving the *same* card touch the same frontmatter region and surface as a conflict hunk with Keep mine / Keep theirs / Keep both (`op-log-merge-conflict`). No board-specific conflict mechanism is needed.


## Many boards, shared cards

Boards are independent documents; nothing constrains a note to one board. The same note can be a card on a roadmap board (column "Doing") and a personal board (column "Today") simultaneously — each board-doc holds its own reference and its own column placement. A note's membership across boards is a reverse lookup over the derived index (`boards_containing_note`), the symmetric query to trails' `trails_containing_note`. [board-many-to-many]


## Board view and the markdown toggle

A board-doc opens in a `board`-kind tab (per `tab-kinds`) that renders columns side by side; each card shows the referenced note's title and opens that note in the editor pane on click. The board tab is per-doc — opening two board-docs gives two board tabs (there is no singleton board tab). [board-view]

The pane carries a **"View as: Board / Markdown"** control in a view-options menu, mirroring the cluster editor's `cluster-editor-graph-view-view-menu` ("View as: Tree / Graph / Markdown"):

- **Board** (default) — the column render described above.
- **Markdown** — the standard editor widget over the same note, so the user hand-edits frontmatter (columns, card order) and body prose directly. Switching back to Board re-renders from the now-current frontmatter.

The toggle is a render choice over one underlying op-log document, not two tabs — the same shape the cluster editor uses to flip a tree between its structured view and its raw markdown. [board-view-toggle]

Board-docs render in the file tree at their natural location with a board glyph; clicking opens the board view by default. [board-view]


## Creating a board

A new board comes from a sidebar `+` action (and the cross-type new-item picker, per `sidebar-new-item-button`), going through a `core` create op that writes a board-doc with `hiker.kind: board`, a fresh ULID, and a default column set (`Todo` / `Doing` / `Done`, editable afterward). The new board-doc opens in the board view with inline-rename active so the user names it before submitting. [board-create]

**Default placement.** New boards land at `<vault>/<new_board_dir>/<name>.md`, `new_board_dir` configurable via `[boards] new_board_dir = "boards/"` (default `"boards/"`, vault-scope eligible per `settings-write-back`, auto-created on first board, empty string = vault root). Boards can be moved anywhere later via filetree drag-and-drop — the board carries its identity in frontmatter. [board-default-location]


## Adding and removing cards

**Add a card.** A note becomes a card via a right-click "Add to board…" verb on indexable note rows in the file tree and an editor-pane affordance when a regular note is open — both pick a target board and column, append the note's `{ id, path }` to that column's `cards` array, and commit through the user-save path. Idempotent per board: a note already a card anywhere on the board disables the verb ("Already on this board"); the same note can still be added to a *different* board. [board-add-card]

**Remove a card.** A per-card "Remove from board" verb drops the card's entry from the board-doc frontmatter. The referenced note is untouched — removal is a board-membership edit, not a note deletion. [board-remove-card]


## Indexer integration

A derived `board_cards` table in `index.db` supports the reverse lookups and the auto-update-on-move path: which boards contain a given note, which cards belong to a given board, and where each card sits. Re-derived from each board-doc's frontmatter on ingest, fail-loud on schema bump, exactly like `trail-waypoints-derived-table`. Board-docs are visible notes under `vault/boards/`, so they ride the standard md ingest path with no watcher carve-out (unlike trails' hidden waypoint dir). [board-cards-derived-table]


## Deferred

- **Drag-and-drop between columns.** v1 moves a card via a per-card "Move to >" menu; DnD rides the uniform vault-path drag payload sketched in `design.md` (`trails-dnd-ingestion`) once it lands, and a card dragged from the file tree onto a column is the same gesture as the "Add to board" verb. [board-dnd]
- **Per-column WIP limits.** Column-level constraints (cap a column at N cards, flag overflow). Within-column ordering is already covered by the array model; only the limit is future. [board-wip-limits]
- **MCP board tools.** `boards_list` / `board_get` / `board_add_card` so attached agents can read boards as context and curate them, gated by `agent-write-review-mode` like the trail tools. Lands once boards are solid in real use. [board-mcp-tools]


## Out of scope

- **Grouping notes by a frontmatter field.** Boards are curated reference lists, not a saved query over a `status:` field — a note's board membership and column are board-owned, not derived from the note's own metadata. A query-defined "smart board" is a different feature and not this doc's concern.
- **Cross-vault boards.** Boards are vault-scoped; ULIDs and rel-paths aren't unique across vaults. Same boundary as trails.
