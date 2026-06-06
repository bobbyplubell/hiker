# Vault view

A second sidebar mode that presents the vault as a **logical lens** over its notes — grouping and nesting by relationship and metadata — alongside the literal on-disk **Files** tree. It exists because source import (`import.md`) deliberately abstracts the filesystem: sidecars sit beside binaries, imported items carry a logical parent that isn't their folder, versioned sources hide artifacts under `.hiker/`. Files mode shows where bytes live; Vault mode shows how the knowledge is organized.

Goal: open the sidebar in Vault mode and see imported multi-item sources with their items nested under them, sidecars under the sources they derive from, and source-type/provenance groups — without any of it moving a file.


## Mode integration

Vault is the fourth entry in the sidebar's uniform mode-switcher row (`sidebar-mode-switcher`), pressed-state like the others; clicking switches the sidebar body in place, leaving the editor pane and discovery panel untouched. Mode persists per-vault under the existing `vault.sidebar_mode` key. The pinned Trash bin and the `+` / `⋯` row stay, their behavior mode-aware as elsewhere: `+` defaults to a new note (cross-type picker on right-click), `⋯` hosts Vault-mode actions (the lens/grouping choice below). [vault-view-mode]


## v1 status

The mode, the read-only lens contract, the registry/activity-bar wiring, and the lens-picker are built. v1 ships the two groupings derivable from data that exists today — **by top-level folder** (flattened within each, distinct from Files' fully-nested tree) and **flat (all notes)** — both read from the index's note paths. The three richer groupings below (import nesting, sidecar surfacing, source-type/provenance) depend on `import.md` (unbuilt) and a provenance index column; their dispatch slots are reserved and light up when that data lands.

## What the lens shows

Vault mode renders a tree whose nodes are notes and whose nesting comes from metadata, not directories. The default lens composes three groupings:

- **Imported sources as parents.** An imported multi-item source (a crawled site, a feed — `import.md`) renders with every child that stamped `hiker.parent: <note-ulid>` nested beneath it, since the import lays children into a companion folder carrying the parent stamp. The nesting reads the stamp, not the physical `<note-name>/` companion folder (`note-companion-folder`), so a stray file isn't a false child. Re-imports don't change the shape — the parent link is stable. [vault-view-crawl-nesting]
- **Trail waypoints under their trail-doc.** A trail-doc (`trail-doc-shape`) renders with its waypoints nested by the `hiker.waypoints` tree — preserving order and side-trail branching, not the flat companion-folder layout — so Vault mode is the clean reading view of a trail that Files mode's folder can't give. [vault-view-trail-nesting]
- **Sidecars under sources.** A non-md source and its extracted-text sidecar (`extract-sidecar-write`) render as one entry — the source with its sidecar nested — rather than the sidecar being hidden (`extract-sidecar-tree-hidden`, which is the Files-mode default). Opening either is one click away. [vault-view-sidecar-surfacing]
- **Source-type / provenance groups.** Remaining notes group under virtual top-level nodes by source type and the authorship trichotomy (`user-authored` / `agent-authored` / `imported`) read from `hiker.author` / `hiker.provenance` (`design.md` provenance axis). Chat sessions (`chat-session-markdown-store`) group here as a "Sessions" bucket (imported sessions as their own sub-bucket), labelled by session title + date rather than the on-disk filename. [vault-view-source-groups]

The grouping is selectable from the `⋯` menu (e.g. group-by-source-type vs group-by-crawl vs flat); the default is the composed lens above. The chosen lens is display state, not stored on any note.


## Rich row previews

A row whose note is a cluster tree (`hiker.kind: cluster-tree`) gets a small
inline force-directed preview thumbnail before its label, hover-expandable into
a larger floating preview that never occludes the rows below it. The thumbnail
is the reusable rich-preview widget — a domain plugs in a provider and the widget
owns rendering, the `.hiker/previews/` cache, and the side-anchored
non-interactable hover-expand. Full surface (the `ThumbnailProvider`
abstraction, the canvas / tree renderers, the hover-expand, the cache) in
`previews.md`. [vault-view-row-previews]


## The read-only rule

Vault mode shows structure; it does not own it. [vault-view-readonly-lens]

- **No moves.** There is no drag-to-reorganize in Vault mode — dragging a note onto a different group or parent would imply a filesystem move the lens has no authority to make. Reorganizing files is a Files-mode (and `move_note`) action. Nesting here is derived from `hiker.parent` / source-type / provenance, full stop.
- **Item actions still work.** Click opens the note (preview-slot semantics per `editor-preview-tab-from-open-callsites`); context verbs (open, view original, view extracted text, the trail/crawl verbs) apply to the focused note exactly as in Files mode.
- **Derived, so always correct.** The tree is recomputed from frontmatter + the index, so it can't drift from truth — there is no separate organizational state to keep in sync, no `hiker.placement` field. A note appears under a crawl job because its frontmatter says so, not because Vault mode remembered a drag.


## Relationship to Files mode

Files mode is the literal on-disk tree and stays the ground truth (real paths, sidecars hidden, renames/moves/drag-and-drop). Vault mode is the lens for *reading* structure. The same note is reachable from both — at its path in Files, under its logical parent/group in Vault. Neither is canonical for *navigation*; for *placement*, the filesystem always wins.


## Deferred

- **Trails / cluster membership as lens groupings** — surfacing trail and cluster membership as additional Vault-mode groupings, once those cross-reference surfaces are worth folding into one view. [vault-view-membership-groups]
- **Saved custom lenses** — user-defined grouping rules (by tag facet, by folder glob, by frontmatter field) saved and re-selectable. [vault-view-saved-lenses]


## Out of scope

Reorganizing the vault (structural edits live in Files mode and `move_note`) and any third storage model — Vault mode persists nothing on notes (`vault-view-readonly-lens`).
