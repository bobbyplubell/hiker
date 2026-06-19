# Kanban

A board view over a vault — columns of cards, where each card references a note and a "move" rewrites which column it sits in. A board is its own curated document, not a derived projection of note frontmatter: the board-doc owns the layout, the referenced notes stay untouched. A first-class core feature — it writes and rides the op-log + structured index directly.

## Board-doc shape

A board-doc is a regular markdown note at a user-chosen vault location (default `boards/`, configurable per [[spec:board-default-location]]). Frontmatter:

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

**Columns are arbitrary and explicit.** `hiker.columns` is the single source of truth for column order, membership, and within-column card order; an empty `cards:` list renders as an empty column (columns are never inferred from the cards present). Count and names are entirely user-defined per board — `Todo / Doing / Done` is only the default seed ([[spec:board-create]]) — with no enum or fixed vocabulary. Because columns are an ordered frontmatter list, not the distinct values of a shared note field, two boards can have completely different column sets and renaming a column never touches any note. [board-column-model]
status:: done
implements:: [[code:hiker/boards/Column]], [[code:hiker/boards/Board]]
touches:: [[code:hiker/boards]]
note:: `hiker.columns` ordered list of arbitrary-named columns; empty columns round-trip; frontmatter is sole source of order · evidence: `core/src/boards/mod.rs` (`Column`, `parse_column`/`column_to_json`)

**Managing columns.** Add, rename, reorder, and delete columns from the board view (a column-header menu) or by hand-editing frontmatter in the markdown view — both are board-doc frontmatter edits on the same op-log path as a card move. Deleting a column that still holds cards prompts first (the cards' references would be dropped from the board; the notes are untouched). [board-column-management]
status:: done
implements:: [[code:hiker/boards/ops/add_column]], [[code:hiker/boards/ops/rename_column]], [[code:hiker/boards/ops/reorder_column]], [[code:hiker/boards/ops/delete_column]], [[code:hiker/panels/board/render_header]]
note:: core allows delete-with-cards; UI prompts via inline confirm row · evidence: `core/src/boards/ops.rs` (`add_column`/`rename_column`/`reorder_column`/`delete_column`); `app/src/panels/board.rs` (column-header `⋯` menu + inline-rename + delete-with-cards confirm)

The body is freeform markdown, hand-authored and never overwritten — the board view renders columns from frontmatter, the body is prose framing. Body edits and card moves both ride the op-log granular-save path ([[spec:op-log-ops-producer-helpers]]), so editing the description and moving a card never churn each other. [board-doc-shape]
status:: done
implements:: [[code:hiker/boards/Board]], [[code:hiker/boards/parse_board]], [[code:hiker/boards/write_board_frontmatter]]
verifies:: [[code:hiker/boards/tests/write_preserves_unknown_hiker_siblings_and_top_level]]
touches:: [[code:hiker/boards]]
note:: visible md note; `.md`-required parse gate; preserves unknown `hiker.*` siblings + top-level keys (round-trip test) · evidence: `core/src/boards/mod.rs` (`Board`/`Column`, `parse_board`/`parse_board_for`/`write_board_frontmatter`)

The board-doc must have a `.md` extension to be recognized as a board. A note carrying `hiker.kind: board` but a non-`.md` extension is treated as a regular note — the discriminator alone isn't enough (same rule trails use).

The parse gate also accepts registered board-like kinds (`hiker.kind: sprint`) alongside `board`, per [[spec:sprint-board-subtype]] — the whole surface in this doc applies to those boards unchanged; what the kind *adds* (derived status, close/rollover) is `pm.md`'s, not this doc's.


## Card references

Every note card is `{ path: <vault-relative-path> }`. The path is the identity — no separate ID half. [board-card-references]
status:: done
implements:: [[code:hiker/boards/BoardCard]], [[code:hiker/boards/parse_card]], [[code:hiker/boards/column_to_json]], [[code:hiker/boards/ResolvedCard]], [[code:hiker/boards/get_board]], [[code:hiker/boards/on_note_moved]], [[code:hiker/boards/ops/run_note_moved]], [[code:hiker/links_rename/on_note_moved]], [[code:hiker/panels/board/add_card]]
note:: note cards carry only `path`; `Orphan` is the only non-Resolved outcome; move-rewrite rides the shared `on_note_moved` pass · evidence: `core/src/boards/mod.rs` (`BoardCard::Note { path }`, `parse_card`, `resolve_card`); `core/src/boards/ops.rs` (`card_matches` polymorphic handle for note path vs freeform card_id; `add_card` / `remove_card` / `move_card` rewritten; `repoint_card` removed); `app/src/panels/board.rs` (`render_card_face_body` / `card_open_action` only `Resolved | Orphan`; `OpenPathConflict` action removed)

Resolution is a single path lookup against the indexer. A card whose path doesn't resolve (note deleted, path stale) renders greyed with a "broken reference" pill and stays in its column so the user decides whether to remove or repoint it.

**Auto-update on note move.** When a referenced note moves ([[spec:move-note-core-cmd]], [[spec:drag-and-drop-move]], or a watcher-detected external rename), the shared [[spec:wikilink-rename-rewrite]] pass updates every affected `cards[].path` in board-docs in the same transaction as the move. The derived index below makes the affected-boards lookup cheap.


## Moving cards

A move is a board-doc frontmatter edit: the card entry is removed from its current column's `cards` array and inserted at the target position in the destination column's array. Reordering within a column is the same edit with source and destination column equal. The write goes through `op_writes::user_save` ([[spec:op-log-ops-producer-helpers]]) — a normal versioned, undoable, syncable user edit. The referenced note is never read or written. [board-move]
status:: done
implements:: [[code:hiker/boards/ops/persist_board]], [[code:hiker/boards/ops/move_card]]
note:: array splice in frontmatter; within-column reorder + cross-column tested; concurrent merges ride the op-log path · evidence: `core/src/boards/ops.rs` (`move_card` / `MoveCardRequest`) via `op_writes::user_save`

Granular saves ([[spec:op-log]] multi-span delta) localize the change to the touched frontmatter region, so a move produces a small op rather than a whole-document rewrite — which is what makes concurrent moves mergeable. Two devices moving different cards merge automatically; two devices moving the *same* card touch the same frontmatter region and surface as a conflict hunk with Keep mine / Keep theirs / Keep both ([[spec:op-log-merge-conflict]]). No board-specific conflict mechanism is needed.


## Many boards, shared cards

Boards are independent documents; nothing constrains a note to one board. The same note can be a card on a roadmap board (column "Doing") and a personal board (column "Today") simultaneously — each board-doc holds its own reference and its own column placement. A note's membership across boards is a reverse lookup over the derived index (`boards_containing_note`), the symmetric query to trails' `trails_containing_note`. [board-many-to-many]
status:: done
implements:: [[code:hiker/boards/list]], [[code:hiker/boards/containing_note_with_paths]], [[code:hiker/store/boards/impl#[Store]boards_containing_note]]
note:: reverse lookup symmetric to `trails_containing_note`; one hit per (board, column) · evidence: `core/src/store/boards.rs` (`boards_containing_note`); `core/src/boards/mod.rs` (`containing_note_with_paths`)


## Board view and the markdown toggle

A board-doc opens in a `board`-kind tab (per [[spec:tab-kinds]]) that renders columns side by side; each card shows the referenced note's title and opens that note in the editor pane on click. The board tab is per-doc — opening two board-docs gives two board tabs (there is no singleton board tab). [board-view]
status:: done
implements:: [[code:hiker/boards/get_board]], [[code:hiker/panels/board/open]], [[code:hiker/tab/TabKind#Board#path]], [[code:hiker/tab/impl#[Tab]persist_key]], [[code:hiker/bootstrap/impl#[AppState]restore_tab_state]]
touches:: [[code:hiker/panels/board]]
note:: per-doc non-buffer tab like cluster tabs; card click opens the referenced note; "Open as board" verb + default-click routing · evidence: `app/src/tab.rs` (`TabKind::Board { path }`), `app/src/panels/board.rs` (`show`/`open`), `app/src/sidebar/files.rs` (`is_board_doc` click-routes to board view)

The pane carries a **"View as: Board / Markdown"** control (mirroring the cluster editor's view menu): **Board** (default) is the column render above; **Markdown** is the standard editor over the same note for hand-editing frontmatter + body, re-rendering on switch-back. It's a render choice over one op-log document, not two tabs. [board-view-toggle]
status:: done
implements:: [[code:hiker/panels/board/Pane#view]], [[code:hiker/panels/board/render_header]]
touches:: [[code:hiker/editor_pane]]
note:: Markdown now renders the board-doc's live editor INLINE in the board pane (same op-log document, not a separate buffer tab); `board::show` takes `rt` so it can host the editor widget · evidence: `app/src/panels/board.rs` (`BoardView` enum + `show` branch hosting `panels::buffer::show` inline; `render_header` View-as toggle flips `Pane.view`); `app/src/editor_pane.rs` (`ensure_vault_buffer_loaded` now `pub(crate)`)

Board-docs render in the file tree at their natural location with a board glyph; clicking opens the board view by default. [board-view]

The board title's right-click menu carries **"Open board in graph"** — the container variant of the universal note-item graph entry, focusing the vault Graph tab on the board-doc node with its cards at depth 1 ([[spec:open-in-graph-containers]], specced in `context-menu.md`).


## Creating a board

A new board comes from a sidebar `+` action (and the cross-type new-item picker, [[spec:sidebar-new-item-button]]) via a `core` create op that writes a board-doc with `hiker.kind: board` and a default column set (`Todo` / `Doing` / `Done`, editable). It opens in the board view with inline-rename active. [board-create]
status:: done
implements:: [[code:hiker/boards/ops/create_board]], [[code:hiker/panels/board/Pane#renaming_title]], [[code:hiker/panels/board/open_for_rename]], [[code:hiker/panels/board/render_header]], [[code:hiker/panels/board/render_title]], [[code:hiker/panels/board/rename_board]], [[code:hiker/sidebar/impl#[AppState]new_board]]
verifies:: [[code:hiker/boards/tests/plan_new_board_picks_free_path_without_writing]]
touches:: [[code:hiker/workbench_host]]
note:: default Todo/Doing/Done columns, `new_board_dir` placement, suffix-on-collision; new board opens in the board view with title inline-rename active (mirrors new-trail/new-file); commit moves the board-doc via `core::vault::move_note` and repoints the tab · evidence: `core/src/boards/ops.rs` (`create_board`); `app/src/sidebar/mod.rs` (`new_board`); `app/src/panels/board.rs` (`open_for_rename` + `render_title` inline rename + `rename_board`); `app/src/workbench_host.rs` (`+` right-click cross-type picker)

**Default placement.** New boards land at `<vault>/<new_board_dir>/<name>.md`, `new_board_dir` configurable via `[boards] new_board_dir = "boards/"` (default `"boards/"`, vault-scope eligible per [[spec:settings-write-back]], auto-created on first board, empty string = vault root). Boards can be moved anywhere later via filetree drag-and-drop — the board carries its identity in frontmatter. [board-default-location]
status:: done
implements:: [[code:hiker/boards/ops/create_board]], [[code:hiker/config/patch/ELIGIBLE_VAULT]], [[code:hiker/config/sections/BoardsConfig]]
note:: `[boards] new_board_dir = "boards/"`, vault-scope eligible, auto-created on first board · evidence: `core/src/config/sections.rs` (`BoardsConfig`), `core/src/config/mod.rs`, `core/src/config/patch.rs` (`boards.new_board_dir` eligible key)


## Boards index page

A singleton **Boards** page — a `boards-index`-kind app-page tab (per [[spec:tab-kinds]]) reached from the toolbar actions menu — is the meta-surface for boards, since boards are per-doc and have no single home tab. It lists every board in the vault, each row showing title + column count + card count from `core::boards::list` (the same enumeration `boards_list` exposes to MCP); empty boards appear too, because the page enumerates board-docs, not just boards-with-cards. Clicking a row opens that board in its board view ([[spec:board-view]]). The page carries a **New board** action (the [[spec:board-create]] op) and a per-row **Delete** action ([[spec:board-delete]]). [board-index-page]
status:: done
implements:: [[code:hiker/toolbar/impl#[AppState]render_actions_menu]], [[code:hiker/tab/TabKind#BoardsIndex]], [[code:hiker/bootstrap/impl#[AppState]restore_tab_state]]
touches:: [[code:hiker/panels/boards_index]], [[code:hiker/workbench_host]]
note:: non-buffer app-page (no editor chrome, like Home/Queue); lists every board via `core::boards::list` (incl. empty) with title + column/card counts; row click → `panels::board::open`; New board → `AppState::new_board`; per-row Delete → [[spec:board-delete]] confirm modal · evidence: `app/src/tab.rs` (`TabKind::BoardsIndex` singleton + label/icon/persist_key `:boards_index`), `app/src/panels/boards_index.rs` (`show`), `app/src/toolbar.rs` (actions-menu `+ Boards` → `open_singleton_tab(TabKind::BoardsIndex)`), dispatch in `app/src/tabs.rs` + `app/src/workbench_host.rs`, `app/src/bootstrap.rs` (`:boards_index` restore)


## Deleting a board

Deleting a board moves its board-doc to `.hiker/trash/` via `core::ops::delete`, like any note — restorable, with the derived `board_cards` rows clearing on the delete-ingest. Referenced notes are never touched; only the board (its columns + card refs) goes away. Surfaced as the per-row Delete on the Boards index page, and — since a board-doc is an ordinary note — via the file tree's standard delete. A confirm step guards it (the layout is discarded, though trash makes it recoverable). [board-delete]
status:: done
implements:: [[code:hiker/indexer/jobs/handle_simple_job]], [[code:hiker/panels/boards_index/show]], [[code:hiker/widgets/modal/impl#[AppState]apply_confirm_delete]]
touches:: [[code:hiker/boards/tests]]
note:: delete a board = move the board-doc to `.hiker/trash/` via `core::ops::delete` (restorable; derived `board_cards` clear on delete-ingest; referenced notes untouched); confirm-guarded; the file-tree delete now shares this `ops::delete` path; open Board tab for the deleted path closes · evidence: `app/src/widgets/modal.rs` (`apply_confirm_delete` now routes through `hiker_core::ops::file::delete`; closes `Board { path }` tabs too), `app/src/panels/boards_index.rs` (per-row Delete sets `Modal::ConfirmDelete`); board_cards clear wired into the trash-routed delete in `core/src/indexer/jobs.rs` (`IndexJob::DeleteNote` → `clear_board_cards_for_delete`); core test `core/src/boards/tests.rs::delete_board_doc_trashes_and_clears_board_cards`


## Adding and removing cards

**Add a card.** A note becomes a card via a right-click "Add to board…" verb on indexable note rows in the file tree — it picks a target board and column, appends `{ path: <note path> }` to that column's `cards` array, and commits through the user-save path. Idempotent per board: a note already a card anywhere on the board disables the verb ("Already on this board"); the same note can still be added to a *different* board. [board-add-card]
status:: done
implements:: [[code:hiker/boards/impl#[Board]contains_note]], [[code:hiker/boards/ContainingNoteHit]], [[code:hiker/boards/containing_note_with_paths]], [[code:hiker/boards/ops/add_card]], [[code:hiker/panels/board/picker_context_ctx]], [[code:hiker/panels/board/column_picker]], [[code:hiker/panels/board/add_card]]
verifies:: [[code:hiker/boards/tests/contains_note_matches_by_path]]
note:: file-tree verb + per-board idempotency intact; card serializes as `{path}` only · evidence: `core/src/boards/ops.rs` (`add_card`/`AddCardArgs` — `store` field dropped; no ULID stamping); `app/src/sidebar/files.rs` + `app/src/panels/board.rs::add_card`

**Remove a card.** A per-card "Remove from board" verb drops the card's entry from the board-doc frontmatter. The referenced note is untouched — removal is a board-membership edit, not a note deletion. [board-remove-card]
status:: done
implements:: [[code:hiker/boards/ops/remove_card]]
touches:: [[code:hiker/panels/board]]
note:: drops the card entry from frontmatter; referenced note untouched · evidence: `core/src/boards/ops.rs` (`remove_card` — polymorphic `card_handle: &str`); `app/src/panels/board.rs::BoardAction::RemoveCard` (per-card `×`)

**Freeform cards.** A card need not reference a note. A **freeform card** carries its own text and a card-local id, with no note ref — for quick items, checklist entries, or placeholders not worth their own note. In `hiker.columns[].cards` it serializes as `{ card_id: <ulid>, text: "..." }`; presence of `text` (or `card_id` without `path`) discriminates from a note card's `{ path }`. The `card_id` is internal — it disambiguates two freeform cards with identical text for move/reorder/delete verbs and never surfaces to the user. Freeform cards have no resolution, no note link, and are **skipped by the derived `board_cards` index** — they aren't notes, so the reverse "boards containing note" lookup and rename-rewrite don't apply to them. Each column carries a **+ Add card** affordance that creates one inline (edit the text in place); editing rewrites the card's `text` through the board-doc user-save path like any other frontmatter edit. A freeform card can later be promoted in place to a real note via the card's **Convert to note** verb ([[spec:freeform-promote-note]], pm.md). [board-freeform-card]
status:: done
implements:: [[code:hiker/boards/BoardCard]], [[code:hiker/boards/parse_card]], [[code:hiker/boards/ResolvedCard]], [[code:hiker/boards/resolve_card]], [[code:hiker/boards/ops/add_text_card]], [[code:hiker/boards/ops/set_card_text]], [[code:hiker/panels/board/Pane#editing_card]], [[code:hiker/panels/board/Pane#card_edit_focus]], [[code:hiker/panels/board/render_card_editor]], [[code:hiker/panels/board/add_text_card]]
verifies:: [[code:hiker/boards/tests/mixed_note_and_text_cards_round_trip]]
note:: discriminator: `text` present → freeform, `path` present → note card; the polymorphic handle string is the note's path for note cards or the `card_id` for freeform cards (`card_matches`) · evidence: `core/src/boards/mod.rs` (`BoardCard::Text { card_id, text }`); `core/src/boards/ops.rs` (text card verbs key on `card_id`); `app/src/panels/board.rs` (inline editor only on Text cards)


## Indexer integration

A derived `board_cards` table in `index.db` supports the reverse lookups and the auto-update-on-move path: which boards contain a given note, which cards belong to a given board, and where each card sits. Re-derived from each board-doc's frontmatter on ingest, fail-loud on schema bump, exactly like [[spec:trail-waypoints-derived-table]]. Board-docs are visible notes under `vault/boards/`, so they ride the standard md ingest path with no watcher carve-out (unlike trails' hidden waypoint dir). [board-cards-derived-table]
status:: done
implements:: [[code:hiker/indexer/jobs/handle_simple_job]], [[code:hiker/indexer/jobs/process_upsert]], [[code:hiker/indexer/jobs/update_board_cards_if_relevant]], [[code:hiker/indexer/jobs/process_delete]], [[code:hiker/store/boards/impl#[Store]replace_board_cards]], [[code:hiker/store/boards/impl#[Store]delete_board_cards_by_board]], [[code:hiker/store/boards/impl#[Store]delete_board_cards_by_board_path]], [[code:hiker/store/boards/impl#[Store]boards_containing_note]], [[code:hiker/store/boards/impl#[Store]cards_of]], [[code:hiker/store/boards/impl#[Store]board_paths]], [[code:hiker/store/boards/impl#[Store]rename_board_card_note_paths]], [[code:hiker/store/boards/impl#[Store]rename_board_card_paths_for_board]], [[code:hiker/store/dto/BoardCardRow]], [[code:hiker/store/dto/BoardContainingHit]]
touches:: [[code:hiker/boards]], [[code:hiker/links_rename]]
note:: re-derived on board-doc ingest (clear-by-board); cleared on delete; Move/MoveFolder/Rename hooks now flow through the shared rename-rewrite pass; `boards_containing_note` is the load-bearing enumeration query for the board side of that pass, sibling to `trails_containing_note`; no watcher carve-out · evidence: `core/src/store/boards.rs` (schema v9: `board_cards` + `replace_board_cards`/`cards_of`/`boards_containing_note`/`rename_*`); `core/src/indexer/jobs.rs` (`update_board_cards_if_relevant`, delete cleanup); `core/src/boards/mod.rs::on_note_moved` invoked from `core/src/links_rename.rs::on_note_moved` (the shared rename-rewrite orchestrator)


## Deferred

- **Drag-and-drop between columns.** v1 moves a card via a per-card "Move to >" menu; DnD rides the uniform vault-path drag payload sketched in `design.md` ([[spec:trails-dnd-ingestion]]) once it lands, and a card dragged from the file tree onto a column is the same gesture as the "Add to board" verb. [board-dnd]
status:: done
implements:: [[code:hiker/panels/board/render_card]]
note:: card is a drag source carrying its id+source column; column is a drop zone for both card-moves and file-row rel-paths; per-card "Move to >" menu kept as fallback · evidence: `app/src/panels/board.rs` (`CardDrag` payload, `render_card`/`render_column` `dnd_drag_source`+`dnd_drop_zone`; card-level drop → precise `to_index`, column-body drop → append; column `dnd_release_payload::<String>` from the file tree → `add_card`)
- **Per-column WIP limits.** Column-level constraints (cap a column at N cards, flag overflow). Within-column ordering is already covered by the array model; only the limit is future. [board-wip-limits]
status:: done
implements:: [[code:hiker/boards/Column#wip_limit]], [[code:hiker/boards/ResolvedColumn#wip_limit]], [[code:hiker/boards/ops/set_column_wip_limit]], [[code:hiker/panels/board/BoardAction#SetWipLimit#name]], [[code:hiker/panels/board/render_column_header]]
note:: per-column WIP limit in `hiker.columns` frontmatter; overflow flagged soft (not blocked) for v1; column-menu sets/clears via the core op; round-trip + set/clear unit tests · evidence: `core/src/boards/mod.rs` (`Column.wip_limit` + `ResolvedColumn.wip_limit`, omitted-when-`None` round-trip); `core/src/boards/ops.rs` (`set_column_wip_limit`); `app/src/panels/board.rs` (header "(count/limit)" + over-limit flag, `render_wip_limit_menu`)
- **MCP board tools.** A read + curate surface so attached agents can read boards as context and reorganize them (read + write verbs gated by [[spec:agent-write-review-mode]]); full surface in `mcp.md` §"Board tools". The active board is injected into the agent's turn context when a board tab is focused ([[spec:chat-active-note-context-injection]]). [board-mcp-tools]
status:: partial
implements:: [[code:hiker/boards/add_card_preview]], [[code:hiker/boards/ops/plan_new_board]], [[code:hiker/boards/ops/BoardEdit]], [[code:hiker/boards/ops/apply_edit]], [[code:hiker/boards/ops/preview_move_card]], [[code:hiker/boards/ops/preview_edit]], [[code:hiker/config/patch/ELIGIBLE_VAULT]], [[code:hiker/config/patch/ELIGIBLE_USER]]
note:: the read pair ([[spec:mcp-tool-boards-list]]/`-board-get`) + all nine new write tools ([[spec:mcp-tool-board-create]]/`-add-text-card`/`-move-card`/`-set-card-text`/`-remove-card`/`-add-column`/`-rename-column`/`-reorder-column`/`-delete-column`) plus the original [[spec:mcp-tool-board-add-card]] are landed and smoke-tested (`board_write_tools_round_trip_direct`, `board_create_commits_directly_even_in_review_mode`, `server_lists_expected_tools`). KNOWN GAPS preserved from the per-tool rows: `board_create` commits directly even in review mode (the op-log whole-file-create path would otherwise seed a phantom empty `.md`); `board_add_card`'s Send-safe path doesn't lazy-stamp the source note's ULID (the `add_card_preview` is read-only) — both folded into their per-tool rows. `repoint_card` is intentionally NOT exposed (`docs/mcp.md` §"Board tools") · evidence: `mcp-server/src/handler/{router,dispatch,params}.rs` (`boards_list`/`board_get` + all eleven board write tools); `core/src/boards/{mod,ops}.rs` (`add_card_preview`, `preview_edit`, `preview_move_card`, `plan_new_board`, `BoardEdit` shared mutation step); `core/src/config/{sections,patch}.rs` (per-tool toggles); `docs/mcp.md`


## Out of scope

- **Grouping notes by a frontmatter field.** Boards are curated reference lists, not a saved query over a `status:` field — a note's board membership and column are board-owned, not derived from the note's own metadata. A query-defined "smart board" is a different feature and not this doc's concern. The saved-query primitive itself lives in `queries.md` ([[spec:query-doc-shape]]); a board-lane presentation of one is a later layer over it. Cross-board meaning for column names is likewise opt-in elsewhere — a board-like kind's column→state mapping ([[spec:kind-column-state-map]]); plain boards keep zero PM semantics.
- **Cross-vault boards.** Boards are vault-scoped; paths aren't unique across vaults. Same boundary as trails.
