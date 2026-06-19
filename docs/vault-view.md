# Vault view

A second sidebar mode that presents the vault as a **logical lens** over its notes — grouping and nesting by relationship and metadata — alongside the literal on-disk **Files** tree. It exists because source import (`import.md`) deliberately abstracts the filesystem: sidecars sit beside binaries, imported items carry a logical parent that isn't their folder, versioned sources hide artifacts under `.hiker/`. Files mode shows where bytes live; Vault mode shows how the knowledge is organized.

Goal: open the sidebar in Vault mode and see imported multi-item sources with their items nested under them, sidecars under the sources they derive from, and source-type/provenance groups — without any of it moving a file.


## Mode integration

Vault is the fourth entry in the sidebar's uniform mode-switcher row ([[spec:sidebar-mode-switcher]]), pressed-state like the others; clicking switches the sidebar body in place, leaving the editor pane and discovery panel untouched. Mode persists per-vault via the workbench panel-set layout (`app/src/side_panel_persist.rs`). The pinned Trash bin and the `+` / `⋯` row stay, their behavior mode-aware as elsewhere: `+` defaults to a new note (cross-type picker on right-click), `⋯` hosts Vault-mode actions (the lens/grouping choice below). [vault-view-mode]
status:: partial
implements:: [[code:hiker/vault_view/actions_menu]]
note:: `app/src/vault_view/` + `HikerMode::Vault` + `PANEL_VAULT` + the `vault` `Feature` (registry entry, safe-dial `Vault` icon). Sidebar mode reachable from the activity bar; `⋯` hosts the lens picker. **Gap:** mode choice not yet persisted (resets to Files on relaunch)


## v1 status

The mode, the read-only lens contract, the registry/activity-bar wiring, and the lens-picker are built. v1 ships the two groupings derivable from data that exists today — **by top-level folder** (flattened within each, distinct from Files' fully-nested tree) and **flat (all notes)** — both read from the index's note paths. The three richer groupings below (import nesting, sidecar surfacing, source-type/provenance) depend on `import.md` (unbuilt) and a provenance index column; their dispatch slots are reserved and light up when that data lands.

## What the lens shows

Vault mode renders a tree whose nodes are notes and whose nesting comes from metadata, not directories. The default lens composes three groupings:

- **Imported sources as parents.** An imported multi-item source (a crawled site, a feed — `import.md`) renders with every child that stamped `hiker.parent: <note-ulid>` nested beneath it, since the import lays children into a companion folder carrying the parent stamp. The nesting reads the stamp, not the physical `<note-name>/` companion folder ([[spec:note-companion-folder]]), so a stray file isn't a false child. Re-imports don't change the shape — the parent link is stable. [vault-view-crawl-nesting]
status:: done
implements:: [[code:hiker/vault_view/tree/build_composed]]
note:: `app/src/vault_view/tree.rs:build_crawl_nodes` nests every note whose `hiker.parent` stamp resolves to a capture/manifest note's id under that note (`NodeKind::Capture`); a folder-mate with **no** stamp, or a dangling stamp, stays a normal node (tested: `crawl_children_nest_by_parent_stamp`, `dangling_parent_stamp_is_not_nested`). Stamp authority, not folder membership; data read once via `Store::notes_with_meta`
- **Trail waypoints under their trail-doc.** A trail-doc ([[spec:trail-doc-shape]]) renders with its waypoints nested by the `hiker.waypoints` tree — preserving order and side-trail branching, not the flat companion-folder layout — so Vault mode is the clean reading view of a trail that Files mode's folder can't give. [vault-view-trail-nesting]
status:: done
implements:: [[code:hiker/store/trails/impl#[Store]all_trail_waypoints]], [[code:hiker/vault_view/tree/build_composed]]
note:: `app/src/vault_view/tree.rs:build_trail_nodes`+`nest_waypoints` nests a trail-doc's waypoints by the resolved `trail_waypoints` tree (`parent_waypoint_id` + `tree_path`, re-derived from `hiker.waypoints` on ingest), preserving order + side-trail branching (tested: `trail_waypoints_nest_with_side_trail_order`). Whole table read once via `Store::all_trail_waypoints`
- **Sidecars under sources.** A non-md source and its extracted-text sidecar (`extract-sidecar-write`) render as one entry — the source with its sidecar nested — rather than the sidecar being hidden (`extract-sidecar-tree-hidden`, which is the Files-mode default). Opening either is one click away. [vault-view-sidecar-surfacing]
status:: done
implements:: [[code:hiker/vault_view/tree/build_composed]]
note:: `app/src/vault_view/tree.rs:build_sidecar_nodes` detects `<src>.<ext>.md` sidecars (`sidecar_source`) and renders the non-md source as a synthetic node with the sidecar nested (tested: `sidecar_source_detection`, `sidecar_pairs_with_synthetic_source`). **GAP:** synthetic source node opens the source path via `editor_pane::open_file` rather than a Files-mode reveal verb — fine for indexed sidecars, "view original" verb is followup
- **Source-type / provenance groups.** Remaining notes group under virtual top-level nodes by source type and the authorship trichotomy (`user-authored` / `agent-authored` / `imported`) read from `hiker.author` / `hiker.provenance` (`design.md` provenance axis). Chat sessions ([[spec:chat-session-markdown-store]]) group here as a "Sessions" bucket (imported sessions as their own sub-bucket), labelled by session title + date rather than the on-disk filename. [vault-view-source-groups]
status:: done
implements:: [[code:hiker/store/dto/NoteMetaRow]], [[code:hiker/store/metadata/impl#[Store]notes_with_meta]], [[code:hiker/vault_view/tree/build_composed]]
note:: `app/src/vault_view/tree.rs:build_source_groups` groups leftover notes by the authorship trichotomy (`hiker.author`/`hiker.provenance` via `author_bucket`); chat sessions collapse into a "Sessions" bucket with an "Imported" sub-bucket, labelled by date+id (`session_label`) not filename (tested: `source_groups_split_by_authorship_and_sessions`). **NOT blocked on a new index column** — the existing `note_meta` index (v8, [[spec:store-note-metadata-index]]) already holds `hiker.author`/`provenance`/`parent`/`kind`; the lens reads them in one query (`core/src/store/metadata.rs:notes_with_meta`), no per-note disk reads, no schema bump

The grouping is selectable from the `⋯` menu (e.g. group-by-source-type vs group-by-crawl vs flat); the default is the composed lens above. The chosen lens is display state, not stored on any note.

Query-docs additionally render as virtual smart folders alongside these groupings — read-only, visually marked membership per [[spec:smart-folder-view]] (`queries.md`).


## Rich row previews

A row whose note is a cluster tree (`hiker.kind: cluster-tree`) gets a small
inline force-directed preview thumbnail before its label, hover-expandable into
a larger floating preview that never occludes the rows below it. The thumbnail
is the reusable rich-preview widget — a domain plugs in a provider and the widget
owns rendering, the `.hiker/previews/` cache, and the side-anchored
non-interactable hover-expand. Full surface (the `ThumbnailProvider`
abstraction, the canvas / tree renderers, the hover-expand, the cache) in
`previews.md`. [vault-view-row-previews]
status:: done
implements:: [[code:hiker/vault_view/row_tree_thumbnail]]
note:: `app/src/vault_view/mod.rs:render_node`+`row_tree_thumbnail` renders the reusable force-directed preview thumbnail (`widgets::preview::thumbnail`) before the label of any `hiker.kind: cluster-tree` row; tree id is the note's filename stem, nodes loaded read-only via `trees.list_nodes`. Canvas thumbnails wire into the canvases activity (`.canvas` files aren't indexed, so they don't surface in the Vault lens). Full preview surface in `previews.md`


## The read-only rule

Vault mode shows structure; it does not own it. [vault-view-readonly-lens]
status:: done
touches:: [[code:hiker/vault_view]]
note:: `app/src/vault_view/mod.rs` renders a read-only tree — click opens (preview-slot / Mod-click sticky), no drag-to-reorganize, no placement state stored. Lenses: `Composed` (default, the metadata-derived groupings below) + by-folder (flattened) + flat, picked from `⋯`

- **No moves.** There is no drag-to-reorganize in Vault mode — dragging a note onto a different group or parent would imply a filesystem move the lens has no authority to make. Reorganizing files is a Files-mode (and `move_note`) action. Nesting here is derived from `hiker.parent` / source-type / provenance, full stop.
- **Item actions still work.** Click opens the note (preview-slot semantics per [[spec:editor-preview-tab-from-open-callsites]]); context verbs (open, view original, view extracted text, the trail/crawl verbs) apply to the focused note exactly as in Files mode.
- **Derived, so always correct.** The tree is recomputed from frontmatter + the index, so it can't drift from truth — there is no separate organizational state to keep in sync, no `hiker.placement` field. A note appears under a crawl job because its frontmatter says so, not because Vault mode remembered a drag.


## Relationship to Files mode

Files mode is the literal on-disk tree and stays the ground truth (real paths, sidecars hidden, renames/moves/drag-and-drop). Vault mode is the lens for *reading* structure. The same note is reachable from both — at its path in Files, under its logical parent/group in Vault. Neither is canonical for *navigation*; for *placement*, the filesystem always wins.


## Deferred

- **Trails / cluster membership as lens groupings** — surfacing trail and cluster membership as additional Vault-mode groupings, once those cross-reference surfaces are worth folding into one view. [vault-view-membership-groups]
status:: planned
note:: deferred — trail / cluster membership as additional Vault-mode groupings
- **Saved custom lenses** — user-defined grouping rules (by tag facet, by folder glob, by frontmatter field) saved and re-selectable. [vault-view-saved-lenses]
status:: planned
note:: deferred — user-defined grouping rules (tag facet / folder glob / frontmatter field) saved and re-selectable


## Out of scope

Reorganizing the vault (structural edits live in Files mode and `move_note`) and any third storage model — Vault mode persists nothing on notes ([[spec:vault-view-readonly-lens]]).
