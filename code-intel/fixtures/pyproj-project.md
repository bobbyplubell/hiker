---
hiker.kind: project
sources:
  - kind: repo
    root: fixtures/pyproj
    repo_id: pyproj
    backend: scip
    index: fixtures/pyproj.scip
    scope:
      include: ["pyproj/**"]
      exclude: ["**/.venv/**", "tests/**"]
  - kind: docs
    root: fixtures/pyproj/docs
---

# pyproj — demo project note

A small Python project (an ABC + two implementations) used to exercise the
**external projects** concept end-to-end: this note binds a `repo` source whose
`.scip` index `hiker-projects` resolves and hands to `hiker-code`'s `ScipAdapter`,
which `code-cli graph --project` renders to an SVG. Paths are relative to the
`code-intel/` workspace root so the demo runs from there.
