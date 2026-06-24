# Code in the vault

Hiker reads and reasons about code — your own projects — alongside notes, specs, and the code-intelligence graph, without ever becoming an editor for it. Two constraints kept colliding: hiker is **not an IDE / not for writing code**, and loading files from *outside* the vault is a **trust** smell. The reframe that resolves both: the boundary is **read vs. write**, not IDE-vs-not. Hiker can read, index, embed, link, and view code; it never writes, edits, commits, or manages its git. This is the authored-vs-reference split applied to files — `.md` notes are authored (editable, versioned, op-logged); everything else (code) is read-only reference content.

## The boundary is read, not write

- **Read vs. write is the line.** Code files are **read-only reference content**: opened read-only and syntax-highlighted (reusing the read-only buffer modes), with no autosave, no op-log / versioning, no edit, and no git operations. "Not an IDE" lives entirely on the *write* side. If you edit a repo in your real editor, hiker's watcher re-indexes passively; the repo's `.git` is the user's, and hiker ignores it for its own versioning. [code-read-vs-write]
status:: partial
note:: code is read-only reference content (no autosave / op-log / edit / git); `.md` stays the only authored content. **Partial**: the broad read-only code-viewer + passive re-index path is item 3, not fully landed · evidence: `docs/code.md` direction; read-only buffer modes
- **Read-only code viewer + syntax highlighting.** A code file opens as a strictly read-only preview buffer (`BufferSource::CodeFile`; the save path short-circuits, so no write ever reaches disk) [code-read-only-view]. Highlighting rides the editor's decoration pipeline: `editor-ts` (tree-sitter) parses the whole doc once per open (code buffers never edit, so the layer caches on doc id) and emits theme-colored marks; the language is picked by extension, and only wired grammars highlight — Rust and Python today, others fall back to plain text rather than guessing (`lang-*` features on `editor-ts`). [code-syntax-highlight]
status:: done
implements:: [[code:hiker/tab/impl#[TabKind]icon]], [[code:hiker/editor_pane/ensure_readonly_buffer_loaded]]
note:: code file opens as a strictly read-only preview buffer; autosave / op-log save path short-circuits (no write reaches disk) · evidence: `app/src/editor_pane.rs` (`open_code_file`, `BufferSource::CodeFile`)

[code-syntax-highlight]
status:: done
touches:: [[code:hiker/panels/buffer/decorations]]
note:: tree-sitter syntax marks on read-only code buffers, by extension (`.rs`, `.py`); whole-doc parse cached on doc id (code is read-only → one parse per open); theme-token colored with built-in fallback palette; more languages = wire another `lang-*` bundle · evidence: `editor/editor-ts` (`languages::bundle` rust+python wired, features `lang-rust`/`lang-python`), `app/src/panels/buffer/decorations.rs` (`code_language_for`, `ts_syntax` layer + cache slot)
- **Code is indexed, embeddable, linkable, viewable — not authored.** Code in the vault is searchable, embeddable, linkable (wikilinks / relations / spec→code), and viewable in a read-only tab. `.md` stays the only authored, writable, versioned content. Embedding is opt-in / per-folder and lazy — embedding a big codebase is real compute. [code-as-reference-content]
status:: partial
note:: code indexed / embeddable / linkable / viewable, not authored; embedding opt-in / per-folder / lazy. **Partial**: code-as-content search/embeddings not wired
- **The vault is the trust boundary.** Hiker only ever reads **inside the vault root** — all disk reads are clamped to it; nothing external is read. This is a hard invariant. A vault may contain code up to whole repos (making `~/projects` itself a vault dissolves the trust problem: if the code is in the vault, reading it is in-boundary by definition). Hiker writing to / committing code, or managing nested repos' git, is explicitly out of scope — the IDE line. [code-vault-trust-boundary]
status:: partial
touches:: [[code:hiker/scip_adapter]]
note:: all disk reads clamped to the vault root; a vault may contain whole repos. **Partial**: panel-level root ⊆ vault enforcement landed; full core "reads-only-inside-vault" invariant is item 1/3 · evidence: `code-intel/hiker-code/src/scip_adapter.rs` (`safe_join`); panel repo-root ⊆ vault clamp

## Ignore policy

- **Ignore is noise, not repos.** Indexing excludes build/vendor noise (`target/`, `node_modules/`, `.venv/`, binaries), **not** the repos themselves. `.gitignore` is respected by default; a vault-root `.hikerignore` adds hiker-specific extras. Notes (`.md`) must **never** be gitignored away from indexing. [code-ignore-noise]
status:: planned
note:: respect `.gitignore` + vault-root `.hikerignore`; exclude build/vendor noise not repos; never gitignore `.md` away from indexing

## The `.scip` index

A `.scip` is mentally a **ZIM file**: a self-contained, read-only, droppable, regenerable index hiker *consumes*, opened from the file tree into a viewer tab and sandboxed like ZIM HTML (see `zim.md`). A repo is the live corpus it's baked from; both are read-only external corpora, and the spec graph is the one place they (and docs) become linkable.

- **The code graph needs only the `.scip`.** Topology, names, kinds, navigation, and blast-radius are all built from the index; **only `content()` / `fingerprint()` read source files** (preview + drift). So "look at an external project" = drop in just its `.scip` — you mirror the *index*, not the project. `.scip`-only implies limited previews; full previews need the source too. [code-scip-droppable]
status:: done
note:: code graph built from the `.scip` alone (topology / names / kinds / nav); only `content()` / `fingerprint()` read source files · evidence: `code-intel/hiker-code/` (`ScipAdapter`); `code-intel/fixtures/*.scip`
- **Path traversal is the one real `.scip` risk** — a crafted index could carry `../../etc/passwd`. The `ScipAdapter` reads every source file through `safe_join`, which refuses absolute / `..` / symlink-escape paths so a crafted index can't read outside the repo root; index strings are treated as display-only. The panel additionally enforces repo root ⊆ vault. Unit-tested. [code-scip-safe-join]
status:: done
touches:: [[code:hiker/scip_adapter]]
note:: every source read clamped to repo root; refuses absolute / `..` / symlink-escape; index strings display-only · evidence: `code-intel/hiker-code/src/scip_adapter.rs` (`safe_join`, unit-tested)

## The code graph

The code graph is a third `graph_view::Source` beside the vault link graph and the cluster graph (`graph-view.md`); it renders through the same engine. The semantic structure (containment, drill-down) lives here; the *rendering* policy (the constant-font label budget, size-by-LOC) is specced in `graph-view.md` ([[spec:graph-label-budget]], [[spec:graph-size-by-loc]]).

- **`CodeGraph` Source.** `ScipAdapter::code_graph()` exposes the in-memory graph in render shape (`CodeGraph { nodes, edges }`, stable per-node index, external/local endpoints dropped, deterministic ordering); `app/src/panels/code_graph.rs::CodeGraphSource` maps entity kind → shape (type = square, else circle) + colour, and edges → index pairs. Verified headlessly: `widgets/graph-view/examples/code_graph_snapshot.rs` drives the real engine over a fixture `.scip` to a PNG. [code-graph-source]
status:: done
touches:: [[code:hiker/panels/code_graph]], [[code:hiker/scip_adapter]]
note:: third `graph_view::Source`: `CodeGraph { nodes, edges }`, kind → shape/colour, edges → index pairs; verified headlessly to PNG over a fixture `.scip` · evidence: `app/src/panels/code_graph.rs` (`CodeGraphSource`), `code-intel/hiker-code/src/scip_adapter.rs` (`code_graph()`), `widgets/graph-view/examples/code_graph_snapshot.rs`
- **The code-graph view panel: shared doc + per-lens views.** The code-graph tab is a `TabKind::Container { primary: CodeGraphLens, secondary: Peer(CodeGraphLens), swapped }` ([[spec:tab-kinds]]). State splits in two: a shared `CodeGraphDoc` (one per `CodeSource`, in `AppState::panels.code_graph_docs` keyed by `CodeSource::key()`) holds the bound adapter, the full unified `EntityGraph`, governance + change set + palette + label-importance, and the SHARED `selected`/`hover_specs`/`focus_hops`; each visible lens is a `LensView` (in `code_graph_lenses`, keyed by `child_state_key`) carrying its OWN single force-layout engine + filtered display + drill scope. A pick in one lens reflects in the other because both read the doc. Flipping tabs keeps each source's doc + lenses (and the non-Clone adapter + layout workers) warm; the file tree routes both openable shapes here (a `.scip` directly, a `hiker.kind: project` note); the first build seeds the global nav stack with the initial overview drill location; and `apply_nav_target` is the pure (egui-free, unit-testable) restore side of Back/Forward. [code-graph-view-source]
status:: done
implements:: [[code:hiker/panels/code_graph/show_lens]], [[code:hiker/panels/code_graph/apply_nav_target]]
touches:: [[code:hiker/panels/code_graph]], [[code:hiker/files/sidebar]]
note:: shared `CodeGraphDoc` (per `CodeSource`) + per-visible-lens `LensView` (one engine each — the old monolithic three-engine `View` is gone). `open` builds a `Container` of two same-source `CodeGraphLens` children (`code_container`); `show_lens` renders the active lens as the main pane, `show_secondary` the corner minimap; both warm-reuse the source-keyed doc. `apply_nav_target` restores `(selected, scope)` without re-recording · evidence: `app/src/panels/code_graph/{doc,lens,mod}.rs` (`CodeGraphDoc`, `LensView`, `show_lens`, `show_secondary`, `apply_nav_target`), `app/src/files/sidebar.rs` (open routing), `app/src/tab.rs` (`TabKind::Container`/`CodeGraphLens`)
- **Open a project note or a `.scip` directly.** `TabKind::CodeGraphLens` carries a `CodeSource` enum (`Project(note) | Index(scip)`): opening a `hiker.kind: project` note routes here (mirroring `is_board_doc`), and the file tree can open a `.scip` directly (repo root defaults to the index's own directory). Both funnel through one `ScipAdapter::load`. [code-graph-tab]
status:: done
touches:: [[code:hiker/panels/code_graph]]
note:: opens a `hiker.kind: project` note or a `.scip` directly; both funnel through one `ScipAdapter::load` · evidence: `app/src/panels/code_graph/mod.rs` (`open`, `CodeSource::{Project,Index}`), `app/src/tab.rs` (`TabKind::CodeGraphLens`), `workbench_host.rs` (`is_project_doc`)
- **Click → read-only detail.** A node's `click_path` carries the SCIP moniker; the panel resolves the selected node's kind + definition `file:line` via the adapter's `locate`. No editable tab is opened — consistent with [[spec:code-read-vs-write]]. [code-node-detail]
status:: done
touches:: [[code:hiker/panels/code_graph]]
note:: click resolves selected node's kind + definition `file:line`; no editable tab (per [[spec:code-read-vs-write]]) · evidence: `app/src/panels/code_graph.rs` (`click_path` → adapter `locate`)
- **Containment collapse.** The adapter derives **containment** (`GraphNode.parent`, from moniker nesting + impl-type fallback; `is_object()` = type / module). A reusable, tested `hiker_code::collapse` lifts hidden members' edges up to their nearest visible ancestor (pure; the consumer supplies the visibility policy). The visibility policy is the entity-kind filter ([[spec:code-graph-kind-filters]]); the fixed Objects → +Functions → Everything tiers it replaced are retired. [code-graph-lod]
status:: done
touches:: [[code:hiker/panels/code_graph]]
note:: containment-based collapse; `hiker_code::collapse` lifts hidden members' edges to the nearest visible ancestor; the visibility policy is the kind filter · evidence: `code-intel/hiker-code/src/` (`collapse`, `GraphNode.parent`, `is_object`), `app/src/panels/code_graph.rs` (`rebuild_display`)
- **Entity-kind filter, auto-populated from the data.** A toolbar filter section with one toggle per entity kind **present in the loaded graph** (sorted, colour-keyed to the nodes) — derived at bind time from the data itself, so a source with new kinds grows new toggles and one without e.g. macros never shows a dead one. Hidden kinds collapse (their members lift edges to the nearest visible ancestor) rather than vanish. Defaults are data-driven: everything visible for small graphs, only structural objects (types/modules) above ~2k nodes — the legibility default the old fixed tiers provided. Persisted as the **hidden** set, so a kind first appearing after a reindex defaults to visible. [code-graph-kind-filters]
status:: done
implements:: [[code:hiker/panels/code_graph/kind_filter_for]]
touches:: [[code:hiker/panels/code_graph]]
- **Scoped by default so a big repo never hairballs.** The default view is scoped to the top nodes by degree (`scope_top_degree`, cap 400) with an explicit "top N of M by degree" summary; a `--focus <name> --depth N` neighbourhood is available in the CLI. [code-graph-scope]
status:: done
touches:: [[code:hiker/panels/code_graph]]
note:: default scoped to top-degree nodes with a "top N of M" summary so a big repo never silently hairballs · evidence: `app/src/panels/code_graph.rs` (`scope_top_degree`, cap 400), `code-intel/code-cli` (`--focus/--depth`)
- **Edge-type toggles.** Calls / Implements edge-type toggles (via the engine's view-options menu) relayout on change. [code-graph-edge-toggles]
status:: done
touches:: [[code:hiker/panels/code_graph]]
note:: Calls / Implements edge-type toggles relayout on change · evidence: `app/src/panels/code_graph.rs` (view-options menu)

### Selection, scope + back/forward

- **Selection + scope dial.** Clicking a node (or a Find-popup pick) **selects** it — selection drives the detail line and the focus spotlight; it never recenters the view by itself (the engine's glide does that, [[spec:code-graph-glide-to-selected]]). A four-position toolbar **Scope** dial decides the display: **Overview** (the whole kind-filtered graph) or the selection's **undirected 1/2/3-hop neighbourhood** (`hop_mask`, a BFS over the symmetric adjacency of the *full* graph — hop distance is structural; the kind filter only decides what's drawn, and the anchor always shows even when its own kind is filtered out). The hop positions are disabled until a node is selected; a cleared/stale selection falls back to the overview display. In a Hops drill the anchor is **stored** (`LensView.hops_anchor`, latched once at drill time by `sync_hops_anchor`) DECOUPLED from the live selection — so clicking empty background clears the spotlight highlight but STAYS in the filtered view; only Esc (the middle rung) or the toolbar Overview leaves it. The small filtered subgraph then shows MOST labels (the small-graph rule, [[spec:graph-label-budget]]). Fresh-layout-on-anchor-change is the crucial bit: when the hops anchor changes, `rebuild_display` re-lays-out the small neighbourhood fresh/compact rather than warm-seeding from the huge overview's positions; hop-count and filter tweaks keep the warm morph. A Find pick from the overview switches to 2-hop scope so the picked node is revealed even when the filter would hide it. [code-graph-scope-hops]
status:: done
implements:: [[code:hiker/panels/graph_nav/hop_mask]], [[code:hiker/panels/code_graph/lens/rebuild_display]], [[code:hiker/panels/code_graph/lens/sync_hops_anchor]], [[code:hiker/panels/code_graph/apply_nav_target]]
touches:: [[code:hiker/panels/code_graph]]
note:: supersedes the click-to-drill focus mode ([[spec:code-graph-focus-neighbourhood]]): click selects, the scope dial drills — orthogonal. The drill anchor is `LensView.hops_anchor`, stored separately from `doc.selected` (`sync_hops_anchor` latches it once, clears it only on Overview), so a background deselect keeps the subgraph put; a small filtered subgraph (≤ `SMALL_GRAPH_LABELS`) lifts the label budget so most labels show. The BFS `hop_mask`, scope dial, Esc rung, and scope persist strings live in the shared navigation layer ([[spec:graph-nav-extract]]); this panel keeps the code policy (collapse-through-filters, fresh-layout-on-anchor-change, SCIP kinds) · evidence: `app/src/panels/code_graph/lens.rs` (`hops_anchor`, `sync_hops_anchor`, `hops_mask`, `rebuild_display`), tests `hops_anchor_persists_through_deselect_and_clears_on_overview`
- **Focus-mode neighbourhood drill.** Clicking a node set `focus` and the display became its neighbourhood, with a separate hops selector and "← Overview" — drill and selection were entangled. [code-graph-focus-neighbourhood]
status:: superseded
note:: superseded by [[spec:code-graph-scope-hops]] — selection and scope split into orthogonal controls; the BFS neighbourhood machinery carries over as `hop_mask`
- **Code-graph back/forward.** Code-graph drills ride the **global** navigation stack (`NavTarget::CodeGraphNode`) over the two settle-triggering fields `(selected, scope)`. Toolbar `⟵`/`⟶` buttons (disabled at the ends), plus Alt+←/→ and mouse Extra1/Extra2 back/forward. Back/Forward restores replay through `apply_nav_target` → `rebuild_display` without re-recording (guarded by the `nav_restoring` flag / `nav.locked`). [code-graph-nav-history]
status:: done
touches:: [[code:hiker/panels/code_graph]]
note:: global-stack back/forward over `(selected, scope)`; restores replay through `apply_nav_target` without re-recording · evidence: `app/src/panels/code_graph.rs` (`nav_snapshot`, `nav_restoring`), `app/src/panels/graph_nav.rs` (`nav_controls`, shared with the vault graph per [[spec:graph-nav-extract]]), `app/src/state.rs` (`NavTarget::CodeGraphNode`), `app/src/editor_pane.rs` (restore arm)
- **Glide to the selected node.** Selecting a node smoothly pans the affine view to centre it (~0.4s ease-out), so the selection's footprint frames itself without a jarring jump. The glide is the engine's affine glide-to-selection ([[spec:graph-glide-to-selected]]), fired when the lens sets the engine's `selected_node`: cancelled on a manual pan/zoom and skipped during a fit/re-fit (a fresh build / scope-drill owns the framing then). [code-graph-glide-to-selected]
status:: done
touches:: [[code:hiker/panels/code_graph]], [[code:hiker/graph_view]]
note:: a thin cross-reference — the behaviour is the engine-level [[spec:graph-glide-to-selected]] (`State::glide_to`, triggered in `panes.rs` from a `selected_node` change); the lens drives it by setting `engine.selected_node` from `doc.selected`. Code tag `code-graph` · evidence: `widgets/graph-view/src/graph_view/nav.rs` (`glide_to`), `app/src/panels/code_graph/lens.rs` (`render_canvas` maps `doc.selected` → `engine.selected_node`)

### Governance & diff overlays

The spec engine's drift data and git's change data, rendered as *coloring* on the existing view —
data-coloring problems, not new-view problems. One toolbar **Overlay** dial picks what the node
fills encode: **Kind** (the original entity-kind palette), **Spec** (governance state), or **Diff**
(working-tree change status). The modes are mutually exclusive by design: all three compete for
the one fill channel, and layering two color encodings on one fill is unreadable — the dial makes
the active encoding explicit. Kind → shape (type = square) is constant across all modes. Overlay
switches are pure recolors: no relayout, just an explicit GPU paint-cache invalidation (fills are
baked into the cached affine batch).

- **Governance drift, directly on translucent edges.** The unified entity graph ([[spec:spec-graph-source]])
  makes a spec a real node and `Governs` a real edge, so spec governance shows WITHOUT a fill
  overlay: the repo-root `links.json` loads beside the adapter (lazily, `GovCache::ensure` —
  drift-checking fingerprints every linked body), folds each symbol's per-link reports to one state
  by severity (missing > drifted > ok) in `hiker_code::governance`, and each `Governs` edge takes
  that state's colour (**ok** green, **drifted** amber, **missing** red, **ungoverned** muted gray —
  `gov_color`). `Governs`/`Reference`/`Implements` edges draw **translucent** (low alpha) so the
  many-to-many spec fan-out recedes into a faint wash like the call edges, instead of an opaque
  saturated hairball; the drift hue is retained at low alpha, and selecting/hovering a spec still
  lights its edges bright via the highlight overlay. The numeric breakdown still appends to the
  summary line and the selected node's state + governing specs to the detail line. Node fill stays
  entity-kind throughout. The old Kind/Spec/Diff fill-overlay dial (`OverlayMode`,
  `Overlay::node_fill`) is gone. [code-graph-governance-overlay]
status:: done
implements:: [[code:hiker/panels/entity_graph/edge_color_for]], [[code:hiker/panels/code_governance/gov_color]], [[code:hiker/governance/impl#[Governance]build]], [[code:hiker/governance/classify]]
verifies:: [[code:hiker/governance/tests/classify_folds_by_severity]], [[code:hiker/governance/tests/build_rolls_up_drift_per_target]], [[code:hiker/panels/code_governance/tests/governance_palette_is_distinct]]
touches:: [[code:hiker/panels/code_graph]], [[code:hiker/governance]]
note:: reframed from a fill overlay to a direct edge colour per the "no overlays" direction. `Governs`/`Reference` edges draw at `GOV_EDGE_ALPHA`, `Implements` at `IMPL_EDGE_ALPHA` (both via `translucent`), so the spec fan-out is a faint wash; the highlight overlay lights a selected/hovered spec's edges bright. Governance loads at view build (`GovCache`) so the spec layer is present from the first frame; `Governs` colours come from `EntityGraphSource::edge_color`/`edge_color_for`. New symbols not in the current `.scip` snapshot — `implements::` resolves on the next index regen
- **Select to spotlight a footprint, with a configurable hop radius.** Selecting a node lights it +
  its footprint and dims the rest to faint context (the focus spotlight, [[spec:code-graph-spec-lighting]]
  in `spec-graph.md`): a SPEC lights itself + every entity it governs; a CODE node lights itself +
  its **1/2/3-hop** neighbourhood (configurable, default 1, set from the node right-click "Highlight
  N hops"). A spec is selected by clicking it, the find popup, the vault graph's spec → code jump
  (`code_graph::select_spec`), or a code node's "Select spec" menu entry. This stays in the full
  overview — it is NOT the filtered `Scope::Hops` drill ([[spec:code-graph-scope-hops]]). It replaces
  the old "light a spec" fill overlay: no dropdown, no whole-graph recolour mode. [code-graph-spec-lighting]
status:: done
implements:: [[code:hiker/panels/code_graph/select_spec]], [[code:hiker/panels/code_graph/lens/focus_set]], [[code:hiker/panels/entity_graph/impl#[EntityGraphSource]with_focus]]
touches:: [[code:hiker/panels/code_graph]], [[code:hiker/graph_view/edges]]
note:: the spotlight is `EntityGraphSource::with_focus` (dim-the-rest); `lens::focus_set` builds the lit index set — a spec's governed-in-display, or a code node's `doc.focus_hops`-bounded BFS over the display edges (`focus_hops == 1` = the historical direct-neighbour set). The hop radius is the shared `CodeGraphDoc.focus_hops`, set by `NodeAction::FocusHops(n)` (right-click "Highlight 1/2/3 hops", clamp 1–3) and persisted. Replaces the removed lighting machinery (`Overlay::light`/`lit_ids`, `pulse_lit`). New symbols not in the current `.scip` snapshot — resolves on the next index regen · evidence: `app/src/panels/code_graph/lens.rs` (`focus_set`, test `focus_set_code_node_bfs_widens_with_hop_radius`), `mod.rs` (`node_menu_ui` `FocusHops` arm)
- **`status::` badge.** A spec node whose `status::` is `planned`/`partial` carries a small violet
  dot on its top-right shoulder — its claims aren't fully landed. Statuses are scanned with
  `links.json` from the repo's `docs/` by the same `[slug]`-anchor association reconcile's link
  lines use. The badge is engine-level (`NodeDescriptor.badge`, Painter-drawn above GPU fills at
  the FULL LOD tier), set by `EntityGraphSource` from the spec node's `status`. [code-graph-status-badge]
status:: done
implements:: [[code:hiker/governance/doc_statuses]], [[code:hiker/governance/status_flagged]]
verifies:: [[code:hiker/governance/tests/doc_statuses_bind_to_nearest_anchor]], [[code:hiker/governance/tests/flagged_follows_planned_and_partial_statuses]]
touches:: [[code:hiker/panels/entity_graph]], [[code:hiker/graph_view/source]]
note:: moved from the old Spec-mode `Overlay::node_badge` to `EntityGraphSource::nodes` (the spec node's own status); shown on spec nodes whenever the graph is drawn
- **Open-bugs badge.** A node with a `manifests-in` edge from a non-struck `bug_tracking.md` row
  ([[spec:tracker-relation-links]], [[spec:tracker-open-is-not-struck]]) carries a hot-coral dot on
  the top-LEFT shoulder — the bug twin of the status badge (violet, top-right): an independent
  engine mark channel (`NodeDescriptor.bug_badge`, Painter-drawn at FULL LOD), fed by the
  governance rollup (`Governance::open_bugs_of`). Struck rows stop counting as open but keep their
  `verifies-fix` regression watch in drift. [code-graph-bug-badge]
status:: done
implements:: [[code:hiker/governance/impl#[Governance]open_bugs_of]], [[code:hiker/governance/impl#[Governance]build]]
touches:: [[code:hiker/panels/entity_graph]], [[code:hiker/graph_view/source]]
note:: now read directly in `EntityGraphSource::nodes` via `Governance::open_bugs_of` (the `Overlay::node_bug_badge` wrapper is gone); shown whenever the graph is drawn, not Spec-mode-gated
- **Change ring (direct, not a fill).** Toggling "Changes" rings each node by its **file's**
  HEAD-vs-worktree change status from `GitBackend::diff_paths` (via the vault git engine;
  vault-relative rows map onto the repo root), using the diff-summary palette (added green /
  modified blue / deleted red / renamed orange). The ring is the node's `resting_stroke` — fill
  stays entity-kind, so change layers over identity instead of replacing it (the "no overlays"
  direction). The change set loads on enable; the toggle is disabled (with a hint) when git isn't
  the vault transport. Refined to symbol grain by [[spec:code-graph-diff-symbol-level]]: within a
  `Modified` file a body-unchanged symbol rings dim, a body-changed one rings full. Change is also
  a lens predicate ("only changed", [[spec:spec-graph-lens]]). [code-graph-diff-coloring]
status:: done
implements:: [[code:hiker/panels/code_governance/impl#[Changes]ring]], [[code:hiker/panels/code_governance/impl#[Changes]load]], [[code:hiker/panels/code_governance/rows_to_repo]]
verifies:: [[code:hiker/panels/code_governance/tests/change_ring_and_touches_follow_refinement]], [[code:hiker/panels/code_governance/tests/rows_to_repo_strips_the_repo_prefix]]
touches:: [[code:hiker/git_sync]], [[code:hiker/panels/entity_graph]]
note:: reframed from a fill overlay mode to a direct node ring (`Changes::ring` → `NodeDescriptor.resting_stroke`) per the "no overlays" direction; the diff DATA generation is unchanged
- **Symbol-level diff coloring.** Within a `Modified` file, Diff mode distinguishes *which*
  symbols actually changed: the file's HEAD text (`show_at`, i.e. `git show HEAD:<path>`) and its
  working-tree text are both parsed whole with the same tree-sitter grammars the drift
  fingerprint uses; every definition-shaped node carrying a `name` field is hashed through the
  drift fingerprint's own token walk (comment/format-insensitive — drift generalized from "vs.
  baseline" to "vs. HEAD"); a symbol is *unchanged* only when its name's sorted fingerprint
  multiset is identical on both sides. Body-unchanged nodes ring the file's status color
  dimmed ("the file churned around it"); body-changed nodes ring full ([[spec:code-graph-diff-coloring]]). **Span-mapping
  design:** index spans are index-time, so the HEAD side is located by **name-anchored
  extraction**, never spans — a pure line move cannot misattribute by construction. **Failure
  direction is over-flag, never silently dim:** anything unprovable stays at the louder full
  file-grain color — same-name namesakes where any one body changed (the multiset differs, so
  the whole name flags), a name absent on either side (added / renamed / kinds the walk can't
  name-locate, e.g. Python module constants), files with no AST grammar, added/deleted/renamed
  paths (no comparable HEAD body), unreadable worktree text, or a parse failure. **Scoping the
  un-locatable-kinds claim honestly:** "kinds the walk can't name-locate read as changed" holds
  only when NO same-name fingerprinted definition exists in the file — the check is by name, so
  a non-fingerprinted symbol (e.g. a Python module constant) sharing its name with an unchanged
  fingerprinted definition inherits that name's identical multiset and **wrongly dims** even if
  the constant itself changed (the namesake under-flag hole; code fix deferred). Known scope
  cut: attribute/decorator-only edits sit outside the definition node on *both* sides, so they
  read as file churn (dim), not body change. The refinement runs synchronously on each switch
  the change set loads (one `show` + two parses per modified file), beside the diff-map build.
  [code-graph-diff-symbol-level]
status:: done
implements:: [[code:hiker/scip_adapter/symbol_changed_vs]], [[code:hiker/scip_adapter/impl#[ScipAdapter]changed_symbols_vs]], [[code:hiker/panels/code_governance/refine_symbol_diff]]
verifies:: [[code:hiker/scip_adapter/tests/symbol_changed_vs_flags_body_edits_not_formatting]], [[code:hiker/scip_adapter/tests/symbol_changed_vs_is_span_free_so_pure_moves_do_not_misattribute]], [[code:hiker/scip_adapter/tests/symbol_changed_vs_overflags_whenever_unprovable]], [[code:hiker/panels/code_governance/tests/change_ring_and_touches_follow_refinement]]
touches:: [[code:hiker/git_sync]], [[code:hiker/panels/code_governance]]
note:: new symbols not in the current `.scip` snapshot — the `implements::`/`verifies::` bodies resolve on the next index regen
- **Open diff from a node.** Right-clicking a code node opens a **menu** — per `interaction.md`
  [[spec:rightclick-menu-always]], replacing this surface's old direct "open source file" binding
  (the code-graph half of `bug-graph-node-right-click-not-menu`): **Open source** (the read-only
  code view), **Open diff vs HEAD** (enabled when git is the vault transport; greyed with the
  reason otherwise), **Copy symbol**, and a **Select spec** entry per governing spec (selecting it
  glows that spec's edges, [[spec:code-graph-spec-lighting]]). Open diff routes through the
  diff-summary panel's shared `open_diff_tab` into an editor tab with `DiffSource::GitRef` — the
  graph shows *where* the change is; the click drops into hunks. The menu is hosted in a popup at
  the latched pointer position (the engine owns its pane response, so `Response::context_menu`
  isn't available). [code-graph-open-diff-from-node]
status:: done
implements:: [[code:hiker/panels/code_governance/node_menu]], [[code:hiker/panels/code_graph/node_menu_ui]]
verifies:: [[code:hiker/panels/code_governance/tests/node_menu_offers_open_diff_and_select_spec]]
touches:: [[code:hiker/panels/git_diff]]

## Spec tooling (code-cli)

The CLI-side audit surfaces over the same engine — the in-app twins are the overlays above; the
reconcile / drift / ack workflow itself is documented in [status.md](status.md) §"Spec→code
links (drift tracking)".

- **Churn-vs-drift silence report.** `code-cli churn <index.scip> <repo_root> [--commits N]`
  (default 50) compares code churn against the drift signal per governed region, to expose
  silently under-watched code: each window commit's changed paths (vs its first parent, via
  `GitBackend::log` + `diff_paths`) map through the code graph's containment onto link targets
  (a target governs its subtree's files — the coverage report's propagate-down rule) and onto
  the specs holding those links. Per spec: commits touching its targets, drift *expected*
  (churned links) vs *observed* (links reading DRIFTED/MISSING now), and the finest churned
  altitude — the dial that explains a silence: `BLIND(Container)` over a hot file is the
  governed-but-blind smell ([[spec:spec-resolution-c4]]'s "audit the silence" leg);
  `BLIND(Code)` is the weak form (the file churned around the pinned body, or drift was acked).
  A second section lists in-index files with churn and **no governing spec** — the silence
  proper — sorted by churn; out-of-index paths are counted, never silently dropped. The heavy
  lifting lives in `hiker_code::churn` (unit-tested over real temp repos + synthetic links);
  drift is the store's *current* check against baselines, not per-commit history.
  [code-cli-churn-vs-drift]
status:: done
implements:: [[code:hiker/churn/churn_report]], [[code:hiker/churn/collect_window]]
verifies:: [[code:hiker/churn/tests/churn_report_flags_blind_specs_and_ungoverned_files]], [[code:hiker/churn/tests/collect_window_diffs_each_commit_against_its_parent]], [[code:hiker/churn/tests/churn_report_over_a_real_repo_window]]
note:: `hiker_code::churn` is a new module not in the current `.scip` snapshot — the `implements::`/`verifies::` bodies resolve on the next index regen

## Deferred / open questions

- **Strictly read-only vs. an "edit anyway" escape hatch** for non-`.md`. Recommendation: strictly read-only — it's the whole point.
- **Overlay polish**: persist the overlay mode in `CodeGraphViewState`; move the first
  governance load (a full drift pass, synchronous today) onto a background worker.
- **Embedding scope / cost controls** for large code vaults (per-folder opt-in, lazy-on-view, size caps).
- **Code embeds poorly with a prose model** — accept degraded code recall, or add a code-specialized embedder.
- **Nested `.git` dirs** — whether they need any handling beyond "hiker ignores them for its own versioning."
