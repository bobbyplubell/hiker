# Settings

Configuration surface for Hiker. v1 ships a TOML loader and the section content needed to unblock the deferred toggles in `editor.md`, `txt-ingest.md`, and `index.md`. No settings UI in v1 — the file is the surface, and a relaunch picks up changes.

The headline decisions:

- **Two TOML files.** Per-user `config.toml` at the platform config dir, per-vault `vault/.hiker/config.toml`. The vault file overrides the user file key-by-key. [settings-user-config-toml, settings-vault-config-toml]
- **Read once at startup.** No watcher, no hot-reload, no per-access reread. Restart to apply changes. The settings struct is built once and handed around frozen. [settings-load-once-at-startup]
- **Strict load.** Any unknown key or type mismatch aborts startup with a clear `file:line` error. Same fail-loud discipline as `store-version-fail-loud` in `index.md` — silently dropping a user-set value is a worse failure mode than refusing to start. [settings-strict-load]
- **Defaults live in Rust, files auto-create on first load.** Every field is `serde(default)`-decorated; the loader treats a missing file as empty *and* writes a full defaults-populated TOML to the expected location so users have a self-documenting file to discover and edit. [settings-defaults-in-code, settings-auto-create-defaults]
- **In-app toggles write back.** The toggles that already exist in the UI (View menu, tree sort, sidebar/related panel state, trash expansion) persist their flips to the vault TOML so they survive restart. No generalized settings panel in v1 — the TOML is still the canonical surface for everything else. [settings-write-back]


## Storage location

**Per-user** at the platform config dir (use the `directories` crate, no hand-rolled paths): [settings-user-config-toml]

- Linux: `~/.config/hiker/config.toml`
- macOS: `~/Library/Application Support/hiker/config.toml`
- Windows: `%APPDATA%\hiker\config.toml`

**Per-vault** at `vault/.hiker/config.toml`. Lives next to `index.db`, `logs/`, `trash/` — same `.hiker/` parent the watcher already ignores. Travels with the vault under Syncthing/git. [settings-vault-config-toml]

Either file may be absent; both absent is the same as both empty.


## Merge & precedence

Conceptually a deep merge of two trees: vault values override user values key-by-key. The user file is the wide default ("my preferences for any vault I open"); the vault file is the local override ("for *this* vault, also do X").

- Maps merge recursively. `[editor]` from the user file plus `[editor]` from the vault file produces a single `editor` table where the vault's keys win on overlap.
- Arrays replace, not concatenate. If the user defines `indexing.ignored_paths = ["foo/"]` and the vault defines `indexing.ignored_paths = ["bar/"]`, the effective value is `["bar/"]`. Concatenation reads as merging but produces surprises (a user can't *remove* an inherited entry). Replace is honest.
- The schema_version comes from whichever file declares it; if both declare and they disagree, vault wins (same key-overlap rule). The mismatch case below covers what happens when a declared version doesn't match the binary's expected version.

There is no per-key tagging of "this setting is user-only" or "this is vault-only." Any key may appear in either file. The conventions section below names which keys make sense where, but the loader doesn't enforce it — putting `vault.recent_vaults` in a vault TOML works, it's just useless.


## Schema version & strict loading

Top-level `schema_version` integer (default `1`). Mismatch with the binary's expected version is a hard startup failure with a message naming both versions. No silent migration. Same posture as `store-version-fail-loud`. [settings-schema-version]

**Strict load posture.** Unknown keys and type mismatches both abort startup. The error names the file path, the offending key, the line/column from the TOML parser, and (for unknown keys) a suggestion if there's a near match. This is stricter than the typical "warn and continue" pattern; rationale:

- A typo'd key that silently does nothing is the worst possible UX — the user thinks they configured something and they didn't. Failing loud surfaces the problem at the moment they made it, not three weeks later when they wonder why the setting "isn't working."
- The downgrade case (running an older binary against a TOML written by a newer binary that knows new keys) is a real concern — migration policy is to bump `schema_version` whenever keys are added or renamed, so the version check fires before the unknown-key check, and the user gets the *right* error ("schema 2, expected 1") instead of a misleading one ("unknown key `editor.foo`").
- Migration: same policy as `index.md`'s pre-real-use clause. Until users start putting their actual notes in this app, schema bumps are handled by deleting the offending TOML. Once real-data use begins, every bump ships an additive migration that reads the old shape and writes the new one in place.

`tracing::error!` events use the `obs-error-context` field discipline: `error!(file = %path.display(), line, col, key = %k, "unknown setting key")`. The user-visible error is a one-line summary plus a "see hiker.log for details" hint.

[settings-strict-load]


## Defaults & auto-create

Every settable field is declared in Rust with `#[serde(default)]` (and `#[serde(deny_unknown_fields)]` per the strict-load rule). The default for each field lives in a single `Default` impl on its containing struct. [settings-defaults-in-code]

**Auto-create.** On `Config::load`, if either expected file is missing the loader writes a fresh TOML at that path containing the current defaults serialized in full, plus a short header comment naming the binary version. The file is then read back through the normal parse path. Two reasons for writing the *full* defaults rather than an empty file:

- **Discoverability.** Users find the file, open it, and see every available key with its current value and (where useful) an inline comment. Documentation that travels with the binary, never out of sync.
- **No template-drift problem.** The file isn't a template shipped with the binary — it's generated by the running binary, so it always reflects the current schema. When the binary version changes and adds a key, that key is missing from existing files; `serde(default)` fills it in transparently. Users who want to see the new defaults can delete the file and it regenerates.

Auto-create runs at most once per file per process. If the file is created and then deleted mid-session, the next load (next launch / vault swap) re-creates it. Concurrent first-launches against the same vault from two processes can race, but the loser harmlessly overwrites with identical content; not worth a lock. [settings-auto-create-defaults]

The user TOML is auto-created on the first launch ever; the vault TOML is auto-created on the first open of each vault that doesn't have one. Both auto-creates use atomic write-then-rename so a crash mid-write can't leave a half-file.


## Sections

Each section gets a fuller spec when its UI consumers are built; the schema below is what v1 actually loads.

### [editor] [settings-section-editor]

Per-vault toggles for the editor pane and View menu. All optional; each has an in-code default. Loaded once at startup; in-session flips from the View menu update both the live state *and* the vault TOML via `settings-write-back`, so a relaunch finds the same state.

| Key | Type | Default | Notes |
| --- | ---- | ------- | ----- |
| `render_txt_as_markdown` | bool | `true` | Backs `txt-render-as-markdown-default` in `txt-ingest.md`. The view-menu entry `view-render-txt-as-markdown-toggle` becomes the in-session override and ungreys with this loader |
| `live_preview` | bool | `true` | Initial state of `view-live-preview-toggle`; matches `live-preview-default-on` |
| `word_wrap` | bool | `true` | Initial state of `view-word-wrap-toggle`; ungreys this entry |
| `show_line_numbers` | bool | `true` | Initial state of `view-line-numbers-toggle` |
| `show_whitespace` | bool | `false` | Initial state of `view-show-whitespace-toggle` |
| `show_chunk_boundaries` | bool | `false` | Initial state of `view-show-chunk-boundaries`; debugging-grade view, off by default |
| `tab_size` | u8 | `2` | CM6 `EditorState.tabSize` |

Theme, font family/size, and autosave-on-idle are intentionally excluded from v1 — they need a UI to be discoverable and the TOML-only surface isn't the right home for them. They're listed in "Deferred" below.

### [indexing] [settings-section-indexing]

Indexer tunables. Vault-level only is the natural scope (different vaults have different content shapes), but a user-level default is fine for the common case.

| Key | Type | Default | Notes |
| --- | ---- | ------- | ----- |
| `model` | string | `"bge-small-en-v1.5"` | Embedder model id; bumping forces re-embed via `embedder-version-tag`. Other values aren't supported in v1 — declared so the field exists, but anything other than the default fails the strict load until a second model lands |
| `batch_size` | u16 | `64` | Embed batch size; backs the partial `embedder-batch-64` |
| `ignored_paths` | array of strings | `[]` | Additional ignore patterns on top of the hard-coded list in `watcher-ignore-hardcoded`. gitignore-style globs, evaluated against vault-relative paths. Replaces, doesn't concatenate, per the array-merge rule above |

The destructive **Reindex (rebuild)** verb (`reindex-rebuild-action`) lives here in the *eventual* settings UI per `editor.md`; the verb itself stays planned in `status.md` until the UI shell lands. The CLI counterpart `cli-reindex-rebuild` continues to cover the operational case.

### [vault] [settings-section-vault]

Per-user vault management plus per-vault UI startup state.

| Key | Type | Default | Notes |
| --- | ---- | ------- | ----- |
| `recent` | array of strings | `[]` | User-only in practice. Most-recently-opened vault paths, freshest first. Written by the Open Vault flow via `settings-write-back` (user-scope) |
| `default` | string | `null` | User-only. Path to a vault the frontend auto-opens on startup; consumed by `settings-default-vault-autoopen`. Empty/absent = show the vault picker. Set by an explicit "make this the default vault" UI action when one lands; until then, hand-edit the user TOML |
| `sidebar_open` | bool | `true` | Vault-level. Initial open/collapsed state of the file tree (`panel-toggle-buttons`) |
| `related_open` | bool | `false` | Vault-level. Initial open/collapsed state of the related-notes panel (`panel-toggle-buttons`) |
| `trash_expanded` | bool | `false` | Vault-level. Initial expanded/collapsed state of the trash row (`tree-trash-bin`) |
| `tree.sort_by` | string | `"name_asc"` | Vault-level. Initial tree sort order (`tree-sort-options`); one of `name_asc` / `name_desc` / `mtime_desc` / `mtime_asc`. Strict-load rejects any other value |

`recent` and `default` are documented as user-only, but the loader doesn't enforce it — see "Merge & precedence" above.

#### Default vault auto-open [settings-default-vault-autoopen]

When `vault.default` is set, the frontend bootstrap auto-opens that path before showing the JS folder dialog. The frontend reads the value via `get_default_vault()` (a small read-only Tauri command) and, if non-empty, calls `open_vault_at(path)` directly. No backend dialog spawning, no "try-and-pick" command — the orchestration lives entirely in the frontend per the rule above.

Failure modes:

- **`vault.default` is unset / the user TOML doesn't exist yet.** `get_default_vault` returns `Ok(None)`; the bootstrap falls through to the JS dialog. No log noise — this is the empty-state case.
- **Configured path no longer exists** (deleted, unmounted drive, typo). `open_vault_at` returns `HikerError::NotFound` with the path; the frontend surfaces a non-fatal toast (`"Default vault at <path> not found — pick a vault"`) and falls through to the JS dialog. **Do not auto-clear the setting** — a user with an unplugged USB drive should find the same `vault.default` waiting after they reattach it. The setting is the user's stated intent; the absent path is a transient circumstance.
- **Path exists but `Vault::open` fails** (permissions, schema mismatch, etc.). Same alert dialog as the manual-open error path — surfaces the real reason rather than masking it as "no default."

Setting `vault.default` from inside the app is deferred until a "make this the default vault" UI action lands; until then, hand-edit the user TOML.

### [keymap]

Stub. v1 does not load keybind overrides; `settings-section-keymap` stays planned. The `keybind-registry` in `editor.md` is shaped to accept overrides — the loader is the missing piece, deferred until a user actually wants to remap something. When it lands the format is `keymap.<binding-id> = "<chord>"`.

### [llm] (deferred — lands with v3.5)

Stub. The full schema lives in `llm.md` (`llm-providers-config`); summarized here so the section list in this doc is complete. Shape: `provider`, `model`, `api_key_env`, `base_url`, `[llm.limits]` for `max_tokens` / `timeout_secs`, `[llm.audit] log_full_prompt`, plus per-feature toggles for background/fan-out features (default off). Loader and validator land alongside the v3.5 milestone; until then the section is unrecognized and `settings-strict-load` will refuse it.

### [embedder] (deferred — lands when cloud/Ollama embedder option ships)

Stub. The full schema lives in `index.md`'s embedder section (`embedder-config-section`); same shape as `[llm]` — `provider`, `model`, `api_key_env`, `base_url`. Default `provider = "fastembed"` matches today's behavior so existing vaults don't change. Until the schema lands, the section is unrecognized and `settings-strict-load` will refuse it.

### [acp] (deferred — lands when `core::acp` ships)

Stub. The full schema lives in `llm.md` (`llm-acp-client-optional`); enables routing the chat panel through an external ACP agent instead of the basic agent loop. Shape: `agent` (registry id, "bundled" alias for the basic loop, or "none" for disable mode). Loader lands with `core::acp` itself.


## Loading lifecycle

Single `Config::load(vault_root: &Path) -> Result<Config>` in `core::config` is the only entry point. Order:

1. Read user config (best-effort: missing file → empty TOML).
2. Read vault config (best-effort: missing file → empty TOML).
3. Parse each into the same `Config` struct independently. Either parse failure aborts (with the offending file named).
4. Deep-merge user under vault per the rules above.
5. Validate cross-field invariants (e.g. `tree.sort_by` is one of the known values; `model` is the supported value). Failures abort.
6. Return the frozen `Config`.

The Tauri layer calls this once inside `open_vault_at` (alongside `init_tracing` per `obs-tracing-baseline`) and stashes the result in `tauri::State<Arc<Config>>`. CLI and MCP entry points call the same `open_vault_at` helper. No mutation, no `RwLock`. [settings-load-once-at-startup]

Open-a-different-vault re-runs `Config::load` against the new vault root. The old `Config` is dropped; in-memory UI state (which view toggles are flipped, which panels are open) does *not* automatically reset to the new defaults — the existing vault-swap reset path in `ui/src/main.ts` handles what should re-init.


## Write-back

Selected in-app UI changes persist by writing back to the appropriate TOML file. The set of write-back-eligible keys is fixed (it's exactly the set with a real-time UI control); arbitrary keys are not user-mutable from inside the app in v1. Eligible keys:

- `[editor].render_txt_as_markdown`, `live_preview`, `word_wrap`, `show_line_numbers`, `show_whitespace`, `show_chunk_boundaries` — written on View menu flip. Vault-scope.
- `[vault].sidebar_open`, `related_open`, `trash_expanded`, `tree.sort_by` — written on the corresponding UI action. Vault-scope.
- `[vault].recent` — written by the Open Vault flow (push-to-front, dedupe, cap at ~10 entries). User-scope.

Single Tauri command `set_setting(scope: SettingsScope, key: String, value: serde_json::Value) -> Result<()>` is the only write path, where `scope` is `User` or `Vault`. Each call:

1. Validates the key is in the eligible set for the requested scope (rejects everything else with `not user-mutable in v1`).
2. Validates the value's type against the field's declared type (a `bool` won't accept `"true"`).
3. Loads the target file via `toml_edit` (preserves comments and key ordering); applies the change in place; atomic write-then-rename.
4. Updates the in-memory `Arc<Config>` so subsequent reads see the new value without a re-load.

**`toml_edit`, not `toml::to_string`.** Round-tripping through `toml::to_string` would discard every comment, custom key order, and any key the loader doesn't know about. `toml_edit` operates on the parsed document tree and patches in place. Worth the dependency: a user who opens their TOML, adds comments, and then flips a toggle in the UI should not lose their comments.

**Watcher coordination.** Writes go to paths under `.hiker/` (vault TOML) or under the platform config dir (user TOML). The vault TOML path is already covered by `watcher-ignore-hardcoded`'s `.hiker/` rule, so write-back never re-enters as a watcher event. The user TOML is outside the vault entirely. No new suppression infrastructure needed.

**External edits while running.** If the user hand-edits a TOML file while the app is open, the in-memory `Config` keeps the old values until the next `set_setting` call (no proactive hot-reload, per `settings-load-once-at-startup`). A subsequent `set_setting` writes through `toml_edit` so the user's manual edits are preserved; only the changed key is overwritten. The "external edit + in-app flip happen on different keys" case works correctly. Last-writer-wins on the same key is acceptable for v1 — concurrent vim + UI edits to the same setting is not a workflow worth designing for.

`set_setting` reloads the full merged Config after writing, so unrelated hand-edits to other keys land in memory at that point as a side effect. This is intentional — it keeps the in-memory and on-disk views from diverging on keys neither the spec nor the user expected to interact — but is *not* a hot-reload guarantee. A user who only hand-edits and never flips an in-app toggle does not see their changes applied until restart.

[settings-write-back]


## Default vault auto-open

On app startup, if `vault.default` in the user TOML is set and non-empty, the frontend opens that path directly without showing the vault picker. Empty or absent → show the picker as today. [settings-default-vault-autoopen]

**The picker is a frontend concern.** It's UI; it shouldn't live in Rust. A CLI invocation should never be at risk of spawning a folder dialog. The frontend calls `@tauri-apps/plugin-dialog` from JS when (and only when) it needs the user to pick a folder. The backend exposes one command, `open_vault_at(path)`, that does the actual open work — `Vault::open` + `init_tracing` + `Config::load` + indexer/watcher spin-up + `vault.recent` push. That command is the same shared helper the CLI / MCP entry points call, with no dialog dependency anywhere in core or in the Tauri command layer.

**Frontend bootstrap.** On window init, the frontend:

1. Reads `vault.default` from the user TOML (via a small `get_default_vault()` Tauri command — or whatever read surface exists once it's wired; the point is it's a value lookup, not a side-effecting "try to open" command).
2. If non-empty, calls `open_vault_at(path)`. On `HikerError::NotFound` (or equivalent — the configured path no longer resolves), logs a `warn!` server-side, surfaces a non-fatal toast on the client (`"Default vault at <path> not found — pick a vault"`), and falls through to step 3. The configured default is **not** auto-cleared (the drive may simply be unplugged — clobbering the setting on a transient failure is the wrong default).
3. If `vault.default` was empty, or step 2 fell through, opens the JS dialog plugin to let the user pick a folder, then calls `open_vault_at` with the chosen path.

**Today's `pick_vault` command goes away.** Its two responsibilities split: the dialog moves to JS, the open work becomes `open_vault_at`. Existing call sites in `ui/src/main.ts` (the "Open vault" button handler) become "JS dialog → `open_vault_at`."

**No first-run interaction.** On a brand-new install, `vault.default` is `null` (the user TOML is auto-created with defaults), so the bootstrap falls through to the picker on first launch. The "make this the default vault" UI action that *sets* this value is deferred (see "Deferred"); until it lands, users hand-edit the user TOML to opt in.

**Reading `vault.default`.** Per "Merge & precedence", `vault.default` is documented as user-only but the loader doesn't enforce it. The bootstrap read targets the user TOML directly (it's the only file available before a vault is open — there's no vault to merge against yet). A `vault.default` set inside a vault TOML is silently meaningless, same as today.


## Module placement

- `core::config` — `Config` struct, all section structs, `Config::load`, `Default` impls. Pure, no Tauri imports. Mirrors the `core::store` / `core::embed` discipline from `index.md`.
- TS types auto-exported via `ts-rs` per design.md so the frontend reads `Config` shape directly without manual duplication.
- No other module reads `*.toml` directly. If a value is needed somewhere, the path goes `Config::load` → struct field → caller; not "open the file again over here."


## Deferred

Real, considered, explicitly not v1.

- **Settings UI shell.** A modal or sidebar pane that surfaces editable fields against the `Config` struct. The TOML is canonical; the panel just calls `set_setting` against the same write-back path the View menu already uses. Defer until a user-facing reason exists (the embedder model picker is the likely first ask).
- **Hot-reload.** Watch the two TOML files and re-apply on change. Not needed in v1 — `set_setting` keeps in-memory and on-disk state in sync for in-app flips, and external hand-edits while running are rare enough that "restart to apply" is acceptable.
- **Expanding the write-back-eligible key set.** v1 hard-codes which keys are user-mutable from inside the app. New eligible keys are added one-at-a-time as new UI controls appear (e.g. theme picker, model picker). Generalized "any key, anywhere" write-back isn't planned — it would require either UI for every key or a generic settings panel.
- **Per-key user-vs-vault scope enforcement on read.** Today the loader accepts any key in either file. If real misuse appears (users putting `vault.recent` in vault TOML and confusing themselves), add a per-field scope tag and reject misplaced keys at parse time. Write-back already routes per scope, so the read side is the only gap.
- **Auto-rewrite of existing TOML files when keys are added.** When a future binary adds a key, existing TOMLs won't have it (`serde(default)` fills in transparently). Auto-rewriting to inject the new key with a comment would keep files self-documenting at the cost of touching user files on every launch — defer until users actually ask for it; "delete the file to regenerate" is the workaround.
- **Theme / font / autosave.** Need a UI to be discoverable; not worth a TOML-only surface. Land alongside the settings UI shell.
- **Sync of settings across machines.** Per-vault settings travel with the vault under Syncthing. The per-user TOML doesn't sync and shouldn't (recent vaults, default vault, machine-specific paths).
- **Backward-compatibility shims for old TOML shapes.** Pre-real-use we delete and re-create; post-real-use we ship per-version migrations. No "load old shape into new struct" code in between.
- **`hiker config get/set` CLI.** Convenient but not required — `vim ~/.config/hiker/config.toml` plus the in-app write-back covers the v1 need.
- **"Did you mean X?" hints on unknown keys.** The strict-load posture above mentions this; v1 doesn't implement it. The hard error already names the bad key plus the file, which is enough for the user to grep the spec. A near-match suggester (Levenshtein or similar over the known field names) is a polish-grade addition that can land when someone hits the case in real use.


## Out of scope

- A web-based settings UI or remote config (Hiker is local-first by design).
- Per-note settings beyond what frontmatter already supports.
- Settings inheritance across multiple vaults (each vault's settings are independent of any other vault's settings).
- Encrypted settings (no secrets live in `config.toml` today; if cloud embedder API keys land later, they get their own keychain-backed storage, not a TOML field).
