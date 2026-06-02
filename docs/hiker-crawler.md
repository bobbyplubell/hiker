# hiker-crawler

A separate, JS-capable companion app. `extract.md` deliberately draws hiker's
extraction boundary at **no JavaScript, no embedded browser engine**
(`extract-web-no-js-stance`) and externalizes JS-heavy sites to "an external
browser-driven tool" via the manifest-import seam (`extract-manifest-import`).
`hiker-crawler` *is* that tool, made first-class: a standalone egui app that
embeds a real engine (Chromium, via CEF — the Chromium Embedded Framework),
lets you load a live site, point-and-click the DOM you want, and emit one of
three artifacts hiker already knows how to consume — a **crawl-job note**, a
**source-plugin extractor**, or a **manifest-import directory** (markdown +
WARC archives). The dangerous, heavyweight machinery (a full Chromium with JS,
a bundled multi-hundred-MB CEF distribution running multiple processes, live
fetching of the open web) lives **only here**, quarantined out of hiker's
clean-SBOM core. Choosing the crawler is the user accepting that baggage; the
notes app never pays for it.

Goal: turn "this site needs a browser" from a dead end into a guided workflow —
load the page, select what matters, and hand hiker a reproducible job or a
per-site extractor that produces the *same* markdown hiker would, plus the
archived bytes.

The headline decisions:

- **Separate app, shared chrome.** A new excluded-from-workspace crate
  `hiker-crawler` with its own binary, built on the same egui/eframe +
  `egui_workbench` shell hiker-app uses, so it looks and feels like hiker.
  It is **not** a workspace member (like `plugins/`): it bundles the
  multi-hundred-MB CEF (Chromium) distribution and links its C++ FFI binding,
  so it builds separately and never blocks `check.sh`. [crawler-app-shell]
- **Browsing is `egui_workbench` tabs, not Chromium's chrome.** CEF runs purely
  windowless (OSR) — it has no browser UI of its own. Each open page is an
  `egui_workbench` tab backed by one windowless CEF browser; the workbench tab
  strip (new / close / switch) drives them, so the crawler's tabs look and feel
  like hiker's. A global CEF runtime (init / subprocess / message-loop pump) is
  split from the per-tab browser (load / paint / input / DevTools). [crawler-browser-tabs]
- **The unsafe surface is quarantined here.** A full Chromium with JS, open-web
  fetching, the C++ CEF dependency, and CEF's multi-process model never enter
  `hiker-core` / `hiker-extract` / `hiker-app`. `extract.md`'s no-JS posture
  and `deny.toml`'s clean-SBOM guarantee stay intact for the notes app; the
  crawler is where the "sketchy" capability is allowed to live, opt-in and run
  by the user — who accepts the browser-integration baggage by choosing it.
  [crawler-quarantine]
- **Shared behavior is shared code, never a second copy.** Anything hiker and
  the crawler both need lives in one crate they both compile; logic trapped in a
  binary (`hiker-app`) or a too-heavy crate (`hiker-core`) is *extracted down*
  into a leaf crate, not duplicated. This design's extractions: `hiker-llm`
  (from core) and `hiker-theme` (from app), plus a small public surface on
  `hiker-extract`. The crawler never forks hiker logic and never depends on
  `hiker-app`. [crawler-share-code]
- **The engine sits behind a `BrowserEngine` trait.** CEF is one impl behind a
  `cef` cargo feature (default off); a `NullEngine` placeholder lets the shell
  build and run without it. Same swappable-seam discipline as `WasmEngine`
  (`plugins.md`) and `search.md`'s `LexicalEngine` — and it keeps the door
  open to a lighter back end (CDP-driven external Chromium) without touching
  the rest of the app. [crawler-engine-trait]
- **CEF renders off-screen into an egui texture.** CEF's off-screen rendering
  (OSR) hands back a BGRA pixel buffer in an `OnPaint` callback; the shell
  uploads it as an `egui::TextureHandle` and draws crawler chrome on top — a
  plain CPU-buffer-to-texture upload, so no GL context sharing or wgpu-version
  boundary is involved. Input is forwarded to
  CEF as host mouse/key events. [crawler-cef-engine]
- **Point-and-click element picker.** Hover/click the live page; the engine
  hit-tests the node and the picker derives stable selector candidates (id /
  class / attribute / nth-child path) plus the node's HTML/text. The user
  confirms a selection set — the basis of every emitted artifact.
  [crawler-element-picker]
- **Three emit targets from one selection.** (1) a `mode: crawl` capture-spec
  note (`crawl-job-note`); (2) a Lua/wasm **source plugin** (`plugins.md`
  source plugins) optimized for the site; (3) a direct **manifest-import
  directory** of markdown + WARC. The picked selectors + scope params drive all
  three. [crawler-emit-targets]
- **Element picking and capture ride CEF's automation surface.** Hit-testing,
  rendered-DOM serialization, and selector probing run as JS in the page
  (`document.elementFromPoint`, `outerHTML`, an injected highlight overlay) via
  CEF's JS execution / DevTools (CDP) path; WARC capture taps CEF's
  resource/CDP Network responses for true wire-level archives, not just a DOM
  re-serialization. [crawler-cef-engine]
- **Link-following is a per-job choice, not a fixed behavior.** When authoring a
  job the user picks how the frontier is fed — a **static list**, **dynamic
  discovery** within scope, or **plugin/API-driven** `next_urls` — and the
  choice templates into the crawl-job's `mode` + `depth` + scope frontmatter.
  These are exactly `extract.md`'s `crawl-modes` (modes are loop parameters), so
  hiker's frontier loop is unchanged. [crawler-link-strategy]
- **Deterministic or agent-assisted authoring.** The crawl-job and source-plugin
  emitters run in two modes: deterministic (template the selectors straight into
  frontmatter / a Lua skeleton) or agent-assisted — the selected DOM + observed
  endpoints handed to the *shared agent chat loop* (`crawler-shared-chat`), which
  authors and **reworks** the extractor across preview/validate iterations rather
  than a one-shot call. Deterministic is the default and always available offline;
  the agent path is opt-in. [crawler-emit-mode]
- **API-fetch plugins are authored from observed traffic, then validated no-JS.**
  For a site that hydrates from an endpoint, the crawler correlates each picked
  field to the network response that produced it (tapping the CDP Network it
  already records for WARC), authors a static API-fetch plugin, and validates it
  with a real no-JS fetch before handoff — so a no-JS artifact ships only when base
  hiker can reproduce it. Captured traffic is redacted of secrets before any agent
  sees it. [crawler-api-fetch-discovery] [crawler-validation-gate] [crawler-traffic-redaction]
- **Preview is WYSIWYG with hiker.** The in-app preview/crawl-run reuses
  `hiker-extract`'s readability + `htmd` markdown pipeline on the engine's
  *rendered* DOM, so what you see in hiker-crawler is byte-identical to what
  hiker produces on ingest. This is the "same results" guarantee — fidelity
  comes from sharing the extractor, not reimplementing it. [crawler-preview-fidelity]
- **Direct site → WARC + .md sidecar.** For JS-only pages, capture the rendered
  page as a WARC (higher fidelity than the single-file HTML archive) plus the
  extracted markdown, written into a `manifest.json` directory hiker imports
  unchanged via `hiker-extract::crawl::manifest::import_dir`. JS ran in the
  crawler; hiker only does vault integration. [crawler-direct-warc]
- **Output is always the existing seam.** Everything hiker-crawler produces
  flows back through a path hiker already has — frontmatter on a capture note,
  an installed source plugin, or a manifest-import dir — so the crawler adds a
  *front end*, not a parallel ingestion path. [crawler-handoff]


## Crate boundary

`hiker-crawler/` is a standalone crate, **excluded** from the root workspace
(`exclude = [..., "hiker-crawler"]`) for the same reason `plugins/` is: its
optional CEF dependency links a C++ Chromium FFI binding, bundles a
multi-hundred-MB CEF distribution, and runs multiple processes — it would
otherwise force every `cargo build`/`check.sh` to pull a browser. It is built
on its own (`cd hiker-crawler && cargo run`).

- **Depends downward only.** It may depend on `hiker-extract` (to reuse the
  readability/markdown pipeline, the crawl frontier loop, and the manifest
  writer) and the shared UI crates (`egui_workbench`, `editor-egui`). It must
  **not** be depended on by any hiker crate — nothing in the notes app links
  the browser engine. [crawler-quarantine]
- **CEF is optional and feature-gated.** `default` features build the shell +
  `NullEngine` with no CEF in the graph (so the scaffold runs today). The `cef`
  feature wires the real engine; until the CEF binding + distribution are
  pinned, the dependency line is documented but inert. [crawler-engine-trait]
- **Reused, not reimplemented.** The markdown a preview/archive produces comes
  from `hiker-extract` (`extract-web-readability`, the `htmd` serializer), and
  the import lands through `hiker-extract::crawl::manifest`. The crawler owns
  the engine, the picker UI, and the emitters — nothing about extraction
  semantics is duplicated. [crawler-preview-fidelity]


## Sharing and code reuse

**Standing rule: any feature or logic shared between hiker and the crawler is
shared *code*, never a second implementation.** If the two need the same
behavior, it lives in one place both compile. When the thing they need is
currently trapped in a binary crate (`hiker-app`) or a heavy crate the crawler
must not pull in whole (`hiker-core`), the fix is to **extract it down into a
leaf crate** both depend on — the same discipline that produced `egui-workbench`,
the `editor-*` crates, `hiker-llm`, and `hiker-extract`. The crawler never
forks hiker logic and never depends on `hiker-app`; extraction, not duplication,
is the only sanctioned way to share. [crawler-share-code]

Already shareable as-is (the crawler just adds the dep):

- **`egui_workbench`** — the activity-bar + dockable-tabs shell, so the crawler
  uses hiker's layout. [crawler-app-shell]
- **`hiker_extract::crawl::{PageSource, run}`** — the frontier loop already
  takes a fetcher trait (`PageSource::fetch(url) -> Option<Extracted>`), with
  `RegistryPageSource` as the static-fetch impl. The crawler's in-app run
  implements `CefPageSource: PageSource` (render in CEF → post-JS DOM → the
  shared transform → `Extracted` with WARC + observed `next_urls`) and calls
  `run(...)`; all governance (scope/dedup/depth/robots/wikilink-rewrite/companion
  writes) is reused verbatim. No new frontier abstraction is needed.
  [crawler-crawl-run]
- **`hiker_extract::crawl::{CrawlParams, write_job_note}`** and
  **`manifest::import_dir`** — the crawler emits crawl-job notes via these
  shared types rather than hand-templating frontmatter, so the on-disk shape
  can't drift from what hiker reads back. [crawler-handoff]
- **`editor-egui` / `editor-md`** — for the markdown preview/edit pane.

Extractions this design requires (each lands as a leaf crate consumed by hiker
*and* the crawler — hiker keeps using the same code, nothing is duplicated):

- **`hiker-llm`** — lift `hiker-core`'s LLM client (`core/src/llm.rs`:
  `GraniteLlmClient`, the `Message`/`ToolDef`/`AgentChunk` types, `ProviderConfig`,
  the provider wiring) into a leaf crate. `hiker-core` then depends on it (the
  config-bridge fns `from_config`/`provider_config_from` stay in core, since
  they reference core's config types); the crawler depends on it directly for
  LLM-assisted authoring (`crawler-emit-mode`). One client, shared — not a
  crawler-side copy. (`clippy::pub_use` is denied, so core's import sites are
  updated to the new path rather than re-exported.) [crawler-shared-llm]
- **`hiker-theme`** — lift `hiker-app`'s theme/style (`app/src/theme.rs`) into a
  leaf crate so the crawler matches hiker's colors/fonts/spacing exactly, not
  just its layout. Both the app and the crawler apply the same theme.
  [crawler-shared-theme]
- **`hiker-extract` surface** — expose a public `extract_from_html(html,
  base_url) -> Extracted` (today `to_article`/`best_body` are `pub(super)`) so
  the preview and `CefPageSource` run the *same* transform hiker's ingest does;
  derive `Serialize` on `manifest::{Page, Manifest}` so the crawler writes the
  manifest with the shared types instead of a mirror. [crawler-preview-fidelity]
- **`hiker-render`** — the in-app HTML/CSS renderer (`extract.md` `htmlview-render`),
  shared so the crawler's preview displays the captured page's HTML/CSS rendition
  through the *same* renderer hiker core uses. WYSIWYG across the boundary: what the
  author sees in the crawler is what the vault shows. [crawler-shared-render]
- **The agent chat loop** — lift hiker-app's chat panel + the message-history /
  tool-dispatch loop (`core::agent`, optionally `core::acp`, `llm.md`) into a leaf
  crate both depend on, so source-plugin authoring is an iterative agent
  conversation (author → preview → rework) instead of the one-shot `block_on` call.
  The crawler wires an authoring-scoped tool set (`mcp-fetch-raw`,
  `mcp-extract-preview` against draft extractors, `mcp-propose-plugin`) — never
  vault-write tools (`mcp-authoring-scoped-subtask`). Heavier than
  `hiker-llm`/`hiker-theme`: the loop is entangled with `core::mcp` + sessions, so
  the extraction must keep `hiker-core` unlinked from the crawler. [crawler-shared-chat]

These extractions are behavior-preserving moves; `check.sh` is the gate that the
refactor didn't change hiker.


## The engine seam

```rust
pub trait BrowserEngine {
    fn load(&mut self, url: &str);
    fn poll(&mut self);                       // pump engine events each frame
    fn current_url(&self) -> Option<String>;
    fn rendered_html(&self) -> Option<String>; // post-JS DOM — the extractor input
    fn pick_at(&mut self, x: f32, y: f32) -> Option<Hit>; // hit-test for the picker
    fn capture_warc(&self) -> Option<Vec<u8>>;  // rendered page as WARC bytes
}
```

- **`NullEngine`** (default): no JS, no rendering; `rendered_html` returns
  `None` and the shell shows "build with `--features cef` for live pages".
  Keeps the app runnable and the emitters testable without a browser.
- **`CefEngine`** (`cef` feature): owns a CEF browser in off-screen-rendering
  mode, drives navigation, and uploads each `OnPaint` BGRA buffer as an egui
  texture. `rendered_html` / `pick_at` run JS in the page via CEF's execution
  path (`document.elementFromPoint`, `outerHTML`, selector probing);
  `capture_warc` taps CEF's resource/CDP Network responses. [crawler-cef-engine]


## Element picker

The bridge from "a live page" to "a selection set". [crawler-element-picker]

- **Hit-test → candidates.** A click hands the engine a point; it returns the
  hit node and the shell derives ranked selector candidates: `#id` (most
  stable), unique class, attribute (`[itemprop=...]`), then a bounded
  `nth-child` CSS path as the fallback. The node's outer HTML and text accompany
  it for preview.
- **A selection is a named field.** The user labels each pick (`title`,
  `body`, `date`, …); the set of `{ field, selector }` pairs is the extractor
  spec. Multi-select (e.g. "every `.article-card` on a listing page") yields a
  repeat/`next_urls` field for hub/list crawls.
- **Live re-highlight.** Hovering a candidate selector re-highlights matching
  nodes in the engine so the user sees exactly what a selector captures before
  committing.


## Link following

How the frontier is fed is a **per-job** control authored in hiker-crawler, not
a fixed behavior. The user picks one of three strategies; the choice templates
into the crawl-job frontmatter (or the in-app run config for a direct-WARC run),
and each maps onto `extract.md`'s existing `crawl-modes` so hiker's frontier
loop sees only ordinary parameters. [crawler-link-strategy]

| Strategy | What it does | Emits |
| -------- | ------------ | ----- |
| **Static list** | A frozen set of URLs — pasted, or harvested by a repeat-selector pick over the loaded page | `crawl_mode: list`, `depth: 0` (follow nothing) |
| **Dynamic discovery** | Follow links from the seed within scope: `follow` / `extract` glob (or `re:` regex) patterns + a depth cap | `crawl_mode: deep` (or `hub` at depth 1) + `crawl-scope-patterns` |
| **Plugin / API-driven** | The chosen extractor owns discovery, emitting `next_urls` (e.g. an API pagination cursor found via network observation); scope only guards them | extractor `next_urls`; scope as a seatbelt |

- **The static-list harvest is a JS→no-JS bridge.** Because CEF renders the page
  *first*, a repeat-selector pick captures links that only exist after JS runs,
  then freezes them into a plain list hiker's no-JS frontier can fetch directly
  — no engine needed at run time. This is the link-discovery counterpart to
  authoring an API-fetch Lua plugin from observed traffic.
- **Strategies aren't exclusive.** Dynamic discovery can run alongside a plugin
  that also emits `next_urls`; the per-job control picks the *primary* source
  and surfaces only that strategy's fields (a list editor, scope patterns + a
  depth slider, or "let the extractor decide"). The picker's repeat field is the
  hint — a multi-select listing pick pre-selects static-list or plugin-driven; a
  single-article pick pre-selects dynamic discovery.
- **Same control governs both emit targets.** A crawl-job note and a direct-WARC
  run share the strategy; only where it lands differs (frontmatter vs the in-app
  run config). [crawler-link-strategy]


## Emit targets

One selection set, three outputs (`crawler-emit-targets`). All three are the
*existing* hiker seams — the crawler is a generator, not a new runtime.

### Crawl-job note

Template the picked seed URL + scope (mode/depth/follow+extract patterns from
`crawl-scope-patterns`) + chosen extractor into a `mode: crawl` capture-spec
note's frontmatter (`crawl-job-note`). Drop it into the vault's capture folder;
hiker's crawl-job form (`crawl-job-form`) runs it like any other. Best when the
site is server-rendered enough that hiker's own static fetch + a selector hint
suffices on re-run. [crawler-emit-crawl-job]

### Source-plugin extractor

Generate a `plugins.md` **source plugin** (`plugin-source-extractor`) tuned to
the site: a manifest with a URL matcher + the requested `net:<host>` grant, and
an `extract` entry point. Deterministic mode templates the picked selectors into
the skeleton (Lua for the no-toolchain path; wasm when the author has `cargo`);
the agent mode (`crawler-shared-chat`) authors a more robust extractor — ideally
the API-fetch variant (`plugin-source-api-fetch`). Installs through the normal
`plugin-install-flow` consent gate. Best when the site needs per-site logic on
every fetch. [crawler-emit-source-plugin]

- **API-fetch authoring from observed traffic.** When a site hydrates from an
  endpoint, the crawler doesn't scrape the post-JS DOM (those nodes don't exist in
  base hiker's static fetch) — it taps the network traffic it already records via
  CEF's CDP Network surface (`crawler-warc-archive`), correlates each picked field
  to the **response that produced it** (matching the picked sample against response
  bodies), and authors an API-fetch plugin (`plugin-source-api-fetch`) that calls
  that endpoint directly. The deterministic correlation narrows hundreds of
  requests to the few that matter; the agent path robustifies (fallbacks,
  pagination cursors). [crawler-api-fetch-discovery]
- **Captured traffic is redacted before authoring.** Recorded requests carry
  cookies, auth headers, and tokens; these are stripped (and token-shaped query
  params scrubbed) before any agent/LLM authoring sees the traffic, so the crawler
  never ships the user's session secrets to a model. [crawler-traffic-redaction]

### Direct WARC + manifest-import

For pages that genuinely need JS *now*, skip code generation: run the (optional)
governed crawl in-app, render each page in the engine, extract markdown via the
shared `hiker-extract` pipeline, archive the rendered page as WARC, and write a
`manifest.json` directory in the shared import format (`{ url, output_file,
html_file?, archive_file, title, links }` per page — exactly `manifest::Page`,
`extract.md` `import-format-contract`), optionally emitting the static HTML/CSS
rendition as `html_file`. Point hiker at it; `import_dir` places the children,
runs the wikilink rewrite, and versions on re-import. JS never runs in hiker.
[crawler-direct-warc]

- **Authoring mode is a per-emit choice.** Deterministic vs agent applies to the
  two code-generating targets; the direct-WARC target is always deterministic
  (it runs the extractor, it doesn't author one). [crawler-emit-mode]


### No-JS validation gate

Both no-JS emit targets — the source plugin and the crawl-job note — run in base
hiker against a *static* fetch, not the post-JS DOM the picker saw. Before handoff
the crawler proves the artifact actually reproduces the picked fields there: it
does a real no-JS fetch of the URL (or the authored API endpoint), runs the
candidate extractor/selectors against it, and diffs the result against the post-JS
picks. The outcome classifies the site and gates the emit:

| Outcome | Meaning | Emit |
| ------- | ------- | ---- |
| **Selectors survive** | server-rendered; picked nodes exist in the static HTML | crawl-job note or DOM source plugin |
| **API-callable** | hydrates from an endpoint reachable with plain headers | API-fetch source plugin (`crawler-api-fetch-discovery`) |
| **Needs JS auth** | the endpoint needs a browser-minted token / signed nonce | no no-JS artifact is viable → Lane-B import snapshot only |

This closes the gap where a preview looks right in the crawler (post-JS) but the
emitted plugin yields nothing in base hiker: a no-JS artifact ships only when the
gate confirms base hiker can reproduce it. [crawler-validation-gate]


## Preview / crawl-run

A preview tab renders the would-be output beside the live page. It calls the
*same* `hiker-extract` readability + `htmd` path hiker's ingest uses, on the
engine's rendered DOM — so the preview is the ground truth for what lands in the
vault. When the capture emits the HTML/CSS rendition kind, the preview renders it
through the shared `hiker-render` — the *same* renderer hiker core uses — so the
rendition is WYSIWYG with the vault, not just the markdown (`crawler-shared-render`).
A full in-app run reuses `hiker-extract`'s frontier loop
(`crawl-frontier-loop`) for scope/dedup/depth/robots, swapping the static
fetcher for the engine so JS pages resolve. [crawler-preview-fidelity] [crawler-crawl-run]


## WARC archive

WARC (vs the core's single-file inlined HTML, `extract-web-archive-singlefile`)
is the higher-fidelity capture format parked for the crawler in `ideas.md`
"Viewers". With CEF it records the actual wire responses (via CEF's
resource/CDP Network surface), not just a DOM re-serialization, so a JS page
can be re-served faithfully offline. The `manifest::Page.archive_file` field
already carries an arbitrary archive path, so a `.warc` flows through import
unchanged; faithful in-app *viewing* of a WARC is a later concern, not part of
this slice. [crawler-warc-archive]


## HTML/CSS rendition output

Some captures lose too much in the markdown reduction — rich tables, styled
docs, layout-bearing content. For those the crawler emits a *curated, static
HTML/CSS rendition* alongside the markdown, carried in the import format's
`html_file` (`extract.md` `import-format-contract`) and displayed in-hiker by
**hiker-render** — the no-JS HTML/CSS renderer, one of `extract.md`'s two
output kinds (`ingest-output-kind` / `htmlview-render`). The markdown is always
emitted too, as the search shadow. The crawler is the natural author: it sees
the styled live page in CEF and reduces it to static HTML/CSS the renderer can
show without scripts. Distinct from the WARC archive (the full wire capture);
this is the curated in-note rendition. [crawler-html-output]

- **Rendered preview.** Alongside the live CEF view, the crawler previews the
  captured page through hiker-render in a second pane next to the markdown
  preview, so the HTML/CSS rendition is verified against the same renderer the
  vault will display it with. [crawler-render-preview]


## Out of scope

- **Running emitted plugins / crawl jobs inside hiker-crawler beyond preview.**
  Production runs happen in hiker (crawl-job form) or the plugin host; the
  crawler previews and authors.
- **A WARC viewer in hiker.** Hiker still opens originals externally
  (`extract-open-original-external`); rendering a captured WARC faithfully is a
  separate future item.
- **Sharing UI *state* with hiker-app.** "Same look" means the shared
  `egui_workbench` chrome and identical extraction *output*, not a shared
  process, vault session, or window.
- **Headless/CI browser.** The engine is an interactive, user-launched tool; no
  headless crawl service is specced here.
