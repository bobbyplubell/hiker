# Graph view

One spatial graph engine drives every node-link surface in hiker: the vault-wide `Graph` tab (`design.md` App-shell) and the cluster / cluster-review graphs. The engine (`hiker-graph-view` crate, `widgets/graph-view/`) owns pan/zoom, layout, the eye-icon view menu, and the node/edge/label/hover/preview paint loop; each caller plugs its own data in through a `Source` trait. The layout math is egui-free in `hiker-graph` (`hiker-render/graph/`), wrapped for egui by the `graph-widgets` façade (`widgets/graph-widgets/`).

The headline decisions:

- **One engine, many sources.** `graph_view::State` carries all view state and the paint loop; a caller implements `Source` to turn its domain data (a `petgraph` vault graph, a slice of cluster `EditableNode`s) into per-frame node descriptors, edge pairs, and a layout tree. One code path renders both views with different colors and options. [graph-view-source-trait]
status:: done
touches:: [[code:hiker/graph_view]], [[code:hiker/panels/cluster_graph]], [[code:hiker/panels/graph]]
note:: one engine, many sources: caller turns its data into node descriptors / edges / layout tree; drives vault Graph tab + cluster graphs · evidence: `widgets/graph-view/src/graph_view/source.rs` (`Source`), `app/src/panels/graph.rs`, `app/src/panels/cluster_graph.rs`
- **Layout math is egui-free.** Force-directed (ForceAtlas2 + Barnes–Hut worker), the radial/vertical/horizontal tree layouts, and the layered (dagre/Sugiyama) port all live in `hiker-graph` with their own `Vec2`; `graph-widgets` is the thin egui façade converting at the boundary. [graph-layout-egui-free]
status:: done
note:: force/tree/dagre layout math is egui-free with its own `Vec2`; `graph-widgets` is the thin egui façade converting at the boundary · evidence: `hiker-render/graph/src/lib.rs` (`hiker-graph`), `widgets/graph-widgets/src/lib.rs`
- **Re-clustering and vault rebuilds morph, they don't reshuffle.** Force layouts carry stable node identity across rebuilds: a retained node keeps and is tethered to its prior position by an anchor spring, so the layout settles into the same shape with the changed parts moved, instead of scattering fresh every time. [force-node-identity, force-anchor-springs]
- **Display-engine controls live in the view/eye menu.** Layout kind, projection mode and config, node/edge/label sizes, and anchor stiffness are surfaced from the surface's eye-icon menu; clustering options stay confined to the clustering engine's own params. [view-menu-display-controls]
status:: done
touches:: [[code:hiker/graph_view]]
note:: eye-menu owns display-engine controls (layout kind, projection, sizes, anchor stiffness); clustering options stay on the clustering engine · evidence: `widgets/graph-view/src/graph_view/mod.rs` (`view_options_menu`)
- **Projection modes are a lens over the same layout.** Fisheye and Poincaré-disk navigation apply to every graph consumer at once; the seam and its controls are specced in `projection.md`. [proj-graph-mode]


## The Source seam

`Source` is the caller-supplied bridge from domain data to the engine (`graph_view/source.rs`). The engine never knows whether it's drawing the vault link graph or a cluster tree. [graph-view-source-trait]

| Method | Purpose |
| ------ | ------- |
| `node_count` | total nodes (length of the positions vector); includes hidden nodes so edge/layout indices stay stable |
| `nodes(positions, style)` | visible node descriptors for the frame; reads `positions[i]` per node |
| `edges` | edges as `positions`-index pairs — used both for drawing and as the force-worker topology |
| `layout_tree(kind)` | spanning/parent tree for the tree layouts (vault graph spans per kind; cluster graph uses its parent tree) |
| `preview_for(index)` | `(title, body)` for the hover-preview card |
| `node_key(index)` | stable per-node identity for temporal stability (below); default `None` opts out |
| `edge_color(index)` | per-edge stroke color override; default `None` keeps the style's single edge color (the vault graph's typed edges supply per-kind hues) |

The vault graph (`app/src/panels/graph.rs`) and the cluster graph (`app/src/panels/cluster_graph.rs`) each implement it over their own storage.


## Layout kinds

Selected from the view menu; switching kind triggers a relayout (`recompute_layout`).

- **Force-directed** — ForceAtlas2 over a Barnes–Hut quadtree, run on a background worker (`hiker-graph::LayoutWorker`) so the layout settles live without blocking the UI; positions are snapshotted into the view each frame until convergence. [graph-force-layout]
status:: done
touches:: [[code:hiker/force]], [[code:hiker/force_layout]]
note:: ForceAtlas2 + Barnes–Hut on a background worker; positions snapshotted into the view each frame until convergence · evidence: `hiker-render/graph/src/force.rs` (`LayoutWorker`), `widgets/graph-widgets/src/force_layout.rs`
- **Radial / vertical / horizontal tree** — pure deterministic tree layouts over the `Source`'s `layout_tree`, computed inline (no worker). [graph-tree-layouts]
status:: done
touches:: [[code:hiker/tree]]
note:: pure deterministic tree layouts over the `Source`'s `layout_tree`, computed inline (no worker) · evidence: `hiker-render/graph/src/tree.rs` (radial / vertical / horizontal)
- **Layered (dagre)** — the Sugiyama/dagre port (`hiker-graph::LayeredEngine`, `layered/`), via the `graph-widgets::layered_layout` façade. Treated as directed; ranks flow per the view menu's rank-direction selector. [graph-layered-layout]
status:: done
note:: dagre/Sugiyama layered layout via the façade; directed; rank direction from the view menu · evidence: `hiker-render/graph/src/layered/`, `widgets/graph-widgets/src/lib.rs` (`layered_layout`)
  - **Routed edges.** The layered layout is the only kind that returns poly-line edge routes (orthogonal routing between ranks); the engine stores them in `State::edge_routes` and the paint loop draws them as routes. Every other kind clears the routes and draws edges as straight segments. [graph-routed-edges]
status:: done
touches:: [[code:hiker/graph_view]]
note:: layered layout returns orthogonal poly-line edge routes; other kinds clear routes and draw straight segments · evidence: `widgets/graph-view/src/graph_view/mod.rs` (`State::edge_routes`)


## Temporal layout stability

Force layouts are intrinsically chaotic — a small change to the graph re-randomizes the whole picture. The engine keeps the force layout *stable across rebuilds* so a re-cluster or vault-graph rebuild reads as the same shape with the changed parts moved, not a fresh scatter.

- **Stable node identity.** `Source::node_key(index)` returns a key that survives a rebuild. Each frame the engine captures the live positions into `prev_positions` keyed by that identity, so the next rebuild can map old positions onto the new graph. [force-node-identity]
status:: done
touches:: [[code:hiker/graph_view]]
note:: stable per-node identity captured each frame so a rebuild maps old positions onto the new graph (morph, not reshuffle) · evidence: `widgets/graph-view/src/graph_view/source.rs` (`Source::node_key`), `mod.rs` (`prev_positions`)
- **Warm seed + anchor springs.** On a same-kind force rebuild that has captured history, the engine builds a warm seed (`build_warm_seed`): a retained node seeds at its prior position and is tethered there by a weak anchor spring; the solver re-converges from that warm seed (`spawn_anchored` / `force_to_convergence_anchored`), so the layout morphs coherently. The anchor spring's strength is the **anchor stiffness** — `0.0` lets nodes settle freely (the un-anchored behavior), higher values hold them closer to where they were. [force-anchor-springs]
status:: done
touches:: [[code:hiker/force]], [[code:hiker/graph_view]]
note:: same-kind force rebuild warm-seeds + tethers retained nodes by anchor springs; re-converges from the warm seed · evidence: `widgets/graph-view/src/graph_view/layout.rs` (`build_warm_seed`), `hiker-render/graph/src/force.rs` (`force_to_convergence_anchored`)
- **Structural-diff spawn for new nodes.** A node whose key is new (no prior position) is *not* anchored; it seeds at the centroid of its already-placed neighbours (plus deterministic jitter), so an added node appears next to where it belongs rather than across the canvas. A node with no placed neighbour falls back to the deterministic scatter. [force-structural-diff]
status:: done
touches:: [[code:hiker/graph_view]]
note:: a new (unkeyed) node seeds at its placed-neighbour centroid + jitter (unanchored), else deterministic scatter · evidence: `widgets/graph-view/src/graph_view/layout.rs` (`build_warm_seed`)
- **Seeded, deterministic scatter.** The fresh-scatter seed (first build, kind switch, or unplaceable new node) is a deterministic hash-based pseudo-random scatter (`scatter` / `scatter_point`, a splitmix-style mixer over the node index), so a layout is reproducible and headless snapshots are stable. [force-seeded-rng]
status:: done
touches:: [[code:hiker/graph_view]]
note:: deterministic hash-based scatter (splitmix-style over node index) for fresh seeds, so layouts/snapshots reproduce · evidence: `widgets/graph-view/src/graph_view/layout.rs` (`scatter`, `scatter_point`)
- **Live-stable re-clustering.** The cluster-review graph's live preview re-runs the structural pass on a debounced config change; because the cluster `Source` supplies a stable `node_key` (the cluster node id), each debounced rebuild morphs from the prior layout instead of jumping, so tuning a slider reads as the clusters reorganizing smoothly. The camera is preserved across the re-seed. [cluster-config-live-stable]
status:: done
touches:: [[code:hiker/clusters/panel]], [[code:hiker/panels/cluster_graph]]
note:: live re-clustering morphs from the prior layout via the cluster `node_key`; camera preserved across the re-seed · evidence: `app/src/panels/cluster_graph.rs` (`node_key`), `app/src/clusters/panel/mod.rs` (debounced live preview)

A fresh scatter and an un-anchored solve remain the fallback whenever no history exists or the source opts out of `node_key` — identical to the pre-anchor behavior.


## Display controls in the view menu

The eye-icon view-options menu (`State::view_options_menu`) is where display-engine controls live — distinct from clustering options, which stay on the clustering engine's own config. [view-menu-display-controls]

- **Layout kind + rank direction** picker, returning whether a relayout is needed.
- **Anchor stiffness** slider (force-directed only) governing the temporal-stability morph: low = lively/free re-settle, high = nodes hold their prior positions tightly. [force-cfg-anchor-stiffness]
status:: done
touches:: [[code:hiker/graph_view]]
note:: view-menu slider (force-directed only): low = lively re-settle, high = nodes hold prior positions · evidence: `widgets/graph-view/src/graph_view/mod.rs` (`anchor_stiffness`, `view_options_menu`)
- **Node / edge / label size** sliders and the common toggles (labels, edges, hover preview).
- **Projection** mode selector and its live config sub-menu (`projection.md`).


## Find and view persistence

Cross-surface affordances the vault graph tab and the code graph share: a keyboard "find / jump to node" picker, and view state that survives a tab close and a restart.

- **Find / jump to node (Ctrl+F).** Both graph surfaces open the shared standalone autocomplete picker over their full node list — note paths on the vault graph, every loaded entity (not just the drawn scope) on the code graph. A pick navigates exactly like a node click: the vault graph opens the note through the same DRIVE/open routing (and, in hops scope, drills like a click — [[spec:graph-nav-extract]]), the code graph selects the node and — from the overview — switches to 2-hop scope so the pick is revealed even when the kind filter or collapse would hide it (and the jump rides Back/Forward). Independent of the editor's Ctrl+F — the binding is read only while a graph tab is showing. [graph-find-popup]
status:: done
implements:: [[code:hiker/panels/graph/find_popup]], [[code:hiker/panels/code_graph/find_popup]]
touches:: [[code:hiker/panels/graph_find]]
note:: previously a comment-only slug (`// status:` tags with no doc anchor); Ctrl+F + the toolbar Find button open the shared `PickerState` over `VaultNodeFindSource` / `CodeNodeFindSource`, and a pick funnels into the click path · evidence: `app/src/panels/graph.rs` (`find_popup`), `app/src/panels/code_graph.rs` (`find_popup`, overview pick → `Scope::Hops(2)`), `app/src/panels/graph_find.rs`
- **View state persists across tab close and restart.** Each graph surface snapshots its engine view (positions, pan/zoom, projection, display toggles, LOD) plus its own display controls into the session maps — the vault graph under the singleton `:graph` key, each code graph under its `CodeSource::key()` with scope, selection, hidden kinds, edge/orphan toggles, and size-by-LOC — captured into the tab-state snapshot on exit (by panel presence, since both panels outlive their tabs) and restored once on the first render/build (`view_restored` guard). A restored view warm-seeds the layout before the solver runs and suppresses the fresh-build auto-fit, so the graph opens where the user left it instead of re-scattering. Serialized through `hiker_core::autosave` (`GraphViewState` / `CodeGraphViewState`). [graph-view-state-persist]
status:: done
implements:: [[code:hiker/panels/graph/capture_graph_view]], [[code:hiker/panels/graph/snapshot_to_view_state]], [[code:hiker/panels/graph/view_state_to_snapshot]], [[code:hiker/panels/code_graph/capture_code_graph_view]], [[code:hiker/panels/code_graph/apply_persisted_view]]
touches:: [[code:hiker/autosave/GraphViewState]], [[code:hiker/autosave/CodeGraphViewState]]
note:: previously a comment-only slug; the vault graph restores inline in `install_and_layout` (seed before layout so the force solver morphs onto the saved shape, then clear `needs_fit`), the code graph via `apply_persisted_view` on first render (`scope_persist_str` round-trips the scope enum) · evidence: `app/src/panels/graph.rs` (`capture_graph_view`, `install_and_layout`), `app/src/panels/code_graph.rs` (`capture_code_graph_view`, `apply_persisted_view`), `core/src/autosave.rs` (`GraphViewState`, `CodeGraphViewState`), `app/src/main.rs` / `app/src/bootstrap.rs` (snapshot save / session-map restore)


## Corner minimap

A first-class corner minimap is engine chrome a host insets in a pane corner: a locked-Poincaré overview of a `Source`, click-to-expand to fill the pane (~0.35s eased swap), with a viewport-location indicator and a swap-back focus the host acts on. It owns **no engine** — every render borrows one through a single seam, so there is no separate minimap-owned layout/cache.

- **Borrowed-engine render seam (`Minimap::ui_for`).** The minimap is pure chrome (corner placement, the expand/collapse swap, the indicator mode, the persistent overview nav, the click `Output`); it renders through a BORROWED `State` engine the host hands it — a peer host passes its secondary view's engine, a self-overview host (the canvas, with no peer engine) passes an engine from `Minimap::overview_engine` whose `positions` it set to its node positions for the frame. The borrowed engine's positions ARE the overview (never re-laid-out here). The minimap SAVES the engine's main-view chrome (projection / nav / disk-zoom / fit / boundary / labels / preview / background / label-pill), installs the locked-Poincaré overview look + its OWN persistent overview nav, renders, then RESTORES — so the same engine still renders faithfully when it's the full-size primary after a swap. It also runs the engine's constant-font label budget LOD ([[spec:graph-label-budget]]) when the host opts into labels (`set_labels`), so a labelled overview (the code graph's spec minimap) caps the small corner to the top handful (specdoc containers + a few top specs). [graph-minimap-chrome]
status:: done
touches:: [[code:hiker/graph_view/minimap]]
note:: `Minimap` owns no engine — `ui_for(ui, host_rect, &mut engine, &source, viewport_world)` is the sole render path; `SavedEngineView` save/restore keeps the borrowed engine faithful as the swapped-in primary; `overview_nav`/`overview_zoom`/`overview_needs_fit` persist the corner navigation on the chrome, not the engine. `BrightenVisible` / `ShowViewport` indicator modes; collapse reports `focused_on_collapse`. Drives the code-graph peer minimap ([[spec:spec-minimap-swap]]) and the canvas self-overview ([[spec:canvas-minimap]]). Code tags `container-tab` / `canvas-minimap` · evidence: `widgets/graph-view/src/graph_view/minimap.rs` (`Minimap::ui_for`, `overview_engine`, `set_labels`, `SavedEngineView`)


## Typed vault graph

The vault `Graph` tab is a navigator of vault *structure*, not just prose links: the build unions typed edge sets from the store's derived tables and types every node from its `hiker.kind` (against the kind registry's shapes) plus the spec-anchor index, so boards, trails, PM containers, and spec notes appear as connected typed nodes instead of disconnected plain notes. Data enrichment only — no engine/layout work; the pure union, classification, scope, and LOD logic live in `app/src/panels/graph_data.rs`, apart from the egui panel.

- **Typed edges from the derived tables.** The vault graph build unions five edge sets, each tagged with an edge kind: **wikilink** (today's body-link scan, unchanged — duplicates and all, though `[[code:…]]` targets are excluded: a code symbol isn't a vault node, and letting one fall through to basename resolution would forge an edge to an unrelated note sharing the symbol's leaf name), **board-membership** (board-doc → card note, straight from the indexed `board_cards` table — no re-parse; SPRINTS ride this same kind, since a sprint is a board-doc deriving the same rows — one mechanism, one legend row, judged against a separate sprint-membership visual kind), **trail-membership** (trail-doc → the waypoint's SOURCE note from `trail_waypoints`: under path-as-identity the row's `trail_id` is the trail-doc's path and `source_path` the real captured note; the `waypoint_path` companion-snapshot pointer gets no edge), **list-membership** (list-doc → member note from the shape-generic `list_refs` table — epic → story, plan → epic/sprint/backlog, and any registered list-like kind; Phase D of the graph-unification plan), and **spec references** ([[spec:vault-graph-spec-edges]] below). Membership/spec edges dedupe per (doc, note, kind) pair; rows referencing paths outside the walk are skipped; freeform cards have no note ref, no derived row, and so no edge. The `Assembler` union point remains the only place that grows. [vault-graph-typed-edges]
status:: done
implements:: [[code:hiker/panels/graph_data/Assembler]], [[code:hiker/store/boards/impl#[Store]all_board_cards]], [[code:hiker/store/lists/impl#[Store]all_list_refs]]
verifies:: [[code:hiker/panels/graph_data/tests/union_builds_typed_edges_from_all_three_sources]], [[code:hiker/panels/graph_data/tests/union_adds_list_membership_and_spec_reference_edges]], [[code:hiker/store/tests/all_board_cards_spans_every_board]], [[code:hiker/store/tests/all_list_refs_spans_every_list]]
touches:: [[code:hiker/panels/graph]]
note:: graph-unification-plan §1 Phases A+D; the app-side `Builder` walks the vault + reads `notes_with_meta` / `all_board_cards` / `all_trail_waypoints` / `all_list_refs` / `all_spec_anchors` under one store lock and feeds the pure `Assembler`; list edges take the epic hue (plans and epics share the one `list_refs` mechanism, so the edge kind carries the canonical container's color) · evidence: `app/src/panels/graph_data.rs` (`Assembler`), `app/src/panels/graph.rs` (`Builder::build_data`), `core/src/store/{boards,lists}.rs` (`all_board_cards`/`all_list_refs`)
- **Edge kind → color + visibility toggle.** Each edge kind draws in its own color — wikilinks keep the style's (user-editable) edge color; board/trail membership edges take their container kind's theme hue — through the new `Source::edge_color` per-edge override, and the toolbar carries one color-keyed toggle per kind PRESENT in the data (the toolbar doubles as the edge legend; a vault with no boards offers no dead toggle), mirroring the code view's calls/implements toggles. An edge draws only when its kind is on AND both endpoints are visible, so a membership edge never dangles into a hidden node; toggling re-lays-out (the filters shape the drawn topology). [vault-graph-edge-toggles]
status:: done
implements:: [[code:hiker/panels/graph/filter_controls]], [[code:hiker/panels/graph_data/visible_edges]], [[code:hiker/graph_view/source/Source#edge_color]]
verifies:: [[code:hiker/panels/graph_data/tests/visible_edges_honor_toggles_and_endpoints]]
note:: hidden edge kinds + the other vault filters persist on the existing `GraphViewState` record ([[spec:graph-view-state-persist]]) as HIDDEN-kind lists, so a kind first appearing after a rebuild defaults to visible (the code graph's `hidden_kinds` posture) · evidence: `app/src/panels/graph.rs` (`capture_graph_view`, `install_and_layout`), `core/src/autosave.rs` (`hidden_edge_kinds`/`hidden_node_kinds`/`detail`)
- **Typed nodes from `hiker.kind`.** Every node classifies off the `note_meta` index (one `notes_with_meta` query, never a per-note disk read), read against the kind REGISTRY'S SHAPES rather than a hardcoded PM name list: the machinery discriminators (board/trail/query) first, then any registered board-like kind classifies as a sprint-class container, any registered list-like kind as plan (the name-special root, `core::pm::PLAN_KIND`) or epic-class (user list-likes bucket with epics — the shape is what makes a note structural), the registered `story`/`task` leaf pair as typed work LEAVES (hued circles — never square, never coarse-level), and spec-anchor definition promotes an otherwise-plain note to a spec node ([[spec:vault-graph-spec-edges]]). Other registered leaf kinds are typed plain notes with no structural role and render plain; an UNREGISTERED `hiker.kind` value (waypoints, sessions, a disabled `epic` entry) classifies plain too — the registry, not the name, is the source. Containers get the square shape + larger `label_scale` treatment, hued per kind from the theme (`hiker_theme::kind_*`); plain notes keep the flat (user-editable) palette, exactly as before. [vault-graph-kind-nodes]
status:: done
implements:: [[code:hiker/panels/graph_data/VaultKind]]
verifies:: [[code:hiker/panels/graph_data/tests/kind_classification_reads_registry_shapes]], [[code:hiker/panels/graph_data/tests/kind_classification_spec_promotion_is_the_weaker_signal]]
touches:: [[code:hiker/panels/graph]]
note:: container hue is shared by the node fill, its membership edges, and the toolbar filter label, so one color means one kind everywhere; Phase D added plan/epic/sprint/story (+ spec from Phase E) to the Phase A board/trail/query set, and the kind filter + Containers detail dial pick the new kinds up data-driven with zero new wiring · evidence: `app/src/panels/graph.rs` (`VaultSource::nodes`, `Builder::indexed_inputs`), `app/src/panels/graph_data.rs` (`VaultKind::classify`), `hiker-theme/src/lib.rs` (`kind_*`)
- **Kind filters.** The vault analogue of the code view's entity-type filters: one toolbar toggle per node kind present in the data (Boards / Trails / Queries / Notes — auto-populated, nothing hardcoded), color-keyed to the nodes. Hiding a kind hides its nodes and every edge into them; indices stay stable so hidden nodes keep their layout slots, and the user's offs survive a rebuild while first-appearing kinds default to visible. [vault-graph-kind-filters]
status:: done
implements:: [[code:hiker/panels/graph_data/kind_filter_for]], [[code:hiker/panels/graph_data/visible_nodes]], [[code:hiker/panels/graph_data/merge_filter]]
verifies:: [[code:hiker/panels/graph_data/tests/filters_autopopulate_and_merge_keeps_choices]]
note:: same data-driven pattern + widgets as `code-graph-kind-filters`; rendered by the shared `filter_controls` toolbar section · evidence: `app/src/panels/graph.rs` (`filter_controls`)
- **Detail (LOD) dial: Containers / Everything.** A two-position toolbar dial: the coarse level shows container kinds only (the vault's "Objects" analogue — the structural skeleton of boards/trails/queries), "Everything" shows all notes. It falls straight out of the kind map — one `Detail::shows(kind)` predicate folded into the same visibility mask as the kind filter, no new LOD machinery (the engine's magnification LOD tiers are untouched). [vault-graph-lod-containers]
status:: done
implements:: [[code:hiker/panels/graph_data/Detail]]
verifies:: [[code:hiker/panels/graph_data/tests/detail_levels_map_through_kinds]], [[code:hiker/panels/graph_data/tests/detail_persist_round_trip]]
note:: persists as a string discriminant on `GraphViewState.detail`; junk/pre-feature empty falls back to Everything · evidence: `app/src/panels/graph_data.rs` (`Detail`)

### Spec notes & drift badges

Phase E of the graph-unification plan, in its honest in-vault form. The plan's full vision assumed spec-KIND notes; the vault registry ships no spec kind (the built-ins are the PM set), so the in-vault discriminator is the spec-anchor index — a note defining `[slug]` anchors IS the vault's spec note. Spec → *code* edges stay out by design: code symbols aren't vault nodes; the two graphs bridge by navigation.

- **Spec edges + spec nodes.** Notes defining `[slug]` anchors classify as spec containers (square, `hiker_theme::kind_spec` hue, their own filter row — an explicit machinery/registry kind always wins over the promotion), and every `[[spec:slug]]` body link adds a deduped Spec edge to the anchor's defining note, resolved through the `spec_anchors` index with the editor click's pick rule (referrer's folder first, else lexicographic first — the edge and the click can never disagree). Self-references and unresolved slugs add nothing. [vault-graph-spec-edges]
status:: partial
implements:: [[code:hiker/panels/graph_data/impl#[Assembler]add_wikilinks]], [[code:hiker/store/spec_anchors/impl#[Store]all_spec_anchors]]
verifies:: [[code:hiker/panels/graph_data/tests/union_adds_list_membership_and_spec_reference_edges]], [[code:hiker/panels/graph_data/tests/spec_anchor_pick_prefers_the_referrers_folder]], [[code:hiker/store/tests/all_spec_anchors_spans_every_note]]
note:: partial, two gaps named: (a) no registered spec KIND exists — when a vault-side spec kind / spec-engine vault integration lands, classification should read it instead of (or before) the anchor-index promotion; (b) the plan bullet's "edges to the notes they govern" has no substrate — the link store links specs to CODE targets only, so no note-governs-note edge exists to derive, and inventing one would be fiction · evidence: `app/src/panels/graph_data.rs` (`pick_spec_anchor`, `VaultKind::classify`), `core/src/store/spec_anchors.rs`

- **Drift badge + jump to the code graph.** A toolbar **Drift** toggle loads the same baseline the code graph's governance overlay reads — each in-vault project repo's `links.json` drift-checked through the shared `code_sources` adapters — folded PER SPEC SLUG (the code side folds per moniker; same severity rule, `Missing` > `Drifted` > `Ok`), and a note's badge is the worst state across the anchors it defines, painted as the engine's badge dot in the code overlay's exact palette (one color means one state across both graphs). Loading is lazy on first enable (drift-checking re-fingerprints every linked body — the code overlay's first-switch cost) and cached; the toggle persists on the view-state record. A spec node's menu composes an **"Open in code graph"** submenu (one entry per anchor): the jump opens/focuses the owning project's code-graph tab and LIGHTS the spec there (governance mode + lighting + pulse, through a pending slot when the view isn't built yet) — the bridge-by-navigation rule, never a merged node set. An ungoverned slug answers with a loud toast. [vault-graph-spec-drift-badge]
status:: partial
implements:: [[code:hiker/panels/graph_spec/load]], [[code:hiker/panels/graph_spec/fold_spec_states]], [[code:hiker/panels/graph_spec/note_badges]], [[code:hiker/panels/graph_spec/jump_to_spec]], [[code:hiker/panels/code_graph/light_spec]]
verifies:: [[code:hiker/panels/graph_spec/tests/fold_spec_states_keeps_the_worst_link_per_spec]], [[code:hiker/panels/graph_spec/tests/note_state_folds_across_the_notes_anchors]]
touches:: [[code:hiker/panels/graph]]
note:: partial, gaps named: the baseline is the project repos' committed `links.json` — there is no vault-NATIVE link store yet, so a vault with no project repo (or none carrying a baseline) gets no badges (the toggle's hover text says so); the rollup refreshes only on re-enable / restart (no staleness watch); and the jump submenu lists every anchor on the doc — right for today's doc-grain spec notes, long for a 30-anchor doc, and naturally collapses to one entry once one-note-one-spec vault kinds exist · evidence: `app/src/panels/graph_spec.rs`, `app/src/panels/graph.rs` (`toggle_drift`, `overlay_controls`, `NodeMenuAction::JumpSpec`), `app/src/panels/code_graph.rs` (`light_spec` + the pending-light consume)


## Shared navigation layer

The code graph's focus-navigation grammar — overview ⇄ depth-bounded neighbourhood, the hops dial, drill + global Back/Forward, the Esc middle rung — is shared scaffolding, not a code-view special: the vault and code panels drive one app-side helper module over the shared `Scope` type, and each panel keeps only its own policy (what counts as an edge, what's drawn, how a drill is recorded). [graph-nav-shared]
status:: done
touches:: [[code:hiker/panels/graph_nav]], [[code:hiker/panels/graph]], [[code:hiker/panels/code_graph]]
note:: the cluster graph deliberately keeps only the find popup from this layer — its data is a strict tree whose hierarchy the radial layout already shows in place, so a hop-neighbourhood (ancestors + parent's other subtrees) adds noise rather than navigation; expand/collapse + find remain its grammar, and the focus/nav-stack pieces are explicitly not wired there

- **Extraction.** The scaffolding lives in `app/src/panels/graph_nav.rs` — an app-side helper over the shared `Scope` type, NOT the `graph-view` widget (the widget stays a render engine; the nav pieces need app types like the global nav stack, so a widget-level abstraction would leak). It carries: the generic depth-bounded neighbourhood BFS (`hop_mask`, undirected, over caller-supplied edge pairs), the Overview/1/2/3 scope dial (hops disabled until an anchor exists), the toolbar Back/Forward controls (+ mouse Extra1/Extra2), the Esc middle-rung gate (popups and focused text fields consume Esc first), and the scope persist round-trip. The code panel was rewired onto it; the vault panel gained the grammar: an overview click opens the note (unchanged) *and* anchors the dial, the hops scope clamps the display to the anchor's neighbourhood over the **typed edge union respecting the edge-kind toggles** (a toggled-off kind carries no reachability; the walk follows only drawn edges so nothing floats unconnected; the anchor survives its own kind filter), a click while focused **drills** (re-anchors — `interaction.md` [click-exceptions]; opening stays on the node menu), drills ride the GLOBAL nav stack (`NavTarget::VaultGraphNode`, restored without re-recording), Esc pops the focus back to the overview, and the focus location persists on the existing `GraphViewState` record. [graph-nav-extract]
status:: done
implements:: [[code:hiker/panels/graph_nav/hop_mask]], [[code:hiker/panels/graph_nav/scope_dial]], [[code:hiker/panels/graph_data/focus_nodes]], [[code:hiker/panels/graph/route_pick]], [[code:hiker/panels/graph/apply_nav_target]]
verifies:: [[code:hiker/panels/graph_nav/tests/hop_mask_bounds_the_neighbourhood_by_depth]], [[code:hiker/panels/graph_data/tests/focus_neighbourhood_bounds_depth_over_typed_edges]], [[code:hiker/panels/graph_data/tests/focus_neighbourhood_respects_edge_kind_toggles]], [[code:hiker/panels/graph_data/tests/focus_anchor_survives_kind_filter_and_stale_focus_falls_back]], [[code:hiker/state/nav_tests/vault_graph_drills_seed_and_walk_back_to_overview]]
touches:: [[code:hiker/state/NavTarget]], [[code:hiker/autosave/GraphViewState]]
note:: graph-unification-plan §3 Phase B; orphan-hiding is skipped inside a focus neighbourhood (only the anchor can be drawn-isolated); overview anchor changes are nav-silent (the click's file open already recorded) while scope/drill changes push, deduped per settle by `NavState::push`
- **Focusable graph tab.** `TabKind::Graph` carries an optional focus target (`GraphFocus`: note path + depth), mirroring how `CodeGraph` carries its view-source — the tab stays a singleton (find-or-focus by kind). Opening focused (`graph::open_focused`, the [[spec:open-in-graph]] dispatch) lands the panel in neighbourhood mode with the nav stack seeded overview-then-focused, so Back from the neighbourhood is the full-vault overview; a not-yet-built panel takes the target through a pending slot consumed (silently, never re-recorded) on its first render. Persistence: a focused tab's restore key is `graph:<depth>:<path>` (the unfocused singleton keeps `:graph`), round-tripping the kind across restart; the *landing* state restores through the persisted view state's scope/focus fields ([[spec:graph-view-state-persist]]), which reflect where the user actually left the panel. [graph-tab-focus]
status:: done
implements:: [[code:hiker/panels/graph/open_focused]], [[code:hiker/tab/GraphFocus]]
verifies:: [[code:hiker/tab/tests/graph_focus_persist_key_round_trips]], [[code:hiker/state/nav_tests/vault_graph_drills_seed_and_walk_back_to_overview]]
touches:: [[code:hiker/bootstrap]], [[code:hiker/panels/graph]]
note:: entry points are the note-item base menu + the board/trail container menus ([[spec:open-in-graph]], [[spec:open-in-graph-containers]], specced in `context-menu.md`); depth clamps to the dial's 1–3
- **Scoped graph from a query.** A query-doc scopes the node set — "graph of this smart folder", the vault analogue of `code-graph-scoped-default`. The smart-folder header's context menu gains **"Open in graph, scoped"** ([[spec:ctxmenu-contextual-extend]] composition; registered in `context-menu.md`): `graph::open_scoped` find-or-focuses the singleton Graph tab carrying the scope payload (`TabKind::Graph.scope_query`, persisting through a `graphq:<query-path>` restore key — scope outranks focus in the key; the LANDING restores via the view-state record, the [[spec:graph-tab-focus]] posture) and lands on the scoped overview. The scope is ORTHOGONAL to the hops focus and composes: the query's match set bounds the node universe (a mask under the kind filter + detail dial), and a focus drill walks only edges inside it — an out-of-scope node is unreachable at any depth. Execution is per-set and per-rebuild against the live index through the one shared `run_query` path (staleness = the graph's own rebuild cadence: 5-minute re-walk or the Rebuild button; the member set is never persisted, only the doc path rides `GraphViewState.scope_query`); the toolbar chip names the scope with its live member count plus a Clear button. A failed parse/run surfaces LOUDLY — red error text in the chip and a display of only the query-doc node (the smart-folder header + error-row posture; never a silent fallback to the full vault). Orphan-hiding is skipped while scoped (the scope IS the folder's member set; degree is global, so hiding members without in-scope edges would silently shrink it). Scope is display state like the filters — it rides the persisted view state, not the nav stack. [graph-scoped-query]
status:: done
implements:: [[code:hiker/panels/graph/open_scoped]], [[code:hiker/panels/graph_data/restrict_to_scope]], [[code:hiker/panels/graph_data/scope_error_mask]], [[code:hiker/vault_view/query_header_menu]]
verifies:: [[code:hiker/panels/graph_data/tests/scope_restricts_the_universe_and_focus_drills_within_it]], [[code:hiker/panels/graph_data/tests/scope_error_mask_keeps_only_the_query_doc]], [[code:hiker/tab/tests/graph_focus_persist_key_round_trips]]
touches:: [[code:hiker/tab/GraphFocus]], [[code:hiker/autosave/GraphViewState]]
note:: graph-unification-plan §3 Phase D; the scope members are deliberately the display universe WITHOUT a query-membership edge kind (members connect by their real typed edges; deriving query edges in the overview would mean running every query per rebuild — exactly the cost scoping confines to the scoped tab), and the chip, not a floating doc node, anchors the scope in the chrome


## Hover & selection highlight

- **Edge highlight, above edges / below everything else.** Hovering a node lights up its incident edges (fade in/out over `fade_secs`); the host-marked selected node's edges stay lit persistently. The overlay mirrors the base edge geometry (routed / geodesic / straight) with a soft multi-pass glow (`HighlightStyle`: color / width / opacity / softness), recomputed per frame outside any cached GPU batch. The shapes fill a slot reserved **after the base edges but before the nodes paint**: the glow traces above the edges yet stays under node shapes and labels. (On the GPU path node fills share the edge callback one slot below, so only the translucent glow washes over them — labels and the hover ring stay on top.) [graph-hover-highlight]
status:: done
implements:: [[code:hiker/graph_view/edges/impl#[State]highlight_edge_shapes]]
touches:: [[code:hiker/graph_view/panes]], [[code:hiker/graph_view/edges]]
- **Dim labels to selection.** An option (view menu → Highlight, on by default): when a node is selected, its label renders at full strength, its 1-hop neighbours' labels semi-dimmed (×0.55), and every other label dimmed (×0.18) — the selection's context pops out of the label field. No selection (or option off) renders exactly as before; the factors ride the existing per-label rim-fade alpha. [graph-label-dim]
status:: done
implements:: [[code:hiker/graph_view/edges/impl#[State]label_dim_factors]]
touches:: [[code:hiker/graph_view/edges]]
- **Hover flow (discrete mode).** When the hover *moves* between two nodes, the highlight doesn't jump: the two nodes are keyframes of a cross-fade (old glow out, new glow in, eased over `flow_secs`), and any edge directly connecting them carries a short bright **pulse travelling** from the old node's end to the new one's — fade in/out plus positional fading. Non-adjacent keyframes simply cross-fade (no path search). Active when `HighlightStyle.fluid` is off. [graph-hover-flow]
status:: done
implements:: [[code:hiker/graph_view/edges/impl#[State]hover_flow_shapes]], [[code:hiker/graph_view/edge_paint/sub_polyline]], [[code:hiker/graph_view/panes/impl#[State]paint_highlight_overlay]]
verifies:: [[code:hiker/graph_view/edge_paint/overlay_tests/sub_polyline_clips_with_interpolated_endpoints]]
- **Fluid highlight (default).** The highlight behaves like a fluid on the graph: hovering injects energy at a node — **ramped** (~150 ms to saturation), so a new hover fades up while the old wake drains rather than hard-activating; each frame the energy **diffuses across edges**, **drifts downhill toward the selected node** (the hop-distance BFS field from the selection is the gravity; flat with no selection), and **decays** — so sweeping the pointer leaves a glowing wake that drains toward the selection. Rendered as per-edge gradient strokes (alpha lerped between endpoint energies) plus soft halos under energized nodes, in the same bottom-most slot. Explicit-Euler with per-step transfer fractions clamped ≪ 0.5, so it can't oscillate at any frame rate; O(V+E) per frame and repaints only while energy remains. Toggle: view menu → Highlight → "Fluid highlight". [graph-hover-fluid]
status:: done
implements:: [[code:hiker/graph_view/edges/impl#[State]fluid_advance_and_shapes]], [[code:hiker/graph_view/edge_paint/hop_potential]], [[code:hiker/graph_view/edge_paint/gradient_strokes]]
verifies:: [[code:hiker/graph_view/edge_paint/overlay_tests/hop_potential_is_bfs_distance_with_unreachable_at_max]]


## Projection modes

The graph is the primary target for the shared `Projection` seam: Off (affine) / Fisheye / Poincaré-disk navigation, with geodesic edges, boundary fade, a magnification-coupled LOD ladder, Möbius fly-to, and a corner Poincaré minimap. Because every graph consumer drives this one engine, the modes land for all of them at once. The seam, its math crate (`hiker-projection`), and the full control set are specced in `projection.md` ([[spec:proj-graph-mode]] and the `proj-*` slugs); this doc does not duplicate them.


## GPU paint path

For large graphs (10k–20k+ nodes) the egui `Painter` tessellation of every circle, line, and glyph dominates the frame. An optional instanced **wgpu** paint path bypasses it for the two highest-volume primitives.

- **Backend = wgpu.** The app runs eframe on the `wgpu` renderer (`eframe::Renderer::Wgpu`); `HIKER_RENDERER=glow` forces the legacy backend. The GPU paint path requires wgpu and is otherwise inert. [app-wgpu-backend]
status:: done
note:: app runs eframe on the wgpu backend; prerequisite for the instanced GPU paint path · evidence: `app/src/main.rs` (`eframe::Renderer::Wgpu`; `HIKER_RENDERER=glow` escape)
- **Instanced node + edge fills.** A custom `egui_wgpu::CallbackTrait` GPU-instances node fills (one unit quad per node, expanded to `±radius` with an AA circle/box SDF in the fragment shader) and edge segments (one instance per polyline segment, expanded to a width quad honouring the edge-width control), bypassing CPU tessellation. Opt-in (on by default under wgpu; an "instancing" toggle + `HIKER_GPU=0` disable it); the egui `Painter` path stays byte-identical as the fallback, and labels / hover ring / disk boundary / tooltips / preview stay on the Painter (drawn after the callback). [gpu-instanced-paint]
status:: done
touches:: [[code:hiker/graph_view/gpu]]
note:: custom `egui_wgpu::CallbackTrait`: instanced node-fill quads (AA circle/box SDF) + edge-segment width quads, bypassing CPU tessellation. Opt-in under wgpu (`HIKER_GPU=0` / menu toggle disable); Painter path is the byte-identical fallback; labels/hover/boundary stay on the Painter · evidence: `widgets/graph-view/src/graph_view/gpu.rs`, `edge_paint.rs` (`draw_node_fill`/`emit_edge`), `panes.rs` (`gpu_batch_*`)
- **Per-pane viewport-relative transform.** egui-wgpu sets the GPU viewport to the callback's pane rect, so the shader maps points → NDC relative to that rect (`2·(p − origin)/size − 1`), carried in a **per-pane** uniform (the main view and the corner minimap have different rects). This is what makes the path correct on HiDPI / fractional-scaling displays and offset sub-rect panes — a full-window-relative transform silently squashes nodes into a sub-region otherwise. [gpu-pane-transform]
status:: done
touches:: [[code:hiker/graph_view/gpu]]
note:: egui-wgpu sets the GPU viewport to the callback rect, so points→NDC map relative to the pane rect, carried per-pane (main view vs corner minimap). Correct on HiDPI/fractional-scaling + offset sub-rect panes (a window-relative transform squashes nodes otherwise) · evidence: `widgets/graph-view/src/graph_view/gpu.rs` (per-pane `Uniform{origin_pts,size_pts}`, `to_ndc`)
- **Affine pan via a view-transform uniform (no per-frame rebuild).** Under the affine (non-lens) view the batch stores **world** positions + base radii; the uniform carries the affine view map (`view_scale` + `view_offset`) the shader applies before the NDC map. So a pure pan/zoom rewrites only the small uniform — the instance/edge buffers are **cached** across frames (keyed by `layout_epoch` + a content fingerprint) and not re-uploaded. This removes the pan-lag bottleneck (edge upload). Lens modes (Poincaré/Fisheye) move every node per frame and rebuild as before. The world-space fill is **not** viewport-culled (the GPU scissor clips it), so a later zoom-out on the cached buffer keeps every node; the per-frame label/hover work still culls to the viewport. [gpu-affine-pan-cache]
status:: done
touches:: [[code:hiker/graph_view/gpu]]
note:: affine batch stores world positions + base radii; pan/zoom rewrites only the uniform, instance/edge buffers cached across frames (no re-upload) → kills pan lag. World-space fill not viewport-culled (GPU scissor clips) so zoom-out keeps all nodes; labels/hover still cull per-frame. Lens modes rebuild per frame · evidence: `widgets/graph-view/src/graph_view/gpu.rs` (`view_scale`/`view_offset` uniform, `affine_cache`), `mod.rs` (`layout_epoch`)
- **Animated edge flow.** A toggle-able animation draws tracer dots travelling each edge from caller→callee, to show call direction. It reuses the cached edge buffer and a `time` uniform — the dot position is `mix(a, b, fract(time·speed + phase(i)))` computed in the shader, so it animates with zero geometry rebuild (the affine cache stays intact). GPU-path only; off by default (it requests continuous repaint while on). [graph-edge-flow]
status:: done
touches:: [[code:hiker/graph_view/gpu]]
note:: toggle-able tracer dots along each edge caller→callee; dot position `mix(a,b,fract(time·speed+phase))` computed in-shader over the cached edge buffer (zero rebuild, cache intact). GPU-path only; off by default; requests continuous repaint while on · evidence: `widgets/graph-view/src/graph_view/gpu.rs` (`flow` pipeline, `time` uniform), `mod.rs` (`flow_enabled`)

## Layout tuning, performance, and the view menu

Beyond the temporal-stability controls, the view menu exposes the force-layout's shape and budget, plus per-source label/size policy. The force solver is also parallelised.

- **Parallel force layout.** The Barnes–Hut repulsion (and the per-node integrate/gravity) step of the FA2 solver fans out across cores with rayon for large graphs (above a small-graph threshold that preserves single-threaded determinism); the quadtree is a flat index-linked `Vec` shared read-only. ~2.5–3× faster settle on the 20k-node graph. Attraction stays sequential. [graph-parallel-layout]
status:: done
touches:: [[code:hiker/force]]
note:: FA2 Barnes–Hut repulsion + per-node integrate fan out across cores for large graphs (small-graph guard preserves single-thread determinism); quadtree is a flat index-linked `Vec` shared read-only. ~2.5–3× faster 20k settle. Attraction sequential · evidence: `hiker-render/graph/src/force.rs` (rayon over repulsion/integrate)
- **Spread control.** A "Spread" slider drives the FA2 repulsion strength (`scaling_ratio`), which is the knob that actually changes the settled extent (weak gravity, being ∝1/dist like repulsion, barely moves it). The runaway safety belt (`bound`) is scaled with the graph's mass (`≈ n + 2·edges`) **and** the spread, so a big graph never piles its periphery into a square wall during settle — a fixed belt becomes a wall as graphs grow. [graph-layout-spread]
status:: done
touches:: [[code:hiker/graph_view]]
note:: "Spread" slider → FA2 `scaling_ratio` (the real extent knob); `bound` safety belt scaled by mass (`n+2·edges`) × spread so a big graph never piles its periphery into a square wall during settle · evidence: `widgets/graph-view/src/graph_view/mod.rs` (`layout_spread`, `recompute_layout`)
- **Settle iterations.** A "Settle iters" slider sets the FA2 `max_iters` so a big graph that still drifts at the default cap can run longer; it still stops early on convergence. [graph-settle-iters]
status:: done
touches:: [[code:hiker/graph_view]]
note:: "Settle iters" slider sets FA2 `max_iters` so big graphs that still drift at the default cap can run longer; stops early on convergence · evidence: `widgets/graph-view/src/graph_view/mod.rs` (`settle_iters`, `recompute_layout`)
- **Constant-font label budget LOD.** Labels are a **constant** screen-size overlay — they never scale with zoom. WHICH labels show is governed by a per-frame **budget**, placed highest-priority-first and de-conflicted by overlap, so the overview shows a readable handful (the most important) and the count GROWS as you zoom in (the budget scales with the fit-relative `label_zoom`, floored at `BASE_LABELS`, capped at `MAX_LABELS`). A node becomes a label *candidate* once its per-node `label_min_zoom` depth gate reveals; the budget then bounds the placed count, replacing the fragile per-node rank gate (which could drop EVERY label on a mis-calibrated fit). A **small** graph (≤ `SMALL_GRAPH_LABELS` — a filtered Hops drill or a single-package SCIP) bypasses the depth gate and lifts the budget to `MAX_LABELS`, so most labels place (subject only to overlap). Identical label texts are de-duplicated per frame (the most prominent instance wins). Priority is structural (`label_scale`, importance-biased); each `NodeDescriptor` carries a `label_min_zoom` gate + a `label_scale` multiplier the source sets by structure. [graph-label-budget]
status:: done
touches:: [[code:hiker/graph_view/edges]], [[code:hiker/panels/entity_graph]]
note:: constant-font screen-space labels; per-frame `label_budget(small_graph, label_zoom)` caps the de-confliction pass (floor `BASE_LABELS`, cap `MAX_LABELS`); a `small_graph` (≤ `SMALL_GRAPH_LABELS`) bypasses the depth gate + lifts the budget to `MAX_LABELS`. Hover EMPHASIS is colour/outline only (accent text + accent pill outline), never a size bump — a size change would reflow neighbours via de-confliction; a node whose label is already drawn suppresses its floating tooltip. The code graph feeds the gate its structural-depth tier + per-id importance · evidence: `widgets/graph-view/src/graph_view/edges.rs` (`BASE_LABELS`/`MAX_LABELS`/`SMALL_GRAPH_LABELS`/`label_budget`/the de-confliction loop), `app/src/panels/entity_graph/mod.rs` (`node_depths`, `label_importance`, `label_scale_for`)
- **Size by LOC.** The code graph can weight node radius by lines-of-code (√-scaled) from the SCIP `enclosing_range` body span (`GraphNode.lines`), instead of by degree — a per-source toggle. The node-colour palette pickers are hidden for sources that colour by another rule (the code graph colours by kind), via a `palette_editable` flag. [graph-size-by-loc]
status:: done
touches:: [[code:hiker/panels/code_graph]], [[code:hiker/scip_adapter]]
note:: per-source toggle: weight node radius by √(LOC) from the SCIP enclosing-range body span instead of degree; `palette_editable=false` hides the vault node-colour pickers where nodes colour by kind · evidence: `app/src/panels/code_graph.rs` (`size_by_loc`), `code-intel/hiker-code/src/scip_adapter.rs` (`GraphNode.lines` from `enclosing_range`)
- **Spatial bundling (marker-cluster collapse-on-zoom).** An engine-level toggle (`State.bundling`, off by default): nodes whose on-screen positions fall in the same world-fixed power-of-2 grid cell collapse into the cell's highest-`label_scale` representative, which draws inflated with a live `· N` count; zooming subdivides the grid in stable octaves so a bundle's members are always within one cell of the rep (revealed in-viewport on zoom-in, never off-screen). Rolled-up edges dedupe to one line between reps. Un-bundling eases members out from the rep over ~0.35s. Affine-path only; the read-only / Poincaré panes pass a non-positive `screen_scale` and get the identity (every node shown), so canvas / vault-graph / minimap are unaffected. The code-graph lens exposes this as its Bundle toggle ([[spec:entity-graph-bundling]]). [graph-spatial-bundling]
status:: done
touches:: [[code:hiker/graph_view/edges]]
note:: `BundleState` (per-pane, built by `compute_bundles` hashing each `world_pos` into a `MERGE_PX`-sized power-of-2 grid cell); rep = highest `label_scale`, tie-break lowest index; `radius_mult` inflates a live bundle, the engine appends the `· N` suffix; `advance_reveal` + `effective_positions` drive the fly-out tween (`REVEAL_DUR`). Identity (`BundleState::identity`) for non-positive `screen_scale` keeps every other source unchanged · evidence: `widgets/graph-view/src/graph_view/edges.rs` (`BundleState`, `compute_bundles`, `MERGE_PX`, `advance_reveal`, `effective_positions`)
- **Affine glide-to-selection.** When the host sets the engine's `selected_node` to a new in-range node, the engine smoothly pans the affine view to centre it (~0.4s ease-out, pan only — zoom untouched). Skipped during a fit/re-fit (a fresh build / scope-drill owns the framing then) and cancelled by any manual pan/zoom, so the user's gesture is never fought; a tiny move snaps without animating. [graph-glide-to-selected]
status:: done
touches:: [[code:hiker/graph_view]]
note:: `State::glide_to`/`advance_glide` (the `Glide` ease-out over `GLIDE_DUR ≈ 0.4`s, pan-only); triggered in `panes.rs` on a `selected_node != prev_selected` change when `!needs_fit`, cancelled on a manual zoom/drag (`needs_fit = false; glide = None`). Code tag `code-graph` · evidence: `widgets/graph-view/src/graph_view/nav.rs` (`glide_to`, `advance_glide`, `Glide`), `panes.rs` (trigger)
- **Graph visual test harness.** A headless harness renders the REAL `EntityGraphSource` through the engine via egui_kittest's wgpu backend to PNGs — overview / zoom / hover / click / bundle-open / reveal-mid scenarios — so the LOD / label / layout / bundling / glide behaviour can be SEEN and verified instead of guessed. `#[ignore]`d (needs a SCIP index + a wgpu device + writes files), run on demand: `HIKER_HARNESS_SCIP=… cargo test -p hiker-app --lib graph_harness -- --ignored --nocapture` (defaults to a small fixture SCIP, skips cleanly with no SCIP / wgpu device). [graph-visual-harness]
status:: done
touches:: [[code:hiker/graph_harness]]
note:: renders the app's real overview display (full code+spec+governance build when the vault/repo mirror is present, else code-only) into `target/graph-harness/<name>.png`; injects pointer input + drives the glide / un-bundling tweens per scenario · evidence: `app/src/graph_harness.rs` (`graph_harness`, `Scenario`, `load`)

## Deferred

- **3D graph mode.** A 2D/3D toggle riding the shared 3D scene substrate, with the cluster hierarchy and the note wikilink graph as alternate edge feeds. Tracked in `ideas.md` ([[spec:graph-3d-mode]] / [[spec:scene3d-shared]]).
- **Edge-flow direction colouring / speed control.** Per-edge-kind tracer colours and a flow-speed slider on top of [[spec:graph-edge-flow]].
- **Parallel attraction.** The per-edge attraction phase is left sequential; a conflict-free parallel reduction could extend [[spec:graph-parallel-layout]].
