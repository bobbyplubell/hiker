# Wikilinks

References between notes, authored and stored as names. A wikilink's target is a vault-relative **path**: `[[Name]]` when the basename is unique in the vault, `[[folder/sub/Name]]` (no `.md` extension) when it isn't. Renames rewrite every referrer; the user-facing identifier is always the path the user sees on disk.

This consolidates the sketch in `design.md` (the `[[…]]` widget + `core::store` resolution + the `[[` picker) and `editor.md` (the decoration layer + `CompletionSource`). The decoration scaffold lives in `editor/editor-md/src/links.rs`; resolution, autocomplete, rename-rewriting, and backlinks are the live slices.


## Stored form and rendering

- **Form.** `[[<path>]]` where `<path>` is either a bare basename (`[[meeting]]`) or a vault-relative path without the `.md` extension (`[[work/meeting]]`). No IDs in note bodies, no frontmatter stamping, no normalization on save — what the user types is what's stored and what's on disk. The `.md` extension is implicit and dropped on insert. Subpaths use forward slashes regardless of platform. [wikilink-path-form]
- **Decoration.** The live-preview decoration replaces the `[[…]]` span with a styled link pill when the cursor is off the line and reveals the raw markdown when the cursor is on it — the standard live-preview reveal that every other decoration uses. [wikilink-render]
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

**Referrer enumeration** uses the structural index (per `wikilink-backlinks`): the indexer maintains a reverse map from target path to referrer paths so a rename is one indexed query plus N writes, not a vault scan. The same index serves backlinks display. A Bloom-filter optimization is deferred (`wikilink-rename-bloom-filter-deferred`).

**External renames** (a file changed outside hiker, e.g. via Syncthing arriving from another device or a terminal `mv`) trigger the same rewrite path through the watcher's rename detection. Unpaired Created+Deleted pairs are too speculative to treat as a rename and surface as a delete + create — the same posture trails uses today.


## Click behavior

A click resolves the path and opens the target through the shared open-note path, landing in the preview slot by default and promoting to a sticky tab on intent — identical to every other open-note entry point (`editor-preview-tab-from-open-callsites`); Mod-click opens sticky directly. An already-open target switches to its tab rather than reopening (`multi-buffer-tree-click-switches-tab`). [wikilink-click-open]


## Hover preview card

Hovering a resolved wikilink for ~400ms shows a small scrollable preview card with the target note's rendered content, letting the user peek at what's behind a link without opening it. [wikilink-hover-preview]

- **Trigger.** Pointer enters a resolved pill; card appears after the hover delay so transient sweeps don't spam previews. Pointer leave (with a brief grace window so the user can move the cursor into the card itself) dismisses.
- **Anchor.** Card paints over the editor canvas anchored to the pill's screen position, with quadrant-flip + canvas-clamp placement. Reuses `panels::graph::paint_preview_card` (same painter primitive as `cluster-editor-graph-view-hover-preview-card`) so both consumers share styling.
- **Body source.** Frontmatter `title` (or basename) plus the first ~30 lines of body with frontmatter stripped, in a scrollable region. Scrolling inside the card scrolls only the card body, not the editor. Long titles wrap inside the card rather than expanding it.
- **Unresolved / ambiguous links** don't get a preview card — the existing unresolved-style affordance (`wikilink-unresolved`) is the right surface for "you need to disambiguate / create this." Hovering one is a no-op.
- **Click through still opens the note** via `wikilink-click-open` — the card is a peek-before-commit, not a replacement. Mod-click while the card is showing closes it and opens the target sticky.
- **Embed (`![[...]]`)** is the deferred sibling (`wikilink-embed`), rendering inline rather than as a hover card.
- **Module placement.** Hover detection + card lifecycle live in the wikilink decoration layer; body resolution reuses the indexer's read path; painting reuses the shared card helper.


## Heading anchors

A link may target a heading inside a note, not just the note itself: `[[Page#Heading]]` opens the note and scrolls to the heading; `[[#Heading]]` is a same-document anchor that scrolls within the current note without opening anything. [wikilink-headings-blocks]

- **Anchor split.** The `#section` trailer is split off the target before resolution (`split_target_section`): the page part (before the first `#`) resolves to a note exactly as a page-level link does — the anchor never changes *which* note a link points at, only where navigation lands. An empty page (a bare `#Heading`) means "this document."
- **Slug matching.** The anchor is matched against the note's headings by **GitHub's heading-slug algorithm** (`heading_slug`): lowercase, drop every character outside `[a-z0-9 -]`, turn whitespace runs into single hyphens. The first ATX heading (`#`…`######`) whose slug equals the anchor's slug wins; fenced code blocks are skipped so a `#` inside a ``` fence is never read as a heading. Slug matching means an anchor copied from a rendered view (`#some-heading`) resolves the same as one typed against the heading text (`#Some Heading`).
- **Navigation.** Resolution yields the heading's byte offset; the caret is placed there and a scroll-into-view is requested, so the heading sits near the top of the viewport once the (possibly just-opened) buffer's layout is built. `[[#Heading]]` scrolls the current buffer directly.
- **Graceful miss.** An anchor that matches no heading is a no-op for scrolling — the note still opens at the top rather than erroring. A `Page#Heading` whose *page* is unresolved follows the normal unresolved/create flow.
- **Block anchors** (`[[Name#^block]]`, which need an auto-injected anchor in the target) stay deferred.


## GitHub-style markdown links

Standard CommonMark inline links `[text](target)` to vault notes resolve and navigate through the *same* path resolution and heading-anchor logic as `[[…]]` — there is one resolver, not two. [markdown-link-vault-nav]

- **Vault vs external.** A markdown link whose destination is non-external — not `http(s)://`, `mailto:`, or `zim://` — is treated as a vault target: a bare name, a relative path, or either with a `#section` anchor (`[text](Page#Section)`, `[text](#Section)`). External-destination links keep the standard markdown rendering and the OS-open behavior; only vault-target links become clickable note pills.
- **Shared resolution.** The page part resolves through `wikilink::resolve_path` (bare-name / explicit-path / ambiguity policy) and the `#section` anchor through the same `heading_slug` matching as wikilinks. Click handling funnels both link kinds into one open path, so ambiguity policy, create-on-miss, and heading scroll all behave identically.
- **Rendering.** A vault-target link renders as a pill labelled with its own `[text]` (markdown links carry their display text directly, unlike wikilinks which resolve a title); a destination the index can't place renders in the unresolved style. The pill rides the existing wikilink click path rather than a separate plumbing layer.


## Backlinks

The structural index records each resolved wikilink as a typed edge (source path → target path). The set of notes linking *to* the active note is surfaced in the discovery panel alongside search results and related notes (`search.md`). Backlinks rewrite naturally when either endpoint renames — both halves go through the rename-rewrite pass. [wikilink-backlinks]


## Unresolved links

A wikilink whose target can't be resolved — a name with no match, an explicit path that doesn't exist, or an ambiguous name under the `"unresolved"` policy — renders in a distinct unresolved style rather than a normal pill. Clicking offers:

- For a no-match link: "Create note at `<inferred path>`" (the bare-name case infers `<linking-note's-folder>/<Name>.md`; explicit-path links use the exact path) → creates the file empty and resolves the link.
- For an ambiguous link under `"unresolved"`: a picker of the matching notes so the user can rewrite to the explicit-path form. [wikilink-unresolved]


## Consumers

Path-form is the single representation every link producer emits:

- **Trails** and **kanban** reference notes by path in YAML (`hiker.references.path` for trail waypoints, `cards[].path` for kanban cards) — the same path-based identity wikilinks use. The picker UI and rename-rewrite machinery is shared.
- **MCP** returns paths as the stable handle on every note-shaped result.


## Deferred

- **Block targets** — `[[Name#^block]]`, which needs an auto-injected block anchor in the target. Heading anchors (`[[Name#Heading]]`) ship now; block anchors stay deferred. [wikilink-headings-blocks]
- **Embeds / transclusion** — `![[Name]]` rendering the target's content (or an image/PDF) inline rather than as a link. [wikilink-embed]
- **Bloom-filter optimization** for referrer enumeration on rename — `wikilink-rename-bloom-filter-deferred`. Build the straightforward index-driven version first; add the filter if profiling shows it matters.


## Out of scope

- **Opaque-ID-based links** (the prior ULID model). Rejected for visible stamping and opaque identifiers; path-based identity keeps notes clean and round-trippable at the cost of one referrer-rewrite pass per rename.
- **The vault-wide graph view.** Wikilinks are an edge source for it, but the graph view is its own `design.md` feature.
