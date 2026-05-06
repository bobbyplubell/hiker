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

Other components:

- Filesystem watcher: notify crate
- Markdown parsing/chunking: pulldown-cmark or comrak
- Vector store: LanceDB (Rust-native) or sqlite + sqlite-vec
- Full-text search: tantivy (hybrid with vector for best results)
- Embeddings: fastembed-rs or candle for local; reqwest for cloud (Voyage / OpenAI) if needed
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
- Provenance — source of ingestion (apple-notes-export, claude-code-transcript, OCR, audio, external-file). Filter by where a note came from.

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
- Trails (memex-style): ordered, named sequences of notes or chunks with **per-waypoint annotations**. A trail is a curated walk through the vault — narrative, not just a set. Useful for investigations ("how I figured out X"), onboarding docs to past projects, or feeding an agent a coherent path rather than a bag of chunks. Trails are queryable via MCP and renderable as a guided walk in the UI.

  **User-authored only.** The clustering / auto-org pipeline never proposes trails — they exist precisely because *the framing is the value*, and that framing has to come from a human who knows why these notes belong together as a path. Auto-discovery would defeat the point. (Clustering can surface "you have a thread crossing these notes" hints — that's a different feature; the user decides whether to turn a hint into a trail.)

  **Annotations live separately from notes.** A trail's per-waypoint commentary belongs to the trail, not to the notes — the same note can appear in many trails with different framings ("this paper, in the context of why I rejected the GPU-resident index" vs. "this paper, in the context of the embedder choice"), and putting trail annotations in a note's frontmatter would balloon it and couple unrelated trails. The principle is locked in; the storage mechanism is not — sidecar yaml, sqlite table, or something else gets decided when trails actually get specced.

  Full mechanics (storage, creation UX, branching vs. linear, agent authorship) deferred to a dedicated `trails.md` when this lands (v4+).


## Auto-organization suggestions

Hiker's clustering pipeline (`clustering.md`) is consumed as a *recommendation engine*, not as durable infrastructure that owns the user's organization. The user organizes their vault; the AI suggests improvements; neither pretends to own the layout. Two flows live on the same engine:

- **One-shot reorganization** — user runs `hiker suggest`, gets a markdown proposal of moves and tags, picks what to apply. Tree is ephemeral; nothing persists except the user's accepted actions and a small rejection log. Each suggestion can be applied as a folder move (filesystem rename) or as a frontmatter tag — the user picks per cluster, with per-note overrides.
- **Saved-tree triage** — user saves a generated tree as a classifier. New notes (default scope: `inbox/`) get auto-routed against it via greedy centroid descent. Confidence-tiered behavior: high → auto-apply with toast + 10s Undo, medium → queue for review, low → leave in inbox.

Triage will not move a note out of any folder *other* than the configured triage scope — the worst case for an over-eager classifier is "wrong subfolder under inbox," never "your important note got moved out from under you." That's the load-bearing safety rule.

There is no per-note `hiker.placement` provenance, no parallel curated-tree-vs-filesystem mental model, and no durable cluster identity carried across runs. The filesystem (and, in tag-mode, frontmatter tags) is the only source of truth. See `suggestions.md` for the full surface — proposal file format, apply mechanic, tag-field configurability, triage thresholds, and the deferred folder-pinning escape hatch.


## Enrichment pipeline

A stage that runs over notes (on ingest, on save, on demand via `hiker enrich`) and produces structured metadata stored back into note frontmatter. The query pipeline reads this metadata via the existing index types — no new index axis needed.

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
- v4+ — first extractor (likely PDF), then incremental: live-preview decorations, more extractors, trails, landmarks, graph view, AI-organize, scrape.

Each step ships something useful. CLI subcommands grow alongside as thin adapters over core: hiker init, hiker stats, hiker ingest, hiker search, hiker related, hiker watch, hiker mcp, hiker scrape, hiker diff.


## License

Apache-2.0. Permissive; explicit patent grant; fine with anyone using or forking it for any purpose.


## Future / deferred

- Apple Notes export ingestion — parse the export (apple_cloud_notes_parser or similar) into one hiker note per item, provenance tagged.
- Claude Code transcript ingestion — scrape `~/.claude/projects/*/` transcripts, split per conversation into hiker notes, provenance tagged.
- Web UI export ingestion — parse Claude.ai data export JSON into per-conversation notes.
- RAG chat over the vault — possible future feature; may not be needed if MCP-from-an-external-agent already covers the use case.
- Cross-index intelligence (combining signals across indexes to detect projects/threads or flag mis-classifications) — defer; weak version is already covered by the query pipeline's rank fusion.
- Per-index "smart" features (temporal anomalies, topic births/deaths, importance dynamics) — out of scope for now; revisit only if a specific need appears.


## Sync / backup

- No VCS. Reference-doc versioning lives in-app (named, semantic, diffable) where it has meaning; personal notes are continuous edits and don't benefit from git overhead.
- Sync between machines: Syncthing — continuous, conflict-aware, no commit ceremony, handles phone.
- Backup with history: OS-level tooling (Time Machine, Backblaze, Restic, btrfs/zfs snapshots, etc.). Index is regenerable from content; only the vault content needs backup.
- Mobile capture: Markor (Android) or Working Copy (iOS) against the synced folder until/unless a mobile client gets built.
- Git remains an option to add later if collaboration or web-publishing needs appear; not a one-way door.
