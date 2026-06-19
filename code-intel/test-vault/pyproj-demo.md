---
hiker:
  kind: project
sources:
  - kind: repo
    root: .
    repo_id: pyproj
    backend: scip
    index: pyproj.scip
    scope:
      include: ["pyproj/**"]
      exclude: ["**/.venv/**", "tests/**"]
---

# pyproj — demo project

A tiny Python project (an ABC `Shape` + `Circle`/`Square` impls) bound as a `repo`
source. Open this note from the **Projects** activity to render its entity graph
(blue squares = types, teal circles = methods; orange edges = `implements`).

The `index:` is a **vault-relative** path to the in-vault `pyproj.scip` (the
CODE-IN-VAULT trust invariant: hiker only reads inside the vault). `root: .`
points at the vault dir, so the topology renders from the index alone; source-file
previews degrade because the pyproj working tree isn't in the vault — graph-only
is the expected outcome here.
