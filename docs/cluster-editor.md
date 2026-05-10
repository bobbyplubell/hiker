# Cluster editor

Interactive surface for viewing, manually editing, and configuring automation on a cluster tree (per `clustering.md`'s `ClusterTree`). Replaces the markdown-proposal flow as the primary review surface for one-shot suggestions; the markdown form (`suggestions-proposal-md`) sticks around as a serialization + audit-log shape, not as the user's working surface.

The headline decisions:

- **Lives in the left sidebar as a switchable mode, with an Expand button to flip into a full center pane for graphical work.** The sidebar gains a mode switcher (Files / Cluster trees / future Trails) just above the existing `+ New note` row; switching to Cluster trees mode swaps the sidebar body to the cluster editor, hiding the filetree-only chrome (+New, `…` actions, Trash bin). An Expand button on the cluster-trees header sends the editor to the center pane, replacing CodeMirror with a graphical tree view tuned for drag-drop reshaping; click a leaf in expanded mode to open the note in the editor, the existing back-navigation returns to the expanded tree. Mirrors `chat-panel-expand-to-editor`. [cluster-editor-sidebar-mode, cluster-editor-pane-expand]
- **Multiple trees can be open at once.** The sidebar's Cluster trees body shows a list of open trees — typically the saved triage tree (always present once saved) plus zero-or-more ephemeral trees from one-shot runs. Each tree expands inline to show its node hierarchy. Header on each tree row shows name + state pill (`draft` / `applied` / `saved as triage`). Switching between trees is just expanding a different one — no modal context-swap. [cluster-editor-multiple-trees-open]
- **Manual reshaping at every level.** Move notes between clusters, merge siblings, merge children up into the parent, split a cluster (re-run HDBSCAN against just its members), rename a cluster, edit an LLM-generated summary, drop a cluster (members fall to outliers), promote outliers into a cluster. Drag-drop where it's natural (move-note); buttons on the multi-select toolbar where it's not (merge / split). Every edit is undoable until apply. [cluster-editor-node-operations]
- **Automation policies attach at any level.** A policy (auto-tag / auto-move / queue-for-review / freeze) can be set on a leaf, a mid-tree cluster, or the root. Resolution at triage time walks up from the affected note to the nearest ancestor with an explicit policy; no policy anywhere up to the root means the note is left alone. A saved tree is a *policy program*, not a single classifier with a knob. [cluster-editor-policy-any-level, cluster-editor-policy-resolution-walk-up]
- **LLM-generated names and summaries are user-editable, with provenance.** Click-to-edit on any cluster's name or summary inside the editor. Edited values get `user_edited: true` stamped on the node so a re-run of the cluster build (or a regen of summaries via `core::tasks`) doesn't clobber them. Re-generating an edited node's summary requires explicit "Regenerate" action — implicit overwrite is forbidden. [cluster-editor-edit-name-summary, cluster-editor-user-edit-provenance]
- **Auto-save in-progress edits to a draft file.** Every manual edit writes to `vault/.hiker/cluster-trees/<id>/draft.json` (the structured form) plus updates the parallel markdown view at `<id>/proposal.md`. Closing and reopening the editor resumes where you left off; the draft survives app restarts. Discard-draft is an explicit button. [cluster-editor-draft-persistence]
- **Apply and Save-as-triage are separate actions.** Apply walks the current tree state, runs the moves/tags through the existing apply mechanic (`suggestions-apply-cmd`), and marks the tree `applied`. Save-as-triage persists the tree with its policies as the active triage classifier (replaces any prior saved tree); does *not* run the apply pass. A user can both apply and save-as-triage from the same tree if both make sense. [cluster-editor-apply-action, cluster-editor-save-as-triage]


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

- **Suggest reorganization** — the primary affordance, kicks off `core::cluster::build_tree` over the current vault and adds the resulting tree to the open list as a new draft. Same as the old `hiker suggest` entry point (per `suggestions.md`); the cluster editor is just the new review surface. [cluster-editor-new-tree-action]
- **`…` menu** — mode-specific overflow: "Open saved triage tree" (no-op when already open), "Import tree from file" (load a `cluster-tree.json` from elsewhere — useful for sharing trees), "Discard all drafts," "Tree settings" (per-node policy defaults, etc.). [cluster-editor-mode-menu]

Each tree's body is a hierarchical list, one row per cluster or leaf note. Cluster rows are collapsible (chevron at left); leaf-note rows are clickable to open the note in the editor pane on the right. Multi-select works via Shift/Cmd-click on rows; selection survives expand/collapse so a bulk merge across collapsed sections is still possible.

Row layout:

```
[chevron] [icon] <name>                          [members] [policy chip] [drag handle]
                 <summary preview, one-line>
```

- **Icon** identifies the row type — folder-style glyph for a cluster, note glyph for a leaf, distinct outlier glyph for the outlier bucket.
- **Name** is the cluster's name (LLM-generated or user-edited) or the note's basename. Click-to-edit on cluster names; leaf names are read-only (rename a note via the regular file-tree affordances).
- **Summary preview** is the cluster's LLM-generated summary truncated to one line; click expands to a full-text editable area below the row, click the area's name in turn to edit. Leaves don't show a summary.
- **Members count** is the number of notes in the cluster's full subtree (including all nested children's leaves). Empty for leaves.
- **Policy chip** shows the resolved-or-explicit policy for this node — `auto-tag: <slug>` / `auto-move: <path>` / `review` / `freeze` / blank (no policy, walks up to nearest ancestor). Clicking the chip opens the policy editor for that node.
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
- **Click a cluster row → expand/collapse** as in the sidebar form. No navigation hop.
- **Multi-select via Shift/Cmd-click** picks rows across levels (selecting a cluster includes its subtree implicitly for purposes of the bulk-action toolbar). Selection ribbon at the bottom of the pane shows actions appropriate to the selection: Merge siblings (when 2+ siblings selected), Merge up (when 1+ children of the same parent selected), Split (when one cluster selected), Drop to outliers, Apply policy, Regenerate summary.
- **Drag-drop is the same as the sidebar form**, just at a larger scale — drop targets light up on hover.
- **Markdown view toggle** in the toolbar flips the pane between the graphical tree and a CodeMirror buffer rendering the equivalent `proposal.md` shape (per `suggestions-proposal-md`). The markdown view is editable; saving it parses back into the structured form. Acts as the escape hatch for users who prefer text editing or want to bulk-rewrite via vim-style motions. [cluster-editor-markdown-view-toggle]
- **Apply / Save as triage / Discard draft** are toolbar buttons. Apply executes the moves/tags via the existing apply mechanic; Save as triage persists the tree as the triage classifier; Discard draft confirms then deletes the draft.
- **Regenerate names** runs the cluster naming/summarization prompt for any cluster whose name/summary is *not* user-edited (per the `user_edited` flag). Each regeneration is one task in `core::tasks` (per `task-queue.md`); the user can watch progress in the queue widget. Regenerating a user-edited node requires per-node explicit "Regenerate this node" from its row menu. [cluster-editor-regenerate-via-task-queue]


## Tree shape on disk

Each open tree is one directory under `vault/.hiker/cluster-trees/<tree-id>/`:

```
.hiker/cluster-trees/
  <tree-id>/
    meta.json          # tree id, created_at, source ('one-shot' | 'saved-triage'), state, vault snapshot rev
    draft.json         # current editable shape (overrides build.json once any edit is made)
    build.json         # original ClusterTree snapshot from the build pass
    proposal.md        # markdown serialization of draft, regenerated on edit
    history.jsonl      # append-only edit log for undo / audit
```

`draft.json` is the source of truth for the editor's working state. Its shape extends `clustering.md`'s `ClusterTree`:

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
    AutoTag { tag: String },
    AutoMove { folder: VaultRel },
    Review,                           // queue for explicit review on triage match
    Freeze,                           // never propose changes for matches under this node
}

enum NodeKind { Cluster, Leaf, OutlierBucket }
```

[cluster-editor-tree-shape-on-disk]

`history.jsonl` records every edit as `{ ts, op, args, undo_args }` so an in-pane Undo / Redo works during the session, and a full history is recoverable from disk for diagnostics. [cluster-editor-undo-redo, cluster-editor-edit-history-jsonl]


## Operations

All operations are local edits to `draft.json` + a `history.jsonl` row + a regenerated `proposal.md`. None of them mutate the vault — only Apply does that.

- **Move note between clusters.** Drag a leaf onto a different cluster row (or use the row menu's "Move to…"). Updates the leaf's `parent`. Centroids of source and target clusters are recomputed. [cluster-editor-move-note-between-clusters]
- **Merge sibling clusters.** Multi-select 2+ clusters at the same level → toolbar "Merge siblings" → creates one cluster whose members are the union, name and summary are auto-regenerated (queues a task in `core::tasks` for naming) unless the user names it explicitly first. [cluster-editor-merge-siblings]
- **Merge children up to parent.** Select the parent → "Merge up" → flattens that subtree by one level. Children's leaves become direct members of the parent; children's clusters disappear. The parent's name/summary is unchanged unless explicitly regenerated. [cluster-editor-merge-children-up]
- **Split a cluster.** Select one cluster → "Split" → re-runs HDBSCAN against just its current members with a tighter `min_cluster_size` (default `max(2, current_min / 2)`). Produces a new layer of children. The user can keep the split or undo. Splitting a cluster preserves the parent's name; the new children get LLM names via the task queue. [cluster-editor-split-cluster]
- **Rename cluster** / **Edit summary.** Click name or summary → inline editor → save. Stamps `user_edited_*: true`. [cluster-editor-edit-name-summary]
- **Drop cluster.** Toolbar action on a selected cluster: drops the cluster, its children's leaves all fall to the nearest outlier bucket. Useful for "this cluster is junk; throw the notes back to inbox-via-outliers." [cluster-editor-drop-cluster]
- **Promote outliers / move out.** Drag a row in/out of the outlier bucket. Same machinery as move-note. [cluster-editor-promote-outlier]
- **Set / clear policy.** Click the policy chip on any node → policy editor (radio: auto-tag/auto-move/review/freeze/none + the action's parameter). Setting a policy on an ancestor automatically becomes the resolved policy for descendants without their own. [cluster-editor-set-policy]


## Per-node automation policy

Policies attach at any level. A note's effective policy at triage time is determined by walking from the note's leaf up the tree, taking the first ancestor with an explicit policy. No policy anywhere = no automation, the note is left alone (matches the existing "low confidence" tier behavior). [cluster-editor-policy-resolution-walk-up]

Policy types and their semantics:

- **auto-tag (slug)** — when triage matches a note into the subtree, write the tag to the note's frontmatter (per `suggestions-mode-tag`). Idempotent.
- **auto-move (folder)** — when triage matches a note, move the file into the target folder via `move_note` (per `suggestions-mode-move`). Subject to the existing safety rule that the source must be inside the configured triage scope.
- **review** — match queues into the pending-review panel (per `triage-pending-review-panel`). Mid-confidence default for "I want a human glance before this lands."
- **freeze** — match is explicitly ignored. Useful for marking a subtree as "this is well-organized already, leave it alone."

[cluster-editor-policy-types]

Confidence still flows through the matching engine — `cluster-place-greedy-descent` returns a target node and a confidence — but the **action** taken is driven by the matched node's resolved policy, not by a global threshold. A node's policy can carry an optional `min_confidence` parameter for users who want "auto-move this cluster only when match confidence ≥ 0.85"; that's a per-policy threshold, not a global one.


## Triage execution

When a tree is in `saved as triage` state and the user has any policy set on any node, triage runs against new/modified notes:

- **On save** (default) — when a note inside the configured triage scope (default `inbox/`) is saved, hiker enqueues a `RaptorTriageMatch` task in `core::tasks` (per `task-queue.md`). The direct-LLM worker (or any MCP-client consumer) processes the task by running the placement classifier (`cluster-place-greedy-descent`) and applying the resolved policy. [cluster-editor-triage-on-save]
- **Scheduled rerun** (opt-in) — `[suggestions.triage] scheduled_rerun = "0 3 * * *"` (cron-shape per the existing settings model) re-runs triage over a configurable scope. The schedule fires submissions to `core::tasks` for each affected note, batched at `Low` priority so it doesn't block foreground work. [cluster-editor-triage-scheduled-rerun]
- **Modified-note rerun** (opt-in) — separate from scheduled: when an existing note (already placed or already in inbox) is meaningfully edited (defined as "embedding changed by more than X cosine distance from prior"), triage re-evaluates. Distinct from on-save because most saves don't change embeddings significantly; the cosine guard avoids re-triaging on every keystroke save. [cluster-editor-triage-modified-rerun]

All three pathways submit through `core::tasks`, which routes to whatever worker is configured. The user watches progress in the queue widget; a per-policy task naturally inherits the queue's cancel + audit machinery. [cluster-editor-triage-via-task-queue]


## LLM actions via the task queue

Three LLM-driven actions in this surface route through `core::tasks` rather than direct `core::llm` calls:

- **Initial cluster naming/summarization** during the build pass (already specced as `cluster-summarize-llm` in `clustering.md`; that fan-out routes through the task queue per the queue's "everything except chat" scope).
- **Regeneration** triggered by the user's "Regenerate names" toolbar button or per-node "Regenerate this node" — submits one task per node needing regeneration, at `Normal` priority (user is watching).
- **Auto-name on merge / split** — merging siblings or splitting a cluster produces new clusters that need names; submitted as `Normal` tasks immediately so the new rows aren't stuck with placeholder names.

[cluster-editor-llm-actions-via-task-queue]

User-edited names/summaries are skipped by all three — the `user_edited_*` flag short-circuits the regeneration path. Explicit "Regenerate this node" from a user-edited node confirms with a dialog before clobbering. [cluster-editor-user-edit-provenance]


## Reusable row primitive

The row component (chevron + icon + name + summary preview + members count + policy chip + drag handle + selection state + multi-level hierarchy + drag-drop + multi-select) lives in `ui/src/treeRows/` and is consumed by the cluster editor. Future hierarchical surfaces (e.g. saved-collections-of-collections, multi-axis cluster trees) plug into the same primitive. [cluster-editor-row-primitive]

Trails and the graph/map view (per `design.md`) deliberately do *not* reuse this primitive — trails are sequential not hierarchical, and the graph view's edge-rendering needs are fundamentally different from a tree's. The reuse is at the row-primitive level only, not at the surface level. [cluster-editor-not-for-trails-or-graph]


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

- `core::cluster_editor` — the implementation home; sibling to `core::cluster` and `core::suggest`. Consumes `core::cluster::build_tree`, owns the editable shape, persists to `.hiker/cluster-trees/`.
- `core::tasks` — every LLM-driven action in this surface (initial naming, regeneration, merge/split renaming) submits there.
- `editor.md` — sidebar mode switcher lives in the editor's domain; this spec defines the cluster-trees mode body, the editor.md mode-switcher entry is owned there.
- `suggestions.md` — the apply mechanic (`suggestions-apply-cmd`) is reused as-is; the markdown proposal format is the on-disk serialization the editor reads/writes.
- `keybind-registry` — chord ids reserved: `cluster-editor.toggle-expand`, `cluster-editor.merge-siblings`, `cluster-editor.merge-up`, `cluster-editor.split`, `cluster-editor.regenerate`. Chords TBD; land when each action is wired.
