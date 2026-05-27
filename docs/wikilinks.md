# Wikilinks

Stable references between notes, authored and read as names while stored as durable IDs. A wikilink's target is a note's ULID, not its name or path — so links survive renames and moves (including the moves hiker itself makes during triage, cluster Apply, and reorg). The opaque ULID is machinery: the user types and sees note *names*, and hiker maintains the ID underneath.

This consolidates the sketch in `design.md` (the `[[id]]` widget + `core::store` resolution + the `[[` picker) and `editor.md` (the decoration layer + `CompletionSource`). The decoration scaffold already exists in `editor/editor-md/src/links.rs`; resolution, autocomplete, stamping, name-normalize, and backlinks are the unbuilt slices.

The headline decisions:

- **Stored form is `[[<ulid>|<display>]]`.** The ULID is the durable target; the display half is a human-readable fallback for plain/external viewers. The ULID never changes when the target is renamed or moved, so no referrer-rewrite engine is needed — the opposite of a name/path-based link model. [wikilink-id-form]
- **Authored and read as names; ULIDs are never typed.** The `[[` autocomplete picker resolves a name to a note and inserts the id form; in rendered view the link shows as a pill of the note's title. The ULID surfaces only in raw-source mode with the cursor on the link, where the display text sits beside it. [wikilink-autocomplete] [wikilink-render]
- **Display renders from the target's live title.** The rendered label is looked up by ULID at render time, so it's always current; the stored `|display` is only the at-write-time fallback. [wikilink-render-live-title]
- **Hand-typed name links resolve, then normalize.** `[[Some Name]]` (a name, not a ULID) resolves by title match and is rewritten to `[[<ulid>|Some Name]]` on save, stamping the target's ULID lazily. This gives name-typing ergonomics on top of id storage, and covers links authored in an external editor. [wikilink-name-normalize]
- **Backlinks come from the structural index**, surfaced in the discovery panel. [wikilink-backlinks]


## Stored form and rendering

- **Form.** `[[<ulid>]]` or `[[<ulid>|<display>]]`. The picker always writes the display half (the target's title at insert time) so the raw markdown stays legible without hiker. [wikilink-id-form]
- **Decoration.** The live-preview decoration (`editor/editor-md/src/links.rs`) replaces the `[[…]]` span with a styled link pill when the cursor is off the line, and reveals the raw markdown when the cursor is on it (the standard live-preview reveal, matching every other decoration). [wikilink-render]
- **Live title.** The pill's label is the target's *current* title resolved by ULID, not the stored `|display`. Renaming the target updates every link's rendered label with no rewrite to the linking notes. If the ULID can't be resolved, the decoration falls back to the stored display, then to the raw ULID. [wikilink-render-live-title]


## Authoring

- **The `[[` picker.** Typing `[[` opens an autocomplete popup over note titles/paths — the same indexer-backed picker as the chat `@`-mention (`app::completion_sources::WikilinkSource`, `editor-view`'s `CompletionSource` trait). Selecting a note ensures it carries a ULID (stamping it if needed, per `wikilink-target-stamp`) and inserts `[[<ulid>|<title>]]`. The user never types or sees a ULID in this path. [wikilink-autocomplete]
- **Hand-typed / external links.** A link whose target is a name rather than a ULID — `[[Some Name]]`, typed by hand or authored in another editor — resolves by unique title match and is rewritten to `[[<ulid>|Some Name]]` on save. A name with no unique match stays an unresolved wikilink (`wikilink-unresolved`) until disambiguated via the picker; hiker never guesses between two same-titled notes. [wikilink-name-normalize]


## Resolution and stamping

- **Resolve via `core::store`.** A click (or any programmatic resolve) maps the ULID to the current note path through the indexer's `path → ulid` table, independent of where the file currently lives. [wikilink-resolve-store]
- **Stamp on link creation.** Creating a link to a note stamps that note's `hiker.id: <ulid>` to its frontmatter — exactly the lazy trigger in `note-id-stamping` ("a note becomes the target of a reference"). Under `id_stamping = all`, every note is already stamped; under the default `lazy`, only referenced notes are. The wikilink feature is one of the cross-reference features that invariant exists for. [wikilink-target-stamp]
- **Move/rename resilience.** Because the target is the ULID and resolution goes through the `path → ulid` table, moving or renaming the target — by the user, by `move_note`, or by hiker's own auto-organization — never touches the linking notes. This is the reason for the id-based model.


## Click behavior

A click resolves the ULID and opens the target through the shared open-note path, landing in the preview slot by default and promoting to a sticky tab on intent — identical to every other open-note entry point (`editor-preview-tab-from-open-callsites`); Mod-click opens sticky directly. An already-open target switches to its tab rather than reopening (`multi-buffer-tree-click-switches-tab`). [wikilink-click-open]


## Backlinks

The structural index records each resolved wikilink as a typed edge (source note → target ULID). The set of notes linking *to* the active note is surfaced in the discovery panel alongside search results and related notes (`search.md`). Backlinks resolve through the same `path → ulid` table, so they survive target moves. [wikilink-backlinks]


## Unresolved links

A wikilink whose target can't be resolved — a name with no match, or a ULID whose note was deleted — renders in a distinct unresolved style rather than a normal pill. Clicking an unresolved *name* offers to create the note (then stamps + links it); clicking an unresolved *ULID* (dangling reference, e.g. the target was deleted) surfaces a "target missing" affordance rather than silently doing nothing. [wikilink-unresolved]


## Consumers

The id form is the single representation every link producer emits:

- **Crawl rewrite** (`extract.md`, `crawl-link-rewrite-wikilinks`) rewrites internal links among crawled pages to `[[<ulid>|<page-title>]]`, stamping each created page's ULID. It can emit the syntax and stamp IDs before this feature's *rendering* lands; the links simply become clickable once it does.
- **Trails** and **MCP stable refs** reference the same ULIDs (`note-id-stamping` is shared), so a waypoint and a wikilink to the same note point at one identity.


## Deferred

- **Heading and block targets** — `[[<ulid>#Heading]]` and `[[<ulid>#^block]]`, the latter needing an auto-injected block anchor in the target. Page-level links ship first. [wikilink-headings-blocks]
- **Embeds / transclusion** — `![[<ulid>]]` rendering the target's content (or an image/PDF) inline rather than as a link. [wikilink-embed]


## Out of scope

- **Name/path-based links with a rename-rewrite engine.** Rejected: hiker moves notes itself (triage, cluster Apply, reorg), so name/path targets would break or demand constant referrer rewrites. The ULID target is stable across exactly those moves.
- **The vault-wide graph view.** Wikilinks are an edge source for it, but the graph view is its own `design.md` feature.
