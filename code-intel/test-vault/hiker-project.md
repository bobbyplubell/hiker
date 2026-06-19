---
hiker:
  kind: project
sources:
  - kind: repo
    root: .
    repo_id: hiker
    backend: scip
    index: hiker.scip
  # NOTE: a `docs` source pointing at hiker's `manual/` was dropped here on purpose.
  # `manual/` lives outside the vault, and the CODE-IN-VAULT trust invariant forbids
  # referencing external paths (hiker only reads inside the vault). The docs source is
  # inert today (no content-index wiring), but leaving an external `root:` would violate
  # the invariant if/when docs binding lands. Re-add it only as an in-vault path.
---

# hiker — code (manual docs source dropped)

A self-referential project binding the repo source whose `.scip` index lives right
here in the vault (`hiker.scip`, ~20k entities) — opening this project renders
hiker's own code-entity graph (scoped to the top hubs by degree).

The `index:` is a **vault-relative** path and `root: .` points at the vault dir, per
the CODE-IN-VAULT trust invariant (hiker only reads inside the vault). The topology
renders from the index alone; source-file previews degrade because the hiker working
tree isn't in the vault — graph-only is the expected outcome.

The `.scip` sitting in the vault also demonstrates the "drop a `.scip` in and open it
directly" path.
