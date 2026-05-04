# Hiker

A personal notes and knowledge system. Plain markdown on disk, semantic search, agent-accessible.

See `notes.md` for the problem space and `design.md` for the implementation design.


## Core ideals

1. **Plain markdown on disk is the source of truth.** Every note is a file you can open, read, and edit with any tool. The system never traps content behind a database or proprietary format.

2. **The index is disposable; content is precious.** Anything regenerable from notes (embeddings, vector store, full-text index, extraction caches) is throwaway and can be rebuilt at any time. Backup, sync, and version-control rules apply to content only. If a feature requires the index to be authoritative, the design is wrong.

3. **Search and relatedness are the core feature, not organization.** Semantic + lexical search and AI-found connections are what make a pile of notes useful. Manual structure is optional on top.

4. **Start unstructured, layer structure later.** A note dropped into the inbox is a first-class citizen. Folders, tags, trees, and trails are things you grow into when they earn their keep — never required upfront.

5. **AI proposes, the user disposes.** Auto-generated organization (topic trees, suggested tags, folder placement) always shows up as a draft you approve, modify, or reject. The curated state is authoritative; re-running AI proposes diffs against it, never overwrites.

6. **Originals are sacred.** Non-markdown files (PDFs, images, audio) and external files (a `design.md` inside a project repo) are indexed without being moved or modified. Hiker's metadata lives in its own notes alongside, never injected into files Hiker doesn't own.

7. **Every searchable thing is a note.** Markdown notes, sidecar notes for non-md sources, pointer notes for external files, manifest notes for versioned references — all route through the same primitives. There is no parallel data model for "attachments" or "references"; everything addresses notes by ID and gets uniform handling in search, links, trails, and MCP.

8. **One substrate, two readers.** The same vault is navigable by you (UI) and by agents (MCP). Search, related-notes, and trails are exposed identically to both.

9. **Versioning where it has semantic meaning.** External reference docs (datasheets, scraped documentation) get explicit, named, diffable versions. Personal notes don't — they're continuous edits, backed up at the OS level.

10. **Composable knowledge spaces.** Multiple vaults (personal, work, school) coexist, can be searched together or scoped, and stay independent for sync and lifecycle.

11. **Spatial navigation as a first-class idea.** Trails (ordered walks), landmarks (pinned anchors), and the map (graph view) treat the vault as a place you move through, not just a bag of documents.

12. **Open source, built in Rust, on a small native stack.** Tauri + CodeMirror 6 + a Rust core. Apache-2.0 licensed.
