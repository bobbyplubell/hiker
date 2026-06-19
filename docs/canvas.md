# Canvas

An editor and renderer for the **JSON Canvas** open format (jsoncanvas.org 1.0): an infinite spatial canvas of nodes (text / file / link / group) connected by edges. A `.canvas` file is a first-class vault document — it opens in its own tab, edits ride the op-log like a note, and it syncs across devices. The work splits an egui-agnostic document core (`hiker-canvas`) and an egui-free view+interaction layer (`canvas-view-core`) from a thin egui shell (`canvas-view`), mirroring the editor's `editor-core` / `editor-view` / `editor-egui` precedent — all under one `hiker-canvas/` directory.

The headline decisions (each detailed in its owning section below):

- **Four layers, mirroring the editor stack**, all under one `hiker-canvas/` directory so the family can be lifted into a standalone repo: `hiker-canvas` (egui-agnostic document core), `canvas-view-core` (egui-free view+interaction), `canvas-view` (thin egui shell + the `NodeContentRenderer` seam), and the `app` canvas panel (tab/nav, op-log binding, injected content engines). [canvas-crate-split]
status:: done
note:: Four layers, mirroring the editor (`editor-core`/`editor-view`/`editor-egui`) rather than the thinner `graph-widgets`. `hiker-canvas` is the egui-free document core (serde model, geometry, `EditOp`s). `canvas-view-core` is the **egui-free** view+interaction layer (camera, edge routing, handle geometry, hit-testing, gesture→`EditOp` decisions, selection, undo) — depends only on `emath` + `hiker-canvas`, unit-tested without a UI. `canvas-view` is the thin egui shell (painter, `CanvasView` widget + `show` loop, pointer plumbing, the `NodeContentRenderer` content seam) and the only crate that depends on `egui`. All three live under one `hiker-canvas/` directory so the family can be extracted as a standalone repo; the content engines (editor-egui / htmlview) stay in the app behind the seam, so no widget crate gains an inter-hiker dep · evidence: `hiker-canvas/core` (crate `hiker-canvas`), `hiker-canvas/view-core` (crate `canvas-view-core`), `hiker-canvas/view` (crate `canvas-view`), `app/src/panels/canvas/` (glue)
- **A `.canvas` file is a first-class op-log document** — the same dirty/save model as a note, under a new `canvas` kind; the canonical JSON *is* the document text edited as `working`, no structural CRDT of nodes (§"Op-log binding"). [canvas-doc-kind, canvas-oplog-binding]
- **Edits localize through canonical JSON serialization** so a single node move is a minimal localized text diff, making concurrent disjoint-node edits merge and same-node edits surface as a conflict hunk (§"Op-log binding", §"Conflict merge"). [canvas-canonical-json, canvas-conflict-merge]
- **Full interactive editor in v1** — select (single/multi/marquee), move, resize, create every node type, draw and re-anchor edges, edit text, pan/zoom; in-session undo/redo, Save commits `working`. [canvas-edit-ops, canvas-selection, canvas-undo-redo]
- **Inserting vault content is the primary workflow** — a file node holds a *pointer* (vault path), never a copy, rendering a live view (§"Interaction"). [canvas-insert-from-vault, canvas-add-to-canvas-verb, canvas-node-create]
- **Node contents reuse the existing engines** through the `NodeContentRenderer` seam, so the widget crates never learn markdown/image/HTML (§"Node content engines"). [canvas-node-content-trait]
status:: done
touches:: [[code:hiker/content]]
note:: `NodeContentRenderer` seam + `DebugContentRenderer`; adapter stays content-engine-free
- **Select / Hand tools with universal pan shortcuts**, and **group containers are first-class, manipulable nodes** (§"Interaction"). [canvas-tool-mode, canvas-group-grab, canvas-group-resize, canvas-group-draw]


## Format model

`hiker-canvas` holds the JSON Canvas 1.0 schema as serde types — the egui-agnostic source of truth every layer reads. [canvas-spec-model]
status:: done
touches:: [[code:hiker/model/node_serde]]
note:: serde JSON Canvas 1.0 (`Canvas`/`Node`/`Edge`); camelCase + `type` tag; `BTreeMap` unknown-field round-trip (test) · evidence: `hiker-canvas/core/src/model.rs` + `src/model/node_serde.rs`

```rust
pub struct Canvas {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

pub struct Node {
    pub id: String,                 // unique within the canvas
    pub kind: NodeKind,             // serde tag: "text" | "file" | "link" | "group"
    pub x: i64, pub y: i64,         // top-left, infinite integer coordinate space
    pub width: i64, pub height: i64,
    pub color: Option<Color>,
}
```

- **Node kinds.** `Text { text }` (markdown), `File { file, subpath }` (vault-relative path + optional `#heading` / `#^block`), `Link { url }`, `Group { label, background, background_style }`. The kind tag and its extra fields flatten into the node object per the spec. [canvas-node-types]
status:: done
note:: text/file/link/group; hand-written serde flattens kind fields beside the `type` tag · evidence: `hiker-canvas/core/src/model.rs` (`NodeKind`)
- **Edges.** `{ id, from_node, from_side?, from_end?, to_node, to_side?, to_end?, color?, label? }` — `*_side` is `top|right|bottom|left`, `*_end` is `none|arrow` (default `to_end = arrow`). An edge whose endpoints don't resolve to live node ids is a dangling edge, surfaced as broken rather than dropped silently. [canvas-edge-model]
status:: done
note:: from/to node+side+end, label, color; dangling edges rendered as broken at the view layer · evidence: `hiker-canvas/core/src/model.rs` (`Edge`, `Side`, `EndCap`)
- **Color.** Either a preset slot `"1".."6"` (mapped to theme tokens at render time, not hard-coded hex) or a `#RRGGBB` literal. One `Color` enum covers both; presets resolve through the active theme so a canvas reads correctly in light and dark. [canvas-color-model]
status:: done
touches:: [[code:hiker/color]]
note:: `Color { Preset(u8) | Hex }`, bare-string serde; presets mapped to theme at paint time

**Unknown-field tolerance.** Unrecognized top-level, node, and edge keys round-trip untouched (serde `flatten` capture), so a canvas authored by another tool isn't lossily rewritten on the first edit — the same posture op-log holds for note bytes ([[spec:op-log-disk-canonical]]).

**Canonical serialization.** `Canvas::to_canonical_json` emits stable key order and fixed pretty-print formatting; node and edge arrays preserve document order, and an edit mutates one element in place. This determinism is load-bearing: it's what makes a node move a localized text diff rather than a whole-file rewrite, which is what makes concurrent edits mergeable in the op-log. [canvas-canonical-json]
status:: done
note:: tab-indented deterministic serialization; idempotence + localized-diff tests (node move = 2 changed lines) · evidence: `hiker-canvas/core/src/canonical.rs`


## Geometry and edit operations

Both egui-free, in `hiker-canvas`, so they're testable without a UI and reusable by a future headless consumer (export, MCP).

- **Geometry.** Node bounds, point/rect hit-testing, top-most-at-point with array order as z-order (later in the array paints on top), canvas content bounds (for zoom-to-fit), and the per-side anchor points an edge attaches to. [canvas-geometry]
status:: done
touches:: [[code:hiker/geometry]]
note:: `Point`/`Rect`, `node_bounds`/`node_anchor`/`hit_test` (z-order)/`content_bounds`; egui-free
- **Edit operations.** Pure `Canvas -> Canvas` (or in-place mutation) verbs: `move_nodes`, `resize_node`, `add_node`, `remove_nodes` (with cascade removal of incident edges), `add_edge`, `set_edge_endpoint`, `set_text`, `set_color`, `set_label`. Each returns enough information for the binding to re-serialize and for undo to invert it. No verb reads or writes any referenced note. [canvas-edit-ops]
status:: done
note:: move/resize/add/remove/connect/set-text-color-label + `SetEdgeLabel`; `apply`+`invert`; remove cascades incident edges (tests) · evidence: `hiker-canvas/core/src/ops.rs` (`EditOp`)


## Render adapter

`canvas-view` (`hiker-canvas/view`) — the thin egui shell over the egui-free `canvas-view-core`, depending only on egui (plus the two canvas crates). It paints and routes egui input into `canvas-view-core`'s hit-testing / gesture logic; the host owns the clip rect, the same division `zim.md` uses for the htmlview tab. [canvas-render-widget]
status:: done
note:: `CanvasView::show(ui, &mut Canvas, &mut dyn NodeContentRenderer) -> CanvasResponse`; host owns clip. The view paints the scene, then allocates its click/drag interaction surface ON TOP of node content (egui gives the topmost widget pointer priority), so the canvas — not the content widgets — owns drag/select/resize/connect; node content is therefore display-only. The surface spans the available rect (below the host header), never the full clip, so it doesn't cover the toolbar · evidence: `hiker-canvas/view/src/widget.rs`

**Card content is display-only.** A read-only card renders its body through the content engine in the editor widget's non-interactive (`interactive(false)`) mode, and the canvas allocates its click/drag interaction surface ON TOP of that content. So the canvas — never the content widget — owns resize / move / select / connect, and dragging a node across a card never text-selects inside it. This is the load-bearing contract that lets the spatial gestures work uniformly over every node kind. [canvas-render-widget]

- **Pan and zoom.** An infinite canvas with a viewport transform (pan offset + scale). Panning is the Hand tool, a held **Space**-drag, a **middle-mouse** drag ([[spec:canvas-tool-mode]]), or a plain scroll when `[ui].canvas_scroll_mode` resolves to pan ([[spec:canvas-scroll-mode]]). A plain scroll pans or zooms per that setting (default **auto**: mouse wheel zooms, touchpad pans); **Ctrl/Cmd+scroll always zooms** to the cursor, as does a trackpad **pinch** (egui surfaces both as `zoom_delta`, one code path). Pinch works on macOS via stock winit and on Linux/Wayland via the pinned winit fork's `zwp_pointer_gestures_v1` backport (`[patch.crates-io]`); Ctrl/Cmd+scroll is the fallback elsewhere. Zoom clamps to a gesture range. "Zoom to fit" frames all content via [[spec:canvas-geometry]]'s content bounds — capped above by the gesture max but NOT floored by the gesture min, so oversized content (e.g. a large tree) zooms out past the floor to frame fully rather than overflow; the next gesture re-clamps. Camera state is view state, never in the op-log. [canvas-pan-zoom]
status:: done
note:: viewport transform, zoom-toward-cursor (clamped 0.05–20×), zoom-to-fit; camera not serialized · evidence: `hiker-canvas/view-core/src/camera.rs`
- **View state persists across close/reopen and restart.** Camera pan/zoom + per-card scroll/zoom survive a tab close→reopen and an app restart via the tab-state store keyed by canvas path (not the `.canvas` file). Captured on tab close and the exit/autosave snapshot, applied once per pane on first creation (a restored camera wins over fresh-create framing). [canvas-view-state-persist]
status:: done
implements:: [[code:hiker/panels/canvas/new_file_pointer]], [[code:hiker/panels/canvas/apply_persisted_view]], [[code:hiker/panels/canvas/render/canvas_body]], [[code:hiker/bootstrap/impl#[AppState]restore_tab_state]]
touches:: [[code:hiker/autosave]], [[code:hiker/editor_pane]]
note:: camera pan/zoom + per-card scroll/zoom persist across tab close→reopen AND restart via the tab-state store (NOT op-log / `.canvas`), keyed by canvas path. Serde round-trip + `#[serde(default)]` keeps old snapshots loadable (tests); camera `set_pan_scale` clamps to the zoom bounds; `view_snapshot`/`restore_view` round-trip (tests). Four wiring points: startup restore copies `ts.canvas_views` into the session map; apply-on-create restores once per pane and suppresses fresh-create `fit_pending`; capture-on-close snapshots before the pane is dropped; capture-on-persist snapshots every open canvas tab into the snapshot. Cross-session restart verified only via the serde round-trip + snapshot/restore unit logic — live close/reopen + restart is user-verified · evidence: `core/src/autosave.rs` (`CanvasViewState`/`CardViewState` + `TabState.canvas_views`), `hiker-canvas/view-core/src/camera.rs` (`set_pan_scale`), `hiker-canvas/view/src/widget.rs` (`view_snapshot`/`restore_view`), `app/src/state.rs` (`Session.canvas_views`), `app/src/panels/canvas/mod.rs` (`apply_persisted_view`/`capture_view` + conversions), `app/src/panels/canvas/render.rs` (apply-on-create), `app/src/editor_pane.rs` (capture-on-close), `app/src/main.rs` (capture-on-persist), `app/src/bootstrap.rs` (startup restore)
- **Edge routing.** Edges draw from the `from_side` anchor to the `to_side` anchor as curved connectors with arrowheads per `*_end`; when a side is unspecified the router picks the facing side from relative node positions. Edge labels paint at the midpoint. [canvas-edge-routing]
status:: done
touches:: [[code:hiker/edges]]
note:: cubic Bézier side-anchored connectors + arrowheads per `*_end`; facing-side fallback; midpoint labels
- **Node frames.** Each node paints a rounded card: border + fill from its `Color`, group nodes paint a translucent background + label behind their members, selection paints handles. Node *content* is delegated (see below) — the frame layer never knows what a text or file node contains. [canvas-node-frame]
status:: done
touches:: [[code:hiker/palette]]
note:: rounded card border/fill from `Color`; group bg+label; selection + resize handles · evidence: `hiker-canvas/view/src/paint.rs`, `src/palette.rs`
- **Background grid.** An optional dotted/grid background scaled with zoom, for spatial reference. [canvas-grid-background]
status:: done
note:: dotted background scaled with zoom · evidence: `hiker-canvas/view/src/paint.rs`
- **Viewport culling.** Only nodes and edges intersecting the visible viewport paint and hit-test, so a large canvas pays for what's on screen — the same viewport-scoping discipline the editor widgets use ([[spec:widget-render-viewport-scoped]]). [canvas-viewport-cull]
status:: done
note:: only nodes/edges intersecting the clip rect paint + hit-test. `node_card` also hands the content engine a child ui clipped to the *viewport* (not just the card's inner rect), and the engine intersects its own clip with it, so a card straddling the pane edge never paints its body over the header / tabs / neighbouring panels · evidence: `hiker-canvas/view/src/paint.rs`
- **Level-of-detail placeholder.** Below a readable on-screen size a card skips the content engine and paints a cheap placeholder — a one-line title (file basename / first text line / link host) plus skeleton bars — keeping a zoomed-out many-document canvas smooth (geometric culling alone leaves every card "visible" at fit). A placeholder has no scrollable content, so a wheel over one passes through to camera zoom ([[spec:canvas-card-scroll]]). [canvas-lod-placeholder]
status:: done
note:: below ~150px on-screen a card skips the content engine and paints a cheap title + skeleton placeholder; collapsed zoom-to-fit on 121 nodes from 81ms to 0.04ms/frame (profiled via `tools/profile-canvas`). A wheel over a placeholder (no scrollable content) passes through to camera zoom via the shared `is_tiny` check, so a tiny card crossing the cursor mid-zoom-out doesn't stall the zoom · evidence: `hiker-canvas/view/src/paint.rs` (`is_tiny`/`lod_title`/`paint_lod_placeholder`, `node_card` branch), `hiker-canvas/view/src/widget.rs` (`handle_zoom` wheel routing)
- **Paint-only static render.** A `show_static` entry point paints the SCENE only (grid, group backgrounds, edges, cards / LOD placeholders) sharing the interactive `show`'s scene-paint helper but allocating no interaction surface, reading no input, painting no overlays/handles, committing nothing — a display-only render for previews/thumbnails safe inside a non-interactable `egui::Area` (it can't steal pointer hover). Paired with the zero-cost `NoContentRenderer`, at fit/thumbnail zoom every node is a LOD placeholder so the `!Send` content engine is never invoked. The hover-preview live-paint (`previews.md`) is the first consumer. [canvas-static-paint]
status:: done
implements:: [[code:hiker/widgets/preview/ThumbnailProvider#expanded_paint]]
touches:: [[code:hiker/content]]
note:: A paint-only display render: `show_static(ui, &Canvas, &mut dyn NodeContentRenderer)` paints the SCENE only (grid + group backgrounds + edges + cards / LOD placeholders) at the current camera with NO interaction — no `ui.interact` surface, no input (zoom/keys/pointer), no overlays/handles/context menu, nothing committed. It calls the same `paint_scene` the interactive `show` does (one helper, no duplication, keeps `show` under the length budget). Safe inside a non-interactable `Area`: registers no interactive widget, so it can't steal row hover. Zero-cost `NoContentRenderer` (paints nothing, echoes card scroll) lets a caller skip the `!Send` per-node content engine — at fit/thumbnail zoom every node is a LOD placeholder, so the renderer is never invoked anyway. First consumer: the canvas expanded hover preview ([[spec:preview-canvas-thumbnail]]). Tested via `egui_kittest` (drives one frame, asserts the ctx wants no pointer/keyboard input) + a `NoContentRenderer` scroll-echo test · evidence: `hiker-canvas/view/src/widget.rs` (`CanvasView::show_static`, shares the private `paint_scene` helper with `show`), `hiker-canvas/view/src/content.rs` (`NoContentRenderer`)


## Interaction

Driven by the adapter, applied through `hiker-canvas`'s edit ops, committed by the app's op-log binding.

- **Tool modes.** Two interaction tools, toggled from the toolbar or with `V` (Select) / `H` (Hand). **Select** (default) routes a left-drag by what's under the cursor — empty canvas marquee-selects, a node body moves, a resize handle resizes, a connector handle draws an edge. **Hand** routes every left-drag to a camera pan and suppresses select/move/marquee. Independent of the tool, holding **Space** during a drag or dragging with the **middle mouse button** pans, and the cursor shows the grab / grabbing hand while it does. The active tool is `CanvasView` view state — it never enters the op-log or the `.canvas` file, the same posture as the camera. [canvas-tool-mode]
status:: done
touches:: [[code:hiker/widget/pointer]]
note:: Select (default) / Hand toolbar toggle + `V`/`H` keys (guarded against typing via egui focus + label_edit); pure `press_action` routes pan/marquee/select; Space-drag pans via input read, middle-drag pans via raw `middle_down` (egui drag senses primary only); grab/grabbing cursor; tool is view state · evidence: `canvas-view-core/src/state.rs` (`Tool`/`press_action`), `canvas-view/src/widget.rs` (`tool`/`handle_tool_keys`/`handle_middle_pan`/`apply_pan_cursor`), `canvas-view/src/widget/pointer.rs` (`on_press`)
- **Selection.** Click selects a node or edge; shift-click extends; under the Select tool a drag on empty canvas marquee-selects; Esc / empty-click clears. Multi-select moves and deletes as a group. [canvas-selection]
status:: done
touches:: [[code:hiker/widget/pointer]]
note:: click / shift-click / marquee; multi-select group move+delete. Selecting a group folds its geometric members into the selection at select-time (they paint as selected), so the move-set is visible and decided once · evidence: `hiker-canvas/view-core/src/state.rs`, `src/widget/pointer.rs`, `interaction.rs` (`group_member_ids` fold)
- **Move.** Drag a selection to reposition. A group node is grabbed by its **label/header strip** (a band along its top edge) rather than its interior, so a drag inside the frame still targets the framed children. Selecting a group folds in its geometric members (the nodes whose bounds sit inside the group's rect) at *select-time*, so the selection visibly shows everything that will move; the moved set is exactly that frozen selection and is never recomputed mid-drag, so dragging a group past another group never grabs the other's cards. [canvas-node-move, canvas-group-move, canvas-group-grab, canvas-selection]
- **Resize.** Eight handles on a single selected node — including group nodes — rewrite `width`/`height` (and `x`/`y` for top/left handles). Resizing a group reframes the container only; its members keep their positions. [canvas-node-resize, canvas-group-resize]
- **Hover affordance on handles.** When the pointer is in range of an interactive handle it gets a subtle hover indicator so the grab target is obvious before pressing: a resize handle and a connector handle grow slightly under the cursor, and a group's header grab-strip ([[spec:canvas-group-grab]]) highlights (a brighter band) on hover. Pure visual feedback driven by the same hit-tests the press path uses — no state change. [canvas-handle-hover]
status:: done
touches:: [[code:hiker/handles]]
note:: `handle_hover` resolves the hovered resize handle, connector `(node, side)`, and group-header id once per frame from the **same** press-path hit-tests (`single_selected_handle` / `hovered_side_handle` / `group_header_hit`), threaded into the paint calls; only when idle/connecting and the pointer is over the viewport. Hovered resize square and connector circle grow `HOVER_GROW` = 1.3x about their center; the hovered group's header band brightens (`gamma_multiply` 0.22→0.4). Purely visual, no state change. `grown_about_center` unit-tested (grows + stays centered, identity at 1.0) · evidence: `hiker-canvas/view/src/widget.rs` (`HandleHover` + `CanvasView::handle_hover`), `hiker-canvas/view/src/paint.rs` (`resize_handles` / `connector_handles` / `group_backgrounds`), `hiker-canvas/view-core/src/handles.rs` (`grown_about_center` + `HOVER_GROW`), `interaction.rs` (`single_selected_handle` now `pub`)
- **Create elements.** The toolbar's create control is a `+` split-button (the shared [[spec:split-add-button]]). The primary `+` click mints a new vault note ([[spec:canvas-new-note]]); the caret opens the remaining insert verbs — **Add text** (empty text node, enters edit), **Insert from vault…** ([[spec:canvas-insert-from-vault]]), **Add link…** (URL prompt), **Add group** ([[spec:canvas-group-draw]]). Each drops at the viewport center immediately (no arm-then-place) and selects the new node. The Select/Hand toggle sits beside it; Fit-to-content lives in the View menu ([[spec:canvas-view-toggle]]). [canvas-node-create]
status:: done
implements:: [[code:hiker/panels/canvas/render/link_prompt_body]]
touches:: [[code:hiker/widgets/split_button]]
note:: toolbar create control is a `+` split-button ([[spec:split-add-button]]): primary `+` mints a new vault note ([[spec:canvas-new-note]]); caret dropdown offers Add text / Insert from vault… / Add link… / Add group. One-click drop at viewport center + auto-select; Text/Group immediate, Link via inline URL prompt; `insert_node_centered` is the primitive the vault-insert path reuses. Select/Hand toggle sits beside it; Fit moved to the View menu ([[spec:canvas-view-toggle]]) · evidence: `hiker-canvas/view/src/widget.rs` (`create_centered`/`insert_node_centered`/`consume_pending`), `app/src/panels/canvas/render.rs` (`create_toolbar`), `app/src/widgets/split_button.rs`
  - **Insert from vault** opens an autocomplete picker (`autocomplete.md`) over vault notes + sources; choosing one drops a *file-node pointer* (vault path only, never content) rendering the referenced content. A right-click **Add to canvas** verb on a file-tree row or multi-selection does the same against a chosen target canvas ([[spec:board-add-card]] shape); file-tree drag is deferred ([[spec:canvas-dnd-add]]). [canvas-insert-from-vault, canvas-add-to-canvas-verb]
  - **New note** mints a fresh vault note (shared `create_new_note` — suffix-counted `new-note-N.md`, indexed, no tab) and drops a `File` pointer ready to inline-edit. Three entry points: the `+` primary click, a right-click empty-canvas **New note** verb, and **Cmd/Ctrl+N** while a canvas tab is active (Cmd/Ctrl+N elsewhere opens a new note in a tab). [canvas-new-note]
status:: done
implements:: [[code:hiker/keybinds/impl#[AppState]handle_keybinds]], [[code:hiker/panels/canvas/add_file_node]], [[code:hiker/panels/canvas/render/render_overview]]
touches:: [[code:hiker/actions]]
note:: right-click empty-canvas "New note" verb + Cmd/Ctrl+N (when a canvas tab is active) mint a vault note via `create_new_note` and drop a `File` pointer at the viewport center (`insert_node_centered`), ready to inline-edit; both paths share `new_note_on_canvas`. The `file.new_note` action is context-dependent: Canvas tab → on-canvas; else → new note tab (`AppState::new_note`). Registered in the action registry + the keybind help catalog (keybinds tests pass) · evidence: `app/src/panels/canvas/mod.rs` (`new_note_on_canvas` + `new_file_pointer`), `app/src/panels/canvas/render.rs` (`request_new_note` handler), `app/src/actions.rs` (`file.new_note` action), `app/src/keybinds.rs` (`Mod-N` chord + catalog row)
- **Canvas settings menu.** A gear dropdown at the toolbar's right edge surfaces the canvas interaction settings that otherwise live in the global Settings window: the **scroll mode** Auto / Pan / Zoom selector ([[spec:canvas-scroll-mode]]) and the **two-finger swipe navigates Back/Forward** toggle ([[spec:navigation-swipe-disable]]). Each reads/writes the live `[ui]` config key (shared `set_setting`), taking effect next frame; the scroll-mode selector is one shared widget rendered here and in Settings, so they never drift. The gear is in the in-tab toolbar, so reader-mode toolbar-hiding ([[spec:view-reader-hide-toolbar]]) hides it. [canvas-settings-menu]
- **Create a group by drawing it.** The "Add group" verb arms a one-shot draw mode; the next left-drag on empty canvas rubber-bands the group's rectangle (live preview) and creates the group at those bounds on release, the standard container gesture. A bare click (no drag) drops a default-sized frame instead, so the one-click path still works. Arming the draw temporarily overrides the Select tool's marquee for that one creation. [canvas-group-draw]
status:: done
touches:: [[code:hiker/widget/pointer]]
note:: "Add group" (toolbar + context menu) arms a one-shot draw; the next empty drag rubber-bands the rect (live preview in `paint_drag_overlay`) and creates the group on release (normalized, min-size clamped); a bare click drops a default-sized frame · evidence: `canvas-view/src/widget/pointer.rs` (`on_press`/`finish_group_draw`/`on_click`), `canvas-view-core/src/state.rs` (`add_group_op`/`normalize_draw_rect`, `Interaction::DrawGroup`)
- **Edges.** Hovering (or selecting) a node reveals four connector handles — small circles floating just outside its edges, clear of the resize handles. Clicking one starts a click-to-connect gesture (a rubber band tracks the cursor); the next click on a node attaches the edge. Press-dragging a handle connects the same way in one gesture; dragging an existing edge endpoint re-anchors it to a different node/side; dropping on empty canvas cancels. Double-clicking an edge opens an inline label field at its midpoint. [canvas-edge-draw, canvas-edge-redirect, canvas-edge-label]
- **Card content: scroll & zoom.** A card is a fixed window into its content, *decoupled from camera zoom* — the body renders at a per-card content zoom (font multiplier, default 100%) so text stays readable at any board zoom. The wheel over a card scrolls its content (clamped to content height); over empty canvas it zooms the camera. Scroll position is stable across content changes. Per-card zoom is adjusted from the card's right-click menu (Zoom in / out / Reset) or Ctrl/Cmd+wheel. [canvas-card-scroll, canvas-card-zoom]
- **Inline editing.** Cards are editable in place (read-only by default). **Enter edit mode** via one seam (`try_enter_edit`, only on an editable full-detail card) by three gestures: **double-click**; **click an already-sole-selected node** (Finder-rename click-again, via the widget's `edit_requested`); or **Enter / F2 with a single editable node selected** (only when no field holds focus, so it can't steal Enter from the edge-label editor). The body becomes a focused editor capturing keyboard + pointer (an overlay tracking the node's screen rect on pan/zoom), opened at the current scroll position, with a **bright accent outline** marking the active editor. Exits on Esc, click outside, selecting another node, or scrolling off-screen. Two write paths by kind: [canvas-inline-edit]
status:: done
implements:: [[code:hiker/panels/canvas/edit/is_editable]], [[code:hiker/panels/canvas/edit/press_outside]], [[code:hiker/panels/canvas/content/Engine#live_text]], [[code:hiker/panels/canvas/render/resolve_edit_overlay]], [[code:hiker/panels/canvas/render/persist_canvas]], [[code:hiker/panels/canvas/render/handle_canvas_save]]
note:: cards editable in place: double-click a full-detail File/Text card → foreground `Area` overlay over the node rect (captures kbd+pointer, tracks rect via `node_screen_rect` on pan/zoom; Esc / click-out / select-other / off-screen exit). Overlay seeds at the card's current scroll (`card_scroll`), no jump to top. Entry gestures (2026-06-03): double-click, **click-again on the already-sole-selected node** (widget `CanvasResponse.edit_requested`), and **Enter/F2 on a single selected editable node** (app-side, focus-gated), all through one `try_enter_edit` seam; a bright accent outline marks the active overlay. Save routing (`handle_canvas_save`, 2026-06-03): Ctrl/Cmd+S **always commits the canvas document** AND saves a single SELECTED File node's note buffer (keyed on selection via `editing_file_path`, not just inline-edit, so re-selecting after click-out still flushes); `save_canvas_document` toasts only on a real `Ok(true)` commit (a no-op no longer fakes a save). [bug-canvas-inline-edit-discoverability] File node → edits the shared note buffer via [[spec:embedded-buffer-view]] (typing shows in any tab, one dirty buffer); Text node → transient editor reconciled to its own `text` via [[spec:canvas-edit-ops]] `SetText` through the `persist_canvas` op-log path. Per-edit `EmbeddedView` / transient `Editor` parked in a `TabId`-keyed thread-local (off `AppState`) so `show_embedded_buffer` holds `&mut app` + `&mut embed` without aliasing — mirrors `content::PANES`. Read path unchanged (cards read live `working` buffer; LOD placeholder double-click opens a tab via the existing `activate_node`). Diagram preview-while-typing added to the [[spec:embedded-buffer-view]] primitive (disjoint `buffers[path]` immut / `panels.edit_preview` mut), so every embed gets it; the buffer tab doesn't use the primitive, so no double-fire. Context-follows-edit: while a File node is in inline-edit, `canvas::inline_edited_note(app, tab_id)` (reads `Pane::editing` + the node's `file`) surfaces the edited note as the activity `Ctx::active_path` (override in `activity::with_ctx`, ahead of the canvas tab's own `.canvas` path), so the context panel (backlinks / related / appears-in) tracks the note you're editing on the canvas rather than the `.canvas` file. Verified: `cargo test -p hiker-app -p canvas-view` 237 pass; clippy clean in touched files (`-D` budget lints); `profile-canvas` zoom-to-fit 0.06ms p50 · evidence: `app/src/panels/canvas/edit.rs` (overlay + `EDIT_VIEWS` thread-local + `forget`/`enter`/`is_editable`/`show_overlay`); `render.rs::activate_or_edit` / `resolve_edit_overlay` / `render_edit_overlay` / `persist_text_edit`; `Pane::editing` in `mod.rs`; `widget.rs::node_screen_rect` / `is_node_lod`; `buffer_view.rs` edit-preview popup + `EmbedOpts::focus`; `edit::forget` in `editor_pane::close_tab`
  - **File node** → edits the referenced note through the reusable embedded buffer view ([[spec:embedded-buffer-view]]): the card attaches to the one shared `session.buffers[path]` editor, so typing on the canvas shows in any open tab of that note (and vice versa), there is only one dirty buffer, and save / autosave / agent-review work identically. The card keeps its own scroll/zoom; cursor and undo are the note's.
  - **Text node** → edits the node's own `text` (which lives in the `.canvas` itself, not a vault note), committing through [[spec:canvas-edit-ops]] `SetText` on the canvas document — no shared-buffer machinery needed.
  - **Read path.** Even outside edit mode, a file-node card reads the *live* shared buffer when one is loaded (showing a note's unsaved edits), falling back to disk otherwise — so an open, dirty note never looks stale on the canvas. A **LOD placeholder** ([[spec:canvas-lod-placeholder]]) is too small to edit, so double-clicking one opens the note in a tab instead of entering edit mode.
  - **Save routing.** Ctrl/Cmd+S while editing a File card saves *that note's* buffer (folding its `working` layer to disk); otherwise — including while editing a Text node, whose text lives in the `.canvas` — it commits the canvas document. The chord is consumed in the Canvas view so it doesn't double-fire with any global save.
- **Context menu.** Right-click dispatches on what it hits: a node → content zoom + Delete; an edge → Edit label + Delete; empty canvas → the toolbar verbs (Add text / Add link… / Insert from vault… / New note / Add group / Fit to content), so the toolbar is reachable without leaving the canvas. [canvas-context-menu]
status:: done
note:: Right-click dispatches by target: a node → content zoom + **Delete**; an edge → **Edit label** + **Delete**; empty canvas → the toolbar verbs (Add text / Add link… / Insert from vault… / Add group / Fit to content). Link/vault-insert need host UI, so they're reported as requests in `CanvasResponse`; the rest act in the widget. Anchored at the right-click position via `menu_anchor` · evidence: `hiker-canvas/view/src/widget.rs` (`show_context_menu`)
- **Delete.** Delete/Backspace removes the selection; removing a node cascades its incident edges. Edges are selectable (the hit-test samples the drawn Bézier, so a bowed connector is clickable) and deletable via Delete or the right-click Delete verb, as are nodes. **The canvas key shortcuts (Delete/Backspace, Ctrl-Z / -Shift-Z, Esc) act on canvas nodes only when no text editor holds keyboard focus**: while a card is in inline-edit mode (or the edge-label editor is open) the focused editor owns those keys, so Backspace deletes text rather than the node. [canvas-delete]
status:: done
touches:: [[code:hiker/interaction]]
note:: Delete/Backspace removes the selection (nodes cascade their edges; test). The canvas key shortcuts (Delete/Backspace, Ctrl-Z / -Shift-Z, Esc) act on nodes ONLY when no text editor holds focus: `handle_keys` early-returns when `label_edit.is_some()` or egui reports a focused widget, so an in-edit card's Backspace deletes text, not the node. Edges are selectable by clicking the curve (hit-test samples the actual Bézier, not the chord, so bowed edges are clickable) and removable via Delete or the right-click **Delete** verb; nodes likewise have a right-click **Delete** · evidence: `hiker-canvas/view-core/src/interaction.rs`, `hiker-canvas/view/src/widget.rs` (`handle_keys` focus early-return + context menu)
- **Undo/redo.** An in-session stack of inverse edit ops (Ctrl/Cmd-Z / -Shift-Z). Distinct from op-log history: undo operates on the in-memory model before Save; once committed, op-log version history ([[spec:op-log-history-materialization]]) is the durable record. [canvas-undo-redo]
status:: done
note:: in-session inverse-`EditOp` stack (Ctrl/Cmd-Z / -Shift-Z); new edit clears redo (test) · evidence: `hiker-canvas/view-core/src/state.rs` (`UndoStack`)


## Auto-arrange

A toolbar / context-menu **Auto-arrange** ("Tidy") verb lays the canvas out hierarchically using the same dagre (layered/Sugiyama) engine the graph view uses (`hiker-graph::LayeredEngine`).

- **Pure, egui-free, op-log-routed.** `tidy::auto_arrange(&Canvas, ArrangeOpts) -> Vec<EditOp>` (`hiker-canvas/core/src/tidy.rs`) maps the canvas onto the layered engine and returns one `SetNodeRect` edit op per node that actually shifts, so the moves drive through the same op-log / undo pipeline as any other edit. Rank direction (top-down / bottom-up / left-right / right-left) and rank/node separation are options. The result is translated so its bounding-box center lands on the original content's center, keeping the board roughly where the user was looking. [canvas-auto-arrange]
status:: done
touches:: [[code:hiker/panels/canvas/menu]], [[code:hiker/tidy]]
note:: pure egui-free dagre "Tidy": maps the canvas onto `hiker_graph::LayeredEngine`, returns one `SetNodeRect` per shifted node (op-log/undo path); rank dir + sep options; result re-centered on the original content · evidence: `hiker-canvas/core/src/tidy.rs` (`auto_arrange`/`ArrangeOpts`/`RankDirection`), `app/src/panels/canvas/menu.rs` (`EmptyMenuAction::AutoArrange`)
- **Groups become dagre clusters.** A `Group` maps to a dagre cluster: leaf membership is geometric (a leaf belongs to the smallest-area group whose bounds contain its center; a group inside a larger group nests as that group's dagre parent), fed in as `node_parents`. Each group frame is resized to the engine-computed cluster rectangle so it wraps its members after the arrange. [canvas-auto-arrange-groups]
status:: done
touches:: [[code:hiker/tidy]]
note:: a `Group` maps to a dagre cluster; geometric leaf membership (smallest containing group, nested) fed as `node_parents`; each group frame resized to the engine's cluster rectangle · evidence: `hiker-canvas/core/src/tidy.rs` (`node_parents`, group cluster rect)


## Node content engines

Node *frames* are the adapter's job; node *contents* reuse hiker's existing renderers, wired by the app so the adapter stays free of the heavy engine deps. [canvas-node-content-trait]

The adapter calls a host-supplied `NodeContentRenderer` with the node and its on-screen rect; the app implements it by dispatching on kind:

- **Text node → markdown.** Rendered through the editor widget (`editor-egui`) with the live-preview decoration providers (`live-preview.md`), so a text node gets the same markdown rendering — and, in edit mode, the same editing — as a note buffer. Editing a text node writes back through [[spec:canvas-edit-ops]] `set_text`, not through a separate op-log document. [canvas-text-node-markdown]
status:: done
implements:: [[code:hiker/panels/canvas/edit/show_text_edit]], [[code:hiker/panels/canvas/content/plan_node]]
note:: text nodes render markdown via the read-only `editor-egui` widget driven by the buffer panel's FULL decoration pipeline (`rebuild_editor_decorations` with `render_widgets: true`), so cards show the rendered math / Mermaid / WaveDrom / table widgets (not raw fences); per-pane `DecorationCache` memoizes layers; `!Send` panes cached per `(tab, node id)` (zim pattern). Widget *interactivity* on cards (click-into-cell, mermaid links, edit-preview popup) is not wired — read-only render only · evidence: `app/src/panels/canvas/content.rs` (`card_decoration_ctx`, `paint_editor`)
- **File node → embed.** Embeds the referenced vault file by type: a markdown note (or the extract sidecar for a PDF / audio / office source) renders as live-preview markdown (optionally scoped to the `subpath` heading/block), an image renders the image, a vault-internal `.html` / captured page renders through `hiker-htmlview` (the `htmlview-render` surface, same no-JS engine the ZIM viewer uses — consistent with `extract-web-no-js-stance`), and a code/text source renders read-only. The path resolves against the vault; an unresolvable path renders a broken-reference card, the kanban [[spec:board-card-references]] posture. [canvas-file-node-embed]
status:: done
implements:: [[code:hiker/panels/canvas/content/plan_node]], [[code:hiker/panels/canvas/content/markdown_plan]]
note:: file node → image / `.md` / `.html` (htmlview) / extract-sidecar markdown / code-as-text / broken-ref card; `#Heading` subpath sliced, `#^block` falls back to whole-file
- **Link node → card.** A link node is a card showing the URL with an open glyph. Because the canvas interaction surface owns pointer input (node content is display-only), opening is a *double-click* activation: the host opens the URL in the OS browser (`ctx.open_url`); double-clicking a file node opens the referenced file in a tab. Hiker does **not** live-fetch or render an external web page inside a canvas — a vault-internal `.html` page is the separate file-node case above. [canvas-link-node-card]
status:: done
implements:: [[code:hiker/panels/canvas/content/plan_node]], [[code:hiker/panels/canvas/render/canvas_body]]
note:: link node is a card (URL + glyph); no live web fetch by design. Now that the interaction surface owns pointer input, the card is display-only and opening is the host's double-click *activation* (`CanvasResponse::activated`): a link node opens via `ctx.open_url`, a file node opens in a tab (`.canvas` → canvas view, else `open_file`). (Renamed from `canvas-link-node-web`; vault-internal `.html` rendering moved under [[spec:canvas-file-node-embed]].) · evidence: `app/src/panels/canvas/content.rs` (`paint_link`), `app/src/panels/canvas/render.rs` (`activate_node`)

The seam keeps the swappability the project favors: a different content engine for any kind is an app-side change behind one trait, and the adapter/core never learn about markdown, images, or HTML.


## Vault, tab, and navigation

- **Document kind.** A `.canvas` file is an op-log document under a new `canvas` `meta.kind`. Bootstrap seeds it by path alongside `.md` notes ([[spec:op-log-path-identity]]) — no `doc_id` is minted; the canonical JSON bytes are its `accepted` text. It rides the same plain-text layered model as a note: edited as `working`, committed on Save, local history as plain-file snapshots ([[spec:plain-file-snapshots]]). [canvas-doc-kind]
status:: done
implements:: [[code:hiker/oplog/doc/kind_for]], [[code:hiker/ops/op_writes/kind_for]]
note:: doc minted lazily by `user_save` on first edit; `meta.kind = "canvas"`; indexer correctly skips `.canvas` (not markdown-chunked) · evidence: `core/src/ops/op_writes.rs` (`kind_for` `.canvas → "canvas"`)
- **Tab.** `TabKind::Canvas { path }` — a per-doc tab (not a singleton), opened by clicking a `.canvas` row in the file tree. Dispatched by `tabs::body` like any other tab kind. [canvas-tab]
status:: done
implements:: [[code:hiker/tab/TabKind#Canvas#path]], [[code:hiker/tab/impl#[Tab]persist_key]], [[code:hiker/bootstrap/impl#[AppState]restore_tab_state]], [[code:hiker/state/MutationEvent]]
touches:: [[code:hiker/panels/canvas]], [[code:hiker/workbench_host]]
note:: per-doc tab; label/icon/persist (`canvas:<path>`)/restore wired · evidence: `app/src/tab.rs` (`TabKind::Canvas { path }`), `app/src/workbench_host.rs`, `app/src/bootstrap.rs`
- **View toggle.** An eye-icon **View** menu in the header flips the pane between the spatial editor and the standard editor widget over the raw `.canvas` text (JSON highlighting via the existing `tree-sitter-json`), both over the one op-log document — the [[spec:board-view-toggle]] shape. The menu also carries the canvas-only **Fit to content** action. [canvas-view-toggle]
status:: done
implements:: [[code:hiker/panels/canvas/Pane#view]], [[code:hiker/panels/canvas/render/header]]
touches:: [[code:hiker/panels/canvas/render]]
note:: eye-icon **View** menu: Canvas / JSON mode switch (JSON branch hosts the editor widget inline over the same op-log doc, mirrors [[spec:board-view-toggle]]) + the canvas-only Fit-to-content action · evidence: `app/src/panels/canvas/render.rs` (`header`, `view_menu`)
- **Op-log binding.** The canvas uses the **same dirty/save model as the text editor** ([[spec:op-log-layered-model]]): edit → dirty → Ctrl+S → commit. The host mirrors a spatial edit into the document's `working` text exactly as the editor mirrors a change set: in Canvas view an edit re-serializes the model ([[spec:canvas-canonical-json]]) and **diffs the new JSON against the current `working` text into a minimal localized text replace** via `OpLog::replace_working` (a working-layer text op over the changed span). The diff must stay localized, NOT a whole-span remove-all+insert-all — a full-span replace makes a concurrent diff mis-anchor: it rewrites the whole document, so an external disjoint edit no longer lands as a clean disjoint hunk (the canvas-corruption hazard). The buffer goes dirty (editable text follows the JSON in lockstep; saved baseline untouched). Ctrl+S commits — `commit_working` folds `working` into `accepted`, atomically rewrites the `.canvas` ([[spec:op-log-atomic-write]]), advances the baseline, and writes a snapshot on a real commit (`Ok(true)`; a no-op save never snapshots). A canvas Ctrl+S also saves a **single selected File node's note buffer** (its text lives in the `.md`), so spatial edits aren't stranded by selection. Reverse direction: an external advance re-parses the JSON and re-renders (clean buffer reload). **Selection and camera survive by stable node `id`**, not text offset, so an external move never disturbs local selection. In JSON view the standard editor binding applies unchanged. [canvas-oplog-binding]
status:: done
touches:: [[code:hiker/panels/canvas/render]]
note:: SAME dirty/save model as the text editor: forward — a canvas edit serializes to canonical JSON and mirrors into the op-log `working` layer (`mirror_json_to_working` = full-span `apply_working_edit`), buffer goes DIRTY (no `loaded_hash` advance); Ctrl+S (`save_canvas_document`) `commit_working` folds → `accepted`+`.canvas` rewrite, clears dirty, writes a snapshot on save (not per-edit). Reverse — re-parse on materialized-text change, selection survives by node id. The dirty dot rides `Buffer::is_dirty()` like a text tab. "Add to canvas" (`add_file_node`) routes through `working` when open / `user_save` when closed · evidence: `app/src/panels/canvas/render.rs` (`persist_canvas`/`mirror_json_to_working`/`save_canvas_document`), `app/src/panels/canvas/mod.rs` (`sync_from_text`/`add_file_node`)
- **Conflict merge.** Concurrent edits ride the op-log's 3-way text merge: disjoint-node edits land in different JSON regions and merge automatically; two devices editing the same node touch the same JSON region and surface as a conflict hunk with Keep mine / Keep theirs / Keep both ([[spec:op-log-merge-conflict]]). No canvas-specific conflict mechanism. [canvas-conflict-merge]
status:: done
note:: inherited from the op-log text-merge path; no canvas-specific mechanism; not yet canvas-specifically exercised · evidence: rides `core::oplog` ([[spec:op-log-merge-conflict]])
- **Nav stack.** Opening a canvas records `NavTarget::File` on the global Back/Forward stack (`nav-stack`), interleaved with note and snapshot history like every other surface. [canvas-nav-stack]
status:: done
implements:: [[code:hiker/smoke_tests/open_file_routes_canvas_to_canvas_view_not_a_text_buffer]]
touches:: [[code:hiker/editor_pane]], [[code:hiker/panels/canvas]]
note:: canvas open records `NavTarget::File`; `open_file` (the back/forward nav path + the tree Open verb) routes a `.canvas` to the canvas view instead of a raw-JSON text buffer; regression tests passing · evidence: `app/src/panels/canvas/mod.rs` (`open`), `app/src/editor_pane.rs` (`open_file` routes `.canvas` → `canvas::open`), `app/src/state.rs` (nav test `canvas_opens_record_as_file_targets_and_interleave`), `app/src/smoke_tests.rs` (`open_file_routes_canvas_to_canvas_view_not_a_text_buffer`)
- **Create.** A sidebar `+` action (and the cross-type new-item picker, [[spec:sidebar-new-item-button]]) seeds an empty `.canvas` through a `core` create op and opens it in the canvas view with inline-rename active — the [[spec:board-create]] shape. [canvas-create]
status:: done
implements:: [[code:hiker/workbench_host/impl#[`HikerWbBehavior<'a>`][`Host<HikerWbTab, _>`]side_bar_action_buttons]]
touches:: [[code:hiker/sidebar]]
note:: seeds empty `.canvas` via `core::ops::file::create_at`, opens framed (mirrors [[spec:board-create]]) · evidence: `app/src/sidebar/mod.rs` (`new_canvas`), `app/src/workbench_host.rs` (`+` picker)
- **File tree.** `.canvas` files render with a canvas glyph and open the canvas view on click (a "View as JSON" verb is the escape hatch), mirroring how board-docs route ([[spec:board-view]]). [canvas-file-tree-glyph]
status:: done
implements:: [[code:hiker/files/sidebar/impl#[`FilesCtx<'_, '_>`]move_into_folder]], [[code:hiker/files/sidebar/export_trail_to_canvas]], [[code:hiker/files/sidebar/default_sort]], [[code:hiker/files/rename/rename_text_edit]]
note:: glyph + default open-as-canvas; "Open as canvas" / "View as JSON" verbs · evidence: `app/src/files/sidebar.rs` (`is_canvas_doc`)
- **File-ref rewrite.** When a referenced note moves, file-node `file` paths rewrite in the same transaction through the shared [[spec:wikilink-rename-rewrite]] pass alongside wikilink bodies, trail waypoints, and board cards. [canvas-file-ref-rewrite]
status:: done
implements:: [[code:hiker/canvas/on_note_moved]], [[code:hiker/links_rename/on_note_moved]]
touches:: [[code:hiker/canvas]]
note:: on note rename, every `.canvas` file's matching File-node `file` rewrites (subpath preserved) via `user_save` in the same rename pass, mirroring the boards branch; unparseable canvases skipped; unit + core tests · evidence: `hiker-canvas/core/src/model.rs` (`rewrite_file_refs`), `core/src/canvas.rs` (`on_note_moved`/`walk_canvas_files`), `core/src/links_rename.rs`
implements:: [[code:hiker/canvas/impl#[`RewriteCtx<'_>`]rewrite_file_ref]]

### Canvases activity [canvas-activity]
status:: done
note:: `activity_bar_plan.md` Phase 3. Left-bar activity (canvas icon) whose single view lists the vault's `.canvas` files via `panels::canvas::list_canvases` (sorted, vault-relative); clicking a row defers `panels::canvas::open` — the same opener the file tree uses — so it summons the existing `TabKind::Canvas` tab. List-only, no new tab kind. Order test updated: files, clusters, trails, vault, canvases, context, search, trash, chat · evidence: `app/src/canvas_activity/mod.rs` (`CanvasActivity` + `CanvasListView`, id `"canvases"`), registered in `app/src/activity/mod.rs::builtin_activities` (after `vault`) with a `"canvases"` `with_ctx` arm; zero-field `State` on `AppState::canvases_activity_state`

A left-bar **Canvases** activity whose single view lists every `.canvas` document in the vault (vault-relative, sorted), each row a clickable title with a hover-expandable thumbnail preview ([[spec:preview-canvas-thumbnail]]). Clicking a row defers `panels::canvas::open` — the same opener the file tree uses — so the canvas appears in the existing `TabKind::Canvas` tab rather than a new tab kind. The listing is read fresh from disk each frame, so the activity is effectively stateless: a zero-field `State` marker keeps the registry's per-activity state seam uniform.
touches:: [[code:hiker/canvas_activity/render_body]], [[code:hiker/canvas_activity/State]], [[code:hiker/canvas_activity/CanvasActivity]], [[code:hiker/canvas_activity/CanvasListView]], [[code:hiker/canvas_activity/impl#[CanvasActivity][`Activity<dyn AppCtx + 'static>`]id]], [[code:hiker/canvas_activity/impl#[CanvasActivity][`Activity<dyn AppCtx + 'static>`]label]], [[code:hiker/canvas_activity/impl#[CanvasActivity][`Activity<dyn AppCtx + 'static>`]icon]], [[code:hiker/canvas_activity/impl#[CanvasActivity][`Activity<dyn AppCtx + 'static>`]views]], [[code:hiker/canvas_activity/impl#[CanvasListView][`View<dyn AppCtx + 'static>`]id]], [[code:hiker/canvas_activity/impl#[CanvasListView][`View<dyn AppCtx + 'static>`]render]]

**Indexing.** A `.canvas` file is an op-log document (synced, versioned) but is **not** markdown-chunked — the indexer still ignores it for semantic/lexical search per `index.md`'s non-markdown tolerance. Making text-node contents searchable is deferred ([[spec:canvas-search-index]]).


## Deferred

- **Search over canvas contents.** Extract text-node bodies (and file-node titles) into the index so a canvas's contents are findable, via a derived projection like [[spec:board-cards-derived-table]]. [canvas-search-index]
status:: planned
note:: deferred: extract text-node contents for search via a derived table like [[spec:board-cards-derived-table]]
- **Drag-to-add from the file tree.** Dropping a file row (or a multi-selection) onto the canvas creates pointer file nodes at the drop point — the same gesture as a board's [[spec:board-dnd]] file drop, riding the uniform vault-path drag payload ([[spec:trails-dnd-ingestion]]) once it lands. The file-tree drag / multi-select plan (`files.md`, [[spec:note-multi-select]] / [[spec:drag-and-drop-move]]) records that canvas must accept this drop; the **Insert from vault** picker and the **Add to canvas** verb cover insertion until then. [canvas-dnd-add]
status:: planned
note:: deferred: drop a file row / multi-selection onto the canvas to create pointer nodes ([[spec:board-dnd]] gesture); file-tree side noted in `files.md` [[spec:note-multi-select]]; the Insert picker + Add-to-canvas verb cover insertion until then
- **Navigate to a node.** A `NavTarget` that focuses a specific node id (Back/Forward into a canvas location, deep-linking a node), extending `nav-stack` beyond file granularity. [canvas-node-nav-target]
status:: planned
note:: deferred: `NavTarget` focusing a specific node id (deep-link / Back-Forward into a node)
- **Minimap / overview.** A corner minimap of the whole canvas for orientation on large boards, reusing the texture-backed minimap renderer. [canvas-minimap]
status:: planned
implements:: [[code:hiker/panels/canvas/render/canvas_body]]
note:: deferred: corner minimap for large canvases, reusing the texture-backed minimap renderer
- **Routed edges after auto-arrange.** Optional orthogonal edge routing, reusing the layered engine's poly-line routes ([[spec:graph-routed-edges]]) so an auto-arranged canvas can draw its connectors along the computed ranks rather than as direct curves. [canvas-routed-edges]
status:: planned
note:: deferred: optional orthogonal edge routing after auto-arrange, reusing the layered engine's poly-line routes ([[spec:graph-routed-edges]]) instead of direct curves
- **Hyperbolic / fisheye projection modes.** The board adopts the shared `Projection` seam (`projection.md`) for an optional fisheye lens and a navigate-only Poincaré disk mode, selectable from the View menu ([[spec:canvas-view-toggle]]) and off by default. A lens over the Euclidean `x/y` only — the `.canvas` format and the canonical-JSON op-log binding are untouched. The Poincaré minimap ([[spec:proj-minimap]]) is the hyperbolic sibling of the affine [[spec:canvas-minimap]] above. [proj-canvas-mode]
- **MCP canvas tools.** A read + curate surface (`canvas_get` / `canvas_list` plus node/edge write verbs gated by [[spec:agent-write-review-mode]]) so attached agents can read a canvas as context and reorganize it, the symmetric surface to [[spec:board-mcp-tools]]. [canvas-mcp-tools]
status:: planned
note:: deferred: MCP read + curate surface (`canvas_get`/`canvas_list` + write verbs), symmetric to [[spec:board-mcp-tools]]


## Out of scope

- **A second canvas/diagram format.** This is the JSON Canvas open format only. The draw.io family (`ideas.md` `[drawio-source-ingest]`) and Mermaid (`editor-widgets.md`) are separate source-type / widget concerns, not this editor.
- **Embedding a canvas inside a markdown note.** A canvas is its own document, not an inline widget in the live-preview layer.
- **Real-time multi-cursor presence.** There is no live shared-cursor session; concurrent editing across machines is whatever the user's file-sync or git does, not a real-time channel this doc owns.
- **Live web rendering / external fetch.** A canvas never fetches and renders an external web page; link nodes are open-externally cards. Vault-internal `.html` is the only HTML a canvas renders, through `hiker-htmlview`.
- **Frontmatter / note metadata on a canvas.** The `.canvas` file is pure JSON Canvas; hiker-namespaced metadata (tags, lifecycle) is not modeled on it in v1.

## Registry imports (from status.md)

Entries imported from the retired status registry that had no anchor in this doc —
re-home them into the relevant sections as the doc evolves.

- **canvas-node-move** — drag-move a selection [canvas-node-move]
  status:: done
  touches:: [[code:hiker/interaction]]
- **canvas-group-move** — the move-set is exactly the selection (group members folded in at select-time, via [[spec:canvas-selection]]), frozen at drag-start and never recomputed mid-drag, so dragging a group past another never grabs the other's cards (tests: `group_member_ids_returns_contained_nodes_only`, `moving_a_frozen_set_does_not_touch_unselected_nodes`) [canvas-group-move]
  status:: done
  touches:: [[code:hiker/interaction]]
  note:: evidence: `hiker-canvas/view-core/src/interaction.rs` (`group_member_ids` at select-time, `move_selection` over the frozen set)
- **canvas-node-resize** — eight handles rewrite width/height (+x/y for top/left; tests) [canvas-node-resize]
  status:: done
  touches:: [[code:hiker/handles]]
- **canvas-insert-from-vault** — "Insert from vault" toolbar button → autocomplete picker ([[spec:autocomplete-picker-widget]] over `VaultSource` NotesAndSources) → drops a `File` pointer at center via `insert_node_centered`; stores the vault path, never the content [canvas-insert-from-vault]
  status:: done
  touches:: [[code:hiker/panels/canvas]], [[code:hiker/panels/canvas/render]]
  note:: evidence: `app/src/panels/canvas/render.rs` (`insert_from_vault`/`file_node`), `app/src/panels/canvas/mod.rs` (`insert_picker: PickerState`)
- **canvas-add-to-canvas-verb** — right-click a vault row → pick a target canvas → appends a `File` pointer via the op-log `user_save` path (works whether the canvas is open or not); mirrors [[spec:board-add-card]] (single-row, matching the board verb; bulk awaits [[spec:note-multi-select]]) [canvas-add-to-canvas-verb]
  status:: done
  implements:: [[code:hiker/files/sidebar/FileVerb#AddToCanvas#canvas_rel]], [[code:hiker/files/sidebar/canvas_glyph_marker]], [[code:hiker/panels/canvas/show]], [[code:hiker/panels/canvas/list_canvases]]
  note:: evidence: `app/src/files/sidebar.rs` (`FileVerb::AddToCanvas` + "Add to canvas…" submenu), `app/src/panels/canvas/mod.rs` (`add_file_node`/`list_canvases`)
- **canvas-activity-new-button** — Plain `+` button (a `Plus` `ImageButton`, NOT a [[spec:split-add-button]] dropdown) in the Canvases activity side-bar header: clicking it calls `AppState::new_canvas` ([[spec:canvas-create]]) — seeds an empty `.canvas` and opens it framed [canvas-activity-new-button]
  status:: done
  touches:: [[code:hiker/workbench_host]]
  note:: evidence: `app/src/workbench_host.rs` (`side_bar_action_buttons`, `"canvases"` branch)
- **canvas-edge-draw** — hovered/selected nodes show four visible connector circles (offset just outside the card, clear of the resize handles). Clicking one starts a click-to-connect gesture (a rubber band follows the cursor); the next click on a node attaches the edge. Press-dragging a handle also connects (drag-to-connect), and drop-on-empty cancels (tests). Paint/hit positions are kept in sync by a shared `CONNECTOR_OFFSET` (regression test) [canvas-edge-draw]
  status:: done
  touches:: [[code:hiker/interaction]], [[code:hiker/widget/pointer]]
  note:: evidence: `hiker-canvas/view-core/src/interaction.rs`, `src/paint.rs` (`connector_handles`), `src/widget/pointer.rs`
- **canvas-edge-redirect** — drag an existing edge endpoint to re-anchor it [canvas-edge-redirect]
  status:: done
  touches:: [[code:hiker/interaction]]
- **canvas-edge-label** — double-click an edge → inline label `TextEdit` at the edge midpoint (foreground area above the interaction surface); Enter / click-outside commits a `SetEdgeLabel`, Esc cancels; the label paints at the spline midpoint ([[spec:canvas-edge-routing]]) [canvas-edge-label]
  status:: done
  touches:: [[code:hiker/widget/pointer]]
  note:: evidence: `hiker-canvas/core/src/ops.rs` (`EditOp::SetEdgeLabel`), `hiker-canvas/view/src/widget.rs` (`draw_label_editor`), `src/widget/pointer.rs`
- **canvas-card-scroll** — Wheel while the pointer is over a card scrolls that card's content (the editor's native `scroll_y`, clamped to content height and echoed back as the effective offset); wheel over empty canvas zooms the camera. A card's scroll is stable across content changes — typing in a tab on the same note doesn't reset the card's scroll. Editor-backed cards (markdown / text / code) scroll; html/image embeds keep their own scroll (follow-up) [canvas-card-scroll]
  status:: done
  touches:: [[code:hiker/panels/canvas/content]]
  note:: evidence: `hiker-canvas/view/src/widget.rs` (`handle_zoom`), `app/src/panels/canvas/content.rs` (editor `scroll_y`)
- **canvas-card-zoom** — Per-card content zoom (font multiplier), **decoupled from camera zoom** so text stays readable at any board zoom — the card is a fixed window, not a thing that scales with the camera. Adjusted via the card's right-click **Zoom in / out / Reset** menu or Ctrl/Cmd+wheel over the card; default 1.0, clamped 0.3–4.0. Carried to the content engine as `content::CardView { zoom, scroll_y }` [canvas-card-zoom]
  status:: done
  touches:: [[code:hiker/content]], [[code:hiker/panels/canvas/content]]
  note:: evidence: `hiker-canvas/view/src/content.rs` (`CardView`), `hiker-canvas/view/src/widget.rs` (menu + wheel), `app/src/panels/canvas/content.rs` (`paint_editor` font)
- **canvas-group-grab** — a press on the group's top header band targets the group (checked before the top-most hit-test in `resolve_target`), so a body press still hits framed children; carries members via [[spec:canvas-group-move]] [canvas-group-grab]
  status:: done
  touches:: [[code:hiker/interaction]]
  note:: evidence: `canvas-view-core/src/interaction.rs` (`group_header_hit`, `GROUP_HEADER_H`)
- **canvas-group-resize** — removed the group exclusions; a singly-selected group shows + resizes via the eight handles, reframing only the container (members keep position; tests) [canvas-group-resize]
  status:: done
  touches:: [[code:hiker/interaction]]
  note:: evidence: `canvas-view-core/src/interaction.rs` (`single_selected_handle`), `canvas-view/src/widget.rs` (`paint_overlays`)
