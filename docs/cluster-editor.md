# Cluster editor

Interactive surface for viewing, manually editing, and configuring automation on a cluster tree (per `clustering.md`'s `ClusterTree`). Replaces the markdown-proposal flow as the primary review surface for one-shot suggestions; the markdown form (`suggestions-proposal-md`) sticks around as a serialization + audit-log shape, not as the user's working surface.

The headline decisions:

- **Lives in the left sidebar as a switchable mode, with an Expand button to flip into a full center pane for graphical work.** The sidebar gains a mode switcher (Files / Cluster trees / future Trails) just above the existing `+ New note` row; switching to Cluster trees mode swaps the sidebar body to the cluster editor, hiding the filetree-only chrome (+New, `…` actions, Trash bin). An Expand button on the cluster-trees header sends the editor to the center pane, replacing CodeMirror with a graphical tree view tuned for drag-drop reshaping; click a leaf in expanded mode to open the note in the editor, the existing back-navigation returns to the expanded tree. Mirrors `chat-panel-expand-to-editor`. [cluster-editor-sidebar-mode, cluster-editor-pane-expand]
- **Multiple trees can be open at once.** The sidebar's Cluster trees body shows a list of open trees — typically the saved triage tree (always present once saved) plus zero-or-more ephemeral trees from one-shot runs. Each tree expands inline to show its node hierarchy. Header on each tree row shows name + state pill (`draft` / `applied` / `saved as triage`). Switching between trees is just expanding a different one — no modal context-swap. [cluster-editor-multiple-trees-open]
- **Manual reshaping at every level.** Move notes between clusters, merge siblings, merge children up into the parent, split a cluster (re-run HDBSCAN against just its members), rename a cluster, edit an LLM-generated summary, drop a cluster (members fall to outliers), promote outliers into a cluster. Drag-drop where it's natural (move-note); buttons on the multi-select toolbar where it's not (merge / split). Every edit is undoable until apply. [cluster-editor-node-operations]
- **Automation policies attach at any level.** A policy (`Tag` / `Move` / `Freeze`) can be set on a leaf, a mid-tree cluster, or the root. `Tag` and `Move` each carry a `require_review` toggle; `Freeze` means "do nothing for matches in this subtree." Resolution at triage time walks up from the affected note to the nearest ancestor with an explicit policy; no policy anywhere up to the root means the note is left alone. A saved tree is a *policy program*, not a single classifier with a knob. [cluster-editor-policy-any-level, cluster-editor-policy-resolution-walk-up]
- **LLM-generated names and summaries are user-editable, with provenance.** Click-to-edit on any cluster's name or summary inside the editor. Edited values get `user_edited: true` stamped on the node so a re-run of the cluster build (or a regen of summaries via `core::tasks`) doesn't clobber them. Re-generating an edited node's summary requires explicit "Regenerate" action — implicit overwrite is forbidden. [cluster-editor-edit-name-summary, cluster-editor-user-edit-provenance]
- **Auto-save in-progress edits to `trees.db`.** Every manual edit writes to `vault/.hiker/trees.db` (the structured form, owned by `core::trees`). The markdown view is rendered on demand from the same rows — no parallel on-disk file. Closing and reopening the editor resumes where you left off; the draft survives app restarts. Discard-draft is an explicit button. [cluster-editor-draft-persistence]
- **Building a tree (or reclustering a subtree) happens in a clustering review tab, not a modal.** "Suggest reorganization" and "Recluster subtree…" both open a `cluster-review` app-page tab. The tab carries three stacked sections: a configuration panel (lifecycle / scope / method / params), a Run button that performs the structural clustering pass without any LLM calls, and a review panel showing the resulting tree shape (member counts, sample titles, placeholder names). The user can adjust params and re-run as many times as wanted before committing. A "Confirm and name" action submits the LLM naming/summarization pass through `core::tasks` and persists the tree as a `draft` in `trees.db`; only then does the tab flip to the existing `cluster-batch-review` kind for editing and review. Nothing about the structural result is written to disk until Confirm. [cluster-review-tab]
- **Apply and Save-as-triage are separate actions.** Apply walks the current tree state and emits one `staging.db` row per leaf whose resolved policy is `Tag` or `Move` (`surface = "cluster-editor"`, `metadata.tree_id`); the user then bulk-reviews via the tree-scoped batch-review pane (see below). Save-as-triage persists the tree with its policies as the active triage classifier (replaces any prior saved tree); does *not* enqueue staging rows itself — triage emits them as matches fire over time. A user can both apply and save-as-triage from the same tree if both make sense. [cluster-editor-apply-action, cluster-editor-save-as-triage]
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
[chevron] [icon] <name>                          [members] [policy chip] [drag handle]
                 <summary preview, one-line>
```

- **Icon** identifies the row type — folder-style glyph for a cluster, note glyph for a leaf, distinct outlier glyph for the outlier bucket.
- **Name** is the cluster's name (LLM-generated or user-edited) or the note's basename. Click-to-edit on cluster names; leaf names are read-only (rename a note via the regular file-tree affordances).
- **Summary preview** is the cluster's LLM-generated summary truncated to one line; click expands to a full-text editable area below the row, click the area's name in turn to edit. Leaves don't show a summary. A `↻ N` badge appears on the right of the summary preview when `summary_membership_churn > 0`, displaying the count of membership changes since the summary was generated (per `cluster-summary-staleness-counter`). Clicking the badge offers a quick Regenerate action for that node. [cluster-editor-summary-staleness-badge]
- **Members count** is the number of notes in the cluster's full subtree (including all nested children's leaves). Empty for leaves.
- **Policy chip** shows the resolved-or-explicit policy for this node — `tag: <slug>` / `move: <path>` / `freeze` / blank (no policy, walks up to nearest ancestor). Chips for policies with `require_review = true` get a small badge (`tag: research ⏸` / `move: research/ ⏸`) so the review-required state is visible without opening the editor. Clicking the chip opens the policy editor for that node.
- **Drag handle** lets the user drag the row onto another cluster (move) or onto another sibling (reorder, no semantic change).

Outliers render as a special virtual node at the bottom of every tree level — labeled "Outliers (N)" with a distinct icon. Drag a leaf into a cluster to promote it; drag a leaf out of any cluster onto Outliers to demote it. The outlier bucket is the sink for "no good cluster fit"; users can manually fish notes out of it during reshape. [cluster-editor-outlier-virtual-node]


## Expanded mode (center pane)

Clicking the Expand button on a tree row sends that tree to the center pane, replacing CodeMirror. This is the surface tuned for heavy graphical reshaping — wider rows, larger drag targets, multi-pane "before/after" preview, more screen real estate for visualizing N-level trees.

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
- **Markdown view toggle** in the toolbar flips the pane between the graphical tree and a CodeMirror buffer rendering the equivalent markdown shape (per `suggestions-proposal-md`). The markdown is rendered on demand from `trees.db` rows; the view is editable, and saving parses back into the structured form (writing updated `cluster_nodes` rows and an appended `cluster_tree_history` entry). Acts as the escape hatch for users who prefer text editing or want to bulk-rewrite via vim-style motions. [cluster-editor-markdown-view-toggle]
- **Apply / Save as triage / Discard draft** are toolbar buttons. Apply emits staging rows for every leaf with a `Tag` or `Move` policy and opens the batch-review pane (see below); Save as triage persists the tree as the triage classifier; Discard draft confirms then drops the tree from `trees.db`.
- **Regenerate names** runs the cluster naming/summarization prompt for any cluster whose name/summary is *not* user-edited (per the `user_edited` flag). Each regeneration is one task in `core::tasks` (per `task-queue.md`); the user can watch progress in the queue widget. Regenerating a user-edited node requires per-node explicit "Regenerate this node" from its row menu. [cluster-editor-regenerate-via-task-queue]


## Tree storage: `trees.db`

Cluster trees live in `vault/.hiker/trees.db` — a SQLite database owned by `core::trees`, sibling to `core::store` (`index.db`), `core::changes` (`changes.db`), and `core::staging` (`staging.db`). Same module-discipline pattern: pure Rust types out of the module, no SQL or rusqlite types leak past the boundary, `pragma user_version` for schema versioning, fail-loud on mismatch (per `store-version-fail-loud`), pre-1.0 policy of "delete the file on schema bump" rather than migration code. [trees-db, trees-module-discipline]

Why a dedicated db rather than rows in `staging.db`: trees are long-lived organizational infrastructure (drafts edited over days, saved triage tree persisted indefinitely), staging is short-lived pending writes (14-day GC). Different lifecycle, different shape (nested nodes vs flat proposals). Keeping them separate avoids carve-outs in either schema. The two databases compose at the *proposal* layer: triage matches produced by a saved tree emit rows into `staging.db` with `surface = "triage"` (per `suggestions.md`), but the tree definition itself stays in `trees.db`.

Three tables:

```sql
-- one row per open or saved tree
CREATE TABLE cluster_trees (
  id              TEXT PRIMARY KEY,           -- ULID
  name            TEXT NOT NULL,              -- user-visible label
  source          TEXT NOT NULL,              -- 'one-shot' | 'saved-triage'
  state           TEXT NOT NULL,              -- 'draft' | 'applied' | 'saved-as-triage'
  scope           TEXT NOT NULL,              -- JSON of BuildScope (per clustering.md)
  created_at_ms   INTEGER NOT NULL,
  vault_snapshot  TEXT                        -- vault rev at build time; advisory
);

-- one row per node within a tree
CREATE TABLE cluster_nodes (
  tree_id              TEXT NOT NULL REFERENCES cluster_trees(id) ON DELETE CASCADE,
  node_id              TEXT NOT NULL,         -- stable within the tree
  parent_id            TEXT,                  -- NULL for root
  kind                 TEXT NOT NULL,         -- 'cluster' | 'leaf' | 'outlier-bucket'
  note_id              TEXT,                  -- present on leaves
  name                 TEXT NOT NULL,         -- user-editable (clusters only)
  summary              TEXT NOT NULL,         -- user-editable (clusters only)
  user_edited_name     INTEGER NOT NULL DEFAULT 0,
  user_edited_summary  INTEGER NOT NULL DEFAULT 0,
  policy               TEXT,                  -- JSON: NodePolicy or NULL
  centroid             BLOB,                  -- packed f32; present on clusters
  confidence           REAL,                  -- from build pass
  summary_membership_churn INTEGER NOT NULL DEFAULT 0,  -- per cluster-summary-staleness-counter
  PRIMARY KEY (tree_id, node_id)
);

-- append-only edit log for undo/audit
CREATE TABLE cluster_tree_history (
  tree_id   TEXT NOT NULL REFERENCES cluster_trees(id) ON DELETE CASCADE,
  seq       INTEGER NOT NULL,                 -- monotonic per tree
  ts_ms     INTEGER NOT NULL,
  op        TEXT NOT NULL,                    -- 'move' | 'merge' | 'split' | 'rename' | 'set-policy' | ...
  args      TEXT NOT NULL,                    -- JSON
  undo_args TEXT NOT NULL,                    -- JSON
  PRIMARY KEY (tree_id, seq)
);
```

Indexes: `cluster_nodes(tree_id, parent_id)` for child lookups, `cluster_nodes(tree_id, note_id)` for "which tree node holds this note" reverse lookups, `cluster_tree_history(tree_id, seq DESC)` for the undo stack. WAL mode + `synchronous=NORMAL`, mirroring `store-wal-mode`. [trees-db-schema]

The build pass writes the initial set of `cluster_nodes` rows when a new tree is created — there is no separate "build snapshot vs editable draft" distinction; every node is editable from the moment it lands. Edits update node rows in place and append to `cluster_tree_history`; the markdown view is rendered on demand from `cluster_nodes` + `cluster_tree_history` and is not persisted. [cluster-editor-draft-persistence, cluster-editor-edit-history]

Discard-draft on a tree whose `source = 'one-shot'` deletes the row from `cluster_trees` (cascading to nodes + history). Discard on a saved-triage tree is "Unsave as triage" — flips `state` back to `draft` and (optionally) deletes; the user is asked which. [cluster-editor-discard-draft]

The in-memory editable shape stays the same — it just hydrates from `trees.db` rows:

```rust
struct EditableNode {
    id: NodeId,                       // stable across edits within this tree's lifetime
    parent: Option<NodeId>,
    kind: NodeKind,                   // Cluster | Leaf | OutlierBucket
    members: Vec<NodeId>,             // children for clusters; empty for leaves
    note_ref: Option<NoteId>,         // present on leaves
    name: String,                     // user-editable (clusters only)
    summary: String,                  // user-editable (clusters only)
    user_edited_name: bool,
    user_edited_summary: bool,
    policy: Option<NodePolicy>,
    centroid: Option<Vec<f32>>,       // present on clusters; recomputed on edits affecting members
    confidence: f32,                  // from build pass; preserved through edits
}

enum NodePolicy {
    Tag  { slug: String,      require_review: bool },
    Move { folder: VaultRel,  require_review: bool },
    Freeze,                                        // never propose changes for matches under this node
}

enum NodeKind { Cluster, Leaf, OutlierBucket }
```

[cluster-editor-tree-shape]

The `cluster_tree_history` table records every edit as `{ seq, ts_ms, op, args, undo_args }` so an in-pane Undo / Redo works during the session, and a full history is recoverable for diagnostics. [cluster-editor-undo-redo]


## Clustering review tab

Building a new tree and reclustering a subtree both go through a `cluster-review` app-page tab. The tab is the configuration surface, the runner, and the structural-result reviewer; only once the user confirms does the LLM pass fire and the tree land in `trees.db`. Entry points: the sidebar's `+ Suggest reorganization` / `+` button (`cluster-editor-new-tree-action`) opens a fresh tab; the row-menu's "Recluster subtree…" entry on a cluster row (`cluster-editor-recluster-subtree`) opens a tab pre-bound to that `(tree_id, node_id)`; the mode menu's "Rebuild" entry on an Evergreen tree opens a tab prefilled with the tree's saved scope/method/params. [cluster-review-tab-from-new-tree-action, cluster-review-tab-from-recluster-action, cluster-review-tab-rebuild-prefill]

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
- **Source types** — checkbox group for the file-extension filter that applies after scope: Markdown (.md) and Plain text (.txt). Default both checked. At least one must be selected before Run; an empty selection is flagged with an inline toast rather than firing the IPC with an empty input set. The selection rides through to the persisted `cluster_trees.scope` so the triage classifier honors it on subsequent saves (a Markdown-only saved tree no-ops on `.txt` note saves). Visible on every purpose including recluster — a recluster of a mixed-type subtree may want to narrow to one type. Per `cluster-build-scope-source-types`.
- **Method** — Cluster (default) / FromFolders radio. Per `cluster-build-method`. Forced to `Cluster` in the recluster case, matching the surrounding-tree-method-agnostic posture of `cluster-build-from-folders-uniform-output`.
- **Include outliers** — top-level toggle (default `on`). Same semantics as the old modal.
- **Carry policies down** — recluster-only checkbox (default `off`). When set, the selected node's resolved policy is copied as explicit policy onto every newly-built direct child. Per `cluster-editor-recluster-subtree-policy-loss`.
- **Advanced disclosure** — default-collapsed `▶`/`▼`. Contents reflow based on method *and* algorithm. `Cluster`: algorithm picker (HDBSCAN default / Leiden / Hybrid / GMM-falls-back); when HDBSCAN/Hybrid/GMM is selected, the form shows `min_cluster_size` (defaulted to half the current value in the recluster case, matching Split's heuristic) + `min_samples`; when Leiden is selected, the form swaps in `k_nearest`, `edge_weight_floor`, `iterations`, and `min_cluster_size` (Leiden-flavored). Common tunables shown regardless of algorithm: `min_clusters_to_recurse`, `summary_confidence_threshold`, and a **Disable recursion** checkbox that short-circuits the recursive merge loop so the build produces a single-level tree (`cluster-review-tab-disable-recursion`). `FromFolders`: outlier threshold (when `Include outliers` is on). `summarize` is *not* surfaced here — it's not a knob in this tab, because the structural pass runs without an LLM and Confirm-and-name always uses the bundled `LlmSummarizer`. Defaults match `clustering.md`. [cluster-review-tab-advanced-disclosure]

### Recursion disable toggle

A dedicated **Disable recursion** checkbox lives in the Advanced disclosure (when `Method == Cluster`). When checked, `core::cluster::build_cluster_tree` returns immediately after the level-0 pass — no recursive merging of cluster summaries into higher-level clusters. The result is a single-level tree of leaf clusters plus the outlier bucket. Useful when (a) the user wants a flat reorganization proposal without higher-level groupings, or (b) Leiden's leaf-level communities are already at the right granularity and a second pass would just merge them back together. Wired through `ClusterParams.disable_recursion: bool` (default `false`); short-circuits inside the recursion loop rather than abusing `min_clusters_to_recurse` as a sentinel — the persisted `cluster_trees.method` JSON carries the explicit field, so the toggle's state is recoverable from a saved tree. [cluster-review-tab-disable-recursion]

### Run clustering

The `Run clustering` button runs the structural-only pass: HDBSCAN partitioning + recursive build per `cluster-build-recursive`, but with `SummarizeMode::None` forced for every level so no LLM calls are made. The result is a `BuiltClusterTree` whose clusters have placeholder names (default: `"Cluster N"` numbered in member-count-descending order) and empty summaries. The result lives in the tab's in-memory state — `trees.db` is not touched. Progress (cluster count, level depth, outlier count) shows inline; the button shows a spinner during the pass. Subsequent Run clicks discard the prior result and run again with the current form values. [cluster-review-tab-run-clustering, cluster-review-tab-iterate]

The build pipeline gains a `summarize: SummarizeMode::None` short-circuit path that skips the `Summarizer` call entirely (today `LlmSummarizer` is built unconditionally per `cluster-summarize-llm`; the new path makes the summarizer optional at build-time). This is the only `core::cluster` change required — `build_tree` already takes `ClusterParams.summarize` as input; today every caller sets it to `Llm` and we error out if the LLM is disabled. [cluster-review-tab-structural-pass-no-llm]

### Result panel

Renders the in-memory `BuiltClusterTree` as a collapsible hierarchical preview. Reuses the row primitive from `cluster-editor-row-primitive` (no policy chip — policies don't exist on un-persisted trees; no drag-handle — structural results are immutable from this surface). Per-cluster row shows: placeholder name (`"Cluster 1"`, `"Cluster 2"`, …), member count, member-titles sample (first 3, "… and N more" beyond), centroid radius / confidence indicators. Outliers render as a sibling row with the distinct outlier glyph. Leaves are click-to-open same as the sidebar form. [cluster-review-tab-structure-preview]

The placeholder names are user-editable inline before Confirm — clicking a placeholder opens the same inline-edit affordance as `cluster-editor-edit-name-summary`. User-renamed clusters stamp `user_edited_name = 1` so the LLM naming pass at Confirm time skips them. Summaries cannot be edited here (no summary to edit yet). [cluster-review-tab-rename-before-llm]

### Confirm and name

The `Confirm and name →` button (disabled until a Run has produced a result) does, in order:

1. Persists the in-memory `BuiltClusterTree` to `trees.db` as a new tree row (state `draft`, source per Sapling/Evergreen lifecycle), inserting one `cluster_nodes` row per node. User-renamed clusters carry their names with `user_edited_name = 1`.
2. Submits one `TaskKind::RaptorSummarize { tree_id, cluster_node_id, level }` task per cluster (skipping user-renamed ones) at `Priority::Normal`. Same queue path as `cluster-editor-regenerate-via-task-queue`.
3. Flips the tab's kind from `cluster-review` to `cluster-batch-review` for the new tree, with sub-mode `cluster-tree`. The user lands inside the existing cluster editor pane (`cluster-editor-pane-mode`) on the tree, where summaries fill in as the LLM tasks complete (the pane already re-renders on `cluster_nodes` row changes). The sidebar's Cluster trees mode picks up the new tree on its next refresh.

If `[llm].enabled = false`, the Confirm button is disabled with a tooltip directing the user to settings. The structural result remains intact in the tab — the user can re-enable LLM and Confirm without re-running clustering. [cluster-review-tab-confirm-and-name, cluster-review-tab-transition-to-pane]

A second trailing button, **`Confirm (no naming)`**, sits beside Confirm-and-name (new-tree / rebuild only — the recluster path's naming submission lives in a separate command and isn't routed through this skip flag yet). It does step 1 with `submit_naming = false` so the tree persists with the placeholder names (`"Cluster 1"`, `"Cluster 2"`, …) intact — step 2 is skipped entirely; step 3 still flips the tab to the cluster-pane. The user can fill in LLM names later via the cluster pane's existing "Regenerate names" toolbar button. This button is **not** gated on `[llm].enabled` — its whole point is to avoid the LLM call — so it stays usable even when LLM is disabled. The toast names the deferral honestly ("Tree persisted with placeholder names — run 'Regenerate names' later to LLM-name clusters") so the user isn't left wondering why the cluster nodes are still labeled `"Cluster N"`. [cluster-review-tab-confirm-skip-naming]

For the recluster-subtree case, step 1 is "replace the selected node's descendants with the new subtree" (per `cluster-editor-recluster-subtree`) instead of "insert a new tree row"; step 3 returns the user to the existing tree's cluster editor pane. The skip-naming button isn't offered on this path.

For the rebuild case, step 1 is "write the new tree alongside the existing one"; the old tree is left intact for the user to discard manually (matches the existing `cluster-build-rebuild` posture).

### Discard

The `Discard` button (or closing the tab) drops the in-memory result and the form state. If a result exists, closes are gated by a `confirm` ("Discard the clustering result? You'll need to re-run to get it back.") — closing the tab with no result skips the prompt. Nothing in `trees.db` is touched. [cluster-review-tab-discard]

### One review tab per target

Opening "Suggest reorganization" with a `cluster-review` tab already open for the new-tree case activates the existing tab rather than spawning a duplicate (same shape as Properties' one-tab-per-path rule). Recluster tabs key on `(tree_id, node_id)`; rebuild tabs key on `tree_id`. The form state and any in-flight result are preserved when re-activating. [cluster-review-tab-deduplication]


## Operations

All operations are local edits to the tree's `cluster_nodes` rows + an appended `cluster_tree_history` row. None of them mutate the vault — only Apply or a staging-row accept does that.

- **Move note between clusters.** Drag a leaf onto a different cluster row (or use the row menu's "Move to…"). Updates the leaf's `parent`. Centroids of source and target clusters are recomputed. [cluster-editor-move-note-between-clusters]
- **Merge sibling clusters.** Multi-select 2+ clusters at the same level → toolbar "Merge siblings" → creates one cluster whose members are the union, name and summary are auto-regenerated (queues a task in `core::tasks` for naming) unless the user names it explicitly first. [cluster-editor-merge-siblings]
- **Merge children up to parent.** Select the parent → "Merge up" → flattens that subtree by one level. Children's leaves become direct members of the parent; children's clusters disappear. The parent's name/summary is unchanged unless explicitly regenerated. [cluster-editor-merge-children-up]
- **Split a cluster.** Select one cluster → "Split" → re-runs HDBSCAN against just its current members with a tighter `min_cluster_size` (default `max(2, current_min / 2)`). Produces a new layer of children. The user can keep the split or undo. Splitting a cluster preserves the parent's name; the new children get LLM names via the task queue. [cluster-editor-split-cluster]
- **Recluster subtree.** Select one cluster → row-menu "Recluster subtree…" → opens a `cluster-review` tab bound to that `(tree_id, node_id)`. Same flow as new-tree: configure → run → review → confirm. On Confirm, the structural result **replaces the subtree in place** — every descendant cluster node is deleted, the freshly-built nodes are inserted under the selected cluster, and the leaves re-parent onto their new positions. The selected cluster's own row (id, name, summary, user-edit flags, policy) is preserved; only its descendants change. Differs from Split: Split is one-level and produces a single new layer of children; Recluster subtree is recursive and rebuilds every level beneath the selected node. The form's default `min_cluster_size` is half the surrounding tree's value (same heuristic as Split). Works on `Cluster`-method and `FromFolders`-method trees alike — the rebuilt subtree is always `Cluster`-shaped, matching `cluster-build-from-folders-uniform-output`. [cluster-editor-recluster-subtree]
- **Rename cluster** / **Edit summary.** Click name or summary → inline editor → save. Stamps `user_edited_*: true`. [cluster-editor-edit-name-summary]
- **Drop cluster.** Toolbar action on a selected cluster: drops the cluster, its children's leaves all fall to the nearest outlier bucket. Useful for "this cluster is junk; throw the notes back to inbox-via-outliers." [cluster-editor-drop-cluster]
- **Promote outliers / move out.** Drag a row in/out of the outlier bucket. Same machinery as move-note. [cluster-editor-promote-outlier]
- **Set / clear policy.** Click the policy chip on any node → policy editor (radio: `Tag` / `Move` / `Freeze` / `None` + the action's parameter for `Tag` (slug) or `Move` (folder) + a "Require review for new matches" checkbox available on `Tag` and `Move`). Setting a policy on an ancestor automatically becomes the resolved policy for descendants without their own. [cluster-editor-set-policy]
- **Stage move / Stage tag (one-off, multi-select).** With N leaf rows selected, the bulk-action toolbar exposes "Stage move to…" and "Stage tag with…". Each verb writes N rows directly into `staging.db` (one per selected leaf) with `surface = "cluster-editor"`, `action = "move_note"` or `"apply_tag"`, and `metadata.tree_id` for traceability. No node policies are mutated. Distinct from setting an `auto-move` / `auto-tag` policy and clicking Apply: the policy path is the saved-tree mechanism that fires on future matches; "Stage move/tag" is a one-shot batch against exactly the selected leaves. Useful when the user wants to act on an arbitrary subset of a tree without committing to a forever-rule. The proposals flow through the standard staging surfaces (activity-detail Pending filter, tree row indicators, editor toolbar pill) — the cluster editor doesn't gain a private review queue. [cluster-editor-multi-select-stage-move, cluster-editor-multi-select-stage-tag]


### Recluster subtree: policy and placement semantics

Two consequences of reclustering a subtree are load-bearing enough to call out:

- **Per-node policies on the replaced descendants are lost.** Reclustering deletes every descendant cluster row, including any `Tag` / `Move` / `Freeze` policy set on those rows. Descendants that had no explicit policy were already inheriting from an ancestor — they keep inheriting (the policy lives on the surviving ancestor, not on the dropped descendant). To soften the loss, the clustering review tab's recluster form offers a "Carry policies down from selected node" checkbox: when checked, the selected node's resolved policy (its own or the nearest ancestor's) is *copied* onto every newly-built direct child as an explicit policy. Default-off; the spec's intent is that reclustering is destructive and the user opts in to carrying rules forward. The undo history (`cluster_tree_history` row of op `recluster-subtree`) snapshots the full prior subtree including its policies, so an undo restores them exactly. [cluster-editor-recluster-subtree-policy-loss]
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

Triage outputs flow through `staging.db` rather than mutating the vault directly. Each match produces one row with `surface = "triage"`, `metadata.tree_id`, `metadata.matched_node_id`, `metadata.confidence`, and an `action` derived from the resolved policy: `auto-move` → `move_note`, `auto-tag` → `apply_tag`, `review` → the same `move_note` / `apply_tag` row marked for explicit user review. Whether the row auto-accepts or waits for the user is gated by `[suggestions.triage].review_required` and the policy type — see `suggestions.md` for the table. The accept path reuses `suggestions-apply-cmd` (the same `move_note(from, to)` and frontmatter-tag-write code that drives one-shot Apply). [cluster-editor-triage-via-staging]


## Batch-review pane (one-shot Apply)

Clicking Apply on a draft tree runs the policy walk, emits one `staging.db` row per leaf whose resolved policy is `Tag` or `Move` (per `cluster-editor-apply-action`), and opens the batch-review pane in place of the expanded tree view. The pane is the tree-scoped view of `staging.db` rows where `metadata.tree_id = <this tree>`.

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
- **Per-row Accept / Reject.** Same `core::staging::accept(id)` / `reject(id)` calls the activity-detail page uses; clicking accepts/rejects exactly that row.
- **Inline edit of the target.** Click the `→ <path>` (or `+ <slug>`) to edit it in place — saves a new `target_path` / tag value on the staging row (re-using the in-flight `staging-amend`-shaped mechanic; if amend isn't wired yet, falls back to "reject this row + propose a new one with the edited target"). Useful for "the cluster name is right but the folder needs to be `research/embeddings-v2/` not `research/embeddings/`."
- **Conflicted rows highlighted.** When a row's state is `conflicted` (per `staging-proposal-state`), the row gets a `⚠` glyph and the reason tooltip; Accept is disabled, Reject still works. Same display rules as the activity-detail page.
- **Accept-all / Reject-all** are batch verbs over the visible (non-conflicted) rows. Confirm dialog when N > 5 for Accept-all; always confirm for Reject-all (rejecting agent/automation work is destructive).
- **Back to tree** returns to the expanded tree view without closing the pending rows — useful if the user wants to adjust a policy and re-run Apply, or just look at the tree structure while reviewing.
- **Auto-close on completion.** When every emitted row is resolved (accepted or rejected), the pane closes, the tree's `state` flips to `applied`, and the expanded tree view reappears with a confirmation toast (`24 changes applied, 0 pending`).
- **Skipped policies surface as a note.** Frozen subtrees and unpolicied leaves don't produce rows; the pane header notes `(7 leaves skipped — no policy assigned)` so the user knows nothing got silently dropped.

The pane is a new editor-pane sub-mode keyed by `buffer.mode.kind === "cluster-batch-review"` with `metadata.tree_id` on the buffer state. Same nav-stack discipline as the expanded tree view: entering pushes once, Back-to-tree pops, individual row accept/reject don't push. [cluster-editor-batch-review-pane-mode]

Why a dedicated pane and not just deep-link to the activity-detail page: a one-shot reorg can produce dozens of rows whose only meaningful framing is "the changes this Apply pass wants to make." A general Pending filter mixes them with triage matches, MCP tool-call proposals, and trail draft rows — losing the "this is one cohesive batch you just authored" framing that's the point of the one-shot flow. The activity-detail page is still the right surface for unscoped review; the in-pane view is the right surface for tree-scoped review.


## LLM actions via the task queue

Three LLM-driven actions in this surface route through `core::tasks` rather than direct `core::llm` calls:

- **Initial cluster naming/summarization** during the build pass (already specced as `cluster-summarize-llm` in `clustering.md`; that fan-out routes through the task queue per the queue's "everything except chat" scope).
- **Regeneration** triggered by the user's "Regenerate names" toolbar button or per-node "Regenerate this node" — submits one task per node needing regeneration, at `Normal` priority (user is watching).
- **Auto-name on merge / split** — merging siblings or splitting a cluster produces new clusters that need names; submitted as `Normal` tasks immediately so the new rows aren't stuck with placeholder names.

[cluster-editor-llm-actions-via-task-queue]

User-edited names/summaries are skipped by all three — the `user_edited_*` flag short-circuits the regeneration path. Explicit "Regenerate this node" from a user-edited node confirms with a dialog before clobbering. [cluster-editor-user-edit-provenance]


## Graph view for policy assignment

Third view variant, alongside the sidebar mode and the row-shaped expanded mode. Renders the tree as a node-link graph using the project's committed graph renderer (`sigma.js` + `graphology`, per `design.md`'s graph-view bullet) — tuned for "see the whole tree at a glance, assign policies by clicking nodes, watch the colors light up." [cluster-editor-graph-view]

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

- **Layout — radial tree by default.** Root at center, descendants on concentric rings, sibling spread by angular sweep. Fits a rectangular pane well, scales to 5–6 levels deep without cramping, and reads at a glance ("which subtrees are big, which are dense"). Alternative layouts (`vertical-tree`, `horizontal-tree`, `force-directed`) are selectable from the view menu. Layout choice is part of the per-tree saved view state (`cluster-editor-graph-view-saved-view-state`); persisting one global preference would conflict with users who like one layout for the Evergreen tree and another for a Sapling. [cluster-editor-graph-view-layout]
- **Layout-extensible framework.** Layouts live behind a `GraphLayout` trait in `ui/src/clusterEditor/graphView/layouts/`, one file per layout, registered into a layout-id-keyed registry. Adding a layout = new file + registry entry + view-menu row. The renderer (`sigma.ts`) calls `layout.assign_positions(graph)` and stays layout-agnostic. [cluster-editor-graph-view-layout-extensible]
- **Node color encodes the resolved policy.** Distinct colors for `Move` / `Tag` / `Freeze` / `no policy`. A `⏸` glyph overlay marks nodes with `require_review = true`. Inheritance is visualized via softer-shade colors on descendants whose policy walked up to an explicit ancestor — at-a-glance "which subtrees actually have rules versus which are riding inherited ones." [cluster-editor-graph-view-color-by-policy]
- **Node size encodes member count.** Leaf clusters with more members render as larger nodes; root and high-level clusters can be configured to scale logarithmically so the root doesn't dwarf everything. [cluster-editor-graph-view-size-by-members]
- **Node-label text styling.** Each node's text label renders on a rounded white background with a 1px solid `#c8c8c8` outline (~6px horizontal / 2px vertical padding, ~3px corner-radius). Text reads against any policy fill / theme backdrop without losing the node's own color encoding. Wired via sigma's `defaultDrawNodeLabel` setting in the renderer adapter so the styling is consumer-agnostic — the vault-wide graph view will pick up the same label background when it lands. [cluster-editor-graph-view-label-style]
- **Summary staleness tint.** Nodes with `summary_membership_churn > 0` render with a soft tint (slight desaturation of the policy color, opacity-style rather than a new hue) so the "this summary may be out of date" signal is visible at a glance without breaking the policy-color encoding. Hovering shows the exact churn count in the tooltip; clicking the policy chip in the popover offers a Regenerate-summary affordance alongside the policy controls. [cluster-editor-graph-view-summary-staleness-tint]
- **Click a node** — opens the policy editor popover anchored to that node. Same editor as the row view (`Tag` / `Move` / `Freeze` / `None` + slug/folder + require-review checkbox). Submitting updates the node row in `trees.db` and recolors immediately. [cluster-editor-graph-view-click-to-edit-policy]
- **Shift-click multi-selects nodes** for bulk policy assignment — set a policy on N nodes at once via the same popover, applied to each. Same multi-select semantics as the row view's multi-select; the bulk-action toolbar from the row view doesn't apply here (merge/split/etc. are textual operations).
- **Hover a node** — overlay tooltip with the cluster's name, summary, member count, and the resolved policy. Held-hover (>500ms) expands the tooltip to show member-note titles (capped at ~10 with "and N more"). The tooltip anchors near the cursor (mouse-relative, with a 12,12 offset and a viewport clamp so edge-hovers don't slide it off-screen) rather than at the canvas corner, with a `min-width` floor so sparse content doesn't collapse to a 1px line. [cluster-editor-graph-view-hover-detail]
- **Filter chrome** — top-of-pane filter strip lets the user dim everything *except* nodes matching a given policy (e.g. "highlight only Move nodes," "highlight only nodes with `require_review`," "highlight no-policy nodes"). Dimming rather than hiding so the structure stays legible. [cluster-editor-graph-view-policy-filter]
- **Click a leaf** — single-click pins the note into the in-canvas note preview card (when the preview is enabled in the view menu) and shows the hover tooltip; **double-click opens the note** in the editor pane, same as the row-shaped expanded view (the open path activates the new tab via the standard `openFile` flow). Clusters retain single-click-opens-policy-popover. Nav-stack discipline unchanged. [cluster-editor-graph-view-leaf-click-opens-note]
- **No drag-reshape from the graph view.** Drag-drop tree edits (move-note-between-clusters, etc.) stay in the row view where the targets are unambiguous; in a graph view, "drag node A onto node B" is too easy to misclick. The graph view's job is overview + policy assignment; reshape is the row view's job. [cluster-editor-graph-view-no-reshape]
- **Pan / zoom via keybinds.** Default chords land via the keybind registry (`keybind-registry`) — `pointer-drag` for pan, `wheel` for zoom, `pinch` for zoom on touchpads. The defaults map to the registry-canonical chord ids `cluster-editor.graph-pan`, `cluster-editor.graph-zoom`, `cluster-editor.graph-zoom-pinch`. Users rebind via the existing keybind UI; no graph-view-specific chrome for pan/zoom. [cluster-editor-graph-view-pan-zoom-keybinds]
- **Selection visual — outline ring.** A selected node renders with a thin outline ring in the accent color (`var(--accent)`). Multi-select shows the same ring on every selected node; the bulk-action toolbar at the bottom of the pane shows the count (`Selected: 3 nodes`). Minimal styling, matches the rest of the UI. [cluster-editor-graph-view-selection-outline]

### View menu

A small icon button on the pane toolbar — same **eye icon** the editor's view-options menu uses (`editor.md` → `## View options menu`) — opens a popover with view-only switches that don't mutate the tree. Choices: [cluster-editor-graph-view-view-menu]

- **Leaf visibility.** Three modes: `Hide leaves` (only cluster nodes render), `Auto (LOD)` (leaves hidden when zoomed out below a threshold, fade in as the user zooms in — default), `Show all leaves` (every leaf node is always present in the canvas). [cluster-editor-graph-view-leaf-visibility]
- **Layout.** Radio: `Radial (default)` / `Vertical tree` / `Horizontal tree` / `Force-directed`. Switching re-runs the chosen layout's `assign_positions` and animates nodes to their new positions (sigma supports tweened layout transitions). [cluster-editor-graph-view-layout]
- **Show outliers.** Bool toggle, default on. When off, the disconnected outlier node (and its leaves, when leaves are visible) is hidden from the canvas. Distinct from the build-time `include_outliers` option (which controls whether outliers are *generated* in the first place — per `cluster-review-tab-config-section`); the view-menu toggle just hides them in the current view. [cluster-editor-graph-view-show-outliers]
- **Reset view** / **Fit to view.** Menu actions that re-center and rescale the canvas. Reset returns to the layout's default zoom + pan; Fit-to-view scales the canvas so every visible (non-hidden by other toggles) node fits in the viewport. [cluster-editor-graph-view-reset-fit]

View-menu choices are per-tree saved view state (persisted on `cluster_trees.view_state` as a JSON column, or in `vault/.hiker/config.toml` keyed by tree id — implementation choice). Pan/zoom positions also persist there so the user comes back to where they left off. [cluster-editor-graph-view-saved-view-state]

### Outlier rendering in the graph

The outlier bucket renders as a **separate disconnected node**, floating off to the side of the main tree (default: lower-right corner of the canvas). It carries the same "Outliers (N)" label as the row view's virtual node. Its policy chip works identically to any other node's — clicking it opens the policy editor; setting a `Move` or `Tag` policy on it applies that policy to every member note classified as outlier by the placement classifier. [cluster-editor-graph-view-outlier-disconnected]

Setting an outlier policy is the canonical pattern for "send unsorted notes to `inbox/unsorted/`" or "tag them `unsorted`" — instead of leaving outliers to rot in the inbox, the user makes the policy explicit. [cluster-editor-outlier-policy]

The build-time `include_outliers = false` option (per `cluster-review-tab-config-section`) suppresses the outlier bucket entirely — notes that would have been outliers get force-routed into their nearest cluster instead. The view-menu's `Show outliers` toggle only hides the rendered node, not the underlying data; both can be set independently.

### View switching

A toggle in the toolbar of the expanded pane flips between tree (row) view, graph view, and markdown view — three peer rendering modes of the same tree. The user's current selection survives the switch (so multi-selecting in row view and toggling to graph view shows those nodes highlighted in the graph). [cluster-editor-graph-view-toggle]

### Renderer integration

The graph view is a `GraphRenderer` consumer per `design.md`'s renderer adapter pattern. Concrete plumbing:

- `core::trees` produces a `ClusterTreeGraph { nodes, edges }` DTO via a dedicated query (`Trees::tree_as_graph(tree_id)`). Nodes carry id, name, kind, member_count, resolved_policy, explicit_policy_flag, depth. Edges are parent → child.
- `ui/src/clusterEditor/graphView/` mounts the renderer adapter (`renderers/sigma.ts` per the existing pattern in `ui/src/graphView/renderers/sigma.ts` — possibly literally the same adapter file or a thin subclass; node-color + node-size and policy-filter overlays are app-state shape concerns that live in the panel module per `design.md:463`).
- Sigma + graphology bundle is lazy-loaded — paying the bundle cost only when a user opens the graph view. Same dynamic-import pattern as the vault graph view. [cluster-editor-graph-view-lazy-load]
- Re-renders on `cluster_nodes` row changes (policy edits, name edits, structure edits via the row view, triage producing new rows). Implementation can either fully re-mount or compute a diff and patch — the renderer adapter's capability flag for in-place updates determines which.

The cluster-tree graph view is a *separate surface* from the vault-wide graph view (different data, different consumer module). They share the renderer primitive, not the data plumbing. [cluster-editor-graph-view-not-vault-graph]


## Reusable row primitive

The row component (chevron + icon + name + summary preview + members count + policy chip + selection state + multi-level hierarchy + multi-select + right-click context menu) lives in `ui/src/clusterEditor/rowPrimitive.ts` and is consumed by both the sidebar (`mountClusterEditor`) and the center pane (`mountClusterEditorPane`). Both surfaces share class names (`.ce-*`); the pane CSS re-targets the shared row rules under `.cluster-editor-pane` so spacing / typography can diverge without forking the markup. Per-surface state (`expanded: Set<NodeId>`, `selection: Set<NodeId>`) is owned by the caller and survives refreshes triggered by queue-event listeners. Future hierarchical surfaces (e.g. saved-collections-of-collections, multi-axis cluster trees) plug into the same primitive. [cluster-editor-row-primitive]

Trails and the vault-wide graph/map view (per `design.md`) deliberately do *not* reuse this row primitive — trails are sequential not hierarchical, and the vault graph's edge-rendering needs are fundamentally different from a tree's. The reuse is at the row-primitive level only.

The cluster editor *does* reuse the **graph renderer** (`sigma.js` + `graphology` per `design.md`'s graph-view bullet) for its own tree-shaped graph view (next section). The exclusion is about data models and surfaces, not about the underlying rendering primitive — the renderer adapter pattern in `design.md` exists precisely so multiple surfaces can ride the same WebGL canvas with their own data. [cluster-editor-renderer-reuse]


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

- `core::trees` — owner of `trees.db`; persists `cluster_trees` / `cluster_nodes` / `cluster_tree_history`. Same module-discipline pattern as `core::store` / `core::changes` / `core::staging` — plain Rust types out, no SQL leakage.
- `core::cluster_editor` — the implementation home of the UI-facing editor operations; sibling to `core::cluster` / `core::suggest` / `core::trees`. Consumes `core::cluster::build_tree`, applies edits through `core::trees`, emits one-off staging proposals via `core::staging` for the multi-select Stage move/tag verbs.
- `core::tasks` — every LLM-driven action in this surface (initial naming, regeneration, merge/split renaming) submits there.
- `editor.md` — sidebar mode switcher lives in the editor's domain; this spec defines the cluster-trees mode body, the editor.md mode-switcher entry is owned there.
- `suggestions.md` — the apply mechanic (`suggestions-apply-cmd`) is reused as-is for one-shot Apply and for the accept path of triage / one-off staging rows produced by this surface. The markdown view is rendered on demand from `trees.db` rows; it is not a persisted on-disk format.
- `settings.md` — `staging.db`'s `action` column gains `move_note` (carrying `source_path` for source-folder safety checks); `surface = "triage"` and `surface = "cluster-editor"` are produced by this doc's flows.
- `keybind-registry` — chord ids reserved: `cluster-editor.toggle-expand`, `cluster-editor.merge-siblings`, `cluster-editor.merge-up`, `cluster-editor.split`, `cluster-editor.regenerate`. Chords TBD; land when each action is wired.
