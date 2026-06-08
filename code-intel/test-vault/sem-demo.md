---
hiker:
  kind: project
sources:
  - kind: repo
    root: /home/bobby/projects/sem
    repo_id: sem
    backend: scip
    index: /home/bobby/projects/sem/references/hiker/code-intel/fixtures/sem.scip
---

# sem — large-graph demo

The `sem` Rust codebase (~2,774 entities) bound via a rust-analyzer `.scip`. Opening
this project exercises the **scoped default**: the graph caps at the top ~400
highest-degree nodes (the panel footer says "top N of M by degree") so a large repo
stays legible instead of hairballing. Toggle Calls / Implements edges from the eye menu.

(Entity previews read from the repo's working tree; the topology renders from the
index alone, so the graph still draws even if some source files have moved.)
