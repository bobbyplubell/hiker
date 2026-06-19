---
hiker:
  kind: project
sources:
  - kind: repo
    root: .
    repo_id: sem
    backend: scip
    index: sem.scip
---

# sem — large-graph demo

The `sem` Rust codebase (~2,774 entities) bound via a rust-analyzer `.scip`. Opening
this project exercises the **scoped default**: the graph caps at the top ~400
highest-degree nodes (the panel footer says "top N of M by degree") so a large repo
stays legible instead of hairballing. Toggle Calls / Implements edges from the eye menu.

The `index:` is a **vault-relative** path to the in-vault `sem.scip` (CODE-IN-VAULT
trust invariant: hiker only reads inside the vault). `root: .` points at the vault
dir; the topology renders from the index alone, so the graph draws even though the
sem working tree isn't in the vault — source previews degrade (graph-only).
