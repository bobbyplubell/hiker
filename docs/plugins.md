# Plugins

**Status: foundation built; UI layer in progress.** The `core::plugins` host — swappable WASM engine, manifest + capability permissions, hash-pinning, the fat-pipe `host_call` ABI, and `notes.query` over the structured index — is implemented and tested. The app-side VDOM renderer + sidebar wiring and a real compiled plugin are the next slice. See `status.md` `## Plugins`.

A WASM-hosted extension system for user- or agent-authored plugins. Plugins extend hiker's UI, behavior, and MCP tool surface — custom sidebar panels, status-bar items, command-palette entries, editor decorations, timer-driven workflows, event-reactive automations, and new MCP tools — without forking the codebase. Plugins run sandboxed inside a hiker-hosted runtime in one of two tiers: **native wasm plugins** compiled ahead of time from any language with a WASM target (Rust, Go via TinyGo, Zig, C/C++), and **Lua script plugins** that ship source and run inside a bundled interpreter with no compile step anywhere. See *Two authoring tiers*.


## Why a real plugin system here

`design.md`'s extractor section rejects runtime plugin loading for the *built-in* extractor registry: the binary formats (PDF, image, audio, office) need native libraries a sandbox can't host and form a small finite set, so they stay built-in and trait-based (`extract.md`). That stance holds for that case. This doc is the *open-ended* extension surface — UI widgets, custom workflows, agent-authored automations — where the surface area is unbounded and a real plugin system pulls its weight. It also owns one slice of ingestion the built-in registry deliberately doesn't: the unbounded text-transform tail (per-site web scrapers, niche text formats), exposed as **source plugins** below. The two coexist: built-in extractors for binary formats; plugins for the byte→text long tail and everything users and agents bolt on top.

Agent authorship is a load-bearing motivator. An attached coding agent (per `llm.md` / `mcp.md`) should be able to author a plugin end-to-end given the host-API docs — write the manifest, implement the hooks, and hand back either Lua source or a self-compiled `.wasm`. That requires the host API and manifest schema to be specified in agent-friendly form (examples, JSON schemas, deterministic behavior). The full loop — scaffolding tools, the no-host-compile rule, and the human consent gate — is *Agent authoring* below.


## Runtime

Embedded WASM runtime behind a swappable `WasmEngine` trait (`core::plugins::runtime`), so the engine is one isolated impl, not a codebase-wide commitment. v1 uses **`wasmi`** — a pure-Rust interpreter with no JIT — chosen over `wasmtime` deliberately: for UI/automation plugins execution speed isn't the bottleneck, and a small, auditable dependency tree fits the clean-SBOM posture `deny.toml` enforces (a JIT/Cranelift in a notes app is exactly the kind of SBOM weight that doc guards against). `wasmtime` and the Component Model (WIT-defined typed interfaces) remain the long-term swap target once the API surface stabilizes; the trait is the seam. [plugin-runtime-wasmi]

v1 ABI is a language-agnostic fat pipe over linear memory: the plugin exports `memory` + `plugin_alloc(len)->ptr` and entry points `(ptr,len)->i64` (a packed `ptr<<32 | len` of a JSON result in plugin memory), and imports `hiker.host_call(name_ptr,name_len,args_ptr,args_len)->i64`. All payloads are JSON strings, so the surface evolves without ABI churn; the Component-Model version splits these into typed functions later. [plugin-host-call]

Per-plugin isolation: each plugin instance runs in its own engine `Store`, with its own memory, no shared state. Plugins cannot directly call each other — cross-plugin coordination is through hiker's event bus (subject to permissions).

Resource limits: every plugin entry call runs under a fuel budget (`wasmi`'s `consume_fuel`), so a runaway plugin traps rather than hanging the host thread; per-plugin memory cap and host-call timeouts layer on. A plugin that exceeds limits is killed and surfaced in the plugins panel with the error. [plugin-resource-limits]

### Two authoring tiers [plugin-runtime-field]

Both tiers share everything below — manifest, capability permissions, hash-pinning, the `host_call` ABI, VDOM, and the `tools` registration (see *Plugins as MCP tool providers*). A manifest `runtime` field selects which path loads:

| `runtime` | Artifact shipped | Compile step | Audience |
| --------- | ---------------- | ------------ | -------- |
| `wasm` (default) | a prebuilt `.wasm` | author's own toolchain, ahead of time | performance / complex / third-party plugins |
| `lua` | Lua **source** | none — interpreted | the default agent-authoring path; glue + UI |

The split maps onto whether a toolchain exists in the *authoring* environment, not the user's machine. A coding agent (per `mcp.md`) has `cargo`; it compiles a `wasm`-tier plugin in its own sandbox and hands over the artifact. A user with no toolchain — or a lighter agent — authors a `lua`-tier plugin that needs no compile anywhere.

### Lua script runtime [plugin-runtime-lua]

The `lua` tier's bundled runtime is the reference C Lua interpreter compiled freestanding to `wasm32-unknown-unknown` — **no WASI**, with the ambient-I/O stdlib libs (`os`, `io`, `package`) stripped — run under the same `wasmi` engine as every other plugin. Because a Lua VM is pure computation it needs no host imports of its own: every effect still flows through the single capability-gated `host_call` pipe, so there is no parallel ambient-capability surface to police. The existing fuel budget (`plugin-resource-limits`) bounds the interpreter, so a runaway script traps like any other plugin.

The interpreter ships as a vendored `.wasm` asset produced by a C→wasm step in hiker's *build* pipeline; it adds no runtime dependency (the host just hands more bytes to `wasmi`) and is versioned with hiker, not pinned per-plugin. Reference C Lua is chosen over a pure-Rust VM (`piccolo`) for Lua-completeness; `piccolo` is the cleaner-build alternative if the C→wasm build step proves costly. An `mlua`-style native binding is rejected outright — it runs Lua in the host process, collapsing the wasm sandbox.

### Script plugin loading [plugin-script-source]

A `lua`-tier plugin ships Lua **source** + a manifest; there is no per-plugin `.wasm`. The bundled interpreter implements the standard plugin ABI (exports `memory` + `plugin_alloc`, the `init` / `on_ui_event` entry points, imports `hiker.host_call`) and delegates to script-level functions. One added host→guest export, `load_script(ptr, len)`, feeds the plugin's source into the interpreter right after instantiation; the interpreter compiles it and wires `init` / `on_ui_event` / tool handlers (see *Plugins as MCP tool providers*) to named Lua functions.

Hash-pinning (`plugin-hash-pin`) covers the **source** + manifest rather than a wasm binary; the interpreter wasm is hiker's, versioned with the app. The manifest records the targeted Lua version (`lua_version`) so a future interpreter bump can refuse a plugin written against an incompatible language version rather than mis-run it.

`require` and multi-file Lua resolve through a custom searcher backed by the plugin bundle's own files — never the filesystem, of which a sandboxed plugin has none. [plugin-script-require]

### No host-side compilation [plugin-no-host-compile]

Hiker never compiles plugin code, never bundles a compiler, and never assumes a toolchain on the user's machine. Compilation is an *authoring* activity that happens only where a toolchain already lives — the agent's own environment for the `wasm` tier — or is sidestepped entirely by the interpreted `lua` tier. This keeps the SBOM posture `deny.toml` enforces intact: a bundled C/Rust→wasm compiler is LLVM-class weight, and the only thing hiker ever ingests is wasm bytes (a plugin's prebuilt `.wasm`, or the vendored Lua interpreter) handed to the engine it already has.


## `plugins.json` and installation

Vault-level `vault/.hiker/plugins.json` is the source of truth for which plugins are loaded. Each entry pins a plugin by **two hashes** — manifest hash and wasm-binary hash — plus its location:

```json
{
  "plugins": [
    {
      "id": "word-count",
      "location": "plugins/word-count/",
      "manifest_hash": "blake3:…",
      "wasm_hash": "blake3:…",
      "enabled": true
    }
  ]
}
```

`location` is either a vault-relative path to a directory containing `manifest.json` + `plugin.wasm`, or a URL (future; v0 path-only). On load, hiker reads the manifest and wasm from `location`, computes both hashes, and aborts with a clear "plugin changed on disk" error if either fails to match the pinned values. Mismatches never silently re-run with new code or widened capabilities.

Two install paths:

- **Manual** — user hand-edits `plugins.json`, supplies location + both hashes. Useful for power users, vault sync scenarios where the entry is already known-good, and for an agent that just authored a plugin in the vault.
- **UI-triggered install** — user points hiker at a plugin location (file picker or "install from URL"); hiker reads the manifest, computes both hashes, **presents the requested permissions to the user**, and on accept writes the `plugins.json` entry with the hashes filled in automatically.

A new plugin version (different wasm or different manifest bytes) hashes differently, loads as a **distinct plugin identity**, and re-runs the permission prompt — manifests can't silently widen capabilities post-grant. The plugins settings panel offers a one-click "migrate grants from old version" path so updates are smooth without losing the consent gate.

A plugins panel in settings lists installed plugins, their granted permissions, enable/disable toggles, last error, and a "remove" action that strips the `plugins.json` entry (the on-disk files stay; only the pin is removed).


## Manifest

Each plugin ships a `manifest.json` alongside the `.wasm`:

```json
{
  "id": "word-count",
  "name": "Word Count",
  "version": "0.1.0",
  "description": "Live word/char count in the status bar.",
  "author": "…",
  "permissions": [
    "read:active-note",
    "subscribe:selection-changed",
    "ui:status-bar"
  ],
  "ui": {
    "status_bar_items": ["wc-status"]
  },
  "entry": "plugin.wasm"
}
```

Manifest fields are stable and versioned via a top-level `schema_version`. The manifest is immutable for a given plugin identity (changing it changes the hash → new identity → re-prompt).

Two further fields drive the tier and tool-provider features:

- **`runtime`** — `"wasm"` (default) or `"lua"`, selecting the tier per *Two authoring tiers* (`plugin-runtime-field`). For `"wasm"`, `entry` names the prebuilt `.wasm`; for `"lua"`, `entry` names the Lua source file and `lua_version` records the targeted language version (`plugin-script-source`).
- **`tools`** — optional array of MCP tools the plugin provides; see *Plugins as MCP tool providers* (`plugin-mcp-tools`).


## Permissions

Capability-scoped. Plugin gets ambient access to nothing — every host API call checks the corresponding permission and fails closed when ungranted. Initial permission vocabulary (illustrative, not final):

- **Vault data**
    - `read:notes` — read any note in the vault
    - `read:active-note` — read only the currently active note
    - `write:notes` — create / edit / delete notes
    - `read:trails`, `write:trails`
    - `read:metadata`, `write:metadata` (frontmatter, tags, links)
- **Events**
    - `subscribe:note-changed`
    - `subscribe:selection-changed`
    - `subscribe:trail-updated`
    - `subscribe:settings-changed`
    - `subscribe:tab-focused`
- **UI surfaces**
    - `ui:sidebar-panel`
    - `ui:status-bar`
    - `ui:command-palette`
    - `ui:editor-decoration`
    - `ui:tab-kind` — register a new tab kind (per `tab-kinds`)
    - `ui:context-menu` — add entries to filetree / editor / sidebar context menus
    - `ui:modal` — open modal dialogs (rate-limited; user can disable)
- **System-ish**
    - `timer` — register periodic / delayed callbacks
    - `net:<host-allowlist>` — outbound HTTP to specific hosts only; wildcard `net:*` is allowed but flagged loudly in the install prompt
    - `mcp:invoke` — call out to MCP tools the user has configured
    - `provide:mcp-tool` — register tools into hiker's MCP surface for external agents to call (the inverse of `mcp:invoke`); see *Plugins as MCP tool providers*
    - `settings:read`, `settings:write` — under a plugin-scoped namespace; no access to other plugins' or hiker core settings unless `settings:read:core` is requested separately
    - `clipboard:read`, `clipboard:write`
    - `fs:vault-scoped` — read/write files in a plugin-private dir under `.hiker/plugins/<id>/`; never general filesystem access

Net access is gated tightly because plugins are otherwise a perfect data exfiltration surface; users should be able to grant `read:notes` without granting `net:*` and have that combination be safe.

Grants persist in `vault/.hiker/plugin-grants.json` keyed by manifest hash. Revoke / re-prompt is one click in the plugins panel.


## Host API surface

Conceptual layers, exposed through the WASM import surface:

**Data**
- `notes.list()`, `notes.get(id)`, `notes.put(id, content)`, `notes.delete(id)`
- `notes.search(query, opts)` — semantic + lexical retrieval; wraps the existing search pipeline (`search.md`)
- `notes.query(spec)` — **structured** retrieval over frontmatter / tags / path / lifecycle: `{ where, order_by, select, limit }`, returning projected rows rather than a full-text ranking. This is the primitive the query/dataview plugin archetype lives on; `notes.list` + per-note `metadata.get` only supports a brute-force client-side scan that collapses on a large vault (N+1 host calls per query). **`notes.query` is only as capable as core's own structured index** — it wraps the structural / metadata query surface that `search.md` currently defers (`search-tag-scope`, `search-folder-scope`, `search-lifecycle-filters`) plus `design.md`'s structural index. Until that core surface exists this host call is a stub. Building it is a *core* feature, not a plugin one, and it's the real prerequisite for a dataview-class plugin ecosystem.
- `trails.list()`, `trails.get(id)`, `trails.append(id, waypoint)`
- `metadata.get(note_id)`, `metadata.set(note_id, patch)`

**Events** — plugin exports a callback the host invokes on subscribed events:
- `on_event(event_kind, payload_json)`

**UI** — see next section.

**System**
- `timer.set(delay_ms, repeating, handle)` → handle for cancellation
- `http.fetch(req_json)` → response_json (subject to `net:` allowlist)
- `mcp.invoke(tool, args_json)` → result_json
- `settings.get(key)`, `settings.set(key, value)`
- `log.info(msg)`, `log.warn(msg)`, `log.error(msg)` — surfaces in a per-plugin log view

All host calls are message-shaped (JSON in / JSON out over a small set of WASM imports) so the surface is language-agnostic and easy to evolve without breaking the ABI. A `host_call(name: string, args_json: string) -> result_json: string` "fat pipe" is the simplest v0 — the Component Model version splits these into typed functions later.


## UI extension: panels, buttons, status items, decorations

The hard part. Plugins live in WASM and can't directly touch the DOM (hiker's UI is the desktop app). Three viable patterns; **hiker should pick a declarative virtual-DOM model in v0** because it keeps the trust boundary clean and the API stable across UI tech changes.

### Declarative VDOM (recommended)

The plugin describes its UI as a serialized tree of allowed primitives. Hiker renders that tree using the native UI stack. The plugin never sees DOM, never ships JS, never gets to inject markup directly.

A small, fixed primitive set (extendable over time, but conservative):

- **Layout**: `vstack`, `hstack`, `spacer`, `divider`, `scroll`
- **Display**: `text` (with style props: bold/italic/dim/code/heading-level/color-from-palette), `icon` (named from a hiker icon set), `image` (vault-scoped paths only), `markdown` (rendered through hiker's own markdown renderer so styling/link-handling stays consistent)
- **Input**: `button`, `toggle`, `text_input`, `textarea`, `select`, `slider`, `checkbox`
- **Composite**: `list` (with row template), `tree`, `tabs`, `accordion`, `card`
- **Note-aware**: `note_link` (clickable, opens in editor), `trail_link`, `waypoint_chip`

Each primitive has bounded styling (palette-driven colors, t-shirt-size spacing, no arbitrary CSS) so plugin UIs stay consistent with hiker's look and themes apply uniformly. Plugins can request a "freeform region" via `ui:custom-canvas` permission for cases that genuinely need pixel control (graph overlays, diagrams) — that primitive renders a fixed-size canvas the plugin draws into via a draw-list API.

Event flow: each interactive primitive carries a plugin-defined string `id`. The host calls back into the plugin with `on_ui_event(panel_id, element_id, event_kind, payload)`. The plugin updates internal state and returns a fresh VDOM tree; the host diffs and re-renders. Standard React-style cycle but with WASM as the renderer-of-trees.

**Per surface:**

- **Sidebar panel** (`ui:sidebar-panel`): plugin registers one or more panels in the manifest, each with an icon and title; panel body is a VDOM tree the plugin updates on demand. Panels live in the existing sidebar tabbed area; the user can show/hide/reorder them.
- **Status bar** (`ui:status-bar`): small VDOM fragment (text, icons, maybe a button) docked in the status bar. Click events route back to the plugin.
- **Command palette** (`ui:command-palette`): plugin registers commands by id + label + optional keybinding hint; selection invokes a plugin callback. No VDOM — just declared command entries.
- **Editor decorations** (`ui:editor-decoration`): plugin subscribes to editor events and returns a list of decoration descriptors `{ from, to, kind, payload }` over the active note's text. Decoration kinds are predefined (highlight, gutter-icon, underline, inline-widget-text) — the plugin doesn't author editor decorations directly. Same primitive-clamp principle: bounded styling, predictable behavior.
- **Tab kinds** (`ui:tab-kind`): plugin can register a new tab kind (per `tab-kinds`); the tab body is a VDOM tree.
- **Context menus** (`ui:context-menu`): plugin declares menu entries against named surfaces (`filetree-file`, `filetree-folder`, `editor-selection`, `sidebar-trail`, etc.); selection invokes a callback.
- **Modals** (`ui:modal`): plugin opens a modal with a VDOM body; rate-limited; user can globally disable modals for a plugin from the plugins panel.

### Why not HTML/CSS or webviews per plugin

The VDOM is the only plugin UI model — no plugin ships HTML/CSS, and hiker embeds no HTML renderer for plugin UI. The heavier alternatives stay rejected:

- **Per-plugin webviews** (each plugin ships HTML **+ JS**, hiker renders it in a system webview) — massively wider attack surface (JS + third-party scripts to audit), a system-webview dependency (three engines across platforms), per-instance memory of N live webviews, and harder for agents to author (CSS/HTML/JS rather than markup).
- **An embedded HTML/CSS renderer** for no-JS plugin layout — rejected for the dependency weight and version-coupling it drags into the app's render stack against the `deny.toml` posture. Plugins that genuinely need CSS-grade layout (a diagram renderer, a dashboard) are a first-class core feature or a `ui:custom-canvas` draw-list, not an HTML tier.

For richer layout the VDOM primitive set extends conservatively over time, and `ui:custom-canvas` covers the pixel-control cases.

### Why not script-injection

Letting plugins inject a code bundle into hiker's own UI process is a non-starter — it collapses the sandbox entirely. Not considered.


## Example: query plugin (what a real plugin demands of the host)

A worked example — both the canonical "hello world with teeth" and the pressure-test that justifies `notes.query`. The plugin renders a sidebar panel with a query input and a live results table: "notes tagged `project` with `status: active`, newest first."

Manifest:

```json
{
  "schema_version": 1,
  "id": "notes-query",
  "name": "Notes Query",
  "version": "0.1.0",
  "description": "Live tables of notes filtered by tag, folder, and frontmatter fields.",
  "permissions": ["read:notes", "read:metadata", "ui:sidebar-panel"],
  "ui": { "sidebar_panels": [{ "id": "query", "title": "Query", "icon": "table" }] },
  "entry": "plugin.wasm"
}
```

It requests `read:notes` + `read:metadata` + `ui:sidebar-panel` and **nothing else** — no `write:*`, no `net:*`. The grant the user makes is "read my notes and draw tables," and the capability model makes that grant *mean* it: the plugin physically cannot mutate the vault or reach the network. That legible, safe grant is the whole reason the permission model exists.

Loop:

1. `init()` returns the initial VDOM — an empty query input.
2. User types → host calls `on_ui_event("query", "query-input", "input", { value })`.
3. Plugin parses the query and calls `notes.query({ where: { tag: "project", "fm.status": "active" }, order_by: "-mtime", select: ["title", "status", "mtime"] })`.
4. Plugin shapes the rows into a `list` tree (columns Note / Status / Modified, `note_link` cells) and returns it. Host paints it in egui; a `note_link` click opens the note directly.

The VDOM returned:

```json
{ "type": "vstack", "children": [
  { "type": "text_input", "props": { "id": "query-input", "placeholder": "tag:project status:active" } },
  { "type": "list", "props": { "columns": ["Note", "Status", "Modified"] }, "rows": [
    { "id": "01HRX…", "cells": [
      { "type": "note_link", "props": { "id": "01HRX…", "label": "Roadmap" } },
      { "type": "text", "props": { "value": "active" } },
      { "type": "text", "props": { "value": "2026-05-20" } } ] } ] } ] }
```

What this pins down:

- The VDOM + panel + `on_ui_event` loop is sufficient as specced — the rendering side needs nothing new.
- The permission model earns its place on the very first plugin: read-only / no-net is a grant the user can actually reason about.
- **The data API is the binding constraint.** Without `notes.query`, the plugin degrades to `notes.list()` + per-note `metadata.get` — N+1 host calls and N frontmatter parses *per keystroke*, fine at 200 notes and dead at 10k. A plugin can only be as capable as core's structured query surface (see Host API surface).
- Re-querying on input wants debounce + the async host-call model (see Open questions); a large-vault `notes.query` is exactly the call that must not block the plugin synchronously.


## Plugins as MCP tool providers

`mcp:invoke` (above) lets a plugin *call* the MCP tools the user has configured. The inverse — a plugin *providing* new tools to hiker's own MCP surface (`mcp.md`) so external agents can call them — is how the plugin system extends the agent toolbox, not just the UI.

A plugin declares tools in its manifest: [plugin-mcp-tools]

```json
"tools": [
  { "name": "rollup_status",
    "description": "Summarize note lifecycle counts by status field.",
    "input_schema": { "type": "object", "properties": { "folder": { "type": "string" } } } }
]
```

Each entry is `{ name, description, input_schema }` — `input_schema` is JSON Schema, the same shape rmcp advertises for built-in tools. At load, hiker registers the plugin's tools and dispatches a matching MCP call into the owning plugin instance's tool handler over the existing `host_call`-shaped path (name + JSON-in → JSON-out; for a `lua`-tier plugin, the named Lua function). Tools ride the **dynamic-capability** seam already used for trails/landmarks (`mcp-dynamic-capabilities`): a plugin's tools are advertised at `initialize` only while it is loaded and enabled, and drop off the surface when it's disabled or removed.

Providing tools requires the `provide:mcp-tool` permission, granted at install like any other capability and distinct from the consume-side `mcp:invoke`. A plugin that only draws panels never touches the MCP surface. [plugin-mcp-tools-permission]

**The host seam.** `mcp-server/` and `core::plugins` are decoupled crates today, sharing only the read `Store` (per `mcp.md`'s architecture). Routing a tool call into a plugin instance introduces a deliberate new edge: the MCP server holds a handle to the `PluginHost` (alongside its `IndexerHandle` / `Store` / `Vault` handles) and calls a single `dispatch_tool(plugin_id, tool_name, args_json)` entry point. The `PluginHost` owns instance lifecycle and fuel/limit enforcement; the MCP server owns wire translation and the per-tool toggle. Neither crate learns the other's internals — the edge is one method, mirroring how the indexer is reached through `IndexerHandle`. [plugin-host-mcp-seam]


## Source plugins

A **source plugin** turns a URL or raw bytes into a markdown note — a fetcher / text-transform, *not* a file-type binary extractor. It is the home for the unbounded ingestion tail `extract.md`'s built-in registry deliberately excludes: per-site web scrapers and niche text formats. The boundary is the sandbox boundary — a Lua-tier plugin is pure byte→text computation, so it can do everything PDF/OCR/audio extractors can't be (those need native libs the sandbox can't reach), and the built-in registry can stay binary-only.

Why this lands as a plugin rather than a built-in or a `CommandExtractor`:

- **Vault-portable.** The plugin is source in the vault; it rides sync and runs identically on every device, unlike a `CommandExtractor` that needs an external binary installed per machine.
- **Sandboxed + agent-authorable.** A coding agent writes one end-to-end (see *Agent authoring*); the Lua tier is auditable source and needs no compile anywhere.
- **No JavaScript engine.** A per-site scraper handles a client-rendered site by calling its backing JSON API directly (`/api/article/123`) via `net:<host>` + `http.fetch`, formatting the response to markdown — doing what the page's JS would have done, without executing it. [plugin-source-api-fetch]

### Mechanism

- **Registers as an extractor.** A source plugin declares a matcher in its manifest — a URL pattern and/or file extension — and a tool-shaped `extract` entry point. The plugin host advertises matched plugins to `extract.md`'s registry *after* the built-in extractors (built-ins win on overlap unless a `hiker.extractor` override names the plugin). [plugin-source-matcher]
- **Runs the same contract.** The host invokes the plugin with the source reference; the plugin either receives the fetched bytes or fetches them itself under its `net:` grant, and returns `{ markdown, frontmatter?, next_urls? }` — the same `Option`-returning fall-through as a built-in extractor (`extract-fallback-chain`). Emitting `next_urls` (e.g. an API pagination cursor) lets a source plugin participate in `extract.md`'s governed frontier loop exactly like a built-in extractor. Output routes into the identical sidecar / versioned-source write path; the sidecar is stamped `hiker.author: imported`. [plugin-source-extractor]
- **Permission.** Providing an extractor requires a `provide:source-extractor` capability, granted at install like any other and distinct from the UI/tool capabilities. A scraper additionally requests the specific `net:<host>` it fetches from — a legible grant ("read+write notes, fetch `example.com`, nothing else"). [plugin-source-permission]
- **Cache + version.** The plugin's manifest `version` participates in `extract.md`'s extraction cache key (`extract-version-cache-key`): extraction runs once per source-version and is cached, so the `wasmi` interpreter's lack of a JIT — slow for heavy text crunching — is paid once, not on every query. Reserve source plugins for the per-site / per-format tail, not bulk corpus reprocessing on a hot path. [plugin-source-cache-version]

### Defensive-parsing upside

Extractors parse *hostile* input — a downloaded page, a malformed document. Running that parse inside the fuel-bounded `wasmi` sandbox (`plugin-resource-limits`) is strictly safer than parsing the same bytes in-process in native Rust: a runaway or malicious input traps the plugin rather than threatening the host. For the text-transform tail, sandboxing the parser is a security gain, not just a portability one. [plugin-source-defensive-parse]

### Agent authoring loop

Authoring a per-site source plugin is the canonical agent-authoring task: when the built-in website extractor (`extract.md`) handles a site poorly, an agent writes a plugin that calls the site's API or parses its specific markup. Two MCP tools (specced in `mcp.md`) give the agent the tight iterate loop authoring needs, on top of the generic `propose_plugin` / `install_plugin` flow (*Agent authoring* below):

- **`fetch_raw`** — pull a sample page's raw HTML (+ detected data-blobs) so the agent can see the structure it writes against. [plugin-authoring-fetch-raw]
- **`extract_preview`** — run a candidate extractor (built-in or a draft plugin) against a URL/file and return `{ markdown, next_urls }` **without writing to the vault**. This is the agent's feedback loop; `extract` being a pure `bytes → markdown` function is what makes it cheap. [plugin-authoring-extract-preview]

This loop fetches untrusted web content into an agent that also holds tools — the prompt-injection danger zone. The security model defends by *where the consequences are*, not by reviewing adversarial content (nobody reads minified HTML): [plugin-authoring-security]

- **Scoped sub-task.** Plugin authoring runs in a host-enforced reduced-capability context — `fetch_raw` + `extract_preview` + propose-code, with **no vault-write and no arbitrary-net**. The restriction is part of the task wiring, not a UI mode the conversation can talk its way out of, so a hijacked agent's worst case is a bad draft. (The broader "session capability partitions" idea is parked in `ideas.md`.)
- **Sandboxed preview.** `extract_preview` runs the candidate in the `wasmi` sandbox (`plugin-resource-limits`), so the agent can run it freely — a draft can only transform bytes, never fetch/write/exfiltrate. No per-run gate is needed.
- **User-confirmed fetch reach.** `fetch_raw` targets a URL the user supplied or one-click-confirmed, not an agent-chosen one — closing exfiltration-via-URL and payload-pulling.
- **Human gate at install.** Installation is the only persistent, consequential step; it routes through `plugin-install-flow`'s permission-review dialog (`plugin-authoring-proposal-gate`), where the human approves the capability grant (`provide:source-extractor` + the specific `net:<host>`) before the plugin is pinned and enabled.


## Agent authoring

An attached coding agent (per `mcp.md`) authors a plugin end-to-end and installs it without hiker ever compiling anything. Two MCP tools, specced in `mcp.md`, drive the loop: `propose_plugin` scaffolds source + manifest into a vault-scoped scratch dir (`mcp-propose-plugin`), and `install_plugin` hands over the artifact — Lua source for the `lua` tier, or a self-compiled `.wasm` for the `wasm` tier (`mcp-install-plugin`).

`install_plugin` routes into the **existing** hash-pin + permission-review path (`plugin-install-flow`); it does not bypass it. An agent-authored plugin enters as a **proposal**, never auto-enabled: the human permission-review dialog still presents the requested capabilities for consent, and the wasm sandbox plus the capabilities the user approves bound the blast radius. This mirrors `mcp.md`'s review-required / pending-proposal model for agent note-writes — an agent-authored plugin is one more pending proposal the user approves. [plugin-authoring-proposal-gate]

Lua source is a trust advantage here: the consent dialog can show the reviewer exactly what the agent wrote, which an opaque `.wasm` blob cannot. The `lua` tier is therefore the default path for agent authorship — human-auditable, zero-toolchain, and bounded by the same capability grants as every other plugin.


## Plugin lifecycle

1. **Load** — hiker reads `plugins.json`, verifies hashes for each enabled plugin, instantiates the WASM module, calls the plugin's `init()` export. Plugin registers panels / commands / status items / event subscriptions via host calls during `init()`.
2. **Run** — plugin reacts to events (`on_event`), UI interactions (`on_ui_event`), timers (`on_timer`), and explicit invocations (command-palette commands, context-menu picks).
3. **Unload** — disable / uninstall / hiker shutdown calls the plugin's `shutdown()` export so it can release resources cleanly. Hard timeouts apply; uncooperative plugins are killed.

Hot reload during development: a `--dev-plugin <path>` flag (or a developer mode toggle in settings) bypasses the hash check for a specific plugin and reloads it on file change. Never enabled by default; surfaces a persistent warning in the UI while active.


## Distribution

Out of scope for v0. Plugins are local files. A future "plugin registry" — discovery, install-from-URL, signed publishers — is its own design problem and inherits all the supply-chain trust concerns of any untrusted package registry. The hash-pinning in `plugins.json` is the foundation any future registry will need anyway.


## What this does *not* try to be

- Not a way to bypass core design decisions (e.g. plugins can't substitute the embedder, the built-in *binary* extractor set, the index store). Source plugins (above) extend extraction along the text-transform tail only — a source-fetcher surface, not a way to replace the native PDF/OCR/audio extractors.
- Not an arbitrary code-execution surface — capability scoping is non-negotiable.
- Not a styling system — plugins use hiker's palette and primitives, not custom CSS.
- Not a way to inject arbitrary code into hiker's own UI process.
- **Not the home for rich-interaction or vault-writing *views*.** The plugin sweet spot is read-oriented, table/panel-shaped extensions — the Notes Query plugin (read + a `list` VDOM, no new primitives, no writes, fully sandboxed) is the exemplar. A feature that wants bespoke drag-and-drop, its own VDOM primitives, and to mutate notes (a kanban board whose board-doc owns columns of cards referencing notes, moved between columns; a calendar) is a *first-class core feature*, not a plugin: it would otherwise force the whole plugin-write surface and new board/card/drag primitives into the host just to serve one plugin, and it benefits from direct op-log writes and the shared vault-path drag payload. Build those in core; reserve plugins for the long tail.


## Open questions

- **Component Model vs raw imports** for v0. Raw is simpler; components give typed cross-language interfaces. Likely raw in v0, graduate when WIT tooling matures further across the target languages.
- **Inter-plugin communication.** Allowed via the event bus only, or never? Leaning never in v0 — plugins shouldn't depend on each other; if shared functionality emerges it belongs in core.
- **Async model.** Single-threaded synchronous host calls keep WASM simple but block the plugin while waiting; a callback / poll-handle model is more flexible but more API. v0 sync, v1 async.
- **Persistent plugin state.** `fs:vault-scoped` plus `settings:write` covers it, but a structured KV store per plugin might be ergonomic enough to add as a first-class primitive.
- **UI freeform-canvas API shape.** Immediate-mode draw list (à la imgui) vs retained primitives. Immediate-mode is simpler for plugin authors and agents; retained is faster to re-render. Likely immediate-mode with host-side caching.
