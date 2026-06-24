# Trails

Curated walks through a vault — a memex-style first-class concept where each waypoint is its own note carrying the user's annotation and a path-based link back to the source the waypoint is about. Trails ride the existing markdown / indexer / watcher / changes / trash machinery so they're searchable, editable, syncable, and backup-able like any other note.

Use cases:

- Ordered context for an agent (a curated walk with per-waypoint "why this matters here" annotations).
- Opt-in agent activity logging — notes an agent reads/writes/cites become a trail. Agent/clustering-proposed trails ride the normal op-log pending/patch-review path like any other agent write.
- Narrative *between* immutable multimodal sources (PDFs, web archives, audio transcripts) you can't annotate inline.
- Re-traversable record of an investigation where the path itself — order, side trips — is the point.


## Storage layout

A trail is a *trail-doc* plus a **companion folder** of *waypoint-notes* beside it:

```
<vault>/
└── trails/                              # default new-trail location (configurable)
    ├── my-trail.md                      # trail-doc — regular md note
    └── my-trail/                        # companion folder (note-companion-folder)
        ├── raptor-paper--7K2A9F.md
        ├── embedding-survey--3Q8M1B.md
        ├── inline-citation--5R7Z2D.md   # child of raptor-paper (order/shape in frontmatter, not dirs)
        └── scratchpad--9X4N6C.md
```

The trail-doc lives at a user-chosen vault location (default `trails/`, configurable), and its waypoints live in the sibling `<trail>/` companion folder ([[spec:note-companion-folder]] in `files.md`) — visible, indexed notes, not hidden under `.hiker/`. The waypoint-note basename is `<source-basename>--<rand6>.md` where `<rand6>` is a 6-char random alphanumeric disambiguator so two waypoints can point at the same source. The trail-doc's identity is its vault path (per [[spec:op-log-path-identity]]) — no stored id, nothing stamped into frontmatter. Renaming or moving the trail-doc is an observed content-preserving move ([[spec:op-log-observed-move]]): it moves the companion folder in the same `move_note` transaction and rewrites the trail↔waypoint paths through the shared [[spec:wikilink-rename-rewrite]] pass, so the trail never loses identity across the rename. [trail-storage-layout]
status:: done
touches:: [[code:hiker/bootstrap]], [[code:hiker/vault]]
note:: waypoints live in the trail-doc's **visible companion folder** `<dir>/<trail>/<basename>--<rand6>.md` ([[spec:note-companion-folder]]). Renaming/moving a trail-doc moves the folder (via `move_note`) + rewrites each waypoint's `in_trail` and the trail-doc's `hiker.waypoints[].path` through the rename-rewrite pass ([[spec:wikilink-rename-rewrite]]); identity stays the vault path. One-time migration relocates legacy `.hiker/trails/<id>/waypoints/` dirs on vault open (idempotent, `app/src/bootstrap.rs::open_and_seed_oplog`). The indexer / `on_note_moved` discriminate trail-doc vs waypoint by `hiker.kind`, not path · evidence: `core/src/trails/mod.rs` (`waypoints_dir_for_doc` = trail-doc companion folder, `waypoint_filename`, `random_alphanumeric_6`); `core/src/trails/ops.rs` (`append_waypoint` writes to the companion folder + lazy-creates it; `migrate_waypoints_to_companion_folders` one-time migration; `on_note_moved` + `rewrite_own_waypoint_paths_on_trail_doc_move`); `core/src/vault.rs` (`move_note` companion-folder pairing)
implements:: [[code:hiker/trails/TRAILS_DIRNAME]], [[code:hiker/trails/DRAFTS_DIRNAME]], [[code:hiker/trails/dir]], [[code:hiker/trails/dir_prefix]], [[code:hiker/trails/waypoints_dir_for_doc]], [[code:hiker/trails/waypoint_filename]]
verifies:: [[code:hiker/trails/tests/parse/waypoints_dir_is_trail_doc_companion_folder]], [[code:hiker/trails/tests/parse/waypoint_filename_uses_rand6_suffix]]
touches:: [[code:hiker/hiker-core/trails]]

The companion folder is **flat regardless of tree depth** — side trails don't get nested directories. The trail-doc's `hiker.waypoints` frontmatter is the only source of truth for both *order* and *tree shape*; filenames are stable identifiers, not position encoders, so reordering, re-parenting, or moving waypoints between depths never renames a file. Vault mode nests the waypoints under the trail-doc by the `hiker.waypoints` tree (`vault-view.md`), preserving order and side-trail shape.


## Trail-doc shape

Frontmatter:

```yaml
---
hiker:
  kind: trail
  last_activated_at: <iso8601>          # drives Trails-mode dropdown ordering
  waypoints:                            # ordered tree of waypoint-note paths
    - path: "trails/my-trail/raptor-paper--7K2A9F.md"
      waypoints:                        # optional — children form a side trail
        - path: "trails/my-trail/inline-citation--5R7Z2D.md"
    - path: "trails/my-trail/embedding-survey--3Q8M1B.md"
---
```

(The trail's identity is its vault path — nothing in frontmatter, per Storage layout. Waypoint paths point into the companion folder and are rewritten on rename like any other reference.)

`hiker.waypoints` is a tree: each entry is a vault-relative path to a waypoint-note and may carry its own `waypoints:` array of child entries forming a side trail. Children are themselves trees — side trails nest arbitrarily deep. A waypoint with no `waypoints:` key (or an empty array) is a leaf; the common case for v1 is a flat tree where most or all waypoints sit at the root and side trails appear only where the user has explicitly digressed. [trail-side-trail-shape]
status:: done
note:: slice S1a backend + UI nested rendering both landed (UI tracked under [[spec:trails-mode-side-trail-render]]) · evidence: `core/src/trails/{mod,ops}.rs` (`WaypointEntry`, `parse_waypoint_entry`, `waypoint_entry_to_json`, `walk_waypoints_depth_first`, `find_waypoint`, `collect_descendant_ids`, `remove_waypoint_from_tree`, `append_waypoint(parent_waypoint_path)`, `remove_waypoint` cascade, `ResolvedWaypoint.children`/`tree_path`, `resolve_waypoint_tree`) + `core/src/store.rs` (schema v6 with `parent_waypoint_id` + `tree_path`) + `core/src/indexer.rs` (recursive trail-doc walk) + `app/src/trails/sidebar.rs` (nested rendering)
implements:: [[code:hiker/indexer/jobs/impl#[`WaypointIngest<'a>`]rebuild_trail_doc_rows]], [[code:hiker/trails/WaypointEntry]], [[code:hiker/trails/TrailDocFrontmatter]], [[code:hiker/trails/parse_waypoint_entry]], [[code:hiker/trails/waypoint_entry_to_json]], [[code:hiker/trails/walk_waypoints_depth_first]], [[code:hiker/trails/find_waypoint_mut]], [[code:hiker/trails/find_waypoint]], [[code:hiker/trails/collect_descendant_paths]], [[code:hiker/trails/ResolvedWaypoint]], [[code:hiker/trails/resolve_waypoint_tree]]
verifies:: [[code:hiker/trails/tests/parse/parse_trail_doc_round_trips_nested_tree]], [[code:hiker/trails/tests/parse/parse_trail_doc_round_trips_empty_tree]], [[code:hiker/trails/tests/parse/walk_waypoints_yields_depth_first_with_tree_paths]]

Body is freeform markdown — a hand-authored README for the trail (why it exists, what it covers, who it's shared with). It is *not* auto-generated and never overwritten by hiker; the sidebar renders the waypoint walk, the body is the trail's prose framing, separate from the per-waypoint annotations (which live in each waypoint-note's body). [trail-doc-shape]
status:: done
note:: recursive tree shape landed in slice S1a; flat-format yaml still parses cleanly as a tree of all-root entries; round-trip tests cover flat / nested / empty · evidence: `core/src/trails/{mod,ops}.rs` (`TrailDocFrontmatter`, `WaypointEntry`, `parse_trail_doc`, `parse_waypoint_entry`, `parse_trail_doc_for`, `write_trail_doc_frontmatter`, `waypoint_entry_to_json`, `create_trail`)
implements:: [[code:hiker/trails/TrailDocFrontmatter]], [[code:hiker/trails/parse_trail_doc]], [[code:hiker/trails/write_trail_doc_frontmatter]], [[code:hiker/trails/ops/create_trail]]
verifies:: [[code:hiker/trails/tests/parse/write_trail_doc_preserves_unknown_hiker_siblings]]
touches:: [[code:hiker/hiker-core/trails]]

The trail-doc must have a `.md` extension to be recognized as a trail. A note carrying `hiker.kind: trail` in frontmatter but a non-`.md` extension is treated as a regular note — the discriminator alone isn't enough.


## Waypoint-note shape

Frontmatter:

```yaml
---
hiker:
  kind: waypoint
  references:                           # vault path to the source note
    path: "research/raptor-paper.md"
  in_trail:                             # vault path to the trail-doc
    path: "trails/my-trail.md"
---
```

Body is the user's annotation — filled in by the user as they author commentary. Standard markdown; renders in the editor like any other note when opened directly. [waypoint-note-shape]
status:: done
implements:: [[code:hiker/trails/parse_waypoint]], [[code:hiker/trails/write_waypoint_frontmatter]], [[code:hiker/trails/ops/AppendWaypointArgs]]
note:: slice 2: ops + empty-body creation landed · evidence: `core/src/trails/{mod,ops}.rs` (`WaypointFrontmatter`, `parse_waypoint`, `write_waypoint_frontmatter`, `append_waypoint`)

The waypoint-note body is **empty when the waypoint is created** (clean canvas). [trail-empty-waypoint-body]
status:: done
implements:: [[code:hiker/trails/ops/AppendWaypointArgs]]
verifies:: [[code:hiker/trails/tests/parse/empty_waypoint_note_has_zero_bytes_after_closing_fm]]
note:: unit test asserts zero bytes after the closing FM · evidence: `core/src/trails/{mod,ops}.rs` (`empty_waypoint_note`, `append_waypoint` with `annotation=None`)

The two-hop structure (trail-doc → waypoint-note → source-note) makes each waypoint searchable, editable, cross-reference-addressable, and visible in the autosave / changes / trash pipelines like any other note.


## Reference shape

Every reference from a trail-doc to a waypoint-note (and from a waypoint-note to its source note or its trail-doc) is a **vault-relative path**. The path is the identity — there is no separate ID half. [trail-path-references]
status:: done
note:: `DoubleLinkRef` removed; every trail / waypoint reference is a vault-relative path, no id half. Legacy `id:` siblings parse as unknown hiker siblings — ignored by the model, preserved on rewrite — so existing trail-docs round-trip · evidence: `core/src/trails/mod.rs` (`WaypointEntry`, `WaypointFrontmatter`, `parse_path_ref`, `path_ref_to_json`); `core/src/trails/ops.rs` (every rewrite uses `fm.references` / `fm.in_trail` as `String`)
implements:: [[code:hiker/trails/WaypointEntry]], [[code:hiker/trails/WaypointFrontmatter]], [[code:hiker/trails/parse_path_ref]], [[code:hiker/trails/parse_waypoint_entry]], [[code:hiker/trails/walk_waypoints_depth_first]]
verifies:: [[code:hiker/trails/tests/parse/write_trail_doc_preserves_unknown_hiker_siblings]]

```yaml
path: "research/raptor-paper.md"
```

**Resolution** when hiker reads a reference:

- Path resolves to an indexed note → resolved.
- Path doesn't resolve (file missing, name typo, target deleted) → orphaned. Surfaced visually in the sidebar (greyed card with "broken reference" pill); waypoint stays in the trail so the user decides whether to delete or fix. [trails-mode-orphan-card]
status:: done
note:: under path-as-identity the resolution outcome collapsed to `Resolved | Orphan` ([[spec:trail-reference-resolution]]); the trails sidebar's broken-card path and the core resolver agree on the same single failure mode · evidence: `app/src/trails/sidebar.rs` (orphan-like card rendering); `core/src/trails/ops.rs::ResolutionOutcome::Orphan`

[trail-reference-resolution]
status:: done
note:: `SelfHeal` + `PathConflict` retired; resolution is a single `store.note_exists(path)` lookup · evidence: `core/src/trails/ops.rs::resolve_reference` returns `Resolved { rel_path } | Orphan` only
implements:: [[code:hiker/trails/ops/ResolutionOutcome]], [[code:hiker/trails/ops/resolve_reference]]
verifies:: [[code:hiker/store/tests]]

Under path-as-identity there is **nothing left to wire** for a path-conflict modal — an unresolved reference is just an orphan, so the old conflict-resolution dialog was retired. [trail-path-conflict-modal]
status:: retired
touches:: [[code:hiker/panels/board]], [[code:hiker/widgets/modal]]
note:: evidence: `Modal::PathConflict`, `PathConflictTarget`, `path_conflict_dialog`, `panels/board::repoint_card` + `break_card` all removed (`app/src/state.rs`, `app/src/widgets/modal.rs`, `app/src/panels/board.rs`); `core::boards::ops::repoint_card` removed; resolution-outcome enum collapsed

**Auto-update on note move.** Path rewriting rides the shared [[spec:wikilink-rename-rewrite]] pass: when a source note moves (via [[spec:move-note-core-cmd]], [[spec:drag-and-drop-move]], or a watcher-detected external rename), the indexer's referrer-rewrite pass updates every affected `hiker.references.path` in waypoint-notes and every affected `hiker.waypoints[].path` in trail-docs in the same transaction as the move itself. Wikilink bodies, kanban card paths, and trail/waypoint paths all flow through one rewrite path. [trail-auto-update-on-note-move]
status:: done
note:: trail-side rewriter now consolidated into the shared [[spec:wikilink-rename-rewrite]] pass — one entry point in the indexer (`IndexJob::Rename` / `Move` / `MoveFolder`) fans out to all three referrer types; indexer's `trails_containing_note` enumeration stays the load-bearing primitive · evidence: `core/src/trails/ops.rs::on_note_moved` (case 1: source-note move → waypoint `references.path`; case 2: trail-doc move → waypoint `in_trail.path`; case 3: waypoint-note move → trail-doc `hiker.waypoints[].path` + derived `trail_waypoints.waypoint_path` rename); called from `core/src/links_rename.rs::on_note_moved` (the shared rename-rewrite orchestrator alongside boards + wikilinks)
implements:: [[code:hiker/indexer/impl#[Handle]attach_watcher]], [[code:hiker/indexer/start_indexer_with_tasks]], [[code:hiker/links_rename/on_note_moved]], [[code:hiker/trails/ops/on_note_moved]]


## Active trail

A vault has at most one active trail at a time. When set, the active trail is the routing target for every "import a source" path — the browser-extension capture, the MCP scrape tool, drag-URL, the share sheet — and is what the Trails sidebar mode renders. Persistence: `vault.active_trail = <rel-path-to-trail-doc> | null`, eligible for `set_setting` write-back per [[spec:settings-write-back]]. [active-trail-state]
status:: done
note:: active trail is `vault.active_trail` config, read each frame via `bridge::active_trail_rel`; there is no in-`AppState` trail model and no host seams — the app calls `core::trails` verbs directly through `app/src/trails/bridge.rs` (block_on on the UI thread). Capture entry points (browser ext, drag-URL, MCP scrape) plug into the append path when they land — [[spec:trail-capture-flow]] covers that · evidence: `core/src/config.rs` (`VaultConfig.active_trail`, eligible vault key `vault.active_trail`) + `core/src/trails/{mod,ops}.rs` (`stamp_last_activated_at`, `list`, `get_trail`, `TrailListItem`, `TrailDetail`, `ResolvedWaypoint`) + `app/src/trails/bridge.rs` (`active_trail_rel`, sync→async core-verb bridge: `create_trail`/`set_append_cursor`/`delete_trail`/`stamp_activated` via `Handle::block_on`) + `app/src/trails/sidebar.rs` (dropdown read/write, activation) + `app/src/files/sidebar.rs` ([[spec:trail-set-as-active-context-verb]])
implements:: [[code:hiker/config/patch/ELIGIBLE_VAULT]], [[code:hiker/config/sections/VaultConfig#active_trail]]

**No indicator outside Trails mode in v1.** When the user is in Files or Cluster trees mode, there's no UI cue which trail is active. Capture-routing into the wrong trail is the failure mode to watch; revisit if it bites in real use.


## Sidebar Trails mode

The Trails mode body is the third sidebar mode (per [[spec:sidebar-mode-switcher]]). Its content is mostly the **active trail rendered vertically, top-to-bottom, read-only**, with header chrome for switching trails and reaching the trail-doc. The editor pane on the right is unaffected — clicking a waypoint opens its source note in the editor; the sidebar stays. [trails-mode-body]
status:: done
note:: slice U2: vertical waypoint card list, header (dropdown + trail-head + expand-all), watcher-driven refresh on `.hiker/trails/` and active trail-doc paths. Reads trails live via `core::trails::list`/`get_trail` through `app/src/trails/bridge.rs` — no in-memory trail model · evidence: `app/src/trails/sidebar.rs` (`render()`)

### Header

A compact row at the top of the sidebar body:

- **Active-trail dropdown** — left side, takes most of the row width. Lists trails ordered by most-recent activation (`hiker.last_activated_at`), top N entries, plus an **"All trails…"** entry at the bottom that opens a flat picker of every trail in the vault, plus **"None"** as the always-present top item that clears the active trail. Selecting a trail activates it (sets `vault.active_trail`, stamps `hiker.last_activated_at` on the trail-doc, body re-renders). [trails-mode-active-trail-dropdown]
  status:: done
  implements:: [[code:hiker/trails/ops/stamp_last_activated_at]]
  note:: popover (matches the existing dropdown idiom) — None + ordered trails + "All trails…" picker; click activates by writing `vault.active_trail` config + `bridge::stamp_activated` (core verb), no host seam · evidence: `app/src/trails/sidebar.rs` (active-trail dropdown + all-trails picker)
  The dropdown sorts trails by `last_activated_at` descending with nulls last, alphabetical title fallback inside ties. [trails-dropdown-ordering]
  status:: done
  note:: sort by `last_activated_at` desc with nulls last; alphabetical title fallback inside ties · evidence: `app/src/trails/sidebar.rs` (trail sort)
- **Trail-head icon** — right of the dropdown. Clicking opens the active trail's trail-doc in the editor pane on the right (so the user can read or edit the trail's framing prose). The icon is the squiggly-trail glyph (same SVG family as the Trails-mode button in the sidebar mode switcher). Disabled when no trail is active. [trails-mode-trail-head-icon]
status:: done
note:: squiggly-trail icon button right of the dropdown; disabled when no active trail; click opens active trail-doc · evidence: `app/src/trails/sidebar.rs` (`header_row()` trail-head button)

### Body

Vertical list of waypoint cards in trail order — depth-first traversal of the waypoint tree. Each card collapsed by default; click the card chevron to expand. [trails-mode-waypoint-card]
status:: done
note:: tree-aware ordinal (`tree_path` "1", "1.2", ...) + nested rendering both landed; the block render recurses on `ResolvedWaypoint.children`, side-trail collapse chevron appears when a waypoint has children, body-expand chevron stays on the right edge of the card · evidence: `app/src/trails/sidebar.rs` (waypoint card + block rendering)

Collapsed state shows:

- Source-note basename
- One-line annotation snippet (first non-empty line of the waypoint-note body)
- A small chevron for expand
- A subtle ordinal showing position within the parent's child list (`1`, `1.2`, `1.2.1` for nested side-trail waypoints)

Expanded state renders the full waypoint-note body inline (markdown, live-previewed via the editor's decoration plumbing). One card expanded at a time; clicking another card collapses the previous. **"Expand all"** affordance in the sidebar header collapses or expands every card together.

Click affordances inside a card:

- **Click the source-note basename** → opens the source note in the editor pane (respects [[spec:file-switch-guard-dirty]]).
- **Click an "edit annotation" affordance inside the expanded body** → opens the waypoint-note itself in the editor for full-screen editing of the commentary.

### Side trail rendering

Side-trail waypoints (waypoints that are children of another waypoint per [[spec:trail-side-trail-shape]]) render **indented under their parent**. Each level of nesting adds one indent step plus a thin left rule running the height of the side-trail block, so the user can visually trace the digression and see where it returns to the main line. Children of the same parent render in their frontmatter order; depth-first traversal matches reading order. [trails-mode-side-trail-render]
status:: done
note:: sidebar renders the waypoint tree directly: each child block gets one indent step + a thin left rule; a parent waypoint with children gets a side-trail collapse chevron at the left edge of the card head (visually distinct from the body-expand chevron at the right edge) that toggles a per-waypoint path in the collapse set (default expanded per spec). Filetree dropdown also walks the tree recursively · evidence: `app/src/trails/sidebar.rs` (waypoint-block rendering + side-trail collapse + side-trail chevron) + `app/src/files/sidebar.rs` (recursive waypoint-children walker for the filetree dropdown)

A parent waypoint with children gets a second chevron (separate from the body-expand chevron) that **collapses or expands the side trail** without touching the parent's own annotation — noise control on long branchy trails. Default state for v1: every side trail expanded, so the full tree is visible on first render. (No reading-position "you are here" pointer in v1 beyond the append-cursor glyph; "follow this digression and hide the rest" is deferred, per Out of scope.)

### Empty states

[trails-mode-empty-states]
status:: done
note:: three branches: no trails in vault (Create-a-trail button), no active trail (pick from dropdown hint), active trail with zero waypoints (capture-or-+ hint) · evidence: `app/src/trails/sidebar.rs` (empty-state rendering)

- Trails mode active, no active trail set: dropdown shows "None"; body shows a hint pointing at the dropdown ("Pick a trail to walk, or `+` to start a new one").
- Trails mode active, no trails in the vault at all: dropdown empty; body shows a "Create a trail" affordance that calls the same path the `+` button does.
- Active trail set with zero waypoints: body shows "Empty trail — capture into it or use `+` to add the first waypoint."
- Active trail with one or more orphaned references (paths that don't resolve): orphan cards rendered greyed with a "broken reference" pill, sorted in their original positions; the rest of the trail renders normally.

### Sidebar invariants

The Trails mode sidebar body is **read-only** for editing operations: no drag-to-reorder, no click-to-rename, no in-place annotation editing. Structure changes happen by editing the trail-doc's frontmatter directly, editing waypoint-notes in the editor, or the one editing verb below. The read-only invariant keeps the sidebar a navigation surface and avoids tangling the linking-metadata between waypoint-notes when waypoints are reordered. [trails-mode-sidebar-read-only]
status:: done
note:: no drag-reorder, no inline rename, no inline annotation editing — only the right-click "Remove waypoint" verb; structural edits go through opening the trail-doc / waypoint-note in the editor · evidence: `app/src/trails/sidebar.rs` (module top comment + per-card click handlers)

The single editing verb is **Remove waypoint** in a row's right-click context menu: deletes the waypoint-note from the companion folder, removes the entry from the trail-doc's `hiker.waypoints` tree, and confirms first (removal moves the annotation to trash, not gone forever). Goes through `core::ops::delete` so the waypoint-note lands in `.hiker/trash/` and is restorable. [trails-mode-remove-waypoint-verb]
status:: done
note:: slice S1a backend cascade + slice S1b UI confirm-with-count both landed: the verb calls `descendant_count` first and shows the cascade-aware copy ("Remove this waypoint and N side-trail waypoints?") when N>0; success toast pluralizes from `removed_count` · evidence: `core/src/trails/ops.rs` (`remove_waypoint`, `descendant_count`, `RemoveWaypointOutcome`) + `app/src/trails/bridge.rs` (`descendant_count`) + `app/src/trails/sidebar.rs` (remove-waypoint verb + context menu) + `app/src/state.rs` (`ConfirmIntent::DeleteTrailWaypoint` runs `core::trails::ops::remove_waypoint` via inline `block_on`)
implements:: [[code:hiker/trails/ops/RemoveWaypointOutcome]], [[code:hiker/trails/ops/remove_waypoint]], [[code:hiker/trails/ops/descendant_count]]

**Removing a parent with children cascades.** If the waypoint being removed has child waypoints (a side trail), the confirm dialog names the child count ("Remove this waypoint and N side-trail waypoints?") and the remove deletes the parent and every descendant in a single pass — each descendant waypoint-note moves to `.hiker/trash/` like the parent, and the trail-doc frontmatter drops the entire subtree. Children are not promoted to the grandparent; trash keeps the cascade reversible.

**Trails mode is independent of Cluster trees mode.** The Trails and Cluster trees sidebar bodies are sibling modes; switching modes swaps which body renders. Bodies don't share state, don't stack, and one mode's UI must never bleed into another's — only the active mode's body renders. [trails-mode-isolation-from-clusters]
status:: done
note:: trails and cluster-trees sidebar bodies are sibling activity views; the activity registry paints only the active activity's view, so neither bleeds into the other · evidence: `app/src/activity/mod.rs` (`ActivityRegistry` renders only the active activity's views); trails + clusters sidebar bodies live with their own activities (`app/src/trails/{mod,sidebar}.rs`, `app/src/clusters/`)


## Trail graph viewer (deferred)

A future trail visualization that opens a trail (or set of trails) as a graph in its own editor-pane tab, separate from the Cluster trees graph view. Nodes are waypoints; edges are sequence + side-trail branching. The renderer reuses the shared egui force-graph widget (`app/src/widgets/force_graph.rs`) so the viewers share rendering primitives, camera/zoom, and selection conventions. Tab kind is `graph` (already reserved in `TabKind`); the trail viewer is one consumer, a future cross-vault overview another.

Scope is light for v1: open the active trail as a graph, waypoints with basenames, edges colored by sequence vs side-trail. Multi-trail overlays, time-axis layouts, and filtering by activation recency are deferred. [trails-graph-viewer-tab]
status:: planned
note:: future `graph` tab (kind already reserved in `TabKind` in `app/src/tab.rs`) hosting a force-graph-rendered trail view via the shared egui force-graph widget (`app/src/widgets/force_graph.rs`) built for the vault graph view and cluster editor; nodes are waypoints, edges are sequence + side-trail branching. Single-trail v1; multi-trail overlays + time-axis + activation-recency filters are deferred polish


## Filetree integration

[trail-row-icon]
status:: done
note:: squiggly-trail glyph prefixed onto trail-doc file rows; muted by default, accent on hover/selection. Cache rebuilds on watcher events touching `.md` outside `.hiker/` (conservative — covers frontmatter flips both ways) · evidence: `app/src/files/sidebar.rs` (trail-doc set cache + trail-icon prefix + vault-open seed + watcher refresh on `.md` outside `.hiker/`)

Trail-docs render in the file tree at their natural FS location with a distinctive **squiggly-trail glyph** on the prefix side (same icon family as the Trails-mode button in the sidebar mode switcher). The trail-doc behaves like any other md note — search opens it, click opens it in the editor, drag-and-drop moves it (with the auto-update-on-move path rewriting any `hiker.in_trail` or `hiker.waypoints` references that pointed at the moved trail-doc).

**Per-trail dropdown chevron.** Each trail-doc row in the tree gets an expand chevron when its trail has at least one waypoint. Clicking the chevron expands the row inline (same machinery as folder expansion) to show the trail's waypoint-notes nested underneath, in trail order. Chevron is hidden for trails with zero waypoints. Expand state is per-trail and resets on vault open (no persistent expansion memory — keeps the tree tidy after a session). [trail-row-dropdown-chevron]
status:: done
note:: trail-doc rows get a chevron when the trail has at least one waypoint; expansion renders the trail's waypoint tree nested under the trail-doc row, mirroring the folder-expansion shape — each side-trail level indents one more step per depth. Per-waypoint side-trail collapse stays in the sidebar Trails mode (per spec); the filetree just expands the whole tree · evidence: `app/src/files/sidebar.rs` (trail expansion + waypoint-children rendering)

**Waypoints in the Files tree.** A trail-doc's waypoints live in its visible companion folder ([[spec:trail-storage-layout]]), so in Files mode the trail-doc and its `<trail>/` folder appear like any note + folder, collapsed by default. No special hide/show toggle — the folder is ordinary tree content the user collapses or ignores. (Vault mode's clean reading nest is per Storage layout.)

**"Set as active trail" right-click verb.** Right-clicking a trail-doc row in the tree opens a context menu with a "Set as active trail" entry; selecting it activates the trail (sets `vault.active_trail`, stamps `hiker.last_activated_at`). Lets the user activate a trail without switching sidebar modes. The verb is hidden on non-trail rows. [trail-set-as-active-context-verb]
status:: done
implements:: [[code:hiker/files/sidebar/pick_copy_target]]
note:: "Set as active trail" entry inserted above Open/Rename/Delete on trail-doc file rows; hidden on non-trail rows; click writes `vault.active_trail` config + stamps activation via the core verb + toast · evidence: `app/src/files/sidebar.rs` (`FileVerb::SetActiveTrail`, `set_active_trail`)
implements:: [[code:hiker/files/sidebar/set_active_trail]]

**"Export to canvas" right-click verb.** Right-clicking a trail-doc row offers **Export to canvas**, which snapshots the trail's waypoint tree into a new `.canvas` document (each waypoint a file-node pointing at its waypoint-note, parent→child links as edges). Snapshot, not synced — defined in `canvas-export.md`. [canvas-export-trail-verb]

**"Add to active trail" right-click verb.** On indexable note rows (`.md` / `.txt`); appends the note as a waypoint of the active trail. Full behavior under "Building a trail while reading" below. On success, toast `"Added to <trail-basename>"`. [trail-add-to-active-from-tree-verb]
status:: done
note:: "Add to active trail '<name>'" entry on indexable note rows; hidden on trail-docs, waypoint-notes under `.hiker/trails/`, and `IndexState` `unsupported`; disabled when none active; click → `core::trails::ops::append_waypoint` (via inline `block_on` at the call site, direct — user-explicit verbs need error visibility) + toast. Idempotency: the verb is also disabled with an "Already in '<name>'" label when the row's path is already a waypoint of the active trail at any depth — read from the active-trail membership set gathered by `trail_deco_snapshot`. Trails-panel refresh wired via explicit callback — the watcher-event refresh path can't fire here because `core::trails::append_waypoint` suppresses the watcher for the trail-doc + waypoint-note paths to prevent indexer feedback loops. See resolved bug `bug-add-to-trail-verbs-dont-refresh-panel` (2026-05-10) · evidence: `app/src/files/sidebar.rs` (`FileVerb::AddToTrail`, `add_to_trail`)


## Creating a trail

A new trail comes from one of three entry points, all going through the same `create_trail` op:

- **Sidebar `+` button while in Trails mode** — left-click creates a new trail with a default name (`new-trail-N.md`, suffix-counted to avoid collision per the same `create_with_suffix` shape [[spec:sidebar-new-item-button]] already uses); the new trail-doc opens in the editor with inline-rename mode active so the user can name it before submitting. The trail is auto-set as active.
- **Sidebar `+` right-click cross-type picker** — picking "New trail" from the picker (per [[spec:sidebar-new-item-button]]) creates a trail regardless of current sidebar mode.
- **MCP `trail_create` tool** — agents can create trails as part of bookkeeping their investigations (per the MCP integration section below).

**Default placement.** New trails land at `<vault>/<new_trail_dir>/<name>.md`, where `new_trail_dir` is configurable. Config key `[trails] new_trail_dir = "trails/"` (default `"trails/"`, vault-scope eligible). The dir is auto-created on first trail. Setting `new_trail_dir = ""` (empty string) places trails at vault root. Users can move trail-docs anywhere in the vault later via filetree DnD — the placement is just a default; the move is an observed content-preserving move ([[spec:op-log-observed-move]]), and since the trail-doc's identity is its vault path the move just rewrites references, never re-mints the trail. [trails-default-location]
status:: done
note:: slice 2: backend behavior wired. Slice U3: UI `+` button now routes through the trails branch when in Trails mode; left-click creates a `new-trail.md` (suffix-counted), auto-activates it, opens it in the editor with inline-rename mode active per spec · evidence: `core/src/config.rs` (`TrailsConfig`) + `core/src/trails/ops.rs` (`create_trail` honors `new_trail_dir`, auto-creates folder, suffix-counts on collision) + `app/src/files/sidebar.rs` (new-item button routes through the trails branch in Trails mode: create → set-active → open + begin-inline-rename)
implements:: [[code:hiker/config/patch/ELIGIBLE_VAULT]], [[code:hiker/config/sections/TrailsConfig]], [[code:hiker/trails/ops/create_trail]]


## Capturing into a trail

When a capture fires (browser-extension Save-to-Hiker per [[spec:browser-extension-capture]], drag-URL, MCP `scrape` tool, share sheet) and an active trail is set, the capture is routed to that trail:

1. The source-derived note (or md note from a generic capture) lands at its normal location — `inbox/`, the source-derived sidecar dir, or a versioned-source manifest dir, depending on the source type per `design.md`'s source-derived-notes framing.
2. A new waypoint-note is created in the trail-doc's companion folder (`<trail>/<source-basename>--<short-id>.md`) with frontmatter linking to the source note (`hiker.references`) and the trail (`hiker.in_trail`).
3. The waypoint-note body is empty.
4. The trail-doc's frontmatter `hiker.waypoints` tree gets the new waypoint-note appended **at the root level** — captures don't auto-nest into side trails. Re-parenting is a frontmatter edit.
5. Sidebar refreshes; the new waypoint card is visible, collapsed.

The user can immediately click the new waypoint's edit affordance to write the annotation. [trail-capture-flow]
status:: partial
note:: append path routes a source path to the active trail via `core::trails::ops::append_waypoint`, no-op when no active trail. v1 has no capture entry points wired (no drag-URL flow today; browser-extension + MCP scrape + share sheet are future slugs), so there are no automatic callers yet — the next entry-point slug drops in without re-deriving the routing logic · evidence: `app/src/trails/bridge.rs` + `app/src/files/sidebar.rs` (append path to the active trail)

When **no active trail** is set, captures land in `inbox/` (or the source type's normal home) without creating any waypoint — the active-trail mode adds routing, never forces it.


## Building a trail while reading

For notes already in the vault — your own writing, prior captures, anything you're re-encountering — two surfaces append the current note as a waypoint on the active trail without going through capture. Both call the same `append_waypoint` op as the capture flow.

- **Tree right-click → "Add to active trail"** — context-menu verb on `.md` / `.txt` rows. Hidden on trail-docs, waypoint-notes, folders, and unsupported extensions. Disabled with a tooltip ("No active trail — pick one in the Trails sidebar") when no trail is active. The note doesn't have to be open in the editor; this is the "I'm browsing the tree and want to mark that one" shape. [trail-add-to-active-from-tree-verb]
- **Editor-pane "Add to trail" affordance** — when a regular note is open in the editor and a trail is active, a pill in the editor toolbar's right-side cluster (just left of Save) reads "Add to trail: <trail-name>" and clicks-through to the same op. Hidden when no trail is active, or when the open buffer is itself a trail-doc / waypoint-note / read-only preview. The pill's trail name doubles as the always-visible "which trail is active" indicator outside Trails mode. [trail-add-to-active-from-editor-verb]
status:: done
touches:: [[code:hiker/panels/buffer]], [[code:hiker/panels/properties]]
note:: "Add to trail: <name>" pill rendered in the right-side toolbar cluster, just left of Save; visible iff active trail set + active buffer is a file + path is `.md`/`.txt` + path isn't a trail-doc or waypoint-note (under `.hiker/trails/`); click → `core::trails::ops::append_waypoint` (via inline `block_on`, direct — user-explicit verb needs error visibility) + toast `"Added to <name>"`. Idempotency: membership set (paths-of-active-trail's-waypoints) populated via `get_trail` walking `ResolvedWaypoint.children` recursively (one `get_trail` per refresh covers every path at once); refreshed on active-trail change and any watcher file events under `.hiker/trails/` or against the active trail-doc. Post-append refresh wired via explicit callback — the watcher-event refresh path can't fire here because `core::trails::append_waypoint` suppresses the watcher for the trail-doc + waypoint-note paths. See resolved bug `bug-add-to-trail-verbs-dont-refresh-panel` (2026-05-10). `containing_note_with_paths` is the reverse-lookup helper (used by `app/src/panels/properties.rs` membership + the planned [[spec:mcp-tool-trails-containing-note]]) · evidence: `app/src/panels/buffer/mod.rs` ("Add to trail" pill in the right-side toolbar cluster, just left of Save, `add_to_trail_pill`) + `app/src/trails/sidebar.rs` (membership read + refresh) + `core/src/trails/{mod,ops}.rs` (`containing_note_with_paths`)
implements:: [[code:hiker/trails/ContainingNoteHit]], [[code:hiker/trails/containing_note_with_paths]]

Both verbs append under the trail's **append cursor** (see below) — the trail's "you are here" position. The cursor defaults to the root tail (consecutive adds extend the main line); the user moves it to a waypoint to branch into a side trail. Idempotency: if the note is already a waypoint at any depth in the trail, the verb is disabled with a tooltip ("Already in this trail") rather than duplicating. The check is per-trail, not per-vault — the same note can be a waypoint of multiple trails simultaneously (per `trails_containing_note`).


## Append cursor — branching the trail

[trail-append-cursor]
status:: done
note:: backend + app-wired: cursor lives in trail-doc frontmatter, resets on cascade-delete. Precedence: explicit `parent_waypoint_path: Some(_)` > cursor > root-tail. Stale-cursor (path no longer in tree) treated as None on read with a warn-log. App drives the cursor via `bridge::set_append_cursor` (core verb, block_on). UI surfaces ([[spec:trail-append-cursor-indicator]], [[spec:trail-append-from-here-verb]], [[spec:trail-reset-cursor-verb]]) read `TrailDetail.append_under` · evidence: `core/src/trails/{mod,ops}.rs` (`TrailDocFrontmatter.append_under`, `parse_trail_doc` reads `hiker.append_under`, `write_trail_doc_frontmatter` round-trips + strips on None, `append_waypoint` cursor-consult + stale-id `tracing::warn!` fallback to root, `remove_waypoint` cursor-reset when cursor in cascade set, `set_append_cursor` op, `TrailDetail.append_under`) + `app/src/trails/bridge.rs` (`set_append_cursor`)
implements:: [[code:hiker/trails/TrailDocFrontmatter#append_under]], [[code:hiker/trails/parse_trail_doc]], [[code:hiker/trails/write_trail_doc_frontmatter]], [[code:hiker/trails/TrailDetail#append_under]], [[code:hiker/trails/ops/append_waypoint]], [[code:hiker/trails/ops/remove_waypoint]], [[code:hiker/trails/ops/set_append_cursor]]
verifies:: [[code:hiker/trails/tests/cursor/set_cursor_writes_append_under]], [[code:hiker/trails/tests/cursor/reset_cursor_to_null_clears_append_under]], [[code:hiker/trails/tests/cursor/stale_cursor_target_warns_and_nulls]], [[code:hiker/trails/tests/cursor/append_lands_under_cursor_and_cursor_stays_put]], [[code:hiker/trails/tests/cursor/cursor_resets_when_removed_waypoint_cascades_through_it]]

The trail-doc carries a **position cursor** in frontmatter — `hiker.append_under: <waypoint-id> | null` — that names where the next append lands. `null` (or absent) means "append at the root tail" (zero migration for flat trails). When the cursor names a waypoint, `append_waypoint` places the new entry as the last child of that waypoint (creating or extending a side trail).

**Cursor stays put across appends.** The cursor only moves when the user explicitly moves it (right-click "Append from here") or resets it (header action) — appends never advance it, so a flat capture session never becomes an accidental ladder. Successive appends under the same cursor become *siblings* — `X.1`, `X.2`, `X.3` — a flat side trail under `X`. To dig into a sub-side-trail off `X.2`, the user right-clicks `X.2` to move the cursor; subsequent appends become `X.2.1`, `X.2.2`.

**Explicit `parent_waypoint_id` overrides the cursor.** `append_waypoint` with `parent_waypoint_id: Some(id)` (used by MCP and any explicit-parent caller) places under that parent; with `None` (the build-as-you-read verbs) it consults the cursor. Explicit > cursor; neither path moves the cursor.

**Cascade-delete safety.** When `remove_waypoint` cascades through the cursor's waypoint (or the cursor's waypoint itself is the target), the cursor resets to `null` in the same trail-doc rewrite. Reading a stale `append_under` that doesn't resolve to any waypoint in the tree (concurrent edit, hand-edited frontmatter pointing at a deleted id) is treated as `null` on read with a `tracing::warn!` — same posture as orphan waypoint refs.

### Indicator — "next append lands here"

[trail-append-cursor-indicator]
status:: done
note:: the cursor's waypoint card carries a user glyph (same as the recent-activity author pill) in accent color appended to the card head before the basename. Header hint row reads "Appending to main line" (cursor null OR stale path that doesn't resolve) or "Appending under <basename>" + Reset button (cursor resolves). Hint hidden when no active trail. Filetree intentionally not extended (per spec) · evidence: `app/src/trails/sidebar.rs` (cursor lookup off `TrailDetail.append_under`, cursor-hint row, cursor-indicator glyph on the waypoint card)

The cursor's waypoint card in the Trails sidebar gets a **little-person glyph** (the existing `Icons.user()` head-and-shoulders SVG, also used in the recent-activity author-pill) in the card head, just before the source-note basename, in the `--accent` color so the cursor's position is scannable at a glance. Non-cursor cards render unchanged.

When the cursor is `null` (root-tail append), no card carries the glyph. The Trails-mode header surfaces a subtle hint instead: a small text row reading `"Appending to main line"` (cursor null) or `"Appending under <basename>"` with a `"Reset to main line"` action button (cursor set). The header hint is the global "what's the next append going to do" signal; the per-card glyph is the locator.

Filetree integration is intentionally not extended to show the cursor — the file tree's job is browsing, not trail-position tracking. The Trails sidebar is the rich-render surface for trail-position concepts.

### Verbs

- **"Append from here" right-click verb on a waypoint card.** Adds an entry to the existing waypoint context menu (currently just "Remove waypoint"), above the Remove entry. Click sets `hiker.append_under` to that waypoint's id, rewrites the trail-doc, refreshes the sidebar; the little-person glyph moves to the clicked card. Available on every waypoint card including those nested in side trails (clicking lets the user branch off a digression). [trail-append-from-here-verb]
status:: done
note:: right-click any waypoint card (root-level or nested side-trail) → "Append from here" → `bridge::set_append_cursor(trail_doc_rel, Some(path))` → refresh + toast. Error path logs + toasts · evidence: `app/src/trails/sidebar.rs` ("Append from here" entry above "Remove waypoint" in the waypoint-card context menu)
- **"Reset to main line" header action.** Surfaces in the Trails-mode header alongside the cursor hint when the cursor is non-null. Click sets `hiker.append_under` to `null`; the next append goes to the root tail. Hidden when the cursor is already `null`. [trail-reset-cursor-verb]
status:: done
note:: "Reset to main line" action button surfaces in the Trails-mode header hint row whenever the cursor is non-null; hidden when cursor is null (hint reads "Appending to main line"). Click → `bridge::set_append_cursor(trail_doc_rel, None)` + refresh + toast; error path logs + toasts · evidence: `app/src/trails/sidebar.rs` (reset-cursor button in the cursor-hint row)

The two verbs are the only cursor-mutation surfaces in v1. The cursor field is also editable directly in the trail-doc frontmatter (it's a regular markdown note); a hand-edit triggers the same indexer / sidebar refresh path as any other trail-doc change.


## Indexer integration

A derived `trail_waypoints` table inside `index.db` supports fast lookups: which trails contain a given note, which waypoint-notes belong to a given trail, and where each waypoint sits in the trail's tree. Schema:

- `trail_waypoints (waypoint_path TEXT PRIMARY KEY, trail_doc_path TEXT, source_path TEXT, parent_waypoint_path TEXT NULL, tree_path TEXT)`
- `parent_waypoint_path` is `NULL` for root-level waypoints; otherwise the vault path of the parent waypoint.
- `tree_path` is a materialized path encoding depth-first position — `"1"`, `"1.2"`, `"1.2.1"`, etc. — so lexical ordering on `tree_path` gives reading order without a recursive query. Re-derived from the trail-doc's `hiker.waypoints` tree on every upsert; not load-bearing for correctness (the frontmatter is truth) but cheap for sidebar paint.
- Indexes on `trail_doc_path`, `source_path`, `parent_waypoint_path`.

Built and maintained by `core::indexer` like every other derived index — re-derived on schema bump, fail-loud per [[spec:store-version-fail-loud]]. The same index feeds the shared rename-rewriter ([[spec:wikilink-rename-rewrite]]) for the trail/waypoint side. [trail-waypoints-derived-table]
status:: done
note:: slice S1a: schema v6 + tree-aware re-derive landed · evidence: `core/src/store.rs` (schema v6, `trail_waypoints` table with `parent_waypoint_id` + `tree_path` + indexes incl. `trail_waypoints_parent_waypoint`, `WaypointRow`, `TrailContainingHit`, `upsert_trail_waypoint`, `delete_trail_waypoints_by_trail`, `delete_trail_waypoint_by_path`, `trails_containing_note`, `waypoints_of` ordered by `tree_path`, `rename_trail_waypoint_paths`) + `core/src/indexer.rs` (`update_trail_waypoints_if_relevant` clears + walks the recursive tree on trail-doc ingest, preserving the per-row `source_path`/`source_id`; per-waypoint ingest writes `(parent=NULL, tree_path="")` and the trail-doc walk fills the canonical values)
implements:: [[code:hiker/indexer/jobs/process_upsert]], [[code:hiker/indexer/jobs/impl#[`WaypointIngest<'a>`]rebuild_trail_doc_rows]], [[code:hiker/indexer/jobs/process_delete]], [[code:hiker/store/dto/WaypointRow]], [[code:hiker/store/dto/TrailContainingHit]], [[code:hiker/store/trails/impl#[Store]upsert_trail_waypoint]], [[code:hiker/store/trails/impl#[Store]delete_trail_waypoints_by_trail]], [[code:hiker/store/trails/impl#[Store]delete_trail_waypoint_by_path]], [[code:hiker/store/trails/impl#[Store]trails_containing_note]], [[code:hiker/store/trails/impl#[Store]waypoints_of]], [[code:hiker/store/trails/impl#[Store]rename_trail_waypoint_paths]]
verifies:: [[code:hiker/indexer/tests/ingesting_trail_doc_and_waypoint_populates_derived_table]], [[code:hiker/store/tests/trail_waypoints_insert_query_delete]], [[code:hiker/store/tests/rename_trail_waypoint_paths_rewrites_prefix]]


## Watcher integration

Trail-docs and waypoint-notes live in visible vault folders ([[spec:subsystem-notes-visible]] in `design.md`), so the watcher routes them to the indexer like any other md file — no `.hiker/` carve-out is involved.

The `core::trails` module owns watcher suppression around its own writes (create / append / remove) using the existing `Watcher::suppress` shape, so notify can't surface an event for a path the indexer has already routed.

There is **no `.hiker/trails/` watcher carve-out** — waypoints live in a visible vault folder ([[spec:subsystem-notes-visible]]), so none is needed. [trail-watcher-carve-out]
status:: done
touches:: [[code:hiker/watcher]]
note:: Retired — waypoints live in a visible vault folder ([[spec:subsystem-notes-visible]]), so no `.hiker/trails/` carve-out is needed. The carve-out (and the `.hiker/sessions/` one) was removed via `bug-watcher-drop-subsystem-carveouts` · evidence: `core/src/watcher.rs::is_ignored` — no per-subsystem carve-out; everything under `.hiker/` is ignored


## Trash integration

Deleting a trail-doc cascades to its companion folder: the trail-doc moves to `.hiker/trash/` and the entire `<trail>/` folder moves alongside it as a single atomic unit. Restoring the trail-doc from trash also restores the folder. Same `core::ops::delete` and `core::ops::restore` paths every other note uses, with the cascade enforced inside `core::trails::delete_trail` so restoring later puts the trail back in working order. [trail-delete-cascade]
status:: partial
note:: slice 2: cascade by routing two `core::ops::delete` calls (trail-doc + companion folder); v1 trade-off — two separate trash entries, user restores both manually. Atomic-pair semantics in `core::trash` deferred (TODO comment in `delete_trail`) · evidence: `core/src/trails/ops.rs` (`delete_trail`) + `app/src/trails/bridge.rs` (`delete_trail`)
implements:: [[code:hiker/trails/ops/delete_trail]]

Deleting a single waypoint-note via the sidebar's "Remove waypoint" verb moves only that waypoint-note to trash; the trail-doc's frontmatter is updated to drop the entry; the rest of the trail is untouched.


## Search integration

Trail-docs and waypoint-notes are indexed and searchable like any other md file in the vault — they show up in lexical and semantic search results, related-notes queries, and the full-text FTS5 index. [trail-searchable-as-notes]
status:: done
note:: trail-docs and waypoint-notes ride the standard md ingest path: indexed, FTS5-searchable, semantic-searchable, related-notes-eligible like any other md file. Once waypoints live in a visible companion folder ([[spec:trail-storage-layout]]) they index with no `.hiker/` carve-out. The per-kind filter pills (`trail` / `waypoint`) plug into the still-`planned` [[spec:search-source-type-filter]] slug when it lands · evidence: `core/src/indexer.rs` (`update_trail_waypoints_if_relevant`, indexable-extension routing)

The search panel's per-source-type filter ([[spec:search-source-type-filter]]) grows two new filterable kinds — `trail` (for trail-docs) and `waypoint` (for waypoint-notes) — alongside the existing `md` / `pdf` / `image` / `audio` / etc. The filter row reads from `hiker.kind` for these two kinds (since they're not source-derived) in addition to its existing `hiker.type` read for source types. Default-all-on, so a baseline search returns trail-docs + waypoints + regular notes mixed. The filter implementation lives under the existing [[spec:search-source-type-filter]] slug in `search.md`; trails just adds to the filterable kinds.


## MCP integration

Trails are a first-class MCP surface (read and write) so attached agents can consume curated trails as context and transcribe their investigations. All MCP writes go through `core::ops::agent_*` helpers, so each write carries the `Author::Agent(<client-id>)` class (surfaced on the git `Hiker-Author` trailer when git is integrated, `git.md`) and rides the normal op-log pending/patch-review path like any other agent write — uniform with the existing MCP write tools. [[spec:mcp-tool-toggles]] lets the user disable any individual trail tool.

### Read tools

- **`trails_list(filters?)`** — enumerate trails with optional filters (containing-note, recently-activated, name-substring). Returns trail-doc path + title + waypoint count + activation timestamp. [mcp-tool-trails-list]
- **`trail_get(id)`** — fetch a trail's full body + ordered waypoint list (each waypoint's source-note ref + annotation body). Detail levels (`digest` | `full`) mirror [[spec:mcp-tool-get-note]]'s shape. [mcp-tool-trail-get]
- **`trails_containing_note(rel_path)`** — reverse lookup; returns trails that include a given note as a waypoint. Useful for "what trails reference the note I'm reading?" [mcp-tool-trails-containing-note]

### Write tools

- **`trail_create(name)`** — create a new trail (empty waypoint list, default placement per `[trails] new_trail_dir`). Returns the new trail-doc's vault path (its identity). Agent-authored trails ride the normal op-log pending/patch-review path like any other agent write. [mcp-tool-trail-create]
- **`trail_append_waypoint(trail_id, source_rel, parent_waypoint_id?, annotation?)`** — append a new waypoint to a trail. Creates the waypoint-note in the trail-doc's companion folder, links to the source, and seeds the annotation with the optional `annotation` argument (omitted → empty body, the v1 capture-flow shape). When `parent_waypoint_id` is provided, the new waypoint is appended as the last child of that parent (a side-trail append); omitted → root-level append. [mcp-tool-trail-append-waypoint]
- **`trail_remove_waypoint(trail_id, waypoint_id)`** — remove a waypoint from a trail. Symmetric to the sidebar's "Remove waypoint" verb; cascades to descendants if the target has children, per the sidebar verb's cascade rule. [mcp-tool-trail-remove-waypoint]

Agent-authored trails are auditable two ways: the `Author::Agent(<client-id>)` class on every agent write to the trail-doc and its waypoint-notes (the git `Hiker-Author` trailer when git is integrated, `git.md`), plus the [[spec:mcp-audit-log-mcp-calls]] record of every trail tool call to `<vault>/.hiker/agent-log/<YYYY-MM-DD>.jsonl` — surface `mcp-tool-call`, feature `trails_list` / `trail_get` / `trail_create` / etc., redacted-by-default body content per `[mcp.audit] log_full_input`.

**No trails-specific draft mechanism.** Agent-created trails are plain trails that ride the normal op-log pending/patch-review path; the earlier `hiker.draft` flag and `.hiker/trails/drafts/` staging surface were removed. The MCP `trail_create` tool (when built per [[spec:mcp-tool-trail-create]]) creates plain trails through that path. [trail-draft-from-agent]
status:: retired
note:: drafts removed 2026-06-05; agent trails use the op-log pending path. Trail drafts (the `hiker.draft` flag, `.hiker/trails/drafts/`, `create_trail(draft)`, `default_draft_for_review_mode`) were deleted from `core::trails`; agent-created trails ride the normal op-log pending/patch-review path like any other agent write. The MCP `trail_create` tool (when built per [[spec:mcp-tool-trail-create]]) creates plain trails through that path

A clustering-proposed trail likewise emits a plain `create_trail` + ordered `append_waypoint`s on the op-log pending path rather than a draft; the conservative reading-order detector + `[clustering] propose_trails` config survive as the gate. [trail-draft-from-clustering]
status:: retired
implements:: [[code:hiker/cluster/ReadingOrderChain]], [[code:hiker/cluster/detect_reading_order_chain]], [[code:hiker/config/sections/ClusteringConfig]], [[code:hiker/config/sections/ClusteringConfig#propose_trails]], [[code:hiker/config/Config#clustering]]
touches:: [[code:hiker/cluster]]
note:: drafts removed 2026-06-05. The conservative reading-order detector + `[clustering] propose_trails` config survive as the gate, but a fired chain now emits a plain `create_trail` + ordered `append_waypoint`s that ride the op-log pending path (no `create_trail(draft=true)`). Still not wired into the live streaming build pipeline (`core::cluster::build::stream`)

Agent/clustering-proposed trails are reviewed through the **shared op-log pending surfaces** ([[spec:staging-accept-reject-from-tree]]/`-editor`/`-chat-card`), not a trails-specific draft review surface. [trail-draft-review-surface]
status:: retired
note:: drafts removed 2026-06-05; agent trails use the op-log pending path. The `hiker.draft` flag, `list(include_drafts)`/`TrailListItem.draft`, `accept_draft`, `reject_draft`, and the `.hiker/trails/drafts/` staging surface were all deleted from `core::trails`. Agent/clustering-proposed trails are reviewed through the shared op-log pending surfaces ([[spec:staging-accept-reject-from-tree]]/`-editor`/`-chat-card`), not a trails-specific draft surface


## Out of scope (v1)

- **Drag-to-reorder waypoints in the sidebar.** Reorder via trail-doc frontmatter edit only. The use case is rare and the linking-metadata invariants are easier to keep with a read-only sidebar.
- **In-place annotation editing in the sidebar.** Open the waypoint-note in the editor instead.
- **"Follow side trail" reading-position mode.** A reading-position concept where entering a side trail collapses the main line and "back up" returns is a richer memex-shape but introduces stateful UI the rest of the sidebar doesn't have. v1 renders everything expanded with indents. The [[spec:trail-append-cursor]] indicator gives a subset of this for free (a single "you are here" glyph on the cursor card) without the collapse-and-restore reading-mode state.
- **Tab-strip waypoint chips / "3/7" indicators on buffer tabs.** When an active trail's waypoint is open in a buffer tab, the tab doesn't currently surface its trail position. Possible future polish; not v1.
- **Stepper-only mode** (`← prev | trail name | next →` row above the editor).
- **Trail-as-document live-preview that renders waypoint refs inline as mini-cards** in the trail-doc body. The body and the waypoint list are intentionally independent in v1.
- **"On trails: …" row in the discovery panel** showing trails that include the active note. Useful future affordance; not v1.
- **Cross-vault trail interop.** Trails are vault-scoped; paths aren't unique across vaults. Bush's "reproduce a trail and hand to a friend" is a future concern once cross-vault identity is solved (or punted via export-as-bundle).


## Deferred

- **Auto-generated trails from LLM tool-use trace.** Audit-shaped: when an agent answers a query, the set of `get_note` / `read_note` / search tool calls during that turn becomes a candidate trail showing the agent's reasoning path. The candidate trail rides the normal op-log pending/patch-review path like any other agent write — user accepts, edits, or discards through the same review flow. Pairs naturally with [[spec:chat-tool-call-opens-touched-note]] since both surface the agent's note-touching trail. Lands once basic trails are solid in real use. [trail-auto-from-llm-trace]
status:: planned
note:: deferred — auto-generate a candidate trail from an agent's tool-use trace during a query turn; rides the normal op-log pending/patch-review path like any other agent write (user accepts/edits/discards); pairs with [[spec:chat-tool-call-opens-touched-note]]
- **Filename color toggle for active-trail waypoints.** Optional toggle (default off) that colorizes the filename of any note that's a waypoint of the *currently active* trail in the file tree, so trail membership is scannable from the tree without opening the Trails sidebar mode. Lives under the View menu when it lands. [trail-active-waypoint-filename-color]
status:: planned
note:: deferred polish — toggle (default off) colorizes filenames in the file tree of notes that are waypoints of the active trail
- **CLI surface.** `hiker trail list / show / activate / new` for shell-script-driven workflows; rides the broader `cli-*` family in `status.md`. [cli-trail-list, cli-trail-show, cli-trail-activate, cli-trail-new]
- **Active-trail indicator outside Trails mode.** A small chip in the editor toolbar or status bar showing which trail is active when sidebar is in Files / Cluster trees mode. Useful if capture-into-wrong-trail becomes a real footgun in v1 use; revisit then.
- **Graph view of trails.** Multi-trail visualization on a note-graph; rides the v4+ graph view feature per `design.md`'s graph-view bullet. Trails are one of two consumers (the other being wikilinks once they exist), not the owner.
