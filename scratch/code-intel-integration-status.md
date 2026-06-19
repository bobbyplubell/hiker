# code-intel ↔ hiker — integration status

Tracks the move of the standalone engine **into the hiker repo** and the **external-projects**
work. Companion to `code-intelligence-vision.md` (vision) and `hiker-integration-plan.md` (plan).

## Where things live now

The engine is a **self-contained sub-workspace inside the hiker repo**:
`references/hiker/code-intel/`. It is **excluded** from the hiker root workspace (which can't build
here anyway — the UI submodules `zxr`/`editor`/`hiker-render`/… are absent), exactly the pattern
hiker already uses for `hiker-crawler`/`editor`/`hiker-render`. It builds, tests, and QAs entirely
on its own:

```
code-intel/          # standalone sub-workspace, flat crate layout (matches editor/ etc.)
  spec-engine/       # the DerivedNodeSource port + node/edge model + link store + drift
  hiker-code/        # SCIP-consumer ScipAdapter (impl the port) + code_graph() render accessor
  code-cli/          # standalone CLI incl. the `graph` SVG/DOT renderer (the QA tool)
  fixtures/          # sem.scip (rust-analyzer), pyproj.scip (scip-python), hiker.scip — all gitignored
  test-vault/        # demo vault: project notes + an in-vault .scip (consumer data, not engine)
  docker/            # scip-python.Dockerfile (Node stays in the container, never on the host)

hiker-projects/      # ← top-level generic hiker crate (decoupled from code-intel)
```

**`hiker-projects` is decoupled and relocated.** It is a generic project-sourcing layer that parses
notes into typed source *descriptors* + thin git metadata, with **no dependency on code
intelligence** (it no longer imports `hiker-code`/`spec-engine` and has no `bind_scip`). It lives at
the hiker repo root as a first-class workspace crate. The repo-descriptor → `ScipAdapter` binding is
done **by the consumer** (the app panel / `code-cli`), so the dependency graph is two clean leaves
the app composes:

```
hiker-projects (notes → descriptors + git)      hiker-code (.scip → graph)
        ▲                                                ▲
        └──────────── app / code-cli (glue both) ────────┘
```

The hiker **root** `Cargo.toml` excludes `code-intel`, adds `hiker-projects` as a member, and
registers `spec-engine`/`hiker-code` (path into `code-intel/`) + `hiker-projects` (top-level) in
`[workspace.dependencies]`.

## Done (buildable + QA-verified here)

- **Crates relocated** into `code-intel/` and wired as an excluded sub-workspace. `cargo build`,
  `cargo test` (6 pass), `cargo clippy --all-targets` all clean.
- **`ScipAdapter::code_graph()`** (plan item A) — exposes the in-memory graph in render shape
  (`CodeGraph { nodes: Vec<GraphNode>, edges: Vec<(usize,usize,EdgeKind)> }`), stable per-node
  index, external/local endpoints dropped. Deterministic ordering.
- **`hiker-projects`** (plan item B / `docs/hiker-projects.md`) — parses a `hiker.kind: project`
  note's frontmatter, resolves each `sources[]` entry; for `kind: repo` it derives `repo_id`
  (frontmatter → git root-commit/remote → path-based fallback), `~`-expands paths, parses
  `scope` include/exclude globs, tracks index staleness via thin read-only git metadata, and
  yields a repo descriptor the consumer binds to a `ScipAdapter`. Other source kinds (`jira`/`docs`) recognized as
  `Unsupported` placeholders. UI-free (no hiker `core` dep). 6 unit tests.
- **`code-cli graph`** (plan "standalone de-risk" + QA goal) — renders a code project's entity
  graph to a **self-contained SVG** (own Fruchterman-Reingold layout + central gravity +
  percentile fit; **no graphviz needed**, since `dot` isn't installed) and optionally Graphviz
  **DOT**. Colored by entity kind, edged by Calls/Implements/TypeRef/Imports, with legend.
  Scoping (plan item F): `--focus <name> --depth N` neighborhood, and a top-degree fallback with
  an explicit "rendering N of M" message so large graphs never silently dump a hairball.
  - Source can be `<index.scip> <repo_root>` **or** `--project <note.md>` (binds through
    hiker-projects end-to-end).

### How to QA the graph render

```
cd code-intel
# small, fully-legible Python graph (ABC + impls); orange edges = implements relationships:
cargo run -p code-cli -- graph fixtures/pyproj.scip fixtures/pyproj --svg pyproj.svg
# same, but driven by the project note (proves the external-projects path):
cargo run -p code-cli -- graph --project fixtures/pyproj-project.md --svg pyproj.svg
# large repo, scoped to a focus node's 2-hop neighborhood (keeps it legible):
cargo run -p code-cli -- graph fixtures/sem.scip <sem-repo> --focus EntityGraph --depth 2 --svg sem.svg
# open the .svg in a browser, or rasterize: `magick -background white pyproj.svg pyproj.png`
```

## In-app integration (rows C–G) — DONE, submodules now initialized

The hiker UI submodules (`zxr`/`editor`/`egui-workbench`/`hiker-render`/`hiker-charts`) were pulled
(`git submodule update --init --recursive`), so the hiker workspace builds. Rows C–G are
implemented and **`cargo check -p hiker-app` is clean** (zero new warnings):

- **C. `graph_view::Source` over `CodeGraph`** — `app/src/panels/code_graph.rs` adds `CodeGraphSource`,
  a third Source beside vault + cluster graphs. Maps entity kind → shape (type=square, else circle)
  + color, degree → radius, edges → index pairs, `preview_for` → kind·file. **Verified headlessly**:
  `widgets/graph-view/examples/code_graph_snapshot.rs` drives the *real* `graph_view::State` engine
  over a fixture `.scip` via `egui_kittest`'s wgpu backend and writes a PNG — proving the engine
  renders our code project, not just the standalone SVG.
- **D. `TabKind::CodeGraph { project_path }` + dispatch + entry point** — variant + exhaustive
  `label`/`icon`/profiler arms; dispatched in `workbench_host.rs` to `panels::code_graph::show`;
  per-note state on `AppState::panels.code_graph`. Opening a `hiker.kind: project` note from the
  file sidebar routes to the code-graph tab (`is_project_doc`, mirroring `is_board_doc`).
- **E. Click → read-only detail** — node `click_path` carries the SCIP moniker; the panel resolves
  the selected node's kind + definition `file:line` via the adapter's `locate` (no editable tab).
- **F. View affordances** — Calls / Implements **edge-type toggles** (via the engine's view-options
  menu) that relayout on change; **scoped top-degree default** (`scope_top_degree`, cap 400 nodes)
  with an explicit "top N of M by degree" summary so a large repo never silently hairballs.
- **G. App dependency wiring** — `spec-engine`/`hiker-code`/`hiker-projects` added to `app/Cargo.toml`
  (one-way: `app → hiker-code/hiker-projects → spec-engine`), resolved through the root
  `[workspace.dependencies]` path deps into `code-intel/`.

## Projects activity + UI project authoring

A first-class **Projects** entry in the activity bar (`app/src/projects_activity/mod.rs`), registered
in `builtin_activities()` between Canvases and Context:

- **Sidebar list** — enumerates every `hiker.kind: project` note via the store's frontmatter index
  (`MetaFilter::Equals { "hiker.kind", "project" }`, the same discovery cluster-presets use). A row
  click opens that project's code graph; a per-row ⚙ opens its config form.
- **+ New project** button → opens the **project-config tab** (`TabKind::ProjectConfig`,
  `app/src/panels/project_config.rs`): a UI form to set the project name and add/configure external
  **sources** (repo / docs) with their kind-specific fields (repo: root, `.scip` index, repo_id,
  scope include/exclude; docs: root). Only kinds with a working binding are offered — jira and the
  LSP backend are design-level future sources with no adapter yet, so they're not selectable.
  **Save** serializes
  to a project note with **nested `hiker: { kind: project }`** frontmatter (the form hiker's own
  notes use) at `projects/<slug>.md` via the indexer-driven write path, then opens its code graph.
  The ⚙ edit affordance loads an existing note's `sources[]` back into the form (faithful round-trip).
- **Frontmatter reconciliation** — `hiker-projects` now parses **both** the flat `hiker.kind:` and
  the nested `hiker: { kind }` forms (the latter is hiker's real convention, per `clusters/preset.rs`),
  so UI-saved notes and the hand-written CLI fixtures both bind. A unit test in `project_config.rs`
  asserts a form-saved note round-trips back through `hiker_projects::Project::parse`.

### QA the in-app render headlessly

```
cargo run -p hiker-graph-view --example code_graph_snapshot -- \
    code-intel/fixtures/pyproj.scip code-intel/fixtures/pyproj /tmp/hiker_pyproj.png
# → renders our CodeGraph through hiker's real graph_view engine to a PNG.
```

## Recent additions (post-merge, on `code-in-hiker-new`)

- **`.scip` direct-open** — `TabKind::CodeGraph` now carries a `CodeSource` enum
  (`Project(note) | Index(scip)`); the file tree opens a `.scip` directly as a code graph (no
  project note; repo root defaults to the index's own directory). Both paths funnel through one
  `ScipAdapter::load` in the panel.
- **Path-traversal hardening** — `ScipAdapter` reads source files through `safe_join`, which refuses
  absolute/`..`/symlink-escape paths so a crafted `.scip` can't read outside the repo root (the
  `CODE-IN-VAULT.md` trust direction). Unit-tested.
- **Level-of-detail / drill-down** (`code-graph-lod`) — the adapter derives **containment**
  (`GraphNode.parent`, from moniker nesting + impl-type fallback; `is_object()` = type/module). A
  reusable, tested **`hiker_code::collapse`** lifts hidden members' edges up to their nearest visible
  object (pure; the consumer supplies the visibility policy). The panel adds a global **Objects →
  +Functions → Everything** control *and* per-object **click-to-expand** (cap-aware: expanded
  subtrees always survive the degree cap). The full colorful graph is the "Everything" setting.
  Verified headlessly via `code-cli graph --level objects|members|all` (the objects view of pyproj =
  the 3 types with aggregated edges).

```
# see the LOD collapse as an SVG (no GUI):
cd code-intel && cargo run -p code-cli -- graph fixtures/pyproj.scip fixtures/pyproj --level objects --svg /tmp/obj.svg
```

**Still pending for the full `CODE-IN-VAULT.md` direction** (not done here): enforcing
*reads-only-inside-the-vault* (the adapter clamps to its repo root, but the panel doesn't yet require
that root ⊆ vault), rewriting the demo notes off absolute external paths, and the broader
"code-as-read-only-reference-content" core change.

## Constraints honored

- **No Node on the host** — `scip-python` runs only in `docker/scip-python.Dockerfile`; the
  fixtures are pre-generated. No host Node was used.
- Self-contained SVG output means **no graphviz dependency** for the CLI QA path; the in-app render
  uses hiker's own wgpu/egui pipeline.
- Stayed within `sem/`: all work is in `references/hiker` (the in-project copy). Submodules were
  fetched only after explicit user authorization.
