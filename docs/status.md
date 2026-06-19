---
resolution: container
---

# Status — subsystem map & spec conventions

High-level map of hiker's systems: what each one is, where its spec lives, and coarse
(container-level) governance links per area. The per-feature registry that used to fill this file
(1,300+ rows) now lives **in the spec docs themselves**: each feature's `[slug]` anchor carries
`status::` / `note::` inline fields plus its typed code links, right next to the spec text that
defines it. This doc governs at the altitude above: subsystems, not features.

## Spec conventions

Every feature is identified by a stable kebab-case slug, anchored in its owning spec doc as a bare
`[slug]` token at the end of the sentence that defines it. Slugs are positional-free — they name
the feature, not its location — so reorganizing a doc never breaks references. Directly after the
anchor line, inline fields carry the feature's registry data:

```
... the cascade also removes the companion folder. [trail-delete-cascade]
status:: done
implements:: [[code:hiker/trails/ops/delete_trail]]
verifies:: [[code:hiker/trails/tests/parse/waypoints_dir_is_trail_doc_companion_folder]]
note:: free-form context that doesn't belong in the spec prose
```

- **`status::`** — `done` | `partial` | `planned` | `draft` | `removed` | `superseded`. `draft`
  marks spec text generated from existing code and not yet human-verified: the *code* may be done;
  the *spec's claims about intent* are unblessed. Never file a generated stub as `done`.
- **When code implements a slug, tag it in a comment**: `// status: trail-delete-cascade` near the
  natural entry point. One tag per feature — the goal is grep-ability, and the tags also protect
  the feature's edges from reconcile's prune. Don't sprinkle the slug across every helper.
- **When you write a new spec**, anchor it where the behavior is defined and give it a
  `status:: planned` line. When a feature is renamed/split/merged, update the anchor and its
  fields first, then the code tags.
- **Referencing another spec in prose**: use a spec link — `[[spec:trail-delete-cascade]]`
  ([[spec:wikilink-spec-links]]). It resolves by anchor search through the indexer's
  spec-anchor index ([[spec:spec-anchor-index]]), so the reference survives the entry moving
  between docs. This is the norm — existing backticked mentions were converted wholesale;
  backticks remain only for slugs that have no anchor yet (nothing to link to).

### Spec→code links (drift tracking)

Spec docs carry **typed links to code entities**, reconciled into a drift baseline (`links.json`
at the repo root, committed) by the code-intel tooling (`code-intel/`: `reconcile_docs`,
`code-cli drift|ack|trace|churn`, `coverage_specs`, `gap_list`):

- **Link lines are bare Dataview-style inline fields** on their own line after the `[slug]` anchor
  they belong to. The `[[code:hiker/<path>]]` body is the symbol's short descriptor path;
  `gap_list` and `code-cli entities <scip> <repo> --shorts` print ready-to-paste bodies. If
  reconcile reports a link `ambiguous`, qualify it.
- **Pick the relation by what the spec claims** — the relation fixes the drift granularity (the
  C4 resolution), enforced by the tooling (the *relation floor*):
  - `implements::` — the spec describes **how** this code behaves. Drifts at **Code** level: a
    real (AST-normalized) body change flags the spec for re-verification; comments/formatting
    never drift.
  - `verifies::` — a test that vouches for the spec'd behavior. Also **Code** level.
  - `touches::` — the spec **governs the shape** of a module/type without pinning bodies. Drifts
    at **Component** level (member add/remove/rename), or coarser if the doc declares it. Prefer
    one `touches::` on a container over sprinkling `implements::` on plumbing.
  - `manifests-in::` / `verifies-fix::` — the **bug-row relations**
    ([[spec:tracker-relation-links]]): where a bug manifests, and the regression test vouching
    for its fix. Both body-level claims → always **Code** (same floor as `verifies`). Authored
    inline on `bug_tracking.md` rows — the row's slug is the anchor — never as link lines under
    spec anchors.
  - You cannot coarsen `implements`/`verifies` (or the bug-row relations) — a body-level claim
    with structure-level drift would refute itself. To govern more loosely, *demote the relation
    to `touches`*.
- **A doc may declare its altitude in frontmatter** (`resolution: container`), applying to its
  `touches::` links only: `component` (default) = member changes · `container` = the owning
  crate's symbol surface · `context` = governs via children, never drifts on code churn. This doc
  declares `container` — its subsystem links below drift only when an area's API surface moves.
- **Baselines move only on `code-cli ack <spec>`** ("I re-verified this spec against its code").
  Re-running reconcile never resets drift. Treat a `DRIFTED` flag like a failing test: re-read the
  spec, fix prose or code, then ack.
- **Deleting a link line (or a `// status:` tag) deletes the edge**: reconcile lists store edges
  no doc line or comment tag claims, and `--prune` drops them.

Bugs live in [`bug_tracking.md`](bug_tracking.md) (same slug conventions; bug rows carry their
own typed code links inline — [[spec:tracker-relation-links]]).

## Subsystems

### Persistence & history

- **Op log** — the local layered editing model every write rides on; documents are plain `.md`
  files (canonical) edited as `accepted`/`working`/`pending`, with local history as plain-file
  snapshots (no op-log history engine, no sync substrate). Spec: [op-log.md](op-log.md). [sys-op-log]
  touches:: [[code:hiker/oplog]], [[code:hiker/snapshot]], [[code:hiker/vault]]
- **Autosave** — crash-recovery snapshots of dirty buffers plus tab-state restore on vault
  re-open. Spec: [autosave.md](autosave.md). [sys-autosave]
- **Watcher** — filesystem watcher driving the indexer and live-buffer drift detection. Spec:
  [watcher.md](watcher.md). [sys-watcher]
  touches:: [[code:hiker/watcher]]
- **Git (optional)** — optional, user-driven git integration (the VSCode model): commit-on-save,
  the `log`/`show`/`diff_paths` read API, the conflict-marker resolver. Off by default; no
  automatic push/pull. Spec: [git.md](git.md). [sys-git-transport]
- **Diff & review** — the unified diff primitive and its editor surfaces; in-editor review of an
  agent session's pending edits. Specs: [diff.md](diff.md), [patch-review.md](patch-review.md).
  [sys-diff-review]

### Indexing & retrieval

- **Index** — watcher → chunker → embedder → store pipeline plus the related-notes query. Spec:
  [index.md](index.md). [sys-index]
  touches:: [[code:hiker/indexer]], [[code:hiker/chunker]], [[code:hiker/store]]
- **Search** — vault-wide lexical + semantic retrieval in one panel. Spec:
  [search.md](search.md). [sys-search]
- **Queries** — saved queries over the derived indexes: query-docs, the closed filter grammar,
  smart folders in Vault mode, and the generic `query` MCP tool. Spec: [queries.md](queries.md).
  [sys-queries]
- **Clustering** — building a hierarchical topic tree from embeddings (build side). Spec:
  [clustering.md](clustering.md). [sys-clustering]
  touches:: [[code:hiker/cluster]], [[code:hiker/trees]]
- **Cluster editor** — viewing, editing, and automating a cluster tree. Spec:
  [cluster-editor.md](cluster-editor.md). [sys-cluster-editor]

### Editor & UI

- **Editor** — the embeddable text-editor widget hosted as the buffer tab kind inside the
  workbench shell. Spec: [editor.md](editor.md). [sys-editor]
  touches:: [[code:hiker/panels/buffer]], [[code:hiker/editor_pane]], [[code:hiker/workbench_host]]
- **Live preview & widgets** — markdown live-preview rendering and the block/inline widgets
  (math, mermaid, tables) drawn in place of source. Specs: [live-preview.md](live-preview.md),
  [editor-widgets.md](editor-widgets.md), [diagram.md](diagram.md). [sys-live-preview]
- **Files & vault views** — the file-tree activity, logical vault lenses, and inline previews.
  Specs: [files.md](files.md), [vault-view.md](vault-view.md), [previews.md](previews.md).
  [sys-files]
- **Interaction grammar** — the cross-surface conventions for affordance signaling and input
  behavior (click/double-click/right-click/drag/hover/Esc) every surface builds against;
  divergences tracked as `bug-…` rows. Spec: [interaction.md](interaction.md). [sys-interaction]
  touches:: [[code:hiker/sidebar]]
- **Wikilinks** — name-addressed references between notes, resolution and navigation. Spec:
  [wikilinks.md](wikilinks.md). [sys-wikilinks]
  touches:: [[code:hiker/wikilink]]
- **Autocomplete** — the shared type-and-pick substrate behind every completion surface. Spec:
  [autocomplete.md](autocomplete.md). [sys-autocomplete]
- **Canvas** — JSON Canvas editor/renderer plus trail/cluster exports. Specs:
  [canvas.md](canvas.md), [canvas-export.md](canvas-export.md). [sys-canvas]
  touches:: [[code:hiker/panels/canvas]]
- **Kanban** — board view over the vault; cards reference notes. Spec: [kanban.md](kanban.md).
  [sys-kanban]
  touches:: [[code:hiker/kanban]]
- **Graph view & projection** — the spatial graph engine behind node-link surfaces, plus
  hyperbolic/fisheye projections. Specs: [graph-view.md](graph-view.md),
  [projection.md](projection.md). [sys-graph-view]
- **App shell** — activity registry, context menus, settings surface, styling tokens, inbox
  rules. Specs: [activity-registry.md](activity-registry.md), [context-menu.md](context-menu.md),
  [settings.md](settings.md), [style.md](style.md), [inbox-rules.md](inbox-rules.md).
  [sys-app-shell]
  touches:: [[code:hiker/keybinds]], [[code:hiker/toolbar]], [[code:hiker/titlebar]]

### Knowledge & AI

- **Trails** — curated memex-style walks through a vault with annotated waypoints. Spec:
  [trails.md](trails.md). [sys-trails]
- **LLM** — generative-LLM usage: routing, prompts, config, policy posture. Spec:
  [llm.md](llm.md). [sys-llm]
  touches:: [[code:hiker/llm]], [[code:hiker/prompts]]
- **Task queue** — the unified queue for non-interactive jobs (LLM work, indexing sweeps). Spec:
  [task-queue.md](task-queue.md). [sys-task-queue]
  touches:: [[code:hiker/tasks]]
- **MCP** — the vault exposed as an MCP server for external agents. Spec: [mcp.md](mcp.md).
  [sys-mcp]
- **Code intelligence** — reading and reasoning about external code repos alongside notes: the
  spec engine, SCIP adapters, and the code graph view. Specs: [code.md](code.md),
  [projects.md](projects.md). [sys-code-intel]
  touches:: [[code:hiker/panels/code_graph]]

### PM

- **Kinds** — the user-definable kind registry: typed fields against closed primitives, state sets
  with category anchors, shapes, lenient validation, and registry-generated MCP tools. First doc of
  the PM layer (queries → kinds → PM semantics → rules). Spec: [kinds.md](kinds.md). [sys-kinds]
- **PM semantics** — the built-in PM kinds wired into boards: derived status from sprint columns,
  sprint close/rollover, epic rollups, plans, freeform-card promotion, and op-log-replay metrics.
  Spec: [pm.md](pm.md). [sys-pm]
- **Rules** — post-index automation: trigger + condition + actions declared in vault config,
  conditions reusing the queries grammar, a closed action-verb set riding the op-log's attributed,
  reviewable write paths. Last layer of the PM arc. Spec: [rules.md](rules.md). [sys-rules]

### Import & viewers

- **Import** — bringing externally-originated content into the vault. Spec:
  [import.md](import.md). [sys-import]
- **ZIM viewer** — in-app viewer for offline `.zim` archives. Spec: [zim.md](zim.md).
  [sys-zim]
  touches:: [[code:hiker/zim]]
- **Txt ingest** — ingesting and rendering plain-text files. Spec:
  [txt-ingest.md](txt-ingest.md). [sys-txt-ingest]

### Meta

- **Design & ideas** — the system-level plan and the not-yet-committed concepts. Specs:
  [design.md](design.md), [ideas.md](ideas.md).
- **Observability** — `tracing` instrumentation plan. Spec:
  [observability.md](observability.md).
- **QA** — retrieval-quality evaluation, distinct from unit tests. Spec: [qa.md](qa.md).
- **CLI** — the command-line surface. Spec: [cli.md](cli.md).
- **Release** — versioning, branch flow, release mechanics. Spec: [release.md](release.md).
- **Bug tracking** — known issues. Spec: [bug_tracking.md](bug_tracking.md).
