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

Build a Reor-like personal notes + knowledge system, in Rust.

- UI shell: Tauri (Rust backend, web frontend) — much smaller binaries than Electron, single mostly-static binary
- Editor: CodeMirror 6 in the webview, @codemirror/lang-markdown, live preview via decorations
    - Alternative if I want full WYSIWYG: Milkdown (ProseMirror-based, round-trips clean markdown)
- Frontend framework: deferred. Prototype v0 with vanilla TS; pick React / Svelte / Solid later only after feeling the pain in v0. Avoids guessing before contact with the actual editor surface.

Live preview approach (CM6):

- Walk the markdown syntax tree (from @lezer/markdown) via a ViewPlugin that emits decorations.
- Replace decorations hide syntax markers (e.g. `**`, `[`, `]`, `(url)`) and render styled output.
- Mark/Line decorations style the surrounding content (bold, headings, blockquotes).
- Cursor-on-line gating: only hide markers on lines that don't currently contain the cursor or selection. When you click into a heading/bold/link, its raw syntax reappears so you can edit it; when you click out, the syntax hides again. Same trick Obsidian's Live Preview uses.
- Widgets for non-text rendered elements: image previews, math (KaTeX), embedded note transclusions, callouts.

Wikilink support:

- Extend the markdown parser via @lezer/markdown's API to add a `WikiLink` node recognizing `[[id]]` / `[[id|display]]` syntax.
- Decorations render the wikilink as a styled pill with the resolved title.
- Click handler resolves the id (via core's path → ulid lookup) and opens the target note.
- Autocomplete source pulls from the indexer to suggest existing notes as you type `[[`.
- **Incoming links surfaced somewhere visible.** When viewing note A, the user should be able to see every note that wikilinks *to* A — backlinks aren't useful as a hidden index, only as a surfaced retrieval. Natural home is the discovery panel (`search.md`), as a section alongside Search results / Related notes / etc.; the panel was designed to absorb new retrieval surfaces of this shape. Backlink resolution rides the structural index axis from `design.md`'s index-types list (which already names "graph of links" as a tracked structural signal); when wikilinks become a real feature, the backlink section in the discovery panel becomes one of its consumers.

Other components:

- Filesystem watcher: notify crate
- Markdown parsing/chunking: pulldown-cmark or comrak
- Vector store: LanceDB (Rust-native) or sqlite + sqlite-vec
- Full-text search: tantivy (hybrid with vector for best results)
- Embeddings: local fastembed-rs by default (`core::embed::FastembedEmbedder`); cloud / Ollama options via `core::embed::LlmEmbedder` (wraps the `llm` crate's `EmbeddingProvider`). Both behind the same `Embedder` trait — see `index.md`'s embedder section.
- MCP server: rmcp (official Rust SDK)
- Ingestion sidecars: docling/marker for PDFs, tesseract for images, whisper.cpp for audio — each produces a sidecar .md alongside the original


## Crate layout (initial sketch)

```
core/       vault model, chunker, indexer, search, extractors — pure library, no frontend deps, no tauri imports
cli/        clap-based CLI, calls core
mcp-server/ rmcp adapter, calls core
ui/         Tauri + CM6 frontend, thin command wrappers calling core
```

Discipline: core has zero knowledge of Tauri, CLI, or MCP. Each frontend is an adapter over the same core API. Tauri commands are 5–15 lines (parse args, call core, return DTO). If logic creeps into a `#[tauri::command]` function, move it to core.


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
| External        | markdown    | no         | `external-pointer` | `.hiker/external/<id>--slug.md`                   | annotations only; original re-read on refresh |
| External        | non-md      | no         | `external-cached`  | `.hiker/external/<id>--slug.md`                   | extracted text (cached)                       |
| Either          | any         | yes        | `versioned`        | `.hiker/refs/<id>/manifest.md` + `vN/` subfolders | manifest annotations + per-version extracted  |

Notes:

- Vault-internal markdown needs no source-derived note — the file is the note. All other rows produce a hiker note.
- External-pointer is the only mode without a content cache; markdown is already plain text and cheap to re-read, so caching adds drift without benefit.
- Versioned mode is reached by opt-in (per-glob in vault config or per-source frontmatter); it supersedes sidecar / external-cached / external-pointer when active.

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

- **Linked (default)** — the sidecar is read-only in hiker's editor. Future re-extractions of the source overwrite the sidecar's body in place; for versioned sources, a re-extraction whose source hash differs from the prior version increments the version (`vN+1`) and writes a fresh extracted body there. The user's role with a linked sidecar is reading + annotating the *source* via trails / links / search, not editing the extracted text.
- **Unlinked** — explicit user action ("Unlink from source") flips the sidecar to RW. Hiker stops overwriting it on re-extraction (the sidecar is now diverged from source by user choice). The relationship to the source survives in frontmatter (`hiker.source`, `hiker.source_sha256` at the time of unlink), but re-extractions of the source no longer touch this sidecar's body. Rationale: gives the user an escape hatch for cases where the extractor mangles content and they want to fix it by hand without permanently disabling extraction for the source-type.

Re-link is supported (flips back to linked + re-extracts to overwrite local edits — confirm modal, since this discards the user's hand edits). Versioned-mode unlink is per-version: unlinking a single `vN` doesn't affect future versions, which still extract fresh.


## Versioned sources

Versioning is a global capability that any source can opt into (per-glob in vault config or per-source in frontmatter). Not type-tied — reference docs almost always opt in, contracts/legal sometimes, transient stuff usually doesn't. Default: off.

Storage layout for a versioned source: folder instead of single note file.

```
vault/.hiker/refs/
  01HRX3...--stm32-rm0090/
    manifest.md           hiker note for the logical document
    v1/
      source.pdf
      extracted.md
      meta.yaml           captured_at, source_url, sha256, extractor version
    v2/
      source.pdf
      extracted.md
      meta.yaml
    v3/...
```

manifest.md frontmatter:

```yaml
---
hiker:
  id: <ulid>
  kind: reference                      # optional metadata tag
  versioned: true
  current_version: v3
  versions:
    - id: v1, captured_at: ..., source_sha256: ..., source_url: ...
    - id: v2, ...
    - id: v3, ...
title: ...
tags: [...]
---
[user annotations, version-independent]
```

Each version's extracted.md has its own frontmatter pointing at the manifest id and naming the version (hiker.parent, hiker.version). Indexer treats each version's extracted.md as separately indexable.

Ingestion paths:

- File-drop — new file appears, hashed, compared to current version, creates vN+1 if different, no-op if identical
- Scrape — `hiker scrape <url>` uses monolith for single-file HTML capture + readability/mdream for markdown extraction; new version on content change. `hiker refresh` re-fetches all scraped sources, creates versions where content changed.

Search defaults:

- All versions indexed; only the latest surfaces in default search results (one hit per logical document).
- `--all-versions` or explicit version scoping reveals older versions.
- Trails can pin a specific version with `<id>@vN` syntax; bare `<id>` resolves to current.

Diff: `hiker diff <id> vA vB` runs a textual diff over extracted.md between two versions. Cheap and disproportionately useful for tracking datasheet revs or scraped doc changes.

Retention: per-source retention policy (keep last N, or keep forever). Datasheets typically forever; scraped docs maybe last 5.


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
- Auto-generated reorganization suggestions and inbox triage — see "Auto-organization suggestions" below, and `suggestions.md` for the full surface
- Pinned anchors / landmarks (other notes get a "nearest landmark" tag in embedding space)
- Trails: ordered, named sequences of notes with per-waypoint annotations. Spec lives in `trails.md`; the framing below captures *why* trails earn their keep, which is the part that wouldn't fit cleanly inside the spec itself.

  **The motivating use cases.** Two paradigms pin trails as worth building:

  1. **Agent-facing trails.** An agent benefits from *ordered* context the way humans benefit from *prose* — handing one a curated walk through 6 notes in order, with per-waypoint annotations explaining "why this chunk matters here," is the right granularity for the agent and very hard to produce in any other form. The MCP integration story (treat agent retrieval as activation) leans hard on this; trails are the structural unit activation wants to return. Two sub-uses live on this: (a) hand-authored or curated trails that the user explicitly hands to an agent as context; (b) opt-in agent activity logging, where every note an agent reads / writes / cites during an MCP session is appended to a draft trail. The agent is the *transcriber* in (b); the user keeps, edits, or discards the resulting trail. (b) doubles as cheap input for (a) — the agent's investigation becomes a reusable walk without the user authoring it from scratch. Trail synthesis from imported agent transcripts (Claude Code transcript / Web UI export ingestion, deferred below) is the same shape as (b) over a static input.
  2. **Narrative layer over multimodal sources.** This case earns its keep specifically because hiker treats every searchable thing as a note (see "Source-derived notes" above): website archives, PDFs, audio transcripts, scraped reference docs all become hiker notes alongside hand-written markdown. You can't add inline narrative to a PDF page or a scraped webpage the way you can to a markdown note — the source isn't yours to edit. A summary note linking to those sources puts the narrative *around* them; a trail puts the narrative *between* them, with per-waypoint annotation of *why this section of this PDF, then this paragraph of this archived article, then this audio timestamp*. For research / learning / investigation workflows that chain together immutable external material, that's a meaningfully different (and better) experience than a summary note. This case is the one that makes website archival load-bearing for hiker, and vice versa: trails get most of their human-facing utility from the existence of multimodal source-derived notes, and source-derived notes get a richer narrative-layer story from trails.

  **Where prose still wins.** A pure-prose summary note with inline links often beats a trail when the chain runs entirely through your own hand-written notes that you're already free to annotate inline. The connective tissue prose provides is more valuable than the per-waypoint structure trails impose. Trails earn their strongest keep when the chain crosses material you *can't* annotate inline (the multimodal case above) or when the *path itself* — order, side trips, the act of walking — is part of what you want to preserve and re-traverse. The two surfaces don't compete; they cover different shapes of "I want this thinking to stick."

  **Trails branch.** A waypoint can have child waypoints forming a side trail; the trail-doc's waypoint list is a tree. The reader walks the main line, drops down a side trail to follow a digression, and walks back up — the Bush memex shape. Cross-references between separate trails handle adjacent cases (linking from one trail's annotation to another trail by id) but don't replace branching: a single trail-doc with side trails is one shareable artifact, one MCP fetch, and reads as one continuous walk in a way that two linked trails do not.

  **Curated, not strictly user-authored.** The user owns every accepted trail, but the clustering pipeline and MCP agents may *propose* draft trails which land in a review queue scoped to trails (parallel to the `suggestions.md` reorganization-proposal shape). The user accepts, edits, or discards. This opens the door to commodity trails — agent-investigation transcripts, clustering-suggested reading orders, future imports of agent transcripts — without compromising "the user owns the trail" as the durable rule.

  See `trails.md` for the full surface — storage layout, reference shape, side-trail tree, sidebar mode, capture flow, build-as-you-read verbs, draft-trail review, MCP integration, indexer / watcher / trash hooks.


## Auto-organization suggestions

Hiker's clustering pipeline (`clustering.md`) is consumed as a *recommendation engine*, not as durable infrastructure that owns the user's organization. The user organizes their vault; the AI suggests improvements; neither pretends to own the layout. Two flows live on the same engine:

- **One-shot reorganization** — user runs `hiker suggest`, gets a markdown proposal of moves and tags, picks what to apply. Tree is ephemeral; nothing persists except the user's accepted actions and a small rejection log. Each suggestion can be applied as a folder move (filesystem rename) or as a frontmatter tag — the user picks per cluster, with per-note overrides.
- **Saved-tree triage** — user saves a generated tree as a classifier. New notes (default scope: `inbox/`) get auto-routed against it via greedy centroid descent. Confidence-tiered behavior: high → auto-apply with toast + 10s Undo, medium → queue for review, low → leave in inbox.

Triage will not move a note out of any folder *other* than the configured triage scope — the worst case for an over-eager classifier is "wrong subfolder under inbox," never "your important note got moved out from under you." That's the load-bearing safety rule.

There is no per-note `hiker.placement` provenance, no parallel curated-tree-vs-filesystem mental model, and no durable cluster identity carried across runs. The filesystem (and, in tag-mode, frontmatter tags) is the only source of truth. See `suggestions.md` for the full surface — proposal file format, apply mechanic, tag-field configurability, triage thresholds, and the deferred folder-pinning escape hatch.


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
- Triage routing — runs the saved-tree classifier (see `suggestions.md`) on notes saved within the configured triage scope. Either moves the file or writes a frontmatter tag (per the saved tree's per-cluster mode), or leaves it alone — never writes a `hiker.placement` field, never auto-overrides anything outside the triage scope.
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

Trait-based, all built-in. No runtime plugin loading (no dynamic libs, no WASM, no plugin manifest). Source types are a small finite set; the cost of a real plugin system isn't worth it for a personal tool.

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

Per-type modules under core::extract::*: pdf, image, audio, office, html, code, markdown, command.

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


## IPC and architecture

Three-layer stack:

- Frontend (TS, CM6) — UI state, editor, rendering. Never touches the filesystem. Never does heavy work.
- Tauri command layer (Rust, thin) — parses args, calls core, translates errors. 5–15 lines per command.
- Core crate (Rust) — vault model, indexer, search, extractors. No tauri imports.

Communication paths:

- `invoke()`/command — request/response for user-triggered operations (open file, save, search, list folder, related)
- emit/listen — push events from Rust to frontend (file changed, indexing progress, ingestion finished). Namespace event names: `hiker:file-changed`, `hiker:reindex-progress`
- channels — typed streaming for long-running ops (streaming search results, reindex progress, RAG chat tokens)
- State — long-lived handles (HikerCore, indexer, db connections) in `tauri::State<Arc<HikerCore>>`; initialized at startup, accessed by every command

Rules:

- Frontend never reads the filesystem directly — all I/O via core through commands. Keeps watcher authoritative, security model meaningful, and lets backend swap (remote / sync) without frontend changes.
- Errors as a typed enum (HikerError) with serde, not strings. Frontend dispatches on error.kind.
- DTOs for wire types — Tauri commands return Serialize-derived DTOs deliberately separate from internal types. Internal types may carry Arc, PathBuf, watcher handles, etc.; DTOs are flat, JSON-friendly, and frontend-shaped.
- Auto-generate TS types from DTOs via ts-rs or specta. No manual TS/Rust type duplication.
- Indexer runs in-process inside Tauri for v0/v1. Daemonizing as a separate OS process (so CLI/MCP/UI share one indexer) is a later option — core supports it without rework, but premature for now.


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

- v0 — Tauri shell + CM6 editor + folder view. Open vault, list tree, click file → CM6 opens, save on Ctrl/Cmd-S. Markdown syntax styling via @codemirror/lang-markdown. No watcher, no index, no search yet. Hold the core/UI separation discipline from day one.
- v1 — notify watcher + sqlite-vec or LanceDB index of chunks + "related notes" panel for the open file.
- v2 — search bar (hybrid lexical + semantic).
- v3 — MCP server adapter over the same core, exposing search and related to agents.
- v3.5 — `core::llm` + `core::agent` (basic agent loop) + chat panel UI. Unlocks all interactive LLM features (chat over vault, vision OCR review flows, cluster naming, bulk reorg conversations) plus opt-in background/fan-out features. `core::acp` (optional ACP client for external agents) is a follow-up. See `llm.md` for the full architecture.
- v4+ — extractors and scrape land as load-bearing infrastructure for the multimodal-vault story (PDF extractor first since it's the most-asked-for source type; web archival via `hiker scrape` close behind). Trails come *after* both, since their richest human-facing case is the narrative layer over the multimodal sources those features produce. Then incremental: live-preview decorations, more extractors, landmarks, graph view, AI-organize. Order within v4+ isn't strict — pick what unblocks the next thing you actually want to use.

Each step ships something useful. CLI subcommands grow alongside as thin adapters over core: hiker init, hiker stats, hiker ingest, hiker search, hiker related, hiker watch, hiker mcp, hiker scrape, hiker diff.


## License

Apache-2.0. Permissive; explicit patent grant; fine with anyone using or forking it for any purpose.


## Future / deferred

- Apple Notes export ingestion — parse the export (apple_cloud_notes_parser or similar) into one hiker note per item, provenance tagged.
- Claude Code transcript ingestion — scrape `~/.claude/projects/*/` transcripts, split per conversation into hiker notes, provenance tagged.
- Web UI export ingestion — parse Claude.ai data export JSON into per-conversation notes.
- RAG chat over the vault — subsumed by the ACP-client milestone (see `llm.md`). The embedded chat panel against any configured ACP agent IS this feature.
- **Habits-of-association ranking layer** (the sharper form of "cross-index intelligence"). Inspired by Vannevar Bush's memex framing: a user's *associative patterns* are personally meaningful and should be allowed to bias retrieval. Concretely: optional score bumps applied to search results and related-notes results, computed from user-authored association signals — wikilink edges between notes, shared trail membership, shared tags, folder co-location, and temporal co-edit / co-open. Plugs into the existing rank-fusion stage of the query pipeline, alongside lexical / semantic / structural scores; doesn't replace them, just adds a personalization signal. **Enable/disable lives in user or vault config** (a `[search.ranking]` settings section, riding the existing `settings-write-back` plumbing) — at minimum a master toggle, and ideally per-signal toggles so users can include wikilinks-and-trails but exclude folder-co-location, etc. Default off in v1 since the layer is experimental and you can't tell if it helps without evaluation data; depends on `qa.md`'s eval framework being real to land safely. Explicitly **excludes** proactive crawling / scraping new external sources to recommend — the system curates within existing material; the user decides what enters the vault. The weak version of cross-index intelligence is already covered by the rank-fusion step (lexical + semantic + structural compose); this entry is the sharper, personalized version.
- Per-index "smart" features (temporal anomalies, topic births/deaths, importance dynamics) — out of scope for now; revisit only if a specific need appears.
- **draw.io ingestion + MCP integration** — ingest `.drawio` (and the related `.drawio.svg` / `.drawio.png`) diagrams as a source type, with the diagram's textual graph (nodes, edges, labels) extracted into a hiker note for search/related-notes/RAG coverage. The diagram file itself stays alongside as the canonical artifact (same dual-file pattern as future PDF/EPUB extractors per `design.md`'s extractor section). Source-derived note carries the diagram's structural skeleton in markdown — node titles as headings, edges as a relationships block — so the embedder has something to chew on. The richer half is **MCP integration**: a `drawio_*` tool family on hiker's MCP server (`get_diagram(path)`, `add_node(path, title, ...)`, `add_edge(path, from, to, label?)`, `update_node(path, id, ...)`) lets an attached agent read and incrementally edit the diagram file in place. Implementation rides drawio's existing XML format (well-documented, embedded in `.drawio.svg` files too) — no headless browser needed for read or basic write; round-tripping layout is the harder problem and may need a constrained "structural-only edits" surface in v1, leaving manual layout to the user. [drawio-source-ingest, drawio-mcp-tools]
- **Browser extension** — companion extension that captures the current page into hiker via the same `hiker scrape` / extractor pipeline already specced for source-derived notes. Two primary buttons in the popup:
    - **Save to Hiker** — one click sends the current URL (and optionally a user-selected text range) to the running hiker instance for ingest. Lands as a source-derived note in `inbox/` per the existing extractor flow.
    - **Save to Hiker and append to active trail** — same capture, plus appends the resulting note as a waypoint on the user's currently-active trail (per line 260's "active trail" mode). With no active trail set, the second button is greyed (or hidden) so the choice between "land in inbox" and "land on the trail" is always explicit.
  Mechanism: the extension talks to the running hiker app over the existing local-only MCP server (per `mcp.md`) — discovery via `vault/.hiker/mcp.json`, scrape tool added to the MCP surface alongside the existing read/write tools. No new transport, no new auth model: the localhost-trust posture extends to the extension because both run on the user's machine. When hiker isn't running, the extension surfaces a clear "open hiker first" hint rather than queuing locally — queued-capture is its own deferred design that can land later if it earns it. Phone capture (OS share-sheet / Android intent) is a sibling future item with the same target shape; the extension is the desktop side of the same active-trail-capture story. [browser-extension-capture]

- **Split view** — shift-clicking a tab in the top tab strip splits the center pane and renders that tab alongside the current one. Tile orientation (horizontal / vertical) is user-selectable via a small toolbar control — placement undecided (editor toolbar's most-likely home; alternative is a per-split chrome corner). Splits compose with tab kinds (buffer + agent, buffer + graph, two buffers, etc., per `tab-kinds`). Each split holds its own active tab and its own scroll/selection state. Shift-clicking again on an existing split's tab cycles it within that split (or pops a new split, TBD). Closing the last tab of a split collapses the split. Persistence: open splits ride the autosave tab-state snapshot so workspace restore on next launch puts the user back in the same layout. Lands after `tab-kinds` since splits care about which kind sits in each pane (a graph view + a buffer side-by-side is the load-bearing case).

- **Graph view** — vault-wide graph of notes as the v1 default render of a graph-view tab. Nodes are notes; edges are wikilinks (when those exist), trail waypoint sequences, and optionally folder co-location / shared-tag / cluster co-membership as filterable overlays. Filtering / selection options on the graph chrome:
    - Folder scope — restrict to a vault subtree (e.g. only `research/`).
    - Edge-kind filters — hide / show wikilinks, trail edges, folder-cohabitation edges, etc., independently.
    - Trail-only mode — show only nodes that participate in at least one trail, with the trail edges drawn over them.
    - Per-source-type node filters (only md, only PDF-derived, etc.) once source-derived notes are real.
  Cross-highlight from the sidebar: hovering or selecting a folder / trail / cluster in the sidebar lights up matching nodes in the graph. Same hook works in reverse — clicking a node in the graph could reveal it in the tree or scroll the sidebar to it. The graph's selected node opens that note in an editor tab on the next click (or in a split, via shift-click) so the graph stays a navigation surface, not a content surface.

  **Renderer choice — sigma.js + graphology.** Sigma is WebGL-backed and stays smooth at 10k+ nodes, which matters once the vault holds source-derived notes at scale (web archives, PDFs, audio/transcripts) on top of hand-written markdown — graph node count grows with every ingested source. Graphology is the data-model half (sigma is rendering-only); the canonical pairing, both MIT and narrow-purpose. Layout via `graphology-layout-forceatlas2` (FA2 surfaces clusters without hand-tuning). Lazy-load via dynamic `import()` so the renderer bundle is paid only when a graph-view tab opens. Cytoscape.js and D3 considered and dropped: cytoscape is Canvas/SVG-bound (loses scale headroom we'll need); D3 is general-purpose viz, not graph-specific. Packages go through `ui/compose.yaml`'s Docker-isolated npm install per the supply-chain hygiene rule.

  **Renderer adapter pattern.** Spec the graph view behind a renderer-agnostic seam so the choice is reversible. Plain `{ nodes, edges }` DTOs from the backend; a TS `GraphRenderer` interface (mount, applyFilters, setHighlight, setSelection, onClick, onHover, exportPositions, capability flags) implemented by a single adapter file (`ui/src/graphView/renderers/sigma.ts`) — the only place sigma / graphology / layout-fa2 are imported. App state (selection / highlight / filters) lives in the panel module, not the renderer. Same module-discipline pattern as `Embedder`, `LlmClient`, `LexicalEngine`. Swapping renderers later is a one-file flip; capability flags let the UI hide options a given renderer doesn't support (e.g. compound-node cluster regions if cytoscape is ever wanted for that).

- **Special-character / control-character visualization.** Notepad++-style toggle that renders non-printable control characters as small inline glyphs in the editor — NULL bytes (0x00), backspace (0x08), ESC (0x1B), DEL (0x7F), C1 controls (0x80–0x9F), and BOM markers all get distinct lightweight glyphs so the user can see them inline rather than have them silently rendered as nothing or as replacement characters. Pairs with the existing `view-show-whitespace-toggle` (which already covers tabs / spaces / newlines via CM6's `highlightWhitespace`); this entry covers the *non*-whitespace control characters. Implementation rides a CM6 ViewPlugin that scans visible ranges and emits `Decoration.replace` widgets for each match — same shape as `live-preview-marker-fade-inline` decoration emission. Toggle in the View menu sibling to the whitespace one; default off; persisted per-vault. Useful for inspecting source-derived notes where extractors might leave embedded control bytes, for diagnosing encoding issues, and for any text-with-binary-debris content. Optional sub-toggle: distinguish line-ending styles (CRLF vs LF vs CR) with different glyphs so mixed-line-ending files are visible at a glance.

- **WASM plugin system** for user- or agent-authored extensions — sandboxed WASM runtime, capability-scoped host API, manifest-declared permissions presented at install, hash-pinned via vault-level `plugins.json`. Open-ended UI/automation surface; explicitly distinct from the "no runtime plugin loading" stance on extractors above (extractors are a finite core concern; plugins cover the unbounded user-extension surface). Full design in `plugins.md`.

- **Hex view mode** for raw byte-level inspection of any file. Lands as a new `kind: "hex"` tab (per `tab-kinds`); payload is the file path; the tab-body renders the standard hex-editor layout (offset column / hex bytes column / ASCII rendering column) with hover-pairing between the hex and ASCII halves. Read-only in v1 — just inspection. Open paths: right-click on a file in the filetree → "Open as hex"; View menu entry while a buffer tab is active → "View as hex" (opens the same file as a hex tab; doesn't replace the buffer). Useful for binary-adjacent files (small images, PDFs, audio sidecars), source-derived notes whose extracted text looks suspect, files with weird line endings or BOMs, and the "is this actually plain text or is something weird in here" diagnostic case. Renderer doesn't need a heavy library — a CM6 view with a custom decoration set + monospace font handles this in ~200 LOC; lazy-loaded so the cost is paid only when the user opens a hex tab. Write-side hex editing is deliberately deferred — the v1 use case is inspection, not editing.


## Sync / backup

- No VCS. Reference-doc versioning lives in-app (named, semantic, diffable) where it has meaning; personal notes are continuous edits and don't benefit from git overhead.
- Sync between machines: Syncthing — continuous, conflict-aware, no commit ceremony, handles phone.
- Crash recovery: hiker autosaves dirty buffers Notepad++-style — every ~5s, each unsaved buffer's current text is written to a sidecar in `.hiker/autosave/`, overwritten in place per tick. A force-kill or power loss leaves at most ~5s of typing on the floor; on next vault open, a recovery modal lists each buffer whose autosaved content differs from disk and offers per-row Restore / Discard. Tab state restores silently. Full spec in `autosave.md`. Distinct from saving (autosave writes a sidecar, not the user's file) and from `changes.md` (which records *committed* writes for agent rollback / sync, not in-flight content).
- Backup with history: OS-level tooling (Time Machine, Backblaze, Restic, btrfs/zfs snapshots, etc.). The vault directory contains three classes of data with different backup semantics:
    1. **Source content** (notes, source files): canonical, must be backed up.
    2. **Durable derived data** (`.hiker/trash/`, `.hiker/changes.db`, `.hiker/agent-log/`, `.hiker/autosave/index.json`, future `.hiker/conflicts/`): user-meaningful records that aren't regenerable from source content. Must be backed up. Typically much smaller than source content.
    3. **Regenerable index** (`.hiker/index.db`, fastembed model cache, `.hiker/autosave/<id>.md` sidecars): rebuilt from source / running memory on demand. Doesn't need backup.
   Simple backup tooling can include the whole `.hiker/` (slightly wasteful but correct); smarter tooling can exclude `index.db` and the model cache. The `.hiker/changes.db` log (per `changes.md`) is durable user data — losing it means losing agent-rollback history and (when sync lands) device-sync state.
- Mobile capture: Markor (Android) or Working Copy (iOS) against the synced folder until/unless a mobile client gets built.
- Git remains an option to add later if collaboration or web-publishing needs appear; not a one-way door.


### Ideas for integrated syncing (deferred)

A first-party sync system isn't on the roadmap — Syncthing covers the use case for v1–v3 — but the shape it would take if/when we built it is worth pinning so the architecture stays sync-ready. Note: the **append-only operation log** below isn't deferred — it lands with v3 as `core::changes` (see `changes.md`) because MCP-driven agent edits need it for rollback. The sync layer when it builds will ride on top of that log; only the cross-device transport, encryption, and conflict-resolution mechanisms below stay deferred. Properties:

- **Notes are the atomic unit of sync.** Whole-file granularity for both markdown notes and source-derived notes (sidecars, manifests, scraped reference docs). No character-level or chunk-level operation tracking.
- **Debounced full-file uploads.** Reuse the watcher's existing 200ms debounce window (`watcher-debounce-200ms`); when the window closes on a modified note, upload the full file. Watcher's self-write suppression (`watcher-suppress-self-writes`) keeps sync-initiated writes from echoing back into the log.
- **Append-only operation log with monotonic version numbers.** Lives in `.hiker/changes.db` (separate from regenerable `index.db`) per `changes.md` — *lands with v3, not deferred*. Op shape mirrors `FileEvent` from the watcher: `Created`, `Modified`, `Deleted`, `Renamed`, plus an `author` field distinguishing user / agent / sync / import. One row per debounced event. Sync layer adds device-id and watermark columns when it lands; the table itself doesn't reshape.
- **Clients pull operations newer than their last-seen version.** Standard log-suffix sync. Each client keeps its watermark; the server hands back ops at and after it.
- **Last-write-wins, no automatic merging.** Same posture as `pre-write-drift-check` — surface the conflict, don't silently merge. LWW for sync is that posture across devices instead of within one.
- **Losing version preserved as a conflict copy.** Routed through a dedicated `.hiker/conflicts/` directory (separate from `vault-trash` so users can distinguish "I deleted this" from "sync displaced this"). File naming follows the trash precedent — collision suffix `_N`, manifest entry tracking origin and timestamp. Indexable like any other note so you can search for what got displaced.
- **Per-file version history surfaced for manual recovery.** Driven directly off the operation log; "show history of `path/to/note.md`" filters log entries on path. *Distinct from* the `versioned sources` feature, which is opt-in user-curated semantic versioning of reference docs — the sync log is automatic per-modification history. Both can coexist; they're different axes.
- **Client-side encryption — server stores ciphertext only.** Per-vault keypair (or passphrase-derived key). The indexer never sees ciphertext: each device decrypts on receive, writes plaintext to the local vault, then runs the existing indexer over local-plaintext as usual. Index never syncs. The "filesystem is truth, index is regenerable" rule is what makes this work — the server only needs to round-trip files, not anything queryable.
- **Sync configuration is per-vault, not per-user-globally.** The `[sync]` config section lives in vault-scope TOML (`vault/.hiker/config.toml`), not user-scope. Each vault opts in independently and carries its own server URL, keypair, device list. The user-scope config can hold defaults a user might want propagated when they create a new vault, but nothing user-scope auto-enables sync on a vault that hasn't opted in.

Compatibility checks against current architecture: this model preserves "filesystem is truth" (sync transports filesystem state; doesn't introduce a new source of truth), preserves "index is regenerable from content" (each device runs its own indexer), and reuses every existing watcher / trash / drift-check mechanism. The integration points are clean.

Costs that pin this as deferred rather than near-term: a server (operational commitment, hosted vs. self-hosted question), a mobile story that diverges from "Markor / Working Copy against the synced folder" (custom protocol means custom mobile clients), key-management UX (lose-key recovery, rotation), and bandwidth concerns for vaults heavy with large source-derived notes (extracted PDFs, audio sidecars).
