# Editor widgets

Rendered block and inline widgets in the live-preview layer: LaTeX math, Mermaid diagrams, and tables drawn in place of their markdown source. This is the Tier 3 surface `live-preview.md` defers — the decoration providers that turn `$…$`, `$$…$$`, ` ```mermaid ` fences, and pipe tables into rasterized widgets rather than tinted source. Builds on the editor crate's widget primitives (`editor/SPEC.md` §3.4–3.5, §9) and the egui-agnostic renderers in `hiker-render`.


## Pipeline

Source span → renderer → SVG → RGBA pixels → egui texture → blit. The work splits across three crates so each stays in its lane:

1. **`editor-md` (detect).** Each kind's detector walks the markdown parse (same tree the mark-only `equations.rs` / `diagrams.rs` / `embeds.rs` use) and yields source spans + kind. The `Decoration::InlineWidget` / `BlockWidget` carrying a trait object is constructed in `app` (rendering is app's job), so `editor-md` stays renderer-unaware.
2. **`app` (render + rasterize).** Calls `hiker_math` / `hiker_mermaid` for the SVG + metrics (+ interaction regions), then rasterizes the SVG to straight **RGBA pixels** via `resvg` (the same path `hiker-render/htmlview` uses; tiny-skia's premultiplied output is un-premultiplied) at size × dpr. The rasterizer loads a shared `fontdb` (system fonts + the bundled `hiker_mermaid` sans-serif) — without it `resvg` renders no glyphs for Mermaid's `<text>` labels, so the diagram draws but every label is blank. Returns the rgba + dimensions + baseline + `content_hash` (+ normalized `DiagramRegion`s for interactive diagrams).
3. **`editor-egui` (upload + cache + blit).** The `InlineWidget` / `BlockWidget` traits expose `pixels()` + `baseline()`; the painter uploads to an egui texture cached by `(widget_id, width, height)` and blits into the reserved rect (mirroring the minimap widget). `widget_id()` is the cache key — app sets it to the render's `content_hash`.

**The egui-free boundary is load-bearing.** `editor-core` has no egui dependency and must keep none, so the `InlineWidget` / `BlockWidget` traits cannot return an egui texture handle. They expose **raw RGBA bytes + dimensions + a content hash** (egui-free); `editor-egui` owns the GPU-texture upload and cache. `app` owns the SVG render (so `hiker-render` deps stay out of the editor crates); `editor-md` only reports spans and keeps the mark-only fallbacks. This split is why no single crate gains a dependency it shouldn't. App-side, the widget types, render helper, providers, and live-edit popup live in `buffer/widgets/`; the decoration-assembly pipeline that invokes the providers lives in the sibling `buffer/decorations.rs`. [widget-render-pipeline, widget-render-module-split]


## Decoration shapes

| Kind | Source | Decoration | Placement |
|---|---|---|---|
| Inline math | `$…$` | `InlineWidget` (atomic) | Rendered in place when its line is inactive; collapses to source with a popup preview while edited (see Cursor reveal). Baseline-aligned via the render's baseline metric |
| Display math | `$$…$$` | `BlockWidget` | Full-width own-height row |
| Mermaid | ` ```mermaid ` fence | `BlockWidget` | Full-width own-height row |
| WaveDrom | ` ```wavedrom ` fence | `BlockWidget` | Full-width own-height row |
| Table | pipe table block | `BlockWidget` | Full-width own-height row, native paint |

Inline math is the only inline widget — `InlineWidget::measure(font_size) → (w, h)` reserves a region among the glyphs and the cursor steps over it atomically (`editor/SPEC.md` §3.4, `editor-core` `InlineWidget`). The render's baseline metric (distance from box top to baseline) is what lets `$x^2$` sit on the surrounding text's baseline rather than floating. Inline math taller than the text line (a fraction, a large operator, a `\sum` with limits) grows that visual line's height to its measured height — the editor's line layout takes `max(text height, tallest inline widget height)` per line and baseline-aligns the widget to the text baseline. Everything else is a `BlockWidget` whose `measure(font_size, width) → height` supplies its own row height (§3.5) and participates in scroll + vertical layout. [widget-inline-math-baseline, widget-block-view-zone]

A block widget renders in place of its source: the provider hides the source lines (`LineStyle.hide`, the same mechanism folds use) and attaches a `BlockWidget` for the rendered output. On cursor-in the hide lifts and the widget is suppressed (see reveal below), so the raw markdown returns for editing. [widget-block-source-hide]

**Widget paint.** The traits expose `pixels()` plus `baseline()` (inline) and `click_regions()` (interactive sub-regions); the `editor-egui` painter uploads to an egui texture cached by `(widget_id, width, height)` and blits into the reserved rect, reusing the minimap's offscreen-texture pattern. The `InlineWidget::display()` text path (`editor-inline-widget-display`) still serves textual widgets. [widget-painter-texture-blit]

**Fit to the text column, clip to the editor body.** A block widget letterboxes (aspect-preserving) into the **content box** — the prose column (`text_origin_x … right`), gutter excluded — not the full row rect, and the blit draws through the editor's body-clipped painter (`TextureCache::blit` takes the caller's clipped `&Painter`, not the unclipped `ui.painter()`). So a diagram wider than the column scales down to fit instead of bleeding into the line-number gutter, and a tall one scrolled to the top is clipped to the editor body instead of painting over the toolbar. To match the paint, `BlockWidget::measure(font_size, width)` is handed the *content* width (full width minus `content_origin_x`) and the diagram widgets (display math, Mermaid, WaveDrom) return a width-scaled own-height (`fit_block_height`: shrink a too-wide diagram and its height together; never upscale a narrow one) so the reserved row matches the letterboxed render with no empty vertical band. Interactive `click_regions` map through the same content-box letterbox, so a diagram node stays clickable where it's drawn. [widget-painter-texture-blit, widget-block-view-zone]

A `BlockWidget` may instead supply an egui-free **retained paint list** (`paint_list`) — plain geometry+style data (filled rects, lines, positioned text runs) — which the painter replays natively with no texture. Used by tables (see Tables). The painter prefers the paint list when present, else blits the texture, else draws the placeholder; a widget supplies one or the other. The hook is generic — any structured native-painted block reuses it. [widget-block-native-paint]


## Cursor reveal

All widgets collapse to source for editing, and a floating popup previews the rendered result while you edit — one model for every kind, built on live preview's per-block trigger (`live-preview.md`):

- **Inline math.** Cursor off the line (no selection overlap): the `$…$` source is replaced in place by the rendered formula (inline atomic widget). Cursor on the line or selection overlap: the source shows as editable styled text in place (the current `equations.rs` mark style). [widget-reveal-inline]
- **Block widgets (display math, Mermaid, WaveDrom, tables).** Cursor anywhere inside the block's source span (delimiters inclusive) or a selection overlap: the source lines reappear (`LineStyle.hide` lifts) and the `BlockWidget` is suppressed; outside, the source hides and the widget renders. Per-block, same trigger as fenced code (`live-preview-code-fence-block-reveal`). [widget-reveal-block]
- **Highlighting a block reveals its delimiters.** A selection overlapping a fenced block expands the *whole* source — fence delimiter lines (` ```mermaid ` / ` ```wavedrom ` and the closing ` ``` `) included — not just the body. This is the `live-preview-selection-reveal-all` / `live-preview-code-fence-block-reveal` rule applied to the styling layer: the fence-marker reveal in `editor-md`'s `style_fenced_code_block` keys on cursor-line **OR** selection-overlap, so selecting the block (caret head landing past the closing fence) no longer collapses the ` ``` ` markers. Without this the widget layer revealed the diagram body while the styling layer still hid the fence lines — a visibly broken half-reveal.
- **Escape dismisses the popup, editing continues.** Pressing Escape while a live edit-preview popup is up hides it without leaving edit mode — the caret and selection are untouched and the source stays revealed for editing. The popup stays hidden until the caret leaves that widget and re-enters it (or moves to a different widget); it is *not* a document-wide suppression (the `Live edit preview` toggle owns that). Mechanism: the popup cache records the dismissed span's anchor (`inner_range.start`); the overlay paints nothing while the active span's anchor matches, and re-arms when the active span clears or changes. The buffer panel consumes Escape **only** when a popup is actually showing — otherwise Escape reaches the editor unchanged (clears selection). One model for inline math, display math, Mermaid, and WaveDrom. [widget-edit-popup-dismiss]
- **Live popup preview while editing.** While the main cursor is inside a widget's source span, a floating preview of the rendered widget shows in a non-focus-stealing, non-interactive egui `Area` (foreground/tooltip layer) anchored just below the span's last line — scroll-correct (the anchor is recomputed each frame from `view.line_top_y(line) + editor_rect`, the same technique the wikilink card uses), nudged to stay on-screen, and *not* covering the line being typed. It does not shift the text (no below-line block). One popup at a time — the span containing the main caret (for per-line inline reveal, the nearest span on the caret's line). It reuses the render's `content_hash` so a static span isn't re-rasterized each frame, and dismisses the moment no span is revealed. Reveal detection scans a window *around the caret*, not the tight viewport — otherwise revealing a span shifts layout/scroll a few px, the span's delimiter scrolls out of the visible-line range, and the popup blinks off/on; anchoring detection to the caret keeps it steady. Gated by its own `Live edit preview` toggle (`widget-edit-popup-preview`), separate from `Render widgets`. You edit source, you watch the render — without a WYSIWYG canvas. [widget-edit-popup-preview]
- **Multi-cursor / selection.** Union of all selection lines, same as `live-preview-selection-reveal-all`. A selection overlapping any part of a widget's span reveals (and previews) it.


## Caching and invalidation

Two caches, owned by different layers, keyed independently so a change invalidates the minimum:

- **SVG render cache (app, the `CachedDeco` layer).** Keyed on `(source_text, kind, style, font_px, theme_token_hash)`. The decoration providers already fingerprint on `(path, selection, folds, viewport, theme)` per `design.md`; the widget layer adds per-span source + font_px to that key. A re-render fires when the user edits the span, the editor font size / zoom changes (`editor-zoom`), or the theme flips (colors are baked into the SVG by the renderer). [widget-render-cache]
- **Texture cache (editor widget).** Keyed on `(svg_hash, target_size, device_pixel_ratio)` per `editor-widget-texture-cache`. A re-raster fires when the SVG changes, the widget is resized (soft-wrap width change), or the window moves to a different-DPR monitor. The editor asks the host for a redraw only when geometry changes (`editor-widget-redraw-on-geometry`).

The split means a theme flip re-renders SVG (new colors) and therefore re-rasters; a pure DPR change re-rasters only; scrolling does neither. Viewport scoping from the existing providers still applies — only spans within the rendered viewport range are rendered/rasterized, so a document with hundreds of equations pays only for what's on screen. [widget-render-viewport-scoped]

- **Persisted disk cache (app, below both in-memory layers).** Both caches above are in-memory and per-session, so the *first* open of a note in a session always misses and pays the full `resvg` blit. A third layer persists each rasterized diagram to disk so that cost survives across sessions: keyed by the **same `content_hash`** the render computes, it stores the RGBA8 output under `<vault>/.hiker/diagram-cache/` plus, for inline math only, a tiny `.base` sidecar carrying the baseline metric (width/height are self-described by the PNG). The disk cache is checked **before** the `resvg` blit and written back on a miss; for Mermaid the parse/layout still runs (it yields the interaction regions) but the blit is skipped. It sits *below* the in-memory caches — it never changes their behavior, only repopulates a render that would otherwise be recomputed. `.hiker/` holds only regenerable data (`design.md` `subsystem-notes-visible`), so the cache is safe to delete at any time, and is kept bounded by a best-effort byte-budget LRU sweep (oldest-by-mtime) run once per session. Gated by `[render] cache_diagrams` (default on, vault scope, `settings.md`); when off the in-memory caches carry the session. Tables paint natively (no `resvg` raster — `widget-table-render`), so there is nothing to persist for them. [widget-render-disk-cache, render-cache-diagrams-toggle]

**Theme-reactive color.** The renderers take foreground/axis colors from `MathOptions` / the Mermaid draw options; hiker passes the active theme's tokens (the same `theme.palette.accent` / `theme.markdown.code_bg` the current mark-only providers read), so light/dark both render correct contrast without a parallel stylesheet. [widget-render-theme-color]


## LaTeX math

First to land. Inline `$…$` and display `$$…$$`, via an `app` helper that wraps `hiker_math` with font size, color, style, and preamble options:

- **Style.** `$…$` → inline (compact, baseline-aligned `InlineWidget`); `$$…$$` → display (full size, centered, `BlockWidget`). [widget-latex-inline, widget-latex-display]
- **Font size.** Tracks the editor's body font size (and `editor-zoom` when it lands) so math scales with the prose around it.
- **Color.** Set from the active theme's text token so math contrasts correctly in light/dark.
- **Preamble.** A per-vault macro file (`\newcommand` / `\def`) threads through the helper's preamble argument (empty by default); the file path + loader are deferred (see Deferred).
- **No external runtime.** `hiker-render/math` is pure Rust (pulldown-latex parser + a TeXbook-Appendix-G layout over an OpenType MATH font). No KaTeX, no MathJax, no JS — consistent with the project's native-Rust posture. [widget-latex-native]

Detection already exists in `equations.rs` (block + inline scan, viewport-scoped); this feature swaps its mark-only output for the render path while keeping the reveal-time mark style as the cursor-in source view.


## Mermaid

Second, now landed. ` ```mermaid ` fenced blocks via `hiker_mermaid::render(src, &MermaidOptions) → Result<MermaidRender, MermaidError>` (SVG out), rasterized to a `BlockWidget` in place of the fence. `render` is the dispatcher — it sniffs the diagram header (honoring `---` frontmatter / `%%{init}%%` config for theme / `Look` / font) and routes to the per-type renderer. The crate covers a broad and growing set (flowchart, sequence, class, state, ER, gantt, gitgraph, pie, …) — several via `hiker-graph` layout, so new diagram types light up through the same provider with no app change. A type `render` can't parse returns `Err`, which the provider falls back to tinted source per `widget-render-error-fallback`. `MermaidOptions` carries font size, a classic/hand-drawn look, and theme colors, wired to the active editor theme. `mermaid_spans` (in `diagrams.rs`) feeds the provider; the `MermaidWidget` lives in `app/src/panels/buffer/widgets/`. [widget-mermaid-render]


## WaveDrom

` ```wavedrom ` fenced blocks (WaveJSON timing waveforms + bitfield/register diagrams) via `hiker_wavedrom::render(src, &WaveDromOptions) → Result<WaveDromRender, WaveDromError>` (SVG out), rasterized to a `BlockWidget` in place of the fence — the same SVG → RGBA → texture path as Mermaid. `render` auto-detects the WaveJSON family (`{signal:[…]}` → timing, `{reg:[…]}` / bare array → bitfield). `WaveDromOptions` carries `font_size_px`, foreground/background colors, and the categorical series palette; hiker threads the active theme's text token as the foreground and a transparent background (the diagram sits on the editor surface), keeping WaveDrom's default skin for the series palette (purpose-chosen, theme-neutral). A body that fails to parse (`WaveDromError::{Parse, Empty, Unsupported}`) returns `Err`, which the provider falls back to tinted source per `widget-render-error-fallback`. `wavedrom_spans` (in `diagrams.rs`, sharing Mermaid's language-parameterized fence scan) feeds the provider; the `WaveDromWidget` lives in `app/src/panels/buffer/widgets/`.

WaveDrom mirrors **display math** rather than Mermaid for interaction: it is a body-clickable block widget (click the rendered waveform → caret into the hidden source, `widget-block-click-to-edit`) with the live edit-preview popup, but it carries **no** interactive regions — WaveJSON has no `click` / link model, so there are no per-node hit regions, registry, or hover tooltips. The same `render_widgets` / `Live edit preview` toggles and markdown gating apply. [widget-wavedrom-render]


## Interactivity (links + tooltips)

Rendered diagrams aren't static images — flowchart and class diagrams carry per-node hit regions, so a mermaid `click` directive becomes a real clickable node.

- **Regions from the renderer.** `hiker_mermaid` returns hit regions (id, viewBox-px rect, link, tooltip) alongside the SVG for flowchart/class diagrams; other types return none (still render). The app normalizes each to a `DiagramRegion` with 0..1 fractional coords keyed off the render dimensions; the diagram `callback` is dropped (no JS engine). [widget-mermaid-links]
- **Regions become editor click-zones.** `BlockWidget::click_regions(font_size, width)` returns egui-free 0..1 regions with ids; the painter emits a `ClickZone { action: WidgetClick(id) }` per region plus the whole-widget fallback zone — the same multi-zone mechanism patch-review's per-hunk buttons use. Regions fire on **both** paint paths: on the texture path the painter maps each through the same letterbox transform as the blit; on the native path (`widget-block-native-paint`) it maps them linearly into the painted content box (no letterbox). The `font_size`/`width` args are the layout inputs (same as `measure`/`paint_list`) so a widget whose regions depend on its laid-out geometry — a **table**, whose per-cell rects depend on column widths at the paint-time width — computes them; resolution-independent regions (a diagram's normalized hit-boxes) ignore the args. Region ids carry a dedicated tag bit (distinct from wikilink's and, for tables, from mermaid's), mixed from the widget content hash + region index; mermaid keeps a per-buffer `id → { link, tooltip }` registry rebuilt each frame (raster-free), while table cells map `id → caret offset` through the shared edit-target map (`widget-table-cell-edit`). [editor-widget-click-regions]
- **Links resolve through one place.** A clicked region's link string goes through `core::url::classify → LinkTarget` and the app's single `diagram_nav::dispatch_link`: `External` (http(s)/mailto) → OS opener; `Zim { archive, article }` → resolve the archive path and open a ZimView tab; `VaultPath` / `Wikilink` → resolve via the index and open the note (unresolved wikilink → create-and-open). `core::url` is the unified link classifier — pure, store-free, shared by the diagram path and (incrementally) the existing wikilink / zim / external-open handlers so every link string in hiker classifies the same way. [url-classify]
- **Hover tooltips.** Hovering a region whose registry entry has a `tooltip` shows an egui tooltip at the pointer — host-side hit-testing of the region click-zones against the pointer, mirroring the wikilink hover card. [widget-diagram-hover-tooltip]


## Tables

Different in mechanism from math/mermaid: pipe tables render as a **natively-painted** `BlockWidget`, not an SVG round-trip. A table is structured layout (cells, alignment, borders) that egui draws directly far crisper than rasterizing an SVG of it, and re-lays-out for free on zoom / DPR / soft-wrap-width change with no re-raster. There is deliberately no `hiker-render` table engine.

This is the one widget kind that paints natively rather than through `pixels()`, so it builds the `BlockWidget` native-paint hook (`paint_list`, see Widget paint): the `TableWidget` returns an egui-free retained paint list — filled rects for header/cell backgrounds and borders, lines for grid rules, positioned text runs honoring each column's alignment — that the `editor-egui` painter replays with its own painter. `editor-core` stays egui-free (it describes *what* to draw, not *how*), and the hook is generic enough that any later structured native widget reuses it. [widget-block-native-paint]

The rest of the flow matches every other kind. `editor-md`'s `table_spans` detector walks the same parse tree the mark-only providers use (GFM tables already parse via `Options::ENABLE_TABLES`) and yields each pipe-table block's `byte_range`. The app provider (`table_widget_decorations`) parses rows / cells / column alignments, builds a `TableWidget: BlockWidget` that measures its own height (row count × wrapped cell content at the paint-time width, via the trait's `measure(font_size, width)`) and emits the paint list, hides the source lines (`LineStyle.hide`), and emits the `BlockWidget` — fingerprint-cached on the shared `render_fp` like the `math_widget` / `mermaid_widget` slots, viewport-scoped. Reveal-on-cursor lifts the hide and shows the raw pipe-and-dash source, same per-block trigger as the others (`widget-reveal-block`). A malformed table (no rule row) falls back to tinted source, never a crash (`widget-render-error-fallback`). [widget-table-render]

**Per-cell click-to-edit.** A table refines the whole-block `widget-block-click-to-edit` model to cell granularity: clicking a cell drops the caret at the **end of that cell's source content** (caret ready to append), which lands inside the table's span and so triggers reveal — the source shows with the caret already in the right cell. Mechanism: the table parse is position-aware (each cell records the byte offset just past its content, escapes-and-trailing-whitespace correct), and `TableWidget::click_regions(font_size, width)` emits one normalized region per cell (row-major, `id = table_cell_id(content_hash, i)` under a dedicated tag bit). The native painter maps each region into the painted box and emits a per-region click-zone (`editor-widget-click-regions`); the app registers every cell `id → absolute caret offset` (plus a whole-widget `id → table start` fallback for clicks off any cell) into the same `WidgetEditTargets` map mermaid/display-math use, so a cell click routes through the existing `place_caret_for_block_click` with no new drain bucket. Caret-to-exact-character within a cell is deferred — end-of-cell is the unit. [widget-table-cell-edit]


## Error fallback

A renderer returning `None` (math parse failure) or `Err` (Mermaid / WaveDrom parse/unsupported-type) never breaks the buffer: the provider falls back to the current tinted-source mark (the `equations.rs` / `diagrams.rs` mark styles — `mermaid_decorations` / `wavedrom_decorations`), plus a small error glyph in the gutter or at the span end with the parse error in its tooltip. The user keeps seeing and editing their source. [widget-render-error-fallback]


## Toggle and gating

- **`Render widgets`** — a new View-menu entry (`editor.md`'s `editor-view-options-menu`), default on, in-memory in v1 (persistence rides `settings.md`'s editor section later). Off shows the tinted-source marks (today's behavior). Distinct from `view-live-preview-toggle`, which governs Tier 1 marker fade — a user can have live preview on and rendered widgets off, or either alone. [widget-render-toggle]
- **`Live edit preview`** — a second View-menu entry, default on, gating only the floating edit popup. The popup shows when `render_widgets && live_edit_preview && is_markdown`; in-place rendering and diagram interactivity ride `render_widgets` alone, so turning the popup off leaves rendered widgets and clickable links intact. [widget-edit-popup-preview]
- **Markdown-buffer-gated.** Like live preview, the providers emit nothing when the buffer isn't rendered as markdown (`live-preview-disabled-non-md`). [widget-render-gating]


## Deferred

- **Inline images (`![alt](url)`).** Same pipeline with an image decode instead of an SVG render — texture cache, host paint, reveal-on-cursor all reuse. Adds fetch (local-path first; remote fetch needs the security model live preview's Tier 3 flags) and alt-text fallback. Lands as its own application of `widget-render-pipeline` once math/mermaid are solid. [widget-image-render]
- **Drag-to-resize.** Resizing a rendered image/diagram by dragging its corner, persisting the size in the source (an HTML width attr or a hiker-namespaced marker). UI + source-write contract deferred. [widget-render-resize]
- **Per-vault math preamble file.** The `MathOptions.preamble` field is plumbed; the file path + loader land with `settings.md`'s vault-config work. [widget-latex-preamble]
- **Math/diagram export.** Copy-as-image / copy-as-SVG on a rendered widget. Trivial given the SVG is already in hand; deferred until asked. [widget-render-export]
- **Background rendering for heavy diagrams.** v1 renders + rasterizes synchronously on the decoration-cache path (cached, viewport-scoped, so cost is bounded to on-screen spans). If a large Mermaid layout (dagre on a big graph) janks a frame, move the render to a background task that paints a spinner placeholder until the texture is ready — same `spawn_blocking` shape the embedder uses (`embedder-spawn-blocking`). Deferred until profiling shows it's needed; the synchronous path is correct, just potentially slow on pathological inputs. [widget-render-async]


## Out of scope

- **Editing diagrams visually.** Mermaid/WaveDrom/tables are edited as source, not through a WYSIWYG canvas. The draw.io family (`ideas.md` `[drawio-source-ingest]`) is a separate source-type concern, not an editor widget.
- **HTML/CSS rendering inside markdown.** Inline `<div>`/styled HTML is not rendered; that's the `hiker-render/htmlview` surface (the no-JS HTML/CSS renderer, e.g. the ZIM viewer in `zim.md`), not the markdown editor.
- **Syntax highlighting inside fenced code** — already owned by `editor-code-syntax-highlight` (`live-preview.md`), a styling layer, not a widget.
- **Anything mutating the source file.** Widgets are decoration-only (drag-to-resize, when it lands, is the single deliberate exception and writes through the normal edit path).
