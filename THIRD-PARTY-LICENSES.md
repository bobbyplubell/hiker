# Third-Party Licenses & Attributions

This file credits external projects whose code, algorithms, or designs are
incorporated into or adapted by hiker — in particular the in-tree editor under
`editor/`. Bundled Cargo dependencies additionally retain their own licenses
(see **Bundled dependencies** at the end).

---

## CodeMirror 6 — MIT

hiker's editor (`editor/editor-core`, `editor/editor-view`, `editor/editor-egui`)
is an **independent Rust engine modeled on CodeMirror 6's architecture** — the
immutable-state / transaction / decoration model — and **adapts specific CM6
algorithms** (notably the change-set `compose` / `map` operations) and a few
behavioral defaults. It is a reimplementation, not a port of the CM6 source.

> Not affiliated with or endorsed by the CodeMirror project. "CodeMirror" is the
> name of that project and is used here only to describe hiker's design lineage.

CodeMirror 6 (`@codemirror/state`, `@codemirror/view`, `@codemirror/commands`)
is MIT-licensed:

```
MIT License

Copyright (C) 2018-2021 by Marijn Haverbeke <marijn@haverbeke.berlin> and others

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in
all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
THE SOFTWARE.
```

---

## Zed — `sum_tree` (Apache-2.0)

`editor/editor-core/src/sumtree.rs` is a Rust **reimplementation of the SumTree**
data structure (a persistent, summary-indexed B-tree) popularized by Zed's
`sum_tree` crate, which was used **only as a reference**. The implementation was
written independently for hiker — its own persistent split/concat, node
capacities, and API surface; it is not a copy of the Zed source.

Zed's `sum_tree` is © 2022–2025 Zed Industries, Inc., licensed under the
**Apache License, Version 2.0**. You may obtain a copy of the License at
<http://www.apache.org/licenses/LICENSE-2.0> (a copy is also vendored at
`references/zed/LICENSE-APACHE`).

---

## tree-sitter — MIT

`editor/editor-ts` uses [tree-sitter](https://tree-sitter.github.io/) for
incremental parsing / syntax highlighting. tree-sitter is © 2018 Max Brunsfeld,
MIT-licensed. Individual grammar crates carry their own (typically MIT) licenses.

---

## Bundled dependencies

The egui UI stack — `egui`, `eframe`, `egui_extras`, `egui_dock`, `egui_tiles`
(and `egui_kittest` for tests) — is dual-licensed **MIT OR Apache-2.0**
(© Emil Ernerfelt and the egui contributors).

All other crates linked into hiker retain their own licenses as declared in
`Cargo.lock`. Regenerate the full bundled-dependency license inventory with:

```
cargo install cargo-about && cargo about generate about.hbs
# or, for a quick summary:
cargo install cargo-license && cargo license
```
