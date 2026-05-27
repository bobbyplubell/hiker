# Cluster editor

Interactive surface for viewing, manually editing, and configuring automation on a cluster tree (per `clustering.md`'s `ClusterTree`). Replaces the markdown-proposal flow as the primary review surface for one-shot suggestions; the markdown form (`suggestions-proposal-md`) is how a tree is serialized on disk — a per-tree `.md` file whose frontmatter holds the structure — not the user's working surface.

The headline decisions:

- **Lives in the left sidebar as a switchable mode, with an Expand button to flip into a full center pane for graphical work.** The sidebar gains a mode switcher (Files / Cluster trees / future Trails) just above the existing `+ New note` row; switching to Cluster trees mode swaps the sidebar body to the cluster editor, hiding the filetree-only chrome (+New, `…` actions, Trash bin). An Expand button on the cluster-trees header sends the editor to the center pane, replacing the editor with a graphical tree view tuned for drag-drop reshaping; click a leaf in expanded mode to open the note in the editor, the existing back-navigation returns to the expanded tree. Mirrors `chat-panel-expand-to-editor`. [cluster-editor-sidebar-mode, cluster-editor-pane-expand]
- **Multiple trees can be open at once.** The sidebar's Cluster trees body shows a list of open trees — typically the saved triage tree (always present once saved) plus zero-or-more ephemeral trees from one-shot runs. Each tree expands inline to show its node hierarchy. Header on each tree row shows name + state pill (`draft` / `applied` / `saved as triage`). Switching between trees is just expanding a different one — no modal context-swap. [cluster-editor-multiple-trees-open]
- **Manual reshaping at every level.** Move notes between clusters, merge siblings, merge children up into the parent, split a cluster (re-run the partitioner against just its members), rename a cluster, edit an LLM-generated summary, drop a cluster (members fall to outliers), promote outliers into a cluster, run scoped Summarize over selected / stale / all clusters. Drag-and-drop reparents in the row view (`cluster-editor-dnd-reparent`); multi-select uses Shift-click for range and Cmd/Ctrl-click for toggle; buttons on the multi-select toolbar drive verbs that aren't naturally drag-shaped (merge / split / Summarize selected / Stage move / Stage tag). Every edit is undoable until apply. [cluster-editor-node-operations]
- **Automation policies attach at any level.** A policy (`Tag` / `Move` / `Freeze`) can be set on a leaf, a mid-tree cluster, or the root. `Tag` and `Move` each carry a `require_review` toggle; `Freeze` means "do nothing for matches in this subtree." Resolution at triage time walks up from the affected note to the nearest ancestor with an explicit policy; no policy anywhere up to the root means the note is left alone. A saved tree is a *policy program*, not a single classifier with a knob. [cluster-editor-policy-any-level, cluster-editor-policy-resolution-walk-up]
- **LLM-generated names and summaries are user-editable, with provenance.** Click-to-edit on any cluster's name or summary inside the editor. Edited values get `user_edited: true` stamped on the node so a re-run of the cluster build (or a regen of summaries via `core::tasks`) doesn't clobber them. Re-generating an edited node's summary requires explicit "Regenerate" action — implicit overwrite is forbidden. [cluster-editor-edit-name-summary, cluster-editor-user-edit-provenance]
- **Auto-save in-progress edits to the tree's `.md` file.** Every manual edit rewrites the tree's frontmatter on its per-tree `.md` file (`vault/.hiker/trees/<tree-id>.md`, owned by `core::trees`) through the op-log working layer — each edit is a `SetFrontmatter` op like any other document write (per `op-log.md`). The markdown body is a regenerated read-only render of the same frontmatter; there's no parallel on-disk store. Closing and reopening the editor resumes where you left off; the draft survives app restarts. Discard-draft is an explicit button. [cluster-editor-draft-persistence]
- **Building a tree (or reclustering a subtree) happens in a clustering review tab, not a modal.** "New tree" and "Recluster subtree…" both open a `cluster-review` app-page tab. The sidebar's Cluster trees mode exposes a single "New tree" button as the entry point — algorithm choice (including `From folders` as one of the algorithm options) happens inside the tab, not via per-algorithm buttons. The tab carries three stacked sections: a configuration panel (lifecycle / scope / algorithm + tunables, all rendered at the same level — no nested "Advanced" disclosure), a Run button that kicks off the structural clustering pass on a background task with live progress + incremental cluster reveal, and a review panel that renders the resulting tree as either an expandable hierarchical list or a graph (the user toggles). The user can adjust params and re-run as many times as wanted before committing. A single Confirm action persists the structural tree as a `draft` `.md` file with placeholder names and flips the tab to the cluster editor pane. LLM naming is a separate optional follow-up — never bundled into Confirm by default — invoked from the cluster editor pane or via an opt-in "Name with LLM after confirm" toggle in the review tab's configuration. Nothing about the structural result is written to disk until Confirm. [cluster-review-tab]
- **Apply and Save-as-triage are separate actions.** Apply walks the current tree state and emits one pending op per leaf whose resolved policy is `Tag` or `Move` (`surface = "cluster-editor"`, `metadata.tree_id`); the user then bulk-reviews via the tree-scoped batch-review pane (see below). Save-as-triage persists the tree with its policies as the active triage classifier (replaces any prior saved tree); does *not* enqueue staging rows itself — triage emits them as matches fire over time. A user can both apply and save-as-triage from the same tree if both make sense. [cluster-editor-apply-action, cluster-editor-save-as-triage]
- **Batch review for one-shot Apply.** Apply opens a tree-scoped review pane that lists every staging row produced by this Apply pass — Accept-all, per-row Accept/Reject, inline edits to target folder / tag slug, all in one surface. The same rows are also visible from the activity-detail Pending filter; the in-pane view is the convenience surface for one-shot users who don't want to leave the cluster editor. The tree's `state` flips to `applied` once every row is resolved (accepted or rejected). [cluster-editor-batch-review-pane]


## Sidebar mode switcher

The left sidebar's top region grows a mode switcher row, sitting between the vault bar (Open vault / Home / Settings — at the top of the app, per `editor.md`) and the existing `+ New note` row. Three modes (initially):

- **Files** (default) — current sidebar content: `+ New note` / `…` actions row at top, file tree, Trash bin pinned at bottom. Unchanged.
- **Cluster trees** — switcher swaps sidebar body to the cluster editor. The `+ New note` / `…` row hides; the cluster editor brings its own header (tree-name selector, "New tree from current vault" action, mode-specific `…` menu). The Trash bin stays pinned at the bottom — trash is multimodal (may contain notes, trails, cluster trees) so it's shared across modes.
- **Trails** — reserved slot, not implemented in v1. The switcher offers it as a greyed entry once trails land.

The switcher lives in the sidebar's top row alongside the persistent `+` (new note) and `⋯` (mode-aware actions) buttons — see `editor.md` for the full row layout. Three icon-buttons on the left (file-tree glyph / cluster-tree glyph / trail glyph), pressed-state on the active mode. Switching modes is purely a sidebar-content swap — the editor pane on the right is unaffected, the active buffer stays loaded, the discovery panel keeps its state. Mode is persisted per-vault under `vault.sidebar_mode` via the existing `set_setting` plumbing. [sidebar-mode-switcher, sidebar-mode-persistence]

A small detail: the sidebar's collapse toggle (`sidebar-toggle-icon`) keeps its existing behavior — it hides the whole sidebar regardless of mode. Modes don't have their own collapse states; they share the sidebar's one.

## Cluster trees mode (sidebar)

Sidebar body when the mode is Cluster trees. Header carries the mode-specific actions; body lists the open trees with their tree contents nested.

```
┌─ Sidebar ─────────────────────────────────┐
│ [Files] [Trees]* [Trails (soon)]          │  ← mode switcher
├───────────────────────────────────────────┤
│ Cluster trees                       [...] │  ← mode-specific header
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

- **New tree** — the primary affordance. Opens a `cluster-review` app-page tab where the user configures scope/method/params, runs the structural clustering pass, reviews the result, and confirms (firing the LLM naming pass and persisting the tree as `draft`). Same as the old `hiker suggest` entry point (per `suggestions.md`); the cluster editor is just the new review surface. Lifecycle (**Sapling** / **Evergreen**) is one of the form fields — a hint that sets the post-confirm default action, not a hard mode. Full detail in the "Clustering review tab" section below. [cluster-editor-new-tree-action, cluster-editor-sapling-evergreen-lifecycle]
- **`…` menu** — mode-specific overflow: "Open saved triage tree" (no-op when already open), "Import tree from file" (load a `cluster-tree.json` from elsewhere — useful for sharing trees), "Discard all drafts," "Tree settings" (per-node policy defaults, etc.). [cluster-editor-mode-menu]

Each tree's body is a hierarchical list, one row per cluster or leaf note. Cluster rows are collapsible (chevron at left); leaf-note rows are clickable to open the note in the editor pane on the right. Multi-select works via Shift/Cmd-click on rows; selection survives expand/collapse so a bulk merge across collapsed sections is still possible.

Row layout:

```
[chevron] <name>                                 [members] [policy chip] [drag handle]
          <summary preview, one-line>
```

- **Chevron** signals row kind and expand state on its own: `▾` / `▸` on a cluster with children, blank space on a leaf or empty cluster. No separate per-row type glyph — chevron + name + members count carry the kind without the visual noise.
- **Name** is the cluster's name (LLM-generated or user-edited) or the note's basename. Click-to-edit on cluster names; leaf names are read-only (rename a note via the regular file-tree affordances).
- **Summary preview** is the cluster's LLM-generated summary truncated to one line; click expands to a full-text editable area below the row, click the area's name in turn to edit. Leaves don't show a summary. A `↻ N` badge appears on the right of the summary preview when `summary_membership_churn > 0`, displaying the count of membership changes since the summary was generated (per `cluster-summary-staleness-counter`). Clicking the badge offers a quick Regenerate action for that node. [cluster-editor-summary-staleness-badge]
- **Members count** is the number of notes in the cluster's full subtree (including all nested children's leaves). Empty for leaves.
- **Policy chip** shows the resolved-or-explicit policy for this node — `tag: <slug>` / `move: <path>` / `freeze` / blank (no policy, walks up to nearest ancestor). Chips for policies with `require_review = true` get a small badge (`tag: research ⏸` / `move: research/ ⏸`) so the review-required state is visible without opening the editor. Clicking the chip opens the policy editor for that node.
- **Drag handle** lets the user drag the row onto another cluster (move) or onto another sibling (reorder, no semantic change).

Outliers render as a special virtual node at the bottom of every tree level — labeled "Outliers (N)" with a distinct icon. Drag a leaf into a cluster to promote it; drag a leaf out of any cluster onto Outliers to demote it. The outlier bucket is the sink for "no good cluster fit"; users can manually fish notes out of it during reshape. [cluster-editor-outlier-virtual-node]


## Expanded mode (center pane)

Clicking the Expand button on a tree row sends that tree to the center pane, replacing the editor. This is the surface tuned for heavy graphical reshaping — wider rows, larger drag targets, multi-pane "before/after" preview, more screen real estate for visualizing N-level trees.

The expanded mode is a new editor-pane sub-mode, joining the existing list (editor / vault-home-overview / vault-home-detail / settings / chat-expanded). The sidebar's cluster trees mode keeps showing the same tree (in its docked form) so the user can switch back by collapsing the expanded view. [cluster-editor-pane-mode]

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

- **Click a leaf note row → open the note in the editor.** This *replaces* the expanded tree view in the center pane with the regular editor on the clicked note. The existing navigation history (`navigation-history-stack`) records the expanded-tree state; the back button returns to it. From the user's perspective the loop is: "expanded tree → click note → read/edit → back → expanded tree." No special "back to tree" affordance needs adding; it's the same nav-stack back the rest of hiker uses. [cluster-editor-pane-leaf-click-opens-note, cluster-editor-pane-back-to-tree]
- **Click a cluster row → expand/collapse** as in the sidebar form. No navigation hop. Pane-local `expanded: Set<NodeId>` is per-tree and survives refreshes (queue-event-driven re-fetch after a `raptor_summarize` task lands doesn't collapse the user's expansion state).
- **Full row UX parity with the sidebar.** The shared row primitive (`cluster-editor-row-primitive`) gives the pane the same chevron / click-to-edit name + summary / right-click context menu / policy chip popover / staleness badge / multi-select behavior as the sidebar. Right-click verbs are identical: cluster rows expose Move to… / Split / Subcluster… / Merge children up / Drop cluster; leaf rows expose Move to… / Promote out of outliers… (under the outlier bucket) / Send to outliers; the outlier bucket exposes Move to… / Drop cluster.
- **Multi-select via Shift/Cmd-click** picks rows across levels (selecting a cluster includes its subtree implicitly for purposes of the bulk-action toolbar). The multi-select toolbar surfaces in the pane header alongside the existing Apply / Save-as-triage cluster when `selection.size > 0`, with verbs Merge siblings / Drop / Stage move to… / Stage tag with… / Clear. Pane-local selection state also survives queue-event refreshes.
- **Markdown view toggle** in the toolbar flips the pane between the graphical tree and a read-only render of the equivalent markdown shape (per `suggestions-proposal-md`) — the same body the tree doc carries on disk. The render is read-only with respect to the structure; structural edits go through the graphical surfaces, which rewrite the frontmatter. The text-editing escape hatch for users who want to bulk-rewrite via vim-style motions is to open the tree's `.md` file directly in the editor and edit its frontmatter — that's an ordinary buffer edit on an ordinary note, and it rides the op-log working layer like any other write. [cluster-editor-markdown-view-toggle]
- **Apply / Save as triage / Discard draft** are toolbar buttons. Apply emits staging rows for every leaf with a `Tag` or `Move` policy and opens the batch-review pane (see below); Save as triage persists the tree as the triage classifier; Discard draft confirms then deletes the tree's `.md` file.
- **Regenerate names** runs the cluster naming/summarization prompt for any cluster whose name/summary is *not* user-edited (per the `user_edited` flag). Each regeneration is one task in `core::tasks` (per `task-queue.md`); the user can watch progress in the queue widget. Regenerating a user-edited node requires per-node explicit "Regenerate this node" from its row menu. Tasks are submitted **bottom-up**: nodes are grouped by depth, and each depth's `RaptorSummarize` batch is awaited before the next shallower batch — so a parent summarizes over its children's real names, not placeholders (each worker reads its children's current `name`/`summary` at execution time). [cluster-editor-regenerate-via-task-queue]


## Tree storage: per-tree `.md` files

Cluster trees live as per-tree markdown documents at `vault/.hiker/trees/<tree-id>.md`, owned by `core::trees` (sibling to `core::store`'s `index.db` and `core::oplog`). Each tree is one `.md` file: the full structure lives in YAML frontmatter (`hiker.kind: cluster-tree`); the body is a regenerated read-only human rendering (per `suggestions-proposal-md`). Trees ride the op-log substrate like any other markdown document (per `op-log.md`) — they sync, they carry version history, and every edit is a CRDT op. [trees-md-store]

The `.hiker/trees/` directory is carved out of the watcher's `.hiker/`-ignore rule (same shape as the `.hiker/trails/` and `.hiker/sessions/` carve-outs) so tree docs route to the indexer and the op-log like any other md file; `core::trees` owns watcher suppression around its own writes. Module discipline mirrors `core::trails`: pure Rust types out of the module, all frontmatter (de)serialization behind the boundary, the on-disk YAML shape never leaks. There is no schema-version file and no migration code — the frontmatter is self-describing, and unknown keys are preserved on round-trip. [trees-module-discipline]

### Frontmatter shape

```yaml
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
  vault_snapshot: <rev>         # vault rev at build time; advisory, optional
  nodes:                        # flat list; hierarchy via `parent`
    - id: root
      kind: cluster
      name: Vault root
      confidence: 1.0
    - id: n1
      parent: root
      kind: cluster
      name: Embedding research
      summary: "Notes about embedding models and vector search."
      user_edited_name: true
      policy: { move: research/embeddings/, require_review: false }
      confidence: 0.91
      churn: 0                  # summary_membership_churn
    - id: leaf-7
      parent: n1
      kind: leaf
      note: { id: 01HR…, path: inbox/whisper-notes.md }   # double-link
---
# Cluster tree — 01HXP7Z…  ·  draft
… regenerated render (per suggestions-proposal-md) …
```

The node list is flat; `parent` (omitted on the root) encodes the hierarchy, mirroring the prior row shape. Defaults keep the YAML terse: `user_edited_name` / `user_edited_summary` default `false`, `churn` defaults `0`, `summary` defaults `""`, `policy` is omitted when none. `note` is present only on leaves and is a double-link `{ id, path }` (per `trail-double-link-references`) — the ULID is canonical and survives note renames, the rel-path keeps the file legible in external editors. **Centroids are not stored here** — packed embedding vectors would bloat the synced text and diff badly; they live in `index.db` (next). [trees-md-frontmatter]

### Centroids in `index.db`

Each cluster node's centroid (the L2-normalized mean of its members' embeddings, consumed by the placement classifier `cluster-place-beam-descent`) is a derived value, recomputable from member embeddings the index already holds. It lives in a derived `cluster_centroids` table in `index.db` keyed by `(tree_id, node_id)`, written by the build pass and by edits that change a cluster's membership. It follows the index's derived-table discipline — rebuilt on schema bump, fail-loud on mismatch (per `store-version-fail-loud`) — and a missing centroid is recomputed from members rather than treated as corruption. [trees-centroids-index]

### Edits as op-log ops

Every manual edit is a frontmatter mutation on the tree doc, committed through the op-log working layer as a `SetFrontmatter` op (per `op-log.md`'s programmatic write path). Because the whole `.md` — frontmatter included — is one `Y.Text`, the edit is an ordinary CRDT op: it merges, syncs, and carries author/timestamp metadata in the op-log side table. The semantic label for the edit (`move` / `merge` / `split` / `rename` / `set-policy` / …) rides the op's `metadata` field; no tree-specific `OpKind` variant is minted — `SetFrontmatter` is the shared substrate for structured-frontmatter writes (trees today, plugin-defined structures later). [trees-edit-setfrontmatter]

The build pass writes the initial node set when a new tree is created — there is no separate "build snapshot vs editable draft" distinction; every node is editable from the moment it lands. [cluster-editor-edit-history]

**Undo / redo.** The cluster editor keeps an in-memory session undo stack; each undo applies the reverse frontmatter edit through the working layer (itself a `SetFrontmatter` op). Cross-session "revert to an earlier state" rides the tree doc's normal version history — the same snapshot machinery every note has — not a bespoke per-tree log. There is no separate persisted history table. [cluster-editor-undo-redo]

Discard-draft on a tree whose `source = 'one-shot'` deletes the tree's `.md` file through `core::ops::delete` (so it lands in trash and is restorable like any note). Discard on a saved-triage tree is "Unsave as triage" — flips `state` back to `draft` and (optionally) deletes; the user is asked which. [cluster-editor-discard-draft]

The in-memory editable shape is unchanged — it just hydrates from the frontmatter `nodes` list instead of `cluster_nodes` rows (children are found by `parent` lookup, as before):

```rust
struct EditableNode {
    id: NodeId,                       // stable across edits within this tree's lifetime
    parent: Option<NodeId>,
    kind: NodeKind,                   // Cluster | Leaf | OutlierBucket
    note_ref: Option<NoteId>,         // present on leaves (path resolved via the index)
    name: String,                     // user-editable (clusters only)
    summary: String,                  // user-editable (clusters only)
    user_edited_name: bool,
    user_edited_summary: bool,
    policy: Option<NodePolicy>,
    confidence: f32,                  // from build pass; preserved through edits
    summary_membership_churn: u32,    // per cluster-summary-staleness-counter
    // centroid is loaded from index.db's cluster_centroids when the
    // placement classifier needs it, not carried on the node row.
}

enum NodePolicy {
    Tag  { slug: String,      require_review: bool },
    Move { folder: VaultRel,  require_review: bool },
    Freeze,                                        // never propose changes for matches under this node
}

enum NodeKind { Cluster, Leaf, OutlierBucket }
```

[cluster-editor-tree-shape]


## Clustering review tab

Building a new tree and reclustering a subtree both go through a `cluster-review` app-page tab. The tab is the configuration surface, the runner, and the structural-result reviewer; only once the user confirms does the LLM pass fire and the tree land on disk as a `.md` file. Entry points: the sidebar's `+ Suggest reorganization` / `+` button (`cluster-editor-new-tree-action`) opens a fresh tab; the row-menu's "Recluster subtree…" entry on a cluster row (`cluster-editor-recluster-subtree`) opens a tab pre-bound to that `(tree_id, node_id)`; the mode menu's "Rebuild" entry on an Evergreen tree opens a tab prefilled with the tree's saved scope/method/params. [cluster-review-tab-from-new-tree-action, cluster-review-tab-from-recluster-action, cluster-review-tab-rebuild-prefill]

### Tab kind

New entry in the `tab-kinds` enumeration (`editor.md`): `cluster-review`. Payload is a `ClusterReviewState` carrying the tab's purpose (`new-tree` | `recluster-subtree { tree_id, node_id }` | `rebuild { tree_id }`), the in-flight `ClusterParams` / `BuildMethod` / `BuildScope`, and — once Run has been clicked — the in-memory `BuiltClusterTree` from the structural pass. Non-buffer; editor toolbar and status bar hide on activation per `tab-kinds`. Opens sticky (directed action), like Properties. The autosave tab-state machinery (`autosave-tab-state-store`) records the tab kind + the configuration form; an in-memory result is *not* persisted across restarts — reopening the tab returns to the configure phase with the form prefilled. [cluster-review-tab-kind]

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

- **Name** — user-editable string, default `"<YYYY-MM-DD> reorg"` for new-tree, `"Recluster <selected node name>"` for recluster, the tree's existing name for rebuild.
- **Lifecycle** — Sapling / Evergreen radio. Sets the post-confirm default action (Apply for Sapling, Save-as-triage for Evergreen). Stored on the tree's row as a hint; doesn't constrain operations. Hidden in the recluster case (subtree reclustering reuses the parent tree's lifecycle).
- **Scope** — Whole vault / Current folder / Selected notes radio. Defaults from current file-tree context the same way the old modal did (`Vault` when nothing selected, `Folder(current)` when a folder is selected, `Notes(selected)` when notes are multi-selected). Per `cluster-build-scope`. Hidden in the recluster case (scope is fixed to the selected subtree's leaves). [cluster-editor-build-scope-picker]
- **Source types** — checkbox group for the file-extension filter that applies after scope: Markdown (.md) and Plain text (.txt). Default both checked. At least one must be selected before Run; an empty selection is flagged with an inline toast rather than firing the IPC with an empty input set. The selection rides through to the persisted tree's `scope` frontmatter so the triage classifier honors it on subsequent saves (a Markdown-only saved tree no-ops on `.txt` note saves). Visible on every purpose including recluster — a recluster of a mixed-type subtree may want to narrow to one type. Per `cluster-build-scope-source-types`.
- **Algorithm** — single-combobox dropdown with `HDBSCAN` (default), `Leiden`, `Hybrid`, `GMM (falls back to HDBSCAN)`, and `From folders`. The choice is both the partitioner pick (for the semantic options) and the underlying `BuildMethod` switch (`From folders` selects `BuildMethod::FromFolders`; everything else selects `BuildMethod::Cluster { params }`). No separate "Build method" picker exists. Selecting an algorithm reactively refreshes the tunables shown below — no manual re-render. Forced to a semantic algorithm in the recluster case, matching the surrounding-tree-method-agnostic posture of `cluster-build-from-folders-uniform-output`. [cluster-review-tab-method-dropdown]
- **Algorithm tunables** — rendered at the same level as the rest of the configuration form (no separate "Advanced" disclosure). Fields shown per algorithm:
  - **HDBSCAN / Hybrid / GMM**: `min_cluster_size` (defaulted to half the current value in the recluster case, matching Split's heuristic) and `min_samples`.
  - **Leiden**: `k_nearest`, `edge_weight_floor`, `resolution` (γ), `iterations`, and a Leiden-flavored `min_cluster_size`.
  - **From folders**: `outlier_threshold` (only when `Include outliers` is on).
  - **Common across the semantic algorithms** (i.e. everything but From folders): `summary_confidence_threshold` and a **Disable recursion** checkbox that short-circuits the recursive Split loop so the build produces a single-level tree (`cluster-review-tab-disable-recursion`).
  
  `summarize` is *not* surfaced — the structural pass runs without an LLM regardless. Defaults match `clustering.md`.
- **Include outliers** — top-level toggle (default `on`). Same semantics as the old modal; applies to both semantic algorithms and From folders.
- **Carry policies down** — recluster-only checkbox (default `off`). When set, the selected node's resolved policy is copied as explicit policy onto every newly-built direct child. Per `cluster-editor-recluster-subtree-policy-loss`.
- **Name with LLM after confirm** — checkbox in the lifecycle area (default `off`). When checked, Confirm additionally submits the per-cluster `RaptorSummarize` tasks for non-user-renamed clusters at `Priority::Normal` (the same path the cluster editor pane's Regenerate names uses). Greyed out with a tooltip pointing at settings when `[llm].enabled = false`. The default is off — the canonical flow is "confirm structural tree, name later (if at all) from the cluster pane." [cluster-review-tab-confirm-with-naming-toggle]

### Recursion disable toggle

A dedicated **Disable recursion** checkbox lives in the Advanced disclosure (when `Method == Cluster`). When checked, `core::cluster::build_cluster_tree` returns immediately after the level-0 pass — no recursive merging of cluster summaries into higher-level clusters. The result is a single-level tree of leaf clusters plus the outlier bucket. Useful when (a) the user wants a flat reorganization proposal without higher-level groupings, or (b) Leiden's leaf-level communities are already at the right granularity and a second pass would just merge them back together. Wired through `ClusterParams.disable_recursion: bool` (default `false`); short-circuits inside the recursion loop rather than abusing `min_clusters_to_recurse` as a sentinel — the persisted tree's `method` frontmatter carries the explicit field, so the toggle's state is recoverable from a saved tree. [cluster-review-tab-disable-recursion]

### Run clustering

The `Run clustering` button kicks off the structural-only pass on a background task: top-down divisive Split per `cluster-build-recipe`, with `SummarizeMode::None` forced so no LLM calls are made. The UI is non-blocking — the user can scroll the Result panel, edit other tabs, or tweak un-locked configuration fields while the pass runs. The result is a `BuiltClusterTree` whose clusters have placeholder names (default: `"Cluster N"` numbered in member-count-descending order at the end of the pass) and empty summaries. The result lives in the tab's in-memory state — no `.md` file is written. Subsequent Run clicks discard the prior result and progress, then restart the pass with the current form values. [cluster-review-tab-run-clustering, cluster-review-tab-iterate]

The build pipeline supports a `summarize: SummarizeMode::None` short-circuit path that skips the `Summarizer` call entirely so the structural pass runs end-to-end without an LLM. [cluster-review-tab-structural-pass-no-llm]

While the pass is running, the Run button flips into a Cancel button — pressing it aborts the in-flight pass cleanly and drops any partial results. The button is the only progress affordance the user has to interact with; everything else (phase label, counters, partial tree) renders into the dedicated progress + result surface described below. [cluster-review-tab-cancel-pass]

### Progress surface

A progress row sits between the configuration section and the result panel while a run is in flight. It surfaces:

- **Phase indicator** — current pipeline phase (e.g. `loading embeddings`, `partitioning level 0`, `partitioning level N`, `finalizing`). Phase strings come from the build pipeline's progress stream (`cluster-build-progress-stream` in `clustering.md`); the UI renders them verbatim plus a small inline spinner.
- **Counters** — items processed, clusters discovered so far, outliers so far. Updated as the stream emits new counter events.
- **Elapsed time** — seconds since Run was pressed. Useful for tracking long pulls on big vaults.

The progress row hides when no run is in flight. It does not persist across re-runs — each Run resets it. [cluster-review-tab-progress-row]

### Result panel

Renders the in-memory `BuiltClusterTree`. Two view variants are available via a Tree / Graph toggle in the panel's toolbar; markdown view is not surfaced here (no LLM names yet). View options — current variant (Tree vs. Graph), per-cluster expand chevron state, and any per-view layout state — persist across re-runs within the same tab session: the user keeps the layout they were working in even after pressing Run again. Stale node ids in the expand set from a prior run are harmless (lookups against the new run's ids just return false). [cluster-review-tab-result-view-toggle, cluster-review-tab-view-state-survives-rerun]

**Tree view (default).** Hierarchical row layout reusing `cluster-editor-row-primitive` minus the affordances that don't apply pre-persistence (no policy chip — policies don't exist on un-persisted trees; no drag-handle — structural results are immutable from this surface; no staleness badge — `summary_membership_churn` doesn't apply). Per-cluster row shows: placeholder name (`"Cluster 1"`, `"Cluster 2"`, …), member count, centroid radius / confidence indicator, and an expand chevron when the cluster has children (nested sub-clusters or leaf members). Expanding a cluster reveals its members inline — nested sub-cluster rows + leaf-note rows, both using the row primitive at deeper indent. Leaves are click-to-open same as the sidebar form. Outliers render as a sibling row at the bottom of the root level with the distinct outlier glyph. Pane-local `expanded: Set<NodeId>` keyed by the in-memory build run's node ids; switching to graph view and back restores the expansion state. [cluster-review-tab-structure-preview, cluster-review-tab-result-expand]

**Graph view.** Node-link rendering of the built tree via the shared egui force-graph widget (per `cluster-editor-renderer-reuse`). Same size-by-members and label-style encoding as `cluster-editor-graph-view`, minus the policy color (no policies) and staleness tint (no churn). Layout default is radial; layout selection rides through the cluster editor's view-menu pattern when the panel is in graph mode. Pan / zoom / hover-detail / click-to-pin-preview behavior is shared with the cluster editor's graph view. Useful at the build-review stage for spotting structural pathologies at a glance — single giant cluster, fragmented outliers, depth imbalance — that the row view buries under expand chevrons. [cluster-review-tab-result-graph-view]

**Live cluster reveal.** Cluster rows appear in the result panel as the partitioner discovers them, not only after the full pass completes. Each top-level cluster's row appears as soon as the partitioner emits it; recursive sub-splits populate the cluster's children inline (driving the expand affordance). The user can browse the partial tree mid-build. The final member-count-descending ordering applies only after the pass finishes; mid-pass ordering is insertion-order so rows don't shuffle while the user is watching. The graph view re-renders incrementally on the same stream. [cluster-review-tab-live-cluster-reveal]

The placeholder names are user-editable inline before Confirm — clicking a placeholder opens the same inline-edit affordance as `cluster-editor-edit-name-summary`. User-renamed clusters stamp `user_edited_name = 1` so the (opt-in) LLM naming pass at Confirm time skips them. Summaries cannot be edited here (no summary to edit yet). [cluster-review-tab-rename-before-llm]

### Confirm

A single **Confirm** button (disabled until a Run has produced a complete result) persists the structural tree and lands the user on the cluster editor pane. Clustering ends here. LLM naming is **not** part of the Confirm flow by default.

Steps:

1. Persists the in-memory `BuiltClusterTree` as a new tree `.md` file (state `draft`, source per Sapling/Evergreen lifecycle), one `nodes` entry per node. User-renamed clusters carry their names with `user_edited_name: true`; every other cluster lands with its placeholder name (`"Cluster 1"`, `"Cluster 2"`, …) intact.
2. *(Opt-in)* If the configuration section's **Name with LLM after confirm** toggle is checked (per `cluster-review-tab-confirm-with-naming-toggle`), submits one `TaskKind::RaptorSummarize { tree_id, cluster_node_id, level }` task per cluster (skipping user-renamed ones) at `Priority::Normal`. Same queue path as `cluster-editor-regenerate-via-task-queue`. Otherwise this step is skipped.
3. Flips the tab's kind from `cluster-review` to `cluster-batch-review` for the new tree, with sub-mode `cluster-tree`. The user lands inside the cluster editor pane (`cluster-editor-pane-mode`); the sidebar's Cluster trees mode picks up the new tree on its next refresh.

The Confirm button itself is never gated on `[llm].enabled` — the structural tree persists regardless. The naming toggle is greyed out when LLM is disabled. [cluster-review-tab-confirm-single-path, cluster-review-tab-transition-to-pane]

The cluster editor pane is the canonical surface for the (optional) naming follow-up: its toolbar's **Name clusters with LLM** button (a contextual rename of the existing Regenerate names verb when the tree's clusters still carry placeholder names) is the primary affordance. Same task-queue path as `cluster-editor-regenerate-via-task-queue`. The user is free to skip naming entirely — live with placeholder names, edit them by hand from the cluster pane's inline-edit, etc. [cluster-editor-pane-name-clusters-cta]

For the recluster-subtree case, step 1 is "replace the selected node's descendants with the new subtree" (per `cluster-editor-recluster-subtree`) instead of "insert a new tree row"; step 3 returns the user to the existing tree's cluster editor pane. The optional naming step applies identically — it submits naming tasks for the new sub-clusters only.

For the rebuild case, step 1 is "write the new tree alongside the existing one"; the old tree is left intact for the user to discard manually (matches the existing `cluster-build-rebuild` posture).

### Discard

The `Discard` button (or closing the tab) drops the in-memory result and the form state. If a result exists, closes are gated by a `confirm` ("Discard the clustering result? You'll need to re-run to get it back.") — closing the tab with no result skips the prompt. Nothing on disk is touched. [cluster-review-tab-discard]

### One review tab per target

Opening "Suggest reorganization" with a `cluster-review` tab already open for the new-tree case activates the existing tab rather than spawning a duplicate (same shape as Properties' one-tab-per-path rule). Recluster tabs key on `(tree_id, node_id)`; rebuild tabs key on `tree_id`. The form state and any in-flight result are preserved when re-activating. [cluster-review-tab-deduplication]


## Operations

All operations are local edits to the tree's frontmatter `nodes` list, committed as one `SetFrontmatter` op on the tree doc (the semantic op name rides the op `metadata`). None of them mutate the vault — only Apply or a staging-row accept does that.

- **Move note between clusters.** Drag a leaf onto a different cluster row (or use the row menu's "Move to…"). Updates the leaf's `parent`. Centroids of source and target clusters are recomputed. [cluster-editor-move-note-between-clusters]
- **Merge sibling clusters.** Multi-select 2+ clusters at the same level → toolbar "Merge siblings" → creates one cluster whose members are the union, name and summary are auto-regenerated (queues a task in `core::tasks` for naming) unless the user names it explicitly first. [cluster-editor-merge-siblings]
- **Merge children up to parent.** Select the parent → "Merge up" → flattens that subtree by one level. Children's leaves become direct members of the parent; children's clusters disappear. The parent's name/summary is unchanged unless explicitly regenerated. [cluster-editor-merge-children-up]
- **Split a cluster.** Select one cluster → "Split" → re-runs HDBSCAN against just its current members with a tighter `min_cluster_size` (default `max(2, current_min / 2)`). Produces a new layer of children. The user can keep the split or undo. Splitting a cluster preserves the parent's name; the new children get LLM names via the task queue. [cluster-editor-split-cluster]
- **Recluster subtree.** Select one cluster → row-menu "Recluster subtree…" → opens a `cluster-review` tab bound to that `(tree_id, node_id)`. Same flow as new-tree: configure → run → review → confirm. On Confirm, the structural result **replaces the subtree in place** — every descendant cluster node is deleted, the freshly-built nodes are inserted under the selected cluster, and the leaves re-parent onto their new positions. The selected cluster's own row (id, name, summary, user-edit flags, policy) is preserved; only its descendants change. Differs from Split: Split is one-level and produces a single new layer of children; Recluster subtree is recursive and rebuilds every level beneath the selected node. The form's default `min_cluster_size` is half the surrounding tree's value (same heuristic as Split). Works on `Cluster`-method and `FromFolders`-method trees alike — the rebuilt subtree is always `Cluster`-shaped, matching `cluster-build-from-folders-uniform-output`. [cluster-editor-recluster-subtree]
- **Rename cluster** / **Edit summary.** Click name or summary → inline editor → save. Stamps `user_edited_*: true`. [cluster-editor-edit-name-summary]
- **Drop cluster.** Toolbar action on a selected cluster: drops the cluster, its children's leaves all fall to the nearest outlier bucket. Useful for "this cluster is junk; throw the notes back to inbox-via-outliers." [cluster-editor-drop-cluster]
- **Promote outliers / move out.** Drag a row in/out of the outlier bucket. Same machinery as move-note. [cluster-editor-promote-outlier]
- **Set / clear policy.** Click the policy chip on any node → policy editor (radio: `Tag` / `Move` / `Freeze` / `None` + the action's parameter for `Tag` (slug) or `Move` (folder) + a "Require review for new matches" checkbox available on `Tag` and `Move`). Setting a policy on an ancestor automatically becomes the resolved policy for descendants without their own. [cluster-editor-set-policy]
- **Stage move / Stage tag (one-off, multi-select).** With N leaf rows selected, the bulk-action toolbar exposes "Stage move to…" and "Stage tag with…". Each verb writes N rows directly into the op log (one per selected leaf) with `surface = "cluster-editor"`, `action = "move_note"` or `"apply_tag"`, and `metadata.tree_id` for traceability. No node policies are mutated. Distinct from setting an `auto-move` / `auto-tag` policy and clicking Apply: the policy path is the saved-tree mechanism that fires on future matches; "Stage move/tag" is a one-shot batch against exactly the selected leaves. Useful when the user wants to act on an arbitrary subset of a tree without committing to a forever-rule. The proposals flow through the standard staging surfaces (activity-detail Pending filter, tree row indicators, editor toolbar pill) — the cluster editor doesn't gain a private review queue. [cluster-editor-multi-select-stage-move, cluster-editor-multi-select-stage-tag]
- **Summarize this cluster.** Cluster-row right-click verb. Calls `Trees::summarize` with `scope = Subset { ids: [node_id] }`, `overwrite_user_edited = false`, `recursive = false`. No-op when the node's name + summary are filled and `summary_membership_churn == 0`. [cluster-editor-summarize-verb]
- **Summarize selected.** Multi-select toolbar verb, visible when `selection.size > 0` and at least one selected node is a cluster. Calls `Trees::summarize` with `scope = Subset { ids: <cluster-rows-from-selection> }`, `overwrite_user_edited = false`, `recursive = false`. Leaves in the selection are ignored (leaves don't carry a summary). [cluster-editor-summarize-verb]
- **Summarize new / changed.** Tree-pane toolbar action (sits alongside Apply / Save-as-triage). Calls `Trees::summarize` with `scope = StaleOrUnfilled`, `subtree_root = None`, `recursive = true`. Disabled when zero rows match (everything is fresh). The button shows the pending count when non-zero (e.g. "Summarize new / changed (4)"). [cluster-editor-summarize-stale-action]
- **Drag-and-drop reparent.** See §"Drag-and-drop reparent" below; one of the operations, broken out separately for the visual-feedback and multi-select-drag detail.


### Drag-and-drop reparent

Drag a cluster row, a leaf row, or the outlier bucket onto another cluster row to set the target as its new parent. Single drops route through `Trees::move_node`; multi-select drags loop the same single-move IPC once per item so each move gets its own `move` history row (preserving per-item step-through undo). Works identically in the sidebar's `Cluster trees` mode and in the expanded center pane's row view (the graph view explicitly opts out per `cluster-editor-graph-view-no-reshape`). [cluster-editor-dnd-reparent]

Drop targets:

- **Onto a cluster row** — reparent under that cluster.
- **Onto the outlier bucket** — demote a leaf to outliers (same as the row-menu "Send to outliers" entry).
- **Onto the empty area above the root list** (a "promote to top level" drop zone, visible only while a drag is in flight) — set `parent_id = NULL` to make the dragged row a top-level child.

Multi-select drag: when `selection.size > 0` and the drag starts on a selected row, the whole selection is dragged. The drag chip near the cursor shows `N items` (per `cluster-editor-dnd-visual-feedback`). Non-selected rows initiate a single-item drag without touching the selection.

Invalid drops are rejected at the start of the drag (drop target rendered with the "no" cursor, no highlight applied) and ignored if a release happens over them:

- **Cycle** — dragging a cluster onto one of its descendants (including itself).
- **Leaf onto leaf** — leaves can't have children.
- **Cluster onto its current parent** — no-op.
- **Drop on a different tree** — DnD targets stay within the originating tree; cross-tree moves don't exist.

Cancellation: Escape during a drag aborts. Releasing over a non-target area (empty space that isn't the promote-to-top zone, panel chrome, the toolbar) also aborts.

History: each successful drag is one `SetFrontmatter` op per item moved (semantic name `move` on the op metadata), pushed onto the session undo stack with the prior parent recorded for the reverse edit. A multi-item drag is N separate ops / undo entries, not a single batch — keeps undo granular (the user can step-undo through them). The selection state is preserved across the drag for round-trip undo readability.

### Drop visual feedback

Three states render during a drag, driven by egui's pointer drag state on the row primitive: [cluster-editor-dnd-visual-feedback]

- **Drop-target highlight** — the row under the pointer (when it's a valid target) renders with a 2px inset accent ring on the row's border and a faint accent background tint. Same visual weight as the focus ring on settings inputs; deliberately subtle so the row's existing chrome (chevron, name, policy chip) stays legible during the drag.
- **Invalid-target rejection** — the row under the pointer (when it's an invalid target) renders with the `not-allowed` cursor and *no* highlight. The drag is allowed to continue moving (the user can keep searching for a valid target); the row just visibly refuses.
- **Drag chip** — a small floating chip follows the cursor (12,12 offset). For a single-item drag, the chip shows the dragged row's icon + name truncated. For a multi-item drag, the chip shows `N items` plus the icon of the first item. The chip uses the same rounded-white-background styling as the graph view's node labels (`cluster-editor-graph-view-label-style`) so it sits cleanly over any pane content.
- **Promote-to-top drop zone** — an inset accent band renders above the root list while a drag is in flight; releasing onto it sets `parent = NULL` via `Trees::move_node`. The band hides when no drag is active.

The visual feedback layer is rendered by the shared row primitive (`cluster-editor-row-primitive`); both the sidebar and the expanded pane pick it up without per-surface chrome.

### Multi-select range (shift-click)

The row primitive's selection handler splits the three modifier gestures (plain / Cmd-Ctrl / Shift) per file-manager convention.

- **Plain click.** Clears any existing multi-selection and re-anchors on the clicked row. The row's primary affordance (open leaf, inline-edit cluster name, expand cluster) still fires — clicking a row isn't a "select this row" gesture on its own; it's a "use this row" gesture.
- **Cmd-click / Ctrl-click.** Toggles the clicked row in the selection set and re-anchors on it (so subsequent shift-clicks pivot off the just-toggled row).
- **Shift-click.** Replaces the selection with the range from the current anchor through the clicked row in current display order (top-to-bottom walk of currently-rendered rows respecting expand/collapse), inclusive. The range can cross cluster boundaries — a range from one cluster's leaf to a sibling cluster's leaf includes every visible row between them (intervening cluster headers, the outlier bucket if it sits in between, etc.). Range membership is computed on the rendered tree at click time; expanding a cluster after a shift-click range was set doesn't grow the existing selection.

The anchor lives on `TreeUIState.anchor` (sidebar) / the equivalent per-tree state in the pane, and is cleared when the tree switches. With no anchor (first interaction with a tree), a shift-click is treated as a single-row range and sets the anchor. [cluster-editor-multi-select-shift-range]


### Recluster subtree: policy and placement semantics

Two consequences of reclustering a subtree are load-bearing enough to call out:

- **Per-node policies on the replaced descendants are lost.** Reclustering deletes every descendant cluster row, including any `Tag` / `Move` / `Freeze` policy set on those rows. Descendants that had no explicit policy were already inheriting from an ancestor — they keep inheriting (the policy lives on the surviving ancestor, not on the dropped descendant). To soften the loss, the clustering review tab's recluster form offers a "Carry policies down from selected node" checkbox: when checked, the selected node's resolved policy (its own or the nearest ancestor's) is *copied* onto every newly-built direct child as an explicit policy. Default-off; the spec's intent is that reclustering is destructive and the user opts in to carrying rules forward. The session undo stack snapshots the full prior subtree (the reverse edit for the `recluster-subtree` op) including its policies, so an undo restores them exactly. [cluster-editor-recluster-subtree-policy-loss]
- **Already-placed notes are not moved.** The filesystem is the source of truth for placement (per `clustering.md`'s framing of the tree as a recommendation surface, not durable infrastructure). Reclustering a saved-as-triage (Evergreen) tree's subtree changes how *future* notes routed through that subtree are classified by `cluster-place-beam-descent`; it does not move existing notes whose folder placement was driven by a prior triage match or by an Apply pass. If the user wants the new structure reflected on disk for the already-classified notes, they re-run Apply (one-shot) on the rebuilt subtree, which emits fresh `move_note` staging rows for each leaf under a `Move` policy. This is the same model as every other reshape op in this surface — no reshape moves files on its own. [cluster-editor-recluster-subtree-placement-decoupled]


## Per-node automation policy

Policies attach at any level — including the outlier bucket. A note's effective policy at triage time is determined by walking from the note's leaf up the tree, taking the first ancestor with an explicit policy. No policy anywhere = no automation, the note is left alone (matches the existing "low confidence" tier behavior). [cluster-editor-policy-resolution-walk-up]

The outlier bucket gets its own policy slot for the canonical "send unsorted notes to `inbox/unsorted/`" or "tag them `unsorted`" flow. Setting a `Move` or `Tag` policy on the outlier bucket applies that policy to every note the placement classifier labels as outlier — no walk-up needed because outliers don't have a parent in the cluster sense. `Freeze` on the outlier bucket is a valid choice for "leave outliers in inbox; I'll triage them by hand." [cluster-editor-outlier-policy]

Three policy shapes — `Tag`, `Move`, and `Freeze` — covering the four meaningful combinations of (action × review requirement):

- **Tag(slug, require_review)** — produce an `apply_tag` row that writes the tag to the note's frontmatter (per `suggestions-mode-tag`). When `require_review = false`, the row auto-accepts (subject to the global flag); when `true`, the row stays pending. Idempotent on accept.
- **Move(folder, require_review)** — produce a `move_note` row that moves the file into the target folder (per `suggestions-mode-move`). Same auto-accept / pending semantics. Subject to the existing safety rule that the source must be inside the configured triage scope.
- **Freeze** — match is explicitly ignored. No staging row is produced. Useful for marking a subtree as "this is well-organized already, leave it alone."

[cluster-editor-policy-types]

`require_review` composes with the global `[suggestions.triage].review_required` flag — a row auto-accepts only when *both* `policy.require_review == false` AND `config.review_required == false`. Either set to `true` forces the row to stay pending in the activity-detail Pending filter. The global flag is "force review on every triage match"; the per-node flag is "force review on this specific subtree" — useful for "I trust the saved tree's `research/` placements, but `inbox/projects/*` matches should always pause for me."

Confidence still flows through the matching engine — the descent classifier returns a target node and a confidence — but the **action** taken is driven by the matched node's resolved policy, not by a global threshold. A node's policy can carry an optional `min_confidence` parameter for users who want "auto-move this cluster only when match confidence ≥ 0.85"; that's a per-policy threshold, not a global one. [cluster-editor-policy-require-review]


## Triage execution

When a tree is in `saved as triage` state and the user has any policy set on any node, triage runs against new/modified notes:

- **On save** (default) — when a note inside the configured triage scope (default `inbox/`) *and* within the saved tree's `BuildScope` (per `cluster-build-scope`) is saved, hiker enqueues a `RaptorTriageMatch` task in `core::tasks` (per `task-queue.md`). The worker runs the placement classifier (`cluster-place-beam-descent`) and resolves the matched node's policy. [cluster-editor-triage-on-save]
- **Scheduled rerun** (opt-in) — `[suggestions.triage] scheduled_rerun = "0 3 * * *"` (cron-shape per the existing settings model) re-runs triage over a configurable scope. The schedule fires submissions to `core::tasks` for each affected note, batched at `Low` priority so it doesn't block foreground work. [cluster-editor-triage-scheduled-rerun]
- **Modified-note rerun** (opt-in) — separate from scheduled: when an existing note (already placed or already in inbox) is meaningfully edited (defined as "embedding changed by more than X cosine distance from prior"), triage re-evaluates. Distinct from on-save because most saves don't change embeddings significantly; the cosine guard avoids re-triaging on every keystroke save. [cluster-editor-triage-modified-rerun]

All three pathways submit through `core::tasks`, which routes to whatever worker is configured. The user watches progress in the queue widget; a per-policy task naturally inherits the queue's cancel + audit machinery. [cluster-editor-triage-via-task-queue]

Triage outputs flow through the op log rather than mutating the vault directly. Each match produces one row with `surface = "triage"`, `metadata.tree_id`, `metadata.matched_node_id`, `metadata.confidence`, and an `action` derived from the resolved policy: `auto-move` → `move_note`, `auto-tag` → `apply_tag`, `review` → the same `move_note` / `apply_tag` row marked for explicit user review. Whether the row auto-accepts or waits for the user is gated by `[suggestions.triage].review_required` and the policy type — see `suggestions.md` for the table. The accept path reuses `suggestions-apply-cmd` (the same `move_note(from, to)` and frontmatter-tag-write code that drives one-shot Apply). [cluster-editor-triage-via-staging]


## Batch-review pane (one-shot Apply)

Clicking Apply on a draft tree runs the policy walk, emits one pending op per leaf whose resolved policy is `Tag` or `Move` (per `cluster-editor-apply-action`), and opens the batch-review pane in place of the expanded tree view. The pane is the tree-scoped view of pending ops where `metadata.tree_id = <this tree>`.

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

- **Rows are grouped by `action`.** Move rows together, Tag rows together. Each group collapsible. Row order within a group: by `target_path` (Moves) or `slug` (Tags) so related items sit adjacent.
- **Per-row Accept / Reject.** Same `core::ops::flip_op_status(op_id, Accepted | Rejected)` calls the activity-detail page uses; clicking accepts/rejects exactly that op.
- **Inline edit of the target.** Click the `→ <path>` (or `+ <slug>`) to edit it in place — saves a new `target_path` / tag value on the staging row (re-using the in-flight `staging-amend`-shaped mechanic; if amend isn't wired yet, falls back to "reject this row + propose a new one with the edited target"). Useful for "the cluster name is right but the folder needs to be `research/embeddings-v2/` not `research/embeddings/`."
- **Conflicted rows highlighted.** When a row's state is `conflicted` (per `staging-proposal-state`), the row gets a `⚠` glyph and the reason tooltip; Accept is disabled, Reject still works. Same display rules as the activity-detail page.
- **Accept-all / Reject-all** are batch verbs over the visible (non-conflicted) rows. Confirm dialog when N > 5 for Accept-all; always confirm for Reject-all (rejecting agent/automation work is destructive).
- **Back to tree** returns to the expanded tree view without closing the pending rows — useful if the user wants to adjust a policy and re-run Apply, or just look at the tree structure while reviewing.
- **Auto-close on completion.** When every emitted row is resolved (accepted or rejected), the pane closes, the tree's `state` flips to `applied`, and the expanded tree view reappears with a confirmation toast (`24 changes applied, 0 pending`).
- **Skipped policies surface as a note.** Frozen subtrees and unpolicied leaves don't produce rows; the pane header notes `(7 leaves skipped — no policy assigned)` so the user knows nothing got silently dropped.

The pane is a new editor-pane sub-mode (`cluster-batch-review`) carrying `tree_id` on the buffer state. Same nav-stack discipline as the expanded tree view: entering pushes once, Back-to-tree pops, individual row accept/reject don't push. [cluster-editor-batch-review-pane-mode]


## LLM actions via the task queue

Three LLM-driven actions in this surface route through `core::tasks` rather than direct `core::llm` calls:

- **Initial cluster naming/summarization** during the build pass (already specced as `cluster-summarize-llm` in `clustering.md`; that fan-out routes through the task queue per the queue's "everything except chat" scope).
- **Regeneration** triggered by the user's "Regenerate names" toolbar button or per-node "Regenerate this node" — submits one task per node needing regeneration, at `Normal` priority (user is watching).
- **Auto-name on merge / split** — merging siblings or splitting a cluster produces new clusters that need names; submitted as `Normal` tasks immediately so the new rows aren't stuck with placeholder names.

[cluster-editor-llm-actions-via-task-queue]

User-edited names/summaries are skipped by all three — the `user_edited_*` flag short-circuits the regeneration path. Explicit "Regenerate this node" from a user-edited node confirms with a dialog before clobbering. [cluster-editor-user-edit-provenance]


## Graph view for policy assignment

Third view variant, alongside the sidebar mode and the row-shaped expanded mode. Renders the tree as a node-link graph using the project's committed graph renderer (the egui force-graph widget, per `design.md`'s graph-view bullet) — tuned for "see the whole tree at a glance, assign policies by clicking nodes, watch the colors light up." [cluster-editor-graph-view]

```
┌─ Cluster editor: 2026-05-08 reorg  [Graph view]  ──────────────────┐
│ [Apply] [Save as triage] [Tree view] [Markdown view] [✕]            │
│                                                                      │
│         ●─── Research (24)                                           │
│        ╱│╲                                                           │
│       ● ● ●  Embeddings   Vector DBs   LLM agents                    │
│       │   │ │                                                        │
│       ● ● ●                                                          │
│         ●─── Projects (15)                                           │
│         │                                                            │
│         ●  Dishwasher                                                │
│                                                                      │
│  Legend: ● Move   ● Tag   ● Freeze   ○ no policy   ⏸ require review │
└─────────────────────────────────────────────────────────────────────┘
```

When to use the graph view vs. the row-shaped expanded view: row view is the working surface for textual operations (read summaries, rename clusters, drag-drop into clusters, multi-select stage actions). Graph view is the working surface for *overview and policy assignment* — the spatial layout makes the tree structure legible at a glance, and color-by-policy turns "did I assign moves to everything that needs it" into a visual scan instead of a row-by-row walk.

Behavior: [cluster-editor-graph-view-behavior]

- **Layout — radial tree by default.** Root at center, descendants on concentric rings, sibling spread by angular sweep. Fits a rectangular pane well, scales to 5–6 levels deep without cramping, and reads at a glance ("which subtrees are big, which are dense"). Alternative layouts (`vertical-tree`, `horizontal-tree`, `force-directed`) are selectable from the view menu. Layout choice is part of the per-tree saved view state (`cluster-editor-graph-view-saved-view-state`); persisting one global preference would conflict with users who like one layout for the Evergreen tree and another for a Sapling. The force-directed variant runs the shared force-graph layout — a native ForceAtlas2 implementation with Barnes–Hut repulsion (`widgets/graph-widgets/src/force_layout.rs`) — with outbound-attraction-distribution on so a hub's leaves settle into a ring at a consistent radius around the parent rather than drifting outward, the look users recognize as "force-directed." The layout settles at a scale that renders legibly at the default zoom. [cluster-editor-graph-view-layout]
- **Layout-extensible framework.** Layouts are variants of `LayoutKind` in the graph panel (`app/src/panels/graph.rs`); adding a layout = new variant + position-assignment arm + view-menu row. The renderer applies the chosen layout's positions and stays layout-agnostic. [cluster-editor-graph-view-layout-extensible]
- **Node color encodes the resolved policy.** Distinct colors for `Move` / `Tag` / `Freeze` / `no policy`. A `⏸` glyph overlay marks nodes with `require_review = true`. Inheritance is visualized via softer-shade colors on descendants whose policy walked up to an explicit ancestor — at-a-glance "which subtrees actually have rules versus which are riding inherited ones." [cluster-editor-graph-view-color-by-policy]
- **Node size encodes member count.** Leaf clusters with more members render as larger nodes; root and high-level clusters can be configured to scale logarithmically so the root doesn't dwarf everything. [cluster-editor-graph-view-size-by-members]
- **Node-label text styling.** Each node's text label renders on a rounded white background with a 1px solid `#c8c8c8` outline (~6px horizontal / 2px vertical padding, ~3px corner-radius). Text reads against any policy fill / theme backdrop without losing the node's own color encoding. Wired via the force-graph widget's node-label drawing so the styling is consumer-agnostic — the vault-wide graph view picks up the same label background. [cluster-editor-graph-view-label-style]
- **Summary staleness tint.** Nodes with `summary_membership_churn > 0` render with a soft tint (slight desaturation of the policy color, opacity-style rather than a new hue) so the "this summary may be out of date" signal is visible at a glance without breaking the policy-color encoding. Hovering shows the exact churn count in the tooltip; clicking the policy chip in the popover offers a Regenerate-summary affordance alongside the policy controls. [cluster-editor-graph-view-summary-staleness-tint]
- **Left-click a node selects it** (single-select; replaces any prior selection). Shift+click extends/toggles the multi-select. **Right-click on a cluster (or outlier bucket)** opens the policy editor popover anchored at the pointer — same editor as the row view (`Tag` / `Move` / `Freeze` / `None` + slug/folder + require-review checkbox). Submitting rewrites the node's policy in the tree doc's frontmatter (a `SetFrontmatter` op) and recolors immediately. Left-click is reserved for select (matching the row view and the rest of the app, and the note-preview overlay which keys off selection). [cluster-editor-graph-view-click-to-edit-policy]
- **Multi-select via Shift+left-click** for bulk policy assignment — set a policy on N nodes at once via the right-click popover, applied to each. Same multi-select semantics as the row view's multi-select; the bulk-action toolbar from the row view doesn't apply here (merge/split/etc. are textual operations).
- **Hover a node** — overlay tooltip with the cluster's name, summary, member count, and the resolved policy. Held-hover (>500ms) expands the tooltip to show member-note titles (capped at ~10 with "and N more"). The tooltip anchors near the cursor (mouse-relative, with a 12,12 offset and a viewport clamp so edge-hovers don't slide it off-screen) rather than at the canvas corner, with a `min-width` floor so sparse content doesn't collapse to a 1px line. [cluster-editor-graph-view-hover-detail]
- **Filter chrome** — top-of-pane filter strip lets the user dim everything *except* nodes matching a given policy (e.g. "highlight only Move nodes," "highlight only nodes with `require_review`," "highlight no-policy nodes"). Dimming rather than hiding so the structure stays legible. [cluster-editor-graph-view-policy-filter]
- **Click a leaf** — single left-click both selects the leaf and opens the note as a preview tab in the editor pane (`{ preview: true }`, matching sidebar / activity behavior). Clusters route to the right-click popover instead. Nav-stack discipline unchanged. [cluster-editor-graph-view-leaf-click-opens-note]
- **Hover preview card.** When the view menu's `Show note preview` toggle is on, the in-canvas preview card tracks the hovered node and anchors next to it (cursor-style nudge: drawn down-and-right of the node, flipped to the opposite quadrant if it would clip the canvas, then clamped). Card body is the note's content with the YAML frontmatter block skipped (so the preview opens on real text, not metadata) for leaves; for cluster nodes the card shows the cluster's `name` + `summary`. When a cluster's `summary` is empty the card falls back to whatever leaf body is still cached so the toggle never produces a useless "(no summary)" card on the common case. Card style is a light `#fafafa` background with a `#c8cdd4` 1px border; title wraps inside the card width so long basenames don't overflow. Toggle is per-tree saved view state alongside the other graph-view view-menu choices. [cluster-editor-graph-view-hover-preview-card]
- **No drag-reshape from the graph view.** Drag-drop tree edits (move-note-between-clusters, etc.) stay in the row view where the targets are unambiguous; in a graph view, "drag node A onto node B" is too easy to misclick. The graph view's job is overview + policy assignment; reshape is the row view's job. [cluster-editor-graph-view-no-reshape]
- **Pan / zoom via keybinds.** Default chords land via the keybind registry (`keybind-registry`) — `pointer-drag` for pan, `wheel` for zoom, `pinch` for zoom on touchpads. The defaults map to the registry-canonical chord ids `cluster-editor.graph-pan`, `cluster-editor.graph-zoom`, `cluster-editor.graph-zoom-pinch`. Users rebind via the existing keybind UI; no graph-view-specific chrome for pan/zoom. [cluster-editor-graph-view-pan-zoom-keybinds]
- **Selection visual — outline ring.** A selected node renders with a thin outline ring in the accent color (`var(--accent)`). Multi-select shows the same ring on every selected node; the bulk-action toolbar at the bottom of the pane shows the count (`Selected: 3 nodes`). Minimal styling, matches the rest of the UI. [cluster-editor-graph-view-selection-outline]

### View menu

A single eye-icon button lives on the **pane's pinned toolbar**, always visible — same eye icon the editor's view-options menu uses (`editor.md` → `## View options menu`). Clicking it opens a unified "View options" popover that carries: (a) a **View as** radio (Tree / Graph / Markdown) that replaces the prior 3-button toggle strip in the toolbar; (b) in tree mode, **Expand all** / **Collapse all** verbs that mutate the pane-local `expanded: Set<NodeId>` (per `cluster-editor-row-primitive`) so the whole tree opens or closes in one click; (c) in graph mode, the graph-specific switches (leaves visibility / layout / show outliers / fit / reset / note-preview toggle). The menu refreshes in place when the view-mode radio changes; switching the View-as mode swaps just the pane body and leaves the toolbar intact. In the egui immediate-mode UI the menu is rebuilt each frame, so there's no mounted-popover lifecycle to manage — the eye button is a stable anchor and the menu's items always reflect the current view mode. Consolidating both into one menu keeps the pinned toolbar tidy and gives the cluster pane a single "view" affordance instead of two separate ones. Choices: [cluster-editor-graph-view-view-menu]

- **Leaf visibility.** Three modes: `Hide leaves` (only cluster nodes render), `Auto (LOD)` (leaves hidden when zoomed out below a threshold, fade in as the user zooms in — default), `Show all leaves` (every leaf node is always present in the canvas). [cluster-editor-graph-view-leaf-visibility]
- **Layout.** Radio: `Radial (default)` / `Vertical tree` / `Horizontal tree` / `Force-directed`. Switching re-runs the chosen layout and animates nodes to their new positions. [cluster-editor-graph-view-layout]
- **Show outliers.** Bool toggle, default on. When off, the disconnected outlier node (and its leaves, when leaves are visible) is hidden from the canvas. Distinct from the build-time `include_outliers` option (which controls whether outliers are *generated* in the first place — per `cluster-review-tab-config-section`); the view-menu toggle just hides them in the current view. [cluster-editor-graph-view-show-outliers]
- **Reset view** / **Fit to view.** Menu actions that re-center and rescale the canvas. Reset returns to the layout's default zoom + pan; Fit-to-view scales the canvas so every visible (non-hidden by other toggles) node fits in the viewport. [cluster-editor-graph-view-reset-fit]

View-menu choices are per-tree saved view state (persisted in the tree doc's frontmatter under `hiker.view_state`, or in `vault/.hiker/config.toml` keyed by tree id — implementation choice). Pan/zoom positions also persist there so the user comes back to where they left off. [cluster-editor-graph-view-saved-view-state]

### Outlier rendering in the graph

The outlier bucket renders as a **separate disconnected node**, floating off to the side of the main tree (default: lower-right corner of the canvas). It carries the same "Outliers (N)" label as the row view's virtual node. Its policy chip works identically to any other node's — clicking it opens the policy editor; setting a `Move` or `Tag` policy on it applies that policy to every member note classified as outlier by the placement classifier. [cluster-editor-graph-view-outlier-disconnected]

Setting an outlier policy is the canonical pattern for "send unsorted notes to `inbox/unsorted/`" or "tag them `unsorted`" — instead of leaving outliers to rot in the inbox, the user makes the policy explicit. [cluster-editor-outlier-policy]

The build-time `include_outliers = false` option (per `cluster-review-tab-config-section`) suppresses the outlier bucket entirely — notes that would have been outliers get force-routed into their nearest cluster instead. The view-menu's `Show outliers` toggle only hides the rendered node, not the underlying data; both can be set independently.

### View switching

A toggle in the toolbar of the expanded pane flips between tree (row) view, graph view, and markdown view — three peer rendering modes of the same tree. The user's current selection survives the switch (so multi-selecting in row view and toggling to graph view shows those nodes highlighted in the graph). [cluster-editor-graph-view-toggle]

### Renderer integration

The graph view consumes the shared graph renderer per `design.md`'s renderer pattern. Concrete plumbing:

- `core::trees` produces a `ClusterTreeGraph { nodes, edges }` DTO via a dedicated query (`Trees::tree_as_graph(tree_id)`). Nodes carry id, name, kind, member_count, resolved_policy, explicit_policy_flag, depth. Edges are parent → child.
- `app/src/panels/cluster_review/graph.rs` drives the cluster-tree graph view, reusing the shared egui force-graph widget (`app/src/widgets/force_graph.rs`) — the same renderer the vault-wide graph view uses; node-color + node-size and policy-filter overlays are app-state shape concerns that live in the panel module per `design.md`.
- Layout runs on a background worker (`force_layout::LayoutWorker`) so opening the graph view doesn't block the UI; being a native app, there's no separate bundle to load. Same pattern as the vault graph view. [cluster-editor-graph-view-lazy-load]
- Re-renders on tree frontmatter changes (policy edits, name edits, structure edits via the row view, triage producing new rows). Implementation can either fully re-mount or compute a diff and patch — the renderer adapter's capability flag for in-place updates determines which.

The cluster-tree graph view is a *separate surface* from the vault-wide graph view (different data, different consumer module). They share the renderer primitive, not the data plumbing. [cluster-editor-graph-view-not-vault-graph]


## Reusable row primitive

The row component (chevron + icon + name + summary preview + members count + policy chip + selection state + multi-level hierarchy + multi-select + right-click context menu) is shared Rust rendering in the cluster panel (`app/src/panels/cluster_review/`), used by both the sidebar mode and the center pane; the two surfaces can diverge in spacing / typography without forking the rendering. Per-surface state (`expanded` / `selection` sets) is owned by the caller and survives refreshes triggered by queue events. Future hierarchical surfaces (e.g. saved-collections-of-collections, multi-axis cluster trees) plug into the same primitive. [cluster-editor-row-primitive]

Trails and the vault-wide graph/map view (per `design.md`) deliberately do *not* reuse this row primitive — trails are sequential not hierarchical, and the vault graph's edge-rendering needs are fundamentally different from a tree's. The reuse is at the row-primitive level only.

The cluster editor *does* reuse the **graph renderer** (the egui force-graph widget per `design.md`'s graph-view bullet) for its own tree-shaped graph view (next section). The exclusion is about data models and surfaces, not about the underlying rendering primitive — the renderer pattern in `design.md` exists precisely so multiple surfaces can ride the same widget with their own data. [cluster-editor-renderer-reuse]


## Out of scope

- **Trails and graph/map view.** Different surfaces, different primitives. Trails are sequential; graph is non-hierarchical. The cluster editor explicitly does not subsume them.
- **Real-time collaborative editing of a cluster tree.** Single-user only.
- **Sharing trees across vaults.** Import-tree-from-file works for cross-vault transfer of a tree shape, but there's no automatic sync.
- **Multi-axis trees** (semantic + temporal + entity layered). Single tree shape per draft; multi-axis is `design.md`'s deferred slug.
- **In-place re-clustering of *already-placed* notes** (i.e., re-running the build over notes that already have folder placements and reconciling the diff). The current model is "build a fresh tree, manually edit, apply, then triage handles ongoing." Not a continuous reconcile.


## Deferred

- **Collaborative review** — multiple cursors, comment threads on cluster nodes. Not on the v1 roadmap.
- **Cluster diff view** — render the diff between two cluster trees (e.g. a fresh build vs. the saved triage tree) so the user can see what changed. Useful for "did the structure shift since I saved?" but speculative until real use shows the need. [cluster-editor-tree-diff-view]
- **Tree export to non-hiker formats** (org-mode, opml, etc.). The markdown view is the in-house export shape; converters land if a real workflow asks.
- **Per-policy `min_confidence` UI.** The data model supports it; the policy editor's UI for setting per-policy thresholds is deferred until users find the simple "policy fires on any match" too aggressive.
- **Branching saved-trees.** One saved triage tree per vault. Multiple saved trees with selection at triage time is the natural extension; deferred until users need it.


## Forward refs

- `core::trees` — owner of the per-tree `.md` files under `vault/.hiker/trees/`; (de)serializes the `hiker.nodes` frontmatter and commits edits through the op-log. Same module-discipline pattern as `core::trails` — plain Rust types out, the on-disk YAML shape never leaks.
- `core::cluster_editor` — the implementation home of the UI-facing editor operations; sibling to `core::cluster` / `core::suggest` / `core::trees`. Consumes `core::cluster::build_tree`, applies edits through `core::trees`, emits one-off pending ops via `core::ops` for the multi-select Stage move/tag verbs.
- `core::tasks` — every LLM-driven action in this surface (initial naming, regeneration, merge/split renaming) submits there.
- `editor.md` — sidebar mode switcher lives in the editor's domain; this spec defines the cluster-trees mode body, the editor.md mode-switcher entry is owned there.
- `suggestions.md` — the apply mechanic (`suggestions-apply-cmd`) is reused as-is for one-shot Apply and for the accept path of triage / one-off staging rows produced by this surface. The markdown body is a read-only render of the tree's frontmatter, regenerated on write; the frontmatter is the persisted structure.
- `settings.md` / `op-log.md` — pending ops carry their action in `OpKind` (`Rename` for moves, `SetFrontmatter` for tags); `surface = "triage"` and `surface = "cluster-editor"` in op metadata are produced by this doc's flows.
- `keybind-registry` — chord ids reserved: `cluster-editor.toggle-expand`, `cluster-editor.merge-siblings`, `cluster-editor.merge-up`, `cluster-editor.split`, `cluster-editor.regenerate`. Chords TBD; land when each action is wired.
