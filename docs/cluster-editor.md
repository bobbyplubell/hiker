# Cluster editor

Interactive surface for viewing, manually editing, and configuring automation on a cluster tree (per `clustering.md`'s `ClusterTree`). A tree is a per-tree `.md` file whose body is an editable outline of clusters and note references; the graphical cluster editor and the markdown editor tab are two surfaces over that one document.

Orientation: a tree lives in the left sidebar's Clusters panel and can Expand into a full center pane for graphical work; multiple trees stay open at once; the document is a visible per-tree `.md` whose outline body is the canonical structure (`core::trees`, op-log substrate). The user reshapes at every level (move/merge/split/rename/drop/promote), attaches `Tag`/`Move`/`Freeze` automation policies at any level, and either Applies one-shot or Saves-as-triage. Building a tree happens in a `cluster-review` tab, not a modal. The sections below own the detail.


## Sidebar placement

Clusters is an independent activity-bar sidebar panel (alongside Files / Trails / Trash / Vault); the arrangement persists via the workbench panel set, not a `vault.sidebar_mode` switcher ([[spec:sidebar-mode-switcher]] in `status.md`). The sidebar's collapse toggle ([[spec:sidebar-toggle-icon]]) hides the whole sidebar. The panel body is described below.

## Cluster trees panel (sidebar)

Sidebar body when the mode is Cluster trees. Header carries the mode-specific actions; body lists the open trees with their tree contents nested. Multiple trees stay open at once — typically the saved triage tree (always present once saved) plus zero-or-more ephemeral one-shot trees, each expanding inline to its node hierarchy with a name + state pill (`draft` / `applied` / `saved as triage`); switching between trees is just expanding a different one, no modal context-swap. [cluster-editor-multiple-trees-open]
status:: done
note:: `app/src/sidebar/clusters/mod.rs` reads `cluster_trees_list` (host command) and renders one collapsible section per row with name + state pill (`draft` / `applied` / `saved-as-triage`). The first tree opens by default; subsequent ones collapse so the sidebar stays scannable. Each tree carries its own selection + expanded sets

The open-trees list renders inline hierarchical rows (chevron + name + summary preview + member count + policy chip + row menu), expand/collapse per cluster, and click-a-leaf to open the note in the editor pane; new-tree and the actions menu anchor off the panel header. [cluster-editor-sidebar-mode]
status:: done
touches:: [[code:hiker/clusters/sidebar]]
note:: `app/src/clusters/sidebar/mod.rs` (+ `tree.rs`) renders the open-trees list with inline hierarchical rows (chevron + name + summary preview + member count + policy chip + row menu), expand/collapse per cluster, click a leaf to open the note in the editor pane. New-tree modal + actions menu anchor off the panel header. Refreshes on vault open. (The former sidebar mode-switcher framing is superseded by the egui-workbench multi-region sidebar — Clusters is now an independent dockable panel; see [[spec:sidebar-mode-switcher]])

```
┌─ Sidebar ─────────────────────────────────┐
│ Cluster trees                       [...] │  ← panel header
│   [+ Suggest reorganization]              │  ← primary action
│                                           │
│ ▼ 2026-05-08 reorg          draft  [⤢]   │  ← tree row, Expand button
│   ▼ Vault root                            │
│     ▼ Research (24)              [tag]   │
│       ▶ Embeddings (8)          [move]   │
│       ▶ Vector DBs (5)          [tag]    │
│       ▶ LLM agents (11)        [review]  │
│     ▶ Projects (15)                       │
│     ▶ Outliers (7)                        │
│                                           │
│ ▶ Saved triage tree         saved   [⤢]  │
└───────────────────────────────────────────┘
```

The `[⤢]` icon on each tree's row is the Expand button — flips that tree into the center-pane expanded mode (see below).

Header actions:

- **New tree** — the primary affordance, the Clusters accordion-header `+` split-button (the shared [[spec:split-add-button]]). The primary `+` opens a `cluster-review` app-page tab with default params; the caret dropdown lists tree-creation presets that prefill the tab. There, the user configures scope/method/params, runs the structural clustering pass, reviews the result, and confirms (firing the LLM naming pass and persisting the tree as `draft`). The cluster editor is the review surface. Full detail in the "Clustering review tab" section below. [cluster-editor-new-tree-action]
status:: done
implements:: [[code:hiker/workbench_host/impl#[`HikerWbBehavior<'a>`][`Host<HikerWbTab, _>`]side_bar_action_buttons]]
touches:: [[code:hiker/clusters/panel]]
note:: Clusters accordion-header `+` split-button ([[spec:split-add-button]]): primary opens the review tab with default params; caret dropdown lists tree-creation presets. The panel body's old "Cluster trees" label + `+ New tree` button were removed. Algorithm choice happens inside the review tab. `cluster_tree_create` backend command preserved for tests / CLI · evidence: `app/src/workbench_host.rs` (`side_bar_action_buttons`, clusters branch), `app/src/clusters/panel/mod.rs` (`ReviewConfig::open`)

  Lifecycle (**Sapling** / **Evergreen**) is one of the form fields — a hint that sets the post-confirm default action, not a hard mode. [cluster-editor-sapling-evergreen-lifecycle]
status:: done
note:: Sapling's once-and-done path (Sprint C) plus the Evergreen path (Sprint D). Save-as-triage button in `app/src/panels/cluster_review/mod.rs` flips the tree's `state` to `saved-as-triage` via `cluster_tree_set_state`; the watcher subscription in the host then runs `core::suggest::triage_all_saved_trees` on every Modified/Created/Renamed event whose path is inside the triage scope, emitting `surface = "triage"` staging rows per matched policy
- **Tree-creation presets.** A preset is a reusable set of tree-creation params (algorithm, source-type filter, post-confirm LLM-naming toggle) — the review form's config minus the per-tree name. A user preset is an **ordinary vault note** carrying `hiker.kind: cluster-preset` frontmatter (the params in a `cluster_preset` block); the `+` dropdown finds them through the store's frontmatter query ([[spec:store-note-query]]), so a note the user hand-typed or imported with that frontmatter is a preset exactly like one hiker saved — nothing lives under `.hiker/`, per [[spec:subsystem-notes-visible]]. The caret lists built-ins first, then user presets. [cluster-preset]
status:: done
implements:: [[code:hiker/clusters/preset/Params]]
touches:: [[code:hiker/workbench_host]]
note:: tree-creation presets (algorithm + source-type filter + post-confirm naming toggle) listed in the Clusters `+` caret; a pick opens the review tab prefilled. User presets are ordinary vault notes found by the frontmatter query ([[spec:store-note-query]]) — not under `.hiker/`; built-ins first, then user presets · evidence: `app/src/clusters/preset.rs` (`load` via `Store::query_notes` on `hiker.kind: cluster-preset`), `app/src/workbench_host.rs` (clusters `+` dropdown, cached in `clusters_state.preset_cache`)

  Built-in presets (Leiden / HDBSCAN / From folders) are virtual (in-code) and always appear. [cluster-preset-defaults]
status:: done
implements:: [[code:hiker/clusters/preset/builtins]]
note:: in-code default presets always shown: Semantic — Leiden / Semantic — HDBSCAN / From folders · evidence: `app/src/clusters/preset.rs` (`builtins`)

  "Save preset" in the review tab writes such a note (default location `cluster-presets/<slug>.md`; the user can move it anywhere — discovery is by frontmatter, not path). [cluster-preset-save]
status:: done
implements:: [[code:hiker/clusters/panel/impl#[`Review<'_>`]show]], [[code:hiker/clusters/preset/save]]
note:: "Save preset" in the review tab writes an ordinary vault note (`hiker.kind: cluster-preset` + a `cluster_preset` params block, default `cluster-presets/<slug>.md`) + indexes it; the frontmatter query reloads it into the `+` dropdown. Discovery is by frontmatter, not path · evidence: `app/src/clusters/preset.rs` (`save`), `app/src/clusters/panel/mod.rs` (review-tab "Save preset")
- **`…` menu** — mode-specific overflow: "Open saved triage tree" (no-op when already open), "Import tree from file" (load a `cluster-tree.json` from elsewhere — useful for sharing trees), "Discard all drafts," "Tree settings" (per-node policy defaults, etc.). [cluster-editor-mode-menu]
status:: partial
note:: `app/src/sidebar/clusters/mod.rs` (the `…` actions menu, clusters mode only). Entries: New tree… / Discard all drafts / Refresh. **Partial:** the spec's "Open saved triage tree" / "Import tree from file" / "Tree settings" entries are not yet wired — they hinge on the triage-tree distinction ([[spec:cluster-editor-save-as-triage]], Sprint C) and on an import pathway that lands later

Each tree's body is a hierarchical list, one row per cluster or leaf note. Cluster rows are collapsible (chevron at left); leaf-note rows are clickable to open the note in the editor pane on the right. Multi-select works via Shift/Cmd-click on rows; selection survives expand/collapse so a bulk merge across collapsed sections is still possible.

Row layout:

```
[chevron] <name>                                 [members] [policy chip] [drag handle]
          <summary preview, one-line>
```

- **Chevron** signals row kind and expand state on its own: `▾` / `▸` on a cluster with children, blank space on a leaf or empty cluster. No separate per-row type glyph — chevron + name + members count carry the kind without the visual noise.
- **Name** is the cluster's name (LLM-generated or user-edited) or the note's basename. Click-to-edit on cluster names; leaf names are read-only (rename a note via the regular file-tree affordances).
- **Summary preview** is the cluster's LLM-generated summary truncated to one line; click expands to a full-text editable area below the row, click the area's name in turn to edit. Leaves don't show a summary. A `↻ N` badge appears on the right of the summary preview when `summary_membership_churn > 0`, displaying the count of membership changes since the summary was generated (per [[spec:cluster-summary-staleness-counter]]). Clicking the badge offers a quick Regenerate action for that node. [cluster-editor-summary-staleness-badge]
status:: done
note:: `app/src/panels/cluster_review/mod.rs` paints a `↻ N` button on cluster rows whose `summary_membership_churn > 0`; clicking enqueues a tree-wide Regenerate (`cluster_regenerate_names`). Per-node "Regenerate this row" lives behind the same task wiring as the toolbar regenerate; the badge today queues a tree-wide pass
- **Members count** is the number of notes in the cluster's full subtree (including all nested children's leaves). Empty for leaves.
- **Policy chip** shows the resolved-or-explicit policy for this node — `tag: <slug>` / `move: <path>` / `freeze` / blank (no policy, walks up to nearest ancestor). Chips for policies with `require_review = true` get a small badge (`tag: research ⏸` / `move: research/ ⏸`) so the review-required state is visible without opening the editor. Clicking the chip opens the policy editor for that node.
- **Drag handle** lets the user drag the row onto another cluster (move) or onto another sibling (reorder, no semantic change).

Outliers render as a special virtual node at the bottom of every tree level — labeled "Outliers (N)" with a distinct icon. Drag a leaf into a cluster to promote it; drag a leaf out of any cluster onto Outliers to demote it. The outlier bucket is the sink for "no good cluster fit"; users can manually fish notes out of it during reshape. [cluster-editor-outlier-virtual-node]
status:: done
touches:: [[code:hiker/trees/build_adapter]]
note:: `app/src/sidebar/clusters/mod.rs` — renders an "Outliers (N)" virtual node pinned at the bottom of every level that contains at least one cluster sibling (plus the root level unconditionally). When a real `outlier-bucket` node exists among the siblings (per `core/src/trees/build_adapter.rs::node_inserts`, build pass emits one at the root whenever `include_outliers = true`) it floats to the bottom with the distinct `◇` icon; otherwise a ghost row is synthesized so the affordance is uniform across depths. Both the top-level tree body and recursive cluster-children paths route through the same helper


## Expanded mode (center pane)

Clicking the Expand button on a tree row sends that tree to the center pane, replacing the editor (mirrors [[spec:chat-panel-expand-to-editor]]). This is the surface tuned for heavy graphical reshaping — wider rows, larger drag targets, multi-pane "before/after" preview, more screen real estate for visualizing N-level trees. [cluster-editor-pane-expand]
status:: partial
touches:: [[code:hiker/clusters/sidebar]]
note:: the cluster-batch-review pane opens via the row menu's "Open in pane"; it starts in `cluster-tree` sub-mode and flips to `cluster-batch-review` after Apply. **Partial**: the spec'd `[⤢]` icon button on each tree row isn't wired yet (`app/src/clusters/sidebar/mod.rs:74`). Evidence: `app/src/clusters/sidebar/mod.rs` (row-menu "Open in pane"), `app/src/panels/cluster_review/mod.rs` (the pane)

The expanded mode is a new editor-pane sub-mode, joining the existing list (editor / vault-home-overview / vault-home-detail / settings / chat-expanded). The sidebar's cluster trees mode keeps showing the same tree (in its docked form) so the user can switch back by collapsing the expanded view. [cluster-editor-pane-mode]
status:: done
note:: cluster-batch-review tab kind handled in `app/src/panels/cluster_review/mod.rs`; the workbench host hides the editor + reveals the cluster-editor pane when the active tab is this kind. Toolbar carries Apply / Save-as-triage / Regenerate / Rebuild / view-toggle / Discard. The header bar is styled as proper top chrome and stays pinned to the pane's top edge as the tree body scrolls past. Tree body has full parity with the sidebar's expand/collapse + click-to-edit (name + summary) + right-click context menu (Move to… / Split / Subcluster… / Merge children up / Drop cluster / Send to outliers / Promote out of outliers…) + policy chip popover + ↻ staleness badge + Shift/Cmd/Ctrl-click selection. Multi-select toolbar (Merge siblings / Drop / Stage move to / Stage tag with / Clear) lands in the pane header when there's a selection. Pane-local expanded + selection sets are keyed by tree id so the queue-event-driven refresh preserves user UI state; first-open seeds expanded with root-level cluster nodes

Layout (sketch):

```
┌─ Cluster editor: 2026-05-08 reorg ─────────────────────────────┐
│ [Apply] [Save as triage] [Regenerate names] [Markdown view] [✕]│
│                                                                │
│ ▼ Vault root                                                   │
│ │                                                              │
│ ├── ▼ Research (24)                          [tag: research]   │
│ │    ├── ▶ Embeddings (8)                   [move: research/]  │
│ │    │    ├── inbox/whisper-notes.md                           │
│ │    │    ├── inbox/voyage-vs-bge.md                           │
│ │    │    └── ...                                              │
│ │    ├── ▶ Vector DBs (5)                   [tag: vectordb]    │
│ │    └── ▶ LLM agents (11)                   [review]          │
│ ├── ▶ Projects (15)                                            │
│ └── ▶ Outliers (7)                                             │
│                                                                │
│ ─── Selected: 3 nodes ─── [Merge siblings] [Merge up] [Split]  │
└────────────────────────────────────────────────────────────────┘
```

Behavior:

- **Click a leaf note row → open the note in the editor.** This *replaces* the expanded tree view in the center pane with the regular editor on the clicked note. [cluster-editor-pane-leaf-click-opens-note]
status:: done
note:: `app/src/sidebar/clusters/mod.rs` (sidebar) + `app/src/panels/cluster_review/mod.rs` (expanded pane) both route leaf-row clicks through the host-supplied open-note path

  The existing navigation history ([[spec:navigation-history-stack]]) records the expanded-tree state; the back button returns to it. From the user's perspective the loop is: "expanded tree → click note → read/edit → back → expanded tree." No special "back to tree" affordance needs adding; it's the same nav-stack back the rest of hiker uses. [cluster-editor-pane-back-to-tree]
status:: done
note:: `app/src/panels/cluster_review/mod.rs` paints a `← Back to tree` button that flips the pane's sub-state back to `cluster-tree` without closing pending staging rows; activation stays on the same tab so the existing nav stack remains coherent
- **Click a cluster row → expand/collapse** as in the sidebar form. No navigation hop. Pane-local `expanded: Set<NodeId>` is per-tree and survives refreshes (queue-event-driven re-fetch after a `raptor_summarize` task lands doesn't collapse the user's expansion state).
- **Full row UX parity with the sidebar.** The shared row primitive ([[spec:cluster-editor-row-primitive]]) gives the pane the same chevron / click-to-edit name + summary / right-click context menu / policy chip popover / staleness badge / multi-select behavior as the sidebar. Right-click verbs are identical: cluster rows expose Move to… / Split / Subcluster… / Merge children up / Drop cluster; leaf rows expose Move to… / Promote out of outliers… (under the outlier bucket) / Send to outliers; the outlier bucket exposes Move to… / Drop cluster.
- **Multi-select via Shift/Cmd-click** picks rows across levels (selecting a cluster includes its subtree implicitly for purposes of the bulk-action toolbar). The multi-select toolbar surfaces in the pane header alongside the existing Apply / Save-as-triage cluster when `selection.size > 0`, with verbs Merge siblings / Drop / Stage move to… / Stage tag with… / Clear. Pane-local selection state also survives queue-event refreshes.
- **Markdown view toggle** in the toolbar flips the pane between the graphical tree and an editable markdown view over the same document — the outline body the tree carries on disk — mirroring the board view's `View as: Board / Markdown` ([[spec:board-view-toggle]]). Editing the markdown edits the tree: structural edits parse back through the parse tiers, and switching to the graphical view re-renders from the now-current body. The same document also opens as a full editor tab for side-by-side work ([[spec:tree-edit-via-tabs]]). [cluster-editor-markdown-view-toggle]
status:: planned
note:: editable markdown view over the outline body as a third view variant alongside Tree / Graph, mirroring [[spec:board-view-toggle]]. Editing the markdown edits the tree (parses back via [[spec:tree-md-parse-tiers]]); switching to graphical re-renders from the body. The same doc also opens as a full editor tab ([[spec:tree-edit-via-tabs]])
- **Apply / Save as triage / Discard draft** are toolbar buttons. Apply emits staging rows for every leaf with a `Tag` or `Move` policy and opens the batch-review pane (see below); Save as triage persists the tree (and its policies) as the active triage classifier, replacing any prior saved tree — it does *not* enqueue staging rows itself, triage emits them as matches fire over time; Discard draft confirms then deletes the tree's `.md` file. A user can both Apply and Save-as-triage from the same tree if both make sense. [cluster-editor-save-as-triage]
status:: partial
touches:: [[code:hiker/clusters/sidebar]]
note:: `app/src/panels/cluster_review/mod.rs` enables the Save-as-triage button when the tree's `state != "saved-as-triage"`. Clicking confirms then calls `cluster_tree_set_state(tree_id, "saved-as-triage")` (Sprint A's `Trees::set_tree_state` free-string setter). Subsequent on-save events fire the classifier against the saved tree. Replaces any prior saved tree implicitly — the spec's one-saved-tree-per-vault rule is enforced by the `state == "saved-as-triage"` predicate. **Partial**: functional only via the batch-review pane button (`cluster_review/mod.rs:28`); the sidebar entry-point shortcut isn't ported (`app/src/clusters/sidebar/mod.rs:9` — "the pane button covers it")
- **Regenerate names** runs the cluster naming/summarization prompt for any cluster whose name is still a `Cluster N` placeholder. Each regeneration is one task in `core::tasks` (per `task-queue.md`); the user can watch progress in the queue widget. Regenerating an already-named cluster requires per-node explicit "Regenerate this node" from its row menu. Tasks are submitted **bottom-up**: nodes are grouped by depth, and each depth's `RaptorSummarize` batch is awaited before the next shallower batch — so a parent summarizes over its children's real names, not placeholders (each worker reads its children's current `name`/`summary` at execution time). [cluster-editor-regenerate-via-task-queue]
status:: done
note:: `cluster_regenerate_names` host command walks every cluster node, skips fully user-edited ones, and submits one `TaskKind::RaptorSummarize { tree_id, cluster_node_id, level: 0 }` task at `Priority::Normal`. Toolbar "Regenerate names" button + the per-row `↻ N` staleness badge both invoke it. Worker that consumes the task (resolves members → LLM call → writes back through `Trees::set_summary`) rides the existing direct-LLM drain. **Ordering:** tasks are submitted bottom-up — nodes grouped by depth from root (`BTreeMap<usize, Vec<EditableNode>>`), each depth's batch is `await_outcome`'d before the next shallower batch is enqueued, so parent summaries see their children's freshly-generated names instead of placeholders ("Cluster 1", "Cluster 2")


## Tree storage: per-tree `.md` files

Cluster trees live as per-tree markdown documents at a visible vault path — `{new_cluster_tree_dir}/<tree-id>.md` (the `[clustering] new_cluster_tree_dir` key, default `cluster-trees/`) — owned by `core::trees` (sibling to `core::store`'s `index.db` and `core::oplog`). Each tree is one `.md` file whose **body is the canonical structure** — a nested outline of clusters and note references a human reads, edits, and copy-pastes directly. Frontmatter holds only tree-level metadata (`hiker.kind: cluster-tree`, id, name, lifecycle, scope, method); it does not hold the node hierarchy. Trees ride the op-log substrate like any other markdown document (per `op-log.md`) — they sync, carry version history, and every edit is a plain-text edit on the substrate. [trees-md-store]
status:: planned
note:: Per-tree `.md` files at a visible vault path (`{new_cluster_tree_dir}/<tree-id>.md`, default `cluster-trees/`, per [[spec:cluster-tree-visible-note]]); tree-level metadata in `hiker` frontmatter (`kind: cluster-tree`), the structure is the editable outline in the body; rides the op-log substrate like any other markdown doc (per `op-log.md`). Owned by `core::trees`; discovered by the frontmatter query, no watcher carve-out (the visible path is indexed like any note). No SQLite store

A tree is user-created content, so by [[spec:subsystem-notes-visible]] it lives at a real, browsable vault path and is discovered through its `hiker.kind: cluster-tree` frontmatter ([[spec:store-note-query]]) — not under `.hiker/`, and not by any directory glob. A tree the user moved, renamed, hand-typed, or imported with that frontmatter is found exactly like one hiker authored; the filename is incidental (the tree id is carried in `hiker.id`, the path is decoupled from the id). The configured directory is only the *default placement* for newly-created trees. [cluster-tree-visible-note]
status:: done
implements:: [[code:hiker/config/patch/ELIGIBLE_VAULT]], [[code:hiker/config/sections/ClusteringConfig#new_cluster_tree_dir]], [[code:hiker/bootstrap/open_vault]]
touches:: [[code:hiker/trees/store]], [[code:hiker/trees/types]]
note:: per-tree `.md` files live at a visible default (`cluster-trees/`), discovered by `hiker.kind: cluster-tree` frontmatter ([[spec:store-note-query]]) — not by a directory glob. Path is decoupled from the tree id (carried in `hiker.id`), so a moved/renamed/imported tree is found like any other. An in-process id→path cache makes a just-created tree loadable + listable before the indexer runs; the configured dir is settings-eligible (`config/patch.rs`). Op-log binding + frontmatter shape unchanged · evidence: `core/src/trees/store.rs` (`insert_tree` writes `{new_cluster_tree_dir}/<id>.md`, `list_trees` + `path_for_tree` use the `hiker.kind: cluster-tree` frontmatter query, `save` suppresses watcher + enqueues `Upsert`), `core/src/config/sections.rs` (`clustering.new_cluster_tree_dir`, default `cluster-trees/`), `core/src/trees/types.rs` (`Db` watcher/index_jobs/id_paths fields), `app/src/bootstrap.rs` (`trees.wire`)

Because the tree path is now a visible note, `core::trees` follows the same write discipline as trail-docs: each save suppresses the watcher around the op-log atomic write and enqueues an explicit indexer `Upsert`, so the tree is queryable immediately without a redundant external-edit reconcile. Module discipline mirrors `core::trails`: pure Rust types out of the module, the on-disk markdown shape (the outline grammar plus metadata frontmatter) never leaks past the boundary. No schema-version file — the document is self-describing, and unknown frontmatter keys are preserved on round-trip. A one-time migration relocates legacy `.hiker/trees/<id>.md` files to the visible default on first open ([[spec:cluster-tree-migration]]). [trees-module-discipline]
status:: planned
touches:: [[code:hiker/trees/types]]
note:: All outline parse/serialize + metadata-frontmatter (de)serialization stays behind the `core::trees` boundary; outside callers consume `TreeRow` / `EditableNode` / `NodeKind` / `NodePolicy` — plain serde Rust in `core/src/trees/types.rs`. Mirrors `core::trails` (markdown, no on-disk-shape leakage): the visible-note write path suppresses the watcher + enqueues an `Upsert` around its op-log save, same as trail-docs. No SQLite, no schema-version file. One legacy-location migration ([[spec:cluster-tree-migration]]). Submodule layout: `mod.rs`, `types.rs` (DTOs + `Db` handle), `store.rs` (frontmatter parse/serialize + op-log writes + migration), `ops/{edit,move_node,merge,drop,folder_rename,split,rollup}.rs`

The migration is one-time and idempotent — it relocates each legacy `.hiker/trees/<id>.md` tree to the visible default on vault open. [cluster-tree-migration]
status:: done
touches:: [[code:hiker/trees/store]]
note:: one-time, idempotent relocation of legacy `.hiker/trees/<id>.md` trees to the visible default on vault open. Per tree: repoint the op-log doc to the new path (`oplog::writes::rename`, preserving its history) **before** moving the file bytes — the ordering that prevents a forked history; legacy files never op-log-seeded just have their bytes moved (the full-scan seeds them fresh). Skips a tree whose new path already exists; removes the empty `.hiker/trees/` shell; no-ops when `.hiker/trees/` is absent. Indexing is deferred to the indexer's initial full-scan (runs after `Db::new`), so no `Upsert`/suppression is needed in the migration · evidence: `core/src/trees/store.rs` (`migrate_legacy_trees`, run from `Db::new`)

### Document shape

Frontmatter carries tree-level metadata only. [trees-md-frontmatter]
status:: planned
note:: Frontmatter carries tree metadata only: `hiker.{kind,id,name,source,state,scope,method,created_at}`. No `nodes` list — the structure lives in the body outline (see [[spec:tree-md-outline-body]]). Unknown frontmatter keys preserved on round-trip. Centroids are NOT stored in the doc (see [[spec:trees-centroids-index]])

The body carries the structure as an outline. [tree-md-outline-body]
status:: planned
note:: The tree's structure is the editable outline in the `.md` body (not frontmatter): headings = clusters (depth = heading level), nested bullets = deeper clusters + leaves, leaf = a bullet wikilink, summary = the paragraph under a heading. Frontmatter holds tree metadata only

````markdown
---
hiker:
  kind: cluster-tree
  id: 01HXP7Z…                  # ULID; matches the filename stem
  name: 2026-05-08 reorg        # user-visible label
  source: one-shot              # one-shot | saved-triage
  state: draft                  # draft | applied | saved-as-triage
  scope: { kind: vault }        # BuildScope (per clustering.md)
  method: { kind: cluster, … }  # BuildMethod; carries params
  created_at: 2026-05-08T15:42:00Z
---
# Embedding research {move: research/embeddings/}
Notes about embedding models and vector search.

- [[inbox/whisper-notes]]
- [[inbox/voyage-vs-bge]]

## Vector DBs {tag: vectordb, review}
- [[inbox/qdrant-notes]]

# Projects {tag: project}
- [[work/migration]]

# Outliers
- [[inbox/coffee-roasting]]
````

**Outline grammar.** [tree-md-outline-grammar]
status:: planned
note:: Outline grammar: `#`…`######` heading or bold bullet = cluster; its text = name; the paragraph under it = summary; `- [[path]]` = leaf; inline `{move:…}`/`{tag:…}`/`{freeze}` (+`review`) on the heading = policy ([[spec:tree-policy-inline]]); `Outliers` = conventionally-named bucket heading

- A **heading** (`#`…`######`) is a cluster; heading depth is tree depth. Past the heading-depth ceiling, nested **bullet lists** carry deeper clusters (a bold bullet, `- **Sub-cluster**`) and their leaves — headings anchor the eye at the top levels, bullets nest without bound beneath.
- A cluster's **name** is its heading (or bold-bullet) text; its **summary** is the paragraph immediately under the heading.
- A **leaf** is a bullet holding one wikilink to a note (`- [[path/to/note]]`). The wikilink path is the only identity — no stamped id. [tree-leaf-path-ref]
status:: planned
note:: A leaf references a note by vault-relative path (the wikilink target), no stamped id — same path-as-identity model as [[spec:board-card-references]]. The leaf is the only pointer to the note
- A cluster's **policy** is an inline attribute on its heading: `{move: <folder>}`, `{tag: <slug>}`, or `{freeze}`, with `review` added for `require_review` (`{tag: vectordb, review}`). Policy is authored content — it lives in the outline, survives round-trip, and travels when a cluster is copy-pasted. [tree-policy-inline]
status:: planned
note:: A cluster's policy is an inline attribute on its heading (`{move: <folder>}` / `{tag: <slug>}` / `{freeze}`, `review` for require-review). Authored content — survives round-trip and travels on copy-paste of a cluster
- **Outliers** is a conventionally-named top-level heading; it carries a policy like any cluster.

**Parse tiers.** The body parses leniently so a half-typed document never corrupts the tree: [tree-md-parse-tiers]
status:: planned
note:: Lenient body parse: resolvable wikilink bullet → leaf; plain-text bullet → freeform text leaf (cf. [[spec:board-freeform-card]]); unresolvable wikilink → broken leaf ([[spec:tree-broken-leaf]]); malformed line (orphan heading, half-written `[[`) → skipped + decorated as an editor parse error, never dropped. Interleaved prose + unknown frontmatter preserved on round-trip

| Body line | Becomes |
| --- | --- |
| Bullet with a resolvable wikilink | a leaf note |
| Bullet with plain text (no wikilink) | a freeform text leaf (same shape as a board freeform card, [[spec:board-freeform-card]]) |
| Bullet with an unresolvable wikilink | a **broken leaf** — kept in place, rendered with a broken-reference marker ([[spec:tree-broken-leaf]]) |
| Malformed line (orphan heading, half-written `[[`) | skipped and decorated as a parse error in the editor tab; never dropped from the file |

Prose the user interleaves between clusters is preserved verbatim on round-trip, as are unknown frontmatter keys.

**Path identity, rename, broken leaves.** A leaf is a path, exactly as a board card is ([[spec:board-card-references]]); the consequences mirror boards: [tree-leaf-path-ref]

- A leaf whose path doesn't resolve renders greyed with a broken-reference pill and stays in its cluster so the user repoints or removes it. The path is the only pointer, so the broken-leaf surface is the integrity net. [tree-broken-leaf]
status:: planned
note:: A leaf whose path doesn't resolve renders greyed with a broken-reference pill and stays in its cluster for the user to repoint or remove — same safety net as a broken board card
- When a referenced note moves, `core::trees::on_note_moved` rewrites every affected leaf path in the same transaction as the move, riding the shared [[spec:wikilink-rename-rewrite]] pass alongside boards, trails, and wikilink bodies. [tree-leaf-rename-rewrite]
status:: planned
note:: `core::trees::on_note_moved` rewrites affected leaf paths in the same transaction as a note move, riding the shared [[spec:wikilink-rename-rewrite]] pass alongside boards / trails / wikilink bodies
- A derived `tree_leaves` table in `index.db` makes the affected-trees lookup cheap (`trees_containing_note`, the symmetric query to `boards_containing_note`), re-derived on each tree-doc ingest, fail-loud on schema bump — exactly like [[spec:board-cards-derived-table]]. [tree-leaves-derived-table]
status:: planned
note:: Derived `tree_leaves` table in `index.db` for the affected-trees reverse lookup (`trees_containing_note`, symmetric to `boards_containing_note`); re-derived on tree-doc ingest, fail-loud on schema bump — mirrors [[spec:board-cards-derived-table]]

**Centroids are not stored in the doc** — packed embedding vectors would bloat the synced text and diff badly; they live in `index.db` (next).

### Identity and centroids

Clusters have no persisted id. A cluster's identity is its **position-and-name in the outline** (`Embedding research / Vector DBs`) — the conceptual-folder model, except nothing moves on the filesystem: a tree is pointers, so the same note can sit in several trees under different organizations, and many trees coexist over one vault. Node ids exist only as ephemeral handles minted at parse time for the duration of an edit session; they are never written to the doc. [tree-cluster-path-identity]
status:: planned
note:: Clusters have no persisted id; identity is position-and-name in the outline (conceptual folders, nothing moves on disk). Node ids are ephemeral parse-time handles. Centroids keyed by cluster path, recomputed on membership change. Enables several overlapping trees over one vault

Each cluster's centroid (the L2-normalized mean of its members' embeddings, consumed by the placement classifier [[spec:cluster-place-beam-descent]]) is a derived value, recomputable from member embeddings the index already holds. It lives in a derived `cluster_centroids` table in `index.db` keyed by `(tree_id, cluster_path)`, recomputed whenever a cluster's membership changes — including when the user edits the outline by hand. It follows the index's derived-table discipline — rebuilt on schema bump, fail-loud on mismatch (per [[spec:store-version-fail-loud]]) — and a missing centroid is recomputed from members rather than treated as corruption. [trees-centroids-index]
status:: planned
implements:: [[code:hiker/store/centroids/impl#[Store]put_cluster_centroid]], [[code:hiker/store/centroids/impl#[Store]cluster_centroids_for_tree]], [[code:hiker/store/centroids/impl#[Store]delete_cluster_centroids_for_tree]], [[code:hiker/store/centroids/impl#[Store]delete_cluster_centroid]]
note:: Derived `cluster_centroids` table in `index.db` keyed by `(tree_id, cluster_path)`, packed little-endian f32; recomputed from member embeddings whenever a cluster's membership changes (including hand edits). Consumed by [[spec:cluster-place-beam-descent]]. Follows the index's derived-table / fail-loud-on-schema-bump discipline (per [[spec:store-version-fail-loud]])

### Edits as op-log ops

Structure lives in the body, so a structural edit is a localized **body** edit — whether it comes from a graphical reshape or from hand-editing the outline. Both commit through the op-log granular user-save path ([[spec:op-log-ops-producer-helpers]]), the same mechanism a board move uses. Because the whole `.md` is one `Y.Text`, every edit merges, syncs, and carries author/timestamp metadata; concurrent edits to different clusters merge automatically, and two edits to the same outline region surface as a conflict hunk ([[spec:op-log-merge-conflict]]). The semantic label (`move` / `merge` / `split` / `rename` / `set-policy` / …) rides the op's `metadata` field. [tree-edit-ops]
status:: planned
note:: Tree edits — graphical reshape or hand-edited outline — are localized body (or frontmatter) edits committed through the op-log granular user-save path ([[spec:op-log-ops-producer-helpers]]), the same mechanism a board move uses. One `Y.Text`; concurrent edits to different clusters merge, same-region edits surface as conflict hunks ([[spec:op-log-merge-conflict]]); semantic label (`move`/`merge`/`split`/`rename`/`set-policy`) rides op `metadata`

The build pass writes the initial outline when a new tree is created — there is no separate "build snapshot vs editable draft" distinction; every cluster and leaf is editable from the moment it lands. [cluster-editor-edit-history]
status:: planned
note:: edits are op-log ops on the tree doc — no `cluster_tree_history` table; author/timestamp ride the op-log side table, the semantic op name rides op `metadata`. The build pass writes the initial outline

Every edit — graphical or hand-typed — auto-saves to the tree's `.md` through this path; there is no parallel on-disk store. Closing and reopening the editor resumes where the user left off and the draft survives app restarts; Discard-draft is an explicit button. [cluster-editor-draft-persistence]
status:: planned
note:: every edit (graphical or hand-typed) writes the tree `.md` through the op-log granular user-save path; the body outline is the structure (no parallel store); the draft survives app restarts via the persisted `.md`

**Undo / redo.** The cluster editor keeps an in-memory session undo stack; each undo applies the reverse edit through the working layer. Cross-session "revert to an earlier state" rides the tree doc's normal version history — the same snapshot machinery every note has — not a bespoke per-tree log. [cluster-editor-undo-redo]
status:: planned
verifies:: [[code:hiker/trees/tests]]
note:: in-memory session undo/redo stack; each step applies the reverse edit through the working layer. Cross-session "revert to an earlier state" rides the tree doc's version history / snapshots, not a bespoke per-tree log

Discard-draft on a tree whose `source = 'one-shot'` deletes the tree's `.md` file through `core::ops::delete` (so it lands in trash and is restorable like any note). Discard on a saved-triage tree is "Unsave as triage" — flips `state` back to `draft` and (optionally) deletes; the user is asked which. [cluster-editor-discard-draft]
status:: planned
note:: `source = one-shot` → delete the tree's `.md` via `core::ops::delete` (lands in trash, restorable like any note); saved-triage → "Unsave as triage" flips `state` back to `draft` and optionally deletes (user chooses)

The in-memory editable shape hydrates from the parsed outline — clusters by heading/bullet, children by nesting, leaves by wikilink:

```rust
struct EditableNode {
    id: NodeId,                       // ephemeral parse-time handle; never persisted
    parent: Option<NodeId>,
    kind: NodeKind,                   // Cluster | Leaf | OutlierBucket
    note_path: Option<VaultRel>,      // leaves only; the wikilink target, resolved via the index
    name: String,                     // clusters only; user-editable
    summary: String,                  // clusters only; user-editable
    policy: Option<NodePolicy>,       // parsed from the heading's inline attribute
    confidence: f32,                  // from build pass; preserved through edits
    summary_membership_churn: u32,    // per cluster-summary-staleness-counter
}

enum NodePolicy {
    Tag  { slug: String,     require_review: bool },
    Move { folder: VaultRel, require_review: bool },
    Freeze,                           // never propose changes for matches under this node
}

enum NodeKind { Cluster, Leaf, OutlierBucket }
```

Centroids are not carried on the node — they load from `index.db`'s `cluster_centroids` (keyed by cluster path) only when the placement classifier needs them.

Names carry no hidden `user_edited` flag: the body is the truth, so a name is whatever the heading says. Regeneration ([[spec:cluster-editor-regenerate-via-task-queue]]) targets only clusters whose name still matches the `Cluster N` placeholder pattern, leaving any human- or LLM-given name alone. [cluster-editor-tree-shape]
status:: planned
touches:: [[code:hiker/trees/types]]
note:: `core/src/trees/types.rs::EditableNode` — `id` (ephemeral parse handle, never persisted), `parent`, `kind` (`Cluster` / `Leaf` / `OutlierBucket`), `note_path` (leaf's wikilink target), `name`, `summary`, `policy` (`NodePolicy::Tag` / `Move` / `Freeze`, parsed from the heading's inline attribute), `confidence`, `summary_membership_churn`. No `user_edited_*` flags — regeneration keys on the `Cluster N` placeholder pattern. Centroid in `index.db` (per [[spec:trees-centroids-index]]). Hydrated by parsing the body outline (clusters by heading/bullet, children by nesting, leaves by wikilink)

### Editing surfaces

A tree has two editing surfaces over the one op-log document; neither is a bespoke dual-editor: [tree-edit-via-tabs]
status:: planned
note:: Two editing surfaces over one op-log doc: the tree `.md` as an ordinary editor tab (inherits find / wikilink-hover / fonts / parse-error decorations) and the graphical tree/graph tab. Both-at-once via existing split-view + [[spec:tabs-linked-targeting]]; the graphical view re-derives leniently from the doc. No bespoke dual-editor or sync layer

- The **markdown surface** is the tree `.md` opened as an ordinary editor tab. The outline is plain markdown, so it inherits every editor affordance for free — find-in-note, wikilink hover-preview, the configured fonts, and the parse-error decorations from the parse tiers above. This is the "get into the md, paste a cluster in or out" surface.
- The **graphical surface** is the tree / graph tab (the row and node-link views below). A graphical reshape serializes to a localized body edit on the same doc.

Both are views of one `Y.Text`, reconciled by the op-log like any other concurrently-edited document — there is no tree-specific sync layer. The graphical view re-derives from the doc on change and parses leniently, so a mid-typing malformed line shows as an editor decoration without disturbing the graphical view. Having both open at once is the existing split-view and linked/targeting-tabs machinery ([[spec:tabs-linked-targeting]]), not a new widget: split the editor tab beside the graph tab, or switch between them.

### Authoring a tree by hand

A tree need not come from a clustering run. **New tree → Empty** creates a tree doc with metadata frontmatter and an empty body; the user then writes clusters as headings and adds notes as wikilinks — either by typing the outline in the markdown surface, or by multi-selecting notes ([[spec:note-multi-select]]) and dropping them into a cluster in the graphical surface. A two-cluster tree (one per project), each carrying a `{tag: …}` or `{move: …}` policy, is the canonical by-example classifier: saved as triage, it routes future notes by similarity to the curated members. Centroids derive from the hand-assigned members ([[spec:tree-cluster-path-identity]]), so the placement classifier works against a hand-built tree exactly as against a generated one. [tree-author-blank]
status:: planned
note:: "New tree → Empty" creates a metadata-only tree doc with an empty body; the user authors clusters as headings and adds notes as wikilinks (typed, or via [[spec:note-multi-select]] drop). A hand-built two-cluster policy tree is the canonical by-example triage classifier; centroids derive from hand-assigned members

### Multi-placement

A note appears once per cluster by default — clustering produces a strict partition. Because a leaf is just a wikilink, a user can also place the same note under a second cluster (the same `[[path]]` under two headings) for notes that genuinely span clusters. Policy resolution on a multiply-placed note unions its **Tag** policies (the note collects every matching tag) and, for conflicting **Move** policies, takes the highest-confidence cluster's target, else routes to review. Automatic soft/overlapping placement by the algorithm is deferred — it arrives with the GMM soft-assignment work in `clustering.md`. [tree-multi-placement]
status:: planned
note:: Default strict partition (a note appears once). The user may manually place the same wikilink under a second cluster. Conflict rules: Tag policies union; conflicting Move takes the highest-confidence cluster, else routes to review. Automatic soft/overlapping placement is deferred to the GMM work


## Clustering review tab

Building a new tree and reclustering a subtree both go through a `cluster-review` app-page tab, not a modal. The tab is the configuration surface, the runner, and the structural-result reviewer; nothing about the structural result is written to disk until a single Confirm action persists it as a `draft` `.md` with placeholder names and flips the tab to the cluster editor pane. [cluster-review-tab]
status:: done
note:: New `cluster-review` app-page tab kind; full configure → run → review → confirm flow lives in `app/src/panels/cluster_review/mod.rs`. Backend commands `cluster_run_structural` + `cluster_persist_built_tree` + `cluster_op_recluster_subtree_from_built` in the host

Entry points:

- The Clusters accordion-header `+` split-button ([[spec:cluster-editor-new-tree-action]]) opens a fresh tab — its primary click with default params, its caret-dropdown presets prefilling the form ([[spec:cluster-preset]]). [cluster-review-tab-from-new-tree-action]
status:: done
note:: Header `+ Suggest reorganization` button (`app/src/sidebar/clusters/mod.rs`), the sidebar `+`-button (clusters mode), and the mode-menu "New tree…" entry all open the `cluster-review` tab
- The row-menu's "Recluster subtree…" entry on a cluster row ([[spec:cluster-editor-recluster-subtree]]) opens a tab pre-bound to that `(tree_id, node_id)`. [cluster-review-tab-from-recluster-action]
status:: done
note:: The cluster-row menu's "Recluster subtree…" entry (`app/src/sidebar/clusters/mod.rs`) opens the `cluster-review` tab in recluster-subtree mode. Backend `cluster_op_recluster_subtree` command is preserved for tests / CLI
- The mode menu's "Rebuild" entry on an Evergreen tree opens a tab prefilled with the tree's saved scope/method/params. [cluster-review-tab-rebuild-prefill]
status:: partial
note:: `clusterReviewTab::open({ kind: "rebuild", treeId })` fetches the tree row via `cluster_trees_list` and pre-populates name + scope + method + cluster params from the saved JSON. **Gap:** the host doesn't yet wire a "Rebuild" entry point on the mode menu / Evergreen tree-row menu, so the tab is only reachable programmatically (e.g. from a future menu entry). The mechanism lands here; the surface entry is the follow-up

### Tab kind

New entry in the [[spec:tab-kinds]] enumeration (`editor.md`): `cluster-review`. Payload is a `ClusterReviewState` carrying the tab's purpose (`new-tree` | `recluster-subtree { tree_id, node_id }` | `rebuild { tree_id }`), the in-flight `ClusterParams` / `BuildMethod` / `BuildScope`, and — once Run has been clicked — the in-memory `BuiltClusterTree` from the structural pass. Non-buffer; editor toolbar and status bar hide on activation per [[spec:tab-kinds]]. Opens sticky (directed action), like Properties. The autosave tab-state machinery ([[spec:autosave-tab-state-store]]) records the tab kind + the configuration form; an in-memory result is *not* persisted across restarts — reopening the tab returns to the configure phase with the form prefilled. [cluster-review-tab-kind]
status:: done
note:: `TabKind` gains `cluster-review`. The per-tab review state (form + result + userRenamed) lives keyed by tab. Non-buffer; sticky; autosave records the kind + synthetic path key only (the in-memory `BuiltClusterTree` is dropped). **Gap (minor):** the autosave shape doesn't carry the user-filled form fields, so restore lands on defaults rather than the previous form values — the spec says "form recorded, result not"; landing the form payload requires widening `core::autosave::TabState` and is left as a follow-up

### Layout

```
┌─ Cluster review: new tree ──────────────────────────────────────────┐
│ [Run clustering]  [Confirm and name →]  [Discard]              [✕] │
│                                                                       │
│ ▼ Configuration                                                       │
│   Name:                [2026-05-08 reorg                ]             │
│   Lifecycle:           (●) Sapling — one-shot                        │
│                        ( ) Evergreen — save as triage classifier      │
│   Scope:               (●) Whole vault                                │
│                        ( ) Current folder: research/                  │
│                        ( ) Selected notes (12 selected)               │
│   Method:              (●) Cluster (RAPTOR-shaped)                    │
│                        ( ) From folders                               │
│   Include outliers:    [✓]                                            │
│   ▶ Advanced                                                          │
│                                                                       │
│ ▼ Result (5 clusters, 142 notes, 7 outliers) — structural only       │
│   ▼ Cluster A (24)                                                    │
│     ├── inbox/whisper-notes.md                                        │
│     ├── inbox/voyage-vs-bge.md                                        │
│     └── … and 22 more                                                 │
│   ▶ Cluster B (18)                                                    │
│   ▶ Cluster C (11)                                                    │
│   ▶ Outliers (7)                                                      │
└─────────────────────────────────────────────────────────────────────┘
```

### Configuration section

Always-visible at the top of the tab. Default-expanded; collapses to a one-line summary once a result has been produced so the result panel gets the real estate. [cluster-review-tab-config-section]
status:: done
implements:: [[code:hiker/clusters/panel/impl#[`Review<'_>`]show]]
note:: Configuration section in `app/src/clusters/panel/mod.rs`: name, lifecycle (hidden for recluster), scope (hidden for recluster), method, include-outliers, advanced disclosure. Auto-collapses to a one-line summary after the first successful Run (but stays open across live-preview re-runs); user can re-expand via the section header. **Layout:** when expanded the form renders in a fixed-width left column (≈42% clamped 300–440px) beside the result view rather than stacked above it, so the graph stays visible while tuning; collapsed, the one-line summary stacks above the full-width result

- **Name** — user-editable string, default `"<YYYY-MM-DD> reorg"` for new-tree, `"Recluster <selected node name>"` for recluster, the tree's existing name for rebuild.
- **Lifecycle** — Sapling / Evergreen radio. Sets the post-confirm default action (Apply for Sapling, Save-as-triage for Evergreen). Stored on the tree's row as a hint; doesn't constrain operations. Hidden in the recluster case (subtree reclustering reuses the parent tree's lifecycle).
- **Scope** — Whole vault / Current folder / Selected notes radio. Defaults from current file-tree context (`Vault` when nothing selected, `Folder(current)` when a folder is selected, `Notes(selected)` when notes are multi-selected in the file tree, per [[spec:note-multi-select]]). Per [[spec:cluster-build-scope]]. Hidden in the recluster case (scope is fixed to the selected subtree's leaves). [cluster-editor-build-scope-picker]
status:: done
note:: Whole-vault / Folder picker now lives in the clustering review tab's Configuration section (`app/src/panels/cluster_review/mod.rs`). Folder-rel text input appears when the Folder radio is selected. The Selected-notes scope remains UI-deferred — backend `BuildScope::Notes` is still wired through `cluster_run_structural` for the recluster case
- **Source types** — checkbox group for the file-extension filter that applies after scope: Markdown (.md) and Plain text (.txt). Default both checked. At least one must be selected before Run; an empty selection is flagged with an inline toast rather than firing the IPC with an empty input set. The selection rides through to the persisted tree's `scope` frontmatter so the triage classifier honors it on subsequent saves (a Markdown-only saved tree no-ops on `.txt` note saves). Visible on every purpose including recluster — a recluster of a mixed-type subtree may want to narrow to one type. Per [[spec:cluster-build-scope-source-types]].
- **Algorithm** — single-combobox dropdown with `HDBSCAN` (default), `Leiden`, `Hybrid`, `GMM (falls back to HDBSCAN)`, and `From folders`. The choice is both the partitioner pick (for the semantic options) and the underlying `BuildMethod` switch (`From folders` selects `BuildMethod::FromFolders`; everything else selects `BuildMethod::Cluster { params }`). No separate "Build method" picker exists. Selecting an algorithm reactively refreshes the tunables shown below — no manual re-render. Forced to a semantic algorithm in the recluster case, matching the surrounding-tree-method-agnostic posture of [[spec:cluster-build-from-folders-uniform-output]]. [cluster-review-tab-method-dropdown]
status:: done
implements:: [[code:hiker/clusters/panel/impl#[`Review<'_>`]render_config_form]]
note:: Single `egui::ComboBox` "Algorithm" picker in `app/src/panels/cluster_review/mod.rs` configuration form — options HDBSCAN / Leiden / Hybrid / GMM / From folders. `From folders` is folded into the algorithm dropdown (no separate "Build method" picker); selecting it switches the run-time builder to `BuildMethod::FromFolders`. The standalone `ReviewMethod` enum and `cfg.method` field were removed. Algorithm choice rides through autosave via `ReviewConfig.algorithm`. Reactive refresh: Leiden surfaces `k_nearest` + `edge_weight_floor` + `resolution` + `iterations` + `min_cluster_size`; HDBSCAN/Hybrid/GMM surface `min_cluster_size` + `min_samples`; FromFolders surfaces `outlier_threshold`. Common tunables across semantic algorithms (`summary_confidence_threshold`, `disable_recursion`) render unconditionally except under FromFolders. Tunables now sit at the same level as the rest of the config (the prior "Advanced" `collapsing` wrapper was dropped)
- **Advanced (grouped by engine stage)** — a collapsible disclosure whose controls are grouped and labeled by which part of the engine they drive, so it's clear what each knob affects: [cluster-review-tab-advanced-grouped]
status:: planned
note:: The review tab's Advanced disclosure groups controls by engine stage — Algorithm (partition tunables) / Recursion ([[spec:cluster-review-tab-recursion-mode]]) / Representation ([[spec:cluster-representation]]) — each labeled so it's clear which part of the engine a knob drives. Naming is a top-level 3-way control, not in this disclosure
  - **Algorithm** — partition tunables for the selected algorithm. HDBSCAN / Hybrid / GMM: `min_cluster_size` (halved in the recluster case) + `min_samples`. Leiden: `k_nearest`, `edge_weight_floor`, `resolution` (γ), `iterations`, Leiden-flavored `min_cluster_size`. From folders: `outlier_threshold` (when `Include outliers` is on). Plus `summary_confidence_threshold`.
  - **Recursion** — the `recursion` mode (Flat (default) / Manual-only / Auto) and, under Auto, `max_depth` / `leaf_min_size` / `leaf_cohesion_threshold` ([[spec:cluster-review-tab-recursion-mode]]).
  - **Representation** — what each note is reduced to before partitioning: Centroid (default) / Lexical (deterministic). Summary-embedding is offered only on Roll-up, not the note-level Split ([[spec:cluster-representation]]).
  
  Only the Algorithm group depends on the algorithm dropdown; Recursion and Representation are algorithm-agnostic. Defaults match `clustering.md`.
- **Include outliers** — top-level toggle (default `on`). Same semantics as the old modal; applies to both semantic algorithms and From folders.
- **Carry policies down** — recluster-only checkbox (default `off`). When set, the selected node's resolved policy is copied as explicit policy onto every newly-built direct child. Per [[spec:cluster-editor-recluster-subtree-policy-loss]].
- **Naming** — a 3-way control (Deterministic (default) / LLM / None) applied at Confirm. Deterministic names extractively on-device (no LLM); LLM submits per-cluster `RaptorSummarize` tasks for placeholder-named clusters at `Priority::Normal` (greyed with a settings tooltip when `[llm].enabled = false`); None leaves `Cluster N` placeholders for later. The structural pass itself never names — naming applies only at Confirm. [cluster-review-tab-confirm-with-naming-toggle]
status:: done
implements:: [[code:hiker/clusters/panel/impl#[`Review<'_>`]render_config_form]], [[code:hiker/clusters/panel/impl#[`Review<'_>`]confirm]], [[code:hiker/clusters/panel/impl#[`Review<'_>`]submit_naming_tasks]]
touches:: [[code:hiker/trees/build_adapter]]
note:: "Name clusters with LLM after Confirm" checkbox in the configuration section of `app/src/panels/cluster_review/mod.rs` (default off). When checked, Confirm submits per-cluster `RaptorSummarize` task-queue tasks via `submit_naming_tasks` (in `panels/cluster_review/mod.rs`) over non-user-renamed clusters — same envelope/priority as the legacy `submit_naming_tasks`. The freshly persisted node list (post `Trees::insert_nodes`) is filtered to `NodeKind::Cluster && !user_edited_name`, preserving inline-renamed clusters, and inherits the child-then-parent insertion order from `core/src/trees/build_adapter.rs::node_inserts`. Submission is spawned onto the host tokio runtime so the egui sync thread doesn't block. Greyed out with tooltip when `[llm].enabled = false`; when disabled, the boolean is force-reset to false defensively

### Recursion mode

The **Recursion** group (Advanced, when `Method == Cluster`) sets `ClusterParams.recursion` to one of three modes ([[spec:cluster-recursion-modes]]): **Flat** (default — `build_cluster_tree` returns after the single top-level partition: leaf clusters plus the outlier bucket), **Manual** (same flat build; the user deepens specific clusters with the row-menu Split verb), or **Auto** (recurses every branch until `max_depth` / `leaf_min_size` / `leaf_cohesion_threshold` trips). Flat is default — a legible single level the user deepens deliberately beats an opaque pre-built hierarchy. The mode persists in the tree's `method` frontmatter. [cluster-review-tab-recursion-mode]
status:: planned
note:: The Recursion group sets `ClusterParams.recursion` to Flat (default) / Manual / Auto ([[spec:cluster-recursion-modes]]); under Auto it exposes `max_depth` / `leaf_min_size` / `leaf_cohesion_threshold`. Persists in the tree's `method` frontmatter. Replaces the `disable_recursion` checkbox

### Run clustering

The `Run clustering` button kicks off the structural-only pass on a background task: the Split step of [[spec:cluster-build-recipe]] in the configured recursion mode (Flat by default), with naming deferred to Confirm so the pass runs without any summarizer. The UI is non-blocking — the user can scroll the Result panel, edit other tabs, or tweak un-locked configuration fields while the pass runs. The result is a `BuiltClusterTree` whose clusters have placeholder names (default: `"Cluster N"` numbered in member-count-descending order at the end of the pass) and empty summaries. The result lives in the tab's in-memory state — no `.md` file is written. [cluster-review-tab-run-clustering]
status:: done
implements:: [[code:hiker/cluster/build/tree_structural]]
note:: Run button in `app/src/panels/cluster_review/mod.rs` calls `cluster_run_structural` which in turn calls `core::cluster::build_tree_structural` (forces `SummarizeMode::None` on the method's params and post-processes leaf-level cluster names to `"Cluster N"` member-count-descending). Result lives on the tab's in-memory state; no tree `.md` is written

The pass runs asynchronously so the UI stays live while it streams. [cluster-review-tab-async-pass]
status:: done
implements:: [[code:hiker/clusters/panel/impl#[`Review<'_>`]show]], [[code:hiker/clusters/panel/impl#[`Review<'_>`]run_structural_streaming]]
touches:: [[code:hiker/clusters/panel]]
note:: `app/src/panels/cluster_review/mod.rs::run_structural_streaming` kicks off `core::cluster::build_tree_structural_streaming` on a background tokio `spawn_blocking` worker (the egui frame loop enters the host runtime each tick so `Handle::current()` is live at call time). The pane stows the `tokio::sync::mpsc::Receiver<BuildEvent>` in an `Arc<Mutex<>>`; `drain_events` pulls events via `try_recv` at the top of each `show()` call and `ui.ctx().request_repaint()` keeps the frame loop ticking while the build is alive. The blocking `build_tree_structural` shim stays for tests

Subsequent Run clicks discard the prior result and progress, then restart the pass with the current form values. [cluster-review-tab-iterate]
status:: done
note:: Subsequent Run clicks clear `result` + `userRenamed` (`app/src/panels/cluster_review/mod.rs`) and re-run with the form's current values. No LLM cost per iteration since the structural pass doesn't invoke a summarizer

The build pipeline supports a `summarize: SummarizeMode::None` short-circuit path that skips the `Summarizer` call entirely so the structural pass runs end-to-end without an LLM. [cluster-review-tab-structural-pass-no-llm]
status:: done
implements:: [[code:hiker/cluster/build/run_summarizer]], [[code:hiker/cluster/build/NoopSummarizer]], [[code:hiker/cluster/build/tree_structural]]
note:: `core::cluster::build_tree_structural` (new) routes through `build_tree` with a `NoopSummarizer`; `run_summarizer` for `SummarizeMode::None` emits empty strings and never reaches the summarizer trait. Public surface: `pub fn build_tree_structural`, `pub struct NoopSummarizer`. No LLM client / `[llm].enabled` required for the structural pass

While the pass is running, the Run button flips into a Cancel button — pressing it aborts the in-flight pass cleanly and drops any partial results. The button is the only progress affordance the user has to interact with; everything else (phase label, counters, partial tree) renders into the dedicated progress + result surface described below. [cluster-review-tab-cancel-pass]
status:: done
implements:: [[code:hiker/clusters/panel/impl#[`Review<'_>`]show]], [[code:hiker/clusters/panel/impl#[`Review<'_>`]cancel_run]]
note:: While running, the Run button flips into Cancel. Cancel flips the shared `Arc<AtomicBool>` (via `Ordering::Relaxed`); the background task notices on its next per-iteration check and emits `BuildEvent::Cancelled`. `drain_events` applies the terminal event: clears the live-reveal cache, drops `result`, surfaces a toast. Discard / tab-close also flip the atomic so the background task wakes promptly

### Progress surface

A progress row sits between the configuration section and the result panel while a run is in flight. It surfaces:

- **Phase indicator** — current pipeline phase (e.g. `loading embeddings`, `partitioning level 0`, `partitioning level N`, `finalizing`). Phase strings come from the build pipeline's progress stream ([[spec:cluster-build-progress-stream]] in `clustering.md`); the UI renders them verbatim plus a small inline spinner.
- **Counters** — items processed, clusters discovered so far, outliers so far. Updated as the stream emits new counter events.
- **Elapsed time** — seconds since Run was pressed. Useful for tracking long pulls on big vaults.

The progress row hides when no run is in flight. It does not persist across re-runs — each Run resets it. [cluster-review-tab-progress-row]
status:: done
note:: `render_progress_row` paints between Configuration and Result while `pane.running`. Spinner + phase label (rendered verbatim from `Phase` events via `phase_label()`: `loading embeddings` / `partitioning level N` / `finalizing`) + counters (items / clusters / outliers from `Counters` events) + elapsed seconds (since `pane.started_at = Instant::now()` set on Run). Hides when no run active

### Result panel

Renders the in-memory `BuiltClusterTree`. Two view variants are available via a Tree / Graph toggle in the panel's toolbar; markdown view is not surfaced here (no LLM names yet). [cluster-review-tab-result-view-toggle]
status:: done
implements:: [[code:hiker/clusters/panel/result/impl#[`Review<'_>`]render_result_panel]]
note:: View toggle row above the result body in `render_result_panel` (`Tree` / `Graph` selectable_value pair). State lives on `ReviewPane.view` so it survives within the tab session. Markdown view not surfaced

View options — current variant (Tree vs. Graph), per-cluster expand chevron state, and any per-view layout state — persist across re-runs within the same tab session: the user keeps the layout they were working in even after pressing Run again. Stale node ids in the expand set from a prior run are harmless (lookups against the new run's ids just return false). [cluster-review-tab-view-state-survives-rerun]
status:: done
note:: The Run reset path in `app/src/panels/cluster_review/mod.rs` leaves `pane.view` (Tree/Graph) and `pane.expanded` (chevron set) intact across re-runs — only the in-flight build state (`result`, `live_top`, `live_pending_children`, `phase`, `counters`, `started_at`, `user_renamed`, `editing`) is cleared. Stale node ids in `expanded` from a prior run are harmless: lookups against the new run's fresh ids return false

**Tree view (default).** Hierarchical row layout reusing [[spec:cluster-editor-row-primitive]] minus the affordances that don't apply pre-persistence (no policy chip — policies don't exist on un-persisted trees; no drag-handle — structural results are immutable from this surface; no staleness badge — `summary_membership_churn` doesn't apply). Per-cluster row shows: placeholder name (`"Cluster 1"`, `"Cluster 2"`, …), member count, centroid radius / confidence indicator, and an expand chevron when the cluster has children (nested sub-clusters or leaf members). Outliers render as a sibling row at the bottom of the root level with the distinct outlier glyph. [cluster-review-tab-structure-preview]
status:: done
note:: `renderResult` paints leaf clusters in member-count-descending order with placeholder name, member count, radius indicator, and a first-3-titles sample (resolved via `note_titles` map returned alongside the build DTO). Outliers render as a sibling block with the dashed border. **Gap:** does not reuse [[spec:cluster-editor-row-primitive]] (`renderNode`) — instead carries a thin variant in-module; the spec's row-primitive extraction note already flagged the extraction as deferred

Expanding a cluster reveals its members inline — nested sub-cluster rows + leaf-note rows, both using the row primitive at deeper indent. Leaves are click-to-open same as the sidebar form. Pane-local `expanded: Set<NodeId>` keyed by the in-memory build run's node ids; switching to graph view and back restores the expansion state. [cluster-review-tab-result-expand]
status:: done
implements:: [[code:hiker/clusters/panel/result/impl#[`Review<'_>`]render_cluster_row]]
note:: Each cluster row carries a `▶`/`▼` chevron. `ReviewPane.expanded: HashSet<NodeId>` tracks which rows are expanded; toggling switches between graph and tree views preserves the set. Expanded branch clusters render their sub-clusters inline via recursive `render_cluster_row` at deeper indent; leaf clusters render their note members (capped at 50 rows before a "and N more" footer). The expand affordance replaces the legacy flat first-3-titles sample

**Graph view.** Node-link rendering of the built tree via the shared egui force-graph widget (per [[spec:cluster-editor-renderer-reuse]]). Same size-by-members and label-style encoding as [[spec:cluster-editor-graph-view]], minus the policy color (no policies) and staleness tint (no churn). Layout default is radial; layout selection rides through the cluster editor's view-menu pattern when the panel is in graph mode. Pan / zoom / hover-detail / click-to-pin-preview behavior is shared with the cluster editor's graph view. Useful at the build-review stage for spotting structural pathologies at a glance — single giant cluster, fragmented outliers, depth imbalance — that the row view buries under expand chevrons. The result graph's view/eye menu also carries the review tab's **Live preview** toggle ([[spec:cluster-review-tab-live-preview]]) — a display control over the debounced live re-cluster, kept here rather than in the clustering-config knobs because those knobs hold only clustering-engine params (HDBSCAN / Leiden tunables) while the view menu owns display controls. The companion display control **Anchor stiffness** ([[spec:force-cfg-anchor-stiffness]], how strongly retained nodes hold their prior spot across a force re-layout) lives in the same view menu. [cluster-review-tab-result-graph-view]
status:: done
implements:: [[code:hiker/clusters/panel/result/impl#[`Review<'_>`]render_graph_view]], [[code:hiker/clusters/panel/result/impl#[`Review<'_>`]built_tree_to_editable_nodes]], [[code:hiker/panels/cluster_graph/show_with_nodes]]
note:: Graph variant in `app/src/panels/cluster_review/mod.rs::render_graph_view` synthesises `Vec<EditableNode>` rows from the in-memory `BuiltClusterTree` (or the live-reveal `live_top` + `live_pending_children` buffers mid-pass) via the local adapters `built_tree_to_editable_nodes` / `live_to_editable_nodes`, then hands them to the new `panels::cluster_graph::show_with_nodes` entry point (the persisted-tree `show` is now a thin wrapper that resolves nodes from the tree `.md` and delegates). Synthesised rows use the spec's encoding rules — placeholder ids, `user_edited_name` reflects the pane's `user_renamed` map, no policy (color), `summary_membership_churn = 0` (no staleness tint). Layout cache is keyed `"review:<tab_id>"` so it never collides with persisted trees. Leaf clicks are disabled (the leaf `note_ref` is a vault-relative path, not necessarily `read_store`-addressable pre-Confirm); pan/zoom/hover-detail are reused unchanged from the persisted renderer

**Live cluster reveal.** Cluster rows appear in the result panel as the partitioner discovers them, not only after the full pass completes. Each top-level cluster's row appears as soon as the partitioner emits it; recursive sub-splits populate the cluster's children inline (driving the expand affordance). The user can browse the partial tree mid-build. The final member-count-descending ordering applies only after the pass finishes; mid-pass ordering is insertion-order so rows don't shuffle while the user is watching. The graph view re-renders incrementally on the same stream. [cluster-review-tab-live-cluster-reveal]
status:: done
implements:: [[code:hiker/clusters/panel/result/impl#[`Review<'_>`]render_tree_view]]
touches:: [[code:hiker/clusters/panel]]
note:: `ClusterDiscovered` events feed `live_top` (`parent = None`) and `live_pending_children` keyed by parent id (child-first ordering; the buffer-by-parent trick documented in the backend's [[spec:cluster-build-progress-stream]] notes). Tree view iterates `live_top` mid-pass in insertion order; on `Done`, the final `BuiltClusterTree` replaces it and a member-count-descending sort applies to the leaf level. Graph view is stubbed (see [[spec:cluster-review-tab-result-graph-view]]) — incremental graph re-render not wired

The placeholder names are user-editable inline before Confirm — clicking a placeholder opens the same inline-edit affordance as [[spec:cluster-editor-edit-name-summary]]. A renamed cluster no longer matches the placeholder pattern, so the (opt-in) LLM naming pass at Confirm time skips it. Summaries cannot be edited here (no summary to edit yet). [cluster-review-tab-rename-before-llm]
status:: done
implements:: [[code:hiker/clusters/panel/result/impl#[`Review<'_>`]render_cluster_row]]
note:: Click a placeholder cluster name in the result panel to open an inline editor (`app/src/panels/cluster_review/mod.rs`); user-renamed names live in the per-tab rename map and ship to `cluster_persist_built_tree` / `cluster_op_recluster_subtree_from_built` so the new rows land with `user_edited_name = 1` and the `RaptorSummarize` task pass skips them

### Confirm

A single **Confirm** button (disabled until a Run has produced a complete result) persists the structural tree and lands the user on the cluster editor pane. Clustering ends here. LLM naming is **not** part of the Confirm flow by default. Nothing is written to disk until this Confirm. [cluster-review-tab-no-persistence-until-confirm]
status:: done
note:: `cluster_run_structural` returns a DTO that lives only in the cluster-review module's per-tab state. no tree `.md` is written until Confirm. Closing the tab drops the pane state via `clusterReviewTab.dropTab`; autosave only persists `(kind, path-key)`, not the form or result

Steps:

1. Persists the in-memory `BuiltClusterTree` as a new tree `.md` file (state `draft`, source per Sapling/Evergreen lifecycle), writing the outline body. User-renamed clusters carry their names; every other cluster lands with its placeholder name (`"Cluster 1"`, `"Cluster 2"`, …) intact.
2. Applies the configuration's **Naming** mode (per [[spec:cluster-review-tab-confirm-with-naming-toggle]]): **Deterministic** names placeholder clusters extractively on-device; **LLM** submits one `TaskKind::RaptorSummarize { tree_id, cluster_node_id, level }` task per placeholder cluster at `Priority::Normal` (same queue path as [[spec:cluster-editor-regenerate-via-task-queue]]); **None** leaves placeholders. Already-named clusters are skipped either way.
3. Flips the tab's kind from `cluster-review` to `cluster-batch-review` for the new tree, with sub-mode `cluster-tree`. The user lands inside the cluster editor pane ([[spec:cluster-editor-pane-mode]]); the sidebar's Cluster trees mode picks up the new tree on its next refresh.

The Confirm button itself is never gated on `[llm].enabled` — the structural tree persists regardless. The naming toggle is greyed out when LLM is disabled. [cluster-review-tab-confirm-single-path]
status:: done
implements:: [[code:hiker/trees/build_adapter/node_inserts]], [[code:hiker/clusters/panel/impl#[`Review<'_>`]show]], [[code:hiker/clusters/panel/impl#[`Review<'_>`]confirm]]
note:: Single `Confirm` button in `app/src/panels/cluster_review/mod.rs` replaces the old dual Confirm-and-name / Confirm-no-naming pair. Always persists the structural tree (placeholder names intact unless inline-renamed) and lands on the cluster sidebar via `app.clusters.selected_tree` (egui port has no separate cluster-pane tab kind yet). Never gated on `[llm].enabled`. Naming branch is opt-in via the new toggle (see [[spec:cluster-review-tab-confirm-with-naming-toggle]]) — when unchecked, Confirm skips the LLM step entirely

On Confirm the tab transitions to the cluster editor pane for the newly-persisted tree (step 3 above). [cluster-review-tab-transition-to-pane]
status:: done
implements:: [[code:hiker/clusters/panel/impl#[`Review<'_>`]confirm]]
note:: On Confirm the tab is closed and a `cluster-pane` tab opens for the new tree id (transition handled in `app/src/panels/cluster_review/mod.rs`). User stays on the same visible tab; the chrome swaps to the cluster editor pane for the newly-persisted tree. **Note:** implemented as close-and-open rather than literal in-place kind flip — the two tabs are keyed differently so a flip would conflict with the cluster-pane's per-tree dedup

The cluster editor pane is the canonical surface for the (optional) naming follow-up: its toolbar's **Name clusters with LLM** button (a contextual rename of the existing Regenerate names verb when the tree's clusters still carry placeholder names) is the primary affordance. Same task-queue path as [[spec:cluster-editor-regenerate-via-task-queue]]. The user is free to skip naming entirely — live with placeholder names, edit them by hand from the cluster pane's inline-edit, etc. [cluster-editor-pane-name-clusters-cta]
status:: planned
implements:: [[code:hiker/clusters/sidebar/tree/impl#[`ClusterCtx<'_, '_>`]toolbar]]
note:: The cluster editor pane's toolbar surfaces a prominent "Name clusters with LLM" button (a contextual rename of the existing Regenerate names verb when the tree's clusters still carry placeholder names). Pressed = same task-queue flow as [[spec:cluster-editor-regenerate-via-task-queue]]. Becomes the canonical surface for the optional LLM-name follow-up after a placeholder-name Confirm. Lands in `app/src/sidebar/clusters/toolbar.rs` / wherever the egui cluster pane toolbar is wired

For the recluster-subtree case, step 1 is "replace the selected node's descendants with the new subtree" (per [[spec:cluster-editor-recluster-subtree]]) instead of "insert a new tree row"; step 3 returns the user to the existing tree's cluster editor pane. The optional naming step applies identically — it submits naming tasks for the new sub-clusters only.

For the rebuild case, step 1 is "write the new tree alongside the existing one"; the old tree is left intact for the user to discard manually (matches the existing [[spec:cluster-build-rebuild]] posture).

### Discard

The `Discard` button (or closing the tab) drops the in-memory result and the form state. If a result exists, closes are gated by a `confirm` ("Discard the clustering result? You'll need to re-run to get it back.") — closing the tab with no result skips the prompt. Nothing on disk is touched. [cluster-review-tab-discard]
status:: done
touches:: [[code:hiker/workbench_host]]
note:: Discard button in `app/src/panels/cluster_review/mod.rs` fires a danger confirm (when a result exists) and then closes the tab. Closing the tab via the tab strip (× / middle-click / `Mod-w`) goes through the workbench host's close path (`app/src/workbench_host.rs`) which intercepts cluster-review tabs with unsaved results and fires the same confirm prompt

### One review tab per target

Opening "Suggest reorganization" with a `cluster-review` tab already open for the new-tree case activates the existing tab rather than spawning a duplicate (same shape as Properties' one-tab-per-path rule). Recluster tabs key on `(tree_id, node_id)`; rebuild tabs key on `tree_id`. The form state and any in-flight result are preserved when re-activating. [cluster-review-tab-deduplication]
status:: done
note:: One review tab per `(purpose, target)`: new-tree is a singleton (key `__cluster-build progress events:new-tree`); recluster keys on `__cluster-build progress events:recluster-subtree:<treeId>:<nodeId>`; rebuild keys on `__cluster-build progress events:rebuild:<treeId>`. `openClusterReviewTab` activates the existing tab + preserves form + in-flight result if a dedup match is found


## Operations

All operations are local edits to the tree's outline body, committed through the op-log granular user-save path (the semantic op name rides the op `metadata`). None of them mutate the vault — only Apply or a staging-row accept does that. Every edit is undoable until apply. [cluster-editor-node-operations]
status:: partial
note:: umbrella tracked across the slugs below. Sprint B lands every reshape op end-to-end (move / merge siblings / merge-children-up / split / rename / edit summary / drop cluster / promote outlier / set policy) via `core/src/trees/` submodule methods + `cluster_op_*` commands + `app/src/sidebar/clusters/mod.rs`. **Partial:** drag-drop affordance is row-menu "Move to…" for Sprint B; full drag-drop lives in the expanded pane mode (Sprint C)

- **Move note between clusters.** Drag a leaf onto a different cluster row (or use the row menu's "Move to…"). Updates the leaf's `parent`. Centroids of source and target clusters are recomputed. [cluster-editor-move-note-between-clusters]
status:: done
touches:: [[code:hiker/trees/ops/move_node]]
note:: `core/src/trees/ops/move_node.rs::move_node` + `cluster_node_move` command + the move-target picker in `app/src/sidebar/clusters/mod.rs` (row menu "Move to…" + leaf-promotion-out-of-outlier path) + drag-drop on rows (per [[spec:cluster-editor-dnd-reparent]]). Each edit appends a `move` history row with the prior parent for undo. Multi-drag loops the move per item so each gets its own history entry
- **Merge sibling clusters.** Multi-select 2+ clusters at the same level → toolbar "Merge siblings" → creates one cluster whose members are the union, name and summary are auto-regenerated (queues a task in `core::tasks` for naming) unless the user names it explicitly first. [cluster-editor-merge-siblings]
status:: partial
implements:: [[code:hiker/trees/ops/merge/impl#[Db]merge_siblings]]
note:: `core/src/trees/ops/merge.rs::merge_siblings` (transactional re-parent + delete, full undo snapshot) + `cluster_op_merge_siblings` command + multi-select toolbar's `Merge siblings` button in `app/src/sidebar/clusters/mod.rs`. The survivor inherits every absorbed cluster's children; absorbed cluster rows are deleted from `cluster_nodes`. **Partial:** the auto-regenerate-name pass via `core::tasks` (per spec line "name auto-regenerated unless the user named it explicitly") routes through the task queue, which lands with [[spec:cluster-editor-llm-actions-via-task-queue]]
- **Merge children up to parent.** Select the parent → "Merge up" → flattens that subtree by one level. Children's leaves become direct members of the parent; children's clusters disappear. The parent's name/summary is unchanged unless explicitly regenerated. [cluster-editor-merge-children-up]
status:: done
implements:: [[code:hiker/trees/ops/merge/impl#[Db]merge_children_up]]
note:: `core/src/trees/ops/merge.rs::merge_children_up` + `cluster_op_merge_children_up` command + node-row menu's "Merge children up" entry. Children of `parent_id` that are themselves clusters have their children re-parented onto `parent_id`, then the now-empty intermediate clusters are deleted. Leaf children of `parent_id` stay put. Undo snapshots both the absorbed cluster rows and each grand-child's prior parent
- **Split a cluster.** Select one cluster → "Split" → re-runs HDBSCAN against just its current members with a tighter `min_cluster_size` (default `max(2, current_min / 2)`). Produces a new layer of children. The user can keep the split or undo. Splitting a cluster preserves the parent's name; the new children get LLM names via the task queue. [cluster-editor-split-cluster]
status:: partial
implements:: [[code:hiker/trees/history/impl#[Db]record_split]]
note:: The row-menu / verb-toolbar UI affordance for the [[spec:cluster-op-split]] primitive. `cluster_op_split` command in the host walks `parent → leaves`, pulls each leaf's note-level embedding from `core::store`, runs `core::cluster::partition` with `min_size = max(2, leaves/4)`, inserts one new cluster row per HDBSCAN label, and reparents each leaf onto its new sub-cluster. Records a `split-cluster` history row via `Trees::record_split` so undo restores. **Partial:** uses a deterministic title-join as the new clusters' names; the spec calls for LLM-named children via `core::tasks` (deferred with the queue wiring). Splits with fewer than two HDBSCAN clusters return an error rather than producing a degenerate single-child fanout. **Gap:** UI invocation is non-recursive (one level only); the recursive Split that drives the build recipe ([[spec:cluster-build-recipe]]) lands as part of [[spec:cluster-op-split]]
- **Recluster subtree.** Select one cluster → row-menu "Recluster subtree…" → opens a `cluster-review` tab bound to that `(tree_id, node_id)`. Same flow as new-tree: configure → run → review → confirm. On Confirm, the structural result **replaces the subtree in place** — every descendant cluster node is deleted, the freshly-built nodes are inserted under the selected cluster, and the leaves re-parent onto their new positions. The selected cluster's own row (id, name, summary, user-edit flags, policy) is preserved; only its descendants change. Differs from Split: Split is one-level and produces a single new layer of children; Recluster subtree is recursive and rebuilds every level beneath the selected node. The form's default `min_cluster_size` is half the surrounding tree's value (same heuristic as Split). Works on `Cluster`-method and `FromFolders`-method trees alike — the rebuilt subtree is always `Cluster`-shaped, matching [[spec:cluster-build-from-folders-uniform-output]]. [cluster-editor-recluster-subtree]
status:: done
implements:: [[code:hiker/trees/history/impl#[Db]record_recluster_subtree]]
note:: UI now routes through the clustering review tab. Row-menu "Recluster subtree…" entry calls `openClusterReviewTab({ kind: "recluster-subtree", treeId, nodeId, nodeName })`; the tab runs the structural pass via `cluster_run_structural` (recluster_target arm) and on Confirm calls `cluster_op_recluster_subtree_from_built` which replaces the subtree the same shape as the legacy `cluster_op_recluster_subtree` (records `recluster-subtree` history row via `Trees::record_recluster_subtree`, namespaced ids `recluster-<node>-<id>`, leaves re-parent through `reparent_many`, selected node row preserved, `carry_policies_down` copies the resolved policy onto direct children) and submits per-cluster `RaptorSummarize` tasks for non-user-renamed nodes. Legacy `cluster_op_recluster_subtree` command is preserved (unused by UI) for tests / CLI
- **Rename cluster** / **Edit summary.** Click name or summary → inline editor → save; the edit writes the heading text / summary paragraph in the outline. [cluster-editor-edit-name-summary]
status:: done
touches:: [[code:hiker/trees/ops/edit]]
note:: `core/src/trees/ops/edit.rs::rename` + `set_summary` (Sprint A) + click-to-edit on cluster names + summary previews in `app/src/sidebar/clusters/mod.rs`. Edits stamp `user_edited_name` / `user_edited_summary` per [[spec:cluster-editor-user-edit-provenance]] and append `rename` / `edit-summary` history rows
- **Drop cluster.** Toolbar action on a selected cluster: drops the cluster, its children's leaves all fall to the nearest outlier bucket. Useful for "this cluster is junk; throw the notes back to inbox-via-outliers." [cluster-editor-drop-cluster]
status:: done
implements:: [[code:hiker/trees/ops/drop/impl#[Db]drop_cluster]]
note:: `core/src/trees/ops/drop.rs::drop_cluster` (DFS over descendants, re-parent leaves onto the outlier bucket, delete cluster + nested cluster rows) + `cluster_op_drop_cluster` command + node-row menu's "Drop cluster" entry. Snapshots every absorbed cluster row + each leaf's prior parent so undo restores the subtree. Toolbar-shaped Drop verb in the multi-select toolbar applies the same op to every selected node
- **Promote outliers / move out.** Drag a row in/out of the outlier bucket. Same machinery as move-note. [cluster-editor-promote-outlier]
status:: partial
implements:: [[code:hiker/trees/ops/move_node/impl#[Db]promote_outlier]]
note:: `core/src/trees/ops/move_node.rs::promote_outlier` + `cluster_op_promote_outlier` command + leaf-row menu's "Promote out of outliers…" / "Send to outliers" entries. Pops a target picker (re-uses `openMoveTargetPicker`) when promoting, lands the leaf back under the outlier bucket on demotion. Records a distinct `promote-outlier` history op so the log reads naturally. Drag-drop in/out also works via [[spec:cluster-editor-dnd-reparent]]'s row DnD — a leaf dropped onto the outlier bucket or vice versa lands via `move_node` (which appends a `move` history row rather than a `promote-outlier` one — the structural effect is identical, the op label differs). **Partial:** DnD takes the `move_node` path rather than `promote_outlier`, so dragged promotions show up as `move` in history. The row-menu path remains the source of `promote-outlier` history entries
- **Set / clear policy.** Click the policy chip on any node → policy editor (radio: `Tag` / `Move` / `Freeze` / `None` + the action's parameter for `Tag` (slug) or `Move` (folder) + a "Require review for new matches" checkbox available on `Tag` and `Move`). Setting a policy on an ancestor automatically becomes the resolved policy for descendants without their own. [cluster-editor-set-policy]
status:: done
note:: `app/src/sidebar/clusters/mod.rs` paints the policy chip popover for every cluster (and outlier-bucket) row with entries Tag… / Move to folder… / Freeze / Clear policy. Each writes through `cluster_node_set_policy` → `core::trees::set_policy` which appends a `set-policy` history row. Tag + Move policies prompt for the action parameter (slug / folder) + a `require_review` confirm
- **Stage move / Stage tag (one-off, multi-select).** With N leaf rows selected, the bulk-action toolbar exposes "Stage move to…" and "Stage tag with…". Each verb writes N rows directly into the op log (one per selected leaf) with `surface = "cluster-editor"`, `action = "move_note"` or `"apply_tag"`, and `metadata.tree_id` for traceability. No node policies are mutated. Distinct from setting an `auto-move` / `auto-tag` policy and clicking Apply: the policy path is the saved-tree mechanism that fires on future matches; "Stage move/tag" is a one-shot batch against exactly the selected leaves. Useful when the user wants to act on an arbitrary subset of a tree without committing to a forever-rule. The proposals flow through the standard staging surfaces (activity-detail Pending filter, tree row indicators, editor toolbar pill) — the cluster editor doesn't gain a private review queue.

  "Stage move to…" stages one pending `Rename` op per selected leaf. [cluster-editor-multi-select-stage-move]
status:: done
implements:: [[code:hiker/suggest/stage_moves]], [[code:hiker/clusters/sidebar/impl#[`ClusterCtx<'_, '_>`]stage_forms_inline]]
touches:: [[code:hiker/suggest]]
note:: `core::suggest::stage_moves` (per [[spec:op-log-reorg-batch]]) + `app/src/sidebar/clusters/mod.rs` multi-select toolbar's "Stage move to…" verb. Stages one pending `Rename` op per selected leaf via `core::ops::op_writes::stage_reorg_batch`, all sharing one cross-document `batch_id`, authored `auto:cluster` with `surface = "cluster-editor"`. Accept/reject flow through the op-log review surfaces (`flip_op_status` / batch `flip_batch_status`); partial apply allowed (a target collision skips that move, not the rest). No-op moves (target == source) skipped

  "Stage tag with…" stages one pending content op per selected leaf. [cluster-editor-multi-select-stage-tag]
status:: done
implements:: [[code:hiker/suggest/stage_tags]], [[code:hiker/clusters/sidebar/impl#[`ClusterCtx<'_, '_>`]stage_forms_inline]]
touches:: [[code:hiker/suggest]]
note:: `core::suggest::stage_tags` + multi-select toolbar's "Stage tag with…" verb. Pre-computes the file content with the tag merged into the configured `tag_field` (default `hiker.suggested_tags`) via `merge_tag_into_frontmatter`, then stages it as one pending `SetFrontmatter`-shaped op via `core::ops::op_writes::stage_auto_content` authored `auto:cluster`
- **Summarize this cluster.** Cluster-row right-click verb. Calls `Trees::summarize` with `scope = Subset { ids: [node_id] }`, `recursive = false`; skips a cluster that already has a non-placeholder name. No-op when the node's name + summary are filled and `summary_membership_churn == 0`. [cluster-editor-summarize-verb]
status:: done
note:: `app/src/sidebar/clusters/mod.rs` (+ `tree.rs`) calls `cluster_summarize` natively with `scope = subset`, `recursive = false`, `summarize_mode = llm`, `overwrite_user_edited = false`. The cluster-row context menu adds a "Summarize" entry (next to Split / Subcluster / Merge children up / Drop cluster); the multi-select toolbar renders a "Summarize" button when the current selection contains at least one cluster row — leaf rows are filtered out before the call, the button is hidden entirely when only leaves are selected. Both surface a toast keyed off `SummarizeSweepOutcome.enqueued` / `skipped_fresh` / `skipped_user_edited`: "Enqueued N cluster summaries — watch the queue for progress" when work was queued, "All N already fresh — nothing to do" when the backend's `StaleOrUnfilled` predicate skipped everything (`skipped_fresh` count is reported). Queue rows fan out per [[spec:cluster-op-summarize-sweep]]
- **Summarize selected.** Multi-select toolbar verb, visible when `selection.size > 0` and at least one selected node is a cluster. Calls `Trees::summarize` with `scope = Subset { ids: <cluster-rows-from-selection> }`, `recursive = false`; skips clusters that already have a non-placeholder name. Leaves in the selection are ignored (leaves don't carry a summary). [cluster-editor-summarize-verb]
- **Summarize new / changed.** Tree-pane toolbar action (sits alongside Apply / Save-as-triage). Calls `Trees::summarize` with `scope = StaleOrUnfilled`, `subtree_root = None`, `recursive = true`. Disabled when zero rows match (everything is fresh). The button shows the pending count when non-zero (e.g. "Summarize new / changed (4)"). [cluster-editor-summarize-stale-action]
status:: done
note:: `app/src/panels/cluster_review/mod.rs` — text button "Summarize new / changed" / "Summarize new / changed (N)" alongside Save-as-triage / Regenerate names / Rebuild in the pane toolbar. Pending count computed from cached nodes by counting cluster rows where `summary_membership_churn > 0 OR summary === '' OR name === ''` — same predicate as `SummarizeScope::StaleOrUnfilled`. Disabled when count == 0. Click invokes `cluster_summarize` with `scope = stale-or-unfilled`, `subtree_root = null`, `recursive = true`, `summarize_mode = llm`, `overwrite_user_edited = false`; toast on outcome. Refresh shape: the tree re-paints on every refresh (queue events, `cluster_nodes` row changes), so the label re-computes naturally without a dedicated subscription. Sidebar tree-row `⋯` affordance deferred — the sidebar's tree-row menu (`app/src/sidebar/clusters/mod.rs`) is a separate insertion that wasn't a small-enough lift to bundle here
- **Export to canvas.** Tree-row right-click verb (and a cluster editor pane toolbar button) that snapshots the tree into a new `.canvas` document — each cluster a group, each leaf a file-node, hierarchy as spatial nesting. Snapshot, not synced; centroids/policies are dropped. Defined in `canvas-export.md`. [canvas-export-tree-verb]
- **Drag-and-drop reparent.** See §"Drag-and-drop reparent" below; one of the operations, broken out separately for the visual-feedback and multi-select-drag detail.


### Drag-and-drop reparent

Drag a cluster row, a leaf row, or the outlier bucket onto another cluster row to set the target as its new parent. Single drops route through `Trees::move_node`; multi-select drags loop the same single-move IPC once per item so each move gets its own `move` history row (preserving per-item step-through undo). Works identically in the sidebar's `Cluster trees` mode and in the expanded center pane's row view (the graph view explicitly opts out per [[spec:cluster-editor-graph-view-no-reshape]]). [cluster-editor-dnd-reparent]
status:: done
note:: `app/src/sidebar/clusters/tree.rs` — drag-and-drop wired onto every rendered row (both sidebar and expanded pane). Multi-drag: when the dragstart row is part of the current selection, the payload is the whole selection; otherwise just that row. Validation refuses cycles (target in dragged's descendant set), self-drop, leaf-onto-leaf, cluster-onto-current-parent. Cross-tree drag refused at the drop site by comparing tree ids. The drop loops the move per item so each move appends its own `move` history row through `core::trees::move_node`. Promote-to-top zone drops with `newParent = null`, which `core::trees::move_node` natively accepts

Drop targets:

- **Onto a cluster row** — reparent under that cluster.
- **Onto the outlier bucket** — demote a leaf to outliers (same as the row-menu "Send to outliers" entry).
- **Onto the empty area above the root list** (a "promote to top level" drop zone, visible only while a drag is in flight) — set `parent_id = NULL` to make the dragged row a top-level child.

Multi-select drag: when `selection.size > 0` and the drag starts on a selected row, the whole selection is dragged. The drag chip near the cursor shows `N items` (per [[spec:cluster-editor-dnd-visual-feedback]]). Non-selected rows initiate a single-item drag without touching the selection.

Invalid drops are rejected at the start of the drag (drop target rendered with the "no" cursor, no highlight applied) and ignored if a release happens over them:

- **Cycle** — dragging a cluster onto one of its descendants (including itself).
- **Leaf onto leaf** — leaves can't have children.
- **Cluster onto its current parent** — no-op.
- **Drop on a different tree** — DnD targets stay within the originating tree; cross-tree moves don't exist.

Cancellation: Escape during a drag aborts. Releasing over a non-target area (empty space that isn't the promote-to-top zone, panel chrome, the toolbar) also aborts.

History: each successful drag is one `SetFrontmatter` op per item moved (semantic name `move` on the op metadata), pushed onto the session undo stack with the prior parent recorded for the reverse edit. A multi-item drag is N separate ops / undo entries, not a single batch — keeps undo granular (the user can step-undo through them). The selection state is preserved across the drag for round-trip undo readability.

### Drop visual feedback

Three states render during a drag, driven by egui's pointer drag state on the row primitive: [cluster-editor-dnd-visual-feedback]
status:: done
note:: `app/src/sidebar/clusters/tree.rs` — render states: a 2px inset accent box-shadow + faint accent tint on valid drop hover, a not-allowed cursor on invalid drop hover, and a floating chip that follows the pointer during drag (rounded white background, 3px corner radius, 1px border per [[spec:cluster-editor-graph-view-label-style]]; shows "N items" + first glyph for multi-drag, name + glyph for single). A "Drop here to make top-level" promote band is rendered into each tree's body during drag and appears/disappears with the drag state

- **Drop-target highlight** — the row under the pointer (when it's a valid target) renders with a 2px inset accent ring on the row's border and a faint accent background tint. Same visual weight as the focus ring on settings inputs; deliberately subtle so the row's existing chrome (chevron, name, policy chip) stays legible during the drag.
- **Invalid-target rejection** — the row under the pointer (when it's an invalid target) renders with the `not-allowed` cursor and *no* highlight. The drag is allowed to continue moving (the user can keep searching for a valid target); the row just visibly refuses.
- **Drag chip** — a small floating chip follows the cursor (12,12 offset). For a single-item drag, the chip shows the dragged row's icon + name truncated. For a multi-item drag, the chip shows `N items` plus the icon of the first item. The chip uses the same rounded-white-background styling as the graph view's node labels ([[spec:cluster-editor-graph-view-label-style]]) so it sits cleanly over any pane content.
- **Promote-to-top drop zone** — an inset accent band renders above the root list while a drag is in flight; releasing onto it sets `parent = NULL` via `Trees::move_node`. The band hides when no drag is active.

The visual feedback layer is rendered by the shared row primitive ([[spec:cluster-editor-row-primitive]]); both the sidebar and the expanded pane pick it up without per-surface chrome.

### Multi-select range (shift-click)

The row primitive's selection handler splits the three modifier gestures (plain / Cmd-Ctrl / Shift) per file-manager convention; selection survives expand/collapse, and when 1+ nodes are selected the tree header surfaces the bulk-action toolbar (Merge siblings / Drop / Stage move to… / Stage tag with… / Clear). [cluster-editor-multi-select]
status:: partial
note:: `app/src/sidebar/clusters/mod.rs` (+ `tree.rs`) — Shift / Cmd / Ctrl-click distinctions (split out from the old single "any modifier toggles" path by [[spec:cluster-editor-multi-select-shift-range]]). Cmd/Ctrl-click toggles a single row in the tree's selection set and re-anchors; Shift-click extends from anchor through clicked id in current display order; plain click clears any prior multi-selection and re-anchors. Selection survives expand/collapse. When 1+ nodes are selected, the tree header surfaces Merge siblings / Drop / Stage move to… / Stage tag with… / Clear. Stage move/tag (`-stage-move`, `-stage-tag`) land via Sprint C's `core::suggest::stage_*` path. **Partial:** "selecting a cluster includes its subtree implicitly for bulk-action purposes" isn't yet wired — the toolbar reads exactly the selected ids, no descendant expansion. Bulk policy-apply across the selection set is still a follow-up

- **Plain click.** Clears any existing multi-selection and re-anchors on the clicked row. The row's primary affordance (open leaf, inline-edit cluster name, expand cluster) still fires — clicking a row isn't a "select this row" gesture on its own; it's a "use this row" gesture.
- **Cmd-click / Ctrl-click.** Toggles the clicked row in the selection set and re-anchors on it (so subsequent shift-clicks pivot off the just-toggled row).
- **Shift-click.** Replaces the selection with the range from the current anchor through the clicked row in current display order (top-to-bottom walk of currently-rendered rows respecting expand/collapse), inclusive. The range can cross cluster boundaries — a range from one cluster's leaf to a sibling cluster's leaf includes every visible row between them (intervening cluster headers, the outlier bucket if it sits in between, etc.). Range membership is computed on the rendered tree at click time; expanding a cluster after a shift-click range was set doesn't grow the existing selection.

The anchor lives on `TreeUIState.anchor` (sidebar) / the equivalent per-tree state in the pane, and is cleared when the tree switches. With no anchor (first interaction with a tree), a shift-click is treated as a single-row range and sets the anchor. [cluster-editor-multi-select-shift-range]
status:: done
note:: `app/src/sidebar/clusters/tree.rs` — split the legacy "Shift/Cmd/Ctrl all toggle" behavior into three distinct gestures (plain / Cmd-Ctrl / Shift). The anchor is set on every non-shift click; shift-click computes the slice from the flat top-to-bottom display order the renderer rebuilds each paint. Collapsed clusters' descendants are not in the order, so they don't appear in ranges — matches the spec. Ranges cross cluster boundaries because the display-order walk is sibling-and-descendant-flat (the outlier bucket virtual row is included when it has a real node id)


### Recluster subtree: policy and placement semantics

Two consequences of reclustering a subtree are load-bearing enough to call out:

- **Per-node policies on the replaced descendants are lost.** Reclustering deletes every descendant cluster row, including any `Tag` / `Move` / `Freeze` policy set on those rows. Descendants that had no explicit policy were already inheriting from an ancestor — they keep inheriting (the policy lives on the surviving ancestor, not on the dropped descendant). To soften the loss, the clustering review tab's recluster form offers a "Carry policies down from selected node" checkbox: when checked, the selected node's resolved policy (its own or the nearest ancestor's) is *copied* onto every newly-built direct child as an explicit policy. Default-off; the spec's intent is that reclustering is destructive and the user opts in to carrying rules forward. The session undo stack snapshots the full prior subtree (the reverse edit for the `recluster-subtree` op) including its policies, so an undo restores them exactly. [cluster-editor-recluster-subtree-policy-loss]
status:: done
implements:: [[code:hiker/trees/history/impl#[Db]record_recluster_subtree]]
note:: `cluster_op_recluster_subtree` walks up from the selected node to the root to compute the *resolved* policy (first explicit ancestor's policy wins, including the selected node's own). When the prompt's "Carry policies down from selected node" checkbox is checked (default-off per spec), that resolved policy is copied as an explicit policy onto every new direct child of the selected node. The `recluster-subtree` history row snapshots `prior_subtree` with each descendant's full row shape including its `policy` JSON — `undo_recluster_subtree` re-inserts them via `restore_node_row`, so undo restores the full prior subtree including policies exactly. Carried policy also flows onto the args JSON so redo replays the same carried-policy state
- **Already-placed notes are not moved.** The filesystem is the source of truth for placement (per `clustering.md`'s framing of the tree as a recommendation surface, not durable infrastructure). Reclustering a saved-as-triage (Evergreen) tree's subtree changes how *future* notes routed through that subtree are classified by [[spec:cluster-place-beam-descent]]; it does not move existing notes whose folder placement was driven by a prior triage match or by an Apply pass. If the user wants the new structure reflected on disk for the already-classified notes, they re-run Apply (one-shot) on the rebuilt subtree, which emits fresh `move_note` staging rows for each leaf under a `Move` policy. This is the same model as every other reshape op in this surface — no reshape moves files on its own. [cluster-editor-recluster-subtree-placement-decoupled]
status:: done
note:: the op is structural only — it never stages a pending edit and never invokes `move_note` against the vault, so no filesystem changes happen as a side effect of reclustering. The selected node's `cluster_nodes` subtree changes shape; the surviving leaves' `note_ref` columns are untouched, so the notes' on-disk paths stay where they were. Future triage classifications fired through [[spec:cluster-place-beam-descent]] use the new centroids; the existing Apply path (per [[spec:cluster-editor-apply-action]]) is the explicit way to project the new structure onto disk via `move_note` staging rows. No code change beyond what [[spec:cluster-editor-recluster-subtree]] already enforces — this slug is the *absence* of the obvious mistake, captured by ensuring the op only mutates `cluster_nodes`


## Per-node automation policy

A policy can attach to a leaf, a mid-tree cluster, the root, or the outlier bucket — a saved tree is a *policy program*, not a single classifier with a knob. [cluster-editor-policy-any-level]
status:: done
implements:: [[code:hiker/suggest/resolve_effective_policy]]
note:: `core::trees::set_policy` already accepts policies on any node kind (cluster / leaf / outlier-bucket) — the table column carries an opaque `policy` JSON regardless of `kind`. `core::suggest::resolve_effective_policy` walks up via the parent chain, treating a leaf's own explicit policy as authoritative if set

A note's effective policy at triage time is determined by walking from the note's leaf up the tree, taking the first ancestor with an explicit policy. No policy anywhere = no automation, the note is left alone (matches the existing "low confidence" tier behavior). [cluster-editor-policy-resolution-walk-up]
status:: done
implements:: [[code:hiker/suggest/resolve_effective_policy]]
note:: `core/src/suggest.rs::resolve_effective_policy(by_id, node_id)` walks the parent chain returning the first node with `Some(NodePolicy)`; `None` means "no policy anywhere up to root" and the leaf is left alone (counted into `ApplyOutcome.unpolicied`)

The outlier bucket gets its own policy slot for the canonical "send unsorted notes to `inbox/unsorted/`" or "tag them `unsorted`" flow. Setting a `Move` or `Tag` policy on the outlier bucket applies that policy to every note the placement classifier labels as outlier — no walk-up needed because outliers don't have a parent in the cluster sense. `Freeze` on the outlier bucket is a valid choice for "leave outliers in inbox; I'll triage them by hand." [cluster-editor-outlier-policy]
status:: done
implements:: [[code:hiker/suggest/resolve_effective_policy]]
note:: `core::trees::set_policy` already accepts policies on any node kind including `outlier-bucket` (Sprint A's tree shape exposes the bucket as a first-class node), and the policy menu in `app/src/sidebar/clusters/mod.rs` is invoked off the policy chip on every cluster row — outlier buckets are clusters in the policy sense. `core::suggest::resolve_effective_policy` reads the bucket's policy directly for any leaf that sits under it (no walk-up needed; outlier buckets are roots in the policy sense)

Three policy shapes — `Tag`, `Move`, and `Freeze` — covering the four meaningful combinations of (action × review requirement):

- **Tag(slug, require_review)** — produce an `apply_tag` row that writes the tag to the note's frontmatter (per [[spec:suggestions-mode-tag]]). When `require_review = false`, the row auto-accepts (subject to the global flag); when `true`, the row stays pending. Idempotent on accept. The slug-ified cluster name is written to the note's frontmatter: a cluster named `"Embedding research"` writes `embedding-research`. The default field is `hiker.suggested_tags` — a list of strings kept separate from the user's own `tags:` field — and is configurable via `[suggestions] tag_field` (in `vault/.hiker/config.toml`). A dedicated default field is greppable (`rg "suggested_tags:"`), mass-wipeable without touching the user's own tags, and namespaced away from the auto-tag enrichment system's constrained vocabulary at `.hiker/vocabulary.yaml` — two distinct systems. Setting `tag_field = "tags"` writes cluster tags into the user's main tag list instead; slug-ification applies regardless. [suggestions-tag-field-configurable]
status:: planned
note:: `[suggestions] tag_field` config; default `hiker.suggested_tags`, can be set to `tags` to use the regular list

  The `apply_tag` row's content-merge mechanic. [suggestions-mode-tag]
status:: done
implements:: [[code:hiker/suggest/ACTION_APPLY_TAG]]
note:: `core/src/suggest.rs::merge_tag_into_frontmatter` pre-computes the file content with the tag appended (idempotent — skipped if already present) under the configured `tag_field` (default `hiker.suggested_tags` per spec). The proposal carries `action = "apply_tag"`, the new content, `source_hash` for drift safety, and `metadata.tag_slug` / `tag_field`. Accept path is the existing content-write path (no new code)
- **Move(folder, require_review)** — produce a `move_note` row that moves the file into the target folder (per [[spec:suggestions-mode-move]]). Same auto-accept / pending semantics. Subject to the scope-safety rule: triage never produces a `move_note` whose `source_path` is outside the configured triage scope (default `inbox/`). A tree saved over `research/` whose triage scope is `inbox/` only auto-moves notes currently under `inbox/`; notes the user placed elsewhere are off-limits to triage — moving them is an explicit user action (drag-drop or `hiker mv`). The worst case for an over-eager classifier is "wrong subfolder under inbox," never "your note moved out from under you." [suggestions-mode-move]
status:: done
touches:: [[code:hiker/suggest]]
note:: `core/src/suggest.rs` stages a pending `Rename` op per move (`target_path = <folder>/<basename>`), authored `auto:cluster`, as part of a reorg batch. Skips no-op moves (target == source). Accept applies the `Rename` through the op log (`op_writes::flip_batch_status`), repointing the document's path; the filesystem move stays the caller's concern downstream of `core::vault::move_note`
- **Freeze** — match is explicitly ignored. No staging row is produced. Useful for marking a subtree as "this is well-organized already, leave it alone."

[cluster-editor-policy-types]
status:: done
touches:: [[code:hiker/trees/types]]
note:: `core/src/trees/types.rs::NodePolicy` — `Tag { slug, require_review }` / `Move { folder, require_review }` / `Freeze`, serialized as JSON tagged on `kind`. Apply pipeline (`core::suggest::apply_tree`) drives behavior off this enum

`require_review` composes with the global `[triage].review_required` flag (default `true`) — a row auto-accepts only when *both* `policy.require_review == false` AND `config.review_required == false`. Either set to `true` forces the row to stay pending in the activity-detail Pending filter. The global flag is "force review on every triage match"; the per-node flag is "force review on this specific subtree" — useful for "I trust the saved tree's `research/` placements, but `inbox/projects/*` matches should always pause for me."

The note's author class is a third input to this composition: an `Agent` classification reliably forces pending. [triage-author-class]
status:: partial
implements:: [[code:hiker/suggest/NoteAuthorClass]]
touches:: [[code:hiker/suggest]]
note:: evidence: Engine supports User vs Agent: `core/src/suggest.rs::NoteAuthorClass { User, Agent }` + `TriageInput.author_class` flow into `effective_requires_review = policy.require_review || author_class == Agent`, so an Agent classification reliably forces pending. **Gap:** no caller in Sprint D actually emits `Agent` against a real agent write. The on-save watcher spawn (the host ~line 1242) hardcodes `User`; `cluster_triage_run` accepts a string override but the only frontend caller passes nothing; `cluster_triage_enqueue` doesn't even carry the hint on its `RaptorTriageMatch` payload (consumer in `DirectWorkerHandlers::try_handle` also passes `User`). The agent-author detection mechanism (watcher event source vs staging-history recent-agent-write lookup vs an explicit producer-side hint on the task variant) lands with the auto-accept worker / cancellable queue. The `AUTHOR_AUTO_TRIAGE` constant ("auto:triage") is reserved on the auto-accept path; the user-accept path keeps `author = "user"` per `op-log.md`

Confidence still flows through the matching engine — the descent classifier returns a target node and a confidence — but the **action** taken is driven by the matched node's resolved policy, not by a global threshold. A node's policy can carry an optional `min_confidence` parameter for users who want "auto-move this cluster only when match confidence ≥ 0.85"; that's a per-policy threshold, not a global one. [cluster-editor-policy-require-review]
status:: done
touches:: [[code:hiker/panels/settings]], [[code:hiker/suggest]]
note:: `core/src/suggest.rs::triage_match` composes the per-policy `require_review` with the global `[suggestions.triage].review_required` (Sprint D `SuggestionsConfig.triage` in `core::config`) **and** the note's `NoteAuthorClass` — any one true forces the row pending; all-false auto-accepts. The Apply path (`apply_tree`) stamps the per-policy flag on `metadata.require_review` for accept-time consumption. Settings UI surfaces the global flag under the "Triage" card in `app/src/panels/settings/mod.rs`


## Triage execution

When a tree is in `saved as triage` state and the user has any policy set on any node, triage runs against new/modified notes:

- **On save** (default) — when a note inside the configured triage scope (default `inbox/`) *and* within the saved tree's `BuildScope` (per [[spec:cluster-build-scope]]) is saved, hiker enqueues a `RaptorTriageMatch` task in `core::tasks` (per `task-queue.md`). The worker runs the placement classifier ([[spec:cluster-place-beam-descent]]) and resolves the matched node's policy. [cluster-editor-triage-on-save]
status:: done
implements:: [[code:hiker/suggest/triage_all_saved_trees]]
note:: The watcher-subscriber spawn in the host (after `spawn_staging_recheck`) consumes every `FileEvent::Modified` / `Created`, applies the `[suggestions.triage].scope` prefix pre-filter, resolves the note's id + embedding from `read_store`, and runs `core::suggest::triage_all_saved_trees` synchronously (microseconds; no LLM). The async/queued variant is `cluster_triage_enqueue` per [[spec:cluster-editor-triage-via-task-queue]]
- **Scheduled rerun** (opt-in) — `[triage] scheduled_rerun = "0 3 * * *"` (cron-shape per the existing settings model) re-runs triage over a configurable scope. The schedule fires submissions to `core::tasks` for each affected note, batched at `Low` priority so it doesn't block foreground work. [cluster-editor-triage-scheduled-rerun]
status:: partial
note:: the host — `parse_rerun_interval` parses a duration-string grammar (`30m`, `1h`, `6h`, `24h`, `7d`) and a tokio task ticks at the parsed interval, enqueueing one `RaptorTriageMatch` at `Priority::Low` per (saved-as-triage tree × note in scope), driven by the existing `[suggestions.triage].scope` prefix filter. Settings UI row already exists from Sprint D. **Partial:** the spec's grammar is cron-shape (e.g. the spec example `"0 3 * * *"`); the runtime accepts only the duration grammar above and silently logs + disables anything else — a user copy-pasting the documented cron example gets no scheduled rerun. Cron-shape support is tracked as [[spec:cluster-editor-triage-scheduled-rerun-cron-syntax]]

  Accepting the documented cron-shape grammar so the spec's example actually schedules is the remaining sub-task. [cluster-editor-triage-scheduled-rerun-cron-syntax]
status:: planned
note:: accept the cron-shape grammar from the spec (`docs/cluster-editor.md` — `scheduled_rerun = "0 3 * * *"`) in `parse_rerun_interval` so the spec's documented example actually schedules. Likely pulls in a cron crate (or hand-rolled 5-field parser) and adapts the tokio ticker to fire on next-occurrence rather than fixed interval
- **Modified-note rerun** (opt-in) — separate from scheduled: when an existing note (already placed or already in inbox) is meaningfully edited (defined as "embedding changed by more than X cosine distance from prior"), triage re-evaluates. Distinct from on-save because most saves don't change embeddings significantly; the cosine guard avoids re-triaging on every keystroke save. [cluster-editor-triage-modified-rerun]
status:: partial
implements:: [[code:hiker/config/patch/ELIGIBLE_VAULT]], [[code:hiker/config/patch/ELIGIBLE_USER]]
touches:: [[code:hiker/panels/settings]]
note:: `[suggestions.triage].modified_rerun` (bool, default `false`) + `.modified_rerun_cosine_guard` (float, default `0.15`) added in `core/src/config.rs`; eligible-key registered for both Vault and User scope; surfaced in the settings UI alongside the other triage controls (`app/src/panels/settings/mod.rs`). **Partial:** the runtime guard isn't yet wired — honoring the cosine threshold requires tracking each note's last-triaged embedding (a new `notes.last_triaged_embedding` column or equivalent), which is a follow-up. Today the on-save trigger fires unconditionally within `scope` per [[spec:cluster-editor-triage-on-save]]; the modified-rerun config is forward-compat plumbing
- **Backfill sweep** (on-demand) — a "Triage existing notes…" action runs the saved tree's classifier across a chosen scope (whole vault / a folder / `inbox/`) in one pass, so a freshly-built or hand-authored classifier can tag or move notes that *already exist* rather than only new ones. The sweep emits its matches as a **previewed, undoable review batch**: every produced row defaults to `require_review` regardless of per-policy settings, surfaced in the tree-scoped batch-review pane ([[spec:cluster-editor-batch-review-pane]]) for Accept-all / per-row review before anything touches the vault. A note already placed where the classifier would put it is a no-op. Batched at `Low` priority. [cluster-editor-triage-backfill]
status:: planned
note:: "Triage existing notes…" action runs a saved tree's classifier over a chosen scope (vault / folder / inbox) in one pass, so it tags/moves notes that already exist, not just new ones. Loops `triage_all_saved_trees` over the scope; emits a previewed, undoable review batch (every row forced `require_review`) into the tree-scoped batch-review pane ([[spec:cluster-editor-batch-review-pane]]). No-op when a note is already placed where the classifier would put it. `Low` priority

All four pathways submit through `core::tasks`, which routes to whatever worker is configured. The user watches progress in the queue widget; a per-policy task naturally inherits the queue's cancel + audit machinery. [cluster-editor-triage-via-task-queue]
status:: done
note:: `cluster_triage_enqueue` host command submits one `TaskKind::RaptorTriageMatch { tree_id, source_path }` task per saved-as-triage tree at `Priority::Normal`. The synchronous classifier ([[spec:cluster-editor-triage-on-save]]) is the on-save fast path; producers wanting queue visibility (CLI parity, scheduled rerun, manual "Re-triage this note") use the async path

Triage outputs flow through the op log rather than mutating the vault directly. Each match produces one row with `surface = "triage"`, `metadata.tree_id`, `metadata.matched_node_id`, `metadata.confidence`, and an `action` derived from the resolved policy: `auto-move` → `move_note`, `auto-tag` → `apply_tag`, `review` → the same row marked for explicit user review. Auto-accept-vs-pending follows the `require_review` × global-flag rule above. The accept path reuses [[spec:suggestions-apply-cmd]] (the same `move_note(from, to)` and frontmatter-tag-write code that drives one-shot Apply): `apply_tag` rows write to the configured `[suggestions] tag_field` on the target note's frontmatter. [cluster-editor-triage-via-staging]
status:: done
implements:: [[code:hiker/suggest/triage_match]]
note:: `core/src/suggest.rs::triage_match` stages one op-log op per match with `surface = "triage"`, author `auto:triage` — a `Move` policy stages a one-move reorg batch (pending `Rename`), a `Tag` policy stages a pending content op. When `effective_requires_review == false` the op is applied immediately (`op_writes::flip_batch_status(accept)`), matching the spec's `status = accepted` direct-apply path; otherwise it stays pending for review. `Freeze` policies and out-of-scope sources skip without emitting an op

The op shape a triage match produces is the staging proposal. [triage-staging-proposals]
status:: done
implements:: [[code:hiker/config/sections/SuggestionsConfig]], [[code:hiker/suggest/SURFACE_TRIAGE]]
touches:: [[code:hiker/suggest]]
note:: `core::suggest::triage_match` stages a pending `Rename` (Move policy) or pending content op (Tag policy) authored `auto:triage` via `op_writes`, applied immediately when no review is required. `Freeze` policies and no-op (target == source) moves skip without emitting an op. Out-of-scope sources (per `[triage].scope`) also skip pre-emission

The accept path is the shared apply mechanic — the same `core::suggest::apply_tree` that drives one-shot Apply. [suggestions-apply-cmd]
status:: done
implements:: [[code:hiker/suggest/apply_tree]]
verifies:: [[code:hiker/suggest/tests/apply_tree_stages_tag_and_move_pending_ops]]
note:: `core/src/suggest.rs::apply_tree(trees, tree_id, vault, store, log, history)` is the shared apply mechanic: `Move` leaves stage one cross-document reorg batch of pending `Rename` ops ([[spec:op-log-reorg-batch]], author `auto:cluster`), `Tag` leaves stage one pending content op each via `op_writes::stage_auto_content`. Nothing reaches disk until the user accepts in the batch-review pane. The UI path satisfies the slug per `docs/cluster-editor.md`'s Apply flow; the CLI binary lands as the sub-slug [[spec:suggestions-apply-cmd-cli]]

The `hiker suggest apply` CLI surface is the sub-slug of the shared apply mechanic. [suggestions-apply-cmd-cli]
status:: planned
note:: sub-slug of [[spec:suggestions-apply-cmd]] — `hiker suggest apply <tree-id>` CLI surface with `--interactive` / `--accept-all` / `--dry-run`. `cli/src/main.rs` is currently a 3-line stub; this slug covers wiring it onto `core::suggest::apply_tree`

An auto-accepted triage match surfaces a toast with a 10s Undo. [triage-auto-undo-toast]
status:: planned
note:: toast + 10s Undo fires from the auto-accept path; Undo logs to rejection history

### `[triage]` config

Triage-level behavior. All keys are eligible for user and vault scope; vault wins per the standard merge rule.

```toml
[triage]
review_required = true       # bool; global "force review on every match" (see above); when false, auto-* matches auto-accept on insert, review-policy matches still wait
scope = "inbox/"             # string; source-folder safety boundary — triage never moves a note whose source_path is outside it; also the on-save trigger folder
scheduled_rerun = ""         # string; cron-shape, empty disables (cluster-editor-triage-scheduled-rerun)
```

All keys are user + vault scope (vault wins), live-applied, strict-load schema coverage per [[spec:settings-strict-load]]; the settings UI grows matching rows under a "Triage" subsection. [triage-review-required]
status:: done
implements:: [[code:hiker/config/patch/ELIGIBLE_VAULT]], [[code:hiker/config/patch/ELIGIBLE_USER]], [[code:hiker/config/sections/SuggestionsConfig]], [[code:hiker/config/sections/TriageConfig]]
verifies:: [[code:hiker/suggest/tests/triage_auto_accepts_move_when_no_review_required]]
touches:: [[code:hiker/suggest]]
note:: | opts.review_required |  | author_class == Agent` and stamps it on `metadata.require_review`. The accept-side reads the flag to decide whether to auto-accept on insert vs hold pending. `[triage].review_required` config key (default `true`) is registered in `ELIGIBLE_USER` + `ELIGIBLE_VAULT` (`core/src/config.rs`) and surfaced in the Settings UI's "Triage" card. The `author = "auto:triage"` stamp on the auto-accepted history frame is `AUTHOR_AUTO_TRIAGE` (per [[spec:triage-author-class]]) — consumed by the accept-time auto-accept path when the worker lands · evidence: `core/src/suggest.rs::triage_match` computes `effective_requires_review = policy.require_review


## Batch-review pane (one-shot Apply)

Clicking Apply on a draft tree runs the policy walk, emits one pending op per leaf whose resolved policy is `Tag` or `Move`, and opens the batch-review pane in place of the expanded tree view. The pane is the tree-scoped view of pending ops where `metadata.tree_id = <this tree>`. [cluster-editor-apply-action]
status:: done
verifies:: [[code:hiker/suggest/tests]]
note:: `core/src/suggest.rs::apply_tree` walks the tree, runs `resolve_effective_policy` per leaf, and stages one pending edit per `Tag` / `Move` leaf — `surface = "cluster-editor"`, `metadata.tree_id`, `metadata.matched_node_id`, `metadata.policy_kind`, `metadata.require_review`, `metadata.tree_member_fingerprint`. Move rows carry `source_path`; Tag rows carry the pre-merged content + `source_hash` for drift safety. Skipped leaves (frozen / unpolicied / missing-note) are counted onto `ApplyOutcome`. `cluster_apply` command + UI [[spec:cluster-editor-pane-mode]] Apply button + auto-flip of tree `state` → `applied` once every emitted row resolves

Layout (sketch):

```
┌─ Apply review: 2026-05-08 reorg  ────────────────────────────────────┐
│ [Accept all (24)] [Reject all] [← Back to tree]            24 pending│
│                                                                       │
│ ▼ Move (16)                                                           │
│   ▢ inbox/whisper-notes.md          → research/embeddings/  [✓] [✗]  │
│   ▢ inbox/voyage-vs-bge.md          → research/embeddings/  [✓] [✗]  │
│   ▢ inbox/whirlpool-error-codes.md  → projects/dishwasher/  [✓] [✗]  │
│   ▢ ⚠ inbox/coffee-roasting.md      → research/             [conflict]│
│   ...                                                                 │
│                                                                       │
│ ▼ Tag (8)                                                             │
│   ▢ inbox/llm-jailbreaks.md         + llm-jailbreaks         [✓] [✗] │
│   ...                                                                 │
└───────────────────────────────────────────────────────────────────────┘
```

Behavior: [cluster-editor-batch-review-pane]
status:: partial
note:: `app/src/panels/cluster_review/mod.rs` filters op-log pending edits by `surface = "cluster-editor"` + `metadata.tree_id`, groups by action (Move / Tag), and paints per-row Accept / Reject (`op_writes::flip_op_status`, plus `cluster_record_rejection`) buttons plus Accept-all / Reject-all batch verbs (with confirm gate for N > 5 / always for reject-all). Conflicted rows show a `⚠` glyph + reason and disable Accept; the pane auto-flips the tree's `state` to `applied` once every row resolves. **Partial:** inline target-path edit (spec line 329) is deferred — Sprint C surfaces a Reject + re-Apply flow for adjustments instead

- **Rows are grouped by `action`.** Move rows together, Tag rows together. Each group collapsible. Row order within a group: by `target_path` (Moves) or `slug` (Tags) so related items sit adjacent.
- **Per-row Accept / Reject.** Same `core::ops::flip_op_status(op_id, Accepted | Rejected)` calls the activity-detail page uses; clicking accepts/rejects exactly that op. A reject is recorded in the rejection history so a re-Apply skips the row. [suggestions-rejection-history]
status:: partial
implements:: [[code:hiker/suggest/compute_fingerprint]]
touches:: [[code:hiker/suggest]]
note:: `core/src/suggest.rs::RejectionHistory` owns `.hiker/suggestion-history.json` — per-`(fingerprint, note_path, action)` rows with a 90-day TTL (GC on every write). Cluster-editor batch-review pane calls `cluster_record_rejection` alongside the op-log reject (`op_writes::flip_op_status`) so the log is populated on every user reject; `apply_tree` consults the log and skips already-rejected rows. **Partial:** the fingerprint is Sprint C's coarse form (`hash(parent_cluster_name + note_path + action)`); the spec's member-set Jaccard recovery is a follow-up. TTL configurability is also deferred
- **Inline edit of the target.** Click the `→ <path>` (or `+ <slug>`) to edit it in place — saves a new `target_path` / tag value on the staging row (re-using the in-flight `staging-amend`-shaped mechanic; if amend isn't wired yet, falls back to "reject this row + propose a new one with the edited target"). Useful for "the cluster name is right but the folder needs to be `research/embeddings-v2/` not `research/embeddings/`."
- **Conflicted rows highlighted.** When a row's state is `conflicted` (per `staging-proposal-state`), the row gets a `⚠` glyph and the reason tooltip; Accept is disabled, Reject still works. Same display rules as the activity-detail page.
- **Accept-all / Reject-all** are batch verbs over the visible (non-conflicted) rows. Confirm dialog when N > 5 for Accept-all; always confirm for Reject-all (rejecting agent/automation work is destructive).
- **Back to tree** returns to the expanded tree view without closing the pending rows — useful if the user wants to adjust a policy and re-run Apply, or just look at the tree structure while reviewing.
- **Auto-close on completion.** When every emitted row is resolved (accepted or rejected), the pane closes, the tree's `state` flips to `applied`, and the expanded tree view reappears with a confirmation toast (`24 changes applied, 0 pending`).
- **Skipped policies surface as a note.** Frozen subtrees and unpolicied leaves don't produce rows; the pane header notes `(7 leaves skipped — no policy assigned)` so the user knows nothing got silently dropped.

The pane is a new editor-pane sub-mode (`cluster-batch-review`) carrying `tree_id` on the buffer state. Same nav-stack discipline as the expanded tree view: entering pushes once, Back-to-tree pops, individual row accept/reject don't push. [cluster-editor-batch-review-pane-mode]
status:: done
note:: A `cluster-batch-review` tab kind keyed off `metadata.tree_id`; the pane re-uses it (shared with the `cluster-tree` sub-mode) so Back-to-tree flips sub-state without closing the tab. The workbench host reveals the cluster-editor pane and hides the editor when this kind is active


## LLM actions via the task queue

Three LLM-driven actions in this surface route through `core::tasks` rather than direct `core::llm` calls:

- **Initial cluster naming/summarization** during the build pass (already specced as [[spec:cluster-summarize-llm]] in `clustering.md`; that fan-out routes through the task queue per the queue's "everything except chat" scope).
- **Regeneration** triggered by the user's "Regenerate names" toolbar button or per-node "Regenerate this node" — submits one task per node needing regeneration, at `Normal` priority (user is watching).
- **Auto-name on merge / split** — merging siblings or splitting a cluster produces new clusters that need names; submitted as `Normal` tasks immediately so the new rows aren't stuck with placeholder names.

[cluster-editor-llm-actions-via-task-queue]
status:: partial
note:: The producer side now routes through `core::tasks`: regenerate (`cluster_regenerate_names`), staleness-badge regenerate, triage classifier dispatch (`cluster_triage_enqueue`). **Partial:** the auto-name-on-merge / auto-name-on-split fan-out is still inline-deterministic naming; promoting those to task submissions is a follow-up tied to the LLM-backed `Summarizer` impl

Already-named (non-placeholder) clusters are skipped by all three — regeneration targets only placeholder-named clusters. Explicit "Regenerate this node" on an already-named cluster confirms with a dialog before clobbering. [cluster-editor-user-edit-provenance]
status:: partial
touches:: [[code:hiker/trees/ops/edit]]
note:: `core/src/trees/ops/edit.rs::rename` + `set_summary` stamp `user_edited_name = 1` / `user_edited_summary = 1` (Sprint A). `app/src/sidebar/clusters/mod.rs` reads those flags and applies italics so user-edited rows are visually distinguishable. **Partial:** the short-circuit on the regeneration path lands with [[spec:cluster-editor-regenerate-via-task-queue]] (deferred with the queue wiring); the "Regenerate this node" confirm dialog ditto


## Graph view for policy assignment

Third view variant, alongside the sidebar mode and the row-shaped expanded mode. Renders the tree as a node-link graph using the project's committed graph renderer (the egui force-graph widget, per `design.md`'s graph-view bullet) — tuned for "see the whole tree at a glance, assign policies by clicking nodes, watch the colors light up." Legend: `●` Move / `●` Tag / `●` Freeze / `○` no policy / `⏸` require review. [cluster-editor-graph-view]
status:: done
touches:: [[code:hiker/panels/cluster_graph]]
note:: `app/src/panels/cluster_graph.rs` (`ClusterGraph`, `show()`). Sub-sub-mode of `cluster-tree` toggled by `Tree`/`Graph` buttons in the pane toolbar. Renders the tree via the shared egui force-graph widget with policy-color / size / staleness / outline encoding

When to use the graph view vs. the row-shaped expanded view: row view is the working surface for textual operations (read summaries, rename clusters, drag-drop into clusters, multi-select stage actions). Graph view is the working surface for *overview and policy assignment* — the spatial layout makes the tree structure legible at a glance, and color-by-policy turns "did I assign moves to everything that needs it" into a visual scan instead of a row-by-row walk.

Behavior: [cluster-editor-graph-view-behavior]
status:: done
note:: umbrella — covered by the per-behavior slugs below (click, hover, multi-select, filter, selection-outline)

- **Layout — radial tree by default.** Root at center, descendants on concentric rings, sibling spread by angular sweep. Fits a rectangular pane well, scales to 5–6 levels deep without cramping, and reads at a glance ("which subtrees are big, which are dense"). Alternative layouts (`vertical-tree`, `horizontal-tree`, `force-directed`) are selectable from the view menu. Layout choice is part of the per-tree saved view state ([[spec:cluster-editor-graph-view-saved-view-state]]); persisting one global preference would conflict with users who like one layout for the Evergreen tree and another for a Sapling. The force-directed variant runs the shared force-graph layout — a native ForceAtlas2 implementation with Barnes–Hut repulsion (egui-agnostic in `hiker-graph`'s `graph/src/force.rs`, wrapped by the egui adapter `widgets/graph-widgets`) — with outbound-attraction-distribution on so a hub's leaves settle into a ring at a consistent radius around the parent rather than drifting outward, the look users recognize as "force-directed." The layout settles at a scale that renders legibly at the default zoom. [cluster-editor-graph-view-layout]
status:: done
touches:: [[code:hiker/force]], [[code:hiker/panels/graph]], [[code:hiker/tree]]
note:: the layout engines are egui-agnostic in `hiker-graph` (the `hiker-render` submodule): `graph/src/tree.rs` (radial / vertical-tree / horizontal-tree pure-position layouts) + `graph/src/force.rs` (ForceAtlas2 + Barnes–Hut), wrapped by the egui adapter `widgets/graph-widgets/src/{graph_layouts,force_layout}.rs` (converts positions at the `egui::Vec2` boundary) and consumed by the cluster editor and the vault graph (`app/src/panels/graph.rs`). Radial is the default; layout id persisted in per-tree saved view state. The force-directed variant runs `LayoutParams { scaling_ratio: 100.0, outbound_attraction_distribution: false, degree_repulsion: true, .. }` — `outbound_attraction_distribution = false` is the ForceAtlas2 default and is what produces the "leaves sit in a ring around their hub at consistent radius" look; turning it on divides each edge's spring by the source mass, which lets hub leaves drift outward instead of settling at a fixed radius. `scaling_ratio = 100` keeps the settled graph spanning enough world units to render legibly at the default zoom. `tools/graph-snapshot/` is the headless harness for iterating on these tunings against synthetic graphs without launching the app
- **Layout-extensible framework.** Layouts are variants of `LayoutKind` in the graph panel (`app/src/panels/graph.rs`); adding a layout = new variant + position-assignment arm + view-menu row. The renderer applies the chosen layout's positions and stays layout-agnostic. [cluster-editor-graph-view-layout-extensible]
status:: done
touches:: [[code:hiker/panels/cluster_graph]]
note:: layout registry in `app/src/panels/cluster_graph.rs` keyed by layout id. Adding a layout = new function + registry entry; the renderer stays layout-agnostic (consumes only `x` / `y` on the node)
- **Node color encodes the resolved policy.** Distinct colors for `Move` / `Tag` / `Freeze` / `no policy`. A `⏸` glyph overlay marks nodes with `require_review = true`. Inheritance is visualized via softer-shade colors on descendants whose policy walked up to an explicit ancestor — at-a-glance "which subtrees actually have rules versus which are riding inherited ones." [cluster-editor-graph-view-color-by-policy]
status:: done
touches:: [[code:hiker/panels/cluster_graph]]
note:: `app/src/panels/cluster_graph.rs` (policy-color encoding) — Move/Tag/Freeze/None each carry a distinct color; inherited (walked-up) policies render at 0.55 alpha; `⏸` glyph appended to the node label when `require_review` is set
- **Node size encodes member count.** Leaf clusters with more members render as larger nodes; root and high-level clusters can be configured to scale logarithmically so the root doesn't dwarf everything. [cluster-editor-graph-view-size-by-members]
status:: done
touches:: [[code:hiker/panels/cluster_graph]]
note:: `app/src/panels/cluster_graph.rs` (member-size encoding) — `6 + log2(memberCount) * 2.5`, capped at 22; leaves render at fixed size 4
- **Node-label text styling.** Each node's text label renders on a rounded white background with a 1px solid `#c8c8c8` outline (~6px horizontal / 2px vertical padding, ~3px corner-radius). Text reads against any policy fill / theme backdrop without losing the node's own color encoding. Wired via the force-graph widget's node-label drawing so the styling is consumer-agnostic — the vault-wide graph view picks up the same label background. [cluster-editor-graph-view-label-style]
status:: done
note:: `app/src/widgets/force_graph.rs` (label rendering) paints a rounded white background (radius 3, padding 6×2) with a 1px `#c8c8c8` border behind every node label, so both cluster-tree and future vault-graph consumers pick it up
- **Summary staleness tint.** Nodes with `summary_membership_churn > 0` render with a soft tint (slight desaturation of the policy color, opacity-style rather than a new hue) so the "this summary may be out of date" signal is visible at a glance without breaking the policy-color encoding. Hovering shows the exact churn count in the tooltip; clicking the policy chip in the popover offers a Regenerate-summary affordance alongside the policy controls. [cluster-editor-graph-view-summary-staleness-tint]
status:: done
note:: `app/src/panels/cluster_review/graph.rs` desaturates the policy color (alpha 0.75) when `summary_membership_churn > 0`. Tooltip shows the count; the policy popover gets a `Regenerate summary (↻ N)` row that enqueues `cluster_summarize_node`
- **Left-click a node selects it** (single-select; replaces any prior selection). Shift+click extends/toggles the multi-select. **Right-click on a cluster (or outlier bucket)** opens the policy editor popover anchored at the pointer — same editor as the row view (`Tag` / `Move` / `Freeze` / `None` + slug/folder + require-review checkbox). Submitting rewrites the node's policy in the tree doc's frontmatter (a `SetFrontmatter` op) and recolors immediately. Left-click is reserved for select (matching the row view and the rest of the app, and the note-preview overlay which keys off selection). [cluster-editor-graph-view-click-to-edit-policy]
status:: done
touches:: [[code:hiker/panels/cluster_graph]]
note:: Moved off plain left-click — the **right-click** path opens the Tag / Move / Freeze / Clear menu anchored at the pointer location. Plain left-click is reserved for select (and pin for leaves) so the row view's click semantics match the graph view. Wired through `app/src/panels/cluster_graph.rs`'s node right-click handling. Cluster / outlier-bucket nodes only; leaves have no per-node menu yet
- **Multi-select via Shift+left-click** for bulk policy assignment — set a policy on N nodes at once via the right-click popover, applied to each. Same multi-select semantics as the row view's multi-select; the bulk-action toolbar from the row view doesn't apply here (merge/split/etc. are textual operations).
- **Hover a node** — overlay tooltip with the cluster's name, summary, member count, and the resolved policy. Held-hover (>500ms) expands the tooltip to show member-note titles (capped at ~10 with "and N more"). The tooltip anchors near the cursor (mouse-relative, with a 12,12 offset and a viewport clamp so edge-hovers don't slide it off-screen) rather than at the canvas corner, with a `min-width` floor so sparse content doesn't collapse to a 1px line. [cluster-editor-graph-view-hover-detail]
status:: done
touches:: [[code:hiker/panels/cluster_graph]]
note:: `app/src/panels/cluster_graph.rs` (hover handling) shows a tooltip with name + summary + member count + resolved policy; 500ms held-hover expands to leaf titles (capped at 10 + "and N more"). The tooltip tracks the cursor (12,12 offset, viewport-clamped) so it follows the pointer instead of pinning at the canvas corner; a `min-width` keeps sparse content from collapsing to a 1px line
- **Filter chrome** — top-of-pane filter strip lets the user dim everything *except* nodes matching a given policy (e.g. "highlight only Move nodes," "highlight only nodes with `require_review`," "highlight no-policy nodes"). Dimming rather than hiding so the structure stays legible. [cluster-editor-graph-view-policy-filter]
status:: done
note:: filter strip at the top of the canvas with All / Move / Tag / Freeze / No policy / Require review chips. Non-matching nodes render at 0.25 opacity rather than hidden, per spec
- **Click a leaf** — single left-click both selects the leaf and opens the note as a preview tab in the editor pane (`{ preview: true }`, matching sidebar / activity behavior). Clusters route to the right-click popover instead. Nav-stack discipline unchanged. [cluster-editor-graph-view-leaf-click-opens-note]
status:: done
note:: Single left-click on a leaf opens the note as a preview tab via the host-supplied `openNote` callback (`{ preview: true }`, matching sidebar / activity). Clusters route to right-click for the policy popover (per [[spec:cluster-editor-graph-view-click-to-edit-policy]]); pin-on-click semantics removed in favor of [[spec:cluster-editor-graph-view-hover-preview-card]] which tracks the card to the hovered node automatically
- **Hover preview card.** When the view menu's `Show note preview` toggle is on, the in-canvas preview card tracks the hovered node and anchors next to it (cursor-style nudge: drawn down-and-right of the node, flipped to the opposite quadrant if it would clip the canvas, then clamped). Card body is the note's content with the YAML frontmatter block skipped (so the preview opens on real text, not metadata) for leaves; for cluster nodes the card shows the cluster's `name` + `summary`. When a cluster's `summary` is empty the card falls back to whatever leaf body is still cached so the toggle never produces a useless "(no summary)" card on the common case. Card style is a light `#fafafa` background with a `#c8cdd4` 1px border; title wraps inside the card width so long basenames don't overflow. Toggle is per-tree saved view state alongside the other graph-view view-menu choices. [cluster-editor-graph-view-hover-preview-card]
status:: done
touches:: [[code:hiker/panels/cluster_graph]], [[code:hiker/panels/graph]]
note:: `app/src/panels/cluster_graph.rs::paint_nodes` records a `HoveredNode { name, summary, leaf_path, screen_pos }` whenever the cursor is over any cluster or leaf node; the panel renders the in-canvas preview card via the shared `crate::panels::graph::paint_preview_card` helper when `state.show_preview` is on, anchored to `screen_pos` with quadrant-flip + canvas-clamp placement. Body source: leaves resolve `leaf_path` via `store.path_for_id(note_ref)` (with a fallback to using `note_ref` as a path string for the cluster-review embed where note_ref carries the vault-relative path directly) and the cached `selected_preview` is refreshed via `update_selection` on hover; the file body is passed through `panels::graph::skip_frontmatter` so the YAML block is dropped before truncation. Cluster nodes use their in-memory `summary`, falling back to whatever cached leaf body is still around when the summary is empty. Card style: light `#fafafa` background, `#c8cdd4` 1px border, dark text; title laid out with `painter.layout` against `inner.width()` so long basenames wrap inside the card. Vault graph (`app/src/panels/graph.rs::show`) shares the same paint helper and the same hover-to-update flow
- **No drag-reshape from the graph view.** Drag-drop tree edits (move-note-between-clusters, etc.) stay in the row view where the targets are unambiguous; in a graph view, "drag node A onto node B" is too easy to misclick. The graph view's job is overview + policy assignment; reshape is the row view's job. [cluster-editor-graph-view-no-reshape]
status:: done
note:: No drag-reshape on the canvas (only click + hover + pan/zoom). Footer note when selection is non-empty reminds the user that tree-shape edits stay in the row view
- **Pan / zoom via keybinds.** Default chords land via the keybind registry ([[spec:keybind-registry]]) — `pointer-drag` for pan, `wheel` for zoom, `pinch` for zoom on touchpads. The defaults map to the registry-canonical chord ids `cluster-editor.graph-pan`, `cluster-editor.graph-zoom`, `cluster-editor.graph-zoom-pinch`. Users rebind via the existing keybind UI; no graph-view-specific chrome for pan/zoom. [cluster-editor-graph-view-pan-zoom-keybinds]
status:: done
note:: The egui force-graph widget (`app/src/widgets/force_graph.rs`) handles pointer-drag pan / wheel zoom / pinch zoom. Chord ids `cluster-editor.graph-pan` / `.graph-zoom` / `.graph-zoom-pinch` are reserved in the spec for keybind-registry follow-up; defaults stay implicit at this layer
- **Selection visual — outline ring.** A selected node renders with a thin outline ring in the accent color (`var(--accent)`). Multi-select shows the same ring on every selected node; the bulk-action toolbar at the bottom of the pane shows the count (`Selected: 3 nodes`). Minimal styling, matches the rest of the UI. [cluster-editor-graph-view-selection-outline]
status:: done
note:: Plain left-click single-selects (replaces prior selection); Shift+left-click extends/toggles for multi-select. Selected nodes render at `size + 2` with `outlineColor: var(--accent)`. A click on the canvas background clears both the selection and the pinned note-preview. Selection survives the row↔graph view switch via `sharedSelection`. Footer shows the count. **Note:** the graph→row write-back path exists (the graph view calls `onSelectionChanged` which feeds `sharedSelection`), but the row view doesn't yet consume `sharedSelection` on re-entry — selection persists across switches *from* the row view *to* graph, not yet the other direction; row-view consumer is a small follow-up

### View menu

A single eye-icon button lives on the **pane's pinned toolbar**, always visible — same eye icon the editor's view-options menu uses (`editor.md` → `## View options menu`). Clicking it opens a unified "View options" popover that carries: (a) a **View as** radio (Tree / Graph / Markdown — three peer rendering modes of the same tree, [[spec:cluster-editor-graph-view-toggle]]) that replaces the prior 3-button toggle strip in the toolbar; (b) in tree mode, **Expand all** / **Collapse all** verbs that mutate the pane-local `expanded: Set<NodeId>` (per [[spec:cluster-editor-row-primitive]]) so the whole tree opens or closes in one click; (c) in graph mode, the graph-specific switches (leaves visibility / layout / show outliers / fit / reset / note-preview toggle). The menu refreshes in place when the view-mode radio changes; switching the View-as mode swaps just the pane body, leaves the toolbar intact, and preserves the current selection (multi-selecting in row view then toggling to graph shows those nodes highlighted). In the egui immediate-mode UI the menu is rebuilt each frame, so there's no mounted-popover lifecycle to manage — the eye button is a stable anchor and the menu's items always reflect the current view mode. Choices: [cluster-editor-graph-view-view-menu]
status:: done
note:: Single eye-icon button on the pane's pinned toolbar (`app/src/panels/cluster_review/mod.rs`, always visible — not gated on the active variant) opens a unified popover with: (a) a "View as" radio (Tree / Graph / Markdown — replaces the prior 3-button toggle strip on the toolbar); (b) in tree mode, **Expand all** / **Collapse all** verbs over the pane-local expanded set (every cluster / outlier-bucket node added on expand-all; cleared on collapse-all); (c) in graph mode, the graph-specific items (leaves / layout radios + show-outliers / fit / reset / note-preview toggle) folded in. Glyph matches the editor toolbar's view-options icon. The view-mode radio refreshes the menu in place. Same menu primitive as the editor's view-options menu

The "View as" radio is a sub-sub-mode of `cluster-tree`, not a new `BufferMode`. [cluster-editor-graph-view-toggle]
status:: done
note:: View-mode selector lives inside the unified eye-icon "View options" menu on the pane toolbar ([[spec:cluster-editor-graph-view-view-menu]]) — a "View as" radio with Tree / Graph / Markdown options, replacing the prior dedicated 3-button toggle strip. Sub-sub-mode of `cluster-tree`, not a new `BufferMode`. Switching destroys+remounts the canvas (no orphan WebGL refs); selection survives

- **Leaf visibility.** Three modes: `Hide leaves` (only cluster nodes render), `Auto (LOD)` (leaves hidden when zoomed out below a threshold, fade in as the user zooms in — default), `Show all leaves` (every leaf node is always present in the canvas). [cluster-editor-graph-view-leaf-visibility]
status:: done
note:: View-menu radio: `Hide leaves` / `Auto (LOD)` (default — hidden when camera ratio ≥ 1.2) / `Show all leaves`. Persisted in saved view state
- **Layout.** Radio: `Radial (default)` / `Vertical tree` / `Horizontal tree` / `Force-directed`. Switching re-runs the chosen layout and animates nodes to their new positions. [cluster-editor-graph-view-layout]
- **Show outliers.** Bool toggle, default on. When off, the disconnected outlier node (and its leaves, when leaves are visible) is hidden from the canvas. Distinct from the build-time `include_outliers` option (which controls whether outliers are *generated* in the first place — per [[spec:cluster-review-tab-config-section]]); the view-menu toggle just hides them in the current view. [cluster-editor-graph-view-show-outliers]
status:: done
note:: View-menu toggle, default on. When off, the outlier-bucket node and its descendants are dropped from the renderer (distinct from build-time `include_outliers` which controls whether they're generated). Persisted
- **Reset view** / **Fit to view.** Menu actions that re-center and rescale the canvas. Reset returns to the layout's default zoom + pan; Fit-to-view scales the canvas so every visible (non-hidden by other toggles) node fits in the viewport. [cluster-editor-graph-view-reset-fit]
status:: done
note:: View-menu `Fit to view` animates the camera to frame the graph; `Reset view` snaps the camera to its default center + zoom. Camera state persisted (debounced 400ms) to saved view state

View-menu choices are per-tree saved view state (persisted in the tree doc's frontmatter under `hiker.view_state`, or in `vault/.hiker/config.toml` keyed by tree id — implementation choice). Pan/zoom positions also persist there so the user comes back to where they left off. [cluster-editor-graph-view-saved-view-state]
status:: done
note:: Per-tree state (layout id, leaf visibility, show-outliers, policy filter, camera) persisted in `localStorage` under `hiker.clusterEditor.graphView.<treeId>`. Spec explicitly allows this or a `cluster_trees.view_state` column; localStorage is the simpler of the two for UI-only state and adds zero schema migration

### Outlier rendering in the graph

The outlier bucket renders as a **separate disconnected node**, floating off to the side of the main tree (default: lower-right corner of the canvas). It carries the same "Outliers (N)" label as the row view's virtual node. Its policy chip works identically to any other node's (the canonical outlier-policy flow, per [[spec:cluster-editor-outlier-policy]]). [cluster-editor-graph-view-outlier-disconnected]
status:: done
touches:: [[code:hiker/panels/cluster_graph]]
note:: the outlier-bucket layout in `app/src/panels/cluster_graph.rs` places the outlier bucket floating off to the lower-right and drops the parent edge whose parent is an outlier bucket, so it renders disconnected. Label `Outliers (N)`

The build-time `include_outliers = false` option (per [[spec:cluster-review-tab-config-section]]) suppresses the outlier bucket entirely — notes that would have been outliers get force-routed into their nearest cluster instead. The view-menu's `Show outliers` toggle only hides the rendered node, not the underlying data; both can be set independently.

### Renderer integration

The graph view consumes the shared graph renderer per `design.md`'s renderer pattern. Concrete plumbing:

- `core::trees` produces a `ClusterTreeGraph { nodes, edges }` DTO via a dedicated query (`Trees::tree_as_graph(tree_id)`). Nodes carry id, name, kind, member_count, resolved_policy, explicit_policy_flag, depth. Edges are parent → child.
- `app/src/panels/cluster_review/graph.rs` drives the cluster-tree graph view, reusing the shared egui force-graph widget (`app/src/widgets/force_graph.rs`) — the same renderer the vault-wide graph view uses; node-color + node-size and policy-filter overlays are app-state shape concerns that live in the panel module per `design.md`.
- Layout runs on a background worker (`force_layout::LayoutWorker`) so opening the graph view doesn't block the UI; being a native app, there's no separate bundle to load. Same pattern as the vault graph view. [cluster-editor-graph-view-lazy-load]
status:: done
touches:: [[code:hiker/panels/cluster_graph]]
note:: the graph view (`app/src/panels/cluster_graph.rs`) only builds its layout + renderer state when the user toggles to graph view; the egui force-graph widget is a native dependency so there's no separate bundle to lazy-load
- Re-renders on tree frontmatter changes (policy edits, name edits, structure edits via the row view, triage producing new rows). Implementation can either fully re-mount or compute a diff and patch — the renderer adapter's capability flag for in-place updates determines which.

The cluster-tree graph view is a *separate surface* from the vault-wide graph view (different data, different consumer module). They share the renderer primitive, not the data plumbing. [cluster-editor-graph-view-not-vault-graph]
status:: done
touches:: [[code:hiker/panels/cluster_graph]]
note:: The cluster-tree graph view (`app/src/panels/cluster_graph.rs`) consumes the shared egui force-graph widget but its data plumbing (cluster trees + policies) is entirely separate from any future vault-wide graph view (notes + wikilinks). Different panels, shared widget primitive only


## Reusable row primitive

The row component (chevron + icon + name + summary preview + members count + policy chip + selection state + multi-level hierarchy + multi-select + right-click context menu) is shared Rust rendering in the cluster panel (`app/src/panels/cluster_review/`), used by both the sidebar mode and the center pane; the two surfaces can diverge in spacing / typography without forking the rendering. Per-surface state (`expanded` / `selection` sets) is owned by the caller and survives refreshes triggered by queue events. Future hierarchical surfaces (e.g. saved-collections-of-collections, multi-axis cluster trees) plug into the same primitive. [cluster-editor-row-primitive]
status:: done
note:: The shared row primitive lives in `app/src/sidebar/clusters/tree.rs`; both the sidebar (`app/src/sidebar/clusters/mod.rs`) and the expanded pane (`app/src/panels/cluster_review/mod.rs`) consume it. It carries: chevron + name (click-to-edit on clusters / click-to-open on leaves) + summary preview (click-to-edit) + members count + ↻ staleness badge + policy chip (popover: Tag/Move/Freeze/Clear) + outlier virtual node + Shift/Cmd/Ctrl-click selection (incl. shift-range via display-order anchor) + right-click context menu (Move to… / Split / Subcluster… / Merge children up / Summarize / Drop cluster / Send to outliers / Promote out of outliers…) + multi-select toolbar (Merge siblings / Drop / Summarize / Stage move to / Stage tag with / Clear) + drag-and-drop reparent with promote-to-top drop band and floating drag chip

Trails and the vault-wide graph/map view (per `design.md`) deliberately do *not* reuse this row primitive — trails are sequential not hierarchical, and the vault graph's edge-rendering needs are fundamentally different from a tree's. The reuse is at the row-primitive level only. [cluster-editor-not-for-trails-or-graph]
status:: planned
note:: the row primitive isn't reused by trails or the vault graph view (different data models)

The cluster editor *does* reuse the **graph renderer** (the egui force-graph widget per `design.md`'s graph-view bullet) for its own tree-shaped graph view (next section). The exclusion is about data models and surfaces, not about the underlying rendering primitive — the renderer pattern in `design.md` exists precisely so multiple surfaces can ride the same widget with their own data. [cluster-editor-renderer-reuse]
status:: done
touches:: [[code:hiker/panels/graph]]
note:: the egui force-graph widget (`app/src/widgets/force_graph.rs`, `app/src/panels/graph.rs`) is the shared renderer. Cluster editor's graph view is the first consumer; a future vault-wide graph view rides the same widget


## Out of scope

- **Trails and graph/map view.** Different surfaces, different primitives. Trails are sequential; graph is non-hierarchical. The cluster editor explicitly does not subsume them.
- **Real-time collaborative editing of a cluster tree.** Single-user only.
- **Sharing trees across vaults.** Import-tree-from-file works for cross-vault transfer of a tree shape, but there's no automatic sync.
- **Multi-axis trees** (semantic + temporal + entity layered). Single tree shape per draft; multi-axis is `design.md`'s deferred slug.
- **In-place re-clustering of *already-placed* notes** (i.e., re-running the build over notes that already have folder placements and reconciling the diff). The current model is "build a fresh tree, manually edit, apply, then triage handles ongoing." Not a continuous reconcile.


## Deferred

- **Collaborative review** — multiple cursors, comment threads on cluster nodes. Not on the v1 roadmap.
- **Cluster diff view** — render the diff between two cluster trees (e.g. a fresh build vs. the saved triage tree) so the user can see what changed. Useful for "did the structure shift since I saved?" but speculative until real use shows the need. [cluster-editor-tree-diff-view]
status:: planned
note:: deferred — diff between two cluster trees (e.g. fresh build vs. saved triage) so user can see what shifted
- **Tree export to non-hiker formats** (org-mode, opml, etc.). The markdown view is the in-house export shape; converters land if a real workflow asks.
- **Per-policy `min_confidence` UI.** The data model supports it; the policy editor's UI for setting per-policy thresholds is deferred until users find the simple "policy fires on any match" too aggressive.
- **Branching saved-trees.** One saved triage tree per vault. Multiple saved trees with selection at triage time is the natural extension; deferred until users need it.


## Forward refs

- `core::trees` — owner of the per-tree `.md` files at the visible `new_cluster_tree_dir` (default `cluster-trees/`); (de)serializes the `hiker.nodes` frontmatter and commits edits through the op-log. Same module-discipline pattern as `core::trails` — plain Rust types out, the on-disk YAML shape never leaks.
- `core::cluster_editor` — the implementation home of the UI-facing editor operations; sibling to `core::cluster` / `core::suggest` / `core::trees`. Consumes `core::cluster::build_tree`, applies edits through `core::trees`, emits one-off pending ops via `core::ops` for the multi-select Stage move/tag verbs.
- `core::tasks` — every LLM-driven action in this surface (initial naming, regeneration, merge/split renaming) submits there.
- `editor.md` — sidebar mode switcher lives in the editor's domain; this spec defines the cluster-trees mode body, the editor.md mode-switcher entry is owned there.
- `settings.md` / `op-log.md` — pending ops carry their action in `OpKind` (`Rename` for moves, `SetFrontmatter` for tags); `surface = "triage"` and `surface = "cluster-editor"` in op metadata are produced by this doc's flows.
- [[spec:keybind-registry]] — chord ids reserved: `cluster-editor.toggle-expand`, `cluster-editor.merge-siblings`, `cluster-editor.merge-up`, `cluster-editor.split`, `cluster-editor.regenerate`. Chords TBD; land when each action is wired.
