//! App-side editor widgets: LaTeX math + Mermaid diagram rendering plus the
//! decoration providers that wire detection (`editor_md::equations::math_spans`,
//! `editor_md::diagrams::mermaid_spans`) to the render pipeline (the sibling
//! [`render`] module's `render_math` / `render_mermaid`) and emit
//! `InlineWidget` / `BlockWidget` decorations into the live buffer.
//!
//! This is the `editor-md`-detect → `app`-render → `editor-egui`-blit seam for
//! widgets (docs/editor-widgets.md). `editor-md` reports spans; this module owns
//! the render + decoration emission (so `editor-md` keeps no `hiker-render` or
//! egui dependency); the editor crates own the texture upload/cache/blit.
//!
//! Reveal: an inline `$…$` shows its formula only when no cursor/selection is
//! on its line; a display `$$…$$` or a ```` ```mermaid ```` fence hides its
//! source lines and renders a block widget only when no cursor/selection is
//! inside the span. When a span *is* revealed (the cursor is editing it) this
//! decoration layer emits no in-place widget — the source stays visible (the
//! `equations.rs` / `diagrams.rs` mark, or the lifted `hide`) and the live
//! render shows in the floating edit-preview overlay instead (the sibling
//! [`edit_preview`] module, painted as a non-interactive egui `Area`). The
//! [`active_preview_span`] selector here picks the single span the main cursor
//! is revealing, reusing the same reveal predicates this layer uses, and the
//! overlay renders + anchors it (`widget-reveal-inline`, `widget-reveal-block`,
//! `widget-edit-popup-preview`).
//!
//! status: widget-render-providers, widget-mermaid-render

use std::sync::Arc;

use editor_core::decoration::{
    BlockWidget, Decoration, InlineWidget, Set as DecorationSet, WidgetPixels,
};
use editor_core::rangeset::RangeSet;
use editor_core::state::Editor as EditorState;
use editor_core::theme::Theme;
use editor_md::diagrams::{mermaid_spans, wavedrom_spans};
use editor_md::equations::{MathKind as SpanKind, math_spans};

pub mod chart;
pub mod diagram_diagnostics;
pub mod disk_cache;
pub mod edit_preview;
pub mod placement;
pub(crate) mod render;
pub mod inline; pub mod tables;

#[cfg(test)]
mod tests;

use std::collections::HashMap;

use disk_cache::DiagramCacheCtx;
use editor_core::decoration::WidgetClickRegion;
use placement::{
    cursor_inside, emit_block_widget, fit_block_height, line_active, selection_overlaps,
};
use render::{
    DiagramRegion, MathKind, MermaidColors, RenderedWidget, WaveDromColors, mermaid_regions,
    render_math, render_mermaid_with_regions, render_wavedrom,
};

/// Tag bit OR-ed into a diagram-region widget-click id. Distinct from
/// `editor_md::links::WIKILINK_WIDGET_TAG` (`1 << 62`) and from a widget's
/// content-hash `widget_id()`, so the buffer panel tells interactive
/// diagram-region clicks apart from wikilink pills and diff-overlay buttons.
/// The low bits carry a content-derived region id (see [`region_click_id`]).
/// status: widget-mermaid-links
pub const MERMAID_REGION_TAG: u64 = 1 << 61;

/// What a clicked / hovered diagram region resolves to: the raw `click`-directive
/// link string (classified at dispatch time) and an optional hover tooltip.
/// status: widget-mermaid-links
#[derive(Clone, Debug, PartialEq)]
pub struct DiagramLink {
    pub link: Option<String>,
    pub tooltip: Option<String>,
}

/// Per-buffer map from a diagram-region widget-click id to its link + tooltip.
/// Rebuilt each frame by [`mermaid_link_registry`] (cache-free, no raster) so
/// the buffer panel can resolve this frame's diagram-region clicks / hovers.
/// status: widget-mermaid-links
pub type DiagramRegionRegistry = HashMap<u64, DiagramLink>;

/// Collision-free id for region `index` of the diagram whose render carries
/// `content_hash`. Mixes the two and folds into the low 61 bits, then OR-s the
/// [`MERMAID_REGION_TAG`] — so the tag and the wikilink tag (bit 62) stay clear
/// and two diagrams' regions never collide unless their content hashes do.
const fn region_click_id(content_hash: u64, index: usize) -> u64 {
    let mixed = content_hash
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ (index as u64).wrapping_add(0x1234_5678);
    MERMAID_REGION_TAG | (mixed & ((1 << 61) - 1))
}

/// An inline `$…$` math widget. Holds the rendered RGBA + metrics; the editor
/// blits it baseline-aligned among the surrounding glyphs.
struct InlineMath {
    rendered: RenderedWidget,
    dpr: f32,
}

impl InlineWidget for InlineMath {
    fn measure(&self, _font_size: f32) -> (f32, f32) {
        // `rendered` is physical px (logical × dpr); the painter reserves a
        // rect in *logical points*, so divide back out (the HiDPI gotcha).
        (
            self.rendered.width as f32 / self.dpr,
            self.rendered.height as f32 / self.dpr,
        )
    }

    fn widget_id(&self) -> u64 {
        self.rendered.content_hash
    }

    fn baseline(&self) -> Option<f32> {
        // Physical-px baseline → logical points so it lines up with the row
        // baseline (which the painter works in points).
        self.rendered.baseline.map(|b| b / self.dpr)
    }

    fn pixels(&self) -> Option<WidgetPixels<'_>> {
        Some(WidgetPixels {
            rgba: &self.rendered.rgba,
            width: self.rendered.width,
            height: self.rendered.height,
        })
    }
}

/// A display `$$…$$` math widget. Full-width own-height block.
struct DisplayMath {
    rendered: RenderedWidget,
    dpr: f32,
}

impl BlockWidget for DisplayMath {
    fn measure(&self, _font_size: f32, width: f32) -> f32 {
        // Own-height in logical points (physical px / dpr), scaled to fit the
        // content width so a wide formula doesn't overflow the column.
        fit_block_height(&self.rendered, self.dpr, width)
    }

    fn widget_id(&self) -> u64 {
        self.rendered.content_hash
    }

    fn handles_click(&self) -> bool {
        // Emit a whole-widget body-click zone so a click on the rendered block
        // routes to the click-to-edit path (place the caret in the hidden
        // source), matching mermaid. status: widget-block-click-to-edit
        true
    }

    fn pixels(&self) -> Option<WidgetPixels<'_>> {
        Some(WidgetPixels {
            rgba: &self.rendered.rgba,
            width: self.rendered.width,
            height: self.rendered.height,
        })
    }
}

/// A ```` ```mermaid ```` diagram widget. Full-width own-height block, same
/// shape as [`DisplayMath`], plus the interactive `click`-directive regions
/// (`widget-mermaid-links`): each region with a `link` emits a per-region
/// [`WidgetClickRegion`] tagged via [`region_click_id`] so the buffer panel can
/// open it; the same ids back the per-buffer [`DiagramRegionRegistry`].
struct MermaidWidget {
    rendered: RenderedWidget,
    dpr: f32,
    /// Normalized interaction regions for this diagram. Empty for diagram types
    /// with no `click` model, or when no `click` directive is present.
    regions: Vec<DiagramRegion>,
}

impl BlockWidget for MermaidWidget {
    fn measure(&self, _font_size: f32, width: f32) -> f32 {
        // Own-height in logical points, scaled to fit the content width.
        fit_block_height(&self.rendered, self.dpr, width)
    }

    fn widget_id(&self) -> u64 {
        self.rendered.content_hash
    }

    fn handles_click(&self) -> bool {
        true
    }

    fn pixels(&self) -> Option<WidgetPixels<'_>> {
        Some(WidgetPixels {
            rgba: &self.rendered.rgba,
            width: self.rendered.width,
            height: self.rendered.height,
        })
    }

    fn click_regions(&self, _font_size: f32, _width: f32) -> Vec<WidgetClickRegion> {
        // Regions are normalized to the SVG viewBox — resolution-independent, so
        // the layout inputs are ignored. Only linked regions are clickable;
        // tooltip-only regions are surfaced for hover via the registry, not as
        // click zones. status: widget-mermaid-links
        self.regions
            .iter()
            .enumerate()
            .filter(|(_, r)| r.link.is_some())
            .map(|(i, r)| WidgetClickRegion {
                x: r.x,
                y: r.y,
                w: r.w,
                h: r.h,
                id: region_click_id(self.rendered.content_hash, i),
            })
            .collect()
    }
}

/// A ```` ```wavedrom ```` diagram widget. Full-width own-height block, same
/// shape as [`DisplayMath`]: a body click drops the caret into the hidden
/// source (`widget-block-click-to-edit`). WaveJSON has no `click` / link model,
/// so — unlike [`MermaidWidget`] — it carries no interactive regions.
/// status: widget-wavedrom-render
struct WaveDromWidget {
    rendered: RenderedWidget,
    dpr: f32,
}

impl BlockWidget for WaveDromWidget {
    fn measure(&self, _font_size: f32, width: f32) -> f32 {
        // Own-height in logical points, scaled to fit the content width — a
        // wide waveform / bitfield shrinks to the column instead of overflowing.
        fit_block_height(&self.rendered, self.dpr, width)
    }

    fn widget_id(&self) -> u64 {
        self.rendered.content_hash
    }

    fn handles_click(&self) -> bool {
        // Whole-widget body click → caret into the hidden source, matching
        // display math / mermaid. status: widget-block-click-to-edit
        true
    }

    fn pixels(&self) -> Option<WidgetPixels<'_>> {
        Some(WidgetPixels {
            rgba: &self.rendered.rgba,
            width: self.rendered.width,
            height: self.rendered.height,
        })
    }
}

/// Build the math-widget decoration layer for the current editor state.
///
/// Renders each on-screen span (viewport-scoped) via the SVG → RGBA pipeline,
/// applies reveal-on-cursor, and emits `InlineWidget` / `BlockWidget`
/// decorations (plus `hide` lines for revealed-off display math). Spans that
/// fail to render emit nothing — the tinted-source `math_decorations` mark is
/// the fallback (`widget-render-error-fallback`).
///
/// `fg` is the active theme's text token (`widget-render-theme-color`); `dpr`
/// is the device pixel ratio; `font_px` the editor body font size.
pub fn math_widget_decorations(
    state: &EditorState,
    theme: Option<&Theme>,
    viewport: Option<&std::ops::Range<usize>>,
    font_px: f32,
    dpr: f32,
    cache: Option<&DiagramCacheCtx>,
) -> DecorationSet {
    let fg = theme_fg(theme);
    let total_lines = state.doc.len_lines();
    let doc_len = state.doc.len_bytes();
    let line_byte_end = |line: usize| -> usize {
        if line + 1 < total_lines {
            state.doc.line_to_byte(line + 1)
        } else {
            doc_len
        }
    };

    // Hoisted out of the per-span loop: this layer now emits the whole
    // document, so materializing the doc text once and slicing per span keeps
    // it O(doc) instead of O(spans × doc).
    let doc_text = state.doc.to_string();
    let mut entries: Vec<(std::ops::Range<usize>, Decoration)> = Vec::new();
    for span in math_spans(state, viewport) {
        match span.kind {
            SpanKind::Inline => {
                // Revealed (cursor on the line / selection overlap): the source
                // stays inline via the `equations.rs` mark and the live render
                // shows in the floating edit-preview overlay (`edit_preview`,
                // `widget-edit-popup-preview`) — this layer emits no in-place
                // widget. Otherwise render the inline formula in place.
                let revealed = line_active(state, &span.byte_range)
                    || selection_overlaps(state, &span.byte_range);
                if revealed {
                    continue;
                }
                let src = &doc_text[span.inner_range.clone()];
                let Some(rendered) = render_math(src, MathKind::Inline, font_px, dpr, fg, "", cache)
                else {
                    continue; // parse failure → fall back to the source mark
                };
                entries.push((
                    span.byte_range.clone(),
                    Decoration::InlineWidget {
                        widget: Arc::new(InlineMath { rendered, dpr }),
                        atomic: true,
                    },
                ));
            }
            SpanKind::Display => {
                // Reveal: cursor anywhere inside the span (delimiters inclusive)
                // or a selection overlap. Revealed → the source lines stay
                // visible and the live render shows in the floating edit-preview
                // overlay (`edit_preview`); this layer emits nothing. Otherwise
                // hide the source and render the block in place.
                let revealed = cursor_inside(state, &span.byte_range)
                    || selection_overlaps(state, &span.byte_range);
                if revealed {
                    continue;
                }
                let src = &doc_text[span.inner_range.clone()];
                let Some(rendered) = render_math(src, MathKind::Display, font_px, dpr, fg, "", cache)
                else {
                    continue;
                };
                emit_block_widget(
                    state,
                    &span.byte_range,
                    Arc::new(DisplayMath { rendered, dpr }),
                    total_lines,
                    &line_byte_end,
                    &mut entries,
                );
            }
        }
    }
    RangeSet::from_iter(entries)
}

/// Build the mermaid-widget decoration layer for the current editor state.
///
/// Mirrors [`math_widget_decorations`]'s display-math path: renders each
/// on-screen ```` ```mermaid ```` fence (viewport-scoped) via the SVG → RGBA
/// pipeline, applies reveal-on-cursor (cursor anywhere inside the fence span or
/// a selection overlap shows the source), and otherwise hides the fence lines +
/// emits a `BlockWidget`. A fence that fails to render emits nothing — the
/// tinted-source `mermaid_decorations` mark is the fallback
/// (`widget-render-error-fallback`). status: widget-mermaid-render
pub fn mermaid_widget_decorations(
    state: &EditorState,
    theme: Option<&Theme>,
    viewport: Option<&std::ops::Range<usize>>,
    font_px: f32,
    dpr: f32,
    cache: Option<&DiagramCacheCtx>,
) -> DecorationSet {
    let colors = theme_mermaid_colors(theme);
    let total_lines = state.doc.len_lines();
    let doc_len = state.doc.len_bytes();
    let line_byte_end = |line: usize| -> usize {
        if line + 1 < total_lines {
            state.doc.line_to_byte(line + 1)
        } else {
            doc_len
        }
    };

    let doc_text = state.doc.to_string();
    let mut entries: Vec<(std::ops::Range<usize>, Decoration)> = Vec::new();
    for span in mermaid_spans(state, viewport) {
        // Reveal: cursor anywhere inside the fence span (delimiters inclusive)
        // or a selection overlap. Revealed → the fence lines stay visible and
        // the live render shows in the floating edit-preview overlay
        // (`edit_preview`); this layer emits nothing. Otherwise hide the fence
        // lines and render the diagram in place.
        let revealed = cursor_inside(state, &span.byte_range)
            || selection_overlaps(state, &span.byte_range);
        if revealed {
            continue;
        }
        let src = &doc_text[span.inner_range.clone()];
        let Some((rendered, regions)) =
            render_mermaid_with_regions(src, font_px, dpr, colors, cache)
        else {
            continue; // parse / unsupported-type failure → fall back to the mark
        };
        let widget = MermaidWidget { rendered, dpr, regions };
        emit_block_widget(
            state,
            &span.byte_range,
            Arc::new(widget),
            total_lines,
            &line_byte_end,
            &mut entries,
        );
    }
    RangeSet::from_iter(entries)
}

/// Build the wavedrom-widget decoration layer for the current editor state.
///
/// Mirrors [`mermaid_widget_decorations`] (minus interaction regions): renders
/// each on-screen ```` ```wavedrom ```` fence (viewport-scoped) via the
/// SVG → RGBA pipeline, applies reveal-on-cursor (cursor anywhere inside the
/// fence span or a selection overlap shows the source), and otherwise hides the
/// fence lines + emits a `BlockWidget`. A fence that fails to render emits
/// nothing — the tinted-source `wavedrom_decorations` mark is the fallback
/// (`widget-render-error-fallback`). status: widget-wavedrom-render
pub fn wavedrom_widget_decorations(
    state: &EditorState,
    theme: Option<&Theme>,
    viewport: Option<&std::ops::Range<usize>>,
    font_px: f32,
    dpr: f32,
    cache: Option<&DiagramCacheCtx>,
) -> DecorationSet {
    let colors = theme_wavedrom_colors(theme);
    let total_lines = state.doc.len_lines();
    let doc_len = state.doc.len_bytes();
    let line_byte_end = |line: usize| -> usize {
        if line + 1 < total_lines {
            state.doc.line_to_byte(line + 1)
        } else {
            doc_len
        }
    };

    let doc_text = state.doc.to_string();
    let mut entries: Vec<(std::ops::Range<usize>, Decoration)> = Vec::new();
    for span in wavedrom_spans(state, viewport) {
        let revealed = cursor_inside(state, &span.byte_range)
            || selection_overlaps(state, &span.byte_range);
        if revealed {
            continue;
        }
        let src = &doc_text[span.inner_range.clone()];
        let Some(rendered) = render_wavedrom(src, font_px, dpr, colors, cache) else {
            continue; // parse / unsupported failure → fall back to the mark
        };
        emit_block_widget(
            state,
            &span.byte_range,
            Arc::new(WaveDromWidget { rendered, dpr }),
            total_lines,
            &line_byte_end,
            &mut entries,
        );
    }
    RangeSet::from_iter(entries)
}

/// Build this frame's diagram-region registry: for every on-screen mermaid
/// fence, classify its interaction regions (link / tooltip) and key them by the
/// same [`region_click_id`] the [`MermaidWidget`]'s `click_regions()` emits, so
/// the buffer panel can resolve a diagram-region click or hover.
///
/// Cache-free and raster-free relative to the widget layer: it re-derives the
/// content hash + regions (parse + layout, no resvg blit) so the registry is
/// always current even on frames where the decoration cache serves the widget
/// layer from memory. Returns tooltip-only regions too (for hover), not just
/// the clickable linked ones. status: widget-mermaid-links / widget-diagram-hover-tooltip
pub fn mermaid_link_registry(
    state: &EditorState,
    theme: Option<&Theme>,
    viewport: Option<&std::ops::Range<usize>>,
    font_px: f32,
    dpr: f32,
) -> DiagramRegionRegistry {
    let colors = theme_mermaid_colors(theme);
    let mut registry = DiagramRegionRegistry::new();
    let doc = state.doc.to_string();
    for span in mermaid_spans(state, viewport) {
        let src = &doc[span.inner_range.clone()];
        let Some((content_hash, regions)) = mermaid_regions(src, font_px, dpr, colors) else {
            continue;
        };
        for (i, r) in regions.iter().enumerate() {
            if r.link.is_none() && r.tooltip.is_none() {
                continue;
            }
            registry.insert(
                region_click_id(content_hash, i),
                DiagramLink { link: r.link.clone(), tooltip: r.tooltip.clone() },
            );
        }
    }
    registry
}

/// Per-buffer map from a block widget's whole-widget click id (its render
/// `content_hash`, the bare `widget_id()`) to the byte offset a body click
/// should place the caret at — the span's `inner_range.start`, inside the span
/// so the reveal predicates (`cursor_inside`) fire and the source + edit-preview
/// popup show. Built for both mermaid fences and display math.
/// status: widget-block-click-to-edit
pub type WidgetEditTargets = HashMap<u64, usize>;

/// Build this frame's click-to-edit target map: for every on-screen block widget
/// (mermaid fence + display math), map its whole-widget click id (the render
/// `content_hash`) to a caret offset *inside* its span. A body click on a
/// rendered block widget routes through this so the caret lands in the hidden
/// source, triggering the existing reveal (source shows + edit-preview popup).
///
/// Raster-free: mermaid uses [`mermaid_regions`] (no resvg blit) and math uses
/// the would-be [`render::math_content_hash`] — both share the exact ids the
/// widget layer's `widget_id()` emits, so a body click resolves here.
/// `viewport` scopes the scan like the providers. status: widget-block-click-to-edit
pub fn widget_edit_targets(
    state: &EditorState,
    theme: Option<&Theme>,
    viewport: Option<&std::ops::Range<usize>>,
    font_px: f32,
    dpr: f32,
) -> WidgetEditTargets {
    let mut targets = WidgetEditTargets::new();
    let doc = state.doc.to_string();

    let colors = theme_mermaid_colors(theme);
    for span in mermaid_spans(state, viewport) {
        let src = &doc[span.inner_range.clone()];
        if let Some((content_hash, _)) = mermaid_regions(src, font_px, dpr, colors) {
            targets.insert(content_hash, span.inner_range.start);
        }
    }

    let fg = theme_fg(theme);
    for span in math_spans(state, viewport) {
        if span.kind != SpanKind::Display {
            continue; // only block widgets carry a whole-widget body click
        }
        let src = &doc[span.inner_range.clone()];
        let hash = render::math_content_hash(src, MathKind::Display, font_px, dpr, fg);
        targets.insert(hash, span.inner_range.start);
    }

    let wd_colors = theme_wavedrom_colors(theme);
    for span in wavedrom_spans(state, viewport) {
        let src = &doc[span.inner_range.clone()];
        let hash = render::wavedrom_content_hash(src, font_px, dpr, wd_colors);
        targets.insert(hash, span.inner_range.start);
    }

    // Tables contribute per-cell targets (cell click → caret at that cell's
    // content end) plus a whole-widget fallback. status: widget-table-cell-edit
    for (id, offset) in tables::table_edit_targets(state, theme, viewport, font_px) {
        targets.insert(id, offset);
    }

    // Charts route a LEFT click to caret-into-source like the other block
    // widgets (the right-click menu opens the builder instead). status: chart-open-in-builder
    for (id, t) in chart::edit_targets(state, theme, viewport, dpr) {
        targets.insert(id, t.inner_range.start);
    }

    targets
}

/// Which consumer a `WidgetClick(id)` routes to. status: widget-block-click-to-edit
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WidgetClickBucket {
    /// Interactive mermaid region (link dispatch).
    Diagram,
    /// Block-widget body / table cell / chart → caret-into-source. (A chart's
    /// *right*-click opens the builder; its left click reveals source here.)
    Edit,
    /// Wikilink pill → open the target.
    Wikilink,
    /// Unclaimed → falls to the diff-overlay hunk consumer.
    Other,
}

/// Route a `WidgetClick` id to its consumer. **Membership-keyed consumers come
/// first, the wikilink BIT test last** — deliberately. A block widget's
/// whole-widget body-click id is a bare `content_hash` (full 64-bit) that can
/// coincidentally set any reserved tag bit, `WIKILINK_WIDGET_TAG` (bit 62)
/// included; checking that bit first stole ~half of all mermaid / display-math
/// body clicks into the wikilink handler, which silently dropped them (the
/// reported "clicking the diagram does nothing"). Region ids (`region_click_id`,
/// bit 61, wikilink bit clear) and table-cell ids (`table_cell_id`, bit 60,
/// wikilink bit clear) are minted with bit 62 clear, and a genuine wikilink pill
/// id (`WIKILINK_WIDGET_TAG | small_index`) is never a content hash, so it never
/// lands in either map — making membership-first both correct and unambiguous.
/// status: widget-block-click-to-edit
pub fn classify_widget_click(
    id: u64,
    diagram_registry: &DiagramRegionRegistry,
    edit_targets: &WidgetEditTargets,
) -> WidgetClickBucket {
    if diagram_registry.contains_key(&id) {
        WidgetClickBucket::Diagram
    } else if edit_targets.contains_key(&id) {
        WidgetClickBucket::Edit
    } else if id & editor_md::links::WIKILINK_WIDGET_TAG != 0 {
        WidgetClickBucket::Wikilink
    } else {
        WidgetClickBucket::Other
    }
}

/// Derive the mermaid draw colors from the active theme so light/dark render
/// correct contrast without a parallel stylesheet (`widget-render-theme-color`).
/// Background is fully transparent so the diagram sits on the editor surface;
/// nodes use the markdown code background + accent stroke, edges/labels the
/// body text token.
fn theme_mermaid_colors(theme: Option<&Theme>) -> MermaidColors {
    let rgba = |c: editor_core::decoration::Color| -> [u8; 4] { [c.r, c.g, c.b, c.a] };
    match theme {
        Some(t) => MermaidColors {
            background: [0, 0, 0, 0],
            // Opaque editor surface so labels/hollow markers read over edges.
            edge_label_bg: { let mut c = rgba(t.palette.bg); c[3] = 255; c },
            node_fill: rgba(t.markdown.code_bg),
            node_stroke: rgba(t.palette.accent),
            edge_stroke: rgba(t.palette.fg),
            text_color: rgba(t.palette.fg),
        },
        None => {
            let fg = rgba(editor_md::equations::COLOR_MATH_FG);
            MermaidColors {
                background: [0, 0, 0, 0],
                edge_label_bg: rgba(editor_md::diagrams::COLOR_MERMAID_BG),
                node_fill: rgba(editor_md::diagrams::COLOR_MERMAID_BG),
                node_stroke: fg,
                edge_stroke: fg,
                text_color: fg,
            }
        }
    }
}

/// Derive the WaveDrom draw colors from the active theme: foreground (lines +
/// labels) from the body text token, background fully transparent so the
/// waveform sits on the editor surface (`widget-render-theme-color`). The
/// categorical series palette stays WaveDrom's default skin (theme-neutral).
fn theme_wavedrom_colors(theme: Option<&Theme>) -> WaveDromColors {
    let rgba = |c: editor_core::decoration::Color| -> [u8; 4] { [c.r, c.g, c.b, c.a] };
    match theme {
        Some(t) => WaveDromColors {
            foreground: rgba(t.palette.fg),
            background: [0, 0, 0, 0],
        },
        None => WaveDromColors {
            foreground: {
                let c = editor_md::equations::COLOR_MATH_FG;
                [c.r, c.g, c.b, c.a]
            },
            background: [0, 0, 0, 0],
        },
    }
}

/// The active theme's text token as straight RGBA (`widget-render-theme-color`).
fn theme_fg(theme: Option<&Theme>) -> [u8; 4] {
    let c = theme
        .map(|t| t.palette.fg)
        .unwrap_or(editor_md::equations::COLOR_MATH_FG);
    [c.r, c.g, c.b, c.a]
}

/// Which widget kind the active edit-preview span renders as. Carries enough to
/// drive the render path (`render_math` / `render_mermaid`) in [`edit_preview`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviewKind {
    /// Inline `$…$` — rendered display-style in the popup (more legible than the
    /// compact inline form when previewing).
    InlineMath,
    /// Display `$$…$$`.
    DisplayMath,
    /// A ```` ```mermaid ```` fence.
    Mermaid,
    /// A ```` ```wavedrom ```` fence.
    WaveDrom,
}

/// The single revealed span the main cursor is currently editing — the one the
/// floating edit-preview overlay renders. `kind` selects the render path;
/// `inner_range` is the source the renderer consumes; `anchor_line` is the
/// span's last source line (the overlay floats just below it).
/// status: widget-edit-popup-preview
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivePreviewSpan {
    pub kind: PreviewKind,
    pub inner_range: std::ops::Range<usize>,
    pub anchor_line: usize,
}

/// Number of lines above/below the caret to scan for the active preview span.
/// Bounds the per-frame scan cost (the popup decision can't see a span whose
/// open delimiter is farther than this from the caret) while being comfortably
/// larger than any realistic math / mermaid block — so a span the caret sits in
/// is always found regardless of where the viewport has scrolled to.
const PREVIEW_SCAN_MARGIN_LINES: usize = 400;

/// Pick the revealed math / mermaid span the main cursor is editing, or `None`
/// when the cursor is in no widget source span (→ the overlay paints nothing).
///
/// Reuses the exact reveal predicates the decoration providers apply — inline
/// math per-line ([`line_active`]) and display math / mermaid per-span
/// ([`cursor_inside`]), both OR-ed with [`selection_overlaps`] — so a span is
/// "revealed" here iff the in-place widget was suppressed there. One popup at a
/// time: the span whose `byte_range` contains the main caret wins; failing a
/// containment (inline reveal is per *line*, so the caret may sit beside the
/// `$…$` rather than within it), the span on the caret's line nearest the caret
/// by byte distance wins.
///
/// The scan is scoped to a window *around the caret* (not the editor viewport)
/// so the reveal is a pure function of the caret/selection: revealing a span
/// un-hides its source lines and can shift layout/scroll a few px, which would
/// flip a viewport-scoped scan between finding the span and not — making the
/// popup flicker. Anchoring the scan to the caret keeps it steady while the
/// caret is parked in a span. The `viewport` argument is no longer consulted
/// (kept for call-site symmetry with the providers).
/// status: widget-edit-popup-preview, widget-block-click-to-edit
pub fn active_preview_span(
    state: &EditorState,
    viewport: Option<&std::ops::Range<usize>>,
) -> Option<ActivePreviewSpan> {
    let _ = viewport; // reveal is caret-driven, not viewport-scoped (anti-flicker).
    let cursor = state.selection.main().head.offset();
    let total_lines = state.doc.len_lines();
    // Caret-anchored scan window: the span the caret reveals is always within
    // `PREVIEW_SCAN_MARGIN_LINES` of the caret line, independent of scroll.
    let scan = caret_scan_window(state, cursor);
    let scan = Some(&scan);
    let anchor_line = |byte_range: &std::ops::Range<usize>| {
        state
            .doc
            .byte_to_line(byte_range.end.saturating_sub(1))
            .min(total_lines.saturating_sub(1))
    };

    // Gather every revealed candidate (math + mermaid), tagged with whether the
    // caret is inside its byte range and its byte distance to the caret.
    let mut best: Option<(bool, usize, ActivePreviewSpan)> = None;
    let mut consider = |contains: bool, dist: usize, cand: ActivePreviewSpan| {
        // Prefer a containing span; among equals, the nearest by byte distance.
        let better = match &best {
            None => true,
            Some((c, d, _)) => contains.cmp(c).then((*d).cmp(&dist)).is_lt(),
        };
        if better {
            best = Some((contains, dist, cand));
        }
    };
    let dist_to = |range: &std::ops::Range<usize>| -> usize {
        if cursor < range.start {
            range.start - cursor
        } else if cursor > range.end {
            cursor - range.end
        } else {
            0
        }
    };

    for span in math_spans(state, scan) {
        let revealed = match span.kind {
            SpanKind::Inline => line_active(state, &span.byte_range),
            SpanKind::Display => cursor_inside(state, &span.byte_range),
        } || selection_overlaps(state, &span.byte_range);
        if !revealed {
            continue;
        }
        let kind = match span.kind {
            SpanKind::Inline => PreviewKind::InlineMath,
            SpanKind::Display => PreviewKind::DisplayMath,
        };
        let contains = cursor >= span.byte_range.start && cursor <= span.byte_range.end;
        consider(
            contains,
            dist_to(&span.byte_range),
            ActivePreviewSpan {
                kind,
                inner_range: span.inner_range.clone(),
                anchor_line: anchor_line(&span.byte_range),
            },
        );
    }
    for span in mermaid_spans(state, scan) {
        let revealed = cursor_inside(state, &span.byte_range)
            || selection_overlaps(state, &span.byte_range);
        if !revealed {
            continue;
        }
        let contains = cursor >= span.byte_range.start && cursor <= span.byte_range.end;
        consider(
            contains,
            dist_to(&span.byte_range),
            ActivePreviewSpan {
                kind: PreviewKind::Mermaid,
                inner_range: span.inner_range.clone(),
                anchor_line: anchor_line(&span.byte_range),
            },
        );
    }
    for span in wavedrom_spans(state, scan) {
        let revealed = cursor_inside(state, &span.byte_range)
            || selection_overlaps(state, &span.byte_range);
        if !revealed {
            continue;
        }
        let contains = cursor >= span.byte_range.start && cursor <= span.byte_range.end;
        consider(
            contains,
            dist_to(&span.byte_range),
            ActivePreviewSpan {
                kind: PreviewKind::WaveDrom,
                inner_range: span.inner_range.clone(),
                anchor_line: anchor_line(&span.byte_range),
            },
        );
    }
    best.map(|(_, _, cand)| cand)
}

/// Byte range of the line window `±PREVIEW_SCAN_MARGIN_LINES` around the caret,
/// clamped to the document. The span-scan in [`active_preview_span`] is scoped
/// to this (rather than the scroll viewport) so the popup's reveal decision is
/// stable as the buffer scrolls. status: widget-edit-popup-preview
fn caret_scan_window(state: &EditorState, cursor: usize) -> std::ops::Range<usize> {
    let total_lines = state.doc.len_lines();
    let doc_len = state.doc.len_bytes();
    if total_lines == 0 {
        return 0..doc_len;
    }
    let caret_line = state.doc.byte_to_line(cursor.min(doc_len));
    let start_line = caret_line.saturating_sub(PREVIEW_SCAN_MARGIN_LINES);
    let end_line = caret_line
        .saturating_add(PREVIEW_SCAN_MARGIN_LINES)
        .min(total_lines.saturating_sub(1));
    let start = state.doc.line_to_byte(start_line);
    let end = if end_line + 1 < total_lines {
        state.doc.line_to_byte(end_line + 1)
    } else {
        doc_len
    };
    start..end
}

