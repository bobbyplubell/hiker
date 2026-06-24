# Rich-row previews

A small inline preview thumbnail rendered next to a list row — a `.canvas`
document's node/edge shape, a cluster tree's force-directed graph — that expands
on hover into a larger floating preview which never occludes the rows below it.
The mechanism is a reusable widget: a domain supplies a *provider*, and the
widget owns rendering, the on-disk cache, the texture upload, and the
hover-expand lifecycle. It drops into any list (`vault-view.md`'s cluster-tree
rows and the canvases activity are the first two consumers).

Key decisions, each detailed in its own section below:

- **One reusable widget, domain-agnostic.** A `ThumbnailProvider` trait is the
  whole seam; new domains add a provider, not a new widget. [preview-thumbnail-provider]
status:: done
note:: Domain-agnostic: provider computes a stable key + rasterizes pixels; the widget owns the cache↔texture round-trip + hover registration. Render version folded into every key so a renderer change invalidates all kinds (tested) · evidence: `app/src/widgets/preview/` (`mod.rs`: `ThumbnailProvider`, `PreviewKey`, `PreviewKind`, `PREVIEW_RENDER_VERSION`, `content_hash`; `thumbnail.rs`: the `thumbnail(ui, provider, vault_root, opts)` widget)
- **Tiny thumbnails are rendered off the live widget tree** as a flat SVG/RGBA
  geometry sketch (canvas drawn LOD-faithful so the tiny thumbnail matches the
  expanded preview). [preview-canvas-thumbnail, preview-tree-thumbnail]
- **Canvas opts into live-painting its expanded preview; trees stay cached** — the
  one live-vs-cached rule, stated under "The provider abstraction" below. [preview-canvas-thumbnail]
status:: done
touches:: [[code:hiker/panels/canvas/thumbnail]], [[code:hiker/widgets/preview]], [[code:hiker/widgets/preview/thumbnail]]
note:: TWO paths, one per size. TINY inline thumbnail: a cached SVG now drawn LOD-FAITHFUL to match the live renderer at fit zoom ([[spec:canvas-lod-placeholder]]) — translucent group rects + header label band, curved edge connectors with arrowheads, rounded cards with a title line + 2–3 decreasing skeleton bars in the node color; rasterized via `render::rasterize_svg`, content hash over the bytes (tested: card + curved-edge + group-band SVG). EXPANDED hover preview: LIVE-PAINTS the real renderer via `canvas_view::CanvasView::show_static` ([[spec:canvas-static-paint]]) zoom-to-fit inside the non-interactable `Area`, no capture and NO `.hiker` cache entry (live every frame); parsed `Canvas` memoized per content-hash on a UI-thread-local (bounded) so a hover doesn't re-parse each frame; parse failure → `None` → falls back to nothing. `PREVIEW_RENDER_VERSION` bumped (1→2) to retire old sketches · evidence: `app/src/panels/canvas/thumbnail.rs:CanvasPreview` (`render` + `expanded_paint`), `app/src/widgets/preview/mod.rs` (`ExpandedPaint` hook + `paint_live_expanded_area`), `app/src/widgets/preview/thumbnail.rs` (stashes the thunk)
- **The hover-expand never steals row hover** (non-interactable side-anchored
  `Area`). [preview-hover-expand-side-anchor]
status:: done
touches:: [[code:hiker/widgets/preview]]
note:: Stashes `(key, anchor_rect, hover_start, live_paint?)` in egui memory on hover; draws the large preview in a non-interactable `Order::Tooltip` `Area` so it never senses the pointer → never steals row hover. ~120ms debounce, side-anchored (right→flip-left→clamp via the shared `expanded_area_min` helper), drops stale requests so it vanishes on pointer-off. Two render branches behind one placement: a provider that opted into `expanded_paint` (canvas) live-paints its thunk into the rect (clipped); otherwise the cached large PNG is blitted (trees) — `show_static` registers no interactive widget, so the live branch is held to the same no-pointer-sense rule · evidence: `app/src/widgets/preview/mod.rs:render_expanded_preview` (called once post-sidebar from `main.rs::update`)
- **Renders are cached under `.hiker/previews/`**, keyed by content hash + kind +
  size with a render-version constant. [preview-disk-cache]
status:: done
touches:: [[code:hiker/widgets/preview/cache]]
note:: PNG at `<vault>/.hiker/previews/<kind>-<hash:016x>-<size>.png`; small/large are distinct `size` buckets; hit→texture, miss→render+write. Best-effort byte-budget LRU sweep (64 MB, oldest-by-mtime, once/session) mirroring the diagram disk-cache. All I/O degrades to live render (tested: round-trip, buckets, kind-collision, sweep) · evidence: `app/src/widgets/preview/cache.rs:PreviewCache`


## The provider abstraction

A `ThumbnailProvider` (in `app/src/widgets/preview/`) is two methods:

- `cache_key() -> PreviewKey` — the entry's identity at the small size. The
  widget derives the large (expanded) key by swapping the `size` bucket.
- `render(px) -> Option<RgbaImage>` — rasterize at `px` (longest edge, physical
  pixels). `None` on any failure; the widget draws a neutral placeholder, never
  panics.
- `expanded_paint() -> Option<ExpandedPaint>` (optional, default `None`) — opt
  into live-painting the expanded preview (see the live-vs-cached rule below).
  `Some(thunk)` hands back a `Send + Sync` thunk (egui's temp store requires it)
  that paints into the given rect and returns `true`; `None` keeps the cached
  path. Only the tiny thumbnail + the cached path use `render`, so a live-paint
  provider needn't warm the large cache entry.

**The live-vs-cached rule (canonical).** Every tiny inline thumbnail is a
cached SVG/RGBA sketch (`render` → PNG under `.hiker/previews/`). The expanded
hover preview is *cached* (blit the large PNG) unless the provider returns a
thunk from `expanded_paint`, in which case it *live-paints* the real widget into
the rect every frame — no cache entry produced. Canvas opts into live-paint for
its expanded preview (driving the real `canvas_view::CanvasView::show_static`,
the paint-only render [[spec:canvas-static-paint]], zoom-to-fit); trees don't opt in,
so both their sizes stay on the cached-image path. Subsequent sections reference
this rule rather than restate it.

A `PreviewKey` is `{ content_hash, kind, size }`. The `content_hash` is folded
from the document's raw hash, the `PreviewKind` discriminant, and a
`PREVIEW_RENDER_VERSION` constant — so a renderer tweak (different node shape,
colors, layout) makes every prior cache entry a miss without a manual sweep,
and two kinds with the same raw hash never collide. [preview-thumbnail-provider]

The `thumbnail(ui, provider, vault_root, opts)` widget allocates a small fixed
rect (~16 logical px), blits the cached small texture (rendering + caching on a
miss), and on hover registers an expand request. It is the only place that
touches the cache and the texture upload; a provider is a pure pixel producer.


## Canvas thumbnail

[preview-canvas-thumbnail]

The canvas provider has two paths per the live-vs-cached rule above.

**Tiny inline thumbnail — cached LOD-faithful SVG.** The provider reads a
`.canvas` document's bytes, parses `hiker_canvas::model::Canvas`, computes its
content bounds (the same zoom-to-fit math the live view uses), and emits a flat
SVG scaled by the fit transform, drawn to RESEMBLE the real renderer's fit/thumbnail
LOD path ([[spec:canvas-lod-placeholder]]): translucent group rectangles with a header
label band first (lowest z), then curved edge connectors with small arrowheads,
then rounded node cards each with a title line + 2–3 decreasing skeleton bars in
the node's preset / hex / kind-neutral color. It's an **approximation** (no real
node bodies, no embedded files) but matches the live expanded preview because both
show LOD placeholders. The content hash is over the `.canvas` bytes, so any edit
invalidates the cache; a `PREVIEW_RENDER_VERSION` bump retires older sketches.

**Expanded hover preview — live-paint via the real renderer** (`expanded_paint`
thunk → `show_static`). It parses the canvas (memoized per content hash on the UI
thread so a hover doesn't re-parse every frame), builds a fresh view, zoom-to-fits
the preview rect, and paints frames + groups + edges + LOD placeholders with the
crate's no-op `NoContentRenderer` — clipped, inside the non-interactable `Area`.
At fit zoom every node is a LOD placeholder, so no content engine (and no `!Send`
per-node engine) is needed. A `.canvas` that fails to parse makes `expanded_paint`
return `None` (or the thunk return `false`), so the expanded draw falls back /
shows nothing rather than panicking.


## Cluster-tree thumbnail

[preview-tree-thumbnail]
status:: done
touches:: [[code:hiker/panels/cluster_thumbnail]]
note:: Loads a tree's nodes, runs a DETERMINISTIC seeded fixed-iteration `hiker_graph::force_layout` (convergence early-out off) — not the live async `LayoutWorker` — so the cache key is reproducible; fits + emits dots(nodes)+lines(edges) SVG. Node count capped. Shape hash ignores summary/policy, tracks re-parent (tested: determinism, cap, hash) · evidence: `app/src/panels/cluster_thumbnail.rs:TreeThumbnail`

The tree provider takes a cluster tree's resolved nodes (loaded read-only via
`trees.list_nodes`, `cluster-editor.md`) and renders a **force-directed**
dots-and-lines sketch (same graph view the cluster graph panel offers,
`clustering.md`). Where that panel runs the live async converging `LayoutWorker`,
the thumbnail runs a **deterministic, seeded, fixed-iteration synchronous pass**
(`hiker_graph::force_layout` over a PRNG-seeded scatter, convergence early-out
disabled so the iteration count is fixed) — so the layout is stable
frame-to-frame and across devices, making its cache key reproducible. Settled
positions are fit into the output square and emitted as an SVG: a line per
parent→child edge, a dot per node (clusters larger + accented, leaves small).

Node count is capped (the first N in tree order) with the rest dropped, so a
huge tree still renders cheaply and the layout stays bounded. The content hash
is over the node *shape* (id + parent, in order), so it churns on add / remove /
re-parent but not on summary / policy edits.

The tree provider does **not** opt into `expanded_paint`, so both its inline and
expanded previews stay on the cached-image path (per the live-vs-cached rule).


## Non-occluding hover-expand

[preview-hover-expand-side-anchor]

When a thumbnail is hovered it stashes a hover request (its cache key + its
screen rect + the hover-start time) in egui memory during the sidebar render.
After the sidebar renders, the frame loop calls `render_expanded_preview` once,
which:

- **Debounces.** The large preview only draws after a short hover-hold (~120ms);
  until then the inline small thumbnail is what's shown. A quick pass down the
  list never flashes a stack of large cards. `request_repaint` keeps the timer
  advancing without further input.
- **Draws in a non-interactable `Area`.** `Order::Tooltip` +
  `interactable(false)`, so the card paints above everything but **never senses
  the pointer**; it never overlaps the rows or steals hover, so moving the
  pointer down the list independently re-triggers each row's own thumbnail. (Same
  egui mechanism as the editor's edit-popup preview, [[spec:widget-edit-popup-preview]].)
  The live-paint path is held to the same rule — `show_static` registers no
  interactive widget, so the live canvas inside the `Area` can't sense the pointer
  either.
- **Cached blit or live paint** per the live-vs-cached rule — both use the same
  side-anchor placement and non-interactable framing.
- **Side-anchors + clamps.** The card flows to the **right** of the thumbnail
  (over the editor area), vertically centered on it; flips **left** when the
  right edge would clip (a docked / wide sidebar), pulls **up** when the bottom
  would clip, then clamps fully on-screen.
- **Drops stale requests.** A request not re-stashed this frame (the pointer
  left every thumbnail) is detected by its write timestamp and dropped, so the
  expanded preview vanishes the instant the pointer moves off the row.


## The cache

[preview-disk-cache]

Rendered previews persist under `<vault>/.hiker/previews/` as one self-describing
PNG each:

  `<kind>-<content_hash:016x>-<size>.png`   — straight RGBA8, dimensions
                                              self-described by the PNG.

Small (inline) and large (expanded) renders are **separate `size` buckets**, so
a hover-expand never evicts the inline thumbnail it grew from. On a cache hit
the PNG decodes straight to an egui texture; on a miss the provider renders, the
result is written, and the pixels are uploaded. The cache serves every tiny
thumbnail and the tree expanded preview (the canvas expanded preview live-paints,
per the live-vs-cached rule, so produces no PNG). A best-effort byte-budget LRU
sweep (64 MB, oldest-by-mtime) runs at most once per session, mirroring the
diagram disk-cache's `sweep_to_budget` ([[spec:widget-render-disk-cache]]). All I/O is
best-effort — any read / decode / write error degrades to a live render, never a
panic. `.hiker/previews/` is regenerable / losable per `design.md`
§[[spec:subsystem-notes-visible]].


## Wiring

[vault-view-row-previews]

The Vault view (`vault-view.md`) renders the tree thumbnail before the label of
any row whose note carries `hiker.kind: cluster-tree`. The canvases activity
renders the canvas thumbnail before each `.canvas` row (`.canvas` files aren't
indexed as notes, so they don't appear in the Vault lens — the canvases activity
is their home). Either list calls the same `thumbnail(...)` widget with the
appropriate provider.

The **Context panel** (`context/backlinks`, `context/appears-in`, `context/related`)
also previews its rows: note rows register a markdown hover-preview (below), and
the `appears-in` view's `.canvas` rows show a **hover-only** canvas preview — a
plain canvas icon in the row, the spatial `CanvasPreview` only on hover (via
`register_hover_only`, no inline thumbnail).

Every preview-showing view (the three context sub-views, the Vault lens, the
canvases activity) carries an **eye button** in its section-header actions whose
menu holds one **Show hover previews** toggle (`[ui].hover_previews_enabled`,
`Scope::Vault`) — one global flag surfaced per-view for discoverability. Off
suppresses the hover-expand popups (canvas / tree / note) — registration still
runs, only the draw is gated at the frame loop (both `render_expanded_preview`
and `render_note_preview`); always-on inline thumbnails (vault cluster-tree rows,
canvases activity) stay. [preview-toggle]
status:: done
touches:: [[code:hiker/workbench_host]]
note:: evidence: `core/src/config/mod.rs` (`Ui::hover_previews_enabled`, default true) + `patch.rs` (eligible bool, both scopes); gate in `app/src/main.rs::update` (skips `render_expanded_preview` + `render_note_preview` when off); `app/src/workbench_host.rs::hover_preview_eye` + the `side_bar_action_buttons` eye, shown for `context/backlinks` / `context/appears-in` / `context/related` / `vault` / `canvases`

A cluster-tree note (`hiker.kind: cluster-tree`) opens in its force-graph
`ClusterGraph` tab — `find_or_open_tab` keyed on the frontmatter `hiker.id` —
not as raw markdown; it has no in-buffer view (unlike a board's Markdown toggle,
so a buffer would just show YAML). Central in `open_file`, so it fires from every
caller: vault sidebar, backlinks, related, appears-in, wikilinks, Back/Forward.
Boards keep their existing file-tree `is_board_doc` routing. [cluster-tree-open-routing]
status:: done
implements:: [[code:hiker/trees/store/impl#[Db]tree_id_at_path]]
touches:: [[code:hiker/editor_pane]]
note:: evidence: `app/src/editor_pane.rs::open_file` (branch after `.canvas`); `core/src/trees/store.rs::Db::tree_id_at_path` (reads `hiker.kind`/`hiker.id` frontmatter)


## Note previews (markdown + diagrams)

[preview-note-hover]
status:: done
touches:: [[code:hiker/appears_in]], [[code:hiker/backlinks]], [[code:hiker/related]], [[code:hiker/widgets/preview]], [[code:hiker/widgets/preview/note]]
note:: Hovering a context-panel note row shows a read-only **markdown-with-diagrams** preview of the note in the same non-interactable `Order::Tooltip` `Area` (hold + stale-drop + side-anchor reused from [[spec:preview-hover-expand-side-anchor]]). Renders via `buffer_view::show_embedded_buffer` (`EmbedOpts { read_only:true, markdown:true, focus:false }`) — needs `&mut AppState` (diagram cache / shared buffer / title resolver, all `!Send`), so it runs at the frame loop, NOT through the `Send+Sync` `ExpandedPaint` thunk; a parallel egui-memory slot (`preview-note-hover-request`), not the thumbnail `HoverRequest`. Per-preview `EmbeddedView` in a UI-thread-local (recreated on path change) so `&mut embed` never aliases `&mut app` — the [[spec:canvas-inline-edit]] pattern. No disk cache (live render). `appears-in` `.canvas` rows show a hover-only `CanvasPreview` spatial preview (plain canvas icon in the row, preview on hover via `register_hover_only` — no inline thumbnail) · evidence: `app/src/widgets/preview/note.rs` (`register_note_hover` + `render_note_preview`, called from `main.rs::update` beside `render_expanded_preview`); `app/src/widgets/preview/mod.rs` (`expanded_area_min` + `EXPAND_HOLD_SECS` made `pub(crate)`); wired in `app/src/backlinks/mod.rs`, `app/src/related/mod.rs` (the related view now renders plain icon+title rows with the hover-preview, replacing the rich `result_card`s — `result_card` stays for the search panel), `app/src/appears_in/mod.rs::draw_ref_row`

A note's body — rendered markdown WITH diagrams (mermaid / math / wavedrom) — is
previewed by a SEPARATE app-level mechanism, not a `ThumbnailProvider`, because it
reuses the live editor render path (`buffer_view::show_embedded_buffer` with
`EmbedOpts { read_only: true, markdown: true, focus: false }`) which needs
`&mut AppState` (diagram cache, shared buffer, wikilink title resolver — all
`!Send`) and so can't ride the provider's `Send + Sync` `ExpandedPaint` thunk:

- A hovered row calls `preview::register_note_hover(ui, row_rect, path)`, which
  stashes a `NotePreviewRequest { path, anchor, hover_started, written_at }` in
  egui memory under its own id (parallel to the thumbnail `HoverRequest`).
- The frame loop calls `preview::render_note_preview(ctx, &mut app)` once after
  the sidebar, applying the same hold + stale-drop + side-anchor placement
  (`expanded_area_min`, `EXPAND_HOLD_SECS`), then rendering the note read-only into
  the non-interactable `Order::Tooltip` `Area` via `show_embedded_buffer`. The
  per-preview `EmbeddedView` lives in a UI-thread-local (recreated when the path
  changes) so `&mut embed` never aliases `&mut app` — the [[spec:canvas-inline-edit]]
  `show_file_edit` pattern.

`read_only: true` + `focus: false` + the non-interactable `Area` keep the preview
from capturing edits or stealing keyboard focus / row hover. There is no on-disk
cache (it live-renders, like the canvas expanded preview).


## Out of scope

- **Faithful tiny thumbnails / full-content previews.** The cached inline
  thumbnail is a geometry sketch, not a pixel-exact capture (no offscreen GPU
  capture of the real renderer for the tiny image — it stays SVG). Document
  bodies, embedded images, and node text are not drawn at any size — even the
  live canvas expanded preview shows LOD placeholders, not full node content. A
  faithful tree expanded preview (live force-graph) is not built; trees stay
  cached SVG.
- **Image / PDF previews.** A note's markdown body (with diagrams) now previews
  via the app-level note-preview path above ([[spec:preview-note-hover]]), but an image
  or PDF preview is not built — each would be its own provider or app-level path,
  not a change to the widget.
