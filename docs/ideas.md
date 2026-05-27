# Ideas

Future and unimplemented concepts. `design.md` is the plan of what's specced and being built; this doc holds the fuzzier, not-yet-committed ideas. An item graduates to its own `docs/<feature>.md` (or into `design.md`) when its design firms up. Slugged items are registered `planned` in `status.md`.

## Source ingestion

- **Apple Notes export ingestion** — parse the export (`apple_cloud_notes_parser` or similar) into one hiker note per item, provenance-tagged.
- **Claude Code transcript ingestion** — scrape `~/.claude/projects/*/` transcripts, split per conversation into hiker notes, provenance-tagged.
- **Web UI export ingestion** — parse Claude.ai data-export JSON into per-conversation notes.
- **draw.io ingestion** — ingest `.drawio` (and `.drawio.svg` / `.drawio.png`) diagrams as a source type. The diagram's textual graph (nodes, edges, labels) extracts into a derived hiker note for search/related coverage; the diagram file stays alongside as the canonical artifact (same dual-file pattern as PDF/EPUB extractors). Node titles become headings, edges a relationships block. [drawio-source-ingest]
- **draw.io MCP tools** — a `drawio_*` tool family (`get_diagram`, `add_node`, `add_edge`, `update_node`) lets an attached agent read and incrementally edit the diagram file in place over drawio's XML format. Layout round-tripping is the hard part; v1 likely restricts to structural-only edits. [drawio-mcp-tools]

## Capture surfaces

- **Browser extension** — companion extension that captures the current page via the existing `hiker scrape` / extractor pipeline. Two popup buttons: **Save to Hiker** (URL + optional selected range → ingest into `inbox/`) and **Save to Hiker and append to active trail** (same capture, plus appends the note as a waypoint on the active trail; greyed when no active trail is set). Talks to the running app over the existing localhost MCP server (`mcp.md`) with a `scrape` tool added to the surface — no new transport or auth. When hiker isn't running, surfaces an "open hiker first" hint rather than queuing. Phone capture (OS share-sheet / Android intent) is the mobile sibling. [browser-extension-capture]

## Viewers

Hiker renders extracted markdown only; sources are viewed in the user's OS apps (`extract-open-original-external`). The one deferred concern beyond that is higher-fidelity *archival* of web sources than a single-file HTML snapshot.

- **High-fidelity WARC capture + replay** — recording the real HTTP request/response set of a visit to a `.warc`, more faithful than the self-contained single-file HTML archive (`extract-web-archive-singlefile`). The static fetch path can capture the resource set it fetches as a basic WARC; full dynamic-page capture (the JS-driven request set, auto-scroll behaviors) needs an external browser-driven tool feeding the manifest-import path (`extract-manifest-import`). The extracted-md sidecar stays the search layer; the WARC is the fidelity/replay layer under design.md versioned sources, and replay opens in an external WARC viewer rather than an in-app renderer. [servo-warc-capture]

## Editor

- **Split view** — shift-clicking a top-strip tab splits the center pane and renders that tab beside the current one. Tile orientation user-selectable. Splits compose with tab kinds (buffer + agent, buffer + graph, two buffers). Each split holds its own active tab and scroll/selection state; closing a split's last tab collapses it. Open splits ride the autosave tab-state snapshot for workspace restore. Lands after `tab-kinds`.
- **Special-character visualization** — Notepad++-style toggle rendering non-printable control characters (NULL `0x00`, BS `0x08`, ESC `0x1B`, DEL `0x7F`, C1 controls `0x80`–`0x9F`, BOM) as distinct inline glyphs. Pairs with `view-show-whitespace-toggle` (whitespace) by covering the *non*-whitespace controls. Rides a decoration provider emitting `Replace { display }` widgets over visible ranges — same shape as the live-preview markdown decorations. View-menu toggle, default off, persisted per-vault. Optional sub-toggle distinguishes line-ending styles (CRLF / LF / CR).
- **Hex view mode** — new `kind: "hex"` tab (per `tab-kinds`) over a file path; renders the standard hex-editor layout (offset / hex bytes / ASCII columns) with hover-pairing between the hex and ASCII halves. Read-only in v1. Opened via filetree right-click "Open as hex" or a View-menu "View as hex" entry. Useful for binary-adjacent files and suspect extracted text. Renders via an egui painter + custom decoration set, lazy on first hex tab.
- **Linked / targeting tabs.** Tabs can be wired to drive or follow each other instead of all tracking the global active buffer:
    - An editor tab points at a Related-notes tab or a Graph tab — and vice versa.
    - A Related-notes tab shows related notes for a chosen *source* tab and opens picks into a chosen *target* tab, rather than always tracking the active editor.
    - A Graph tab either highlights whichever note is active in a chosen tab, or drives a chosen editor tab to open whatever note is hovered/clicked in the graph.

  Generalizes v1's "Related stays bound to the active editor file" (`search-related-stays-bound`) into explicit per-tab source/target wiring.

## Plugins

- **WASM plugin system** — sandboxed WASM runtime for user- or agent-authored extensions: capability-scoped host API, manifest-declared permissions presented at install, hash-pinned via vault-level `plugins.json`. Open-ended UI/automation surface, distinct from the finite built-in extractor set. Full design in `plugins.md`; unbuilt beyond the manifest viewer.

## Agent session capabilities

- **Session capability partitions** — partition agent sessions so no single session ever holds all three prompt-injection "lethal trifecta" legs at once: exposure to untrusted content, vault-write, and arbitrary outbound network. A web-facing session can fetch + read but not write the vault; a vault-editing session can write but not reach untrusted hosts. This is the enforced-partition generalization of the per-task scoping the plugin-authoring loop uses (`plugin-authoring-security` / `mcp-authoring-scoped-subtask`), surfaced as a structural property of a session rather than a UI mode the model can be talked out of. Broad — touches `mcp.md`, the chat surface, `task-queue.md`, and `plugins.md`; wants its own spec when picked up. [agent-session-partitions]

## Ranking

- **Habits-of-association ranking** — optional score bumps on search and related-notes results, computed from user-authored association signals: wikilink edges, shared trail membership, shared tags, folder co-location, temporal co-edit/co-open. Plugs into the rank-fusion stage alongside lexical/semantic/structural scores as a personalization signal. Enable/disable in user or vault config (`[search.ranking]`), master toggle plus ideally per-signal toggles. Default off; depends on `qa.md`'s eval framework to land safely. Excludes proactive crawling — the system curates within existing material only.
- **Per-index smart features** — temporal anomalies, topic births/deaths, importance dynamics. Revisit only if a specific need appears.

## Clustering

- **Chunk-level clustering (chunks as leaves).** A parallel feature to note-level clustering, not a replacement — the curated tree's leaves stay notes (placement and navigation are per-note). Chunk clusters surface signals note-level clustering can't:
    - Cross-note thread surfacing — tightly-clustered chunks from different notes hint at a thread crossing several notes (a "you might be writing about X across these places" hint, not an auto-built trail; trails are user-authored only). [cluster-chunk-thread-hint]
    - Multi-topic flagging — a note whose chunks scatter across many clusters is a split candidate (soft suggestion, not auto-action). [cluster-chunk-multitopic-flag]
    - Section reorganization — chunk clusters within a single note suggest heading reorganization.

## RAG chat

- **RAG chat over the vault** — the embedded chat panel against any configured ACP agent IS this feature; subsumed by the ACP-client milestone (`llm.md`).

## Integrated sync

Now a full spec: `sync.md` (identity/enrollment, the encrypted libp2p transport, and the zero-knowledge server) over the op-log substrate (`op-log.md`). Syncthing covers file-level sync until it lands.

## Graph view enhancements

The vault-wide graph view ships today (`design.md` App-shell `Graph` tab). Deferred enhancements:

- Edge-kind filters — hide/show wikilinks, trail edges, folder-cohabitation edges independently.
- Trail-only mode — show only nodes participating in at least one trail, with trail edges drawn over them.
- Per-source-type node filters (only md, only PDF-derived, etc.) once source-derived notes are real.
- Sidebar cross-highlight — hovering/selecting a folder / trail / cluster lights up matching nodes, and vice versa.
