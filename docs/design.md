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

Live preview: per-frame decoration providers in `app/src/panels/buffer.rs` produce `DecorationSet` layers fingerprint-cached on `(doc_id, selection, folds, viewport, theme)`. Decoration kinds: `Mark`, `Line`, `Replace { display }`, `Block`, `Widget`. The markdown provider walks the buffer with `pulldown-cmark`; `Replace` fades syntax markers, `Mark`/`Line` styles content, and a decoration is suppressed when its line overlaps a selection so clicking in reveals raw markers. Widgets handle images, math, wikilink pills, callouts.

Wikilinks: the markdown decoration provider emits a widget for `[[Name]]` / `[[folder/Name]]`; click resolves the path via `core::store`. `[[` opens an autocomplete popup driven by the same indexer path cache the chat `@`-mention picker uses (`editor-view`'s `CompletionSource` trait; `app::completion_sources::WikilinkSource`). Backlinks surface in the discovery panel alongside search results / related notes (`search.md`). Full spec: `wikilinks.md` (path form, autocomplete, render-from-live-title, rename-rewrite, backlinks).

Other components:

- Filesystem watcher: notify crate
- Markdown parsing/chunking: pulldown-cmark or comrak
- Vector store: sqlite + sqlite-vec
- Full-text search: tantivy (hybrid with vector for best results)
- Embeddings: local fastembed-rs by default (`core::embed::FastembedEmbedder`); cloud / Ollama options via `core::embed::LlmEmbedder` (wraps the `llm` crate's `EmbeddingProvider`). Both behind the same `Embedder` trait — see `index.md`'s embedder section.
- MCP server: rmcp (official Rust SDK)
- Ingestion sidecars: docling/marker for PDFs, tesseract for images, whisper.cpp for audio — each produces a sidecar .md alongside the original


## Crate layout

```
core/             vault model, chunker, indexer, search, extractors, agent, llm, mcp handler, staging, sessions, trees, autosave, changes — pure library, no UI deps
cli/              clap-based CLI, calls core
mcp-server/       rmcp adapter, calls core (and reuses core::mcp::HikerHandler for in-process MCP)
app/              egui desktop app. Owns tabs, panels, sidebar, toolbar, chat, settings, modals. Holds long-lived subsystems (vault, indexer, watcher, autosave, changes, staging, trees, chat, mcp) on AppState and pumps mpsc channels each frame.
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
  id: <ulid>
  source: <absolute path>
  source_sha256: <hash>
  source_mtime: <iso8601>
  type: pdf | image | audio | markdown | docx | html | code | ...
  storage: sidecar | hiker-owned | external-pointer
tags: [...]
---
```

A path → ulid lookup table maintained by the indexer makes IDs stable across renames. Filenames in hiker-owned areas embed the ulid + a debuggable slug, e.g. `01HRX3...--design.md`.

Storage modes (each row maps unambiguously to one combination of source-location × type × versioned):

| Source location | Source type | Versioned? | Mode               | Where the note lives                              | Body                                          |
| --------------- | ----------- | ---------- | ------------------ | ------------------------------------------------- | --------------------------------------------- |
| Vault-internal  | markdown    | no         | (none)             | the file itself is the note                       | the file's own contents                       |
| Vault-internal  | non-md      | no         | `sidecar`          | next to source as `<full-source-filename>.md`     | extracted text (cached)                       |
| URL capture     | web/feed    | no         | `capture`          | a visible capture note in the vault (`clips/`, a crawl/feed companion folder) | extracted text; see `extract.md` `capture-spec-note` |
| External (file) | markdown    | no         | `external-pointer` | `.hiker/external/<id>--slug.md`                   | annotations only; original re-read on refresh |
| External (file) | non-md      | no         | `external-cached`  | `.hiker/external/<id>--slug.md`                   | extracted text (cached)                       |
| Either          | any         | yes        | `versioned`        | sidecar note (op-log history) + `.hiker/refs/<id>/` retained artifacts | extracted text, versioned via the op-log; old artifacts kept per `extract-artifact-retention` |

Notes:

- Vault-internal markdown needs no source-derived note — the file is the note. All other rows produce a hiker note.
- A captured **URL** (a one-off scrape, a crawled page, a feed entry) is a visible capture note, not a hidden cache file — `capture` mode in `extract.md`. The `external-cached` / `external-pointer` rows cover **external files on disk outside the vault** that hiker watches read-only, a distinct case from URL capture.
- External-pointer is the only file mode without a content cache; markdown is already plain text and cheap to re-read, so caching adds drift without benefit.
- Versioned mode is reached by opt-in (per-glob in vault config or per-source frontmatter); it supersedes sidecar / external-cached / external-pointer when active.

**Subsystem notes are first-class visible files.** Any document a subsystem produces that carries a *user-authored body* — trail waypoints, chat sessions, captured pages — lives at a real vault path (a folder, a companion folder) and is an ordinary indexed note, surfaced cleanly in Vault mode by source-type/provenance grouping. `.hiker/` is reserved for non-note machinery only: regenerable caches (`index.db`, `doc-index.db`, `changes.db`), in-flight scratch (`autosave/`), deleted content (`trash/`), retained binary artifacts (`refs/`), external-file caches (`external/`), and config. This is why the watcher needs no per-subsystem carve-out of its `.hiker/` ignore — nothing indexable lives under `.hiker/`. [subsystem-notes-visible]

Per-type extractors:

- PDF — pdftotext (poppler) for clean text, marker / docling for scanned/complex layouts
- Image — tesseract for printed text; vision LLM (Claude / GPT-4o) for handwriting, results cached by image hash
- Audio — whisper.cpp (CPU-only ok)
- Office docs — pandoc (.docx/.odt), markitdown for xlsx/pptx
- HTML / web archives — monolith + readability/mdream/html-to-md
- Code files — index original directly; optional LLM-summary sidecar for semantic discoverability

External file interaction:

- Read-only from Hiker. Hiker's editor never opens external files.
- Search results show the source path with a snippet; clicking opens the file in the OS handler (xdg-open / equivalent), not in Hiker.
- Watcher follows external paths; on change → re-read, re-chunk, re-embed.
- Trails and links reference the hiker note's stable id, not the path.
- On missing source → mark `orphaned: true` in frontmatter, stop refreshing, keep the index entries so prior search/links/trails don't break. User decides whether to delete or fix the path.

UI affordance: hide hiker-owned sidecar files (`*.<ext>.md` next to non-md sources, plus `.hiker/` contents) from the file tree by default; surface them via "view extracted text" actions on the original and in search results.

**Linked vs. unlinked sidecars.** Every extracted-text sidecar (sidecar / external-cached / versioned modes) carries a `hiker.link_state: linked | unlinked` field, default `linked`. Semantics:

- **Linked (default)** — the sidecar is read-only in hiker's editor. A re-extraction overwrites the sidecar's body in place via an `extractor` op on the document's `accepted` state, so the prior body stays in op-log history rather than in a separate version file. The user's role with a linked sidecar is reading + annotating the *source* via trails / links / search, not editing the extracted text.
- **Unlinked** — explicit user action ("Unlink from source") flips the sidecar to RW. Hiker stops overwriting it on re-extraction (the sidecar is now diverged from source by user choice). The relationship to the source survives in frontmatter (`hiker.source`, `hiker.source_sha256` at the time of unlink), but re-extractions of the source no longer touch this sidecar's body. Rationale: gives the user an escape hatch for cases where the extractor mangles content and they want to fix it by hand without permanently disabling extraction for the source-type.

Re-link is supported (flips back to linked + re-extracts to overwrite local edits — confirm modal, since this discards the user's hand edits). Link-state is a property of the sidecar document, not of any one capture — re-extractions land as `extractor` ops on the linked sidecar and prior bodies stay in op-log history.


## Versioned sources

The version history of a source-derived note is the op-log (`op-log.md`), not a parallel per-version store. A sidecar is a Yrs document; a re-extraction (changed source, bumped extractor version, re-fetch, re-crawl) lands as an `extractor`-authored op on its `accepted` state. So a source's "versions" are its op-log history, and diff / per-hunk restore / the version dropdown reuse the existing op-log surfaces. An identical re-extraction is a no-op, so versions accrue only on real change. The concrete slice — re-extraction policies, the crawl-job manifest note, retention — lives in `extract.md` and `op-log.md`.

- **Logical documents spanning many sources** (a crawl, a multi-file capture) are represented by a manifest note; members carry `hiker.parent: <manifest-ulid>`. A single scraped or dropped source needs no manifest — the sidecar note is itself the versioned unit.
- **Binary artifacts** (the source bytes, the per-capture HTML archive) are what the op-log can't hold — it versions text, not blobs. Whether old artifacts are retained is a per-source retention cascade (`extract-artifact-retention`): vault default → per-crawl/glob → per-source frontmatter; values `latest` / `keep:N` / `forever`. Retained artifacts live under `.hiker/refs/<doc_id>/` keyed by the producing op, and are device-local (the op-log syncs sidecar text, not blobs).
- **Search** indexes the current accepted state (what's on disk); historical versions live in the op-log and surface on demand rather than as separate default-search hits. Trails pin a point in a note's history via its op-log snapshot id.


## Index model

Two orthogonal axes: index type, and index level (granularity within a type).

Types (parallel indexes over the same content):

- Lexical — tantivy/FTS over raw tokens. Exact matches, names, code, command snippets.
- Semantic — vector embeddings of chunks/notes. "Find related notes about X."
- Structural — graph of links, headings, tags, folder paths. "What references this," "what's under this heading."
- Temporal — by mtime/ctime or explicit dates in frontmatter. "What was I working on last Thursday."
- Entity — extracted named entities (people, projects, places) with their own embeddings/aliases. "Everything about Alice" regardless of phrasing.
- Provenance — source of ingestion (apple-notes-export, claude-code-transcript, OCR, audio, external-file, user-authored, agent-authored). Two filter axes ride this: the specific provenance label (fine-grained, "show me everything from this Apple Notes export"), and a coarser **authorship trichotomy** — `user-authored / agent-authored / imported` — for the everyday surfaces ("show me only my own writing," "show me only what got pulled in from outside"). Stored as `hiker.provenance:` (specific) and `hiker.author:` (coarse) in frontmatter. Default for hand-typed notes is `user-authored`; the import paths (scrape, drag-and-drop, transcript ingestion) stamp `imported`; agent writes via MCP stamp `agent-authored`. Surfaced in the file tree via per-source-type icons (see trails seedling for icon shape) and filterable in the discovery panel (`search.md` deferred slugs `search-authorship-filter` + `search-source-type-filter`).

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
- **Saved-tree triage** — save a tree as a classifier. New notes (default scope: `inbox/`) get routed against it via centroid descent (`cluster-place-beam-descent`) and the matched cluster's policy fires. Matches stay **pending for review by default** (`[triage].review_required = true`); auto-apply is the per-tree opt-in once a classifier is trusted.

Triage will not move a note out of any folder *other* than the configured scope — the worst case for an over-eager classifier is "wrong subfolder under `inbox/`," never "your important note got moved out from under you." That's the load-bearing safety rule.

See `cluster-editor.md` for the full surface — the policy model, apply + batch review, tag-field configurability, and triage scope/scheduling.


## Enrichment pipeline

A stage that runs over notes (on ingest, on save, on demand via `hiker enrich`) and produces structured metadata stored back into note frontmatter. The query pipeline reads this metadata via the existing index types — no new index axis needed.

**Routing per `llm.md`:** every LLM-driven enrichment stage below (auto-tag, type classification, summary, vision OCR for image extractors) runs as a *background* feature when triggered automatically (on ingest / save) and as a *fan-out* feature when triggered as a batch (e.g. `hiker enrich --all`). Both shapes call `core::llm` direct — single-shot prompts per note, no agent loop, no ACP. Entity extraction and reference extraction may use NER / pattern-matching rather than LLM (per their descriptions below); when they do use LLM calls, same routing applies.

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


## Extractors

Trait-based, all built-in, living in a decoupled `hiker-extract` leaf crate that `core` does not depend on (the sidecar `.md` on disk is the seam — see `extract.md`). No runtime plugin loading (no dynamic libs, no WASM, no plugin manifest) for this registry: the binary formats (PDF, image, audio, office) need native libraries and form a small finite set, so a real plugin system isn't worth it here. The concrete first slice — the registry, sidecar write path, PDF, website-to-markdown, and the governed crawl loop — is specified in `extract.md`. The unbounded text-transform tail (per-site scrapers, niche text formats) is a separate, plugin-eligible source-fetcher surface — `plugins.md`'s source plugins — not part of this built-in set.

Shape:

```rust
pub trait Extractor: Send + Sync {
    fn name(&self) -> &str;
    fn matches(&self, source: &SourcePath) -> bool;     // file ext, mime, URL pattern
    fn extract(&self, source: &SourcePath, ctx: &Ctx) -> Result<Option<Extracted>>;
    fn version(&self) -> &str;                          // participates in cache key
}
```

A Registry holds Box<dyn Extractor> instances and routes a source to the first matching one. Adding a new type = one new module + one registration line.

Per-type modules under hiker_extract::*: pdf, image, audio, office, html, code, markdown, command.

Multi-extractor fallback: matches() can return true for several extractors; extract() returns Result<Option<Extracted>> so an extractor can say "I don't actually handle this, try the next." E.g. PDF: pdftotext fast path → marker fallback for scanned/garbage output.

Per-source override: hiker.extractor: <name> in frontmatter forces a specific extractor.

Cache invalidation: extractor.version() is part of the cache key for extracted content. Bumping a version (e.g. upgrading marker) re-extracts everything from that extractor naturally.

Generic escape hatch — CommandExtractor: configured per-glob in vault config:

```toml
[[extractor.command]]
match = "**/*.epub"
command = ["epub2txt", "{input}", "-o", "{output}"]
output_format = "text"
```

User can support a new format without writing code, just by pointing at an existing CLI tool. Covers the long tail; reserve real Rust extractors for cases that need richer logic.


## LLM strategy

Generative LLM access lives in `core::llm` (built on the [`llm`](https://crates.io/crates/llm) crate, multi-provider). Background and fan-out features call it directly. Interactive features go through `core::agent` (a basic in-hiker agent loop using `core::llm`), or optionally via `core::acp` to an external ACP agent (Claude Code, Goose, etc.) as an escape hatch. The whole layer is disable-able. ACP is interactive-only — never wired for background or fan-out — which keeps subscription-billed agents in the role they're priced for. Embeddings stay local in `core::embed` and are out of scope of the LLM strategy.

Full spec in [`llm.md`](llm.md). Anywhere `design.md` mentions an LLM-driven feature (vision OCR, auto-tag, summary, cluster naming, RAG chat, etc.), the implementation flows through the path described there.


## Architecture

Two layers, in-process, no IPC:

- **`app/`** — owns the UI tree and `AppState`, which holds `Arc` handles to every long-lived subsystem (vault, indexer, watcher, autosave, changes, staging, trees, chat, mcp).
- **`core/`** — vault, indexer, search, extractors, agent, LLM, in-process MCP handler. No UI deps.

Communication: direct function calls (panels take `&mut AppState` and call subsystem APIs), `tokio::sync::mpsc` channels for async events drained each frame, `Mutex`/`RwLock` for the few cross-thread subsystems (held briefly, never across `.await`). Channels follow one pattern across `fs_events`, `indexer_events`, `mutation_events`, and `ChatRegistry::rx`: a tokio task posts, the frame loop drains with `try_recv`, state mutates before rendering.

Rules: all filesystem access goes through `core::vault::Vault` so the watcher stays authoritative and drift checks remain meaningful. Errors are typed enums (`HikerError`, `StoreError`, `StagingError`, …) matched per-variant by panels and routed to toasts or modals. No DTO layer — `core` types are consumed directly. Indexer is in-process; daemonization stays a future option.

### App shell

Single window, fixed layout:

- **Top strip** (`toolbar.rs`) — nav buttons, singleton-tab icons (Home / Queue / Index / Settings / Graph / Patch-review / Agent-changes / Plugins) with live count badges on Queue + Patch-review, new-chat quick button, vault picker + label (right-click → set as default), sidebar / discovery toggles, and the **tab strip** inline. `▾` overflow button reveals all open tabs.
- **Sidebar** (`sidebar/`) — three modes via switcher: **Files** (tree, rename, dnd, index-state markers), **Clusters** (tree picker, multi-select stage-moves / stage-tags, undo/redo, graph view), **Trails** (active-trail picker, side-trail tree, orphan badges, remove / append-from-here). Trash pinned at the bottom. `…` actions menu has Refresh + Sort by.
- **Discovery panel** (`panels/discovery.rs`) — search box, results (grouped by note, `<mark>` highlights), related notes, backlinks. All toggles + per-mode options + Limit/Types/Order filters live in a right-click menu on the 🔍 icon. Collapsible chat dock at the bottom.
- **Central pane** — tab body dispatched by `tabs::body` from the active `TabKind`.

### Tab kinds

`TabKind` (in `app/src/tab.rs`) dispatches on the central pane; renderers live under `panels/`. Singletons (Home, Queue, Settings, Graph, PatchReview, Plugins, IndexerDetail, Changes) open-or-focus via `toolbar::open_singleton_tab`.

- `Buffer { path }` — editor widget. Chrome (version dropdown, diff-vs-disk, view-options wrench, wand-menu) and status bar in `panels/buffer/`. When the active buffer's path has pending `edit_note` staging proposals, the panel renders the inline patch-review decorations + per-file pill on top — no separate tab kind, no mode flip.
- `BufferDiff { path }`, `SnapshotPreview`, `StagingPreview`, `TrashPreview` — read-only review surfaces over the same widget. StagingPreview includes per-hunk review (line numbers, ±2 lines context, partial-apply via byte-range splice).
- `Home` / `HomeDetail { which }` — vault summary, snapshots, per-path history (`HomeDetail::ActivityRow`). The dashboard-wide activity widget moved into the unified `Changes` tab.
- `Queue` / `QueueDetail { task_id }` — task queue with state filter pills, leased-row pulse, worker controls.
- `IndexerDetail` — model id, status, reindex, progress log with filter pills.
- `Settings` — scope-aware form (Refresh / Open / Reveal / Reset-to-defaults), raw-TOML fallback.
- `Properties { path }` — disk + indexer metadata + trails / clusters membership.
- `Graph` — vault-wide note-link force-directed graph (`petgraph` + painter).
- `ClusterReview { config_json }` — preview-then-persist build flow.
- `ClusterGraph { tree_id }` — radial dendrogram (color-by-policy, size-by-members, staleness tint).
- `PatchReview` — cross-vault list of pending staging proposals with bulk + per-row accept/reject. Sibling to the in-buffer inline UI on `Buffer` tabs.
- `Changes` — unified activity / changes feed (replaces the prior `AgentChanges` tab). One filterable view over `core::activity::list` (a projection over the op log carrying both pending and accepted ops) with author / source / op filter chips. The legacy `:agent_changes` persist key maps forward to this tab.
- `Agent { session_id }` — full-tab chat.
- `Plugins` — manifest viewer for `<vault>/.hiker/plugins.json`. No host runtime — manifest edits only.
- `ZimView { zim_path, article }` — offline `.zim` archive viewer with browser-style link nav (`zim.md`).

Buffer tabs autosave per vault path; singleton page-kinds persist via a synthetic `:<kind>` key. `bootstrap::restore_tab_state` rehydrates both on vault open; payload-bearing previews (Trash/Snapshot/Staging) drop silently.

### Frame loop

`app/src/main.rs::App::update` each frame:

1. Enter tokio runtime guard.
2. If `pending_vault_switch` is set, re-bootstrap and return.
3. Run window-level keybinds before panels see input; clear `swipe_skip_rects`.
4. Drain mpsc channels: `fs_events` (watcher → cache invalidations + clean-buffer reloads), `indexer_events` (→ ring buffer), `mutation_events` (→ buffer body + toasts), `chat::state::pump_events` (→ active session).
5. Tick autosave every ~5s.
6. `request_repaint_after(750ms)` to keep status / animations alive without input.
7. Render: titlebar → toolbar → tab strip → sidebar → discovery → central body → modal → toasts.

State only mutates inside the frame loop or via channel events folded by it.


## MCP surface

Treat agent retrieval as activation, not just retrieval — the MCP server is responsible for fitting results into a bounded context with appropriate detail level, not just returning everything that matched.

Tools exposed:

- `search_notes(query, scope?, budget?, detail?)` — hybrid search with explicit budget and detail control
- `get_note(id, detail?)` — fetch a single note at a given detail level
- `related_notes(id, k?, budget?)` — neighbors in embedding space for a known note
- `list_trails(scope?)`, `get_trail(id)` — walk a curated trail
- `list_landmarks(scope?)` — landmarks for orientation
- `list_collections(scope?)`, `get_collection(id)` — saved queries / groupings

Budget-aware returns:

- Every search/related call accepts a `budget` (approx token count or chunk count). Server returns the highest-priority hits that fit; remainder is truncated and reported as `truncated_count` in the response.
- Default budget chosen so a single call fits comfortably in a typical agent's context allocation.

Progressive disclosure (detail levels):

- digest — id + title + 1–3 sentence summary (from the Summary enrichment stage) + score. Cheapest. Default for multi-hit search responses.
- snippet — digest + the matching chunk(s) with surrounding context. Default for single-hit / direct lookups.
- full — entire note body. Returned only on explicit request.
- Each result carries its own `id`, so the agent can call `get_note(id, detail="full")` to escalate just the ones it cares about. Avoids dumping full bodies on first hit.

Stable references:

- Every returned chunk has a stable `chunk_id` (note ulid + chunk index or hash). Agents can reference prior results in subsequent calls (`expand_chunk(chunk_id)`, `get_note_context(chunk_id, before, after)`) without re-querying.
- Trail and landmark ids are likewise stable; agents can pin specific waypoints across a session.

Lifecycle awareness:

- By default, search excludes notes with `archived`, `redacted`, or `retired` set. Agents can opt-in via scope flags to include them when intentionally auditing or recovering history.
- Redacted notes are returned as id + title only; their bodies and chunks are unreachable via MCP.

Streaming: Long-running operations (large reindex, scrape refresh) expose progress via MCP notifications rather than blocking call-response.


## Build order

- v0 — egui shell + in-tree editor widget + folder view. Open vault, list tree, click file → buffer opens in a tab, save on Ctrl/Cmd-S. Markdown syntax styling via `editor-md` + the live-preview decoration provider in `app/`. No watcher, no index, no search yet. Hold the core/UI separation discipline from day one.
- v1 — notify watcher + sqlite-vec index of chunks + "related notes" panel for the open file.
- v2 — search bar (hybrid lexical + semantic).
- v3 — MCP server adapter over the same core, exposing search and related to agents.
- v3.5 — `core::llm` + `core::agent` (basic agent loop) + chat panel UI. Unlocks all interactive LLM features (chat over vault, vision OCR review flows, cluster naming, bulk reorg conversations) plus opt-in background/fan-out features. `core::acp` (optional ACP client for external agents) is a follow-up. See `llm.md` for the full architecture.
- v4+ — extractors and scrape land as load-bearing infrastructure for the multimodal-vault story (PDF extractor first since it's the most-asked-for source type; web archival via `hiker scrape` close behind). Trails come *after* both, since their richest human-facing case is the narrative layer over the multimodal sources those features produce. Then incremental: live-preview decorations, more extractors, landmarks, graph view, AI-organize. Order within v4+ isn't strict — pick what unblocks the next thing you actually want to use.

Each step ships something useful. CLI subcommands grow alongside as thin adapters over core: hiker init, hiker stats, hiker ingest, hiker search, hiker related, hiker watch, hiker mcp, hiker scrape, hiker diff.


## License

Apache-2.0. Permissive; explicit patent grant; fine with anyone using or forking it for any purpose.


## Future / deferred

Future, unimplemented, and fuzzier-than-spec concepts live in `ideas.md`.

## Sync / backup

- No VCS. Reference-doc versioning lives in-app (named, semantic, diffable) where it has meaning; personal notes are continuous edits and don't benefit from git overhead.
- Sync between machines: Syncthing — continuous, conflict-aware, no commit ceremony, handles phone.
- Crash recovery: hiker autosaves dirty buffers Notepad++-style — every ~5s, each unsaved buffer's current text is written to a sidecar in `.hiker/autosave/`, overwritten in place per tick. A force-kill or power loss leaves at most ~5s of typing on the floor; on next vault open, a recovery modal lists each buffer whose autosaved content differs from disk and offers per-row Restore / Discard. Tab state restores silently. Full spec in `autosave.md`. Distinct from saving (autosave writes a sidecar, not the user's file) and from the op log (`op-log.md`, which records *committed* writes for agent rollback / sync, not in-flight content).
- Backup with history: OS-level tooling (Time Machine, Backblaze, Restic, btrfs/zfs snapshots, etc.). The vault directory contains three classes of data with different backup semantics:
    1. **Source content** (notes, source files): canonical, must be backed up.
    2. **Durable derived data** (`.hiker/trash/`, `.hiker/oplog/`, `.hiker/agent-log/`, `.hiker/autosave/index.json`, future `.hiker/conflicts/`): user-meaningful records that aren't regenerable from source content. Must be backed up. Typically much smaller than source content.
    3. **Regenerable index** (`.hiker/index.db`, fastembed model cache, `.hiker/autosave/<id>.md` sidecars): rebuilt from source / running memory on demand. Doesn't need backup.
   Simple backup tooling can include the whole `.hiker/` (slightly wasteful but correct); smarter tooling can exclude `index.db` and the model cache. The `.hiker/oplog/` directory (per `op-log.md`) is durable user data — losing it means losing edit history, agent-rollback substrate, and (when sync lands) device-sync state.
- Mobile capture: Markor (Android) or Working Copy (iOS) against the synced folder until/unless a mobile client gets built.
- Git remains an option to add later if collaboration or web-publishing needs appear; not a one-way door.
