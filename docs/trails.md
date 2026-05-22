# Trails

Curated walks through a vault — a memex-style first-class concept where each waypoint is its own note carrying the user's annotation and a double-link back to the source the waypoint is about. Trails ride the existing markdown / indexer / watcher / changes / trash machinery so they're searchable, editable, syncable, and backup-able like any other note.

The headline decisions:

- **A trail-doc is a regular markdown note in the vault.** Frontmatter holds the trail's metadata (id, activation timestamp, waypoint tree); body is freeform README-shape prose the user authors. The body is *not* auto-generated from waypoints — the sidebar renders the walk; the body is for the trail's framing. [trail-doc-shape]
- **Each waypoint is its own markdown note** at `<vault>/.hiker/trails/<trail-id>/waypoints/<source-basename>--<short-id>.md`. Body is empty by design — a clean canvas the user fills with their commentary about the source. Frontmatter double-links to the source note and the trail. Order and tree position live in the trail-doc's frontmatter, not in filenames. [waypoint-note-shape, trail-storage-layout, trail-empty-waypoint-body]
- **Every reference is a double-link** `{ id: <ulid>, path: <rel-path> }`. ULID is the canonical pointer that survives renames; rel-path is the externally-interoperable half so a trail-doc opened in any other markdown editor stays legible. [trail-double-link-references]
- **Trails branch.** A waypoint can have child waypoints forming a side trail; the trail-doc's waypoint list is a tree, not a flat list. Side trails render nested under their parent in the sidebar. The reader walks the main line, drops down a side trail to follow a digression, and walks back up — the Bush memex shape. [trail-side-trail-shape, trails-mode-side-trail-render]
- **Trails are user-curated, not strictly user-authored.** The user owns every accepted trail. The clustering pipeline and MCP agents may *propose* draft trails which land in a review queue; the user accepts (with optional edits) or discards. Accepted drafts become normal trails. The proposal mechanic mirrors the shape `suggestions.md` uses for reorganization proposals, scoped to trails. [trail-draft-review-surface, trail-draft-from-agent, trail-draft-from-clustering]
- **Build-as-you-read verbs.** Two surfaces append the current note as a waypoint on the active trail without going through capture: a tree right-click "Add to active trail" verb on note rows, and an editor-pane affordance when a regular note is open and a trail is active. The reading-loop gesture has a native handle, not just the external-source ingest path. [trail-add-to-active-from-tree-verb, trail-add-to-active-from-editor-verb]
- **MCP exposes trails for read and write.** Agents read trails to consume curated context and write trails to transcribe their own investigations as draft trails (gated by the standard `agent-write-review-mode` setting). [mcp-tool-trails-list, mcp-tool-trail-create, mcp-tool-trail-append-waypoint]
- **The Trails sidebar mode shows the active trail vertically, top-to-bottom, read-only.** The dropdown at the top selects which trail is active; the trail-head icon next to it jumps to the trail-doc. The editor stays untouched — clicking a waypoint opens the source note in the editor pane on the right. [trails-mode-body, trails-mode-active-trail-dropdown]


## Storage layout

A trail is a *trail-doc* plus a hidden subsystem dir of *waypoint-notes*:

```
<vault>/
├── trails/                              # default new-trail location (configurable)
│   └── my-trail.md                      # trail-doc — regular md note
└── .hiker/
    └── trails/
        └── <trail-id>/
            └── waypoints/
                ├── raptor-paper--7K2A9F.md
                ├── embedding-survey--3Q8M1B.md
                ├── inline-citation--5R7Z2D.md       # child of raptor-paper
                └── scratchpad--9X4N6C.md
```

The trail-doc lives at a user-chosen vault location (default `trails/`, configurable). Its hidden waypoint directory lives under `.hiker/trails/<trail-id>/waypoints/` and follows the same carve-out shape `chat-session-markdown-store` already uses for `.hiker/sessions/`. The waypoint-note basename is `<source-basename>--<short-id>.md` where `short-id` is a 6-char suffix of the waypoint's ULID; collisions across trails are impossible since each trail gets its own dir, and the short-id disambiguates two waypoints that point at the same source within one trail. [trail-storage-layout]

The waypoint dir is **flat regardless of tree depth** — side trails don't get nested directories. The trail-doc's `hiker.waypoints` frontmatter is the only source of truth for both *order* and *tree shape*. Filenames are stable identifiers, not position encoders: reordering, re-parenting, or moving waypoints between depths never renames a file. The tradeoff is that `ls`-ing the dir gives you no reading order — the sidebar (and the trail-doc itself) is the right surface for that.


## Trail-doc shape

Frontmatter:

```yaml
---
hiker:
  kind: trail
  id: <ulid>
  last_activated_at: <iso8601>          # drives Trails-mode dropdown ordering
  waypoints:                            # ordered tree of double-links
    - id: <waypoint-note ulid>
      path: ".hiker/trails/<trail-id>/waypoints/raptor-paper--7K2A9F.md"
      waypoints:                        # optional — children form a side trail
        - id: <waypoint-note ulid>
          path: ".hiker/trails/<trail-id>/waypoints/inline-citation--5R7Z2D.md"
    - id: <waypoint-note ulid>
      path: ".hiker/trails/<trail-id>/waypoints/embedding-survey--3Q8M1B.md"
---
```

`hiker.waypoints` is a tree: each entry is a double-link to a waypoint-note and may carry its own `waypoints:` array of child entries forming a side trail. Children are themselves trees — side trails nest arbitrarily deep. A waypoint with no `waypoints:` key (or an empty array) is a leaf; the common case for v1 is a flat tree where most or all waypoints sit at the root and side trails appear only where the user has explicitly digressed. [trail-side-trail-shape]

Body is freeform markdown — a hand-authored README for the trail. The user writes whatever framing they want here: why the trail exists, what it covers, where it ends, who they're sharing it with. The body is *not* auto-generated and never overwritten by hiker; the sidebar already renders the waypoint walk, the body is for the trail's prose framing, separate from the per-waypoint annotations (which live in each waypoint-note's body). [trail-doc-shape]

The trail-doc must have a `.md` extension to be recognized as a trail. A note carrying `hiker.kind: trail` in frontmatter but a non-`.md` extension is treated as a regular note — the discriminator alone isn't enough.


## Waypoint-note shape

Frontmatter:

```yaml
---
hiker:
  kind: waypoint
  id: <ulid>                            # waypoint-note's own stable id
  references:                           # double-link to the source note
    id: <source-note ulid>
    path: "research/raptor-paper.md"
  in_trail:                             # double-link to the trail-doc
    id: <trail-doc ulid>
    path: "trails/my-trail.md"
---
```

Body is the user's annotation — empty when the waypoint is created (clean canvas), filled in by the user as they author commentary. Standard markdown; renders in the editor like any other note when opened directly. [waypoint-note-shape, trail-empty-waypoint-body]

The two-hop structure (trail-doc → waypoint-note → source-note) means each waypoint is searchable, editable, addressable for cross-references, and visible in the autosave / changes / trash pipelines like any other note in the vault.


## Reference shape

Every reference from a trail-doc to a waypoint-note (and from a waypoint-note to its source note) is a **double-link**: both the **ULID** and the **rel-path** of the target. [trail-double-link-references]

```yaml
{ id: "01HRX...", path: "research/raptor-paper.md" }
```

ULID is the canonical pointer. The indexer's `path-ids` lookup (per `index.md`) guarantees stability across renames. Rel-path is the externally-interoperable half — when a user opens the trail-doc in any other markdown editor (Obsidian, vscode, plain `cat`), the path lets them navigate without consulting hiker's sqlite db. Locking trails to ULIDs alone would tie them to hiker; the rel-path keeps trails legible everywhere.

**Resolution rule** when hiker reads a reference:

- Both match → resolved.
- ULID resolves but rel-path differs → ULID wins; trail-doc's path field is rewritten to the current path (self-healing). One `core::changes` row appended for the rewrite, `author='user'` since the move that caused the drift was user-initiated.
- Rel-path matches an indexed note but the ULID matches a *different* note (rare — happens when a note was deleted and recreated, or a copy landed at a prior path) → **confirm modal** surfaces: "This reference's recorded note has changed identity at `<path>`. Keep the old reference (orphan) / Repoint to the current note at this path / Break the reference (delete the waypoint)?" The modal pauses resolution until the user decides. Logged either way for audit. [trail-path-conflict-modal]
- Neither matches → orphaned. Surfaced visually in the sidebar (greyed card with "broken reference" pill); waypoint stays in the trail so the user decides whether to delete or fix. [trails-mode-orphan-card]

[trail-reference-resolution]

**Auto-update on note move.** When a note moves via `move-note-core-cmd` or `drag-and-drop-move`, trails referencing that note get their stored rel-path rewritten to match. The ULID is unchanged — the move is path-only. Mechanics: `core::ops::move_note` (and `move_folder`) call into a new `core::trails::on_note_moved(old_rel, new_rel)` helper that queries the derived `trail_waypoints` table for affected trail-docs and a parallel `waypoint_outgoing_refs` table for waypoint-notes, rewriting each match's `path` field via `vault.write_file_checked`. One `core::changes` row per touched file. Watcher-driven external moves (the `From`+`To` paired-rename branch in `watcher.md`) trigger the same path; unpaired Created+Deleted pairs are too speculative to act on. [trail-auto-update-on-note-move]


## Note ID stamping policy

Currently ULIDs live only in the indexer's `path_ids` table — they're not stamped onto md files. Trails (and any future cross-reference feature: wikilinks, MCP stable refs, source-derived dedup) need ULIDs to survive an indexer rebuild and to be readable in external tools. Two modes, vault-scope-configurable; the user picks one:

- **`all`** — indexer writes `hiker.id: <ulid>` to every note's frontmatter on first ingest (lazy backfill via the standard reindex path for pre-existing notes). Notes without frontmatter get a freshly-minted block. Pros: durable across indexer wipes for the entire vault; external tools see a stable identity for every note. Cons: hiker mutates every user file at least once (one-time cost, but visible).
- **`lazy`** (default) — indexer stamps `hiker.id: <ulid>` only when a note becomes the *target* of a reference (waypoint of a trail, or future wikilink target, or any other cross-reference feature). Notes that nothing references stay untouched. Once stamped, the stamp stays even if all references are later removed (cheap to keep; not worth a "stamp GC" pass). Pros: matches "filesystem is truth" — hiker only writes when the user has voluntarily made the note a referent.

Both modes share the invariant that **any note referenced by a trail (or future wikilink) has its ULID stamped to frontmatter**; the choice is just *when other notes also get stamped*. Config key `[indexing] id_stamping = "all" | "lazy"`, vault-scope eligible per `settings-write-back`. Default `lazy`. [note-id-stamping]

This decision is vault-wide — it shapes the future wikilink feature, MCP stable-ref behavior, and source-derived note dedup as much as it shapes trails. Trails just happens to be the first feature that earns its keep.


## Active trail

A vault has at most one active trail at a time. When set, the active trail is the routing target for every "import a source" path — the browser-extension capture, the MCP scrape tool, drag-URL, the share sheet — and is what the Trails sidebar mode renders. Persistence: `vault.active_trail = <rel-path-to-trail-doc> | null`, eligible for `set_setting` write-back per `settings-write-back`. [active-trail-state]

**No indicator outside Trails mode in v1.** When the user is in Files or Cluster trees mode, there's no UI cue which trail is active. Capture-routing into the wrong trail is the failure mode to watch; revisit if it bites in real use.


## Sidebar Trails mode

The Trails mode body is the third sidebar mode (per `sidebar-mode-switcher`). Its content is mostly the **active trail rendered vertically, top-to-bottom**, with header chrome for switching trails and reaching the trail-doc. The editor pane on the right is unaffected — clicking a waypoint opens its source note in the editor; the sidebar stays.

### Header

A compact row at the top of the sidebar body:

- **Active-trail dropdown** — left side, takes most of the row width. Lists trails ordered by most-recent activation (`hiker.last_activated_at`), top N entries, plus an **"All trails…"** entry at the bottom that opens a flat picker of every trail in the vault, plus **"None"** as the always-present top item that clears the active trail. Selecting a trail activates it (sets `vault.active_trail`, stamps `hiker.last_activated_at` on the trail-doc, body re-renders). [trails-mode-active-trail-dropdown, trails-dropdown-ordering]
- **Trail-head icon** — right of the dropdown. Clicking opens the active trail's trail-doc in the editor pane on the right (so the user can read or edit the trail's framing prose). The icon is the squiggly-trail glyph (same SVG family as the Trails-mode button in the sidebar mode switcher). Disabled when no trail is active. [trails-mode-trail-head-icon]

### Body

Vertical list of waypoint cards in trail order — depth-first traversal of the waypoint tree. Each card collapsed by default; click the card chevron to expand. [trails-mode-waypoint-card]

Collapsed state shows:

- Source-note basename
- One-line annotation snippet (first non-empty line of the waypoint-note body)
- A small chevron for expand
- A subtle ordinal showing position within the parent's child list (`1`, `1.2`, `1.2.1` for nested side-trail waypoints)

Expanded state renders the full waypoint-note body inline (markdown, live-previewed via the same plumbing the chat panel uses). One card expanded at a time; clicking another card collapses the previous. **"Expand all"** affordance in the sidebar header collapses or expands every card together.

Click affordances inside a card:

- **Click the source-note basename** → opens the source note in the editor pane (respects `file-switch-guard-dirty`).
- **Click an "edit annotation" affordance inside the expanded body** → opens the waypoint-note itself in the editor for full-screen editing of the commentary.

### Side trail rendering

Side-trail waypoints (waypoints that are children of another waypoint per `trail-side-trail-shape`) render **indented under their parent**. Each level of nesting adds one indent step plus a thin left rule running the height of the side-trail block, so the user can visually trace the digression and see where it returns to the main line. Children of the same parent render in their frontmatter order; depth-first traversal matches reading order. [trails-mode-side-trail-render]

A parent waypoint with children gets a second chevron (separate from the body-expand chevron) that **collapses or expands the side trail** without touching the parent's own annotation. Default state for v1: every side trail expanded, so the full tree is visible on first render. The collapse-side-trail chevron is for noise control on long branchy trails, not a reading-position concept — there's no "you are here" pointer in v1. The dedicated affordance for "follow this digression and hide the rest" is deferred polish.

### Empty states

[trails-mode-empty-states]

- Trails mode active, no active trail set: dropdown shows "None"; body shows a hint pointing at the dropdown ("Pick a trail to walk, or `+` to start a new one").
- Trails mode active, no trails in the vault at all: dropdown empty; body shows a "Create a trail" affordance that calls the same path the `+` button does.
- Active trail set with zero waypoints: body shows "Empty trail — capture into it or use `+` to add the first waypoint."
- Active trail with one or more orphaned references (broken double-links, neither half resolves): orphan cards rendered greyed with a "broken reference" pill, sorted in their original positions; the rest of the trail renders normally.

### Sidebar invariants

The Trails mode sidebar body is **read-only** for editing operations: no drag-to-reorder, no click-to-rename, no in-place annotation editing. Changes to the trail's structure happen by editing the trail-doc's frontmatter directly, by editing waypoint-notes in the editor, or via the one editing verb below. The read-only invariant keeps the sidebar a navigation surface and avoids tangling the linking-metadata between waypoint-notes when waypoints are reordered. [trails-mode-sidebar-read-only]

The single editing verb is **Remove waypoint** in a row's right-click context menu: deletes the waypoint-note from `.hiker/trails/<trail-id>/waypoints/`, removes the entry from the trail-doc's `hiker.waypoints` tree, confirms before deleting (the annotation is in the waypoint-note body and removal moves it to trash, not gone forever). Goes through `core::ops::delete` so the waypoint-note lands in `.hiker/trash/` like any other note and is restorable. Reorder and re-parenting are intentionally not exposed in v1 — users who want to restructure edit the trail-doc frontmatter directly. [trails-mode-remove-waypoint-verb]

**Removing a parent with children cascades.** If the waypoint being removed has child waypoints (a side trail), the confirm dialog names the child count ("Remove this waypoint and N side-trail waypoints?") and the remove operation deletes the parent and every descendant in a single pass — each descendant waypoint-note moves to `.hiker/trash/` like the parent. The trail-doc frontmatter drops the entire subtree. Cascade beats orphan-promotion (children floating up to grandparent) because the user's mental model is "this digression goes away," not "the parent goes away but its tail-end stays." Trash makes the cascade reversible if the user changes their mind.

**Trails mode is independent of Cluster trees mode.** The Trails sidebar body and the Cluster trees sidebar body are siblings under `#sidebar`, gated by mode classes (`#sidebar.mode-trails` vs `#sidebar.mode-clusters`). Switching modes swaps which body is visible; bodies don't share state, don't stack, and one mode's UI must never bleed into another mode's panel. Each mode body defaults to `display: none` and only the active mode's body promotes to `display: block`. [trails-mode-isolation-from-clusters]


## Trail graph viewer (deferred)

A future trail visualization that opens a trail (or a set of trails) as a graph in its own editor-pane tab — separate from the Cluster trees graph view. Nodes are waypoints; edges are sequence + side-trail branching. The renderer reuses the shared sigma+graphology adapter built for the cluster editor (`ui/src/graphRenderer/`) so the two viewers share rendering primitives, camera/zoom, and selection conventions. The tab kind is `graph` (already reserved in `TabKind` per `app/state.ts`); the trail viewer is one consumer, a future cross-vault overview is another.

Scope is intentionally light for v1: open the active trail as a graph, render waypoints with their basenames, edges colored by sequence vs side-trail. Multi-trail overlays, time-axis layouts, and filtering by activation recency are deferred polish — they're things the user might ask for, not things v1 needs. [trails-graph-viewer-tab]


## Filetree integration

[trail-row-icon]

Trail-docs render in the file tree at their natural FS location with a distinctive **squiggly-trail glyph** on the prefix side (same icon family as the Trails-mode button in the sidebar mode switcher). The trail-doc behaves like any other md note — search opens it, click opens it in the editor, drag-and-drop moves it (with the auto-update-on-move path rewriting any `hiker.in_trail` or `hiker.waypoints` references that pointed at the moved trail-doc).

**Per-trail dropdown chevron.** Each trail-doc row in the tree gets an expand chevron when its trail has at least one waypoint. Clicking the chevron expands the row inline (same machinery as folder expansion) to show the trail's waypoint-notes nested underneath, in trail order. Chevron is hidden for trails with zero waypoints. Expand state is per-trail and resets on vault open (no persistent expansion memory — keeps the tree tidy after a session). [trail-row-dropdown-chevron]

The expand-in-tree affordance is independent of the global hide-waypoints setting (below) — even with waypoints globally hidden, the user can drill into a specific trail's waypoints via the chevron without flooding the tree.

**Global waypoint-visibility toggle.** Waypoint-notes (under `.hiker/trails/<id>/waypoints/`) are *hidden* in the file tree by default — they're indexed and searchable, but cluttering the tree with every trail's waypoints is rarely what the user wants. A toggle `vault.show_trail_waypoints_in_tree` plugs into the existing `tree-source-visibility-toggles` registry; when on, waypoint-notes appear as a virtual top-level "Trail waypoints" group in the tree (same shape as the Sessions group). Off by default.

**"Set as active trail" right-click verb.** Right-clicking a trail-doc row in the tree opens a context menu with a "Set as active trail" entry; selecting it activates the trail (sets `vault.active_trail`, stamps `hiker.last_activated_at`). Lets the user activate a trail without switching sidebar modes. The verb is hidden on non-trail rows. [trail-set-as-active-context-verb]

**"Add to active trail" right-click verb.** Right-clicking an indexable note row (`.md` / `.txt`) in the tree shows an "Add to active trail" entry that appends the note as a new waypoint of the currently active trail. Routes through the existing `captureToActiveTrail(sourceRel)` helper, which wraps `Ipc.trailAppendWaypoint(activeTrailDocRel, sourceRel, null)` — same code path the future capture entry points (browser-ext, drag-URL, MCP scrape, share sheet) use. On success, toast `"Added to <trail-basename>"`. The verb is **hidden** on trail-doc rows (already a trail), waypoint-note rows under `.hiker/trails/`, folders, and unsupported file types. The verb is **disabled with a tooltip** ("No active trail — pick one in Trails mode") when no active trail is set; surfacing it disabled rather than hiding it teaches the affordance — once the user activates any trail the verb lights up. This is the v1 stopgap until real capture entry points land; it doesn't substitute for them (capture flow still routes external sources to an active trail without hopping through the tree). [trail-add-to-active-from-tree-verb]


## Creating a trail

A new trail comes from one of three entry points, all going through the same `core::trails::create_trail(name)` op:

- **Sidebar `+` button while in Trails mode** — left-click creates a new trail with a default name (`new-trail-N.md`, suffix-counted to avoid collision per the same `create_with_suffix` shape `sidebar-new-item-button` already uses); the new trail-doc opens in the editor with inline-rename mode active so the user can name it before submitting. The trail is auto-set as active.
- **Sidebar `+` right-click cross-type picker** — picking "New trail" from the picker (per `sidebar-new-item-button`) creates a trail regardless of current sidebar mode.
- **MCP `trail_create` tool** — agents can create trails as part of bookkeeping their investigations (per the MCP integration section below).

**Default placement.** New trails land at `<vault>/<new_trail_dir>/<name>.md`, where `new_trail_dir` is configurable. Config key `[trails] new_trail_dir = "trails/"` (default `"trails/"`, vault-scope eligible). The dir is auto-created on first trail. Setting `new_trail_dir = ""` (empty string) places trails at vault root. Users can move trail-docs anywhere in the vault later via filetree DnD — the placement is just a default; the trail-doc carries its own identity in frontmatter. [trails-default-location]


## Capturing into a trail

When a capture fires (browser-extension Save-to-Hiker per `browser-extension-capture`, drag-URL, MCP `scrape` tool, share sheet) and an active trail is set, the capture is routed to that trail:

1. The source-derived note (or md note from a generic capture) lands at its normal location — `inbox/`, the source-derived sidecar dir, or a versioned-source manifest dir, depending on the source type per `design.md`'s source-derived-notes framing.
2. A new waypoint-note is created at `.hiker/trails/<trail-id>/waypoints/<source-basename>--<short-id>.md` with frontmatter linking to the source note (`hiker.references`) and the trail (`hiker.in_trail`).
3. The waypoint-note body is empty.
4. The trail-doc's frontmatter `hiker.waypoints` tree gets the new waypoint-note appended **at the root level** — captures don't auto-nest into side trails. Re-parenting is a frontmatter edit.
5. Sidebar refreshes; the new waypoint card is visible, collapsed.

The user can immediately click the new waypoint's edit affordance to write the annotation. [trail-capture-flow]

When **no active trail** is set, captures land in `inbox/` (or the source type's normal home) without creating any waypoint — the active-trail mode adds routing, never forces it.


## Building a trail while reading

Capture is the right shape when an external source enters the vault. For notes already in the vault — your own writing, prior captures, anything you're re-encountering — there are two surfaces that append the current note as a waypoint on the active trail without going through capture. Both call into the same `core::trails::append_waypoint` op as the capture flow (root-level append; re-parenting via frontmatter edit).

- **Tree right-click → "Add to active trail"** — context-menu verb on `.md` / `.txt` rows in the file tree. Hidden on trail-docs, waypoint-notes, folders, and unsupported extensions. Disabled with a tooltip ("No active trail — pick one in the Trails sidebar") when no trail is active. The note doesn't have to be open in the editor; this is the "I'm browsing the tree and want to mark that one" shape. [trail-add-to-active-from-tree-verb]
- **Editor-pane "Add to trail" affordance** — when a regular note is open in the editor and a trail is active, a small pill in the editor toolbar's right-side cluster (just left of Save) reads "Add to trail: <trail-name>" and clicks-through to the same op. Hidden when no trail is active, or when the open buffer is itself a trail-doc / waypoint-note / read-only preview. The trail name in the pill doubles as the always-visible "which trail is active" indicator the spec otherwise lacks outside Trails mode. [trail-add-to-active-from-editor-verb]

Both verbs append under the trail's **append cursor** (see below) — the trail's "you are here" position. By default the cursor sits at the root tail (so consecutive adds extend the main line, today's behavior); the user moves the cursor to a specific waypoint to branch into a side trail. Idempotency: if the note is already a waypoint at any depth in the trail, the verb is disabled with a tooltip ("Already in this trail") rather than creating a duplicate. The check is per-trail, not per-vault — the same note can be a waypoint of multiple trails simultaneously (per `trails_containing_note`).


## Append cursor — branching the trail

[trail-append-cursor]

The trail-doc carries a **position cursor** in frontmatter — `hiker.append_under: <waypoint-id> | null` — that names where the next append lands. `null` (or absent) means "append at the root tail," matching the original flat-trail behavior with zero migration. When the cursor names a waypoint, `core::trails::append_waypoint` places the new entry as the last child of that waypoint (creating or extending a side trail).

**Cursor stays put across appends.** The cursor only moves when the user explicitly moves it (right-click "Append from here") or resets it (header action). Successive appends under the same cursor become *siblings* — `X.1`, `X.2`, `X.3` — a flat side trail under `X`, mirroring how flat the main line stays when the cursor is `null`. To dig deeper into a sub-side-trail off `X.2`, the user right-clicks `X.2` to move the cursor; subsequent appends become `X.2.1`, `X.2.2`. Auto-advance was rejected because it produces unintended deepening (every append would nest one level further than the last), turning a flat capture session into an accidental ladder.

**Explicit `parent_waypoint_id` overrides.** When `core::trails::append_waypoint` is called with `parent_waypoint_id: Some(id)` (the existing arg, used by MCP and any future explicit-parent caller), the explicit parent wins over the cursor. When `parent_waypoint_id: None` (the build-as-you-read verbs), the cursor is consulted and used as the parent. This keeps the typed surface honest — explicit > cursor — while letting the verbs ride the cursor without plumbing a separate arg through. Neither path moves the cursor; the cursor is exclusively user-controlled.

**Cascade-delete safety.** When `remove_waypoint` cascades through the cursor's waypoint (or the cursor's waypoint itself is the target), the cursor resets to `null` in the same trail-doc rewrite. Reading a stale `append_under` that doesn't resolve to any waypoint in the tree (concurrent edit, hand-edited frontmatter pointing at a deleted id) is treated as `null` on read with a `tracing::warn!` — same posture as orphan waypoint refs.

### Indicator — "next append lands here"

[trail-append-cursor-indicator]

The cursor's waypoint card in the Trails sidebar gets a **little-person glyph** (the existing `Icons.user()` head-and-shoulders SVG, already used in the recent-activity author-pill) rendered in the card head, just before the source-note basename. The glyph is an accent color (matching the existing `--accent` token used elsewhere) so the cursor's position is scannable at a glance without reading every card. Cards that aren't the cursor render unchanged. The same glyph as the activity pill keeps the visual vocabulary consistent — "where the user is" reads the same across surfaces.

When the cursor is `null` (root-tail append), no card carries the glyph. The Trails-mode header surfaces a subtle hint instead: a small text row reading `"Appending to main line"` (cursor null) or `"Appending under <basename>"` with a `"Reset to main line"` action button (cursor set). The header hint is the global "what's the next append going to do" signal; the per-card glyph is the locator.

Filetree integration is intentionally not extended to show the cursor — the file tree's job is browsing, not trail-position tracking. The Trails sidebar is the rich-render surface for trail-position concepts.

### Verbs

- **"Append from here" right-click verb on a waypoint card.** Adds an entry to the existing waypoint context menu (currently just "Remove waypoint"), above the Remove entry. Click sets `hiker.append_under` to that waypoint's id, rewrites the trail-doc, refreshes the sidebar; the little-person glyph moves to the clicked card. Available on every waypoint card including those nested in side trails (clicking lets the user branch off a digression). [trail-append-from-here-verb]
- **"Reset to main line" header action.** Surfaces in the Trails-mode header alongside the cursor hint when the cursor is non-null. Click sets `hiker.append_under` to `null`; the next append goes to the root tail. Hidden when the cursor is already `null`. [trail-reset-cursor-verb]

The two verbs are the only cursor-mutation surfaces in v1. The cursor field is also editable directly in the trail-doc frontmatter (it's a regular markdown note); a hand-edit triggers the same indexer / sidebar refresh path as any other trail-doc change.

### Why this shape

The earlier deferred design (`"Branch from this waypoint" sidebar verb`) required a parent-picker gesture every time the user wanted to branch — a heavier interaction the v1 spec punted on. The cursor design replaces that with a *persistent positional* concept: the user marks where the next appends should land, and they keep landing there until moved. Reading a trail and adding a digression becomes one gesture (right-click "Append from here") that covers any number of follow-up appends; growing that digression into a sub-digression takes another single gesture on the new parent. The cursor is exclusively user-controlled — appends never move it — so the user never has to undo unintended nesting. It also subsumes the `"Follow side trail" reading-position mode` deferred under "Out of scope (v1)": the cursor is the position, no separate reading-mode state needed.


## Indexer integration

A derived `trail_waypoints` table inside `index.db` supports fast lookups: which trails contain a given note, which waypoint-notes belong to a given trail, what's the waypoint-note's `(trail_id, source_path)` pair, and where each waypoint sits in the trail's tree. Schema:

- `trail_waypoints (waypoint_path TEXT PRIMARY KEY, waypoint_id TEXT, trail_id TEXT, source_id TEXT, source_path TEXT, parent_waypoint_id TEXT NULL, tree_path TEXT)`
- `parent_waypoint_id` is `NULL` for root-level waypoints; otherwise the ULID of the parent waypoint.
- `tree_path` is a materialized path encoding depth-first position — `"1"`, `"1.2"`, `"1.2.1"`, etc. — so lexical ordering on `tree_path` gives reading order without a recursive query. Re-derived from the trail-doc's `hiker.waypoints` tree on every upsert; not load-bearing for correctness (the frontmatter is truth) but cheap for sidebar paint.
- Indexes on `trail_id`, `source_id`, `source_path`, `parent_waypoint_id`.

Built and maintained by `core::indexer` like every other derived index — re-derived on schema bump, fail-loud per `store-version-fail-loud`. Schema version increments to match. [trail-waypoints-derived-table]

A parallel structure (column or sibling table) tracks trail-doc frontmatter waypoint refs for the auto-update-on-move path so a single query gives all the affected trail-docs without parsing every trail-doc on disk. The exact split (one table or two) is an implementation detail; the index just has to make `trails_containing(note_id)` and `waypoints_of(trail_id)` cheap.


## Watcher integration

`.hiker/trails/` is carved out of the watcher's standard `.hiker/`-ignore rule so trail-docs and waypoint-notes are routed to the indexer like any other md file. Same shape as the carve-out for `.hiker/sessions/` (per `chat-session-show-in-tree-toggle`). [trail-watcher-carve-out]

The `core::trails` module owns watcher suppression around its own writes (create / append / remove) using the existing `Watcher::suppress` shape, so notify can't surface an event for a path the indexer has already routed.


## Trash integration

Deleting a trail-doc cascades to its `.hiker/trails/<trail-id>/waypoint/` directory: the trail-doc moves to `.hiker/trash/` and the entire waypoint dir moves alongside it as a single atomic unit. Restoring the trail-doc from trash also restores the waypoint dir. Same `core::ops::delete` and `core::ops::restore` paths every other note uses, with the cascade enforced inside `core::trails::delete_trail` so restoring later puts the trail back in working order. [trail-delete-cascade]

Deleting a single waypoint-note via the sidebar's "Remove waypoint" verb moves only that waypoint-note to trash; the trail-doc's frontmatter is updated to drop the entry; the rest of the trail is untouched.


## Search integration

Trail-docs and waypoint-notes are indexed and searchable like any other md file in the vault — they show up in lexical and semantic search results, related-notes queries, and the full-text FTS5 index. [trail-searchable-as-notes]

The search panel's per-source-type filter (`search-source-type-filter`) grows two new filterable kinds — `trail` (for trail-docs) and `waypoint` (for waypoint-notes) — alongside the existing `md` / `pdf` / `image` / `audio` / etc. The filter row reads from `hiker.kind` for these two kinds (since they're not source-derived) in addition to its existing `hiker.type` read for source types. Default-all-on, so a baseline search returns trail-docs + waypoints + regular notes mixed. The filter implementation lives under the existing `search-source-type-filter` slug in `search.md`; trails just adds to the filterable kinds.


## MCP integration

Trails are a first-class MCP surface — both read and write — so attached agents can consume curated trails as context and transcribe their own investigations as draft trails. All MCP-routed writes go through `core::ops::agent_*` helpers, append `core::changes` rows tagged `author='agent:<client-id>'`, stamp `hiker.author: agent-authored` on affected files, and pass through staging if `agent-write-review-mode` is enabled — uniform with the existing MCP write tools. The `mcp-tool-toggles` machinery lets the user disable any individual trail tool independently.

### Read tools

- **`trails_list(filters?)`** — enumerate trails with optional filters (containing-note, recently-activated, name-substring). Returns trail-doc id + title + waypoint count + activation timestamp + `path`. [mcp-tool-trails-list]
- **`trail_get(id)`** — fetch a trail's full body + ordered waypoint list (each waypoint's source-note ref + annotation body). Detail levels (`digest` | `full`) mirror `mcp-tool-get-note`'s shape. [mcp-tool-trail-get]
- **`trails_containing_note(rel_path)`** — reverse lookup; returns trails that include a given note as a waypoint. Useful for "what trails reference the note I'm reading?" [mcp-tool-trails-containing-note]

### Write tools

- **`trail_create(name, draft?)`** — create a new trail (empty waypoint list, default placement per `[trails] new_trail_dir`). When `draft=true`, the trail-doc is created with `hiker.draft: true` in its frontmatter and lands at the draft path per the draft-trail surface below; the user reviews and accepts to promote. Returns the new trail-doc's id and rel-path. [mcp-tool-trail-create]
- **`trail_append_waypoint(trail_id, source_rel, parent_waypoint_id?, annotation?)`** — append a new waypoint to a trail. Creates the waypoint-note under `.hiker/trails/<trail-id>/waypoints/`, links to the source, and seeds the annotation with the optional `annotation` argument (omitted → empty body, the v1 capture-flow shape). When `parent_waypoint_id` is provided, the new waypoint is appended as the last child of that parent (a side-trail append); omitted → root-level append. [mcp-tool-trail-append-waypoint]
- **`trail_remove_waypoint(trail_id, waypoint_id)`** — remove a waypoint from a trail. Symmetric to the sidebar's "Remove waypoint" verb; cascades to descendants if the target has children, per the sidebar verb's cascade rule. [mcp-tool-trail-remove-waypoint]

Agent-authored trails are auditable two ways: the `hiker.author: agent-authored` stamp on the trail-doc and every waypoint-note, plus the `mcp-audit-log-mcp-calls` record of every trail tool call to `<vault>/.hiker/agent-log/<YYYY-MM-DD>.jsonl` — surface `mcp-tool-call`, feature `trails_list` / `trail_get` / `trail_create` / etc., redacted-by-default body content per `[mcp.audit] log_full_input`.


## Draft trails

The clustering pipeline and MCP agents may *propose* trails the user hasn't authored. Drafts are first-class trails (same shape, same storage, same indexer treatment) flagged with `hiker.draft: true` in the trail-doc frontmatter and parked at a separate path so they don't pollute the user's trail dropdown until accepted. The user accepts (with optional edits to either the body, the waypoints, or both), edits-and-re-saves to keep iterating, or discards.

### Draft sources

- **MCP agents** — any agent calling `trail_create(draft=true)` or appending waypoints to a draft trail produces a draft. The default for agent-initiated trail creation depends on the `agent-write-review-mode` flag: when review mode is on, `draft=true` is the implicit default (matching the existing staging-as-default-for-agent-writes shape); when review mode is off, agents can still opt into drafts explicitly. [trail-draft-from-agent]
- **Clustering pipeline** — when `core::cluster` produces a tree containing a notable thread (e.g., a chain of related notes that look like an implicit reading order), it can emit a draft trail proposal alongside the existing reorganization proposal. The shape rides the same `vault/.hiker/proposals/<timestamp>.md` machinery `suggestions.md` already uses; trails-flavored proposals point at a draft trail-doc instead of a folder-move list. Off by default — opt-in via `[clustering] propose_trails = false`. [trail-draft-from-clustering]
- **Auto-generated from LLM tool-use trace** — folds into this same draft pipeline. When an agent answers a query, the set of `get_note` / `read_note` / search tool calls during that turn becomes a candidate draft trail. Specced under `trail-auto-from-llm-trace`; the draft-review surface here is its consumer.

### Storage

Draft trail-docs live at `<vault>/.hiker/trails/drafts/<trail-id>.md` (parallel to the active-trail path under `.hiker/trails/<trail-id>/`). Their waypoint-notes live at `.hiker/trails/<trail-id>/waypoints/` exactly like a normal trail's — drafts and accepted trails are storage-shape-identical except for the trail-doc location and the `hiker.draft: true` flag. Drafts are excluded from the Trails sidebar dropdown, the filetree's trail rows, and `trails_list` MCP results unless the caller passes `include_drafts=true`. [trail-draft-review-surface]

### Review surface

Draft trails are reviewed inline from two surfaces: the trails panel itself, and the activity detail page (per `settings.md`'s pending change review section). [trail-draft-review-surface]

**Trails panel.** When the active trail is a draft, a muted banner row appears below the header (same visual weight as the append-cursor hint):

```
ⓘ Draft — proposed by agent
[Accept trail]  [Reject]
```

When the active trail has pending waypoint additions proposed by an agent, minimal collapsed rows appear at the end of the waypoint list:

```
── Proposed ─────────────────────────────
+  research/raptor.md   [Accept] [Reject]
+  notes/whisper.md     [Accept] [Reject]
```

Each row is the source basename plus two muted action links. No full waypoint card — the source path is enough context.

**Activity detail page.** The existing `vault-home-recent-activity-detail` page gains a "Pending" filter pill that lists all pending proposals, including draft trails and waypoint additions. Each row carries [Accept] [Reject]; [Accept all (N)] batch-approves.

**Accept** — strips `hiker.draft: true`, moves the trail-doc from `.hiker/trails/drafts/` to the configured `[trails] new_trail_dir` (default `trails/`), keeps the waypoints in place, appends a `core::changes` row tagged `metadata.reviewed = true` + `metadata.review_source = "trail-draft"`. The trail joins the dropdown as a normal trail.

**Reject** — deletes the trail-doc and the entire `.hiker/trails/<trail-id>/` directory (waypoint-notes included). No trash, no `core::changes` row — drafts are pre-acceptance, so rejection is hard-delete.

Drafts also appear in the Trails sidebar dropdown when the user explicitly toggles "Show drafts" — useful for working on a draft over multiple sessions before deciding to accept. Toggle state is per-vault, off by default.


## Out of scope (v1)

- **Drag-to-reorder waypoints in the sidebar.** Reorder via trail-doc frontmatter edit only. The use case is rare and the linking-metadata invariants are easier to keep with a read-only sidebar.
- **In-place annotation editing in the sidebar.** Open the waypoint-note in the editor instead.
- **"Follow side trail" reading-position mode.** A reading-position concept where entering a side trail collapses the main line and "back up" returns is a richer memex-shape but introduces stateful UI the rest of the sidebar doesn't have. v1 renders everything expanded with indents. The `trail-append-cursor` indicator gives a subset of this for free (a single "you are here" glyph on the cursor card) without the collapse-and-restore reading-mode state.
- **Tab-strip waypoint chips / "3/7" indicators on buffer tabs.** When an active trail's waypoint is open in a buffer tab, the tab doesn't currently surface its trail position. Possible future polish; not v1.
- **Stepper-only mode** (`← prev | trail name | next →` row above the editor).
- **Trail-as-document live-preview that renders waypoint refs inline as mini-cards** in the trail-doc body. The body and the waypoint list are intentionally independent in v1.
- **"On trails: …" row in the discovery panel** showing trails that include the active note. Useful future affordance; not v1.
- **Cross-vault trail interop.** Trails are vault-scoped; ULIDs aren't unique across vaults and rel-paths aren't either. Bush's "reproduce a trail and hand to a friend" is a future concern once cross-vault identity is solved (or punted via export-as-bundle).


## Deferred

- **Auto-generated trails from LLM tool-use trace.** Audit-shaped: when an agent answers a query, the set of `get_note` / `read_note` / search tool calls during that turn becomes a candidate trail showing the agent's reasoning path. The candidate trail rides the draft-trail surface (`trail-draft-review-surface`) — user accepts, edits, or discards through the same review flow. Pairs naturally with `chat-tool-call-opens-touched-note` since both surface the agent's note-touching trail. Lands once basic trails are solid in real use. [trail-auto-from-llm-trace]
- **Filename color toggle for active-trail waypoints.** Optional toggle (default off) that colorizes the filename of any note that's a waypoint of the *currently active* trail in the file tree, so trail membership is scannable from the tree without opening the Trails sidebar mode. Lives under the View menu when it lands. [trail-active-waypoint-filename-color]
- **CLI surface.** `hiker trail list / show / activate / new` for shell-script-driven workflows; rides the broader `cli-*` family in `status.md`. [cli-trail-list, cli-trail-show, cli-trail-activate, cli-trail-new]
- **Active-trail indicator outside Trails mode.** A small chip in the editor toolbar or status bar showing which trail is active when sidebar is in Files / Cluster trees mode. Useful if capture-into-wrong-trail becomes a real footgun in v1 use; revisit then.
- **Graph view of trails.** Multi-trail visualization on a note-graph; rides the v4+ graph view feature per `design.md`'s graph-view bullet. Trails are one of two consumers (the other being wikilinks once they exist), not the owner.
