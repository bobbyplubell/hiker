# Extraction

`design.md` ("Source-derived notes", "Extractors", "Versioned sources") sketches the full source-ingestion model; this doc nails down the first concrete slice: the extractor registry, the sidecar write path, the first two extractors (PDF and website-to-markdown), and the crawl loop that turns a single website extractor into a whole-site archiver. Image/audio/office extractors layer on later behind the same registry. The open-ended, agent-authored long tail lives in `plugins.md`'s source plugins, not here.

Goal: a non-markdown source (a PDF on disk, a URL) becomes a searchable hiker note via an extracted-text `.md` sidecar, while the original byte-for-byte artifact is preserved untouched and read in the user's own OS apps.

The headline decisions:

- **Ingestion has two lanes, split by one question: does it need JavaScript?** No → hiker *pulls* it itself (Lane A, no-JS: clip / crawl / feed). Yes → an external tool acquires it and hiker *imports* the result (Lane B). Both converge on one vault-integration spine. [ingest-two-lane-model]
- **The original is never modified, moved, or deleted.** Hiker owns only the sidecar; the source PDF / HTML / binary stays exactly as the user put it and is the canonical artifact. [extract-preserve-original]
- **Extraction is a decoupled leaf crate, `hiker-extract`.** All format/network dependencies (PDF parser, HTTP client, HTML parser) live only there; `core` does *not* depend on it. The `.md` sidecar written to disk is the seam — `core`'s existing watcher ingests it like any other note, with no knowledge that extraction exists. [extract-crate-decoupled]
- **One built-in trait registry routes a source to the first matching extractor; the extracted `.md` sidecar is the indexed unit, cached and keyed on the extractor's version.** Built-in and finite — no plugin loading on this path. [extract-registry] [extract-version-cache-key]
- **Extracted content renders in-app in one of two kinds: markdown, or a static HTML/CSS rendition** (the latter via hiker-render, a no-JS renderer). The *raw* source artifact (PDF, the page's WARC) still opens in the OS handler; hiker embeds no browser engine and runs no JS. [extract-open-original-external] [ingest-output-kind] [htmlview-render]
- **PDF v1 is a pure-Rust text fast path** producing a sidecar; scanned/complex-layout fidelity (marker/docling) is deferred. [extract-pdf-fast-path]
- **Website-to-markdown is static fetch only — no JavaScript, no browser engine** — and the captured page is archived as a self-contained single-file HTML you can open offline. [extract-web-static-fetch] [extract-web-archive-singlefile]
- **Crawling is the extractor contract plus a governed frontier loop**: an extractor optionally emits the links it found, and a generic loop owns scope/dedup/depth/rate/robots. The same loop covers list, hub, and deep crawls as parameters. [extract-contract-next-urls] [crawl-frontier-loop]
- **One note primitive drives every capture.** A *capture-spec note* declares a source (URL or local path) + extraction params in its frontmatter and has a Run action; single clip, crawl job, and RSS feed are three instances of it, sharing one form-over-frontmatter config surface. [capture-spec-note]
- **The note's body is the user's by default; filling it with extracted text is opt-in.** A `fill_body` frontmatter toggle decides: off (default) leaves the body user-owned (notes about the capture) and untouched by Run; on makes the body a linked, read-only extracted sidecar that re-runs overwrite. [capture-fill-body-toggle]
- **Multi-page output lands in a sibling folder, not hidden state.** Crawl/RSS children are written into a `<note-name>/` companion folder beside the capture note and nested under it via `hiker.parent`. [crawl-job-note]
- **RSS/Atom feeds are a scheduled, dedup-by-guid capture instance** — a living subscription whose new entries accrue as child notes on each poll, reusing the same contract + frontier loop. [rss-feed-extractor]
- **Versioning reuses the op-log, not a bespoke version store.** Re-extraction is an `extractor`-authored op, so text history / diff / restore come from the existing op-log surfaces; only the binary artifact needs a retention policy, set by the user as a cascade. [extract-version-oplog] [extract-artifact-retention]
- **Extraction triggers on a hybrid model** — configured `auto_globs` folders extract on appear/change; every other non-md source stays ignored until an explicit "Make searchable" action. [extract-trigger-auto-glob] [extract-trigger-on-demand]
- **The unbounded text-transform tail (per-site scrapers, niche text formats) is a `plugins.md` source-plugin surface**, deliberately distinct from this binary-capable built-in registry. [extract-source-plugin-boundary]


## Ingestion model

Everything that enters the vault sorts into one of **two lanes** by a single question: **does ingesting it need JavaScript / a real browser?** No → hiker *pulls* it itself (Lane A, no-JS). Yes → an external tool acquires it and hiker *imports* the result (Lane B). Both lanes converge on one vault-integration spine — sidecar write, child/parent nesting, wikilink rewrite, op-log versioning, retention — so they are two front doors, not two systems. [ingest-two-lane-model]

| | Lane A — internal pull | Lane B — external import |
| --- | --- | --- |
| Acquires | hiker, in `hiker-extract` (no-JS) | hiker-crawler, an OTS acquisition engine, or any external crawler project |
| Shapes | clip (single fetch) / crawl (`crawl-frontier-loop`) / feed (`rss-feed-extractor`) | a directory in the import format (`import-format-contract`) |
| JavaScript | never | ran in the external tool, never in hiker |
| Output kind | markdown only | markdown, or a static HTML/CSS rendition |

- **Lane A hands every fetched page to an extractor** from the one registry: built-in (`extract-registry`), a net-capable **source-plugin** scraper (`plugin-source-extractor`), or a no-net **wasm extractor** for the niche-format tail. Markdown out.
- **Lane B is one tool-agnostic import format** (`import-format-contract`); hiker does only the vault integration (`extract-manifest-import`). It is the home for anything a real browser must render.
- **Two output kinds.** A note's extracted content displays as straight **markdown** or a static **HTML/CSS rendition** shown by **hiker-render** — a no-JS renderer (the JS that built the rendition ran in the external crawler). The HTML/CSS kind is Lane-B-only; Lane A is always markdown. Both kinds always carry indexable text for search. [ingest-output-kind] [htmlview-render]

### Vocabulary

Pinned terms — kept distinct so the model stays legible:

| Term | Meaning |
| --- | --- |
| **pull** / **import** | hiker fetches (Lane A) / hiker ingests an externally-produced artifact (Lane B) |
| **extractor** | a byte→text transform (built-in, source plugin, or wasm extractor) — never an "engine" |
| **engine** | a **browser engine** (CEF, in hiker-crawler) or an **acquisition engine** (an OTS crawler, e.g. yt-dlp) — Lane-B / crawler-side only |
| **source plugin** | a per-site **scraper**, net-capable, a Lane-A extractor (`plugin-source-extractor`) |
| **wasm extractor** | a no-net byte→text **extractor plugin** for the niche-format tail |
| **crawl** / **hiker-crawler** / **external crawler** | the Lane-A frontier loop / the CEF companion app / a third-party tool — both crawlers are Lane-B producers |


## The capture-spec note

Every capture is one note. Its frontmatter declares the source and the extraction params; a Run action executes it. The three instances differ only in source kind and fan-out, not in mechanism — all share the registry/contract below, the op-log versioning, and one config form. [capture-spec-note]

| Instance | Frontmatter source | Lifecycle | Output |
| -------- | ------------------ | --------- | ------ |
| **Single clip / sidecar** | one URL or one local path | one-shot, re-runnable | the note's own body (`fill_body: true`) or one child |
| **Crawl job** | seed(s) + scope/depth | one-shot snapshot, re-runnable | child notes in the companion folder |
| **RSS feed** | feed URL + `poll_interval` | living subscription | child notes accrue over time |

- **Discriminator + form.** A capture-spec note carries `hiker.kind: capture` (with a `capture.mode: clip | crawl | feed`) in frontmatter. Like the crawl/Trails surfaces, it renders through one form-over-frontmatter view (`crawl-job-form`) toggle-able against the raw markdown — editing the form writes frontmatter, Run launches the capture. A non-`.md` extension with the discriminator is treated as a regular note (mirrors `trail-doc-shape`). [capture-spec-note]
- **Body ownership is a toggle, not a fixed rule.** `hiker.fill_body` defaults `false`: the body is user-owned prose about the capture, never touched by Run, and re-runs are safe. `true` makes the body a linked extracted sidecar — read-only in the editor, overwritten in place by an `extractor` op on re-run (the `extract-sidecar-linked-state` semantics, scoped to the body). `fill_body` is the body's link-state switch; there is no separate `link_state` field on a capture-spec note. [capture-fill-body-toggle]
- **Quick-capture sets the toggle for you.** Hand-authored capture notes default `fill_body: false` (you typed the body; Run must not clobber it). The one-gesture "New from URL" / "Make searchable" sugar auto-creates a `mode: clip` note with `fill_body: true` and Runs it immediately — the intent there is the content, so the body fills. [capture-quick-from-url]
- **Children attach via a companion folder.** A `mode: crawl | feed` note's children are written into a `<note-name>/` folder beside it (`note-companion-folder` in `files.md`) and stamped `hiker.parent` for Vault-mode nesting (`vault-view-crawl-nesting`). The folder is the physical home; `hiker.parent` is the logical truth. [crawl-job-note]


## Crate boundary

Extraction lives in `hiker-extract`, a leaf crate beside `hiker-sync` / `mcp-server`. It owns the registry, the extractors, and their dependencies — a pure-Rust PDF crate, `reqwest`+`rustls`, an HTML parser, the single-file archiver. [extract-crate-decoupled]

- **`core` does not depend on `hiker-extract`.** The crate's only output is files written into the vault (the sidecar `.md`, the archived artifact). `core`'s indexer/watcher picks the sidecar up through the ordinary markdown ingest path — so the network + hostile-input-parser surface never enters the foundational crate, and `core` keeps its no-outbound-network posture.
- **`app` / `cli` wire it.** The trigger (auto-glob, "Make searchable", a crawl preset) is an app/cli-side action that calls `hiker-extract` to produce files; nothing about extraction is reachable from `core`.
- This is a stronger seam than a `core::extract` module would be: an entire dependency tree is confined to a crate the index layer never links.


## Ingest trigger

How a non-markdown source reaches the registry. Hybrid: auto for opted-in folders, on-demand everywhere else, ignored by default. Extends `index.md`'s ingest triggers (`ingest-startup-scan`, `ingest-watcher-driven`) and `watcher.md`'s dispatch — which today route only indexable `.md`/`.txt` — to also enqueue extract jobs for matching non-md sources. The sidecar output then re-enters the ordinary markdown ingest path unchanged.

- **Auto per glob.** `[extract].auto_globs` (gitignore-style globs over vault-relative paths, default empty) names the folders that auto-extract. A non-md source matching an auto-glob enqueues an extract job on appear/change — through both the startup scan and the watcher. Default empty means nothing auto-extracts until the user opts a folder in; there is no whole-vault auto-extraction. [extract-trigger-auto-glob]
- **On-demand elsewhere.** A non-md source outside every auto-glob is not extracted automatically. A filetree right-click "Make searchable" (and extract-on-open) enqueues a one-off extract job for that single file. [extract-trigger-on-demand]
- **Ignored by default.** A non-md source neither matched by an auto-glob nor explicitly extracted stays ignored: it keeps the unsupported (⊘) tree marker and opens in the OS handler (xdg-open / equivalent), exactly as today. [extract-trigger-default-ignore]
- **Tracked after first extract.** Once a source has a sidecar (auto or on-demand), later changes to the source re-extract it through the existing cache-key + linked-sidecar machinery (`extract-version-cache-key`, `extract-sidecar-linked-state`) — no second "Make searchable" needed.

Config shape lives in `[extract]` (`settings.md`).


## Extractor registry and contract

Concretizes design.md's `Extractor` trait + registry. Built-in, trait-based, no runtime plugin loading on this path — the binary formats (PDF, and later image/audio/office) need native libraries a sandbox can't host, and the type set is small and finite. Adding a built-in type is one module + one registration line under `hiker_extract::*`.

- **Route to first match.** The registry holds the ordered built-in extractors and routes a source (by extension / MIME / URL pattern) to the first whose `matches()` returns true. [extract-registry]
- **The return carries optional follow-up links.** `extract()` returns `Result<Option<Extracted>>` where `Extracted` is `{ markdown, frontmatter?, archive?, next_urls: [] }`. An extractor that emits no `next_urls` is a plain one-page extractor; one that emits links becomes crawl-capable without any other change. The crawl loop consumes `next_urls`; a non-crawl ingest ignores them. [extract-contract-next-urls]
- **Fallback chain.** An extractor that matched but can't actually handle the input returns `Ok(None)`, and the registry tries the next match. The PDF fast-path → (deferred) marker fallback is the canonical case. [extract-fallback-chain]
- **Per-source override.** `hiker.extractor: <name>` in the sidecar frontmatter pins a specific extractor, bypassing match order. [extract-per-source-override]
- **Version in the cache key.** Each extractor reports `version()`; the extracted-content cache key is `(source content hash, extractor name, extractor version)`. Bumping a version re-extracts everything that extractor owns on the next ingest pass — same mechanism as `embedder-version-tag` in `index.md`. [extract-version-cache-key]


## Sidecar write path

The extracted text lands in a sidecar note the indexer (`index.md`) treats as an ordinary `.md` file. This extends the v1 "non-markdown files are silently ignored" rule (`status.md` vault tolerance): the extractor produces the sidecar, the indexer ingests the sidecar.

- **Vault-internal source → sidecar beside it.** A non-md file already in the vault extracts to `<full-source-filename>.md` alongside it (e.g. `rm0090.pdf` → `rm0090.pdf.md`), per design.md's storage-mode table. The sidecar carries the `hiker:` frontmatter shape from design.md "Source-derived notes" (`source`, `source_sha256`, `source_mtime`, `type`). Re-extraction overwrites the sidecar body in place (it behaves as a `fill_body: true` capture whose source is a local path). [extract-sidecar-write]
- **URL source → a visible capture note.** A one-off `hiker scrape <url>` (no vault file to sit beside) creates a `mode: clip` capture note in the vault — a normal, visible note, not a hidden cache file. Destination is the configured capture folder (default `clips/`, see `scrape-cmd`); filename is `<slugified-page-title>.md`, collision-suffixed `-2`, `-3`, …, falling back to a slug of the URL path when no title is found. `fill_body: true` for the quick-capture path so the article lands in the body; the `.html` archive sits in the note's companion folder. [extract-sidecar-write]
- **Original preserved.** Writing the sidecar never touches the source bytes. On source deletion the sidecar is orphaned (`orphaned: true`), not auto-removed, so links/trails/search survive — the user decides. This is the concrete enforcement of [extract-preserve-original].
- **Provenance.** Sidecars stamp `hiker.author: imported` and a specific `hiker.provenance` label (`pdf`, `web-scrape`, `rss`, …) per design.md's provenance axis. [extract-sidecar-provenance]
- **Linked vs. unlinked.** A `fill_body: true` body is `linked` — read-only in the editor, overwritten in place by re-extraction. "Unlink from source" flips it to RW and stops re-extraction overwriting it (escape hatch for mangled extraction the user wants to hand-fix); re-link re-extracts and discards hand edits behind a confirm. The toggle and link-state are the same control (`capture-fill-body-toggle`). [extract-sidecar-linked-state]
- **Tree affordance.** A sidecar that sits *next to a non-md source* (`*.<ext>.md`) is hidden in the Files tree by default and surfaced via a "view extracted text" action on the original and in search results. Standalone capture notes (URL clips, crawl/feed notes) are ordinary visible notes — they are not hidden. [extract-sidecar-tree-hidden]


## Viewing: open the original externally

Hiker renders a note's extracted content in-app — markdown, or a Lane-B HTML/CSS rendition via hiker-render (`htmlview-render`). To see a *raw source* as it really looks (a PDF, the page's WARC, the original HTML), the original is handed to the OS handler — hiker embeds no browser engine and runs no JS. [extract-open-original-external]

- **One rule for every non-md source.** "View original" on a sidecar (or the source in the tree) opens the source in the system default app: the browser for an archived web page, the system viewer for a PDF, etc. — the same `xdg-open` path `extract-trigger-default-ignore` already uses for unsupported files.
- **Web pages open from their archive, offline.** A scraped page is opened from its self-contained HTML archive (`extract-web-archive-singlefile`), not re-fetched live, so "view original" works without a network round-trip and shows the page as captured.
- The extracted markdown sidecar stays the indexed/searchable unit regardless of how the original is viewed.


## PDF extractor

v1 is the most-asked-for type and the load-bearing first extractor (design.md build order).

- **Pure-Rust text fast path.** A pure-Rust PDF text extraction crate produces the sidecar body — no external `pdftotext` binary, no bundled poppler C dependency, preserving hiker's single-binary / clean-SBOM posture (the same reasoning that picks `wasmi` over a JIT in `plugins.md`). [extract-pdf-fast-path]
- **Scanned/empty detection.** When the fast path yields empty or garbage text (a scanned/image-only PDF), the extractor returns `Ok(None)` so the fallback chain (`extract-fallback-chain`) can take over, and records a skip reason on the sidecar when no fallback is configured. [extract-pdf-scanned-detect]
- **CommandExtractor escape hatch.** Users who want higher fidelity now can wire `pdftotext`, `marker`, or `docling` through design.md's per-glob `CommandExtractor` without waiting on a native fallback. [extract-pdf-command-escape]

Native marker/docling fallback and OCR (tesseract, via the image extractor) are deferred — see below.


## Website-to-markdown extractor

The lightweight realization of design.md's `hiker scrape`: capture a web page as a clean markdown sidecar **plus a self-contained HTML archive**, with **no JavaScript execution and no embedded browser engine**. Server-rendered content is the target.

- **Static fetch.** One HTTP GET (async `reqwest` with `rustls` for a pure-Rust TLS stack, no system OpenSSL). No script execution, no headless browser. [extract-web-static-fetch]
- **Parse + readability.** An HTML parser builds the DOM; a readability pass isolates the main article content and a markdown serializer emits the sidecar body. [extract-web-readability]
- **Server-rendered data-blob parse.** Before falling back, the extractor checks for structured content already embedded in the static HTML — `<script id="__NEXT_DATA__">`, `__NUXT__`, and `<script type="application/ld+json">` (JSON-LD). This JSON is *parsed, never executed*, and pulls a large slice of framework-rendered "SPA" sites into reach without running their JS. [extract-web-data-blob]
- **Self-contained HTML archive.** Alongside the markdown sidecar, the page is saved as a single `.html` file with its subresources (CSS, images, fonts) inlined as data URIs and scripts stripped, via a native-Rust single-file archiver. This is the canonical artifact "view original" opens — it renders faithfully in any browser, fully offline, with no JS. [extract-web-archive-singlefile]
- **Fallback chain for thin pages.** When the main fetch yields little usable content, try, in order: the page's declared RSS/Atom full-text entry, an AMP variant (`<link rel="amphtml">`), then a print view. First that produces real content wins. [extract-web-fallbacks]
- **Re-fetch versions through the op-log.** `hiker scrape <url>` re-extracts on demand; `hiker refresh` re-fetches all scraped sources. A re-fetch whose content changed lands as an `extractor` op on the sidecar (`extract-version-oplog`) — so history and diff come from the op-log surfaces, not a bespoke version store. Identical content is a no-op. [extract-web-versioned]
- **CLI + provenance.** `hiker scrape <url>` / `hiker refresh` are thin CLI adapters over the extractor. `scrape` creates a visible `mode: clip` capture note in the capture folder (`[extract].clip_folder`, default `clips/`); a `--into <folder>` flag overrides per call. Clips stamp `hiker.provenance: web-scrape`, `hiker.author: imported`, and record `source_url` + `captured_at`. [scrape-cmd]

What this covers vs. doesn't:

| Works well | Partial / fails |
| ---------- | --------------- |
| Server-rendered articles, blogs, docs, wikis | Client-only SPAs with no SSR data-blob and content behind authenticated XHR — sidecar is empty/thin |
| Framework sites that ship a `__NEXT_DATA__`/JSON-LD blob | Infinite-scroll / interaction-gated content (only what's in the initial response) |
| Sites with a full-text RSS feed or AMP variant | Server-side paywalls (no JS to bypass; the bytes aren't sent) |
| JS-based consent/soft walls (the wall script never runs) | Interactive content — maps, dashboards, calculators |

The no-JS stance is the deliberate boundary: it deletes the entire browser-engine dependency (RAM, sandbox, maintenance) and makes the whole path native-Rust string processing. Sites that genuinely need their JS executed are served by the source-plugin fallback (which calls a site's backing API directly rather than running its scripts) or, for crawls, by manifest-import from an external browser-driven tool — not by an engine hiker embeds. [extract-web-no-js-stance]


## RSS / Atom feeds

A feed is the third capture instance: a **scheduled, dedup-by-guid, list-mode** subscription. It reuses the extractor contract and the frontier loop almost entirely — the feed extractor parses the feed and emits each entry's link as `next_urls` (`extract-contract-next-urls`), and the loop fetches them. What a feed adds over a one-shot crawl is recurrence, cross-run dedup, and a living lifecycle. [rss-feed-extractor]

- **Feed extractor.** Parses the feed via `feed-rs` — the unified pure-Rust parser (on `quick-xml`) that normalizes RSS 0.9x/1.0/2.0, Atom, and JSON Feed into one model, so the dialect/date/namespace tail isn't ours to maintain — over a static fetch (`extract-web-static-fetch`), yielding per-entry `{ guid, link, title, published, content? }`. Emits entry links as `next_urls`; the parse never executes anything. The same crate path serves the thin-page RSS fallback (`extract-web-fallbacks`). [rss-feed-extractor]
- **Poll schedule.** A `mode: feed` note carries `poll_interval` (e.g. `30m`, `6h`); a timer enqueues a re-fetch on that cadence — a third trigger kind distinct from the filesystem `auto_globs` (`extract-trigger-auto-glob`) and the manual Run. Polling rides the same non-LLM worker lane as crawls (`crawl-task-queue-lane`). A feed with no `poll_interval` is manual-Run-only. [rss-poll-schedule]
- **Dedup by guid across runs.** The loop's in-run dedup is by URL; a feed dedups by entry `guid` **across** polls, so only genuinely new entries become new child notes. The `guid → child-path` map persists in the feed note's frontmatter (or its companion-folder manifest), the same stable-map idea re-crawl relies on. An entry whose content changed re-extracts as an `extractor` op on its existing child (`extract-version-oplog`); an unchanged entry is a no-op. [rss-guid-dedup]
- **Content-source toggle.** `feed.full_text` picks the entry body: off (default for summary-only feeds) stores the feed-provided `<content:encoded>` / `<summary>`; on follows each entry `link` and runs the website extractor (`extract-web-readability`) for the full article. Per-feed in frontmatter. [rss-content-source]
- **Item retention.** Feeds grow unbounded, so `feed.item_retention` bounds child count — `keep:N` (default), `forever` — reusing the cascade shape of `extract-artifact-retention` (vault `[extract]` default → per-feed frontmatter). Pruned children move to trash, not silent deletion. [rss-item-retention]
- **Subscription, not snapshot.** A feed note's Run is an *ongoing* state, not a one-shot button: the form shows "subscribed / polling every N, last checked …" with pause/resume, where a crawl shows "Run / re-run". Same capture-spec note + companion folder; different lifecycle affordance. [rss-subscription-lifecycle]


## Crawling

Crawling is not a separate engine — it is the extractor contract (`extract-contract-next-urls`) wrapped in a small governed loop. An extractor returns the links it found; the loop decides which to actually visit. This keeps every extractor (built-in or plugin) automatically safe to crawl with, because the dangerous parts live in one place.

### Frontier loop

A queue ("frontier") of URLs with a worker that drains it: pop a URL → extract it → write its sidecar + archive → take its `next_urls` → admit the survivors → repeat until the queue empties or a limit trips. [crawl-frontier-loop]

- **The extractor proposes; the loop governs.** All crawl governance — in-scope check, dedup, depth cap, page-count cap, rate limit, robots.txt — lives in the loop, written once. No extractor can runaway-crawl the open web; the loop is the seatbelt. [crawl-governance]
- **Wikilink rewrite.** The loop assigns each URL its sidecar path as it enqueues, so it holds the full `URL → sidecar` map. A final pass rewrites links among crawled pages into `wikilinks.md`'s id form — `[[<ulid>|<page-title>]]`, stamping each created page's ULID (`wikilink-id-form`, `wikilink-target-stamp`) — so a crawled site becomes a real subgraph (backlinks, graph, trails); same-site links not in the crawl set stay as URLs, external links stay as URLs. The rewrite emits the syntax and stamps IDs regardless of whether the wikilink *rendering* feature has landed; the links become clickable once it does. [crawl-link-rewrite-wikilinks]
- **Re-crawl** re-runs the job from its seed; each changed page re-extracts as an `extractor` op on its existing sidecar (`extract-version-oplog`), and the `URL → sidecar` map stays stable so wikilinks don't break.

### Modes are loop parameters

| Mode | Loop parameters | Use |
| ---- | --------------- | --- |
| **List** | multi-seed, depth 0 (follow nothing) | extract a known set of URLs |
| **Hub** | single seed, depth 1 | harvest one index/hub page's links |
| **Deep** | depth N + scope patterns | archive a section of a site |

[crawl-modes]

- **Scope patterns.** Two gitignore-style glob fields (regex escape hatch if globs prove too blunt on URLs): a **follow-pattern** ("only continue into links matching X") and an **extract-pattern** ("only keep pages matching Y"). The common case sets one; together they express "follow `/docs/**` but extract everything reached" and similar. [crawl-scope-patterns]
- **Extract-the-seed flag.** Whether the seed/hub page itself becomes a sidecar. Defaults off for list/hub (the index page is usually just a launcher) and on for deep crawl. [crawl-extract-seed-flag]
- **List from a note.** The list-mode seed set can be pulled from the links in a note (or the current selection): right-click a note full of URLs → "extract all links" harvests them into sidecars. Ties crawling into the tree/editor UI rather than a separate dialog. [crawl-list-from-note]

### Crawl jobs

A crawl is a `mode: crawl` capture-spec note (`capture-spec-note`). Its frontmatter holds every parameter (seed URL(s), mode, depth, follow/extract patterns, extract-seed flag, chosen extractor via `extract-per-source-override`, `artifact_retention`, rate limits); its body is user-owned (`fill_body: false` — your notes about the crawl), and the run log + captured-page index render in the form, not the body. The job is a normal synced/versioned note, so a crawl is saved and re-runnable by construction. [crawl-job-note]

- **Form over the note, not a modal.** A hamburger menu item ("New crawl…") opens a tab that renders the **capture-spec form over the note's frontmatter** — the Trails-mode pattern (`sidebar-mode-switcher`), not a throwaway dialog. The form surfaces the params, a seed/URL input, an extractor picker, Run / cancel, live progress, and the captured-page index; editing it writes the note's frontmatter, running it launches the crawl. Re-running re-crawls (`extract-version-oplog`). The form is toggle-able against the raw markdown editor in the same tab (mirroring `board-view-toggle`), re-reading the frontmatter on the flip back so neither the form's nor the markdown's edits are lost. [crawl-job-form] [capture-view-toggle]
- **Captured pages live in a companion folder, nested as children.** Each page is written into the job note's `<note-name>/` companion folder (`note-companion-folder` in `files.md`) and stamps `hiker.parent: <job-ulid>` (the manifest-parent precedent from design.md versioned sources). The folder is the physical home in Files mode; the `hiker.parent` stamp drives the logical nesting under the job note in Vault mode (`vault-view-crawl-nesting`). The stamp, not folder membership, is the nesting authority — a stray file dropped into the folder without a parent stamp is just a normal file. [crawl-child-parent]
- **Progress via the task queue.** A crawl runs as a `task-queue.md` job on a non-LLM worker lane, and its per-page extractions are child tasks grouped under the parent crawl job (one rolled-up row in the queue widget, not N). Cancel/progress reuse the queue surface. [crawl-task-queue-lane]
- **On-demand presets** ride the same "Make searchable" menu for the no-config cases: "extract this page", "extract this page's links" (hub), "extract these links" (list, incl. all links in a note), "crawl this section…" (deep — opens the crawl-job form pre-seeded).

## External import (Lane B)

JS-heavy sites that genuinely need a real browser are acquired *outside* hiker and handed in as one documented import format; hiker owns only the vault integration. The frontier loop runs an extractor per page in-process, so it can't drive a browser efficiently and a sandboxed plugin can't drive one at all — Lane B is where browser-grade capture lives. JS always runs in the producer, never in hiker (the Lane-B half of `extract-web-no-js-stance`). [extract-manifest-import]

- **One tool-agnostic format.** A directory with a `manifest.json` describing each item: `{ url, output_file (markdown), html_file?, archive_file? (WARC), title?, links }`. Hiker imports the directory — places the sidecars, builds the `URL → sidecar` map, runs the same wikilink rewrite (`crawl-link-rewrite-wikilinks`), stores archives, and versions on re-import (`extract-version-oplog`). The format is documented and tool-agnostic, so **any** producer targets it: hiker-crawler, an OTS acquisition engine it drives, or an unrelated crawler project (Playwright, Scrapy, …). [import-format-contract]
- **Optional HTML/CSS rendition.** An item may carry a static HTML/CSS rendition (`html_file`) beside its markdown; hiker shows it via hiker-render as the HTML/CSS output kind (`ingest-output-kind`). The markdown is always present as the search shadow. The producer ran the JS to build the rendition; hiker renders only the static result.


## Versioning and retention

Re-extraction (a changed source, a bumped extractor version, a re-crawl) needs a version history. That history is the **op-log**, not a parallel store — this is the concrete shape of design.md's "Versioned sources" for extracted content.

- **Text versions are op-log ops.** A sidecar is a Yrs document (`op-log.md`); a re-extraction applies an `extractor`-authored op to its `accepted` state (`op-log-reextract-replace`). So each re-pull is one attributable history entry, and the note's version history, diff, per-hunk restore, and the status-bar version dropdown all come from the existing op-log/`core::changes` surfaces. The "versions" view of a source is its op-log history filtered to `extractor` + `user` ops. A re-extraction with identical output is a no-op, so versions only accrue on real change. [extract-version-oplog]
- **Artifacts are retained by a user-set cascade.** The op-log versions *text*, not bytes — it can't hold the old PDF or the prior HTML archive. Whether those binary artifacts are kept across captures is a per-source retention policy, resolved as a cascade (lower wins): vault default `[extract] artifact_retention` → per-crawl / per-glob override (stamped onto captured pages) → per-source `hiker.artifact_retention` frontmatter. Values: `latest` (default — keep only the current artifact), `keep:N` (last N captures), `forever`. Because text history is always complete via the op-log, `latest` still loses nothing but the ability to *re-open the page as it looked* at an older version. [extract-artifact-retention]
- Retained per-capture artifacts live hidden under `.hiker/refs/<doc_id>/` keyed by the producing op; the sidecar itself stays a visible note. Per-capture artifacts are device-local (the op-log syncs the sidecar text, not blobs).


## Source plugins for the long tail

Per-site scrapers and niche text formats are an unbounded set; the built-in registry above is deliberately not the home for them. That tail is the `plugins.md` **source-plugin** surface — a Lua-tier, sandboxed, vault-portable, agent-authorable byte→markdown transform that registers as an extractor. A per-site source plugin calls the site's backing JSON API directly (`net:<host>` + `http.fetch`) instead of executing JavaScript, and can emit `next_urls` (e.g. an API pagination cursor) to participate in the frontier loop exactly like a built-in extractor. Its output flows into the same sidecar / versioned-source write path described here. Full spec: `plugins.md` "Source plugins". [extract-source-plugin-boundary]


## Deferred

- **Marker / docling native fallback** — higher-fidelity PDF extraction for scanned/complex layouts as a built-in fallback behind the fast path. Until it lands, the CommandExtractor escape hatch covers the need. [extract-pdf-marker-fallback]
- **Image + audio extractors** — OCR (tesseract) for scanned PDFs and images, whisper.cpp for audio; same registry, separate modules. [extract-image-ocr] [extract-audio-transcribe]


## Out of scope

- **In-app rendering of the *raw* source (a PDF, the WARC, the original HTML).** Raw artifacts open in the user's OS apps (`extract-open-original-external`); hiker renders extracted markdown and the static HTML/CSS rendition (`htmlview-render`), not the raw source. Faithful in-app WARC viewing stays parked in `ideas.md` "Viewers".
- **JavaScript execution / SPA rendering / headless browser inside hiker.** Hiker runs no JS; the source-plugin API-fetch pattern (Lane A) and external import (Lane B) are the supported alternatives, both running any JS outside hiker. hiker-render renders only static HTML/CSS, no scripts. [extract-web-no-js-stance]
