# Code-intel test vault

A throwaway vault for exercising the **Projects** activity + code-graph view.

Launch hiker on this folder:

```
cd /home/bobby/projects/sem/references/hiker
cargo run -p hiker-app -- code-intel/test-vault
```

Then:

1. Click the **Projects** icon in the left activity bar (the `{}` glyph).
2. The sidebar lists `pyproj-demo`, `sem-demo`, and `hiker-project` (discovered by
   their `hiker.kind: project` frontmatter).
3. **Click a project** → its code-entity graph opens in a tab.
   - `pyproj-demo` is small + fully legible (types as squares, methods as circles,
     orange `implements` edges).
   - `sem-demo` (~2.8k entities) and `hiker-project` (~20k — **hiker indexing itself**)
     are large → each scoped to the top ~400 nodes by degree.
   - `hiker-project` binds **two** sources: a `repo` (its `.scip` lives in this vault,
     `hiker.scip`) and a `docs` source pointing at hiker's `manual/` folder. The repo
     drives the graph; the docs source is recorded but not yet wired to anything.
4. Click the **⚙** on a row to edit that project's sources in the UI form.
5. Click **+ New project** to author a fresh project: set a name, **Add source**,
   fill in a repo's `root` + `.scip` `index` (e.g. the absolute paths in
   `pyproj-demo.md`), then **Save project** — it writes a new note under
   `projects/` and opens its graph.

The two demo notes point at the prebuilt `.scip` fixtures in
`code-intel/fixtures/` via absolute paths, so they resolve no matter where hiker
is launched from.
