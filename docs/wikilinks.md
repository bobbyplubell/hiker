# Wikilinks

References between notes, authored and stored as names. A wikilink's target is a vault-relative **path**: `[[Name]]` when the basename is unique in the vault, `[[folder/sub/Name]]` (no `.md` extension) when it isn't. Renames rewrite every referrer; the user-facing identifier is always the path the user sees on disk.

This consolidates the sketch in `design.md` (the `[[…]]` widget + `core::store` resolution + the `[[` picker) and `editor.md` (the decoration layer + `CompletionSource`). The decoration scaffold lives in `editor/editor-md/src/links.rs`; resolution, autocomplete, rename-rewriting, and backlinks are the live slices.


## Stored form and rendering

- **Form.** `[[<path>]]` where `<path>` is either a bare basename (`[[meeting]]`) or a vault-relative path without the `.md` extension (`[[work/meeting]]`). No IDs in note bodies, no frontmatter stamping, no normalization on save — what the user types is what's stored and what's on disk. The `.md` extension is implicit and dropped on insert. Subpaths use forward slashes regardless of platform. [wikilink-path-form]
status:: done
touches:: [[code:hiker/wikilink]]
note:: parser drops ULID detection + `|display` alias half; the body is the target verbatim (bare basename or `folder/sub/Name`). The save-time normalize pass was previously removed · evidence: `core/src/wikilink.rs` (`parse_links`, `ParsedLink`)
- **Decoration.** The live-preview decoration replaces the `[[…]]` span with a styled link pill when the cursor is off the line and reveals the raw markdown when the cursor is on it — the standard live-preview reveal that every other decoration uses. [wikilink-render]
status:: partial
implements:: [[code:hiker/panels/buffer/wikilink_nav/title_resolver]], [[code:hiker/wikilink/shortest_unambiguous]]
touches:: [[code:hiker/links]], [[code:hiker/completion_sources]]
note:: decoration parses path-form bodies and renders the resolver-provided title (basename today); frontmatter `title` lookup deferred until the indexer surface for that lands · evidence: `editor/editor-md/src/links.rs` (`wikilink_decorations()`); `app/src/panels/buffer/wikilink_nav.rs` (`title_resolver`)
- **Rendered label.** The pill's label is the target's current title — frontmatter `title` if present, otherwise the basename without `.md`. Resolved by path at render time; renaming the target (which also rewrites every referrer's path) refreshes the label on the next decoration rebuild. If the target can't be resolved, the decoration falls back to the raw path the user typed in an unresolved style. [wikilink-render]


## Authoring

- **The `[[` picker.** Typing `[[` opens an autocomplete popup over note titles/paths — the same indexer-backed picker as the chat `@`-mention (`app::completion_sources::WikilinkSource`, `editor-view`'s `CompletionSource` trait). Selecting a note inserts the **shortest unambiguous form**: bare basename when unique vault-wide, otherwise the vault-relative path with as many leading folder segments as needed for uniqueness. The picker shows the full path alongside the title so the user knows which note they're picking when basenames collide. [wikilink-autocomplete]
status:: done
implements:: [[code:hiker/wikilink/shortest_unambiguous]]
touches:: [[code:hiker/completion_sources]]
note:: inserts `wikilink::shortest_unambiguous(paths, rel)` — bare basename when vault-unique, minimal folder-prefix when not · evidence: `app/src/completion_sources.rs` (`WikilinkSource`)
- **Hand-typed links.** A link typed by hand or authored in an external editor is resolved by path. Bare-name links go through the ambiguity policy below; explicit-path links resolve directly. Resolution is read-only — nothing is rewritten on save. [wikilink-hand-typed]
status:: done
touches:: [[code:hiker/wikilink]]
note:: bare-name + explicit-path resolution via `resolve_path`; no rewrite on save, no stamping. The click resolver in `wikilink_nav.rs` consumes it directly · evidence: `core/src/wikilink.rs` (`resolve_path`)


## Resolution and ambiguity

- **Resolve via the indexer.** A click (or any programmatic resolve) looks the path up in the indexer's note table — the same path that `core::store` already uses. No `path → id` indirection; the path *is* the identity. [wikilink-resolve]
status:: done
implements:: [[code:hiker/wikilink/resolve_path]]
note:: bare-name matches every path with the matching basename; explicit-path matches the exact vault path; ambiguity routed through `AmbiguityPolicy` (config-driven). No `path_for_id` / `id_for_path` indirection. Backlinks scan + click flow consume this resolver directly · evidence: `core/src/wikilink.rs` (`resolve_path`)
- **Bare-name resolution.** `[[Name]]` matches every note whose basename (without `.md`) equals `Name`. Zero matches → unresolved. One match → resolved. Multiple matches → ambiguous, handled per the policy below.
- **Explicit-path resolution.** `[[folder/sub/Name]]` resolves to exactly the note at `folder/sub/Name.md`. Zero matches → unresolved; never ambiguous.
- **Ambiguity policy.** Vault-scope config `[wikilinks] ambiguous_resolution = "unresolved" | "lex-first" | "nearest-folder"`, default `"unresolved"`. [wikilink-ambiguous-resolution]
status:: done
implements:: [[code:hiker/config/sections/WikilinksConfig]], [[code:hiker/config/Config#wikilinks]], [[code:hiker/store/notes/impl#[Store]find_notes_by_basename]], [[code:hiker/wikilink/AmbiguityPolicy]], [[code:hiker/wikilink/resolve_path]]
note:: vault-scope `[wikilinks] ambiguous_resolution = "unresolved" | "lex-first" | "nearest-folder"`, default `"unresolved"`. Lex-first sorts and picks first; nearest-folder counts shared path-segment prefix with the referrer · evidence: `core/src/wikilink.rs` (`AmbiguityPolicy`, `resolve_path`); `core/src/config/sections.rs` (`WikilinksConfig`, `AmbiguousResolution`)
  - `"unresolved"` — render the link as broken with a disambiguation affordance; click offers a picker of the matching notes so the user can rewrite the link to the explicit-path form.
  - `"lex-first"` — resolve to the lexicographically-first matching path; surface a one-time warning in the discovery panel ("Ambiguous link `[[Name]]` in `<referrer>`") so the user knows it's a guess.
  - `"nearest-folder"` — resolve to the match with the longest shared path prefix with the linking note's own path (ties broken lex-first). Useful when the user's mental model is "this folder's notes link to each other first."

**Case sensitivity** follows the host filesystem: case-sensitive on Linux, case-insensitive-preserving on macOS, case-insensitive-preserving on Windows. A vault that syncs across platforms inherits the strictest rule (case-sensitive) so a link valid on the authoring device is valid everywhere; the sync layer warns when it detects case-only collisions in a cross-platform vault. [wikilink-case-sensitivity]
status:: planned
note:: path equality follows host FS rules; cross-platform vaults inherit case-sensitive; sync layer warns on case-only collisions


## Rename rewriting

When a note is renamed or moved ([[spec:move-note-core-cmd]], [[spec:drag-and-drop-move]], or a watcher-detected external rename per `watcher.md`), every referrer is rewritten in the same operation:

- **Wikilink bodies** — every `[[…]]` whose resolved path was the renamed note is rewritten to the new path's shortest-unambiguous form. A bare-name link stays a bare-name link unless the rename introduces ambiguity; an explicit-path link gets its path rewritten end-to-end.
- **Trail waypoint paths** — the `hiker.references.path` field in each affected waypoint-note rewrites (per [[spec:trail-auto-update-on-note-move]]).
- **Kanban card paths** — each `cards[].path` entry in affected board-docs rewrites (per [[spec:board-card-references]]).
- **List-doc refs** — each `hiker.refs[].path` entry in affected list-docs (epics, plans) rewrites (per [[spec:pm-epic-derived-table]]).

All rewrites for a single rename commit through the op-log as one logical rename batch: each referrer's edit is its own op (so per-file history stays clean), but the indexer's referrer enumeration + edit application happens inside one transaction. A crash mid-rename leaves either all referrers rewritten or none — never dangling links. [wikilink-rename-rewrite]
status:: done
implements:: [[code:hiker/indexer/jobs/impl#[`JobCtx<'a>`]handle_rename]], [[code:hiker/indexer/jobs/handle_simple_job]], [[code:hiker/trails/ops/write_with_suppress_and_reindex_for_links]], [[code:hiker/links_rename/on_note_moved]]
note:: shared rename-rewrite pass over all four referrer types runs as one orchestrator from the indexer; each domain is best-effort (errors logged, never propagated). The fourth arm — list-doc refs — landed with [[spec:pm-epic-derived-table]] as `links_rename::run_lists_sweep` over `pm::on_note_moved` (commit 5971d3b). Wikilink bodies use `wikilink::shortest_unambiguous` for the replacement form (same picker rule autocomplete uses). The Bloom-filter optimization is intentionally skipped ([[spec:wikilink-rename-bloom-filter-deferred]]) — the straightforward `walk_indexable_files` enumerator lands first · evidence: `core/src/links_rename.rs` (`on_note_moved` → trails + boards + lists + wikilink-body sweep, `run_lists_sweep`, `rewrite_wikilink_bodies` + `splice_link_bodies`); `core/src/indexer/jobs.rs` (single call site replaces the prior `run_trails_on_note_moved` / `run_boards_on_note_moved` pair on `IndexJob::Rename` / `Move` / `MoveFolder`); `core/src/trails/ops.rs` (`write_with_suppress_and_reindex_for_links` re-export)

**Referrer enumeration** uses the structural index (per [[spec:wikilink-backlinks]]): the indexer maintains a reverse map from target path to referrer paths so a rename is one indexed query plus N writes, not a vault scan. The same index serves backlinks display. A Bloom-filter optimization is deferred ([[spec:wikilink-rename-bloom-filter-deferred]]).

**External renames** (a file changed outside hiker, e.g. via Syncthing arriving from another device or a terminal `mv`) trigger the same rewrite path through the watcher's rename detection. Unpaired Created+Deleted pairs are too speculative to treat as a rename and surface as a delete + create — the same posture trails uses today.


## Click behavior

A click resolves the path and opens the target through the shared open-note path, landing in the preview slot by default and promoting to a sticky tab on intent — identical to every other open-note entry point ([[spec:editor-preview-tab-from-open-callsites]]); Mod-click opens sticky directly. An already-open target switches to its tab rather than reopening ([[spec:multi-buffer-tree-click-switches-tab]]). [wikilink-click-open]
status:: partial
implements:: [[code:hiker/panels/buffer/wikilink_nav/open_at]]
note:: path-form click resolves through `wikilink::resolve_path` with the vault's `[wikilinks] ambiguous_resolution` policy and the referrer's path. The `WidgetClick` plumbing in `panels/buffer/mod.rs` itself is owned by the user's in-flight click-pattern refactor — left untouched · evidence: `app/src/panels/buffer/mod.rs` (`open_wikilink_at`) + `app/src/panels/buffer/wikilink_nav.rs` (`open_at`)


## Hover preview card

Hovering a resolved wikilink for ~400ms shows a small scrollable preview card with the target note's rendered content, letting the user peek at what's behind a link without opening it. [wikilink-hover-preview]
status:: done
touches:: [[code:hiker/panels/buffer/wikilink_nav]], [[code:hiker/panels/graph]]
note:: ~400ms hover on a resolved pill shows a scrollable preview card painted on a tooltip-layer over the editor. Title is frontmatter `title` if present, else basename; body is the first ~30 lines after `skip_frontmatter`. Pointer-leave dismisses after a 200ms grace (covers pill→card slide). Scroll wheel over the card scrolls only the card body. Unresolved / ambiguous pills get no card. Mod-click closes the card and opens the target sticky via [[spec:wikilink-click-open]]. Reuses `paint_preview_card` styling shared with [[spec:cluster-editor-graph-view-hover-preview-card]] · evidence: `app/src/panels/buffer/wikilink_nav.rs` (`HoverState`, `track_hover`, `resolve_preview`) + `app/src/panels/graph.rs` (`paint_preview_card_with` scrollable variant)

- **Trigger.** Pointer enters a resolved pill; card appears after the hover delay so transient sweeps don't spam previews. Pointer leave (with a brief grace window so the user can move the cursor into the card itself) dismisses.
- **Anchor.** Card paints over the editor canvas anchored to the pill's screen position, with quadrant-flip + canvas-clamp placement. Reuses `panels::graph::paint_preview_card` (same painter primitive as [[spec:cluster-editor-graph-view-hover-preview-card]]) so both consumers share styling.
- **Body source.** Frontmatter `title` (or basename) plus the first ~30 lines of body with frontmatter stripped, in a scrollable region. Scrolling inside the card scrolls only the card body, not the editor. Long titles wrap inside the card rather than expanding it.
- **Unresolved / ambiguous links** don't get a preview card — the existing unresolved-style affordance ([[spec:wikilink-unresolved]]) is the right surface for "you need to disambiguate / create this." Hovering one is a no-op.
- **Click through still opens the note** via [[spec:wikilink-click-open]] — the card is a peek-before-commit, not a replacement. Mod-click while the card is showing closes it and opens the target sticky.
- **Embed (`![[...]]`)** is the deferred sibling ([[spec:wikilink-embed]]), rendering inline rather than as a hover card.
- **Module placement.** Hover detection + card lifecycle live in the wikilink decoration layer; body resolution reuses the indexer's read path; painting reuses the shared card helper.


## Heading anchors

A link may target a heading inside a note, not just the note itself: `[[Page#Heading]]` opens the note and scrolls to the heading; `[[#Heading]]` is a same-document anchor that scrolls within the current note without opening anything. [wikilink-headings-blocks]
status:: done
implements:: [[code:hiker/wikilink/split_target_section]], [[code:hiker/wikilink/heading_slug]], [[code:hiker/wikilink/find_heading_byte]], [[code:hiker/wikilink/resolve_path]], [[code:hiker/panels/buffer/wikilink_nav/open_target]], [[code:hiker/panels/buffer/wikilink_nav/scroll_open_buffer_to_section]]
note:: `[[Page#Heading]]` and same-document `[[#Heading]]` heading anchors. The `#section` is split off the target (`split_target_section`); the page resolves as before, then `find_heading_byte` locates the first ATX heading whose GitHub slug (`heading_slug`: lowercase, drop non-`[a-z0-9 -]`, spaces→`-`, fenced code skipped) equals the anchor's slug. Navigation places the caret on the heading line and sets `view.scroll_caret_into_view` so the heading lands near the top once the (possibly just-opened) buffer's height map is built. `[[#Heading]]` (empty page) scrolls within the current buffer with no open. A `#section` that matches no heading is a graceful no-op (note opens at top). Block anchors are the sibling slug [[spec:wikilink-block-anchors]] · evidence: `core/src/wikilink.rs` (`split_target_section`, `heading_slug`, `find_heading_byte`; `resolve_path` drops the `#section` before page matching); `app/src/panels/buffer/wikilink_nav.rs` (`open_target`, `scroll_open_buffer_to_section`, `scroll_buffer_to_byte`, `title_resolver` strips the anchor for the pill label)

- **Anchor split.** The `#section` trailer is split off the target before resolution (`split_target_section`): the page part (before the first `#`) resolves to a note exactly as a page-level link does — the anchor never changes *which* note a link points at, only where navigation lands. An empty page (a bare `#Heading`) means "this document."
- **Slug matching.** The anchor is matched against the note's headings by **GitHub's heading-slug algorithm** (`heading_slug`): lowercase, drop every character outside `[a-z0-9 -]`, turn whitespace runs into single hyphens. The first ATX heading (`#`…`######`) whose slug equals the anchor's slug wins; fenced code blocks are skipped so a `#` inside a ``` fence is never read as a heading. Slug matching means an anchor copied from a rendered view (`#some-heading`) resolves the same as one typed against the heading text (`#Some Heading`).
- **Navigation.** Resolution yields the heading's byte offset; the caret is placed there and a scroll-into-view is requested, so the heading sits near the top of the viewport once the (possibly just-opened) buffer's layout is built. `[[#Heading]]` scrolls the current buffer directly.
- **Graceful miss.** An anchor that matches no heading is a no-op for scrolling — the note still opens at the top rather than erroring. A `Page#Heading` whose *page* is unresolved follows the normal unresolved/create flow.


## Block anchors

A link may target a specific block — a paragraph, list item, or line — rather than a heading: `[[Page#^blockid]]` opens the note and scrolls to the block carrying that marker; `[[#^blockid]]` is a same-document block anchor. The markdown-link form `[text](Page#^blockid)` resolves identically. [wikilink-block-anchors]
status:: done
touches:: [[code:hiker/panels/buffer/wikilink_nav]], [[code:hiker/wikilink]]
note:: `[[Page#^blockid]]`, same-document `[[#^blockid]]`, and `[text](Page#^blockid)` block anchors. A block is tagged by a trailing whitespace-preceded ` ^blockid` token at the END of its line (id charset `[A-Za-z0-9-]`); `find_block_byte` matches the id **exactly** (not slugged), skips fenced code, and returns the start byte of the marked line so navigation lands at the block top. `split_target_section` splits the `#^blockid` trailer; `block_anchor_id` classifies it (vs a heading slug); `anchor_byte` is the one funnel the same-document + post-open scroll paths share. A `^blockid` matching no block is a graceful no-op (note opens at top). The trailing marker conceals off the cursor line ([[spec:wikilink-block-marker-conceal]]); link-side detection is unchanged in `editor-md` (the page split already strips `#^block`) · evidence: `core/src/wikilink.rs` (`block_anchor_id` classifies a `^`-prefixed section + returns the raw id, `find_block_byte` locates the marked line, `line_block_id`/`is_block_id_byte` helpers); `app/src/panels/buffer/wikilink_nav.rs` (`anchor_byte` routes `#section` to block-vs-heading finder; `open_target`, `scroll_open_buffer_to_section`, `title_resolver` all anchor-kind-aware)

- **The block marker.** A block is tagged by a trailing ` ^blockid` token at the END of its line: `Some paragraph text. ^abc123`. The marker is whitespace-preceded and the last token on the line; the id charset is `[A-Za-z0-9-]`. A bare `^id` line (no preceding text) or an `^id` glued to a word / sitting mid-sentence is not a marker.
- **Anchor split + classify.** The `#^blockid` trailer splits off the target the same way a heading anchor does (`split_target_section`), then `block_anchor_id` classifies a `^`-prefixed, well-formed section as a block anchor (vs a heading slug) and returns the raw id. The page part resolves to a note exactly as a page-level link does; the anchor only steers where navigation lands.
- **Block matching.** `find_block_byte` scans for the first line whose trailing marker id equals the link's id **exactly** (not slugged — the id is an explicit handle, not derived from prose). Fenced code blocks are skipped, so a `^id` token inside a ``` fence is never read as a marker. The returned byte offset is the start of the marked line, so navigation lands at the top of the block.
- **Navigation.** One funnel (`anchor_byte` in the nav layer) routes a `#section` to `find_block_byte` when `block_anchor_id` matches, else to `find_heading_byte`, so the same-document and post-open scroll paths agree. `[[#^blockid]]` scrolls the current buffer directly; `[[Page#^blockid]]` opens the page first.
- **Graceful miss.** A `^blockid` matching no block is a no-op for scrolling — the note opens at the top, same posture as a heading miss.
- **Marker conceal.** Off the cursor line the trailing ` ^blockid` marker conceals so it reads as prose, not noise; the cursor landing on that line reveals the raw marker for editing — the same live-preview reveal every other markup uses. Disambiguation reuses the exact read-side predicate (`line_block_id` / `block_anchor_id`): only a whitespace-preceded last-token `^[A-Za-z0-9-]+` on a non-empty line conceals, so an incidental `^` in prose (`2^10`, a mid-line `^`) and a bare `^id` line never do. Fenced code is skipped. The conceal lives in the renderer-unaware `editor-md` decoration layer (no `hiker-core` dep), so detection and conceal share one rule. [wikilink-block-marker-conceal]
status:: done
touches:: [[code:hiker/links]], [[code:hiker/completion_sources]], [[code:hiker/ops/file]], [[code:hiker/panels/buffer/wikilink_nav]], [[code:hiker/wikilink]]
note:: The trailing ` ^blockid` marker conceals (a `Replace { display: None }` over the whitespace+`^id` token) on every off-cursor line carrying a well-formed marker, and reveals (no conceal) when the cursor is on that line — the same live-preview reveal the other markup uses. Disambiguation reuses the exact read-side predicate (`core::wikilink::line_block_id`/`block_anchor_id`, mirrored locally so `editor-md` keeps no `hiker-core` dep, the `is_external_link_dest` posture): a marker is the whitespace-preceded last token `^[A-Za-z0-9-]+` on a non-empty line, so incidental `^` in prose (`2^10`, `a ^ b`, mid-line `^`) and a bare `^id` line never conceal. Fenced code is skipped · evidence: `editor/editor-md/src/links.rs` (`trailing_block_marker` predicate, `block_marker_decorations`; called from `wikilink_decorations`)
- **Auto-injection.** Authoring a link to a block offers a picker: `[[Page#^` enumerates the target note's blocks by preview text. A block that already carries a marker is offered with its existing id (reused, never duplicated); an un-anchored block is offered with a fresh content-addressed id, and on pick the marker ` ^id` is injected onto that block in the target note so the link resolves. The id is the block's content hash, so the chosen block is re-located from the id alone — no stored side-channel. The injection rides the normal edit path: an open target (including a same-document `[[#^` pick of the current buffer) is edited through an undo/op-log transaction, a disk-only target through the same watcher-suppress + reindex write the rename-rewrite pass uses. A plain `[[Page#` (no `^`) offers the target's headings instead. [wikilink-block-anchor-autoinject]
status:: done
touches:: [[code:hiker/completion_sources]], [[code:hiker/ops/file]], [[code:hiker/panels/buffer/wikilink_nav]], [[code:hiker/wikilink]]
note:: Authoring a link to a block: typing `[[Page#` offers the target's headings, `[[Page#^` offers its blocks (preview text per block). An already-anchored block reuses its `^id`; an un-anchored block is offered with a fresh content-addressed id (`fresh_block_id`), and on commit the per-frame `reconcile_block_anchors` pass injects ` ^id` onto the matching block in the target note — re-located from the id alone since the id is the block's content hash. Same-document `[[#^` picks current-buffer blocks; an open target is edited via an `Input` transaction (rides undo/op-log), a disk-only target via the `core::ops::file::inject_block_marker` op (watcher-suppress + reindex, the cross-note write path the rename-rewrite pass uses). Edge cases: existing-id blocks reuse (never duplicate), an id matching no block is a no-op · evidence: `core/src/wikilink.rs` (`scan_blocks` lists `(range, preview, existing_id)` per block, `fresh_block_id` mints a collision-free content-addressed id, `inject_block_marker` appends ` ^id`); `app/src/completion_sources.rs` (`anchor_matches`/`resolve_page_body` switch the `[[` picker to headings on `#` and blocks on `#^`; `block_items`/`heading_items`/`scan_headings`); `core/src/ops/file.rs` (`inject_block_marker` op = suppress→write→reindex); `app/src/panels/buffer/wikilink_nav.rs` (`reconcile_block_anchors` + helpers inject on commit)


## Code links

A link may reference a code entity through the code-intelligence graph: `[[code:<repo_id>/<symbol>]]`. The `code:` prefix marks the namespace, the FIRST `/` splits the repo id from the symbol (which may itself contain `/`, `#`, `.`, bracket groups, and backtick spans), and `<symbol>` is the entity's canonical **short descriptor path** (the `short_sym` form — `trails/ops/delete_trail`): the body format every spec doc's `implements::` / `touches::` / `verifies::` link lines use (`status.md`), reconciled into the drift baseline by the code-intel tooling. Clicking resolves the symbol through the repo's bound SCIP adapter and opens the code-graph tab preselected on it. The stored body is the canonical form and is never rewritten for display — it must round-trip through resolve/locate/fingerprint. [spec-code-link]
status:: done
implements:: [[code:hiker/wikilink/parse_code_target]], [[code:hiker/panels/buffer/wikilink_nav/open_code_target]]
touches:: [[code:hiker/code_sources]]
note:: anchor added for the long-standing comment-only slug; `parse_code_target` splits repo id / symbol, `open_code_target` binds the adapter and navigates · evidence: `core/src/wikilink.rs` (`parse_code_target`), `app/src/panels/buffer/wikilink_nav.rs` (`open_code_target`), `app/src/code_sources.rs` (`resolve_or_bind`)

- **Nested brackets and backtick spans parse.** A code body may be an impl-qualified moniker — ``impl#[`Builder<'a>`]top_level_split``, carrying nested `[`/`]` groups and backtick spans — so the `[[…]]` scanner gives the `code:` namespace a depth-aware matcher: bracket depth is tracked, backtick spans are opaque, and the link closes on the first `]]` at depth zero outside backticks. Ordinary (non-`code:`) bodies keep the strict flat rule (a stray `]` rejects), so vault links parse exactly as before. [wikilink-code-nested-brackets]
status:: done
implements:: [[code:hiker/wikilink/parse_links]]
note:: fixes `bug-code-wikilink-impl-moniker-not-parsed` (the four `cluster/build` impl bodies rendered as plaintext). Reconcile's own parser already accepted these bodies — the divergence was core-side; the editor decoration scanner's companion fix landed (editor repo cc6ca1e, `code_body_close` ported byte-for-byte + 5 tests) — both scanners agree · evidence: `core/src/wikilink.rs` (`parse_links`, `code_body_close`; unit-tested over the four known bodies)
- **Pretty pill label.** The rendered pill shows a friendly label derived from the moniker — `Builder::top_level_split` for an impl-qualified body (backticks stripped, generic arguments and the trait group dropped; the first bracket group is the self type), `Type::member` for `Type#member`, otherwise the last path segment — the way vault wikilink pills show live titles rather than stored paths. Display-only: the stored body stays the canonical short-sym form. [wikilink-code-pretty-label]
status:: done
implements:: [[code:hiker/panels/buffer/wikilink_nav/title_resolver]]
touches:: [[code:hiker/wikilink]]
note:: `code_link_label` derives the label; `title_resolver` returns it for `code:` targets (replacing the raw last-segment fallback) · evidence: `core/src/wikilink.rs` (`code_link_label`, `impl_self_type`; unit-tested), `app/src/panels/buffer/wikilink_nav.rs` (`title_resolver`)
- **Crate-qualified bodies.** A short path that names a symbol in more than one crate (`trails` lives in both `hiker-core` and `hiker-app`) can never resolve — reconcile reports it ambiguous and refuses to pick a winner. The disambiguating body form prefixes the crate name: `[[code:hiker/hiker-core/trails]]` — the moniker's SCIP `<package>` slot, which the plain short form drops. Both forms are indexed by one shared builder, so authoring, reconcile, and click-resolution agree on what a body is. [code-link-crate-qualified]
status:: done
touches:: [[code:hiker/scip_adapter]]
note:: `crate_qualified_sym` derives `<crate>/<short>`; `index_short_forms` folds both forms into the adapter's `by_short` and reconcile's doc-link index with the same collision→ambiguous rule per form · evidence: `code-intel/hiker-code/src/scip_adapter.rs` (`crate_qualified_sym`, `index_short_forms`; unit-tested), `code-intel/hiker-code/examples/reconcile_docs.rs`


## Spec links

A link may reference a spec feature by its stable kebab-case slug: `[[spec:trail-delete-cascade]]` opens the note whose bracketed [[spec:trail-delete-cascade]] anchor defines that spec, scrolled to the defining line. Like `[[code:…]]`, the `spec:` prefix marks a namespace resolved through a dedicated resolver rather than page-path resolution — and the resolution is **by anchor search, not location**, so spec links are positional-free: re-homing a spec entry between docs never breaks references, the same property the slugs themselves carry. Resolution is accelerated by the indexer's spec-anchor index (`index.md` [[spec:spec-anchor-index]]): one indexed store lookup per click, no vault walk. [wikilink-spec-links]
status:: done
implements:: [[code:hiker/wikilink/parse_spec_target]], [[code:hiker/wikilink/find_slug_anchor_byte]], [[code:hiker/panels/buffer/wikilink_nav/open_spec_target]], [[code:hiker/panels/buffer/wikilink_nav/pick_anchor_path]]
touches:: [[code:hiker/panels/buffer/wikilink_nav]]

- **Form.** `[[spec:<slug>]]`; the slug charset is `[a-z0-9-]` with at least one dash — the same token rule the spec engine's reconcile uses to recognize `[slug]` anchors, so anything addressable is also linkable. A malformed body (uppercase, spaces, no dash) is not a spec link and falls through to ordinary page resolution.
- **Resolution.** The slug is looked up in the store's `spec_anchors` index (re-derived on every ingest, so it tracks edits/renames/deletes). When more than one note defines the anchor, the referrer's folder wins, else the lexicographically first path — deterministic, mirroring the spec engine's resolution posture.
- **Landing.** The anchor line is found live in the opened buffer (`find_slug_anchor_byte`: first line carrying the bare `[slug]` token outside fenced code), then the caret/scroll funnel heading anchors use — robust against index/disk lag because the byte offset is never stored.
- **Pill + hover.** The pill renders the slug as its label; hovering previews the defining note, same as a resolved page link.
- **Graceful miss.** A slug the index doesn't know toasts ("spec slug not found") rather than offering note creation — a spec link names a definition, and creating an empty note would not define it.
- **Deferred.** `[[spec:` autocomplete enumerating the index's known slugs (the read side is a `SELECT DISTINCT slug` away); spec→spec edges reconciled into the typed graph.

## GitHub-style markdown links

Standard CommonMark inline links `[text](target)` to vault notes resolve and navigate through the *same* path resolution and heading-anchor logic as `[[…]]` — there is one resolver, not two. [markdown-link-vault-nav]
status:: done
touches:: [[code:hiker/links]], [[code:hiker/panels/buffer/wikilink_nav]], [[code:hiker/styling]]
note:: GitHub-style inline links `[text](Page)` / `[text](Page#Section)` to vault notes resolve and navigate through the same `core::wikilink` path + section-anchor logic as `[[…]]`. A markdown link whose dest is non-external (not `http(s)`/`mailto:`/`zim://`) renders as a clickable note pill labelled with its `[text]`, tagged `WIKILINK_WIDGET_TAG` so it rides the existing wikilink click bucket/handler (no new WidgetClick plumbing). External-dest links keep the standard markdown decoration. Shared resolution: both link kinds funnel into `wikilink_nav::open_target` · evidence: `editor/editor-md/src/links.rs` (`is_external_link_dest`, `parse_md_link`, `emit_md_link_pill`; `wikilink_decorations` claims vault-target md links); `editor/editor-md/src/styling.rs` (`Tag::Link` skips non-external dests so the layers don't double-decorate); `app/src/panels/buffer/wikilink_nav.rs` (`markdown_link_dest_at`, `open_at` falls back to the md-link dest)

- **Vault vs external.** A markdown link whose destination is non-external — not `http(s)://`, `mailto:`, or `zim://` — is treated as a vault target: a bare name, a relative path, or either with a `#section` anchor (`[text](Page#Section)`, `[text](Page#^block)`, `[text](#Section)`). External-destination links keep the standard markdown rendering and the OS-open behavior; only vault-target links become clickable note pills.
- **Shared resolution.** The page part resolves through `wikilink::resolve_path` (bare-name / explicit-path / ambiguity policy) and the `#section` anchor through the same heading-slug / block-id matching as wikilinks. Click handling funnels both link kinds into one open path, so ambiguity policy, create-on-miss, and anchor scroll all behave identically.
- **Rendering.** A vault-target link renders as a pill labelled with its own `[text]` (markdown links carry their display text directly, unlike wikilinks which resolve a title); a destination the index can't place renders in the unresolved style. The pill rides the existing wikilink click path rather than a separate plumbing layer.


## Backlinks

The structural index records each resolved wikilink as a typed edge (source path → target path). The set of notes linking *to* the active note is surfaced in the discovery panel alongside search results and related notes (`search.md`). Backlinks rewrite naturally when either endpoint renames — both halves go through the rename-rewrite pass. [wikilink-backlinks]
status:: partial
note:: scan now resolves every parsed link via `wikilink::resolve_path` and matches against the active path. Still O(total bytes) per scan — the spec'd reverse-index (`structural index records source-path → target-path edges`) lands with [[spec:wikilink-rename-rewrite]] · evidence: `app/src/panels/backlinks.rs` (`scan_backlinks`)


## Unresolved links

A wikilink whose target can't be resolved — a name with no match, an explicit path that doesn't exist, or an ambiguous name under the `"unresolved"` policy — renders in a distinct unresolved style rather than a normal pill. Clicking offers:

- For a no-match link: "Create note at `<inferred path>`" (the bare-name case infers `<linking-note's-folder>/<Name>.md`; explicit-path links use the exact path) → creates the file empty and resolves the link.
- For an ambiguous link under `"unresolved"`: a picker of the matching notes so the user can rewrite to the explicit-path form. [wikilink-unresolved]
status:: partial
touches:: [[code:hiker/links]], [[code:hiker/panels/buffer/wikilink_nav]]
note:: unresolved style + create-on-click stays; ambiguous-under-unresolved currently surfaces as a toast ("Multiple notes named …") — the disambiguation picker UI is the remaining gap · evidence: `editor/editor-md/src/links.rs` (`COLOR_WIKILINK_UNRESOLVED`); `app/src/panels/buffer/wikilink_nav.rs` (`open_at` → `create_and_open`)


## Consumers

Path-form is the single representation every link producer emits:

- **Trails** and **kanban** reference notes by path in YAML (`hiker.references.path` for trail waypoints, `cards[].path` for kanban cards) — the same path-based identity wikilinks use. The picker UI and rename-rewrite machinery is shared.
- **MCP** returns paths as the stable handle on every note-shaped result.


## Deferred

- **Embeds / transclusion** — `![[Name]]` rendering the target's content (or an image/PDF) inline rather than as a link. [wikilink-embed]
status:: planned
note:: deferred — `![[Name]]` transclusion rendering the target's content/image/PDF inline
- **Bloom-filter optimization** for referrer enumeration on rename — [[spec:wikilink-rename-bloom-filter-deferred]]. Build the straightforward index-driven version first; add the filter if profiling shows it matters.


## Out of scope

- **Opaque-ID-based links** (the prior ULID model). Rejected for visible stamping and opaque identifiers; path-based identity keeps notes clean and round-trippable at the cost of one referrer-rewrite pass per rename.
- **The vault-wide graph view.** Wikilinks are an edge source for it, but the graph view is its own `design.md` feature.

## Registry imports (from status.md)

Entries imported from the retired status registry that had no anchor in this doc —
re-home them into the relevant sections as the doc evolves.

- **wikilink-rename-bloom-filter-deferred** — deferred optimization: Bloom over "note contains any wikilinks" to skip the no-link majority on rename; build the straightforward enumerator first [wikilink-rename-bloom-filter-deferred]
  status:: planned
