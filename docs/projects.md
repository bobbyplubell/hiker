# Projects

A **project** is a hiker note that describes one or more external corpora — code repos, doc sets — that the vault references. The note is the authored, in-vault handle; the corpora it points at are read-only reference content (`code.md`). A generic project-sourcing crate parses these notes into typed source descriptors, and the consumer (the app panel, the CLI) binds a repo descriptor to a `ScipAdapter` to render its code graph.

## The `hiker-projects` crate

- **Decoupled, generic sourcing layer.** `hiker-projects` (a first-class workspace crate at the repo root) parses notes into typed source *descriptors* + thin git metadata, with **no dependency on code intelligence** — it no longer imports `hiker-code` / `spec-engine` and has no `bind_scip`. The repo-descriptor → `ScipAdapter` binding is done by the consumer, so the dependency graph is two clean leaves the app composes (`hiker-projects` = notes → descriptors + git; `hiker-code` = `.scip` → graph; app / `code-cli` glue both). UI-free (no hiker `core` dep). [projects-crate-decoupled]
status:: done
note:: generic project-sourcing layer: notes → typed source descriptors + thin git metadata; consumer binds repo descriptor → `ScipAdapter`. UI-free (no `core` dep). 6 unit tests · evidence: `hiker-projects/` (root workspace crate; no `hiker-code` / `spec-engine` dep)

## Project notes

- **`hiker.kind: project` frontmatter.** A project note carries a `sources[]` list under its frontmatter. The parser accepts **both** the flat `hiker.kind:` form and the nested `hiker: { kind }` form (the latter is hiker's real convention, per `clusters/preset.rs`), so UI-saved notes and hand-written CLI fixtures both bind. [projects-note-frontmatter]
status:: done
note:: `hiker.kind: project` note with `sources[]`; parses both flat `hiker.kind:` and nested `hiker: { kind }` forms · evidence: `hiker-projects/src/` (`Project::parse`)
- **Repo descriptors.** For a `kind: repo` source the crate derives `repo_id` (frontmatter → git root-commit / remote → path-based fallback), `~`-expands paths, parses `scope` include / exclude globs, and tracks index staleness via thin read-only git metadata, yielding a repo descriptor the consumer binds to a `ScipAdapter`. [projects-repo-descriptor]
status:: done
note:: `kind: repo` → `repo_id` (frontmatter → git root-commit/remote → path fallback), `~`-expand, scope include/exclude globs, read-only git staleness tracking · evidence: `hiker-projects/src/` (`repo_id` derivation, scope globs, git staleness)
- **Source kinds.** `repo` and `docs` are supported; the LSP backend is recognized as an `Unsupported` placeholder (a design-level future source with no adapter yet), and any unknown hand-authored kind loads as `Unsupported` rather than erroring; neither is offered in the authoring UI. [projects-source-kinds]
status:: done
note:: `repo` + `docs` supported; LSP + unknown kinds recognized as `Unsupported`, not offered in the authoring UI · evidence: `hiker-projects/src/` (`Unsupported` placeholders)

## Project authoring UI

- **Projects activity-bar entry.** A first-class **Projects** entry (`app/src/projects_activity/mod.rs`), registered in `builtin_activities()` between Canvases and Context. The sidebar enumerates every `hiker.kind: project` note via the store's frontmatter index (`MetaFilter::Equals { "hiker.kind", "project" }`, the same discovery cluster-presets use); a row click opens that project's code graph, and a per-row ⚙ opens its config form. [projects-activity-bar]
status:: done
touches:: [[code:hiker/projects_activity]]
note:: sidebar lists every `hiker.kind: project` note via the store frontmatter index; row click → code graph, per-row ⚙ → config form · evidence: `app/src/projects_activity/mod.rs` (registered in `builtin_activities()` between Canvases and Context)
- **Project-config tab.** **+ New project** opens `TabKind::ProjectConfig` (`app/src/panels/project_config.rs`): a form to set the project name and add / configure external **sources** with kind-specific fields (repo: root, `.scip` index, `repo_id`, scope include / exclude; docs: root). Only kinds with a working binding are offered. **Save** serializes to a project note with nested `hiker: { kind: project }` frontmatter at `projects/<slug>.md` via the indexer-driven write path, then opens its code graph; the ⚙ edit affordance loads an existing note's `sources[]` back into the form (faithful round-trip, unit-tested). [projects-config-tab]
status:: done
touches:: [[code:hiker/panels/project_config]]
note:: authoring form (name + sources with kind-specific fields); Save serializes to `projects/<slug>.md` with nested `hiker: { kind: project }`; ⚙ round-trips an existing note's `sources[]`. Unit-tested round-trip · evidence: `app/src/panels/project_config.rs` (`TabKind::ProjectConfig`)
