# hiker-charts — Product Spec

A reusable charting component for native Rust UIs. It turns tabular data (CSV today)
into charts described by a **compact, declarative specification** — so a chart is a small
piece of text that lives next to the data and notes it describes, version-controllable,
diffable, and reproducible.

It offers **two co-equal authoring surfaces over one model**: a *text* surface (edit the
spec directly) and a *visual builder* (point-and-click). Both are bidirectional views of
the same spec — an edit in one is reflected in the other.

The rendering core is **UI-toolkit-agnostic**; egui is the reference GUI host. The
component produces a self-contained rendered artifact (SVG, rasterized pixels, or a native
draw list) that a host can embed anywhere.

This document is the **user-facing requirements** — what the component does, not how it is
built. The how lives in a companion `IMPLEMENTATION.md`.

---

## 1. What this is

A drop-in chart component a host application embeds. Given a chart spec plus tabular data,
it computes and paints a chart. The host owns everything around it — where the spec text
lives, how data files are read, theming, change-watching, and where the rendered output is
placed.

It is **not** a data-analysis tool or dashboard application. The value is narrow and
deliberate: a chart definition that is **co-located** with its source, **reproducible** from
text, and **diffable** in version control. Heavy data wrangling is out of band — in v1 the
data is assumed plot-ready.

---

## 2. Specification format

### 2.1 A compact grammar of graphics
A chart is described by a small, declarative **grammar-of-graphics** format: `data` + `mark` +
`encoding`, where each encoding channel binds a data field to a visual channel (position,
color, …) with a declared type. The model is borrowed in *shape* from established
grammar-of-graphics designs, but the format is the component's **own**, not a subset of an
external standard. It describes *what chart*, not *how it is drawn*, so it stays independent
of any particular rendering backend.

### 2.2 The format is exactly what can be rendered
The format's feature set is **bounded by the rendering substrate's capabilities** (§4): a
mark, channel, or scale exists in the format only if it can be drawn — so there is no "the
spec says X but we can't draw it" gap. Unknown or malformed fields are reported as
diagnostics, never silently dropped.

### 2.3 v1 scope
- **Marks:** `bar`, `line`, `point`, `area`.
- **Encoding channels:** `x`, `y`, `color`.
- **Types:** `quantitative`, `temporal`, `ordinal`, `nominal`, declared in the encoding.
- **Data:** inline values, or a CSV referenced by identifier.

### 2.4 Growth and optional portability bridge
Coverage grows by adding marks, channels, scales, and (later) optional data transforms — each
bounded by what the renderer supports (§2.2). The format is intentionally shaped to resemble
established interchange grammars, so an **optional exporter** to such a grammar (for opening a
chart in an external viewer) stays cheap to add later. Cross-tool portability is a *future
bridge*, not a foundational dependency.

---

## 3. Data: model, sources, dependencies

### 3.1 Data is supplied, never read directly
A spec references its data by an opaque **identifier** (path or URI). The component does
**not** read files itself — the host's **data resolver** (§10) maps an identifier to tabular
rows, keeping file access, sandboxing, and network policy host-side. Inline data (values
embedded in the spec) is also supported.

### 3.2 Declared dependencies
The component exposes the **set of data identifiers a spec depends on** (§10), so the host can
watch those sources and ask for a re-render when they change. It does not poll or watch
anything itself.

### 3.3 Identifiers are opaque
The component never interprets, resolves, rewrites, or persists a data identifier. Renames,
moves, and relinking are the host's concern; the component only ever hands an identifier to
the resolver.

### 3.4 Typing and coercion
- The encoding's declared **type** is authoritative.
- When a type is absent, it is **inferred** by sniffing the column.
- The host may optionally supply an external **column-type schema** (e.g. Frictionless Table
  Schema) to make typing explicit and portable.
- All string→typed coercion (numbers, dates/times) is **self-contained** — no native or
  system math/locale libraries.

### 3.5 Tabular abstraction
CSV (delimited text) is the v1 input, but the resolver interface is **format-agnostic**: it
yields typed columns/rows, so a host can back it with any tabular source.

---

## 4. Rendering

### 4.1 One first-party geometry engine, pluggable backends
A single **first-party** geometry engine computes scales, ticks, layout, axes, and mark
geometry **once**, independent of any drawing target. A **backend** then paints that geometry,
and backends are swappable without touching chart logic.

The geometry is the component's **own native code** — scale math, tick generation, axes,
legends, and mark shapes — **not** a wrapper over a third-party charting library. Only the
lowest-level substrate (e.g. rasterizing a finished SVG to pixels) may be delegated to an
existing library. Owning the renderer is deliberate: the output stays fully under the
component's control (theming, layout, exact shape) with no foreign API to fight, the
dependency footprint stays small, and it is what lets the format and the drawing capability
map one-to-one (§2.2).

### 4.2 SVG backend (v1 reference)
Produces a **self-contained SVG** (and a rasterized RGBA form) suitable for embedding as a
static image, snapshotting, or export. This is the portable, host-cheap path: render once,
cache, blit.

### 4.3 Native draw-list backend
Paints chart geometry directly into the GUI toolkit's draw list. It is
**resolution-independent** (crisp at any zoom) and cheap to redraw per frame. This backend
powers the visual builder's live preview and sharp display when a host scales the chart
(e.g. inside a zoomable spatial canvas), and is the foundation for future interactivity.

### 4.4 Determinism
Identical `(spec + resolved data + theme + size)` inputs produce **byte-identical** output.
This enables golden-file/snapshot testing.

### 4.5 No heavyweight runtime
Rendering is native code. **No embedded browser, no JavaScript engine**, no reliance on an
external service. The SVG path has no GPU requirement.

### 4.6 Self-contained math
Any numerical work — scale computation now, binning/regression/density later — is
implemented without native/system numerical libraries.

---

## 5. Sizing & layout

### 5.1 Constraint-driven
The chart renders to a host-provided **available width** with an aspect/height policy
(fill-width at an aspect ratio, or explicit dimensions). Given constraints, it reports the
size it occupies so the host can reserve space.

### 5.2 DPI-aware
Rasterized output is produced at the host's pixel ratio for sharpness. The native draw-list
backend is resolution-independent and needs no DPI input.

---

## 6. Theming

### 6.1 Injected palette
**All** colors — background, axis, gridlines, text, and categorical/series scales — come
from a **host-supplied theme**. Nothing is hardcoded, so a chart matches the host's light or
dark appearance automatically.

### 6.2 Theme is part of identity
A theme change alters the rendered output and therefore the content identity (§7), so caches
invalidate correctly on theme switches.

### 6.3 Spec overrides
A spec may override palette choices (via the format's config/encoding) but **defaults to the
injected theme** when it does not.

---

## 7. Caching & identity

The component exposes a **stable content hash** over `(spec + resolved data + theme + size)`.
A host keys its rendered-output cache on this hash: any change to any input → new hash →
re-render; no change → cache hit, no work. This is the single contract that lets a host
re-render exactly when (and only when) an input — including the referenced data file —
actually changes, never per frame for a static chart.

---

## 8. Authoring surfaces

### 8.1 Text surface
The spec text is the **source of truth** and round-trips **losslessly**. Parse errors,
unknown fields, and unsupported-feature notices are reported back to the host for display.

### 8.2 Visual builder
A GUI panel to: pick a data source, choose a mark, bind columns to channels (`x`/`y`/`color`)
by selection or drag, set types, and see a **live preview**. Every action mutates the same
spec model; the spec a builder produces is identical to one authored by hand.

### 8.3 Bidirectional and lossless
Text edits update the builder; builder edits update the text. Neither direction is lossy.
The builder **preserves fields it does not understand** — it never reorders or drops parts of
a spec outside its knowledge.

### 8.4 Both are optional
A host may embed the engine alone (text-only), the builder alone, or both. The builder is a
separate layer that depends on the engine, not the reverse.

### 8.5 Compact syntaxes share the engine
Terser, purpose-specific chart syntaxes (e.g. a minimal inline syntax for quick bar/line
charts) are implemented as **thin front-ends that lower to the same spec model**, never as
parallel renderers. The geometry engine, backends, theming, and caching are shared; only the
surface parsing differs.

---

## 9. Packages

Mirrors the established "portable core + thin toolkit layer" split:

- **`core`** — UI-agnostic engine: the spec model (grammar-of-graphics types + serde), the
  data model and typing/coercion, the **first-party** scale/layout/geometry engine, the
  backend trait, the SVG backend, content hashing, dependency extraction, the host interfaces
  (data resolver, theme), and the visual builder's **UI-agnostic state model and edit
  transitions**. **No GUI-toolkit dependency.**
- **`gui`** — the reference egui layer: a **thin adapter** that renders the builder state,
  provides the native draw-list backend, and shows the live preview. The interaction logic
  lives in `core`; this layer only binds it to the toolkit. Depends on `core` + the toolkit.
- **`cli` / examples** — headless render (`spec + data → SVG/PNG`) for snapshot tests and
  batch export.

---

## 10. Host integration surface

The interfaces a host implements or consumes:

| Interface | Direction | Responsibility |
| --- | --- | --- |
| **Data resolver** | host implements | identifier → typed tabular rows; owns files/sandbox/network |
| **Theme / palette** | host implements | supplies all colors |
| **Dependency query** | host consumes | the data identifiers a spec depends on, for change-watching |
| **Content hash** | host consumes | cache key over `(spec + data + theme + size)` |
| **Size constraints** | host provides | available width + aspect/height policy |
| **Output sink** | host consumes | receives SVG / RGBA / native draw list |
| **Diagnostics** | host consumes | parse errors, unsupported-feature notices |

---

## 11. Out of scope (v1)

- **Data transforms** — aggregation, filtering, joins, statistics. Deferred to the format's
  `transform` layer; v1 data is assumed plot-ready.
- **Interactivity** — hover, tooltips, zoom, pan, drill-down, selection. The native backend
  enables it later; v1 is static.
- **Composition beyond the v1 scope** — layering, faceting, concatenation, repetition.
- **Geographic/map marks**, animation, and non-tabular data.
- **Wholesale adoption of an external chart-spec standard** as the native format (§2.1).
  Interop is via an optional exporter (§2.4), not by rendering a foreign spec directly.

---

## 12. Performance expectations

- A static render of a **modest** dataset (up to a few thousand marks) is fast enough to
  produce on change without perceptible lag, and is cached after first paint.
- **Large** datasets (tens of thousands of marks) are a known limit of the SVG/raster path;
  the native backend and/or data reduction address this later, not in v1.
- Re-rendering happens **only on input change** (§7), never per frame for a static chart.

---

## 13. Testing & determinism

- **Golden-file SVG snapshots** across the v1 scope (each mark × encoding combination).
- **Round-trip tests:** `spec → builder model → spec` is identity, including preservation of
  unknown fields.
- **Coercion tests:** string→number and string→date/time edge cases.
- All enabled by the determinism guarantee in §4.4.
