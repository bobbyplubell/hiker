# Vault view

A second sidebar mode that presents the vault as a **logical lens** over its notes — grouping and nesting by relationship and metadata — alongside the literal on-disk **Files** tree. It exists because source ingestion (`extract.md`) deliberately abstracts the filesystem: sidecars sit beside binaries, crawled pages carry a logical parent that isn't their folder, versioned sources hide artifacts under `.hiker/`. Files mode shows where bytes live; Vault mode shows how the knowledge is organized.

Goal: open the sidebar in Vault mode and see crawl jobs with their captured pages nested under them, sidecars under the sources they derive from, and source-type/provenance groups — without any of it moving a file.

The headline decisions:

- **Vault is a new sidebar mode**, switched via the existing `sidebar-mode-switcher` (Files / Cluster trees / Trails / Vault). Files mode is unchanged and stays the default. [vault-view-mode]
- **Vault mode is a read-only lens, never a second source of truth.** Its structure is *derived* from frontmatter + the index; it never moves or renames files. The filesystem remains authoritative for placement, exactly as `design.md` and `clustering.md` insist. [vault-view-readonly-lens]
- **Crawl jobs render with their captured pages nested** via `hiker.parent`, regardless of where the pages physically sit. [vault-view-crawl-nesting]
- **Source sidecars surface under their originals** instead of being hidden as they are in Files mode. [vault-view-sidecar-surfacing]
- **Source-type / provenance virtual groups** organize the rest — the realized, mode-scoped form of `tree-source-visibility-toggles`. [vault-view-source-groups]


## Mode integration

Vault is the fourth entry in the sidebar's uniform mode-switcher row (`sidebar-mode-switcher`), pressed-state like the others; clicking switches the sidebar body in place, leaving the editor pane and discovery panel untouched. Mode persists per-vault under the existing `vault.sidebar_mode` key. The pinned Trash bin and the `+` / `⋯` row stay, their behavior mode-aware as elsewhere: `+` defaults to a new note (cross-type picker on right-click), `⋯` hosts Vault-mode actions (the lens/grouping choice below). [vault-view-mode]


## What the lens shows

Vault mode renders a tree whose nodes are notes and whose nesting comes from metadata, not directories. The default lens composes three groupings:

- **Crawl jobs as parents.** A crawl-job manifest note (`crawl-job-note`) renders with every page that stamped `hiker.parent: <job-ulid>` nested beneath it. Re-crawls don't change the shape — the parent link is stable. This is the presentation half of `crawl-child-parent`. [vault-view-crawl-nesting]
- **Sidecars under sources.** A non-md source and its extracted-text sidecar (`extract-sidecar-write`) render as one entry — the source with its sidecar nested — rather than the sidecar being hidden (`extract-sidecar-tree-hidden`, which is the Files-mode default). Opening either is one click away. [vault-view-sidecar-surfacing]
- **Source-type / provenance groups.** Remaining notes group under virtual top-level nodes by source type and the authorship trichotomy (`user-authored` / `agent-authored` / `imported`) read from `hiker.author` / `hiker.provenance` (`design.md` provenance axis). This is `tree-source-visibility-toggles` realized as Vault-mode grouping rather than Files-mode show/hide. [vault-view-source-groups]

The grouping is selectable from the `⋯` menu (e.g. group-by-source-type vs group-by-crawl vs flat); the default is the composed lens above. The chosen lens is display state, not stored on any note.


## The read-only rule

Vault mode shows structure; it does not own it. [vault-view-readonly-lens]

- **No moves.** There is no drag-to-reorganize in Vault mode — dragging a note onto a different group or parent would imply a filesystem move the lens has no authority to make. Reorganizing files is a Files-mode (and `move_note`) action. Nesting here is derived from `hiker.parent` / source-type / provenance, full stop.
- **Item actions still work.** Click opens the note (preview-slot semantics per `editor-preview-tab-from-open-callsites`); context verbs (open, view original, view extracted text, the trail/crawl verbs) apply to the focused note exactly as in Files mode.
- **Derived, so always correct.** The tree is recomputed from frontmatter + the index, so it can't drift from truth — there is no separate organizational state to keep in sync, no `hiker.placement` field. A note appears under a crawl job because its frontmatter says so, not because Vault mode remembered a drag.


## Relationship to Files mode

Files mode is the literal on-disk tree and stays the ground truth: it shows real paths, hides sidecars by default, and is where renames/moves/drag-and-drop happen. Vault mode is the lens for *reading* the vault's structure. The same note is reachable from both — at its path in Files, under its logical parent/group in Vault. Neither is canonical over the other for *navigation*; for *placement*, the filesystem (Files) always wins.


## Deferred

- **Trails / cluster membership as lens groupings** — surfacing trail and cluster membership as additional Vault-mode groupings, once those cross-reference surfaces are worth folding into one view. [vault-view-membership-groups]
- **Saved custom lenses** — user-defined grouping rules (by tag facet, by folder glob, by frontmatter field) saved and re-selectable. [vault-view-saved-lenses]


## Out of scope

- **Reorganizing the vault.** Vault mode never moves files; structural edits live in Files mode and `move_note`. A lens is not an organizer.
- **A third storage model.** Vault mode persists nothing on notes — no placement field, no per-note view state. Everything it shows is derived.
