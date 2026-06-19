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

The code graph is a third `graph_view::Source` beside the vault link graph and the cluster graph (`graph-view.md`); it renders through the same engine. The semantic structure (containment, drill-down) lives here; the *rendering* policy (label LOD by zoom, size-by-LOC) is specced in `graph-view.md` ([[spec:graph-label-lod]], [[spec:graph-size-by-loc]]).

- **`CodeGraph` Source.** `ScipAdapter::code_graph()` exposes the in-memory graph in render shape (`CodeGraph { nodes, edges }`, stable per-node index, external/local endpoints dropped, deterministic ordering); `app/src/panels/code_graph.rs::CodeGraphSource` maps entity kind → shape (type = square, else circle) + colour, and edges → index pairs. Verified headlessly: `widgets/graph-view/examples/code_graph_snapshot.rs` drives the real engine over a fixture `.scip` to a PNG. [code-graph-source]
status:: done
touches:: [[code:hiker/panels/code_graph]], [[code:hiker/scip_adapter]]
note:: third `graph_view::Source`: `CodeGraph { nodes, edges }`, kind → shape/colour, edges → index pairs; verified headlessly to PNG over a fixture `.scip` · evidence: `app/src/panels/code_graph.rs` (`CodeGraphSource`), `code-intel/hiker-code/src/scip_adapter.rs` (`code_graph()`), `widgets/graph-view/examples/code_graph_snapshot.rs`
- **The code-graph view panel.** The panel module that owns the surface: per-source `View` state lives on `AppState::panels.code_graph` keyed by `CodeSource::key()`, so flipping tabs keeps each project's layout (and its non-Clone adapter + background layout worker) warm; the file tree routes both openable shapes here (a `.scip` directly, a `hiker.kind: project` note); the first build seeds the global nav stack with the initial overview drill location, so a Back after the first drill returns to the overview rather than leaving the tab; and `apply_nav_target` is the pure (egui-free, unit-testable) restore side of Back/Forward. [code-graph-view-source]
status:: done
implements:: [[code:hiker/panels/code_graph/show]], [[code:hiker/panels/code_graph/apply_nav_target]]
touches:: [[code:hiker/panels/code_graph]], [[code:hiker/files/sidebar]]
note:: previously a comment-only slug (`// status:` tags with no doc anchor to land on) — the module doc, `show`'s nav-stack seeding, the sidebar's `.scip` / project-note routing, and `apply_nav_target` carry the tags · evidence: `app/src/panels/code_graph.rs` (module doc, `show`, `apply_nav_target`), `app/src/files/sidebar.rs` (open routing), `app/src/tab.rs` (`TabKind::CodeGraph`)
- **Open a project note or a `.scip` directly.** `TabKind::CodeGraph` carries a `CodeSource` enum (`Project(note) | Index(scip)`): opening a `hiker.kind: project` note routes here (mirroring `is_board_doc`), and the file tree can open a `.scip` directly (repo root defaults to the index's own directory). Both funnel through one `ScipAdapter::load`. [code-graph-tab]
status:: done
touches:: [[code:hiker/panels/code_graph]]
note:: opens a `hiker.kind: project` note or a `.scip` directly; both funnel through one `ScipAdapter::load` · evidence: `app/src/panels/code_graph.rs` (`TabKind::CodeGraph`, `CodeSource::{Project,Index}`), `workbench_host.rs` (`is_project_doc`)
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

- **Selection + scope dial.** Clicking a node (or a Find-popup pick) **selects** it — selection drives the detail line, the edge highlight, and the hops anchor; it never recenters the view by itself. A four-position toolbar **Scope** dial decides the display: **Overview** (the whole kind-filtered graph) or the selection's **undirected 1/2/3-hop neighbourhood** (`hop_mask`, a BFS over the symmetric adjacency of the *full* graph — hop distance is structural; the kind filter only decides what's drawn, and the anchor always shows even when its own kind is filtered out). The hop positions are disabled until a node is selected; a cleared/stale selection falls back to the overview display. Fresh-layout-on-anchor-change is the crucial bit: when the hops anchor changes, `rebuild_display` calls `engine.reset_layout_history()` so the small neighbourhood lays out fresh/compact rather than warm-seeding from the huge overview's positions; hop-count and filter tweaks keep the warm morph. A Find pick from the overview switches to 2-hop scope so the picked node is revealed even when the filter would hide it. [code-graph-scope-hops]
status:: done
implements:: [[code:hiker/panels/graph_nav/hop_mask]], [[code:hiker/panels/code_graph/rebuild_display]], [[code:hiker/panels/code_graph/apply_nav_target]]
touches:: [[code:hiker/panels/code_graph]]
note:: supersedes the click-to-drill focus mode ([[spec:code-graph-focus-neighbourhood]]): click selects, the scope dial drills — selection and scope are orthogonal · the scaffolding (BFS `hop_mask`, the scope dial, the Esc rung, scope persist strings) moved to the shared navigation layer ([[spec:graph-nav-extract]], `app/src/panels/graph_nav.rs`); this panel keeps the code-specific policy (collapse-through-filters, fresh-layout-on-anchor-change, SCIP kinds)
- **Focus-mode neighbourhood drill.** Clicking a node set `focus` and the display became its neighbourhood, with a separate hops selector and "← Overview" — drill and selection were entangled. [code-graph-focus-neighbourhood]
status:: superseded
note:: superseded by [[spec:code-graph-scope-hops]] — selection and scope split into orthogonal controls; the BFS neighbourhood machinery carries over as `hop_mask`
- **Code-graph back/forward.** Code-graph drills ride the **global** navigation stack (`NavTarget::CodeGraphNode`) over the two settle-triggering fields `(selected, scope)`. Toolbar `⟵`/`⟶` buttons (disabled at the ends), plus Alt+←/→ and mouse Extra1/Extra2 back/forward. Back/Forward restores replay through `apply_nav_target` → `rebuild_display` without re-recording (guarded by the `nav_restoring` flag / `nav.locked`). [code-graph-nav-history]
status:: done
touches:: [[code:hiker/panels/code_graph]]
note:: global-stack back/forward over `(selected, scope)`; restores replay through `apply_nav_target` without re-recording · evidence: `app/src/panels/code_graph.rs` (`nav_snapshot`, `nav_restoring`), `app/src/panels/graph_nav.rs` (`nav_controls`, shared with the vault graph per [[spec:graph-nav-extract]]), `app/src/state.rs` (`NavTarget::CodeGraphNode`), `app/src/editor_pane.rs` (restore arm)

### Governance & diff overlays

The spec engine's drift data and git's change data, rendered as *coloring* on the existing view —
data-coloring problems, not new-view problems. One toolbar **Overlay** dial picks what the node
fills encode: **Kind** (the original entity-kind palette), **Spec** (governance state), or **Diff**
(working-tree change status). The modes are mutually exclusive by design: all three compete for
the one fill channel, and layering two color encodings on one fill is unreadable — the dial makes
the active encoding explicit. Kind → shape (type = square) is constant across all modes. Overlay
switches are pure recolors: no relayout, just an explicit GPU paint-cache invalidation (fills are
baked into the cached affine batch).

- **Spec-governance overlay.** Spec mode loads the repo-root `links.json` beside the adapter
  (lazily, on the first switch — drift-checking fingerprints every linked body) and runs
  `check_drift`; each symbol's per-link reports fold to one state by severity (missing > drifted >
  ok) in `hiker_code::governance`, and the node fill colors it: **ok** green, **drifted** amber,
  **missing** red (linked but no longer fingerprintable), **ungoverned** muted gray — the
  ungoverned share of the codebase becomes a literally visible mass, with the numeric breakdown
  appended to the summary line and the selected node's state + governing specs on the detail line.
  The toggle is disabled (with a hint) when the repo has no `links.json`. [code-graph-governance-overlay]
status:: done
implements:: [[code:hiker/panels/code_governance/toolbar_section]], [[code:hiker/panels/code_governance/impl#[Overlay]node_fill]], [[code:hiker/governance/impl#[Governance]build]], [[code:hiker/governance/classify]]
verifies:: [[code:hiker/governance/tests/classify_folds_by_severity]], [[code:hiker/governance/tests/build_rolls_up_drift_per_target]], [[code:hiker/panels/code_governance/tests/governance_palette_is_distinct]]
touches:: [[code:hiker/panels/code_graph]], [[code:hiker/governance]]
note:: per-view, loaded once per panel build (a re-open re-checks drift); the engine grew `invalidate_paint_cache` for recolors. Overlay mode is session-only (not in `CodeGraphViewState`) — persistence deferred
- **Spec lighting.** In Spec mode, a toolbar dropdown (or a node menu's "Light spec" entry —
  every governed node lists its specs there) lights one spec: its `implements`/`touches` targets
  plus their 1-hop blast radius via the adapter's `neighbors` stay at full strength while every
  other fill dims toward the background, and the lit nodes get a one-shot fluid pulse
  (`State::pulse_nodes` injects energy into [[spec:graph-hover-fluid]]'s field, so the lighting
  *drains* across the graph rather than blinking). `verifies` targets don't seed the lighting —
  it shows where a spec lives, not what vouches for it. [code-graph-spec-lighting]
status:: done
implements:: [[code:hiker/governance/impl#[Governance]lighting]], [[code:hiker/panels/code_governance/impl#[Overlay]light]], [[code:hiker/panels/code_graph/pulse_lit]], [[code:hiker/graph_view/impl#[State]pulse_nodes]]
verifies:: [[code:hiker/governance/tests/lighting_is_targets_plus_blast_radius]], [[code:hiker/graph_view/tests/pulse_tests/pulse_nodes_injects_energy_where_fluid_is_on]]
touches:: [[code:hiker/panels/code_governance]]
note:: the steady signal is the dimmed-fill contrast; the pulse is the "where did it go" moment. Lit members hidden by the kind filter don't pulse (their visible ancestor carries structure, not the spec claim)
- **`status::` badge.** Nodes whose governing spec entry is `status:: planned`/`partial` carry a
  small violet dot on the node's top-right shoulder — a separate channel from the governance fill
  (status is "are the spec's claims landed", not "did the code drift"). Statuses are scanned once
  with `links.json` from the repo's `docs/` by the same `[slug]`-anchor association reconcile's
  link lines use, so the two can't disagree on what an anchor is. The badge is engine-level
  (`NodeDescriptor.badge`, Painter-drawn above GPU fills at the FULL LOD tier) and shows in Spec
  mode only. [code-graph-status-badge]
status:: done
implements:: [[code:hiker/governance/doc_statuses]], [[code:hiker/panels/code_governance/impl#[Overlay]node_badge]]
verifies:: [[code:hiker/governance/tests/doc_statuses_bind_to_nearest_anchor]], [[code:hiker/governance/tests/flagged_follows_planned_and_partial_statuses]]
touches:: [[code:hiker/graph_view/source]], [[code:hiker/graph_view/edges]]
- **Open-bugs badge.** Nodes with a `manifests-in` edge from a non-struck `bug_tracking.md` row
  ([[spec:tracker-relation-links]], [[spec:tracker-open-is-not-struck]]) carry a hot-coral dot on
  the top-LEFT shoulder in Spec mode — the bug twin of the status badge (violet, top-right): an
  independent engine mark channel (`NodeDescriptor.bug_badge`, Painter-drawn at FULL LOD), fed by
  the same governance rollup pass (`links.json` + the tracker's struck-row scan, loaded together).
  The selected node's detail line appends "N open bugs: <slugs>". Struck rows stop counting as
  open but keep their `verifies-fix` regression watch in drift; bug slugs stay out of the spec
  channels (no "Light spec" menu entries, since bugs have no lighting targets). [code-graph-bug-badge]
status:: done
implements:: [[code:hiker/governance/impl#[Governance]build]], [[code:hiker/panels/code_governance/impl#[Overlay]detail_fragment]]
touches:: [[code:hiker/graph_view/source]], [[code:hiker/graph_view/edges]]
note:: `Governance::open_bugs_of` / `Overlay::node_bug_badge` / `governance::struck_bug_rows` and the rollup tests (`open_bugs_rollup_counts_non_struck_manifests_in`, `struck_bug_rows_collects_only_struck_slugs`) are new symbols not in the current `.scip` snapshot — point `implements::`/`verifies::` at them on the next index regen
- **File-level diff coloring.** Diff mode colors each node by its **file's** HEAD-vs-worktree
  change status from `GitBackend::diff_paths` (via the vault git engine; vault-relative rows map
  onto the repo root), using the diff-summary panel's status palette (added green / modified blue
  / deleted red / renamed orange) with unchanged files as the same muted mass. The map reloads on
  every switch into the mode so it tracks the working tree; the toggle is disabled (with a hint)
  when git isn't the vault transport. Refined to symbol grain by
  [[spec:code-graph-diff-symbol-level]]: within a `Modified` file, body-unchanged symbols dim to
  a quieter tone of the same status color. [code-graph-diff-coloring]
status:: done
implements:: [[code:hiker/panels/code_governance/rows_to_repo]]
verifies:: [[code:hiker/panels/code_governance/tests/rows_to_repo_strips_the_repo_prefix]]
touches:: [[code:hiker/git_sync]], [[code:hiker/panels/git_diff]]
- **Symbol-level diff coloring.** Within a `Modified` file, Diff mode distinguishes *which*
  symbols actually changed: the file's HEAD text (`show_at`, i.e. `git show HEAD:<path>`) and its
  working-tree text are both parsed whole with the same tree-sitter grammars the drift
  fingerprint uses; every definition-shaped node carrying a `name` field is hashed through the
  drift fingerprint's own token walk (comment/format-insensitive — drift generalized from "vs.
  baseline" to "vs. HEAD"); a symbol is *unchanged* only when its name's sorted fingerprint
  multiset is identical on both sides. Body-unchanged nodes render the file's status color
  dimmed ("the file churned around it"); body-changed nodes keep full color. **Span-mapping
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
  into Diff mode (one `show` + two parses per modified file), beside the diff-map reload.
  [code-graph-diff-symbol-level]
status:: done
implements:: [[code:hiker/scip_adapter/symbol_changed_vs]], [[code:hiker/scip_adapter/impl#[ScipAdapter]changed_symbols_vs]], [[code:hiker/panels/code_governance/refine_symbol_diff]]
verifies:: [[code:hiker/scip_adapter/tests/symbol_changed_vs_flags_body_edits_not_formatting]], [[code:hiker/scip_adapter/tests/symbol_changed_vs_is_span_free_so_pure_moves_do_not_misattribute]], [[code:hiker/scip_adapter/tests/symbol_changed_vs_overflags_whenever_unprovable]], [[code:hiker/panels/code_governance/tests/diff_fill_dims_only_proven_unchanged_bodies]]
touches:: [[code:hiker/git_sync]], [[code:hiker/panels/code_governance]]
note:: new symbols not in the current `.scip` snapshot — the `implements::`/`verifies::` bodies resolve on the next index regen
- **Open diff from a node.** Right-clicking a code node opens a **menu** — per `interaction.md`
  [[spec:rightclick-menu-always]], replacing this surface's old direct "open source file" binding
  (the code-graph half of `bug-graph-node-right-click-not-menu`): **Open source** (the read-only
  code view), **Open diff vs HEAD** (enabled when the Diff overlay knows the file changed; greyed
  with the reason otherwise), **Copy symbol**, and a **Light spec** entry per governing spec.
  Open diff routes through the diff-summary panel's shared `open_diff_tab` into an editor tab
  with `DiffSource::GitRef` — the graph shows *where* the change is; the click drops into hunks.
  The menu is hosted in a popup at the latched pointer position (the engine owns its pane
  response, so `Response::context_menu` isn't available). [code-graph-open-diff-from-node]
status:: done
implements:: [[code:hiker/panels/code_governance/node_menu]], [[code:hiker/panels/code_graph/node_menu_ui]]
verifies:: [[code:hiker/panels/code_governance/tests/node_menu_offers_open_diff_and_light_spec]]
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
