"""Single source of truth for the Rust source roots governed by the repo's
structural lint scripts (`check-lengths.py` and `check-splits.py`).

These two scripts previously each carried their own hand-maintained
`RUST_ROOTS` list, and the lists DRIFTED: `hiker-sync` was length-linted
but not split-linted, `egui-workbench` split-linted but not length-linted,
`editor-ts` split-only — so whole production crates (all of `hiker-render`,
`hiker-canvas`, `hiker-llm`, etc.) were invisible to one or both checks.
Consolidating the list here makes that class of gap impossible: a crate is
either governed by both structural checks or neither.

To bring a crate under the file-length cap AND the anti-split checks, add
its `src` dir here. Excluded by intent: dev-only tooling (`tools/*`), the
website crate (`site`). Editing this list is a deliberate posture change,
not an agent's escape hatch.
"""

from __future__ import annotations

RUST_ROOTS = [
    # Application + binaries
    "app/src",
    "cli/src",
    "mcp-server/src",
    # Core library
    "core/src",
    # Editor crates
    "editor/editor-core/src",
    "editor/editor-view/src",
    "editor/editor-egui/src",
    "editor/editor-md/src",
    "editor/editor-diff/src",
    "editor/editor-ts/src",
    "egui-workbench/src",
    # Sync + LLM
    "hiker-sync/src",
    "hiker-llm/src",
    # Embeddable / lite + theming + feature registry
    "hiker-lite/src",
    "hiker-theme/src",
    "hiker-features/src",
    # Canvas
    "hiker-canvas/core/src",
    "hiker-canvas/view/src",
    "hiker-canvas/view-core/src",
    # Render engines (the submodule's crates, each at its root)
    "hiker-render/graph/src",
    "hiker-render/mermaid/src",
    "hiker-render/htmlview/src",
    "hiker-render/math/src",
    "hiker-render/wavedrom/src",
    "hiker-render/chart/src",
    # Widgets + archive reader
    "widgets/graph-widgets/src",
    "zxr/src",
]
