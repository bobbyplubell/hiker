# Settings

Configuration surface for Hiker. v1 ships a TOML loader and the section content needed to unblock the deferred toggles in `editor.md`, `txt-ingest.md`, and `index.md`.

- **Two TOML files.** Per-user `config.toml` at the platform config dir, per-vault `vault/.hiker/config.toml`. Vault overrides user key-by-key. [settings-user-config-toml] [settings-vault-config-toml]
- **Read once at startup.** No watcher, no hot-reload, no per-access reread. Restart to apply. Struct built once and handed around frozen. [settings-load-once-at-startup]
- **Strict load.** Unknown key or type mismatch aborts startup with a `file:line` error. Same fail-loud discipline as `store-version-fail-loud` in `index.md` — silently dropping a user-set value is worse than refusing to start. [settings-strict-load]
- **Defaults live in Rust, files auto-create on first load.** Every field is `serde(default)`; the loader treats a missing file as empty *and* writes a defaults-populated TOML so users have a self-documenting file to edit. [settings-defaults-in-code] [settings-auto-create-defaults]
- **In-app toggles write back.** Existing UI toggles (View menu, tree sort, sidebar/related panel state, trash expansion) persist their flips to the vault TOML so they survive restart. No generalized settings panel in v1 — the TOML is still canonical for everything else. [settings-write-back]


## Storage location

**Per-user** at the platform config dir (use the `directories` crate, no hand-rolled paths): [settings-user-config-toml]

- Linux: `~/.config/hiker/config.toml`
- macOS: `~/Library/Application Support/hiker/config.toml`
- Windows: `%APPDATA%\hiker\config.toml`

**Per-vault** at `vault/.hiker/config.toml`. Lives next to `index.db`, `logs/`, `trash/` — same `.hiker/` parent the watcher already ignores. Travels with the vault under Syncthing/git. [settings-vault-config-toml]

Either file may be absent; both absent is the same as both empty.


## Merge & precedence

Deep merge: user is the wide default ("my preferences for any vault"), vault is the local override ("for *this* vault, also do X"), vault wins on key overlap.

- Maps merge recursively. `[editor]` in both files produces a single table where vault keys win on overlap.
- Arrays replace, not concatenate. User `indexing.ignored_paths = ["foo/"]` plus vault `["bar/"]` = `["bar/"]`. Concatenation would prevent a user from *removing* an inherited entry.
- `schema_version` comes from whichever file declares it; if both disagree, vault wins (same overlap rule). Mismatch with the binary's expected version is the strict-load case below.
- No per-key user-only/vault-only tagging. Any key may appear in either file. The section tables name which keys make sense where, but the loader doesn't enforce it — putting `vault.recent` in a vault TOML works, it's just useless.


## Schema version & strict loading

Top-level `schema_version` integer (default `1`). Mismatch with the binary's expected version is a hard startup failure with a message naming both versions. No silent migration. Same posture as `store-version-fail-loud`. [settings-schema-version]

**Strict load posture.** Unknown keys and type mismatches abort startup, naming the file, offending key, and line/column. The `schema_version` check fires before the unknown-key check, so a downgrade (older binary against a newer TOML) reports "schema N, expected M" rather than a misleading unknown-key error. Migration follows `index.md`'s policy: pre-real-use, schema bumps are handled by deleting the offending TOML; post-real-use, every bump ships an additive migration that reads the old shape and writes the new one in place.

`tracing::error!` events use the `obs-error-context` field discipline: `error!(file = %path.display(), line, col, key = %k, "unknown setting key")`. The user-visible error is a one-line summary plus a "see hiker.log for details" hint.

[settings-strict-load]


## Defaults & auto-create

Every settable field is declared in Rust with `#[serde(default)]` (and `#[serde(deny_unknown_fields)]` per the strict-load rule). The default for each field lives in a single `Default` impl on its containing struct. [settings-defaults-in-code]

**Auto-create.** On `Config::load`, a missing file is written fresh with the full defaults serialized (plus a header comment naming the binary version), then read back through the normal parse path — so the file is self-documenting. When a future binary adds a key, existing files lack it and `serde(default)` fills it in transparently; delete the file to regenerate with new defaults.

Auto-create runs at most once per file per process. If the file is created and then deleted mid-session, the next load (next launch / vault swap) re-creates it. Concurrent first-launches against the same vault from two processes can race, but the loser harmlessly overwrites with identical content; not worth a lock. [settings-auto-create-defaults]

The user TOML is auto-created on the first launch ever; the vault TOML is auto-created on the first open of each vault that doesn't have one. Both auto-creates use atomic write-then-rename so a crash mid-write can't leave a half-file.


## Sections

Each section gets a fuller spec when its UI consumers are built; the schema below is what v1 actually loads.

### [editor] [settings-section-editor]

Per-vault toggles for the editor pane and View menu. All optional; each has an in-code default. Loaded once at startup; in-session flips from the View menu update both the live state *and* the vault TOML via `settings-write-back`, so a relaunch finds the same state.

| Key | Type | Default | Notes |
| --- | ---- | ------- | ----- |
| `render_txt_as_markdown` | bool | `true` | Backs `txt-render-as-markdown-default` in `txt-ingest.md`; `view-render-txt-as-markdown-toggle` is the in-session override and ungreys with this loader |
| `live_preview` | bool | `true` | Initial state of `view-live-preview-toggle`; matches `live-preview-default-on` |
| `word_wrap` | bool | `true` | Initial state of `view-word-wrap-toggle`; ungreys this entry |
| `show_line_numbers` | bool | `true` | Initial state of `view-line-numbers-toggle` |
| `show_whitespace` | bool | `false` | Initial state of `view-show-whitespace-toggle` |
| `highlight_trailing_whitespace` | bool | `false` | Initial state of `view-highlight-trailing-whitespace-toggle` |
| `show_chunk_boundaries` | bool | `false` | Initial state of `view-show-chunk-boundaries`; debugging-grade view, off by default |
| `intraline_diff` | bool | `false` | Initial state of `view-intraline-diff-toggle`; off keeps the existing line-level rendering. See `diff.md`'s "Diff style" section |
| `tab_size` | u8 | `2` | Editor indent width — spaces inserted per Tab / list-indent step |
| `system_font` | string | platform UI default | Font for non-editor chrome (toolbars, menus, sidebar, status bar, tabs). Per `editor-three-fonts` |
| `editor_font` | string | platform proportional default | Font for the editor canvas's prose body. Per `editor-three-fonts` |
| `code_font` | string | platform monospace default | Font for fenced code blocks AND frontmatter blocks (per `editor-frontmatter-rendering-fix`) AND inline code AND the diff layer's code-shaped hunks. Per `editor-three-fonts` |

Font family rides the three font slots per `editor-three-fonts`; per-slot **size** and theme are deferred (theme tokens own size). Crash-recovery autosave (`autosave.md`) is on with a fixed 5s tick and no config surface in v1.

### [indexing] [settings-section-indexing]

Indexer tunables. Vault-level is the natural scope, but a user-level default is fine.

| Key | Type | Default | Notes |
| --- | ---- | ------- | ----- |
| `model` | string | `"bge-small-en-v1.5"` | Embedder model id: `bge-small-en-v1.5` (default, 384-dim, English), `bge-m3` (1024-dim, multilingual, 8k ctx), `embedding-gemma-300m` (768-dim); strict-load rejects others. Bumping forces full re-embed (`embedder-version-tag`); a dim change rebuilds the vec0 table (`store-rebuild-chunk-vecs-on-dim-change`). UI: interactive dropdown gated by `settings-embedder-model-change-warning` |
| `batch_size` | u16 | `64` | Embed batch size; backs the partial `embedder-batch-64` |
| `ignored_paths` | array of strings | `[]` | Additional ignore patterns on top of the hard-coded list in `watcher-ignore-hardcoded`. gitignore-style globs, evaluated against vault-relative paths. Replaces, doesn't concatenate, per the array-merge rule above |

The destructive **Reindex (rebuild)** verb (`reindex-rebuild-action`) lives here in the *eventual* settings UI per `editor.md`; the verb itself stays planned in `status.md` until the UI shell lands. The CLI counterpart `cli-reindex-rebuild` continues to cover the operational case.

#### Embedder-model change warning [settings-embedder-model-change-warning]

Switching `[indexing].model` is the single most expensive setting flip in the app — every note in the vault has to be re-embedded, and a dim change additionally rebuilds the vec0 table. A pulldown that commits silently would let users trigger hours of CPU work on an accidental click. The model dropdown gates the write through a confirm modal:

```
┌─ Change embedding model? ─────────────────────────┐
│                                                   │
│  Switching from bge-small-en-v1.5 to bge-m3 will  │
│  re-embed every note in this vault.               │
│                                                   │
│  • All chunks re-embedded (no chat / search       │
│    answers from semantic until done)              │
│  • Dim change (384 → 1024): the vector table is   │
│    dropped and recreated                          │
│  • Expect minutes to hours depending on vault     │
│    size and CPU                                   │
│                                                   │
│  [Cancel]              [Change model and re-embed]│
└───────────────────────────────────────────────────┘
```

Cancel is default-focused; the action button carries the standard accent treatment used for other consequential confirms (`confirm3-real-modal`). The body names the current and new model verbatim. The "Dim change" bullet is only rendered when the new model's dim differs from the current one — same-dim swaps omit it. The time estimate is a qualitative range, not a computed number.

Behavior on Confirm:
1. `set_setting("indexing.model", new_id)` writes through `toml_edit` per `settings-write-back`.
2. The indexer hot-reloads the embedder in place — no app / vault restart. A new `IndexJob::ReloadEmbedder { model_id }` job goes through the existing mpsc channel; the indexer task loads the new model on `spawn_blocking`, swaps it into the live `Arc<dyn Embedder>` (also visible to the search-query embedder via the `OnceCell` per `search-query-embed-spawn-blocking`), calls `Store::ensure_chunk_vecs_dim(new_dim)` to run any rebuild, then enqueues a vault-wide reindex. [embedder-hot-reload-on-model-change]
3. The store's dim-check inside that handler (`store-rebuild-chunk-vecs-on-dim-change`) catches the dim mismatch and rebuilds `chunk_vecs` before the first new batch lands.
4. Status bar reflects the re-embed progress via the existing `status-bar-index-label` machinery, with a transient "Loading <model>…" sub-state while the new model weights download / load.

Behavior on Cancel: the dropdown reverts to the previous value; no write occurs.

The warning is only on the **settings UI** path. A vault-TOML hand-edit surfaces no warning — strict-load picks up the new value on the *next launch*, the embedder loads at the new id, and the re-embed runs (hot-reload is UI-only, "restart to apply" otherwise, per `settings-load-once-at-startup`). Documented in the inline "Notes" comment of the auto-created TOML.

### [chat] [settings-section-chat]

| Key | Type | Default | Notes |
| --- | ---- | ------- | ----- |
| `chats_dir` | string | `"chats/"` | Visible folder holding native + imported chat-session notes (`chat-session-markdown-store`); imports land in its `imported/` subfolder |

### [render] [settings-section-render]

Vault-wide policy for the editor's rendered-widget layer (LaTeX math, Mermaid / WaveDrom diagrams, tables). Per-vault only — the cache it governs lives in the vault's `.hiker/`. Live-applied (no restart).

| Key | Type | Default | Notes |
| --- | ---- | ------- | ----- |
| `cache_diagrams` | bool | `true` | Persist rasterized math / Mermaid / WaveDrom widgets to `<vault>/.hiker/diagram-cache/`, keyed by `content_hash`, so reopening a note skips the `resvg` blit. Sits below the in-memory `CachedDeco` / texture caches (`widget-render-disk-cache`); off = in-memory only. Best-effort 64 MB LRU sweep bounds the dir; tables paint natively (unaffected). Surfaced as the "Cache rendered diagrams to disk" toggle (`render-cache-diagrams-toggle`) |

### [inbox] [settings-section-inbox]

Deterministic auto-organization rules for newly-created notes. Per-vault only (rules are vault content). Full behavior spec lives in `docs/inbox-rules.md`; the schema below is what the loader reads.

| Key | Type | Default | Notes |
| --- | ---- | ------- | ----- |
| `rules` | array of tables | `[]` | Ordered list of `{ match, action }` entries; first match wins. See `inbox-rules` |

Each `rules[i]` entry is a table:

```toml
[[inbox.rules]]
match = { basename = "^TODO-.*", body = "^#"  }   # any of: basename regex, body regex, both; AND-combined when both present
action = { move_to = "inbox/todos/", add_tag = "todo" }   # one or both of move_to / add_tag
```

Strict-load rejects malformed entries (invalid regex, missing both `move_to` and `add_tag`). Default empty array = no auto-organization, all new notes land at their original create path.

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

#### Default vault auto-open

When `vault.default` is set, the app auto-opens that path on startup instead of the picker; unset or absent shows the picker. Full bootstrap steps and failure modes in the "Default vault auto-open" section below.

### [keymap]

Stub. v1 does not load keybind overrides; `settings-section-keymap` stays planned. The `keybind-registry` in `editor.md` is shaped to accept overrides — the loader is the missing piece, deferred until a user actually wants to remap something. When it lands the format is `keymap.<binding-id> = "<chord>"`.

### [llm]

The full schema lives in `llm.md` (`llm-providers-config`); summarized here so the section list in this doc is complete. Shape: top-level `enabled` (gates `llm-features-disable-entirely`), `[llm.provider]` (backend / model / api_key_env / base_url), `[llm.limits]` (max_tokens / timeout_secs), `[llm.agent]` (iteration_cap / tool_timeout_secs per `agent-iteration-cap-prompt` and `agent-tool-call-timeout`), `[llm.audit]` (log_full_prompt — obs-no-content discipline). Loader, strict validation, and `core::llm` client construction are wired today; per-feature toggles for background/fan-out features land with those features themselves.

### [embedder] (deferred — lands when cloud/Ollama embedder option ships)

Stub. The full schema lives in `index.md`'s embedder section (`embedder-config-section`); same shape as `[llm]` — `provider`, `model`, `api_key_env`, `base_url`. Default `provider = "fastembed"` matches today's behavior so existing vaults don't change. Until the schema lands, the section is unrecognized and `settings-strict-load` will refuse it.

### [acp] (deferred — lands when `core::acp` ships)

Stub. The full schema lives in `llm.md` (`llm-acp-client-optional`); enables routing the chat panel through an external ACP agent instead of the basic agent loop. Shape: `agent` (registry id, "bundled" alias for the basic loop, or "none" for disable mode). Loader lands with `core::acp` itself.

### [sync]

The full schema lives in `sync.md` (`sync-config-section`); summarized here so the section list is complete. Per-vault: `enabled` / `mode` / `server_url` / `discovery` / `device_name`; `devices` + the learned `device_names` map are enrollment state. Secrets (content key, device private key) are user-scope and never appear. Exposed as standard settings rows (vault scope); `devices` is read-only there. [settings-section-sync]


## Pending change review

Some writes shouldn't land directly on disk: the user didn't author the bytes (agent writes), or can't watch them all at once (batch mutations). These land as `status=pending` ops in the op log (per `op-log.md`) until accepted. Accept/reject lives inline on every surface that cares (enumerated below) — no dedicated staging editor sub-mode.

Two flows produce pending ops:

- **Agent writes (opt-in)** — MCP tool calls and background features are gated by `review_required` flags; flipping them on makes the produced ops enter the log as `status=pending` instead of `status=accepted`. [agent-write-review-mode]
- **Batch mutations (always pending)** — multi-note mutation actions (e.g., "reformat every `.txt` in `inbox/`") fan out N tasks per `task-queue.md`; the user can't watch N buffers, so each result lands as pending unconditionally. Single-note user-initiated mutations stay in-buffer per `editor.md`'s Note-mutations menu.

The substrate — per-document pending queue (`<doc-id>.pending`), op shapes (`edit_note` → N `Replace` ops sharing a `batch_id`; `write_note`/`set_frontmatter`/`apply_tag` → one op; `move_note`/`rename` → `Rename`), drift detection, status states, and restart survival — is owned by `op-log.md` (`op-log-pending-queue`, `op-log-op-shape`, `op-log-status-states`, `op-log-pending-survives-restart`). What this doc owns is the settings-facing review surface behavior:

- **Agent-write review is off by default; opt-in per write surface.** Agent ops enter as `status=accepted` and reach disk immediately by default; users flip the per-surface `review_required` flag for a checkpoint. [agent-write-review-mode]
- **Accept/reject is integrated into every relevant surface, not a separate editor mode.** Each surface renders accept/reject inline (chat cards, trails panel, file tree context menu, unified `Changes` tab). Hunk-shaped pending ops render in the live editable buffer as inline decorations + per-file pill; whole-file pending ops open via the read-only-buffer-with-diff-toggle pattern, framed "Review rewrite" / "Review new note" (both per `patch-review.md`, `write-note-review-surface`). Drifted ops surface with Accept disabled, Reject active.
- **The activity detail page is the central review surface.** The existing `vault-home-recent-activity-detail` page gains a "Pending" filter pill (alongside the author-class pills) showing all pending ops across all sources; each row carries [Accept] [Reject], with [Accept all (N)] at the top. [staging-review-activity-detail-filter]
- **MCP write responses are honest about pending.** When review mode is on, MCP write tools return success-with-pending — the JSON response carries `status: "pending"` plus the op id. [staging-review-pending-response]
- **Accept navigates to the target note as a preview tab.** After an individual accept (not batch), the UI opens the affected note at its target path with `preview: true`. Batch accept (`Accept all`) stays on the current surface. [staging-accept-navigates-to-preview]

### Review surfaces

Five surfaces plus the central activity-detail page. One proposal may appear on several (an agent write shows on its chat card AND the activity detail page AND the file tree row); accept from any surface removes it from all.

#### Surface 1: Chat panel tool-call cards

The tool-call card appends two text links inline:

```
▸ write_note(research/paper.md) ✓ Proposed  [Accept] [Reject]
```

Accept → drift-checked write → navigates to the target note as a preview tab. Card updates to `✓ Applied`. Reject → discard → card updates to `✗ Rejected`. Same proposal also appears on the activity detail page and the file tree; accepting from the chat card removes it everywhere. [staging-accept-reject-from-chat-card]

#### Surface 2: Trails panel

Two cases.

**Whole trail is a draft** (agent or clustering proposed a new trail). Below the panel header, a muted banner row (append-cursor-hint weight):

```
┌─ Trails ──────────────────────────────────┐
│ [my-trail ▾]  [↗]                         │
│ ⓘ Draft — proposed by agent               │
│ [Accept trail]  [Reject]                   │
```

Waypoint cards render normally below for inspection. Accept → moves trail-doc to `[trails] new_trail_dir`, strips `hiker.draft`, appends `core::changes` row. Reject → confirm then hard-deletes trail-doc + waypoint dir (no trash — never user data). [staging-accept-reject-from-trails]

**Active trail has pending waypoint additions.** Collapsed rows at the end of the waypoint list:

```
┌─ Trails ──────────────────────────────────┐
│ 2  notes/ideas.md                         │
│ ── Proposed ───────────────────────────── │
│ +  research/raptor.md   [Accept] [Reject] │
│ +  notes/whisper.md     [Accept] [Reject] │
```

Each row is the source basename plus two muted action links — no full waypoint card. Also appears on the activity detail page with the same Accept/Reject. [staging-accept-reject-from-trails]

#### Surface 3: File tree

Pending proposals are **merged into the file tree at their target path**, not isolated in a separate panel:

- **New files** (target path doesn't yet exist) appear as synthetic file rows at their destination folder with a **greyed name**.
- **Changes to existing files** show the **same dirty indicator** (suffix dot) used for dirty buffers — the universal "something unresolved" signal.
- **Synthetic directories** are created when a proposal targets a path inside a folder that doesn't exist yet; the folder row appears greyed and expands to reveal staged children.
- The tree refreshes automatically on staging-snapshot updates so proposals appear/disappear as they're resolved from any surface.

Right-click context menu on the row grows staging actions:

```
  Open
  Rename
  Delete
  Properties
  ———————
  Review pending change
  Accept change
  Reject change
```

"Review pending change" opens the file as a read-only buffer with the diff toggle (reuses `snapshot-preview-mode`). Accept and Reject work from the menu without opening the file. [staging-accept-reject-from-tree]

#### Surface 4: Editor toolbar

When the open buffer has a pending proposal, a pill appears in the toolbar's right cluster, just left of Save (same placement/weight as the "Add to trail" pill):

```
Proposed change — [Accept] [Reject]
```

Hidden when there's no proposal for the active file. Accept drift-checked-writes and removes the proposal from all surfaces; if the active buffer is the target file it reloads from disk, otherwise navigates to the target as a preview tab. Reject discards. No separate preview — the user is already looking at the file. [staging-accept-reject-from-editor]

#### Surface 5: Activity detail page (central review surface)

The existing `vault-home-recent-activity-detail` page gains a "Pending" filter pill alongside the author-class pills (user/robot icons). When active:

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

Click a pending row → opens the editor tab in diff mode with `DiffSource = StagingProposal(id)` (per `diff-as-mode`). Accept → navigates to the target note as a preview tab. Reject → returns to the activity detail page. [Accept all (N)] at the top runs a confirm-then-batch-apply (stays on current surface). [staging-review-activity-detail-filter]

#### Surface 6: Queue button (combined badge)

The existing top-strip queue button shows a combined badge: tasks + pending reviews. Click opens the queue detail page (unchanged); the full review list lives on the activity detail page. [staging-review-top-bar-badge]

### Button styling

No green/red. All Accept/Reject use the muted token system and the text-link weight of the `[Restore this version]` link in the activity detail:

- **Accept** / **Accept all** — `var(--accent)` muted outline, low-opacity fill on hover.
- **Reject** — `var(--danger-text)` muted outline; keeps the danger semantic without screaming.

### Storage, lifecycle, and queries

The substrate is owned by `op-log.md` and `patch-review.md`, not respecified here. Pending-op storage layout, the produce → `stage_pending` → surface-refresh → accept/reject (`flip_op_status`) lifecycle, drift derivation, query filters (`core::oplog::query` by `path` / `trail_id` / `surface` / `session_id` / `status`), op-log change events, and retention/auto-reject behavior all live in `op-log.md` (`op-log-store-layout`, `op-log-op-shape`, `op-log-status-states`, `op-log-config-section`). There is no separate staging database; the `[staging]` config section is replaced by `[op-log]`. The substrate API is `core::oplog`; producer helpers and `flip_op_status` live in `core::ops`.

### Config keys for review

Per-surface gates live in their owning sections; log-level behavior (`auto_reject_on_drift`, `metadata_retention_days`, `rejected_retention_days`) lives in `[op-log]` per `op-log-config-section`.

- **`[mcp.tools].review_required`** (bool, default `true`) — extends `mcp-config-section`. When true, every MCP tool-write enters the log as `status=Pending` instead of `Accepted`. Live-applied. Exposed as a bool toggle in the MCP settings UI section.
- **`[llm.background].review_required`** (bool, default `false`) — lands with the `[llm]` section. When true, debounced background features emit pending ops instead of mutating frontmatter directly.

Batch mutations have no `review_required` flag — they always emit pending ops.

### Forward refs

- `op-log.md` — the substrate; storage, op shape, status states, materialization, drift, config section, unified activity feed (`metadata.agent_op_id` carries the originating op for traceability).
- `patch-review.md` — inline per-hunk accept/reject for `Replace` ops plus the write-note review surface for whole-file ops; `diff.md` is the underlying diff primitive.
- `mcp.md` `mcp-config-section` gets the `tools.review_required` row; `mcp-tool-edit-note` produces per-edit `Replace` ops.
- `llm.md` `[llm.background]` config gets `review_required`.
- `task-queue.md` — batch mutation fan-out producers append pending ops on completion.
- `trails.md` — draft trail review hooks into the surfaces described here.


## Settings UI shell

A vault-bar gear button toggles a settings surface that replaces the editor (same shape as the vault home page — a sub-mode of the editor pane state). The TOML files stay canonical; the panel is a UI on top of the existing `set_setting` / `Config::load` infrastructure, not a parallel storage path. [settings-pane-mode]

- **Pane mode, not modal.** A sub-mode of the editor pane (alongside vault home overview / detail). Clicking the gear swaps `#editor-pane` to a settings layout; clicking any tree row / recents row / search result returns to the editor. Same shape `setVaultHomeVisible` uses, with the same dirty-buffer protection (`file-switch-guard-dirty` modal before the swap). [settings-pane-mode]
- **Gear icon in the vault bar between Home and Open-vault.** `vault-bar` order becomes Home / Settings / Open-vault, then the vault path, then back/forward. Icon-only treatment matching the existing vault-bar buttons; pressed state reflects whether the pane is visible. Tooltip "Settings." [vault-bar-settings-icon]
- **A long scrollable page, sections stacked.** Mirrors `vault-home-overview`: a header with vault name + scope toggle, then one section card per `[section]` in the loaded config. Each card lists rows; each row is one config key with its current value, an inline control, a reset affordance, and a one-line description. No left-rail tabs in v1. [settings-pane-section-list]
- **Eligible keys are interactive; everything else is read-only with an "edit TOML" affordance.** The write-back-eligible key set (closed list in `core::config`, enumerated under "Write-back") drives which rows get a live control vs. read-only display. Read-only rows show the value in muted code style with an "Open `config.toml`" link firing `reveal_in_file_manager`. Non-eligible keys can't be written through `set_setting` — same fail-loud / restart-to-apply model as today. [settings-pane-eligible-key-controls] [settings-pane-readonly-display]
- **Per-section scope toggle.** A `[User] [Vault]` segmented control per card flips which file the displayed values come from, since either may carry any key per "Merge & precedence." Default scope: vault for `[editor]` / `[vault]`-UI keys, user for `[vault].recent` / `[vault].default`. Per-section, per-session, not persisted. [settings-pane-scope-toggle]


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
│    Model         [bge-small-en-v1.5 ▾]  [reset]  │  ← dropdown; flip prompts re-embed
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

- **Entering settings.** The gear calls `setSettingsPaneVisible(true)`, swapping `#editor-pane` to the settings layout and rendering cards from the current `Config`. A dirty editor buffer fires the `file-switch-guard-dirty` modal first.
- **Live updates.** Every interactive flip calls `set_setting`, which updates the in-memory `Arc<Config>` and writes through `toml_edit`; the UI re-renders the affected row. No separate "save" button.
- **External edits.** Hand-edits to a TOML while the pane is open leave displayed values stale until the next `set_setting` (which reloads the merged Config as a side effect) or a pane reopen. A header "Refresh" affordance forces a reload without writing — a new `reload_config` command re-runs `Config::load`. Lands when a user hits the staleness case. [settings-pane-manual-refresh]
- **Exiting settings.** Any tree row / recents row / search result / Home button exits, same as home — no save protection (every flip is already committed).
- **Navigation history.** Entering settings pushes onto `navigation-history-stack` like home; Back returns the user where they were.

### Keybind

Reserves `settings.open` in `keybind-registry`. Chord: `Cmd-,` on macOS (matches every macOS preferences convention), `Ctrl-,` elsewhere. Toggles the settings pane open/closed. Lands when the keybind is wired; the registry entry is the seam. [settings-pane-keybind]


### Out of scope (this surface)

- **Theme / font / color-scheme.** Needs a theme system first; lands when theming is real.
- **Live keybind editor.** The `[keymap]` section is a stub (loader doesn't read overrides per `settings-section-keymap`). UI shows the section header with "no overrides yet" and a pointer to `keybind-registry`; the editor lands with the loader.
- **LLM provider / model picker.** Behind the deferred `[llm]` section; shown as deferred. Lands with `llm-providers-config`.
- **Cloud / Ollama embedder provider picker.** The `[embedder]` *provider* section stays deferred until `embedder-llm-crate-backed`. Within-fastembed model selection (bge-small / bge-m3 / embedding-gemma-300m) is live in `[indexing].model` per `embedder-model-selectable`.
- **ACP agent picker.** Deferred with `core::acp`.
- **Schema migration UI.** Bumps are hard-fail per `settings-schema-version`; "delete to regenerate" is the workaround until post-real-use migration is a concern.
- **Search across settings.** v1 has ~12 interactive keys — search would be more chrome than benefit.
- **Per-key changelog.** "Last changed on <date> by <action>" — not load-bearing; defer.


## Loading lifecycle

Single `Config::load(vault_root: &Path) -> Result<Config>` in `core::config` is the only entry point. Order:

1. Read user config (best-effort: missing file → empty TOML).
2. Read vault config (best-effort: missing file → empty TOML).
3. Parse each into the same `Config` struct independently. Either parse failure aborts (with the offending file named).
4. Deep-merge user under vault per the rules above.
5. Validate cross-field invariants (e.g. `tree.sort_by` is one of the known values; `model` is the supported value). Failures abort.
6. Return the frozen `Config`.

The host calls this once inside `open_vault_at` (alongside `init_tracing` per `obs-tracing-baseline`) and stashes the result in per-vault state as `Arc<Config>`. CLI and MCP entry points call the same `open_vault_at` helper. No mutation, no `RwLock`. [settings-load-once-at-startup]

Open-a-different-vault re-runs `Config::load` against the new vault root. The old `Config` is dropped; in-memory UI state (which view toggles are flipped, which panels are open) does *not* automatically reset to the new defaults — the app's existing vault-swap reset path handles what should re-init.


## Write-back

Selected in-app UI changes persist by writing back to the appropriate TOML file. The set of write-back-eligible keys is fixed (it's exactly the set with a real-time UI control); arbitrary keys are not user-mutable from inside the app in v1. Eligible keys:

- `[editor].render_txt_as_markdown`, `live_preview`, `word_wrap`, `show_line_numbers`, `show_whitespace`, `highlight_trailing_whitespace`, `show_chunk_boundaries` — written on View menu flip. Vault-scope.
- `[vault].sidebar_open`, `related_open`, `trash_expanded`, `tree.sort_by` — written on the corresponding UI action. Vault-scope.
- `[vault].recent` — written by the Open Vault flow (push-to-front, dedupe, cap at ~10 entries). User-scope.
- `[mcp.tools].review_required` — bool toggle in the MCP server settings section. When on, MCP write tools route through staging. User + vault scope. Live-applied.
- `[llm.background].review_required` — bool toggle in the LLM settings section's background subsection. When on, background features write to staging. User + vault scope. Live-applied.
- `[op-log].auto_reject_on_drift` — bool toggle in the Op-log settings section. When on, pending ops that become drifted auto-flip to `rejected`. User + vault scope. Live-applied. Per `op-log-config-section`.
- `[op-log].metadata_retention_days` — int (>0) in the Op-log settings section. GC age threshold for accepted-op metadata rows (the Yrs Doc content lives indefinitely). User + vault scope. Live-applied.
- `[op-log].rejected_retention_days` — int (>0) in the Op-log settings section. GC age threshold for rejected ops (defaults shorter than accepted retention). User + vault scope. Live-applied.
- `[op-log].review_required` — bool toggle. Default policy for agent-authored ops. Surface-specific overrides win. User + vault scope. Live-applied.

Single command `set_setting(scope: SettingsScope, key: String, value: serde_json::Value) -> Result<()>` is the only write path, where `scope` is `User` or `Vault`. Each call:

1. Validates the key is in the eligible set for the requested scope (rejects everything else with `not user-mutable in v1`).
2. Validates the value's type against the field's declared type (a `bool` won't accept `"true"`).
3. Loads the target file via `toml_edit` (preserves comments and key ordering); applies the change in place; atomic write-then-rename.
4. Updates the in-memory `Arc<Config>` so subsequent reads see the new value without a re-load.

**Watcher coordination.** Writes go to paths under `.hiker/` (vault TOML) or under the platform config dir (user TOML). The vault TOML path is already covered by `watcher-ignore-hardcoded`'s `.hiker/` rule, so write-back never re-enters as a watcher event. The user TOML is outside the vault entirely. No new suppression infrastructure needed.

**External edits while running.** A hand-edit to a TOML while the app is open keeps the old in-memory values until the next `set_setting` (no proactive hot-reload, per `settings-load-once-at-startup`). That `set_setting` writes through `toml_edit` overwriting only the changed key, so manual edits on other keys are preserved; last-writer-wins on the same key. After writing, `set_setting` reloads the full merged Config, so unrelated hand-edits land in memory at that point as a side effect — but this is *not* a hot-reload guarantee: a user who only hand-edits and never flips a toggle sees no change until restart.

[settings-write-back]


## Default vault auto-open

On app startup, if `vault.default` in the user TOML is set and non-empty, the app opens that path directly without showing the vault picker. Empty or absent → show the picker as today. [settings-default-vault-autoopen]

**The picker stays in the app layer, not core.** The native folder picker lives in the egui app (a CLI invocation must never spawn a dialog) and fires only when a folder is needed. Backend exposes `open_vault_at(path)` for the open work — `Vault::open` + `init_tracing` + `Config::load` + indexer/watcher spin-up + `vault.recent` push — the same helper the CLI / MCP entry points call; no dialog dependency in core or host.

**App bootstrap.** On window init:

1. Read `vault.default` from the user TOML via `get_default_vault()` (a value lookup, not a side-effecting "try to open").
2. If non-empty, call `open_vault_at(path)`. On `HikerError::NotFound`, `warn!` server-side, non-fatal toast (`"Default vault at <path> not found — pick a vault"`), fall through to step 3. **Do not** auto-clear the setting (drive may be unplugged — clobbering on a transient failure is wrong). If the path exists but `Vault::open` fails (permissions, schema mismatch), surface the real error via the same alert as the manual-open path rather than masking it as "no default."
3. If empty or step 2 fell through, open the native folder picker and call `open_vault_at` with the chosen path.

**No first-run interaction.** On a brand-new install `vault.default` is `null`, so the bootstrap falls through to the picker. The "make this the default vault" UI action is deferred (see "Deferred"); until then, hand-edit the user TOML.

**Reading `vault.default`.** Documented as user-only but not enforced (per "Merge & precedence"). Bootstrap reads the user TOML directly — it's the only file available before a vault is open. `vault.default` in a vault TOML is silently meaningless.


## Module placement

- `core::config` — `Config` struct, all section structs, `Config::load`, `Default` impls. Pure, no host imports. Mirrors the `core::store` / `core::embed` discipline from `index.md`.
- TS types auto-exported via `ts-rs` per design.md so the frontend reads `Config` shape directly without manual duplication.
- No other module reads `*.toml` directly. If a value is needed somewhere, the path goes `Config::load` → struct field → caller; not "open the file again over here."


## Deferred

Real, considered, explicitly not v1.

- **Hot-reload.** Watch the two TOML files and re-apply on change. `set_setting` already keeps in-memory and on-disk state in sync for in-app flips; external hand-edits while running are rare enough that "restart to apply" is acceptable.
- **Expanding the write-back-eligible key set.** New eligible keys are added one-at-a-time as new UI controls appear (theme picker, model picker). Generalized "any key, anywhere" write-back isn't planned.
- **Per-key user-vs-vault scope enforcement on read.** The loader accepts any key in either file. If real misuse appears, add a per-field scope tag and reject misplaced keys at parse time. Write-back already routes per scope.
- **Auto-rewrite of existing TOML files when keys are added.** `serde(default)` fills new keys transparently; auto-injecting them with comments would touch user files on every launch. "Delete the file to regenerate" is the workaround.
- **Theme / font.** Need a UI to be discoverable. Land alongside the settings UI shell.
- **`[autosave]` config section.** Crash-recovery autosave (`autosave.md`) ships with a hard-coded 5s tick and no knob; the section lands if a workflow asks for `tick_secs` / `enabled` / on-blur-only. Strict-load and write-back already cover the shape.
- **Sync of settings across machines.** Per-vault settings travel with the vault under Syncthing; the per-user TOML doesn't sync and shouldn't (recent vaults, default vault, machine-specific paths).
- **Backward-compatibility shims for old TOML shapes.** Pre-real-use we delete and re-create; post-real-use we ship per-version migrations. No "load old shape into new struct" code in between.
- **`hiker config get/set` CLI.** `vim ~/.config/hiker/config.toml` plus in-app write-back covers the v1 need.
- **"Did you mean X?" hints on unknown keys.** The hard error already names the bad key plus file. A near-match suggester is polish-grade.

**Out of scope (not deferred — never):** a web-based settings UI or remote config (Hiker is local-first); per-note settings beyond frontmatter; settings inheritance across vaults (each vault is independent); encrypted settings (no secrets in `config.toml` — cloud embedder API keys, if they land, get keychain-backed storage, not a TOML field).
