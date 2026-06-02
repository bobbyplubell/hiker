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

## Embedded web engine

A compile-time-optional in-app web engine for rendering — and optionally executing — real web pages. **Under evaluation, not committed.** The lean default build ships no engine; a `cef` feature flag pulls it in. Leading candidate: **CEF** (Chromium, off-screen rendering → egui texture); **Servo** is the Rust-native alternative. A native system webview (`wry`) is ruled out for tight integration — it's an OS overlay window, not a texture, so it can't be a proper egui tab; **Blitz** (Rust, no-JS) is the clean *viewer* if JS is never needed. This revisits the "external viewer only" stance above (`servo-warc-capture`) and `extract.md`'s no-JS / no-in-app-renderer posture. [embedded-web-engine]

Could unlock — a self-contained "web vertical" that leaves the editor / clustering / sync core untouched:

- **Visual + agent-assisted scraper authoring** (the standout) — load a site live, click an element, feed its selector + the rendered DOM to the agent to author a reusable source-plugin (`plugins.md`, `mcp-extract-preview`). Turns the per-site scraper long tail from blind CSS selectors into point-and-click; needs the *interactive* viewer, not just headless.
- **Agent-driven web research** — the agent navigates / reads / interacts and captures findings to the vault; the concrete instance of `agent-session-partitions` (the sandbox is what breaks the lethal trifecta).
- **In-app browsing + native trail capture** — browse in hiker, one-click a page as a trail waypoint; subsumes `browser-extension-capture`.
- **JS-rendered crawl / extraction** — fixes the SPA gap in `extract.md`, retires `extract-manifest-import`, improves `recipe-source-ingest` / `rss-feed-ingest`.
- **Faithful in-app viewing of web archives (WARC)** — rendering a recorded request/response set offline through the engine, more faithful than the single-file HTML archive. (Offline `.zim` / Wikipedia viewing is served without an engine by the no-JS `zim.md` viewer.)

Architecture if pursued: out-of-process renderer + capability starvation (no vault handles; network only via hiker's governed fetcher) breaks the lethal trifecta cross-platform, with the OS sandbox as defense-in-depth — CEF ships Chromium's own sandbox + Google security patches for free, where Servo needs `gaol` hand-wired. Lazy-download the ~150–250 MB engine blob on first enable so the default install stays lean.

Downsides being weighed:

- **Heavy + not pure Rust** — a ~200 MB C++ Chromium blob + real per-page RAM; clears the npm bar but breaks the lean / pure-Rust posture.
- **Two-tier product + permanent maintenance** — even gated off, the ON path is a browser-engine subsystem (CEF version-tracking for security, OSR / input plumbing, sandbox config) the lean build must stay a first-class peer to.
- **macOS packaging** — Chromium needs nested, individually code-signed helper `.app` bundles with per-helper entitlements (JIT, etc.) + notarization; `cargo` can't produce this, so it requires a bespoke macOS build / sign pipeline.
- **Security responsibility** — embedding a JS engine is a CVE surface; CEF's sandbox + patch stream largely mitigate (Servo does not), but it's a posture shift for a local-first tool.
- **Opportunity cost** — a multi-month vertical vs. the core roadmap + the bug backlog.
- **Engine fork unresolved** — CEF (mature, Chromium-grade compat, free sandbox; big C++, fiddly packaging) vs Servo (Rust-native, lighter; immature embedding, partial compat, you own the sandbox).

## Generalized crawler engines

A future refactoring of `hiker-crawler.md`, captured from a brainstorm. The *baseline* it builds on is now committed: the two-lane ingestion model and the one tool-agnostic import format any external crawler can target are specced in `extract.md` (`ingest-two-lane-model`, `import-format-contract`). This section is the still-open part — letting hiker-crawler *host* **off-the-shelf platform crawlers** (yt-dlp, gallery-dl, snscrape, instaloader, RSS-Bridge, per-platform API clients) as engines it drives, alongside the custom CEF point-and-click flow. Today the crawler is built around a single engine shape, `BrowserEngine` (CEF / `NullEngine`, `crawler-engine-trait`), which is fundamentally *CEF-shaped*: load → paint an OSR texture → point-and-click `pick_at` → `rendered_html`.

The reframe: an OTS media crawler is a **whole acquisition pipeline**, not a page renderer. Given a channel URL or a handle it does its own discovery, fetches media + metadata, and dumps a set of artifacts — no live texture, no DOM to pick, and it usually manages its own frontier rather than wanting hiker's frontier loop to drive it URL-by-URL. So it doesn't fit `BrowserEngine` at all; generalizing means splitting that one trait into a small taxonomy. [crawler-engine-taxonomy]

- **Base `Engine`** (`id`, `matches(url)`, `config_ui`) with capability traits layered on: **`Interactive`** (`render` + `pick_at`) — CEF only, the point-and-click authoring path unchanged; **`Acquire`** (`run(target, cfg) → artifacts`) — CEF's crawl-run *and* every OTS tool implement this. All engines feed the *same* preview + emit surface in the bench.

Two roles fall out, and which one an engine has is decided purely by which seam its output lands in:

- **Authoring engines** — hiker-crawler-only; may be heavy / in-crate / CEF. They run (or are point-and-clicked) against a live target and emit a *one-shot* artifact (a manifest-import dir, a source plugin, or a crawl-job note). They are never something hiker itself runs. **Constraint: in-crate "tier-2" engines (CEF and any Rust OTS adapter compiled into the bench) live only in hiker-crawler — hiker-core never deals with a CEF-like engine.** [crawler-authoring-engines]
- **Runtime engines** — the only engines hiker *re-runs* are the two seams core already has: a **subprocess command** (the `extract-pdf-command-escape` `{input}`/`{output}` escape hatch) and a **source plugin** (Lua/wasm, `plugin-source-api-fetch`). Nothing new enters core. An OTS tool hiker re-runs on a schedule is just "a configured command whose output gets `import_dir`'d" — i.e. an **engine-job note** is `CommandExtractor` + `extract-manifest-import` composed into a capture note, no CEF and no heavy dep. [crawler-runtime-engines] [crawler-engine-job-note]

The integration spectrum, cheapest-to-purest: a **source plugin** (native, in-vault, API-fetch) → an **in-crate Rust adapter** (bench-only, may pull sketchy deps since hiker-crawler is already the quarantine zone) → an **external subprocess** (bench-runnable *and* hiker-re-runnable via the command seam + manifest handoff). CEF is just the *interactive* member of the in-crate tier.

- **Live-test bench.** Generalize the side panel so an engine dropdown drives the mode: CEF → today's point-and-click; an OTS engine → a config form + "Run" that executes into a temp dir, parses the output into the manifest model, and previews the resulting markdown through the *same* `hiker-extract` transform (`crawler-preview-fidelity`) plus a media/metadata view. This is the "live test the engine before emitting" capability, and the bench is the right home because it can hold the messy deps. [crawler-live-test-bench]
- **Credentials are pass-through config, never managed by hiker.** Platform auth (cookies file, env var, profile dir) is the engine's own concern; the crawler config just *names where to load them* and forwards the string. hiker / hiker-crawler stores no secrets and runs no login flow — keeps the whole auth surface out of the quarantine. [crawler-engine-credentials]

Open decisions deferred to whenever this firms up:

- **Media output shape** — the biggest unsolved modeling problem. `manifest::Page` is single-doc-shaped (`{ output_file (md), archive_file, links }`), but a gallery-dl / yt-dlp pull is *N media files + per-item metadata*. Options: extend `Page` to carry multiple attachments per note; emit one note per media item with metadata as frontmatter + the media as a companion file; or start **text-only** (titles / descriptions / transcripts) and treat media archival as a later phase. Shapes whether hiker becomes a media archiver or stays text-first.
- **Phasing** — a plausible phase 1 is "generalize `BrowserEngine` → the `Engine`/`Acquire` taxonomy in the bench + one real OTS subprocess engine end-to-end to a one-shot manifest dir, text-only," which touches zero core code and proves the taxonomy + live-test against (say) yt-dlp before any re-runnable engine-job-note work.

## Editor

- **Split view** — shift-clicking a top-strip tab splits the center pane and renders that tab beside the current one. Tile orientation user-selectable. Splits compose with tab kinds (buffer + agent, buffer + graph, two buffers). Each split holds its own active tab and scroll/selection state; closing a split's last tab collapses it. Open splits ride the autosave tab-state snapshot for workspace restore. Lands after `tab-kinds`.
- **Special-character visualization** — Notepad++-style toggle rendering non-printable control characters (NULL `0x00`, BS `0x08`, ESC `0x1B`, DEL `0x7F`, C1 controls `0x80`–`0x9F`, BOM) as distinct inline glyphs. Pairs with `view-show-whitespace-toggle` (whitespace) by covering the *non*-whitespace controls. Rides a decoration provider emitting `Replace { display }` widgets over visible ranges — same shape as the live-preview markdown decorations. View-menu toggle, default off, persisted per-vault. Optional sub-toggle distinguishes line-ending styles (CRLF / LF / CR).
- **Hex view mode** — new `kind: "hex"` tab (per `tab-kinds`) over a file path; renders the standard hex-editor layout (offset / hex bytes / ASCII columns) with hover-pairing between the hex and ASCII halves. Read-only in v1. Opened via filetree right-click "Open as hex" or a View-menu "View as hex" entry. Useful for binary-adjacent files and suspect extracted text. Renders via an egui painter + custom decoration set, lazy on first hex tab.
- **Linked / targeting tabs.** Tabs can be wired to drive or follow each other instead of all tracking the global active buffer:
    - An editor tab points at a Related-notes tab or a Graph tab — and vice versa.
    - A Related-notes tab shows related notes for a chosen *source* tab and opens picks into a chosen *target* tab, rather than always tracking the active editor.
    - A Graph tab either highlights whichever note is active in a chosen tab, or drives a chosen editor tab to open whatever note is hovered/clicked in the graph.
    - The same per-tab source/target wiring extends to the cluster vector-space visualization (`cluster-vector-viz`) — a viz tab can highlight the active editor's note within the projection or open clicked points into a chosen editor tab.

  Generalizes v1's "Related stays bound to the active editor file" (`search-related-stays-bound`) into explicit per-tab source/target wiring. [tabs-linked-targeting]

## Rendered-diagram node navigation

- **Click a diagram node → its source, with footprint highlight** — clicking a node in a rendered mermaid diagram selects the first occurrence of that node's id in the fenced source. One action, two payoffs: it drops the caret into the node for editing, and — because the existing occurrence-highlight fires on any non-empty selection (`editor-view/src/highlight.rs`) — every other place that node appears lights up. That turns "a node is referenced many times" from a problem into the feature: you see the node's whole footprint. Extends `editor-widgets.md` (`widget-block-click-to-edit`, `widget-table-cell-edit`, `editor-widget-click-regions`); the whole-diagram body click (caret at source top) stays the fallback. The app-side logic (emit click-zones for *all* regions a diagram reports, select the first occurrence of `region.id`) is **diagram-type-agnostic** — built once, it works for whatever types report hit regions, and new types light up with zero app change.

  Why deferred — the blockers:
  - **Only 4 of ~30 types report per-node regions today.** `hiker_mermaid::render_with_regions` derives `HitRegion`s only for flowchart / class / state / ER, whose renderers expose positioned, identified node boxes; every other type goes through `dispatch()`, which returns the SVG only. There's no shared positioned-element representation — each diagram type is a bespoke renderer — so full coverage is ~26 separate per-type changes in the `hiker-render` mermaid submodule (each a `render_X_with_regions` that surfaces its elements + a source-meaningful id).
  - **Not every type has a sensible node→source mapping.** Node-ish types do (sequence participants/messages, gantt tasks, pie slices, mindmap/timeline/journey nodes, gitgraph commits, requirements); chart-ish types (xychart, quadrant, radar, sankey, packet, info, treemap) have no discrete addressable source token.
  - **`HitRegion` carries the node id string, not a source byte span** — so the app maps id→source by first-occurrence (word-boundary) search, inheriting the occurrence-highlight feature's substring imprecision (selecting `A` also lights `A` inside `AB`). A renderer that reported per-element source spans would remove both this and the heuristic.

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

- **Reusable clustering recipes (named param presets).** A way to save a `cluster-review` configuration (scope + method + params + naming) as a named, reusable preset decoupled from any one tree, so a build setup can be re-applied across vaults / trees without re-tuning. Today an Evergreen tree already carries its own recipe (`clustering.md` build-scope/method), so the lighter version is "clone a tree to inherit its params"; this is the explicit-preset upgrade if that proves too implicit. Surfaced as a save/load on the review tab's config section — *not* a separate job-note (the tree `.md` already fuses recipe + artifact, unlike the external-input extractor jobs). [cluster-config-presets]
- **Seeded / anchored clusters.** Let the user supply a list of expected clusters as a build input — anchor topics the partitioner is biased toward (semi-supervised: seed centroids / must-link hints), instead of a purely unsupervised pass. A new `ClusterParams` input + a config-section affordance, gated on its own merit. [cluster-seeded-anchors]
- **Sticky manual overrides across re-runs.** Make a user's manual reassignments (move note → cluster) and per-note pins survive a rebuild / re-triage, so curation isn't lost when an Evergreen tree re-runs against fresh data. The cluster editor already does the reassignment (`cluster-editor-node-operations`); the new part is persisting those decisions as build/triage constraints the next pass respects. [cluster-sticky-overrides]

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
- **3D graph mode** — a 2D/3D toggle on the graph tab, reusing the shared 3D scene substrate ([[scene3d-shared]]). Edge-set fork: the cluster hierarchy (small, cheap, structural) vs the note wikilink graph (the "knowledge-graph" look — needs Barnes-Hut / octree for the force layout at note scale, runs on a background thread, positions cached). The note-link feed doubles as the visualization of the link signal a "Connections" clustering lens would cluster on. 3D is explore/delight-oriented; keep 2D for precise structure reading. [graph-3d-mode]

## Cluster / embedding visualization

Backburnered while the clustering UX core (path-only links, editable-md outline, flat-first recursion, representation generalization) is the focus. The visualizer is the delight layer on top, not load-bearing.

- **Cluster vector visualization** — projection of the existing note-level embeddings (`cluster-note-embeddings`) rendered in a canvas tab, colored by cluster membership from the active cluster tree, label-on-hover, click-to-open. Compatible with [[tabs-linked-targeting]] — a viz tab can highlight the active editor's note and/or drive a target editor tab. [cluster-vector-viz]
- **3D embedding galaxy (GL)** — the committed direction for the viz above: a 3D, orbit-able point cloud rendered through an `egui_glow` paint callback (instanced point sprites, an MVP camera uniform, depth test + distance size-attenuation for the "galaxy" look). **No UMAP dependency** — positions come from a force-directed layout (3D has more room to separate than 2D; PCA-to-3-components is the cheap fallback). The projection is a **vault-level artifact** — a function of embeddings, not of any tree — so it's computed/cached once (a `projection_coords` table, sibling to `cluster_centroids`, carrying `x,y,z`), new notes are transformed into the existing space, and a refit only happens on an explicit re-layout. Each tree supplies a *coloring overlay*, so switching trees recolors without relayout. Interactions are deliberately light: orbit / zoom / pan the camera, hover→preview card (reuses `paint_preview_card`), click→pick/open a note. It is **not** a reorg or multi-select surface — positions are read-only (derived from the embedding), so you launch notes from it, you don't reorganize by it. Decorative-first; can downsample at scale since it's orientation, not a workspace. Picking stays CPU-side (transform the cached positions by the same MVP, take the front-most within radius). [cluster-embedding-galaxy-3d]
- **One 3D scene, two feeds** — the 3D embedding galaxy and the [[graph-3d-mode]] graph are the same renderer (camera, orbit controls, point + line shaders, CPU picking), differing only in inputs: the galaxy is nodes-only positioned by embedding *similarity* with edges hidden; the graph is nodes **plus edges** positioned by a force layout of an explicit edge set with edges drawn. Build one 3D scene component and instantiate it twice rather than two engines. Runs on the glow backend already linked via eframe (no new render-backend dep). [scene3d-shared]

## Editor polish + missing essentials

These are mostly small but visible gaps in the editor surface:

- **Editor zoom** — Ctrl+scroll / Ctrl+`=`/`-` scales the active editor's font size; per-tab, not persisted. [editor-zoom]
- **Collapsed-section indicator** — when a heading section is folded, render a thin `...` glyph at the fold so the collapsed range is visible. [editor-fold-indicator]
- **Minimal gutter mode** — view-menu toggle that replaces gutter line numbers with an unfilled dot per line (and a dot-in-dot glyph at a fold-capable line). Aimed at a clean typing view. [editor-gutter-minimal]
- **Multi-line tab strip** — when the tab strip overflows, wrap to additional rows (à la VS Code / Notepad++) instead of relying solely on the `▾` overflow button. View-menu toggle, default off. [tab-strip-multiline]
- **Sidebar list-style tabs** — a leftmost button-strip item that opens a vertical tab list in the leftmost panel; supports tab hierarchy (parent / child) so deep tab sets stay scannable. [tab-list-sidebar]
- **Close-all-tabs verb** — context-menu entry on the tab strip; "Close others" / "Close to the right" follow the same surface. [tab-close-all]
- **Vertical split in the left sidebar** — split the left panel vertically to show two sidebar modes at once (e.g. filetree + trails). Each pane has its own mode switcher. [sidebar-vertical-split]

## Markdown editing affordances

- **Standard markdown toolbar** — a thin toolbar over the editor pane with bold / italic / heading / bullet-list / numbered-list / quote / code / link buttons. View-menu toggle, default off (keyboard-first); on by default in reader-adjacent modes. [editor-md-toolbar]
- **Copy as rich text** — selection → HTML (and optionally Jira/Confluence markup) on the clipboard, so pasted content keeps formatting in external tools. Conversion runs over the same markdown parse the renderer uses. [editor-copy-rich-text]

## Notifications + history

- **Notifications panel** — singleton tab that archives every toast and completed task notification, so dismissed messages are still inspectable. Filterable by source (task queue / sync / extractor / etc.). Lives alongside the existing toast surface; toasts continue to dismiss as today. [notifications-panel]
- **Toast surface revision** — revisit the current toast styling/placement (TBD; tracked as a placeholder for the visual + UX pass once `[notifications-panel]` lands). [toast-revision]

## Source ingestion (more types)

- **RSS feed ingestion** — periodic fetch of a configured feed list; each entry becomes a sidecar note via the web extractor (`extract-web-readability` reused on the entry's link, or the feed-supplied full-text when present). The feed list itself lives in a manifest note (the same shape as crawl-job notes). Cheaper than the full crawler — single seed list, no scope traversal. [rss-feed-ingest]
- **Code-file ingestion source type** — admit code files (e.g. `.rs`, `.py`, `.ts`) to the indexer alongside `.md` / `.txt`. A code chunker (line-pack v1; tree-sitter-aware later) sits beside `core::chunker::txt`. Discoverability boost for devs storing notes adjacent to source. Defer per-language polish. [code-source-ingest]
- **Zim wiki ingestion** — read `.zim` archives (offline Wikipedia, etc.) as a source type: extract per-article markdown sidecars into a vault folder, archive the original `.zim` as the canonical artifact. Niche; tracked for completeness. [zim-source-ingest]
- **Recipe source type** — recognized recipe schemas (schema.org `Recipe`, common JSON-LD shapes) extract into a structured note (ingredients / steps / yield / time) rather than freeform prose. Reuses the website extractor's data-blob parse (`extract-web-data-blob`). [recipe-source-ingest]
- **External source — solidify spec + implementation** — the "external" source category (notes whose canonical artifact lives outside the vault, referenced by URL/path) is half-specced today. Nail the storage shape, refresh model, and offline behavior; align with the sidecar pattern from `extract.md`. [external-source-solidify]

## Auto-organization

- **Inbox rules** — deterministic auto-organization for newly created notes, expressed as user-authored rules (regex over basename + body) that move/tag notes into folders. Sits alongside the agentic Trees system (`docs/trees.md` / `cluster-editor.md`) as the predictable / non-AI option. Rule list lives in `[inbox]` (`settings.md`) or in a manifest note; runs on the watcher's create event. First-match wins; explicit "no rule matched" disposition keeps the note in `inbox/`. [inbox-rules]
- **Auto-index from headers** — generate a per-vault (or per-folder) index note from the H1/H2 headers of contained notes. Runs as a deterministic pass over the index data (`store::notes`); the resulting index is a real note so links to it survive. Optional regex-based auto-tagging (e.g. `PWS-[0-9]{4}` → tag) uses the same scanner. [auto-index-from-headers] [auto-tag-from-regex]

## Git integration

- **Built-in git diff viewer** — wire git as a source for the existing diff UI (`diff.md`): "compare with HEAD" / "compare with branch" on a note opens a diff tab using the same renderer that powers op-log diffs. No commit/push UI in v1 (git stays a user-managed tool). [git-diff-view]

## Tooling

- **Terminal tab** — a `kind: "terminal"` tab (per `tab-kinds`) hosting a real PTY (`portable-pty` or similar), so a working shell sits alongside notes without leaving the app. Per-tab cwd defaults to the vault root. v1: one tab = one shell, no multiplexing. [terminal-tab]

## Agent chat improvements

- **Agent chat polish** — collected polish items for the embedded chat: (a) live markdown preview in the compose box, not raw text; (b) formatted/streamed rendering of agent responses (not flat text); (c) tool-call details collapsed by default with expand-on-click; (d) scrollable message body (current widget pins); (e) top-bar reflow at narrow widths. [agent-chat-polish]

## Storage layout

- **Consolidate sqlite under `.hiker/db/`** — minor cleanup. Today `.hiker/index.db` lives at `.hiker/` top level while op-log meta dbs sit under `.hiker/oplog/`. A unified `.hiker/db/` would tidy the layout but isn't load-bearing (sync ships op-log frames, not these local sqlite files). Low priority. [db-consolidate-subdir]
