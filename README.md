<p align="center">
  <img src="images/icon.png" alt="Hiker" width="160">
</p>

# Hiker

A personal notes and knowledge system. Plain markdown on disk, semantic search, agent-accessible.

See `design.md` for the implementation design.


## What is Hiker?

Hiker is a modernized, multimodal take on Vannevar Bush's memex: a personal knowledge store you actually navigate, not just dump into. Every note lives as plain markdown on the filesystem, but the vault is wrapped in a hybrid lexical and semantic index, related-notes discovery, ordered "trails" through content, pinned "landmarks" in embedding space, and a graph "map" view.

Multimodal means non-markdown sources (PDFs, images, audio, web captures) are ingested via sidecar notes that sit alongside the originals without modifying them. Everything addressable, everything searchable, everything routable through the same primitives whether you are at the keyboard or an agent is reaching in over MCP.

The same vault is navigable by a human in the desktop UI and by agents over MCP, with identical search, related-notes, and trail semantics on both sides.

<p align="center">
  <img src="images/screenshot_2.png" alt="Hiker editor with sidebar tree, live-preview markdown, and discovery panel" width="900">
</p>


## Core ideals

1. **Plain markdown on disk is the source of truth.** Every note is a file you can open, read, and edit with any tool. The system never traps content behind a database or proprietary format.

2. **The index is disposable; content is precious.** Anything regenerable from notes (embeddings, vector store, full-text index, extraction caches) is throwaway and can be rebuilt at any time. Backup, sync, and version-control rules apply to content only. If a feature requires the index to be authoritative, the design is wrong.

3. **Search and relatedness are the core feature, not organization.** Semantic + lexical search and AI-found connections are what make a pile of notes useful. Manual structure is optional on top.

4. **Start unstructured, layer structure later.** A note dropped into the inbox is a first-class citizen. Folders, tags, trees, and trails are things you grow into when they earn their keep, never required upfront.

5. **AI proposes, the user disposes.** Auto-generated organization (topic trees, suggested tags, folder placement) always shows up as a draft you approve, modify, or reject. The curated state is authoritative; re-running AI proposes diffs against it, never overwrites.

6. **Originals are sacred.** Non-markdown files (PDFs, images, audio) and external files (a `design.md` inside a project repo) are indexed without being moved or modified. Hiker's metadata lives in its own notes alongside, never injected into files Hiker doesn't own.

7. **Every searchable thing is a note.** Markdown notes, sidecar notes for non-md sources, pointer notes for external files, manifest notes for versioned references, all route through the same primitives. There is no parallel data model for "attachments" or "references"; everything addresses notes by ID and gets uniform handling in search, links, trails, and MCP.

8. **One substrate, two readers.** The same vault is navigable by you (UI) and by agents (MCP). Search, related-notes, and trails are exposed identically to both.

9. **Versioning where it has semantic meaning.** External reference docs (datasheets, scraped documentation) get explicit, named, diffable versions. Personal notes don't, they're continuous edits, backed up at the OS level.

10. **Composable knowledge spaces.** Multiple vaults (personal, work, school) coexist, can be searched together or scoped, and stay independent for sync and lifecycle.

11. **Spatial navigation as a first-class idea.** Trails (ordered walks), landmarks (pinned anchors), and the map (graph view) treat the vault as a place you move through, not just a bag of documents.

12. **Open source, built in Rust, on a small native stack.** Tauri + CodeMirror 6 + a Rust core. Apache-2.0 licensed.


## Features

Status legend: **[implemented]** working today, **[in progress]** partially built, **[planned]** specced and not started, **[deferred]** intentionally pushed past v1. Per-feature granularity lives in `docs/status.md`.

### Editor and vault

- **[implemented]** CodeMirror 6 editor with markdown live preview (cursor-line reveal, fade markers, heading styles, fenced code reveal)
- **[implemented]** Three-column layout: file tree, editor, discovery panel; collapsible and resizable sides
- **[implemented]** Sidebar mode switcher (Files / Clusters / Trails) with persistent per-vault default
- **[implemented]** File tree with drag-and-drop move, inline rename, context menu, sort options, refresh-on-watcher
- **[implemented]** Soft-delete trash with restore, permanent delete, empty-trash, and orphan recovery
- **[implemented]** Pre-write drift check and conflict modal for external edits
- **[implemented]** Vault home screen with stats, recently-modified, and recently-accessed widgets
- **[implemented]** Per-buffer view options: live preview, word wrap, whitespace, line numbers, chunk boundaries, render `.txt` as markdown
- **[implemented]** Plain `.txt` ingest with structure-aware paragraph and sentence packing
- **[implemented]** Note mutation actions menu (LLM-driven in-buffer transforms; v1 entry: "Reformat as markdown")
- **[implemented]** Note properties tab (identity, file state, index state, chunks, access tracking, changes); trail/cluster membership and live-refresh deferred
- **[implemented]** Autosave: per-buffer crash-recovery sidecars and tab-state snapshots; silent restoration on vault reopen, no close-prompt modal
- **[implemented]** Status-bar version dropdown spanning the unified activity feed (current buffer + snapshots + pending agent proposals) with live refresh
- **[planned]** Status-bar goto-line, help-panel keybind enumeration, in-place frontmatter editing inside the properties tab

### Index, search, and discovery

- **[implemented]** SQLite + sqlite-vec store, heading-bounded markdown chunker, fastembed (bge-small) embeddings
- **[implemented]** Filesystem watcher driven incremental indexing, with rename preservation and overflow rescan
- **[implemented]** Related-notes panel driven by per-chunk KNN
- **[implemented]** Vault-wide hybrid search (FTS5 lexical + semantic vectors, RRF fusion) in the discovery panel
- **[implemented]** Type-ahead search with debounce, epoch cancellation, click-to-chunk navigation, full keyboard nav
- **[implemented]** Lexical and semantic option menus (case sensitivity, prefix match, min-similarity, recency bias) persisted per vault
- **[partial]** CLI surface (`hiker reindex`, `hiker query` wired; trash commands and additional verbs still in progress)
- **[planned]** Pluggable embedder backends (OpenAI, Ollama, Cohere, Mistral, HuggingFace) via the `llm` crate
- **[planned]** Folder, tag, lifecycle, and authorship scoping; multi-vault search; saved-collection results
- **[planned]** Tantivy lexical engine swap for ranking quality

### Organization and AI assist

- **[implemented]** Local agent loop with multi-provider LLM core (`core::agent::run_turn`, tool dispatch, cancellation, cap/timeout handling) and chat panel
- **[implemented]** Optional ACP client for external agents (Claude Code, Goose) — hiker MCP server auto-attached to the spawned session
- **[implemented]** Per-feature prompt files (two-tier loader: user `~/.config/hiker/prompts/` + vault `.hiker/prompts/`, vault wins); in-app settings-tab editor planned
- **[implemented]** Unified LLM/agent task queue: priorities, leases, event-streamed progress, direct worker plus external-MCP-client worker support
- **[implemented]** Changes log: append-only record of every note mutation (user + agent authored), per-path history, rollback primitives for agent edits and snapshot restore
- **[implemented]** Staging & review: pending agent proposals (`write_note`, `edit_note`, frontmatter, tags) route through `.hiker/staging/` when review-mode is on, surfaced in the unified activity feed and the version dropdown with author/source filtering
- **[implemented]** Patch review: per-hunk accept/reject for agent `edit_note` proposals, conflicted-state detection, anchor-drift safeguards, batch grouping
- **[partial]** LLM audit log (JSONL at `.hiker/agent-log/`, daily rotation, shared by core agent + MCP + ACP surfaces) done; cost-transparency status indicator planned
- **[planned]** Recursive RAPTOR-shaped clustering with HDBSCAN/GMM, LLM cluster naming, saved topic trees
- **[planned]** Cluster editor surface (interactive tree editing, policies, node operations, triage view)
- **[planned]** One-shot suggestion proposals (move and tag modes) with markdown audit log and rejection history
- **[planned]** Confidence-tiered triage: high-confidence auto-apply with undo, medium queued for review

### Spatial navigation

- **[implemented]** Trails: ordered, memex-style walks through notes — create, append waypoints, reorder, remove with cascade, side-trail hierarchies, capture-to-active, append-cursor insertion, watcher-driven auto-update on note move
- **[planned]** Landmarks: pinned anchors with nearest-landmark tagging in embedding space
- **[planned]** Map: graph visualization of the vault

### Multimodal ingest

- **[implemented]** `.txt` ingest via dedicated chunker
- **[planned]** PDF sidecars via docling/marker
- **[planned]** Image sidecars via tesseract OCR
- **[planned]** Audio sidecars via whisper.cpp
- **[planned]** Web capture and EPUB sources

### Agent surface (MCP)

- **[implemented]** MCP server (`rmcp` over streamable HTTP) brought up per-vault, with discovery file and per-tool toggles
- **[implemented]** Read tools: `search_notes`, `get_note`, `related_notes`
- **[implemented]** Write tools: `write_note`, `set_frontmatter`, `apply_tag`, `remove_tag`; `edit_note` (span-anchored patches, per-edit staging proposals)
- **[implemented]** Audit log and per-tool review-mode routing through staging
- **[planned]** Staging introspection (`list_pending_proposals`, `get_pending_proposal` with per-edit anchor-status liveness) and `amend_pending_proposal`
- **[planned]** Trails, landmarks, collections, and bulk write tools (`move_note`, `delete_note`) — each lands with its backing feature

### Diff viewer

- **[implemented]** Unified diff renderer with line + intraline character-level highlights
- **[implemented]** Snapshot preview diff, dirty-buffer-vs-disk diff, and pending-proposal-vs-disk diff via toolbar toggle

### Platform

- **[implemented]** Tauri shell, Rust core with strict module discipline (rusqlite, fastembed, notify each isolated to one module)
- **[implemented]** Per-vault settings (TOML, user + vault layered, strict load, write-back preserving comments)
- **[implemented]** Tracing-based observability with daily-rotating file logs; chunk-boundary editor gutter
- **[implemented]** Multi-vault open with default-vault auto-open
- **[deferred]** Tracing spans per pipeline stage, in-app log viewer, frontend log bridge
- **[deferred]** `.hiker/ignore` config file
- **[planned]** QA harness: golden-set evaluation, thumbs feedback, synthetic-corpus benchmark

# Related Research
```
RAPTOR https://arxiv.org/html/2401.18059v1
Possible improvement on clustering https://en.wikipedia.org/wiki/Leiden_algorithm
```
