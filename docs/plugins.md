# Plugins

**Status: deferred / exploratory.** Not on the v1 roadmap. This doc sketches the shape so the surrounding architecture (event bus, settings, MCP, tab kinds, host API surface) can be designed with a future plugin layer in mind rather than retrofit.

A WASM-hosted extension system for user- or agent-authored plugins. Plugins extend hiker's UI and behavior — custom sidebar panels, status-bar items, command-palette entries, editor decorations, timer-driven workflows, event-reactive automations — without forking the codebase. Plugins compile from any language with a WASM target (Rust, Go via TinyGo, AssemblyScript, Zig, C/C++, etc.) and run sandboxed inside a hiker-hosted runtime.


## Why a real plugin system here

`design.md`'s extractor section explicitly rejects runtime plugin loading for extractors: source types are a small finite set, the cost isn't worth it. That stance stands for that case. This doc is the *open-ended* extension surface — UI widgets, custom workflows, agent-authored automations — where the surface area is unbounded and a real plugin system pulls its weight. The two coexist: extractors stay built-in and trait-based; plugins are for everything users and agents want to bolt on top.

Agent authorship is a load-bearing motivator. An attached coding agent (per `llm.md` / `mcp.md`) should be able to author a plugin end-to-end given the host-API docs — pick a compile target, write the manifest, implement the hooks, hand back a `.wasm` + `manifest.json`. That requires the host API and manifest schema to be specified in agent-friendly form (examples, JSON schemas, deterministic behavior).


## Runtime

Embedded WASM runtime — `wasmtime` is the leading candidate (Rust-native, well-maintained, fuel/epoch-based interruption for runaway plugins). `wasmer` is the alternative. The Component Model (WIT-defined interfaces) is the right long-term target since it gives strongly-typed host APIs across languages; the v0 sketch can use raw imports/exports and graduate to components once the API surface stabilizes.

Per-plugin isolation: each plugin instance runs in its own `Store`, with its own memory, no shared state. Plugins cannot directly call each other — cross-plugin coordination is through hiker's event bus (subject to permissions).

Resource limits: per-plugin memory cap, CPU fuel budget per host call, hard timeout on host calls. A plugin that exceeds limits is killed and surfaced in the plugins panel with the error.


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
    - `settings:read`, `settings:write` — under a plugin-scoped namespace; no access to other plugins' or hiker core settings unless `settings:read:core` is requested separately
    - `clipboard:read`, `clipboard:write`
    - `fs:vault-scoped` — read/write files in a plugin-private dir under `.hiker/plugins/<id>/`; never general filesystem access

Net access is gated tightly because plugins are otherwise a perfect data exfiltration surface; users should be able to grant `read:notes` without granting `net:*` and have that combination be safe.

Grants persist in `vault/.hiker/plugin-grants.json` keyed by manifest hash. Revoke / re-prompt is one click in the plugins panel.


## Host API surface

Conceptual layers, exposed through the WASM import surface:

**Data**
- `notes.list()`, `notes.get(id)`, `notes.put(id, content)`, `notes.delete(id)`
- `notes.search(query, opts)` — wraps the existing search pipeline (`search.md`)
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

The hard part. Plugins live in WASM and can't directly touch the DOM (hiker's UI is the Tauri webview running React/CodeMirror). Three viable patterns; **hiker should pick a declarative virtual-DOM model in v0** because it keeps the trust boundary clean and the API stable across UI tech changes.

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
- **Editor decorations** (`ui:editor-decoration`): plugin subscribes to editor events and returns a list of decoration descriptors `{ from, to, kind, payload }` over the active note's text. Decoration kinds are predefined (highlight, gutter-icon, underline, inline-widget-text) — the plugin doesn't author CM6 decorations directly. Same primitive-clamp principle: bounded styling, predictable behavior.
- **Tab kinds** (`ui:tab-kind`): plugin can register a new tab kind (per `tab-kinds`); the tab body is a VDOM tree.
- **Context menus** (`ui:context-menu`): plugin declares menu entries against named surfaces (`filetree-file`, `filetree-folder`, `editor-selection`, `sidebar-trail`, etc.); selection invokes a callback.
- **Modals** (`ui:modal`): plugin opens a modal with a VDOM body; rate-limited; user can globally disable modals for a plugin from the plugins panel.

### Why not webviews / iframes per plugin

It's the obvious alternative: each plugin ships HTML/JS, hiker renders it in an isolated webview. Rejected for v0:

- Massively wider attack surface — DOM, JS, third-party scripts to audit.
- Theming breaks — every plugin re-invents typography, colors, spacing.
- Performance cost — N webviews open at once.
- Harder for agents to author correctly — they'd need to write CSS / HTML / JS in addition to plugin logic.

The webview path can be added later for plugins that *really* need it (e.g. a Mermaid diagram editor) behind a separate `ui:webview` permission that's flagged loudly at install time. The VDOM path covers the 90% case.

### Why not script-injection

Letting plugins ship a JS bundle that runs in hiker's main webview is a non-starter — it collapses the sandbox entirely. Not considered.


## Plugin lifecycle

1. **Load** — hiker reads `plugins.json`, verifies hashes for each enabled plugin, instantiates the WASM module, calls the plugin's `init()` export. Plugin registers panels / commands / status items / event subscriptions via host calls during `init()`.
2. **Run** — plugin reacts to events (`on_event`), UI interactions (`on_ui_event`), timers (`on_timer`), and explicit invocations (command-palette commands, context-menu picks).
3. **Unload** — disable / uninstall / hiker shutdown calls the plugin's `shutdown()` export so it can release resources cleanly. Hard timeouts apply; uncooperative plugins are killed.

Hot reload during development: a `--dev-plugin <path>` flag (or a developer mode toggle in settings) bypasses the hash check for a specific plugin and reloads it on file change. Never enabled by default; surfaces a persistent warning in the UI while active.


## Distribution

Out of scope for v0. Plugins are local files. A future "plugin registry" — discovery, install-from-URL, signed publishers — is its own design problem and inherits all the supply-chain concerns called out in [[feedback_npm_isolation]]. The hash-pinning in `plugins.json` is the foundation any future registry will need anyway.


## What this does *not* try to be

- Not a way to bypass core design decisions (e.g. plugins can't substitute the embedder, the extractor set, the index store).
- Not an arbitrary code-execution surface — capability scoping is non-negotiable.
- Not a styling system — plugins use hiker's palette and primitives, not custom CSS.
- Not a way to ship JS into hiker's main webview.


## Open questions

- **Component Model vs raw imports** for v0. Raw is simpler; components give typed cross-language interfaces. Likely raw in v0, graduate when WIT tooling matures further across the target languages.
- **Inter-plugin communication.** Allowed via the event bus only, or never? Leaning never in v0 — plugins shouldn't depend on each other; if shared functionality emerges it belongs in core.
- **Async model.** Single-threaded synchronous host calls keep WASM simple but block the plugin while waiting; a callback / poll-handle model is more flexible but more API. v0 sync, v1 async.
- **Persistent plugin state.** `fs:vault-scoped` plus `settings:write` covers it, but a structured KV store per plugin might be ergonomic enough to add as a first-class primitive.
- **UI freeform-canvas API shape.** Immediate-mode draw list (à la imgui) vs retained primitives. Immediate-mode is simpler for plugin authors and agents; retained is faster to re-render. Likely immediate-mode with host-side caching.
