# Wikilinks

References between notes, authored and stored as names. A wikilink's target is a vault-relative **path**: `[[Name]]` when the basename is unique in the vault, `[[folder/sub/Name]]` (no `.md` extension) when it isn't. Renames rewrite every referrer; the user-facing identifier is always the path the user sees on disk.

This consolidates the sketch in `design.md` (the `[[…]]` widget + `core::store` resolution + the `[[` picker) and `editor.md` (the decoration layer + `CompletionSource`). The decoration scaffold lives in `editor/editor-md/src/links.rs`; resolution, autocomplete, rename-rewriting, and backlinks are the live slices.

The headline decisions:

- **Stored form is the path the user sees.** `[[Name]]` for unique basenames, `[[folder/sub/Name]]` when disambiguation is needed. No IDs in note bodies, no frontmatter stamping, no normalization on save — what the user types is what's stored and what's on disk. [wikilink-path-form]
- **Rendered label is the target's title.** The pill shows the target's current title (its basename, or frontmatter `title` when set), resolved by path at render time. Renaming the target updates the rendered label naturally; a referrer whose path got rewritten on rename also sees the new label. [wikilink-render]
- **Autocomplete picks the shortest unambiguous form.** Typing `[[` opens a title/path picker; selecting a note inserts `[[Name]]` if the basename is unique in the vault, otherwise `[[folder/sub/Name]]`. The user never has to think about disambiguation unless they want to override. [wikilink-autocomplete]
- **Rename rewrites every referrer.** Moving a note rewrites every `[[…]]` body, trail waypoint path, and kanban card path that pointed at it. One transaction — no dangling links from a crash mid-rename. [wikilink-rename-rewrite]
- **Ambiguity policy is configurable.** When a name has more than one match, the resolver default is `unresolved` (render as broken, force the user to disambiguate). Two opt-in modes — `lex-first` (lexicographically-first matching path, with a warning) and `nearest-folder` (nearest match in the folder tree) — for users who prefer a guess. [wikilink-ambiguous-resolution]
- **Backlinks come from the structural index**, surfaced in the discovery panel. [wikilink-backlinks]


## Stored form and rendering

- **Form.** `[[<path>]]` where `<path>` is either a bare basename (`[[meeting]]`) or a vault-relative path without the `.md` extension (`[[work/meeting]]`). The `.md` extension is implicit and dropped on insert. Subpaths use forward slashes regardless of platform. [wikilink-path-form]
- **Decoration.** The live-preview decoration (`editor/editor-md/src/links.rs`) replaces the `[[…]]` span with a styled link pill when the cursor is off the line and reveals the raw markdown when the cursor is on it — the standard live-preview reveal that every other decoration uses. [wikilink-render]
- **Rendered label.** The pill's label is the target's current title — frontmatter `title` if present, otherwise the basename without `.md`. Resolved by path at render time; renaming the target (which also rewrites every referrer's path) refreshes the label on the next decoration rebuild. If the target can't be resolved, the decoration falls back to the raw path the user typed in an unresolved style. [wikilink-render]


## Authoring

- **The `[[` picker.** Typing `[[` opens an autocomplete popup over note titles/paths — the same indexer-backed picker as the chat `@`-mention (`app::completion_sources::WikilinkSource`, `editor-view`'s `CompletionSource` trait). Selecting a note inserts the **shortest unambiguous form**: bare basename when unique vault-wide, otherwise the vault-relative path with as many leading folder segments as needed for uniqueness. The picker shows the full path alongside the title so the user knows which note they're picking when basenames collide. [wikilink-autocomplete]
- **Hand-typed links.** A link typed by hand or authored in an external editor is resolved by path. Bare-name links go through the ambiguity policy below; explicit-path links resolve directly. Resolution is read-only — nothing is rewritten on save. [wikilink-hand-typed]


## Resolution and ambiguity

- **Resolve via the indexer.** A click (or any programmatic resolve) looks the path up in the indexer's note table — the same path that `core::store` already uses. No `path → id` indirection; the path *is* the identity. [wikilink-resolve]
- **Bare-name resolution.** `[[Name]]` matches every note whose basename (without `.md`) equals `Name`. Zero matches → unresolved. One match → resolved. Multiple matches → ambiguous, handled per the policy below.
- **Explicit-path resolution.** `[[folder/sub/Name]]` resolves to exactly the note at `folder/sub/Name.md`. Zero matches → unresolved; never ambiguous.
- **Ambiguity policy.** Vault-scope config `[wikilinks] ambiguous_resolution = "unresolved" | "lex-first" | "nearest-folder"`, default `"unresolved"`. [wikilink-ambiguous-resolution]
  - `"unresolved"` — render the link as broken with a disambiguation affordance; click offers a picker of the matching notes so the user can rewrite the link to the explicit-path form.
  - `"lex-first"` — resolve to the lexicographically-first matching path; surface a one-time warning in the discovery panel ("Ambiguous link `[[Name]]` in `<referrer>`") so the user knows it's a guess.
  - `"nearest-folder"` — resolve to the match with the longest shared path prefix with the linking note's own path (ties broken lex-first). Useful when the user's mental model is "this folder's notes link to each other first."

**Case sensitivity** follows the host filesystem: case-sensitive on Linux, case-insensitive-preserving on macOS, case-insensitive-preserving on Windows. A vault that syncs across platforms inherits the strictest rule (case-sensitive) so a link valid on the authoring device is valid everywhere; the sync layer warns when it detects case-only collisions in a cross-platform vault. [wikilink-case-sensitivity]


## Rename rewriting

When a note is renamed or moved (`move-note-core-cmd`, `drag-and-drop-move`, or a watcher-detected external rename per `watcher.md`), every referrer is rewritten in the same operation:

- **Wikilink bodies** — every `[[…]]` whose resolved path was the renamed note is rewritten to the new path's shortest-unambiguous form. A bare-name link stays a bare-name link unless the rename introduces ambiguity; an explicit-path link gets its path rewritten end-to-end.
- **Trail waypoint paths** — the `hiker.references.path` field in each affected waypoint-note rewrites (per `trail-auto-update-on-note-move`).
- **Kanban card paths** — each `cards[].path` entry in affected board-docs rewrites (per `board-card-references`).

All rewrites for a single rename commit through the op-log as one logical rename batch: each referrer's edit is its own op (so per-file history stays clean), but the indexer's referrer enumeration + edit application happens inside one transaction. A crash mid-rename leaves either all referrers rewritten or none — never dangling links. [wikilink-rename-rewrite]

**Referrer enumeration** uses the structural index (per `wikilink-backlinks`): the indexer maintains a reverse map from target path to referrer paths so a rename is one indexed query plus N writes, not a vault scan. The same index serves backlinks display. A future Bloom filter over "does this note contain any wikilinks at all" is the obvious optimization if profiling shows the writes-per-rename are hot; not built first. [wikilink-rename-bloom-filter-deferred]

**External renames** (a file changed outside hiker, e.g. via Syncthing arriving from another device or a terminal `mv`) trigger the same rewrite path through the watcher's rename detection. Unpaired Created+Deleted pairs are too speculative to treat as a rename and surface as a delete + create — the same posture trails uses today.


## Click behavior

A click resolves the path and opens the target through the shared open-note path, landing in the preview slot by default and promoting to a sticky tab on intent — identical to every other open-note entry point (`editor-preview-tab-from-open-callsites`); Mod-click opens sticky directly. An already-open target switches to its tab rather than reopening (`multi-buffer-tree-click-switches-tab`). [wikilink-click-open]


## Hover preview card

Hovering a resolved wikilink for ~400ms shows a small scrollable preview card with the target note's rendered content — title, frontmatter summary, and a scrollable body. Lets the user peek at what's behind a link without opening it. [wikilink-hover-preview]

- **Trigger.** Pointer enters a resolved wikilink pill; card appears after a short hover delay (~400ms) so transient sweeps of the pointer don't spam previews. Pointer leave (with a brief grace window so the user can move the cursor into the card itself) dismisses.
- **Anchor.** Card paints over the editor canvas anchored to the pill's screen position, with quadrant-flip + canvas-clamp placement — same painter primitive as `cluster-editor-graph-view-hover-preview-card`. Reuses `panels::graph::paint_preview_card` so both consumers share styling.
- **Body source.** Target note's rendered markdown: frontmatter `title` (or basename), first ~30 lines of body with frontmatter stripped via `panels::graph::skip_frontmatter`, and a scrollable region for more. Scrolling inside the card scrolls only the card body, not the editor.
- **Style.** Same light-background / 1px-border treatment as the cluster-graph card; long titles wrap inside the card rather than expanding it.
- **Unresolved / ambiguous links** don't get a preview card — the existing unresolved-style affordance (per `wikilink-unresolved`) is the right surface for "you need to disambiguate / create this." Hovering an unresolved pill is a no-op.
- **Click through still opens the note** via `wikilink-click-open` — the card doesn't replace the click path, it's a peek-before-commit. Mod-click on a pill while its card is showing closes the card and opens the target sticky, same as without the card.
- **Embed (`![[...]]`)** is the deferred sibling of this feature (per `wikilink-embed`) and renders inline rather than as a hover card; the two are separate features serving different needs.
- **Module placement.** Hover detection + card lifecycle live in the wikilink decoration layer (`editor/editor-md/src/links.rs`); body resolution reuses the indexer's read path; painting reuses the shared card helper.


## Backlinks

The structural index records each resolved wikilink as a typed edge (source path → target path). The set of notes linking *to* the active note is surfaced in the discovery panel alongside search results and related notes (`search.md`). Backlinks rewrite naturally when either endpoint renames — both halves go through the rename-rewrite pass. [wikilink-backlinks]


## Unresolved links

A wikilink whose target can't be resolved — a name with no match, an explicit path that doesn't exist, or an ambiguous name under the `"unresolved"` policy — renders in a distinct unresolved style rather than a normal pill. Clicking offers:

- For a no-match link: "Create note at `<inferred path>`" (the bare-name case infers `<linking-note's-folder>/<Name>.md`; explicit-path links use the exact path) → creates the file empty and resolves the link.
- For an ambiguous link under `"unresolved"`: a picker of the matching notes so the user can rewrite to the explicit-path form. [wikilink-unresolved]


## Consumers

Path-form is the single representation every link producer emits:

- **Crawl rewrite** (`extract.md`, `crawl-link-rewrite-wikilinks`) rewrites internal links among crawled pages to `[[<path>]]`, picking the shortest-unambiguous form against the crawl manifest.
- **Trails** and **kanban** reference notes by path in YAML (`hiker.references.path` for trail waypoints, `cards[].path` for kanban cards) — the same path-based identity wikilinks use. The picker UI and rename-rewrite machinery is shared.
- **MCP** returns paths as the stable handle on every note-shaped result.


## Deferred

- **Heading and block targets** — `[[Name#Heading]]` and `[[Name#^block]]`, the latter needing an auto-injected block anchor in the target. Page-level links ship first. [wikilink-headings-blocks]
- **Embeds / transclusion** — `![[Name]]` rendering the target's content (or an image/PDF) inline rather than as a link. [wikilink-embed]
- **Bloom-filter optimization** for referrer enumeration on rename — `wikilink-rename-bloom-filter-deferred`. Build the straightforward index-driven version first; add the filter if profiling shows it matters.


## Out of scope

- **Opaque-ID-based links** (the prior ULID model). Rejected: the stamping was visible in user files, the ID had to be normalized into every typed link on save, and the user-facing identifier was an opaque string. Path-based identity keeps notes clean and round-trippable through external editors at the cost of one referrer-rewrite pass per rename — a cost the indexer's structural index makes cheap.
- **The vault-wide graph view.** Wikilinks are an edge source for it, but the graph view is its own `design.md` feature.
