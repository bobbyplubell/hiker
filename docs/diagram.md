# Diagram renderers

Hiker renders mermaid, WaveDrom, and LaTeX-math diagrams with its own pure-Rust engines — no browser, no JS toolchain. This doc covers the shared seam that unifies them (`hiker-diagram`, `hiker-render/diagram/`) and the two consumers that use the seam: the editor's inline diagnostics and the agent-facing `check_diagram` helper. The individual engines (`hiker-mermaid`, `hiker-wavedrom`, `hiker-math`) keep their own free `render_*` entry points and their own option types; this doc is about the common shape over them, not their internals.

The headline decisions:

- **One trait over every engine.** `hiker_diagram::DiagramRenderer` (a zero-sized marker per engine — `Mermaid`, `WaveDrom`, `Math`) exposes `render` and `check`, so a host can render or syntax-check any engine through one shape. Egui-agnostic and graphics-free, like the rest of `hiker-render`. [diagram-crate, diagram-render-trait]
- **A common rendered-output shape.** `render` returns a `DiagramRender` (`svg`, `width_px`, `height_px`) or a `Vec<Diagnostic>` of `{ message, span, severity }` on failure; the per-engine render structs flatten to it. [diagram-render-output]
status:: done
note:: common output: SVG + px size, or `Vec<Diagnostic>` of `{message, span, severity}`; per-engine render structs flatten to it · evidence: `hiker-render/diagram/src/lib.rs` (`DiagramRender`, `Diagnostic`, `Severity`)
- **`check()` is the parse-only syntax seam.** An empty `Vec` means well-formed; a non-empty one carries the problems. It does only the work needed to decide "does this parse?", with byte spans where the engine can cheaply localize them. [diagram-check]
status:: done
note:: parse-only syntax seam; empty `Vec` = OK; coarse v0 byte spans, enrichable; clamped into the checked block by the host · evidence: `hiker-render/diagram/src/lib.rs` (`DiagramRenderer::check`)
- **Two consumers, one seam.** The editor draws squiggle diagnostics under failing fenced blocks, and a `check_diagram(lang, src)` core/MCP helper lets an agent validate a diagram before writing it — both call the same `check()`. [diagram-editor-diagnostics, diagram-agent-check]


## The seam

`hiker-diagram` adds one common trait the three engines implement. [diagram-crate]
status:: done
note:: shared egui-free seam over the mermaid / wavedrom / math engines; each keeps its own `render_*` + options type · evidence: `hiker-render/diagram/src/lib.rs` (`hiker-diagram`)

```rust
pub trait DiagramRenderer {
    type Options;
    fn render(src: &str, opts: &Self::Options) -> Result<DiagramRender, Vec<Diagnostic>>;
    fn check(src: &str) -> Vec<Diagnostic>;
}
```

- **`DiagramRenderer`** is implemented as a zero-sized marker type per engine, so the trait is usable without an instance (`Mermaid::check(src)`). [diagram-render-trait]
status:: done
note:: per-engine zero-sized marker (`Mermaid`/`WaveDrom`/`Math`) with `render` + parse-only `check`; usable without an instance · evidence: `hiker-render/diagram/src/lib.rs` (`DiagramRenderer`)
- **`DiagramRender`** is the flattened output — a self-contained SVG document plus its CSS-pixel size. The per-engine render structs carry the same fields (math additionally carries a baseline); the trait flattens them to this one shape. [diagram-render-output]
- **`Diagnostic`** is `{ message, span: Option<Range<usize>>, severity }` with `Severity::{Error, Warning, Info}`. `span` is a byte range in the source when known; `None` is a coarse whole-source diagnostic the engine couldn't cheaply localize. The v0 spans are coarse and enrichable — a host clamps a returned span into the block it checked. [diagram-check]

`check()` does the minimum work to decide whether the source parses, rather than rendering and discarding the SVG. It is the editor/agent seam. [diagram-check]

### Math: diagnostics, not Option

The math engine's render/check path returns a `Result` carrying a `MathError` / diagnostics rather than an `Option`, so a malformed expression yields a message and (where localizable) a span instead of a silent `None`. This is what lets math participate in the shared `check()` seam alongside mermaid and WaveDrom. [diagram-math-diagnostics]
status:: done
note:: math render/check returns a `Result` carrying `MathError`/diagnostics (not `Option`), so it joins the shared `check()` seam · evidence: `hiker-render/math/src/lib.rs` (`MathError`, `check_latex`)


## Editor diagnostics

The editor draws a severity-colored squiggle (underline + gutter marker) under the source of any ```` ```mermaid ```` / ```` ```wavedrom ```` fenced block whose body fails `check()`. Each fence's inner source is checked via `check_diagram`; every returned `Diagnostic` is mapped into an editor decoration whose range is the engine's local span shifted into document byte coordinates (clamped to the fence body), or the whole inner block when the engine couldn't localize it. [diagram-editor-diagnostics]
status:: done
touches:: [[code:hiker/panels/buffer/widgets]]
note:: editor squiggle (underline + gutter) under ```mermaid/```wavedrom fences failing `check_diagram`; independent of the Render-widgets toggle · evidence: `app/src/panels/buffer/widgets/mod.rs` (`diagram_diagnostic_decorations`)

This layer does **not** depend on the "Render widgets" toggle — a malformed block is exactly the one that fails to render, so its squiggle must show even when the in-place render is off.

**Scope.** The editor squiggle path covers the fenced-block diagram languages (mermaid / wavedrom). Math is reachable through the shared seam from core/MCP but is **not** on the editor fence path, because math is inline `$…$` / display `$$…$$` rather than a ```` ```math ```` fence.


## Agent check

`hiker_core::diagrams::check_diagram(lang, src) -> Vec<Diagnostic>` routes a language tag (`mermaid` / `wavedrom` / `wavejson` / `math` / `latex`, case- and whitespace-insensitive) to the matching engine's `check()`. It is the single place both consumers call, so the editor and the agent see identical results. [diagram-agent-check]
status:: done
implements:: [[code:hiker/config/sections/McpToolsConfig#check_diagram_enabled]]
touches:: [[code:hiker/handler/dispatch/diagram]]
note:: `check_diagram(lang, src)` core helper + MCP tool returning `{ok, diagnostics}`; one seam shared with the editor; math reachable here (not on the fence path) · evidence: `core/src/diagrams.rs` (`check_diagram`), `mcp-server/src/handler/dispatch/diagram.rs`

The MCP `check_diagram` tool wraps it: a stateless, no-vault-access syntax check returning `{ ok, diagnostics: [...] }` (`ok` true iff there are no error-severity diagnostics), so an attached agent can validate a diagram before writing the fenced block into a note. Math is reachable here even though the editor fence path skips it.


## Out of scope

- **The `chart` engine.** A separate standalone repo; not part of this seam.
- **Engine internals.** Each engine's parsing, layout, and SVG emission live in its own crate (`hiker-mermaid` / `hiker-wavedrom` / `hiker-math`); this doc is only the shared trait + the two consumers.
- **draw.io and the JSON Canvas format.** Those are separate source-type / editor concerns (`ideas.md` [[spec:drawio-source-ingest]], `canvas.md`), not diagram renderers.
