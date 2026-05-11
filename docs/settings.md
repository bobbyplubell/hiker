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

Theme and font family/size are intentionally excluded from v1 — they need a UI to be discoverable and the TOML-only surface isn't the right home for them. They're listed in "Deferred" below. Crash-recovery autosave (`autosave.md`) is on with a fixed 5s tick and has no config surface in v1; an `[autosave]` section can land later if a workflow asks.

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

### [llm]

The full schema lives in `llm.md` (`llm-providers-config`); summarized here so the section list in this doc is complete. Shape: top-level `enabled` (gates `llm-features-disable-entirely`), `[llm.provider]` (backend / model / api_key_env / base_url), `[llm.limits]` (max_tokens / timeout_secs), `[llm.agent]` (iteration_cap / tool_timeout_secs per `agent-iteration-cap-prompt` and `agent-tool-call-timeout`), `[llm.audit]` (log_full_prompt — obs-no-content discipline). Loader, strict validation, and `core::llm` client construction are wired today; per-feature toggles for background/fan-out features land with those features themselves.

### [embedder] (deferred — lands when cloud/Ollama embedder option ships)

Stub. The full schema lives in `index.md`'s embedder section (`embedder-config-section`); same shape as `[llm]` — `provider`, `model`, `api_key_env`, `base_url`. Default `provider = "fastembed"` matches today's behavior so existing vaults don't change. Until the schema lands, the section is unrecognized and `settings-strict-load` will refuse it.

### [acp] (deferred — lands when `core::acp` ships)

Stub. The full schema lives in `llm.md` (`llm-acp-client-optional`); enables routing the chat panel through an external ACP agent instead of the basic agent loop. Shape: `agent` (registry id, "bundled" alias for the basic loop, or "none" for disable mode). Loader lands with `core::acp` itself.


## Staging review

Some writes shouldn't land directly: the user didn't author the bytes (agent writes), or the user can't watch them all happen at once (batch mutations). Hiker stages these in `vault/.hiker/staging/` and exposes accept/reject inline on every surface that cares — the chat card where the proposal originated, the trails panel for draft-trail proposals, the file tree for per-file proposals, the editor toolbar when the open file has a pending proposal, and the activity detail page as the central review surface. No dedicated staging editor sub-mode — accept/reject lives alongside the content it affects.

Two flows feed staging:

- **Agent writes (opt-in)** — MCP tool calls and background features are gated by `review_required` flags; flipping them on routes the writes to staging instead of applying directly. [agent-write-review-mode]
- **Batch mutations (always staged)** — multi-note mutation actions (e.g., "reformat every `.txt` in `inbox/`") fan out N tasks per `task-queue.md`; the user can't watch N buffers, so each result lands in staging unconditionally. Single-note user-initiated mutations stay in-buffer per `editor.md`'s Note-mutations menu — staging is for the multi-note case.

The headline decisions:

- **Agent-write review is off by default; opt-in per write surface.** Existing behavior — agent writes apply directly + append a `core::changes` row — stays the default. Users who want a checkpoint in the loop flip the flag per surface (MCP / background features). [agent-write-review-mode]
- **Proposed writes land in `vault/.hiker/staging/`** — a `pending.json` index file plus one `<id>.md` per proposal (the proposed content). Source on disk is unchanged until Accept. Watcher's `.hiker/` ignore covers the dir for free. [staging-dir]
- **Accept/reject is integrated into every relevant surface, not a separate editor mode.** There is no `staging-preview-mode` editor sub-mode. Instead, each surface renders its own accept/reject inline: chat tool-call cards, the trails panel, the file tree context menu, the editor toolbar, and the activity detail page as the central review surface. Clicking a proposal opens it as a read-only buffer with the existing diff toggle (reuses `snapshot-preview-mode`'s pattern) — but accept/reject stays on the owning surface's row, not in the editor toolbar.
- **The activity detail page is the central review surface.** The existing `vault-home-recent-activity-detail` page gains a "Pending" filter pill (alongside the existing author-class pills) that shows all pending proposals across all sources. Each row carries [Accept] [Reject]. An [Accept all (N)] button at the top batch-approves. [staging-review-activity-detail-filter]
- **Proposals are queryable by surface-specific filters.** `core::staging::list()` accepts an optional filter (`by_path`, `by_trail_id`, `by_surface`, `by_session_id`) so each surface pulls only its relevant proposals. [staging-review-filtering]
- **MCP write responses are honest about staging.** When review mode is on, MCP write tools return success-with-pending — the JSON response carries `status: "staged"` plus the staging id so the agent can describe the outcome accurately. [staging-review-pending-response]

### Review surfaces

Five surfaces plus the central activity-detail page. One proposal may appear on multiple surfaces (e.g., an agent write appears on the chat card that produced it AND on the activity detail page AND on the file tree row for the affected file). Accept from any surface removes it from all.

#### Surface 1: Chat panel tool-call cards

When an agent proposes a write in review mode, the tool-call card appends two small text links inline:

```
▸ write_note(research/paper.md) ✓ Proposed  [Accept] [Reject]
```

Accept → drift-checked write → card updates to `✓ Applied`. Reject → discard → card updates to `✗ Rejected`. Same proposal also appears on the activity detail page and the file tree; accepting from the chat card removes it everywhere. [staging-accept-reject-from-chat-card]

#### Surface 2: Trails panel

Two cases:

**Whole trail is a draft** (agent or clustering proposed a new trail). Below the trails panel header, a muted banner row (same visual weight as the existing append-cursor hint):

```
┌─ Trails ──────────────────────────────────┐
│ [my-trail ▾]  [↗]                         │
│ ⓘ Draft — proposed by agent               │
│ [Accept trail]  [Reject]                   │
```

The waypoint cards render normally below so the user can inspect before deciding. Accept → moves trail-doc to `[trails] new_trail_dir`, strips `hiker.draft` flag, appends `core::changes` row. Reject → confirm then hard-deletes trail-doc + waypoint dir (no trash — this was never user data). [staging-accept-reject-from-trails]

**Active trail has pending waypoint additions.** Minimal collapsed rows at the end of the waypoint list:

```
┌─ Trails ──────────────────────────────────┐
│ 2  notes/ideas.md                         │
│ ── Proposed ───────────────────────────── │
│ +  research/raptor.md   [Accept] [Reject] │
│ +  notes/whisper.md     [Accept] [Reject] │
```

Each row is the source basename plus two muted action links. No full waypoint card — the source path is enough context for "do I want this in my trail?" The proposal also appears on the activity detail page with the same Accept/Reject. [staging-accept-reject-from-trails]

#### Surface 3: File tree

Files with pending proposals show the **same dirty indicator** (the suffix dot) already used for dirty buffers. The dot is the universal "this file has something unresolved" signal — dirty buffer, pending proposal, same visual.

Right-click context menu on the row grows a "Pending change" submenu:

```
  Open
  Rename
  Delete
  Properties
  ———————
  Pending change ▸
    Review pending change
    Accept change
    Reject change
```

"Review pending change" opens the file as a read-only buffer with the diff toggle (reuses `snapshot-preview-mode`'s existing diff pattern). Accept and Reject work from the menu without opening the file. [staging-accept-reject-from-tree]

#### Surface 4: Editor toolbar

When the open buffer has a pending proposal, a small pill appears in the editor toolbar between the `#mode-controls` slot and the right-side cluster (same placement and visual weight as the existing "Add to trail" pill):

```
Proposed change — [Accept] [Reject]
```

Hidden when there's no proposal for the active file. Accept drift-checked-writes and removes the proposal from all surfaces. Reject discards. The pill does not open a separate preview — the user is already looking at the file on disk. [staging-accept-reject-from-editor]

#### Surface 5: Activity detail page (central review surface)

The existing `vault-home-recent-activity-detail` page (`vault-home-recent-activity-detail`) gains a "Pending" filter pill alongside the existing author-class pills (user/robot icons). When active:

```
┌─ Activity ───────────────────────────────┐
│ [●] [○] [Pending]                        │  ← filter pills
│                                           │
│ [Accept all (3)]                          │
│                                           │
│ ◈ Write — research/paper.md              │
│   agent:claude · 2m ago     [Accept] [✕] │
│                                           │
│ ◈ Trail draft — "Embedding Survey"       │
│   clustering · 5m ago       [Accept] [✕] │
│                                           │
│ ◈ Waypoint add to active trail           │
│   research/raptor.md · 8m ago [Accept] [✕]│
│ ───────────────────────────────────────── │
│ ◈ Modified — research/paper.md  · 1h ago │  ← committed changes below
│ ◈ Modified — notes/ideas.md    · 2h ago  │
└───────────────────────────────────────────┘
```

Click a pending row → opens as read-only buffer with the existing diff toggle (reuses `snapshot-preview-diff-toggle`). Accept/Reject stay on the row — the editor toolbar doesn't gain accept/reject buttons for staging. [Accept all (N)] at the top runs a confirm-then-batch-apply. [staging-review-activity-detail-filter]

#### Surface 6: Queue button (combined badge)

The existing queue button in the top strip shows a combined badge: tasks + pending reviews. Click opens the queue detail page (unchanged). The review count is folded into the badge — users who want the full list go to the activity detail page. [staging-review-top-bar-badge]

### Button styling

No green/red. All Accept/Reject use the existing muted token system and the same text-link weight as the `[Restore this version]` link in the activity detail:

- **Accept** — `var(--accent)` muted outline, same color family as active states elsewhere. Low-opacity fill on hover.
- **Reject** — `var(--danger-text)` muted outline. Keeps the danger semantic but doesn't scream.
- **Accept all** — same accent outline with "all" label.

### Storage

```
.hiker/staging/
  pending.json    # [{ id, surface, action, target_path, trail_id, content_hash,
                  #    created_at, metadata }]
  <id>.md         # proposed content (empty for waypoint-adds — metadata IS the proposal)
```

One JSON index, one `.md` per proposal. Accept reads the content, drift-checks against source, writes, appends `core::changes` row, removes both files. Reject removes both files, no changelog row. GC on vault open: proposals older than 14 days discarded. [staging-retention]

### Lifecycle of a staged proposal

1. The producer writes to `core::staging::propose()`:
    - **Agent path** — `mcp-tool-write-note` (or a background feature) sees `review_required = true` and calls `core::staging::propose(action, target_path, content, metadata)` instead of writing directly.
    - **Batch mutation path** — each fanned-out task in `core::tasks` calls `core::staging::propose()` on completion.
2. Content is written to `.hiker/staging/<id>.md`; metadata to `pending.json`.
3. `hiker:staging-changed` fires. All active surfaces (chat cards, trails panel, tree row indicators, editor toolbar pill, activity detail page) refresh.
4. User accepts from any surface → `core::staging::accept(id)` drift-checked writes to source, appends `core::changes` row with `metadata.staging_proposal_id` + `metadata.action`, removes staging files. Reject → `core::staging::reject(id)` removes staging files, no changelog row.
5. Stale proposals (older than 14 days; configurable) GC on vault open.

The staging dir does *not* count as part of `core::changes` history — proposals that never apply leave no trace beyond the GC log line. Only accepted writes hit the changelog.

### `core::staging` module

```rust
Staging::open(vault_root) -> Staging

Staging::propose(input: ProposalInput) -> ProposalId
// ProposalInput { surface, action, target_path, trail_id, content, metadata }

Staging::accept(id, vault, changes) -> AcceptOutcome
// drift-checked write + core::changes row + cleanup

Staging::reject(id)
// delete staging files, no changelog row

Staging::accept_all(filter, vault, changes) -> Vec<AcceptOutcome>

Staging::list(filter) -> Vec<Proposal>
// filter: StagingFilter { path, trail_id, surface, session_id }

Staging::count(filter) -> u32
Staging::gc(max_age_days)
```

Events: `hiker:staging-changed` broadcast on propose/accept/reject so all surfaces stay in sync.

### Where the config keys live

The keys themselves live in their owning sections:

- **`[mcp.tools].review_required`** (bool, default `false`) — extends `mcp-config-section`. When true, every successful tool-write routes through staging instead of writing directly. Exposed as a bool toggle in the MCP settings UI section alongside the per-tool toggles.
- **`[llm.background].review_required`** (bool, default `false`) — lands with the v3.5 `[llm]` section. When true, debounced background features write to staging instead of mutating frontmatter directly. Exposed as a bool toggle in the LLM settings UI section's background subsection.

Batch mutations don't have a `review_required` flag — they always stage.

### Module placement

- `core::staging` — `Staging` struct, `Staging::propose/accept/reject/list/count/gc`. Pure, no Tauri imports. All `.hiker/staging/` filesystem touches confined here.
- `core::ops::agent_write_note` (and frontmatter / tag wrappers) branch on the loaded `Config`'s review flag and route to `core::staging::propose` when set.
- UI: activity detail page filter + inline accept/reject, chat tool-call card buttons, trails panel banner + proposed rows, tree context menu + dot indicator, editor toolbar pill. Each surface calls `core::staging::list(filter)` with its relevant filter.
- MCP server response shapes extend per `staging-review-pending-response`.

### Forward refs

- `diff.md` — the diff toggle already works in snapshot preview mode; staging reuses it. No new diff surface needed.
- `mcp.md` `mcp-config-section` gets the `tools.review_required` row.
- `llm.md` `[llm.background]` config gets `review_required`.
- `task-queue.md` — batch mutation fan-out producers call `core::staging::propose` on completion.
- `trails.md` — draft trail review hooks into the staging surfaces described here.
- `core::changes` — sees only accepted writes; `metadata` column carries `staging_proposal_id` for traceability.


## Settings UI shell

A vault-bar gear button toggles a settings surface that replaces the editor (same shape as the vault home page — a sub-mode of the editor pane state). The TOML files stay canonical; the panel is a UI on top of the existing `set_setting` / `Config::load` infrastructure, not a parallel storage path. [settings-pane-mode]

The headline decisions:

- **Pane mode, not modal.** The settings surface is a sub-mode of the editor pane, alongside the vault home overview and home detail. Clicking the gear swaps `#editor-pane` to a settings layout; clicking any tree row / recents row / search result returns to the editor on that note. Same shape `setVaultHomeVisible` already uses today, with the same dirty-buffer protection (a dirty editor buffer gets the existing `file-switch-guard-dirty` modal before the swap). [settings-pane-mode]
- **Gear icon in the vault bar between Home and Open-vault.** `vault-bar` order becomes Home / Settings / Open-vault, then the vault path display, then back/forward at the trailing edge. Same icon-only treatment as the existing vault-bar buttons (gear / cog glyph in the established line-weight family). Pressed/unpressed state reflects whether the settings pane is currently visible. Tooltip "Settings." [vault-bar-settings-icon]
- **A long scrollable page, sections stacked.** The body mirrors `vault-home-overview`: a header with the vault name + scope toggle, then a stack of section cards — one per `[section]` in the loaded config. Each card is a list of rows; each row is one config key with its current value, an inline control, a "reset to default" affordance, and a one-line description. No left-rail tabs in v1 — a single scrollable page is honest about the v1 surface size and matches the home page's vertical stack. [settings-pane-section-list]
- **Eligible keys are interactive; everything else is read-only with a "edit TOML" affordance.** The write-back-eligible key set (the closed list in `core::config`) drives which rows get a live control vs. a read-only display. Read-only rows show the current value in a muted code style with an "Open `config.toml`" link that opens the TOML in the system file manager via the existing `reveal_in_file_manager` Tauri command. Same fail-loud / restart-to-apply model as today — non-eligible keys can't be written through `set_setting` for safety, the TOML is the source. [settings-pane-eligible-key-controls, settings-pane-readonly-display]
- **Per-section scope toggle.** A small `[User] [Vault]` segmented control on each section card flips which file the displayed values come from, since either file may carry any key per "Merge & precedence." Default scope per section: vault for `[editor]` / `[vault]`-UI keys, user for `[vault].recent` / `[vault].default`. The user can flip; the choice is per-section and per-session, not persisted. [settings-pane-scope-toggle]


### Layout

```
┌─ Settings ────────────────────────────────────────┐
│  Settings · <vault path>                          │
│                                                   │
│  ▾ Editor                          [User] [Vault] │
│    Render .txt as markdown        [✓]   [reset]  │
│    Live preview                   [✓]   [reset]  │
│    Word wrap                      [✓]   [reset]  │
│    Show line numbers              [✓]   [reset]  │
│    Show whitespace                [ ]   [reset]  │
│    Show chunk boundaries          [ ]   [reset]  │
│    Tab size                       [ 2 ] [reset]  │
│                                                   │
│  ▾ Indexing                       [User] [Vault] │
│    Model                bge-small-en-v1.5  ⓘ     │  ← read-only, ⓘ = "edit TOML to change"
│    Batch size                       64            │  ← read-only
│    Ignored paths                  [empty]         │  ← read-only
│                                                   │
│  ▾ Vault                          [User] [Vault] │
│    Sidebar open                   [✓]   [reset]  │
│    Related panel open             [ ]   [reset]  │
│    Trash expanded                 [ ]   [reset]  │
│    Tree sort                      [Name (A→Z) ▾] │
│    Default vault            ~/notes/work    ⓘ    │  ← user scope
│    Recent vaults                  [3 entries] ⓘ  │  ← user scope, read-only
│                                                   │
│  ▸ Keymap (no overrides yet)                      │
│  ▸ LLM (deferred)                                 │
│  ▸ ACP (deferred)                                 │
│                                                   │
│  Schema version: 1                                │
│  [Open user config.toml]  [Open vault config.toml]│
└───────────────────────────────────────────────────┘
```

Section cards collapse/expand via a chevron (state in-memory only; no per-section persistence — the UI is meant to be skim-then-act, not a persistent workspace). Deferred sections (LLM, ACP, embedder) appear collapsed with a one-line description of what they'll cover when their backing feature lands; no editable rows. [settings-pane-deferred-sections-stub]


### Row controls

The control type per row is inferred from the field's declared type in `core::config` and a small per-key annotation:

- **bool** → checkbox toggle. Click flips immediately, calls `set_setting`, no separate Save button.
- **integer (bounded enum)** like `tree.sort_by` → small dropdown with the known values.
- **integer (numeric)** like `tab_size` → number input with the field's min/max bounds (declared as part of the strict-load validation).
- **string (enum)** like `indexing.model` → dropdown when the eligible-value set is small; read-only display when it's a free-form string the loader validates.
- **string (free-form)** → text input with debounced commit (300ms after last keystroke, same shape the search input uses).
- **array of strings** → tag-style pill list with add/remove. Eligible only when a real consumer needs it (`indexing.ignored_paths` is the natural first; the rest are rare).

Every interactive row ends with a small `[reset]` affordance that writes the in-code default for that key. Greyed when the current value already equals the default. [settings-pane-reset-row]

Read-only rows (`settings-pane-readonly-display`) show a muted info glyph; click opens a small popover with: the current value spelled out, the file it came from (`<vault>/.hiker/config.toml` or the user TOML), and an "Open in file manager" button that fires `reveal_in_file_manager` against that path. [settings-pane-open-toml-link]


### Lifecycle

- **Entering settings.** The gear button calls `setSettingsPaneVisible(true)` (same shape as `setVaultHomeVisible`), which swaps `#editor-pane` to the settings layout and renders the cards from the current `Config`. If the editor buffer is dirty, the existing `file-switch-guard-dirty` modal fires first.
- **Live updates.** Every interactive flip calls the existing `set_setting` Tauri command. The command already updates the in-memory `Arc<Config>` and writes through `toml_edit`; the UI re-renders the affected row from the new value. No separate "save" button.
- **External edits.** If the user hand-edits a TOML while the settings pane is open, the displayed values are stale until the next `set_setting` (which reloads the merged Config as a side effect, per existing `settings-write-back` semantics) or until the user closes and reopens the pane. A small "Refresh" affordance in the header forces a reload without making a write — calls a new `reload_config` command that re-runs `Config::load` and re-renders. Cheap to add; lands when a user actually hits the staleness case in real use. [settings-pane-manual-refresh]
- **Exiting settings.** Clicking any tree row, recents row, search result, or the Home button exits settings the same way it exits home — no save protection needed (nothing is dirty in the settings pane; every flip is committed).
- **Navigation history.** Entering settings is a content-surface change; pushes onto `navigation-history-stack` like home does. Back returns the user to wherever they were.

### Keybind

Reserves `settings.open` in `keybind-registry`. Chord: `Cmd-,` on macOS (matches every macOS preferences convention), `Ctrl-,` elsewhere. Toggles the settings pane open/closed. Lands when the keybind is wired; the registry entry is the seam. [settings-pane-keybind]


### Out of scope (this surface)

- **Theme / font / color-scheme.** Visual customization needs a theme system first; settings UI doesn't ship its own. Lands when theming is real.
- **Live keybind editor.** The `[keymap]` section is a stub — the loader doesn't yet read keybind overrides per `settings-section-keymap`. Settings UI shows the section header with "no overrides yet" and a one-line pointer to `keybind-registry`. The full editor lands with the loader.
- **LLM provider / model picker.** Lives behind the deferred `[llm]` section; the settings UI shows the section as deferred. Lands with `llm-providers-config`.
- **Embedder model picker.** Same shape — `[embedder]` is deferred until a second model lands. The card surfaces it as deferred with a one-line note.
- **ACP agent picker.** Deferred with `core::acp`.
- **Schema migration UI.** Schema bumps are still hard-fail per `settings-schema-version`; "delete to regenerate" is the workaround. A migration UI is deferred until post-real-use migration is a real concern.
- **Search across settings.** A search input that filters rows by key/description. Useful at scale; v1 has ~12 interactive keys total — search would be more chrome than benefit.
- **Per-key changelog.** Showing "this key was last changed on <date> by <action>" is interesting but not load-bearing; defer.


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
- `[mcp.tools].review_required` — bool toggle in the MCP server settings section. When on, MCP write tools route through staging. Vault-scope (per-surface gate that makes sense per vault). Live-applied.
- `[llm.background].review_required` — bool toggle in the LLM settings section's background subsection. When on, background features write to staging. Vault-scope. Live-applied.

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

- ~~Settings UI shell.~~ Promoted out of "Deferred" — see `## Settings UI shell` above. The TOML is still canonical; the panel just calls `set_setting` against the same write-back path the View menu already uses, plus read-only display + "edit TOML" affordance for non-eligible keys.
- **Hot-reload.** Watch the two TOML files and re-apply on change. Not needed in v1 — `set_setting` keeps in-memory and on-disk state in sync for in-app flips, and external hand-edits while running are rare enough that "restart to apply" is acceptable.
- **Expanding the write-back-eligible key set.** v1 hard-codes which keys are user-mutable from inside the app. New eligible keys are added one-at-a-time as new UI controls appear (e.g. theme picker, model picker). Generalized "any key, anywhere" write-back isn't planned — it would require either UI for every key or a generic settings panel.
- **Per-key user-vs-vault scope enforcement on read.** Today the loader accepts any key in either file. If real misuse appears (users putting `vault.recent` in vault TOML and confusing themselves), add a per-field scope tag and reject misplaced keys at parse time. Write-back already routes per scope, so the read side is the only gap.
- **Auto-rewrite of existing TOML files when keys are added.** When a future binary adds a key, existing TOMLs won't have it (`serde(default)` fills in transparently). Auto-rewriting to inject the new key with a comment would keep files self-documenting at the cost of touching user files on every launch — defer until users actually ask for it; "delete the file to regenerate" is the workaround.
- **Theme / font.** Need a UI to be discoverable; not worth a TOML-only surface. Land alongside the settings UI shell.
- **`[autosave]` config section.** Crash-recovery autosave (`autosave.md`) ships with a hard-coded 5s tick and no on/off knob. If a workflow asks for `tick_secs` / `enabled` / on-blur-only mode, the section lands then; the strict-load posture and write-back machinery already cover the shape.
- **Sync of settings across machines.** Per-vault settings travel with the vault under Syncthing. The per-user TOML doesn't sync and shouldn't (recent vaults, default vault, machine-specific paths).
- **Backward-compatibility shims for old TOML shapes.** Pre-real-use we delete and re-create; post-real-use we ship per-version migrations. No "load old shape into new struct" code in between.
- **`hiker config get/set` CLI.** Convenient but not required — `vim ~/.config/hiker/config.toml` plus the in-app write-back covers the v1 need.
- **"Did you mean X?" hints on unknown keys.** The strict-load posture above mentions this; v1 doesn't implement it. The hard error already names the bad key plus the file, which is enough for the user to grep the spec. A near-match suggester (Levenshtein or similar over the known field names) is a polish-grade addition that can land when someone hits the case in real use.


## Out of scope

- A web-based settings UI or remote config (Hiker is local-first by design).
- Per-note settings beyond what frontmatter already supports.
- Settings inheritance across multiple vaults (each vault's settings are independent of any other vault's settings).
- Encrypted settings (no secrets live in `config.toml` today; if cloud embedder API keys land later, they get their own keychain-backed storage, not a TOML field).
