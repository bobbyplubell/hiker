# Settings

User-facing settings for Hiker. Stub for now — design fills in as concrete settings land. Not built in v0/v1; first real surface arrives alongside the v1 indexer (the embedder model download progress is the first thing that genuinely needs a user-visible toggle).

This doc covers *what* settings exist and *where* they live on disk, not the UI shell that exposes them. The settings menu (a modal or a sidebar pane) is its own UI task to be specced when we build it.


## Storage location

Per-user config file at the platform's standard config dir: [settings-user-config-toml]

- Linux: `~/.config/hiker/config.toml`
- macOS: `~/Library/Application Support/hiker/config.toml`
- Windows: `%APPDATA%\hiker\config.toml`

Use the `directories` crate; do not roll path logic by hand. Format is TOML — human-readable, comment-friendly, common in Rust tooling.

Per-vault config lives separately at `vault/.hiker/config.toml` and overrides per-user defaults for that vault. Some settings are per-user only (model location), some per-vault only (chunk size for that vault's content), some both with vault winning. [settings-vault-config-toml]


## Sections (planned, not yet built)

Each section gets a fuller spec when implemented. One-line stubs here so future work knows where it goes.

- **Indexing** — embedder model selection, model download trigger / progress, batch size, ignored-paths additions on top of watcher.md's hard-coded list. The settings page is also the home for the destructive **Reindex (rebuild)** verb — drops and recreates the schema, then full reindex; UI counterpart to `cli-reindex-rebuild` and intentionally kept off the tree toolbar's `…` menu so an accidental click can't trigger a full re-embed from scratch. The day-to-day reindex verbs (`reindex-all-action`, `reindex-current-file-action`) live in the tree toolbar per `editor.md`. See `index.md` for the underlying mechanics. [settings-section-indexing, reindex-rebuild-action]
- **Keymap** — overrides for the keybind registry (editor.md). Maps `binding.id` → key chord. Loaded from `vault/.hiker/keybinds.toml` per editor.md's deferred override mechanism; user-global overrides live in the user config. [settings-section-keymap]
- **Editor** — tab size, line wrapping, theme (light/dark/system), font family/size, autosave on idle (deferred per editor.md). [settings-section-editor]
- **Vault** — recently-opened vaults list, default vault on startup. [settings-section-vault]
- **Sync / backup** — informational only at first (Hiker doesn't do sync; design.md:441 defers to Syncthing). Settings page links out to docs.
- **Telemetry / privacy** — opt-in toggles for any external calls (cloud embedders if/when added, scraping). Default off.


## Migration

Bump a `schema_version` field at the top of `config.toml`. On version mismatch, load with defaults for unknown keys, log a warning for removed keys, never silently lose user-set values. Migration code is per-version, additive. [settings-schema-version]


## Out of scope

- Sync of settings across machines (Syncthing handles it if the user wants it).
- A web-based settings UI (Hiker is local-first; no remote config server).
- Per-note settings beyond what frontmatter already supports.
