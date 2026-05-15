# Auto-organization suggestions

Hiker's clustering pipeline (see `clustering.md`) is consumed as a *recommendation engine*, not as durable organizational infrastructure. The user owns the vault's structure; the AI suggests improvements; neither pretends to own the layout.

Two flows live on top of the same engine:

- **One-shot suggestions** — user runs "Suggest reorganization," reviews a markdown proposal, picks what to apply. Tree is ephemeral; nothing persists except the user's accepted actions and a small rejection log.
- **Saved-tree triage** — user saves a generated tree as a classifier; new notes get auto-routed against it (auto-apply on high confidence, queued for review on medium, left alone on low).

The headline decisions:

- **`trees.db` is the single source of truth for cluster trees.** Owned by `core::trees` (per `cluster-editor.md`); both Sapling (one-shot) and Evergreen (saved-triage) trees live as rows there. The filesystem stays the source of truth for note placement — trees are a recommendation surface, not durable organizational infrastructure. [suggestions-one-shot-flow]
- **Markdown is an optional export, not a persisted format.** The cluster editor's markdown view (per `cluster-editor-markdown-view-toggle`) renders trees on demand from `trees.db` rows. A `hiker suggest export <tree-id> [--out <path>]` CLI command writes the same rendered markdown to a file for sharing / audit / offline review; the export is one-way and is not parsed back on Apply. [suggestions-markdown-export]
- **The cluster editor is the primary review surface.** Structured tree view + the batch-review pane (per `cluster-editor-batch-review-pane`) are where users decide what to apply. The markdown view + export are escape hatches for text-editing or sharing — never authoritative.
- **Per-leaf granularity at Apply time.** Each leaf with a resolved `Tag` or `Move` policy produces one `staging.db` row; the user accepts/rejects rows independently from the batch-review pane.
- **Two output modes per suggestion: move or tag.** Move = filesystem rename into a (possibly new) folder. Tag = write to the note's frontmatter, no fs change. Configurable per cluster, overridable per note. [suggestions-mode-move, suggestions-mode-tag]
- **Saved-tree triage runs per-node policies (per `cluster-editor.md`), not a single confidence threshold.** Confidence is still computed by the matcher but applies as a per-policy parameter on the matched node rather than a global threshold.
- **Triage matches flow through `staging.db`.** Every match — `auto-move`, `auto-tag`, or `review` — produces a `staging.db` row with `surface = "triage"` and the resolved policy mapped to `action = "move_note"` or `"apply_tag"`. Whether the row auto-accepts or waits for the user is gated by `[suggestions.triage].review_required` plus the policy type: `auto-*` policies auto-accept when the flag is off; `review` policy always waits for explicit user accept. The activity-detail page's Pending filter (`staging-review-activity-detail-filter`) is the single review surface — `surface = "triage"` is a filter chip there, not a separate panel. [triage-staging-proposals, triage-review-required]


## One-shot suggestions (Sapling)

User invokes "Suggest reorganization" — from the sidebar's Cluster trees mode (per `cluster-editor-new-tree-action`) or via `hiker suggest` on the CLI. The system:

1. Resolves the build inputs (scope / method / params) via the clustering review tab (`cluster-review-tab-config-section`) for the UI path or CLI flags (`hiker suggest --scope vault --method cluster --algorithm hdbscan ...`).
2. Runs the clustering build pipeline (`clustering.md`) and inserts the resulting tree into `trees.db` with `state = 'draft'`, `source = 'one-shot'`.
3. UI: opens the new tree in the cluster editor's expanded view, ready for review and policy assignment. CLI: prints the new tree's id and a one-line summary; users open it in the UI to review, or use `hiker suggest show <tree-id>` to dump a markdown rendering to stdout.
4. User edits policies / reshapes / sets per-leaf overrides, then clicks Apply (UI) or runs `hiker suggest apply <tree-id>` (CLI).

The flow is non-interactive at the engine level — no streaming, no partial application during generation. The tree lands as a complete object in `trees.db`; the user takes their time. Reconcile is *not* an interactive operation in the sub-second sense (`clustering.md`'s cost model puts a 10k-note vault at ~30s). [suggestions-one-shot-flow]


## Markdown rendering

The cluster editor's markdown view (`cluster-editor-markdown-view-toggle`) and the `hiker suggest export` CLI command both render a tree from its `trees.db` rows into the format below. The format is **read-only** with respect to the tree — edits to an exported markdown file are not parsed back. Edits happen in the cluster editor (UI) or via `hiker suggest set-policy / hiker suggest move` CLI commands that mutate `trees.db` directly. [suggestions-proposal-md]

```markdown
# Cluster tree — 01HXP7Z…  ·  2026-05-06 15:42  ·  draft

Scope: Vault   Method: Cluster (hdbscan)   24 clusters · 142 leaves · 7 outliers

## Embedding research  ·  confidence 0.91  ·  policy: move → `research/embeddings/`

- `inbox/whisper-notes.md`
- `inbox/voyage-vs-bge.md`
- `notes/random-thought-on-vec.md`

## Project: dishwasher  ·  confidence 0.78  ·  policy: tag `project-dishwasher`

- `inbox/whirlpool-error-codes.md`
- `inbox/dishwasher-pump-replacement.md`

## Mixed cluster  ·  confidence 0.62  ·  policy: none

- `inbox/protein-folding.md`
- `inbox/llm-jailbreaks.md`
- `inbox/coffee-roasting.md`
```

The rendering carries the tree's id, build timestamp + state, scope + method, member counts, per-cluster confidence + policy, and the leaf list per cluster. Outliers render as their own section. There are no checkboxes — Apply is driven by the tree's policies, not by checking the rendering.

**Export.** `hiker suggest export <tree-id> [--out <path>] [--format md|json]` writes the rendering to a file. Useful for audit, sharing a tree with a teammate, or offline review on a different device. The export is one-way; importing back requires `hiker suggest import <path>` which creates a fresh `trees.db` entry — there is no "diff this exported file against my current tree" path in v1. [suggestions-markdown-export, suggestions-tree-export-cli]


## Apply

`hiker suggest apply <tree-id>` (and the Apply button in the cluster editor) walks the tree's leaves, resolves each leaf's effective policy via the walk-up rule (per `cluster-editor-policy-resolution-walk-up`), and emits one `staging.db` row per leaf whose resolved policy is `Tag` or `Move`. The behavior matches the UI's Apply flow exactly — same `surface = "cluster-editor"`, same `metadata.tree_id`, same accept path (`suggestions-apply-cmd`). [suggestions-apply-cmd]

- **CLI Apply default is `--interactive`:** prints each staging row, prompts y/n/edit-target. `--accept-all` skips prompts; `--dry-run` lists what would be staged without inserting rows.
- **UI Apply opens the batch-review pane** (per `cluster-editor-batch-review-pane`); no extra prompts inline.
- **Frozen leaves and unpolicied leaves are skipped** — they're listed in the post-apply summary as `(N leaves skipped — no policy assigned, M leaves frozen)` so nothing is silently dropped.
- **Rejection bookkeeping.** When a staging row is rejected (CLI `n` answer, UI Reject), the rejection is recorded against `(tree_member_fingerprint, note_id, action)` in a small TTL'd rejection log (per `suggestions-rejection-history`) so a re-run of `hiker suggest` doesn't propose the same thing in the next reasonable window. [suggestions-rejection-history]

`tree_member_fingerprint` = stable hash of the leaf's member-set + parent cluster's name. Survives reshape if Jaccard ≥ 0.7 against the prior member set, in the spirit of the dropped `cluster-stable-identity` rule but applied only at the rejection-history layer, not as durable cluster IDs.

History bookkeeping is small — one row per rejected `(note, suggested-action)` pair, with a TTL so a route the user rejects in 2026 isn't suppressed forever. Default TTL: 90 days, configurable.


## Tag mode

Tag-mode is the alternative to file moves for users who prefer flat folders + rich metadata. Cluster names from LLM summarization (e.g. `"Embedding research"`) are slug-ified (`embedding-research`) and written to the note's frontmatter.

Default tag field: `hiker.suggested_tags` — a list of strings on the note's frontmatter, distinct from the user's own `tags:` field.

```yaml
---
hiker:
  suggested_tags: [embedding-research, vector-db]
tags: [research, todo]
---
```

Why a dedicated field by default: greppable (`rg "suggested_tags:"`), wipeable (mass-clear without nuking the user's own tags), and namespaced away from the auto-tag enrichment system in `design.md` (which has its own constrained vocabulary at `.hiker/vocabulary.yaml`). Two systems, both valid, distinct surfaces.

**Configurable.** Per-vault config flag at `vault/.hiker/config.toml`: [suggestions-tag-field-configurable]

```toml
[suggestions]
tag_field = "hiker.suggested_tags"   # default; set to "tags" to use the regular tag list
```

Setting `tag_field = "tags"` writes cluster tags to the user's main `tags:` list. For users who want one tag namespace and don't mind AI-suggested tags landing alongside their own, this is the simpler shape. The slug-ification rule applies regardless.

**Future integration with auto-tag.** The auto-tag enrichment pipeline (`design.md` enrichment section) has its own constrained-vocabulary system, confidence tiers, and review flow. Cluster-driven tagging is *adjacent* to it, not a replacement, and the two should converge later — likely with cluster names becoming new auto-tag vocabulary entries on user accept. Deferring that integration is intentional: the simpler shape ships first, the convergence story gets specced when both surfaces have real users.


## Saved-tree triage

The one-shot flow is for "I want to look at my whole vault and reorganize." Triage is the complementary mode: the user saves a tree as the active triage classifier, and from then on every new note in `inbox/` gets routed against it without re-clustering.

The save-as-triage entry point lives in the cluster editor (`cluster-editor-save-as-triage`); a `hiker suggest save <tree-id>` CLI parity command exists for headless workflows. The on-disk shape, draft persistence, per-node policy program, and the editor surface itself are specced in `cluster-editor.md`. This section covers the runtime — when triage fires and how its matches resolve to actions.

Triage runs through `core::tasks` (per `task-queue.md`) on three pathways:

1. **On save** — when a note inside the configured triage scope (default `inbox/`) is saved, hiker enqueues a `RaptorTriageMatch` task at `Normal` priority.
2. **Manually** — `hiker suggest triage [--scope inbox]` enumerates the scope and submits one task per note.
3. **Scheduled rerun** (opt-in) — `[suggestions.triage] scheduled_rerun` cron-shape config; `Low`-priority submissions over a configurable scope.

Each task runs the greedy centroid descent classifier (`cluster-place-beam-descent`), produces a target node + confidence, resolves the matched node's policy (per the per-node policy walk-up in `cluster-editor.md`), and emits a `staging.db` row carrying the proposed action. The classifier doesn't mutate the vault directly — every change rides the staging substrate. [triage-classifier-engine]

Mapping from resolved policy → staging row:

| Policy | `action` | Notes |
| ------ | -------- | ----- |
| `Move(folder, require_review)` | `move_note` | `source_path` = current note path; `target_path` = destination folder + basename. |
| `Tag(slug, require_review)` | `apply_tag` | Tag value carried in the row's content payload; merges into frontmatter on accept. |
| `Freeze` | (none) | No staging row emitted; the match is dropped. |

All rows carry `metadata.tree_id`, `metadata.matched_node_id`, `metadata.confidence`. Auto-accept gating composes the per-policy and global flags:

```
effective_requires_review = policy.require_review || config.review_required
```

- `effective_requires_review == false` — the staging row is created, accepted, and removed in one transaction. The accepted write lands in `core::changes` with `author = "auto:triage"`, `metadata.staging_proposal_id`, `metadata.auto_accepted = true`, `metadata.tree_id`, `metadata.matched_node_id` for traceability and rollback.
- `effective_requires_review == true` — the row stays pending. The user reviews via the activity-detail Pending filter (chip: `surface = "triage"`). On user accept, the changes-row author is `user` (the user owned the decision); on reject, the row deletes with no changes-row.

Accept reuses the existing apply mechanic (`suggestions-apply-cmd`): `move_note` rows call `core::vault::move_note(source, target)` (auto-creating target folders), `apply_tag` rows write to the configured `[suggestions] tag_field` on the target note's frontmatter. No new code paths.

**The "auto" in auto-organize is bounded.** Even in triage mode, the system will not produce a `move_note` row whose `source_path` is outside the configured triage scope (default `inbox/`). The check runs at staging-row insert time — a tree saved over `research/` whose triage scope is `inbox/` only emits `move_note` rows for notes whose current path is under `inbox/`. Notes the user has placed elsewhere are off-limits to triage — moving them is an explicit user action via DnD or `hiker mv`. The worst case for an over-eager classifier is "wrong subfolder under inbox," never "your important note got moved out from under you."


## Pinning (deferred)

Folder-level pin: mark a folder as "off-limits to suggestions" so neither Sapling Apply nor Evergreen triage propose moves *out of* it. Two natural surfaces:

- A `.hiker.yaml` sidecar in the folder.
- A `pinned: true` flag on the folder's row in a future `vault/.hiker/folders.yaml`.

Per-note pinning is not on the table — it's the inverse of the simplification this whole doc is about. If a user wants a single note to stick, they put it in a pinned folder. [suggestions-folder-pin]

Deferred until either flow has real users complaining about specific things drifting.


## `[suggestions.triage]` config section

Triage-level behavior. Both keys are eligible for user and vault scope; vault wins per the standard merge rule. [triage-review-required]

```toml
[suggestions.triage]
review_required = false      # auto-* policies bypass review when off; require accept when on
scope = "inbox/"             # source-folder safety boundary (also drives on-save trigger)
scheduled_rerun = ""         # cron-shape; empty disables (per cluster-editor-triage-scheduled-rerun)
```

| Key | Type | Default | Scope | Behavior |
| --- | ---- | ------- | ----- | -------- |
| `review_required` | bool | `false` | user + vault | When `true`, every triage match stays pending in `staging.db` until the user accepts. When `false`, `auto-move` / `auto-tag` matches auto-accept on insert; `review`-policy matches always require user accept. Live-applied. [triage-review-required] |
| `scope` | string | `"inbox/"` | user + vault | Source-folder boundary for triage moves. Triage never produces a `move_note` row whose `source_path` is outside this folder. Also the default folder for on-save trigger. |
| `scheduled_rerun` | string | `""` | user + vault | Cron-shape; empty disables. Per `cluster-editor-triage-scheduled-rerun`. |

Strict-load schema coverage per `settings-strict-load`. The settings UI grows a "Triage" subsection under the Suggestions card with rows for each key; live-applied (no restart).


## Module placement

- `core::cluster` — the build engine (`clustering.md`).
- `core::trees` — `trees.db` owner; persists saved/draft trees (per `cluster-editor.md`).
- `core::suggest` — Apply mechanic (walk tree → emit staging rows), markdown-rendering helper (`render_markdown(tree_id) -> String` consumed by the editor view and CLI export), history bookkeeping, triage classifier wrapping `cluster-place-beam-descent`. Reads trees from `core::trees`, writes proposals through `core::staging::propose`, never produces or reads proposal files.
- `core::staging` — the substrate for every triage match (and every one-off Stage move/tag from the cluster editor). Accept path reuses `suggestions-apply-cmd`.
- `ui` — cluster editor (Apply button, batch-review pane, markdown-view toggle), activity-detail Pending filter (`surface = "triage"` chip is just a value the existing filter already understands), toast + Undo for triage auto-accepts (the toast is fired by the auto-accept transaction, not by the queue).
- CLI — `hiker suggest [--scope … --method …]`, `hiker suggest show <tree-id>`, `hiker suggest apply <tree-id> [--accept-all | --dry-run]`, `hiker suggest export <tree-id>`, `hiker suggest import <path>`, `hiker suggest set-policy <tree-id> <node-id> <policy>`, `hiker suggest move <tree-id> <node-id> <new-parent>`, `hiker suggest save <tree-id>`, `hiker suggest triage [--scope inbox]`.

Module discipline mirrors the other engines: `core::suggest` consumes plain Rust types from `core::cluster`, doesn't reach into HDBSCAN or embedder internals, and exposes a narrow API to UI / CLI.


## Out of scope

- A durable curated tree alongside the filesystem. The filesystem is the only source of truth for organization; the tree is a recommendation tool, not a structural overlay.
- Multi-axis suggestions (semantic + temporal + entity trees applied together). One semantic tree per run.
- Cross-vault suggestions. One vault per run.
- Per-note `hiker.placement` provenance. Replaced by the one-decision-per-suggestion model: when the user accepts, the move/tag *is* the decision; nothing extra needs to be tracked.
- Trail discovery from clusters. Trails are user-authored only by design.


## Deferred

- Folder pinning (above).
- Multiple saved trees per vault.
- Tighter integration with the auto-tag enrichment system — convergence is the goal, but the simpler split ships first.
- `--watch` triage mode (scheduled triage runs).
- Cluster-driven splits/merges *of existing folders* (suggestions that say "your `work/` folder is actually two distinct clusters; consider splitting"). Speculative; revisit if real use shows it.
- Heading-level suggestions inside a single note (driven by `cluster-chunk-multitopic-flag`). Different surface; out of this doc.
