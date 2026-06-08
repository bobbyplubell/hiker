---
hiker:
  kind: project
sources:
  - kind: repo
    root: /home/bobby/projects/sem/references/hiker
    repo_id: hiker
    backend: scip
    index: /home/bobby/projects/sem/references/hiker/code-intel/test-vault/hiker.scip
  - kind: docs
    root: /home/bobby/projects/sem/references/hiker/manual
---

# hiker — code + manual

A self-referential project binding **two** external sources:

- a **repo** source whose `.scip` index lives right here in the vault
  (`hiker.scip`, ~20k entities) — opening this project renders hiker's own
  code-entity graph (scoped to the top hubs by degree);
- a **docs** source pointing at hiker's `manual/` folder (the user-facing
  manual: `chart.md`, `widgets.md`, …).

The `.scip` sitting in the vault also demonstrates the "drop a `.scip` in and
open it directly" path. Note: the **docs** source is currently declarative —
`hiker-projects` records it, but there's no content-index wiring yet, so only
the repo source drives the graph today.
