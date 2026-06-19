# Design

## Project name (working): Hiker

The system has a genuinely spatial-navigation shape — embedding space, anchors, ordered paths through content, exploration vs. targeted retrieval. Hiker is a brand for that vocabulary.

Earned vocabulary (use these in UI and docs):

- Trail — memex-style ordered walk through the vault (first-class concept in the index model)
- Landmark — pinned anchor; other notes get a nearest-landmark tag in embedding space
- Map — the graph visualization

Keep neutral terms elsewhere: vault, note, folder, search. Don't force hiking words onto things that aren't spatial (the data-structure tree, the ingestion pipeline, the editor).

Sanity notes:

- Crate name `hiker` is taken on crates.io by an unrelated small crate — would need a suffix (hiker-notes, hiker-vault, hiker-rs)
- CLI command `hiker` or `hk` both feel natural


## Target stack

A Reor-like personal notes + knowledge system, all-Rust.

- UI shell: **egui** via eframe. Single native binary; no webview.
- Editor: in-tree widget under `editor/` — `editor-core` (rope, `EditorState`, `Selection`, `Transaction`, `Decoration`/`DecorationSet`), `editor-view` (commands, decoration providers, `ViewState`, completion source trait), `editor-egui` (input translation + painter), `editor-diff` (read-only unified-diff view), `editor-md` (markdown indent; live-preview decorations live in `app/`).

Live preview: per-frame decoration providers in `app/src/panels/buffer.rs` produce `DecorationSet` layers fingerprint-cached on `(path, selection, folds, viewport, theme)`. Decoration kinds: `Mark`, `Line`, `Replace { display }`, `Block`, `Widget`. The markdown provider walks the buffer with `pulldown-cmark`; `Replace` fades syntax markers, `Mark`/`Line` styles content, and a decoration is suppressed when its line overlaps a selection so clicking in reveals raw markers. Widgets handle images, math, wikilink pills, callouts.

Wikilinks: the markdown decoration provider emits a widget for `[[Name]]` / `[[folder/Name]]`; click resolves the path via `core::store`. `[[` opens an autocomplete popup driven by the indexer path cache (`editor-view`'s `CompletionSource` trait; `app::completion_sources::WikilinkSource`). Backlinks surface in the discovery panel alongside search results / related notes (`search.md`). Full spec: `wikilinks.md` (path form, autocomplete, render-from-live-title, rename-rewrite, backlinks).

Other components:

- Filesystem watcher: notify crate
- Markdown parsing/chunking: pulldown-cmark or comrak
- Vector store: sqlite + sqlite-vec
- Full-text search: tantivy (hybrid with vector for best results)
- Embeddings: local fastembed-rs by default (`core::embed::FastembedEmbedder`); cloud / Ollama options via `core::embed::LlmEmbedder` (wraps the `llm` crate's `EmbeddingProvider`). Both behind the same `Embedder` trait — see `index.md`'s embedder section.
- MCP server: rmcp (official Rust SDK) — the sole agent surface (`mcp.md`); off by default, opt-in per vault
- Ingestion: hiker does **no** in-process extraction. External tools (working name *trailblazer*) produce content + a manifest hiker imports; the markdown shadow is indexed, artifacts land under `.hiker/refs/` (`import.md`)


## Crate layout

```
core/             vault model, chunker, indexer, search, llm (background/fan-out), staging, trees, autosave, snapshots — pure library, no UI deps
cli/              clap-based CLI, calls core
mcp-server/       rmcp adapter (the sole agent surface), calls core
hiker-llm/        the multi-provider generative client (shared by core + crawler; confines the `llm` crate)
hiker-git/        libgit2 wrapper for the optional, user-driven git integration
app/              egui desktop app — tabs, panels, sidebar, toolbar, settings, modals; holds long-lived subsystems on AppState, pumps mpsc channels each frame
editor/
  editor-core/    rope, EditorState, decoration model, selection, transactions — pure data
  editor-view/    commands, decoration providers, completion source trait, search, multi-cursor — platform-agnostic
  editor-egui/    the egui widget: input translation, painter-based rendering
  editor-diff/    PreviewBuffer + unified-diff renderer reusing the widget surface
  editor-md/      markdown indent provider (live-preview decorations live in app/)
```

Discipline: `core/` has no UI deps. `editor-*/` knows nothing of `core` or `app` — reusable, could be lifted out. `app/` glues them: it's where multi-subsystem coordination (open buffer, resolve wikilink, run mutation, accept proposal) lives. Panel renderers stay thin — read `AppState`, fold pending channel events, draw, route mutations back into subsystem APIs.


## Vault layout

```
notes/
  inbox/         unstructured dump, AI-organized later
  personal/
  work/
  school/
  archive/
  project-x/
    overview.md
    attachments/
      diagram.png
      diagram.png.md          sidecar with OCR + frontmatter
      contract.pdf
      contract.pdf.md         sidecar with extracted text
      meeting-2026-04-12.m4a
      meeting-2026-04-12.md   sidecar with whisper transcript
```

External-file ingestion: a configurable list of absolute paths / globs outside the vault. Indexer watches them, embeds and indexes their contents, but never writes to the originals.


## Source-derived notes

Unifying model: every searchable thing in Hiker is a note. Non-md sources and external files get a hiker note that gives them a stable identity inside the system; the body of that note depends on the source type.

The note's frontmatter is the same shape across cases:

```yaml
---
hiker:
  source: <absolute path>
  source_sha256: <hash>
  source_mtime: <iso8601>
  type: pdf | image | audio | markdown | docx | html | code | ...
  storage: sidecar | hiker-owned | external-pointer
tags: [...]
---
```

Identity is the note's vault path ([[spec:op-log-path-identity]]) — there is no minted id and no path→id table. A rename is an observed content-preserving move ([[spec:op-log-observed-move]]) that moves the note and rewrites references. Hiker-owned sidecar/cache filenames derive from the source basename (a debuggable slug), e.g. `design.md.pdf` → `design.md.pdf.md`; no id is embedded.

Storage modes (each row maps unambiguously to one combination of source-location × type × versioned):

| Source location | Source type | Versioned? | Mode               | Where the note lives                              | Body                                          |
| --------------- | ----------- | ---------- | ------------------ | ------------------------------------------------- | --------------------------------------------- |
| Vault-internal  | markdown    | no         | (none)             | the file itself is the note                       | the file's own contents                       |
| Vault-internal  | non-md      | no         | `sidecar`          | next to source as `<full-source-filename>.md`     | extracted text (cached)                       |
| Imported        | web/other   | no         | `imported`         | a visible note in the vault, original/archive beside it | a markdown shadow; original opens in its viewer (`import.md`) |
| External (file) | markdown    | no         | `external-pointer` | `.hiker/external/<slug>.md`                       | annotations only; original re-read on refresh |
| External (file) | non-md      | no         | `external-cached`  | `.hiker/external/<slug>.md`                       | extracted text (cached)                       |
| Either          | any         | yes        | `versioned`        | sidecar note (snapshot history) + `.hiker/refs/<sidecar-path>/` retained artifacts | imported text, versioned via the ordinary save path; old artifacts kept per `extract-artifact-retention` |

Notes:

- Vault-internal markdown needs no source-derived note — the file is the note. All other rows produce a hiker note.
- **Imported** content (a web page, a crawled site, produced by an external tool — `import.md`) lands as a visible note, not a hidden cache file. The `external-cached` / `external-pointer` rows cover **external files on disk outside the vault** that hiker watches read-only, a distinct case from imported content.
- External-pointer is the only file mode without a content cache; markdown is already plain text and cheap to re-read, so caching adds drift without benefit.
- Versioned mode is reached by opt-in (per-glob in vault config or per-source frontmatter); it supersedes sidecar / external-cached / external-pointer when active.

**Subsystem notes are first-class visible files, typed by frontmatter.** Any document a subsystem produces that is *user-created or imported content* — trail waypoints, captured pages, cluster-tree presets, cluster trees, boards — lives at a real vault path and is an ordinary indexed note. A note's *type* is carried in its `hiker.kind` frontmatter (`board` / `cluster-tree` / [[spec:cluster-preset]] / …) and discovered through the store's frontmatter index ([[spec:store-note-query]]), never inferred from a hiker-owned location. The load-bearing consequence: a note the user hand-typed or imported with the right frontmatter is treated identically to one hiker authored — there is no hidden registry that confers special status, only the note's own frontmatter (the one bounded exception is the kinds *schema*, which legitimately lives in `.hiker/config.toml`; the kind *data* is plain frontmatter — `kinds.md`). `.hiker/` never holds user-created or imported notes; it holds a small, named **durable** set plus regenerable cache. **Durable** (not reconstructible from the `.md` alone): un-accepted edits in `.hiker/pending/` (`op-log.md`), imported binary artifacts in `.hiker/refs/` (`import.md`), the agent-call provenance log `.hiker/agent-log/` (`llm.md`), and trash in `.hiker/trash/`. **Regenerable cache:** the snapshot history `.hiker/history/`, `index.db`, the embedding cache, and `.hiker/autosave/`. So the watcher needs no per-subsystem carve-out — nothing indexable lives under `.hiker/`. [subsystem-notes-visible]
status:: partial
note:: principle (design.md): any subsystem doc with a user-authored body (trail waypoints, captured pages) is a first-class visible note at a real vault path; `.hiker/` holds only a named durable set (`pending/`, `refs/`, `agent-log/`, `trash/`) + regenerable cache (`history/` snapshots, `index.db`, embeddings, `autosave/`). PROGRESS: trail waypoints migrated to visible companion folders (`bug-trails-waypoints-to-companion-folder` resolved); chat sessions removed entirely (no in-app chat after the rework) · evidence: `core/src/trails/` (waypoints in the trail-doc's visible companion folder, [[spec:trail-storage-layout]])

Source/binary types are converted by external producers or handled by core viewers (`import.md`):

- HTML / web archives — produced externally; displayed via `hiker-htmlview`, indexed from the markdown shadow
- PDF — text layer indexed like `.txt` (cheap deterministic chunking); the PDF opens in its viewer
- Image / audio — OCR / transcription via external producers; the artifact opens in its OS handler
- Code files — indexed directly; optional LLM-summary sidecar for semantic discoverability

External file interaction:

- Read-only from Hiker. Hiker's editor never opens external files.
- Search results show the source path with a snippet; clicking opens the file in the OS handler (xdg-open / equivalent), not in Hiker.
- Watcher follows external paths; on change → re-read, re-chunk, re-embed.
- Trails and links reference the hiker note by its vault path.
- On missing source → mark `orphaned: true` in frontmatter, stop refreshing, keep the index entries so prior search/links/trails don't break. User decides whether to delete or fix the path.

UI affordance: hide hiker-owned sidecar files (`*.<ext>.md` next to non-md sources, plus `.hiker/` contents) from the file tree by default; surface them via "view extracted text" actions on the original and in search results.

**Linked vs. unlinked sidecars.** Every extracted-text sidecar (sidecar / external-cached / versioned modes) carries a `hiker.link_state: linked | unlinked` field, default `linked`. Semantics:

- **Linked (default)** — the sidecar is read-only in hiker's editor. A re-import (the producer re-emitting the manifest) overwrites the sidecar's body in place through the ordinary note-write path, so the prior body stays in the plain-file snapshot history rather than in a separate version file. The user's role with a linked sidecar is reading + annotating the *source* via trails / links / search, not editing the imported text.
- **Unlinked** — explicit user action ("Unlink from source") flips the sidecar to RW. Hiker stops overwriting it on re-import (the sidecar is now diverged from source by user choice). The relationship to the source survives in frontmatter (`hiker.source`, `hiker.source_sha256` at the time of unlink), but re-imports of the source no longer touch this sidecar's body. Rationale: an escape hatch for cases where the producer mangled content and the user wants to fix it by hand.

Re-link is supported (flips back to linked + re-imports to overwrite local edits — confirm modal, since this discards the user's hand edits). Link-state is a property of the sidecar document, not of any one capture.


## Versioned sources

The version history of a source-derived note is the plain-file snapshot history (`op-log.md` "Local history"), with optional git when integrated (`git.md`) — not a parallel per-version store. A sidecar is an ordinary `.md` text file; a re-import (changed source, re-fetch) is the producer re-emitting the manifest, landed through the normal note-write path. So a source's "versions" are its snapshots, and diff / restore / the version dropdown reuse the existing surfaces. An identical re-import is a no-op, so versions accrue only on real change. Hiker performs **no in-process extraction**; re-import policies and retention live in `import.md`.

- **Logical documents spanning many sources** (a crawl, a multi-file capture) are represented by a manifest note; members carry `hiker.parent: <manifest-path>`. A single scraped or dropped source needs no manifest — the sidecar note is itself the versioned unit.
- **Binary artifacts** (the source bytes, the per-capture HTML archive) are what the snapshot history can't hold — it versions text, not blobs. Whether old artifacts are retained is a per-source retention cascade (`extract-artifact-retention`): vault default → per-crawl/glob → per-source frontmatter; values `latest` / `keep:N` / `forever`. Retained artifacts live under `.hiker/refs/<sidecar-path>/` keyed by the sidecar's vault path (consistent with path identity), and are device-local.
- **Search** indexes the current accepted state (what's on disk); historical versions live in the snapshot history and surface on demand rather than as separate default-search hits. Trails reference a note live by its vault path, like any other link — a waypoint is a note reference, not a pin to a historical version.


## Index model

Two orthogonal axes: index type, and index level (granularity within a type).

Types (parallel indexes over the same content):

- Lexical — tantivy/FTS over raw tokens. Exact matches, names, code, command snippets.
- Semantic — vector embeddings of chunks/notes. "Find related notes about X."
- Structural — graph of links, headings, tags, folder paths. "What references this," "what's under this heading."
- Temporal — by mtime/ctime or explicit dates in frontmatter. "What was I working on last Thursday."
- Entity — extracted named entities (people, projects, places) with their own embeddings/aliases. "Everything about Alice" regardless of phrasing.
- Provenance — source of ingestion (apple-notes-export, claude-code-transcript, OCR, audio, external-file, user-authored, agent-authored). Two filter axes ride this: the specific provenance label (fine-grained, "show me everything from this Apple Notes export"), and a coarser **authorship trichotomy** — `user-authored / agent-authored / imported` — for the everyday surfaces ("show me only my own writing," "show me only what got pulled in from outside"). Stored as `hiker.provenance:` (specific) and `hiker.author:` (coarse) in frontmatter. Default for hand-typed notes is `user-authored`; the import paths (scrape, drag-and-drop, transcript ingestion) stamp `imported`; agent writes via MCP stamp `agent-authored`. Surfaced in the file tree via per-source-type icons (see trails seedling for icon shape) and filterable in the discovery panel (`search.md` deferred slugs [[spec:search-authorship-filter]] + [[spec:search-source-type-filter]]).

Levels (granularity, applies within a type — primarily semantic):

- Chunk-level — paragraphs / heading-bounded sections. Highest recall, noisiest.
- Note-level — one embedding per file. Good for related-notes without chunk noise.
- Cluster-level — embeddings of groups of related notes (computed offline). Good for "what topics exist."
- Vault/folder-level — one embedding per folder/vault. Good for routing a query to the right vault first, then drilling down.

Query pipeline (compose as needed):

- Route → vault/folder-level picks top-k vaults
- Recall → lexical + semantic + entity in parallel at chunk level within those
- Fuse → reciprocal rank fusion across types, group by parent note
- Rerank → optional cross-encoder pass on top N
- Expand → pull sibling chunks and linked notes via structural index for context

User-authored layer on top of the automatic indexes:

- Collections / saved queries (named groupings — tags, folder globs, manual note IDs + order)
- Tree-driven auto-organization and inbox triage — durable cluster trees with per-cluster Tag / Move policies — see "Auto-organization" below, and `cluster-editor.md` for the full surface
- Pinned anchors / landmarks (other notes get a "nearest landmark" tag in embedding space)
- Trails: ordered, named sequences of notes with per-waypoint annotations and side-trail branches (the waypoint list is a tree). Curated — the user owns every accepted trail, but the clustering pipeline and MCP agents may propose drafts into a trail-scoped review queue. Full surface in `trails.md`.

  **Drag-and-drop ingestion (deferred).** Trails should accept items dragged in from outside the trails panel — primarily file rows from the Files panel, and eventually note tabs and search-result cards. The drop target semantics match the in-panel reorder DnD already shipped: dropping onto the top half of a waypoint card inserts the new waypoint as a sibling before it; dropping onto the bottom half nests it as a child; dropping on the head / tail strips lands it at the start / end of the trail. Implementation is deferred — the in-panel reorder lands first; cross-panel ingestion piggybacks on the same drop zones once a uniform "vault path" drag payload is in place across panels. [trails-dnd-ingestion]


## Auto-organization

Hiker's clustering pipeline (`clustering.md`) produces **trees** — a durable, user-authored organizing layer curated over the notes. Each cluster can carry a **policy** (Tag / Move / Freeze) that drives real changes: a Move policy renames the file into a folder, a Tag policy writes the note's frontmatter. The filesystem still holds the notes' bytes; the tree is the structure organizing them, and several trees can overlap the same vault (e.g. one by project, one by topic). The cluster editor is the surface (`cluster-editor.md`).

Two ways policies fire:

- **One-shot Apply** — build (or hand-author) a tree, assign policies, and Apply: each policied leaf produces a reviewable move/tag op. Reversible; nothing touches the vault until you accept.
- **Saved-tree triage** — save a tree as a classifier. New notes (default scope: `inbox/`) get routed against it via centroid descent ([[spec:cluster-place-beam-descent]]) and the matched cluster's policy fires. Matches stay **pending for review by default** (`[triage].review_required = true`); auto-apply is the per-tree opt-in once a classifier is trusted.

Triage will not move a note out of any folder *other* than the configured scope — the worst case for an over-eager classifier is "wrong subfolder under `inbox/`," never "your important note got moved out from under you." That's the load-bearing safety rule.

See `cluster-editor.md` for the full surface — the policy model, apply + batch review, tag-field configurability, and triage scope/scheduling.


## Enrichment pipeline

A stage that runs over notes (on ingest, on save, on demand via `hiker enrich`) and produces structured metadata stored back into note frontmatter. The query pipeline reads this metadata via the existing index types — no new index axis needed.

**Routing per `llm.md`:** every LLM-driven enrichment stage below (auto-tag, type classification, summary) runs as a *background* feature when triggered automatically (on save) and as a *fan-out* feature when triggered as a batch (e.g. `hiker enrich --all`). Both shapes call `core::llm` direct — single-shot prompts per note, no agent loop. Enrichment is on-demand, user-invoked, frontmatter-only — never auto-on-ingest, and it writes no `.hiker/` substrate. Entity extraction and reference extraction may use NER / pattern-matching rather than LLM; when they use LLM calls, the same routing applies.

Stages (each independent, opt-in per-vault):

- Auto-tag — constrained vocabulary, faceted (`topic:`, `person:`, `source:`, `project:`, `status:`, `type:`, ...). Vocabulary lives at `vault/.hiker/vocabulary.yaml` per facet, with canonical tags + descriptions + aliases. LLM is prompted with the vocabulary so suggestions converge rather than fragment.
    - Confidence tiers:
        - high — auto-apply
        - medium — queued for review
        - new — LLM proposes a new vocabulary entry; explicit accept required before added to vocabulary
    - Vocabulary maintenance: `hiker vocab merge a → b`, `hiker vocab rename`, `hiker vocab prune` (mass-rewrites tag references in vault).
    - Tag-description embeddings disambiguate ambiguous LLM calls (similarity check augments LLM judgment).
- Type classification — runs as a facet of auto-tag (`type:lecture`, `type:meeting`, `type:recipe`, `type:reference`, ...). Not a separate pipeline; reuses auto-tag machinery and vocabulary file.
- Entity extraction — NER over note content; populates the entity index. Coreference resolution (Lamport / Leslie Lamport / L. Lamport → one entity) handled with embedding-similarity merging, conservative on auto-merge to avoid false positives.
- Reference extraction — extracts URLs, file paths, hashes, mentioned artifacts (firmware blobs, ROM images, binaries, datasheet IDs) into a `references:` frontmatter list. Lightweight — pattern-matching primarily, no academic citation parsing. Each reference is resolvable: a hash matches a file in `.hiker/refs/`, a URL matches a scraped reference doc, a path matches an external-file note. References become queryable typed edges.
- Triage routing — runs the saved-tree classifier (see `cluster-editor.md`) on notes saved within the configured triage scope. Either moves the file or writes a frontmatter tag (per the saved tree's per-cluster mode), or leaves it alone — never writes a `hiker.placement` field, never auto-overrides anything outside the triage scope.
- Summary — short LLM-generated digest at note level (1–3 sentences) and optionally cluster level. Cached in frontmatter as `hiker.summary` (and on cluster nodes in the tree config). Refreshed on content change. Used as the cheap "what's in this" representation for MCP progressive disclosure, UI hover previews, and cluster overviews — distinct from chunk-level retrieval, which is the full text in pieces.

Each stage records its version in the note's frontmatter (`hiker.enrichment.<stage>: <version>`) so future re-runs can detect stale enrichment. Bumping a stage's version forces re-enrichment naturally.


## Lifecycle operations

Beyond create/edit/delete, notes have explicit lifecycle states tracked in frontmatter. The indexer respects these flags when serving search and MCP queries.

- archive — note is preserved but excluded from default search results. Reachable via explicit scope (`--include-archived`) and direct id lookup. For obsolete project notes, completed work, anything you don't want surfacing in casual queries.
- redact — content is intentionally absent from indexes (lexical, semantic, entity). The note file may stay on disk; the indexer simply does not embed or tokenize its body. For sensitive material that should not be reachable via vector or full-text similarity. Frontmatter is still indexed (so the note is findable by tag / id / path).
- retire — note is hidden from search and from default UI listings, but kept on disk and reachable by id. Stronger than archive (archive is "old," retire is "no longer to be surfaced"). Trails and links that reference the note still resolve.
- supersede — already covered by versioned sources for external references; for internal notes, frontmatter `hiker.superseded_by: <id>` redirects searches and links to the successor unless explicitly scoped to the original.

These are frontmatter fields, not separate stores — `hiker.archived: true`, `hiker.redacted: true`, `hiker.retired: true`. The indexer reads them on each ingest/refresh.

Delete remains the destructive option: file removed → all index entries removed → trail/link references become orphaned (handled like missing-source orphans elsewhere in the design).

Linking metadata (general): Trails are one instance of a broader idea — first-class metadata that links content items together with semantics. Other instances: tags (set membership), collections (named groupings), typed edges between notes (cites, contradicts, supersedes, depends-on, derived-from), annotations on those edges. Stored as structured metadata (frontmatter and/or a sidecar index file), exposed to search and to agents. Keeps the substrate plain markdown while letting structure accrete on top.


## Source import & viewers

Content that originates outside the vault — a web page, a PDF, a crawled site — is produced by external tools and imported, never fetched or scraped by hiker itself. **Hiker does zero in-process extraction.** An external producer (working name *trailblazer*) acquires content and emits content (a markdown shadow + optional artifacts) plus a tool-agnostic manifest; hiker imports the result, displays each item through a finite built-in viewer registry (markdown, and HTML/CSS via `hiker-htmlview`; PDF/image later), indexes the markdown shadow as the search layer, and lands artifacts under `.hiker/refs/<rel-path>/`. The manifest contract is hiker's one ingest seam. No runtime plugin loading; no built-in PDF/image/audio/office extractors, no multi-extractor fallback chain, no `CommandExtractor`, no ML/Python toolchain in the hiker tree. Full spec: `import.md`.


## LLM strategy

There are exactly two LLM surfaces (`llm.md`): **`core::llm`** for background and fan-out features (summaries, cluster naming, the summaries/embeddings that feed trees) — single-shot prompts, no agent loop; and **the MCP server** (`mcp.md`) as the **sole agent surface**, off by default. There is no in-app chat, no in-house agent loop, and no ACP client — external agents (Claude Code, Goose, …) connect to hiker's MCP server and use their own UI, with their writes landing as reviewable pending edits. The whole generative layer is disable-able (`[llm] enabled`). Embeddings stay local in `core::embed`, out of scope of the LLM strategy.

Full spec in [`llm.md`](llm.md). Anywhere `design.md` mentions an LLM-driven feature (auto-tag, summary, cluster naming, etc.), the implementation flows through the `core::llm` background/fan-out path described there.


## Architecture

Two layers (`app/`, `core/`), in-process, no IPC — roles per the crate layout above; `AppState` holds `Arc` handles to every long-lived subsystem.

Communication: direct function calls (panels take `&mut AppState` and call subsystem APIs), `tokio::sync::mpsc` channels for async events drained each frame, `Mutex`/`RwLock` for the few cross-thread subsystems (held briefly, never across `.await`). Channels follow one pattern across `fs_events`, `indexer_events`, and `mutation_events`: a tokio task posts, the frame loop drains with `try_recv`, state mutates before rendering.

Rules: all filesystem access goes through `core::vault::Vault` so the watcher stays authoritative and drift checks remain meaningful. Errors are typed enums (`HikerError`, `StoreError`, `StagingError`, …) matched per-variant by panels and routed to toasts or modals. No DTO layer — `core` types are consumed directly. Indexer is in-process; daemonization stays a future option.

### App shell

Single window, fixed layout:

- **Top strip** (`toolbar.rs`) — nav buttons, singleton-tab icons (Home / Queue / Index / Settings / Graph / Patch-review / Plugins) with live count badges on Queue + Patch-review, vault picker + label (right-click → set as default), sidebar / discovery toggles, and the **tab strip** inline. `▾` overflow button reveals all open tabs.
- **Sidebar** (`sidebar/`) — three modes via switcher: **Files** (tree, rename, dnd, index-state markers), **Clusters** (tree picker, multi-select stage-moves / stage-tags, undo/redo, graph view), **Trails** (active-trail picker, side-trail tree, orphan badges, remove / append-from-here). Trash pinned at the bottom. `…` actions menu has Refresh + Sort by.
- **Discovery panel** (`panels/discovery.rs`) — search box, results (grouped by note, `<mark>` highlights), related notes, backlinks. All toggles + per-mode options + Limit/Types/Order filters live in a right-click menu on the search icon. (No chat dock — there is no in-app chat after the rework.)
- **Central pane** — tab body dispatched by `tabs::body` from the active `TabKind`.

### Tab kinds

`TabKind` (in `app/src/tab.rs`) dispatches on the central pane; renderers live under `panels/`. Singletons (Home, Queue, Settings, Graph, PatchReview, Plugins, IndexerDetail) open-or-focus via `toolbar::open_singleton_tab`. (The Agent / Changes / Sync tabs were removed with the in-app chat, the activity feed, and multi-device sync respectively.)

- `Editor { buffer: BufferSource, diff: Option<DiffSource> }` — editor widget over a buffer (vault file, history version, proposal, or trash entry), optionally layered with a diff. Chrome (version dropdown, diff-vs-disk, view-options wrench, wand-menu) and status bar in `panels/buffer/`. When the active buffer's path has pending `edit_note` staging proposals, the panel renders the inline patch-review decorations + per-file pill on top — no separate tab kind, no mode flip. The diff/snapshot/staging/trash review surfaces are read-only `Editor` layerings over the same widget; staging review includes per-hunk review (line numbers, ±2 lines context, partial-apply via byte-range splice).
- `Home` / `HomeDetail { which }` — vault summary, per-path snapshot history (the version dropdown reads `core::snapshot`, with git when integrated).
- `Queue` / `QueueDetail { task_id }` — task queue with state filter pills, leased-row pulse, worker controls.
- `IndexerDetail` — model id, status, reindex, progress log with filter pills.
- `Settings` — scope-aware form (Refresh / Open / Reveal / Reset-to-defaults), raw-TOML fallback.
- `Properties { path }` — disk + indexer metadata + trails / clusters membership.
- `Graph` — vault-wide note-link force-directed graph (`petgraph` + painter).
- `ClusterReview { config_json }` — preview-then-persist build flow; `ClusterGraph { tree_id }` — radial dendrogram (color-by-policy, size-by-members, staleness tint).
- `PatchReview` — cross-vault list of pending staging proposals with bulk + per-row accept/reject. Sibling to the in-buffer inline UI on editor tabs.
- `Plugins` — manifest viewer for `<vault>/.hiker/plugins.json`. No host runtime — manifest edits only.
- `ZimView { zim_path, article }` — offline `.zim` archive viewer with browser-style link nav (`zim.md`).

Buffer tabs autosave per vault path; singleton page-kinds persist via a synthetic `:<kind>` key. `bootstrap::restore_tab_state` rehydrates both on vault open; payload-bearing previews (Trash/Snapshot/Staging) drop silently.

### Frame loop

`app/src/main.rs::App::update` each frame:

1. Enter tokio runtime guard.
2. If `pending_vault_switch` is set, re-bootstrap and return.
3. Run window-level keybinds before panels see input; clear `swipe_skip_rects`.
4. Drain mpsc channels: `fs_events` (watcher → cache invalidations + clean-buffer reloads), `indexer_events` (→ ring buffer), `mutation_events` (→ buffer body + toasts).
5. Tick autosave every ~5s.
6. `request_repaint_after(750ms)` to keep status / animations alive without input.
7. Render: titlebar → toolbar → tab strip → sidebar → discovery → central body → modal → toasts.

State only mutates inside the frame loop or via channel events folded by it.


## MCP surface

Agent retrieval is activation, not just retrieval — the MCP server fits results into a bounded context at an appropriate detail level. Read/search tools take a `budget` and a `detail` level (digest / snippet / full) for progressive disclosure; chunks/trails/landmarks carry stable ids for cross-call reference; lifecycle flags (`archived` / `redacted` / `retired`) are excluded from search by default and opt-in via scope. Full spec in `mcp.md`.


## Build order

- v0 — egui shell + in-tree editor widget + folder view. Open vault, list tree, click file → buffer opens in a tab, save on Ctrl/Cmd-S. Markdown syntax styling via `editor-md` + the live-preview decoration provider in `app/`. No watcher, no index, no search yet. Hold the core/UI separation discipline from day one.
- v1 — notify watcher + sqlite-vec index of chunks + "related notes" panel for the open file.
- v2 — search bar (hybrid lexical + semantic).
- v3 — MCP server adapter over the same core (the sole agent surface), exposing search/related/write to external agents; off by default, opt-in per vault.
- v3.5 — `core::llm` for opt-in background/fan-out features (auto-tag, summary, cluster naming, RAPTOR tree build). No in-app chat / agent loop — interactive use is an external agent over MCP. See `llm.md`.
- v4+ — external import/scrape tooling (the manifest producer) lands as load-bearing infrastructure for the multimodal-vault story; hiker imports its output (`import.md`). Trails come *after*, since their richest human-facing case is the narrative layer over imported sources. Then incremental: live-preview decorations, landmarks, graph view, AI-organize. Order within v4+ isn't strict.

Each step ships something useful. CLI subcommands grow alongside as thin adapters over core: hiker init, hiker stats, hiker import, hiker search, hiker related, hiker watch, hiker mcp, hiker diff.


## License

Apache-2.0. Permissive; explicit patent grant; fine with anyone using or forking it for any purpose.


## Future / deferred

Future, unimplemented, and fuzzier-than-spec concepts live in `ideas.md`.

## Sync / backup

- **No multi-device sync engine.** The always-on libp2p sync engine and the integrated git push/pull driver were removed. A vault is a folder of plain files, so any third-party file sync (Syncthing, iCloud/Dropbox folder, etc.) moves it as-is; **optional, user-driven git** (`git.md`, VSCode model) is the richer, shareable history when the user opts in. Neither is an engine hiker runs on its own.
- **Local history** is the plain-file snapshots under `.hiker/history/` (`op-log.md` "Local history") — whole-`.md` copies per save, capped by `[history]`, disposable cache. Git (when integrated) is the parallel, globally-ordered commit graph.
- Crash recovery: hiker autosaves dirty buffers Notepad++-style — every ~5s, each unsaved buffer's current text is written to a sidecar in `.hiker/autosave/`, overwritten in place per tick. A force-kill or power loss leaves at most ~5s of typing on the floor; on next vault open, a recovery modal lists each buffer whose autosaved content differs from disk and offers per-row Restore / Discard. Full spec in `autosave.md`. Distinct from saving (autosave writes a sidecar, not the user's file) and from snapshots (which record *committed* saves).
- Backup with history: OS-level tooling (Time Machine, Backblaze, Restic, btrfs/zfs snapshots, etc.). The vault directory holds three backup classes:
    1. **Canonical source** (notes, source files): must be backed up.
    2. **Durable derived data** (un-accepted `.hiker/pending/` edits, retained artifacts under `.hiker/refs/`, the agent-call log `.hiker/agent-log/`, `.hiker/trash/`): user-meaningful records not regenerable from source content. Must be backed up. Typically small.
    3. **Regenerable cache** (`.hiker/history/` snapshots — losing them loses only *local* version history, not canonical content; `.hiker/index.db` — the sole database, search/vector only, single-writer, rm-and-reindex; the fastembed model + embedding caches; `.hiker/autosave/`): rebuilt on demand. Doesn't need backup. (When git is integrated, the commit graph is the durable, shareable history that survives a `.hiker/history/` loss.)
   Simple backup tooling can include the whole `.hiker/` (slightly wasteful but correct); smarter tooling can exclude `index.db`, the model cache, and `.hiker/history/`.
- Mobile capture: against a third-party file sync of the vault directory, or a git-synced folder, until/unless a mobile client gets built.
