---
hiker:
  kind: project
sources:
  - kind: repo
    root: /home/bobby/projects/sem/references/hiker/code-intel/fixtures/pyproj
    repo_id: pyproj
    backend: scip
    index: /home/bobby/projects/sem/references/hiker/code-intel/fixtures/pyproj.scip
    scope:
      include: ["pyproj/**"]
      exclude: ["**/.venv/**", "tests/**"]
  - kind: docs
    root: /home/bobby/projects/sem/references/hiker/code-intel/fixtures/pyproj/docs
---

# pyproj — demo project

A tiny Python project (an ABC `Shape` + `Circle`/`Square` impls) bound as a `repo`
source. Open this note from the **Projects** activity to render its entity graph
(blue squares = types, teal circles = methods; orange edges = `implements`).

The `root`/`index` paths are absolute so the SCIP adapter resolves them regardless
of where hiker is launched from.
