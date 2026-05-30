# Settings

Configuration surface for Hiker. v1 ships a TOML loader and the section content needed to unblock the deferred toggles in `editor.md`, `txt-ingest.md`, and `index.md`. No settings UI in v1 — the file is the surface, and a relaunch picks up changes.

- **Two TOML files.** Per-user `config.toml` at the platform config dir, per-vault `vault/.hiker/config.toml`. Vault overrides user key-by-key. [settings-user-config-toml, settings-vault-config-toml]
- **Read once at startup.** No watcher, no hot-reload, no per-access reread. Restart to apply. Struct built once and handed around frozen. [settings-load-once-at-startup]
- **Strict load.** Unknown key or type mismatch aborts startup with a `file:line` error. Same fail-loud discipline as `store-version-fail-loud` in `index.md` — silently dropping a user-set value is worse than refusing to start. [settings-strict-load]
- **Defaults live in Rust, files auto-create on first load.** Every field is `serde(default)`; the loader treats a missing file as empty *and* writes a defaults-populated TOML so users have a self-documenting file to edit. [settings-defaults-in-code, settings-auto-create-defaults]
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
- Arrays replace, not concatenate. User `indexing.ignored_paths = ["foo/"]` plus vault `["bar/"]` = `["bar/"]`. Concatenation reads as merging but is surprising (a user can't *remove* an inherited entry). Replace is honest.
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

Font family rides the three font slots above per `editor-three-fonts`; per-slot **size** is intentionally excluded from v1 (theme tokens own size) and is listed in "Deferred" below. Theme is also deferred. Crash-recovery autosave (`autosave.md`) is on with a fixed 5s tick and has no config surface in v1; an `[autosave]` section can land later if a workflow asks.

### [indexing] [settings-section-indexing]

Indexer tunables. Vault-level only is the natural scope (different vaults have different content shapes), but a user-level default is fine for the common case.

| Key | Type | Default | Notes |
| --- | ---- | ------- | ----- |
| `model` | string | `"bge-small-en-v1.5"` | Embedder model id. One of `bge-small-en-v1.5` (default, 384-dim, English), `bge-m3` (1024-dim, multilingual, 8k context), `embedding-gemma-300m` (768-dim). Strict-load rejects anything else. Bumping forces full re-embed via `embedder-version-tag`; a dim change additionally rebuilds the vec0 table via `store-rebuild-chunk-vecs-on-dim-change`. UI surface: interactive dropdown gated by `settings-embedder-model-change-warning` |
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

Cancel is default-focused; the action button carries the standard accent treatment used for other consequential confirms (`confirm3-real-modal`). The "Dim change" bullet is only rendered when the new model's dim differs from the current one — same-dim swaps (e.g. a hypothetical future 384-dim model) omit it.

Body wording rules:
- Names the *current* model and the *new* model verbatim, so the user can verify they're switching the thing they think they're switching.
- States the re-embed consequence in plain language; no jargon about `embedder_version` or vec0.
- The time estimate is a qualitative range, not a computed estimate — vault size and CPU vary too widely for a number to be honest.

Behavior on Confirm:
1. `set_setting("indexing.model", new_id)` writes through `toml_edit` per `settings-write-back`.
2. The indexer hot-reloads the embedder in place — no app / vault restart. A new `IndexJob::ReloadEmbedder { model_id }` job goes through the existing mpsc channel; the indexer task loads the new model on `spawn_blocking`, swaps it into the live `Arc<dyn Embedder>` (also visible to the search-query embedder via the `OnceCell` per `search-query-embed-spawn-blocking`), calls `Store::ensure_chunk_vecs_dim(new_dim)` to run any rebuild, then enqueues a vault-wide reindex. [embedder-hot-reload-on-model-change]
3. The store's dim-check inside that handler (`store-rebuild-chunk-vecs-on-dim-change`) catches the dim mismatch and rebuilds `chunk_vecs` before the first new batch lands.
4. Status bar reflects the re-embed progress via the existing `status-bar-index-label` machinery, with a transient "Loading <model>…" sub-state while the new model weights download / load.

Behavior on Cancel: the dropdown reverts to the previous value; no write occurs.

The warning is only on the **settings UI** path. Hand-edits to the vault TOML don't surface a warning — strict-load picks up the new value on the *next launch*, the embedder loads at the new model id, and the re-embed runs. (Hot-reload is a UI-driven path only; the TOML hand-edit case is rare enough that "restart to apply" is acceptable, same posture as the rest of `settings-load-once-at-startup`.) Documented in the inline "Notes" comment of the auto-created TOML.

### [extract] (deferred — lands when `core::extract` ships) [settings-section-extract]

> **Deferred — not yet loaded.** There is no `ExtractConfig` field on the
> strict-load `Config` struct (`core/src/config/mod.rs`), so a well-formed
> `[extract]` table in a live vault's TOML currently aborts vault open with a
> `HikerError::Config` ("unknown field") under `settings-strict-load`. The
> schema below is the *planned* shape; it lands with the extract feature. Until
> then, do not add an `[extract]` table to a vault TOML.

Extraction tunables (`extract.md`). Vault-level is the natural scope (which folders hold extractable sources is per-vault), with a user-level default fine for the common case.

| Key | Type | Default | Notes |
| --- | ---- | ------- | ----- |
| `auto_globs` | array of strings | `[]` | Folders/globs whose non-md sources auto-extract on appear/change (`extract-trigger-auto-glob`). gitignore-style globs over vault-relative paths; replaces, doesn't concatenate, per the array-merge rule. Default empty = no auto-extraction; non-md elsewhere extracts only on the explicit "Make searchable" action (`extract-trigger-on-demand`) |

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


## Pending change review

Some writes shouldn't land directly on disk: the user didn't author the bytes (agent writes), or the user can't watch them all happen at once (batch mutations). These writes land as `status=pending` ops in the op log (per `op-log.md`) until the user accepts. Accept/reject lives inline on every surface that cares — the chat card where the proposal originated, the trails panel for draft-trail proposals, the file tree for per-file proposals, the editor toolbar when the open file has a pending op, and the activity detail page as the central review surface. No dedicated staging editor sub-mode — accept/reject lives alongside the content it affects.

Two flows produce pending ops:

- **Agent writes (opt-in)** — MCP tool calls and background features are gated by `review_required` flags; flipping them on makes the produced ops enter the log as `status=pending` instead of `status=accepted`. [agent-write-review-mode]
- **Batch mutations (always pending)** — multi-note mutation actions (e.g., "reformat every `.txt` in `inbox/`") fan out N tasks per `task-queue.md`; the user can't watch N buffers, so each result lands as pending unconditionally. Single-note user-initiated mutations stay in-buffer per `editor.md`'s Note-mutations menu.

The headline decisions:

- **Pending ops live in a per-document queue, not a separate staging database.** Agent ops are serialized Yrs updates in `<doc-id>.pending` (per `op-log-pending-queue`); accept applies them to the document's `accepted` CRDT state, reject discards them. Closing the app and reopening reconstitutes pending ops naturally — the queue is on disk. [op-log-pending-survives-restart]
- **Agent-write review is off by default; opt-in per write surface.** Existing behavior — agent ops enter as `status=accepted` and reach disk immediately — stays the default. Users who want a checkpoint in the loop flip the per-surface `review_required` flag. [agent-write-review-mode]
- **`edit_note` calls emit multiple `Replace` ops sharing a `batch_id`** so consumers can group them visually but accept/reject each independently. `write_note`, `set_frontmatter`, and `apply_tag` emit one op per call (whole-document `Replace`, `SetFrontmatter`, and `SetFrontmatter`-on-tags respectively). Op shapes specced in `op-log-op-shape`.
- **`move_note` and `rename` flow through dedicated op kinds** per `op-log-op-shape` (`Rename` for path moves; the indexer logic that calls `core::vault::move_note` is unchanged from the prior design). Triage and the cluster editor's one-off Stage move verb emit `Rename` ops with `author = "auto:triage"` / `"auto:cluster-editor"` respectively.
- **Drifted ops** (an anchor's `old_str` no longer resolves uniquely against the accepted materialization) surface with Accept disabled, Reject active. See `patch-review.md` for the inline drift surface. [op-log-status-states]
- **Accept/reject is integrated into every relevant surface, not a separate editor mode.** Each surface renders its own accept/reject inline: chat tool-call cards, the trails panel, the file tree context menu, the unified `Changes` tab. Hunk-shaped pending ops render directly in the live editable buffer as inline decorations + per-file pill (see `patch-review.md`). Whole-file pending ops open via the existing read-only-buffer-with-diff-toggle pattern, framed as "Review rewrite" / "Review new note" in the mode-controls label (per `write-note-review-surface`).
- **The activity detail page is the central review surface.** The existing `vault-home-recent-activity-detail` page gains a "Pending" filter pill (alongside the existing author-class pills) that shows all pending ops across all sources. Each row carries [Accept] [Reject]. An [Accept all (N)] button at the top batch-approves. [staging-review-activity-detail-filter]
- **Pending ops are queryable by surface-specific filters.** `core::oplog::query()` accepts a filter (`by_path`, `by_trail_id`, `by_surface`, `by_session_id`, `status`) so each surface pulls only its relevant ops.
- **MCP write responses are honest about pending.** When review mode is on, MCP write tools return success-with-pending — the JSON response carries `status: "pending"` plus the op id so the agent can describe the outcome accurately. [staging-review-pending-response]
- **Accept navigates to the target note as a preview tab.** After a successful individual accept (not batch), the UI opens the affected note at its target path in an editor tab with `preview: true`. Batch accept (`Accept all`) stays on the current surface. [staging-accept-navigates-to-preview]

### Review surfaces

Five surfaces plus the central activity-detail page. One proposal may appear on multiple surfaces (e.g., an agent write appears on the chat card that produced it AND on the activity detail page AND on the file tree row for the affected file). Accept from any surface removes it from all.

#### Surface 1: Chat panel tool-call cards

When an agent proposes a write in review mode, the tool-call card appends two small text links inline:

```
▸ write_note(research/paper.md) ✓ Proposed  [Accept] [Reject]
```

Accept → drift-checked write → navigates to the target note as a preview tab. Card updates to `✓ Applied`. Reject → discard → card updates to `✗ Rejected`. Same proposal also appears on the activity detail page and the file tree; accepting from the chat card removes it everywhere. [staging-accept-reject-from-chat-card]

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

Pending proposals are **merged into the file tree at their target path**, not isolated in a separate panel:

- **New files** (the target path does not yet exist on disk) appear in the tree as synthetic file rows at their destination folder, rendered with a **greyed name** so the user can see where the proposal will land if accepted.
- **Changes to existing files** show the **same dirty indicator** (the suffix dot) already used for dirty buffers. The dot is the universal "this file has something unresolved" signal — dirty buffer, pending proposal, same visual.
- **Synthetic directories** are created when a proposal targets a path inside a folder that doesn't exist yet (e.g. `newfolder/file.md`). The folder row appears greyed and can be expanded to reveal the staged children.
- The tree refreshes automatically on staging-snapshot updates so proposals appear and disappear as they are accepted or rejected from any surface.

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

"Review pending change" opens the file as a read-only buffer with the diff toggle (reuses `snapshot-preview-mode`'s existing diff pattern). Accept and Reject work from the menu without opening the file. [staging-accept-reject-from-tree]

#### Surface 4: Editor toolbar

When the open buffer has a pending proposal, a small pill appears in the editor toolbar's right-side cluster, just left of Save (same placement and visual weight as the existing "Add to trail" pill):

```
Proposed change — [Accept] [Reject]
```

Hidden when there's no proposal for the active file. Accept drift-checked-writes and removes the proposal from all surfaces. If the active buffer is the target file, the buffer reloads from disk; otherwise navigates to the target as a preview tab. Reject discards. The pill does not open a separate preview — the user is already looking at the file on disk. [staging-accept-reject-from-editor]

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

Click a pending row → opens the editor tab in diff mode with `DiffSource = StagingProposal(id)` (per `diff-as-mode`). Accept → navigates to the target note as a preview tab. Reject → returns to the activity detail page. [Accept all (N)] at the top runs a confirm-then-batch-apply (stays on current surface). [staging-review-activity-detail-filter]

#### Surface 6: Queue button (combined badge)

The existing queue button in the top strip shows a combined badge: tasks + pending reviews. Click opens the queue detail page (unchanged). The review count is folded into the badge — users who want the full list go to the activity detail page. [staging-review-top-bar-badge]

### Button styling

No green/red. All Accept/Reject use the existing muted token system and the same text-link weight as the `[Restore this version]` link in the activity detail:

- **Accept** — `var(--accent)` muted outline, same color family as active states elsewhere. Low-opacity fill on hover.
- **Reject** — `var(--danger-text)` muted outline. Keeps the danger semantic but doesn't scream.
- **Accept all** — same accent outline with "all" label.

### Storage

Accepted state lives in `.hiker/oplog/<doc-id>.yrs` (the document's Yrs CRDT state); pending ops queue separately in `<doc-id>.pending` as serialized Yrs updates paired with side-table metadata. Storage layout, op shapes, drift detection, and the layered model are all specced in `op-log.md` (`op-log-store-layout`, `op-log-op-shape`, `op-log-layered-model`, `op-log-status-states`). The `[staging]` config section is replaced by `[op-log]` per `op-log-config-section`.

There is no separate staging database, no `staging.db` schema, no proposal table. Pending ops are queryable via `core::oplog::query({ status: Pending, ... })` — the same query surface every other consumer reads.

### Lifecycle of a pending op

1. **Producer emits an op via `core::ops`.**
    - **Agent path** — `mcp-tool-write-note` / `mcp-tool-edit-note` / a background feature emits ops via `core::ops::agent_*`. When `review_required` is on for the producer, the ops queue in `<doc-id>.pending` instead of applying to `accepted`. `edit_note` queues N `Replace` ops sharing a `batch_id`; `write_note` queues a single whole-document `Replace`; `set_frontmatter` and `apply_tag` queue `SetFrontmatter` ops (per `op-log-op-shape`). Validation (anchor uniqueness, no textual overlap, anchors resolve against pre-application content) happens once at the producer per `mcp-edit-note-validation`.
    - **Batch mutation path** — each fanned-out task in `core::tasks` queues its result op on completion.
2. **`OpLog::stage_pending` writes the op to `<doc-id>.pending`** and broadcasts op-log change events. The pending op is now durable across restarts.
3. **All active surfaces refresh on the next frame.** Chat cards, trails panel, tree row indicators, in-buffer hunks, write-note pending banner, Changes tab. They read pending ops via `core::oplog::query` filtered to their relevant scope.
4. **Drift detection.** Every op that advances `accepted` re-derives drift for the outstanding pending ops: hiker tries to apply each queued Yrs update to a clone of current `accepted`. A `Replace` whose `AnchorHint` no longer resolves uniquely (per `op-log-op-shape`) is drifted, surfaced as `(M drifted)` in the file pill (per `patch-review.md`). Drift is a derived signal, not a status — recomputed from the queue on demand.
5. **User accepts from any surface** → `core::ops::flip_op_status(op_ids, Accepted)` applies the queued Yrs updates to `accepted`, writes their `op_metadata` rows, and re-runs save-to-disk per `op-log-atomic-write`. Reject → `flip_op_status(op_ids, Rejected)` drops them from the queue and writes a rejected audit row. Accepted ops join the synced CRDT state; rejected ops never reach `accepted`.
6. **Auto-reject on drift.** When `[op-log] auto_reject_on_drift = true`, an op that becomes drifted is rejected automatically with `metadata.auto_rejected_reason` set. Per `op-log-config-section`.
7. **Retention.** Accepted-op metadata GCs per `[op-log] metadata_retention_days`; rejected audit rows GC per `[op-log] rejected_retention_days`. Pending ops are never auto-GC'd — they sit in the queue until the user resolves them.

### `core::oplog` module

The substrate API lives in `core::oplog` per `op-log.md`'s "Module placement." Producer-facing helpers (`agent_write_note`, `agent_edit_note`, `agent_set_frontmatter`, `agent_apply_tag`, `flip_op_status`) live in `core::ops`. UI surfaces call `core::oplog::query(filter)` with their relevant filter, not the producer helpers.

Event: op-log change events broadcasts on every append / status-flip so all surfaces stay in sync.

### Where the config keys live

Two kinds of keys: per-surface gates (in their owning sections), and log-level behavior (in the `[op-log]` section specced in `op-log.md`).

- **`[mcp.tools].review_required`** (bool, default `true`) — extends `mcp-config-section`. When true, every MCP tool-write enters the log as `status=Pending` instead of `Accepted`. Live-applied. Exposed as a bool toggle in the MCP settings UI section alongside the per-tool toggles.
- **`[llm.background].review_required`** (bool, default `false`) — lands with the v3.5 `[llm]` section. When true, debounced background features emit pending ops instead of mutating frontmatter directly.

Batch mutations don't have a `review_required` flag — they always emit pending ops.

Log-level behavior (`auto_reject_on_drift`, `metadata_retention_days`, `rejected_retention_days`) lives in `[op-log]` per `op-log-config-section`.

### Forward refs

- `op-log.md` — the substrate; storage, op shape, status states, materialization, drift, config section.
- `diff.md` — the diff primitive used for whole-file and per-hunk review.
- `patch-review.md` — owns the inline per-hunk accept/reject surface for `Replace`-shaped pending ops plus the write-note review surface for whole-file pending ops.
- `mcp.md` `mcp-config-section` gets the `tools.review_required` row; `mcp-tool-edit-note` is the producer of per-edit `Replace` ops.
- `llm.md` `[llm.background]` config gets `review_required`.
- `task-queue.md` — batch mutation fan-out producers append pending ops on completion.
- `trails.md` — draft trail review hooks into the surfaces described here.
- `op-log.md` — accepted ops surface via the unified activity feed; `metadata.agent_op_id` carries the originating op for traceability.


## Settings UI shell

A vault-bar gear button toggles a settings surface that replaces the editor (same shape as the vault home page — a sub-mode of the editor pane state). The TOML files stay canonical; the panel is a UI on top of the existing `set_setting` / `Config::load` infrastructure, not a parallel storage path. [settings-pane-mode]

- **Pane mode, not modal.** The settings surface is a sub-mode of the editor pane, alongside the vault home overview and home detail. Clicking the gear swaps `#editor-pane` to a settings layout; clicking any tree row / recents row / search result returns to the editor on that note. Same shape `setVaultHomeVisible` already uses today, with the same dirty-buffer protection (a dirty editor buffer gets the existing `file-switch-guard-dirty` modal before the swap). [settings-pane-mode]
- **Gear icon in the vault bar between Home and Open-vault.** `vault-bar` order becomes Home / Settings / Open-vault, then the vault path display, then back/forward at the trailing edge. Same icon-only treatment as the existing vault-bar buttons (gear / cog glyph in the established line-weight family). Pressed/unpressed state reflects whether the settings pane is currently visible. Tooltip "Settings." [vault-bar-settings-icon]
- **A long scrollable page, sections stacked.** The body mirrors `vault-home-overview`: a header with the vault name + scope toggle, then a stack of section cards — one per `[section]` in the loaded config. Each card is a list of rows; each row is one config key with its current value, an inline control, a "reset to default" affordance, and a one-line description. No left-rail tabs in v1 — a single scrollable page is honest about the v1 surface size and matches the home page's vertical stack. [settings-pane-section-list]
- **Eligible keys are interactive; everything else is read-only with a "edit TOML" affordance.** The write-back-eligible key set (the closed list in `core::config`) drives which rows get a live control vs. a read-only display. Read-only rows show the current value in a muted code style with an "Open `config.toml`" link that opens the TOML in the system file manager via the existing `reveal_in_file_manager` command. Same fail-loud / restart-to-apply model as today — non-eligible keys can't be written through `set_setting` for safety, the TOML is the source. [settings-pane-eligible-key-controls, settings-pane-readonly-display]
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

- **Entering settings.** The gear button calls `setSettingsPaneVisible(true)` (same shape as `setVaultHomeVisible`), which swaps `#editor-pane` to the settings layout and renders the cards from the current `Config`. If the editor buffer is dirty, the existing `file-switch-guard-dirty` modal fires first.
- **Live updates.** Every interactive flip calls the existing `set_setting` command. The command already updates the in-memory `Arc<Config>` and writes through `toml_edit`; the UI re-renders the affected row from the new value. No separate "save" button.
- **External edits.** If the user hand-edits a TOML while the settings pane is open, the displayed values are stale until the next `set_setting` (which reloads the merged Config as a side effect, per existing `settings-write-back` semantics) or until the user closes and reopens the pane. A small "Refresh" affordance in the header forces a reload without making a write — calls a new `reload_config` command that re-runs `Config::load` and re-renders. Cheap to add; lands when a user actually hits the staleness case in real use. [settings-pane-manual-refresh]
- **Exiting settings.** Clicking any tree row, recents row, search result, or the Home button exits settings the same way it exits home — no save protection needed (nothing is dirty in the settings pane; every flip is committed).
- **Navigation history.** Entering settings is a content-surface change; pushes onto `navigation-history-stack` like home does. Back returns the user to wherever they were.

### Keybind

Reserves `settings.open` in `keybind-registry`. Chord: `Cmd-,` on macOS (matches every macOS preferences convention), `Ctrl-,` elsewhere. Toggles the settings pane open/closed. Lands when the keybind is wired; the registry entry is the seam. [settings-pane-keybind]


### Out of scope (this surface)

- **Theme / font / color-scheme.** Visual customization needs a theme system first; settings UI doesn't ship its own. Lands when theming is real.
- **Live keybind editor.** The `[keymap]` section is a stub — the loader doesn't yet read keybind overrides per `settings-section-keymap`. Settings UI shows the section header with "no overrides yet" and a one-line pointer to `keybind-registry`. The full editor lands with the loader.
- **LLM provider / model picker.** Lives behind the deferred `[llm]` section; the settings UI shows the section as deferred. Lands with `llm-providers-config`.
- **Cloud / Ollama embedder provider picker.** The `[embedder]` *provider* section stays deferred until `embedder-llm-crate-backed` lands. Within-fastembed model selection (bge-small / bge-m3 / embedding-gemma-300m) is live in `[indexing].model` per `embedder-model-selectable`.
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

**External edits while running.** If the user hand-edits a TOML file while the app is open, the in-memory `Config` keeps the old values until the next `set_setting` call (no proactive hot-reload, per `settings-load-once-at-startup`). A subsequent `set_setting` writes through `toml_edit` so the user's manual edits are preserved; only the changed key is overwritten. The "external edit + in-app flip happen on different keys" case works correctly. Last-writer-wins on the same key is acceptable for v1 — concurrent vim + UI edits to the same setting is not a workflow worth designing for.

`set_setting` reloads the full merged Config after writing, so unrelated hand-edits to other keys land in memory at that point as a side effect. This is intentional — it keeps the in-memory and on-disk views from diverging on keys neither the spec nor the user expected to interact — but is *not* a hot-reload guarantee. A user who only hand-edits and never flips an in-app toggle does not see their changes applied until restart.

[settings-write-back]


## Default vault auto-open

On app startup, if `vault.default` in the user TOML is set and non-empty, the app opens that path directly without showing the vault picker. Empty or absent → show the picker as today. [settings-default-vault-autoopen]

**The picker stays in the app layer, not core.** A CLI invocation should never risk spawning a folder dialog, so the native folder picker lives in the egui app and is invoked only when a folder is actually needed — never in `core`. Backend exposes `open_vault_at(path)` for the actual open work — `Vault::open` + `init_tracing` + `Config::load` + indexer/watcher spin-up + `vault.recent` push. Same shared helper the CLI / MCP entry points call; no dialog dependency in core or the host layer.

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
