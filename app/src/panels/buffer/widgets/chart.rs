//! Inline ```` ```chart ```` block widget: the `hiker-charts` render + decoration
//! provider, mirroring the mermaid path in the parent [`super`] module.
//!
//! Detection lives in `editor_md::diagrams::chart_spans`; this module owns the
//! render (parse → resolve → plotters-SVG → RGBA, via [`super::render`]) and the
//! `BlockWidget` / open-in-builder wiring, so `editor-md` stays renderer-free.
//! A block's external `data:` reference resolves through the host's Vault-backed
//! [`crate::charts::VaultDataResolver`]. status: widget-chart-render

use std::collections::HashMap;
use std::sync::Arc;

use editor_core::decoration::{BlockWidget, Decoration, Set as DecorationSet, WidgetPixels};
use editor_core::rangeset::RangeSet;
use editor_core::state::Editor as EditorState;
use editor_core::theme::Theme;
use editor_md::diagrams::chart_spans;

use super::disk_cache::DiagramCacheCtx;
use super::render::{RenderedWidget, chart_widget_id, render_chart};
use super::{cursor_inside, emit_block_widget, fit_block_height, selection_overlaps};

/// The canvas size inline ```` ```chart ```` widgets render at, in chart pixels.
/// A wide chart is letterboxed down to the editor column by [`fit_block_height`]
/// at paint time, so this is the rendered aspect, not a hard pixel budget.
const INLINE_CHART_SIZE: hiker_charts_core::backend::Size =
    hiker_charts_core::backend::Size { width: 720, height: 420 };

/// A ```` ```chart ```` widget. Full-width own-height block: a body click opens
/// the block in the chart builder (`chart-open-in-builder`) rather than revealing
/// the YAML. `widget_id` is the data-free [`chart_widget_id`] (the texture-cache
/// + edit-target key); the rendered pixels carry the full data-inclusive render
/// hash. status: widget-chart-render
struct ChartWidget {
    rendered: RenderedWidget,
    dpr: f32,
    widget_id: u64,
}

impl BlockWidget for ChartWidget {
    fn measure(&self, _font_size: f32, width: f32) -> f32 {
        fit_block_height(&self.rendered, self.dpr, width)
    }

    fn widget_id(&self) -> u64 {
        self.widget_id
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
}

/// Build the chart-widget decoration layer for the current editor state.
///
/// Mirrors `super::mermaid_widget_decorations`: renders each on-screen ```` ```chart
/// ```` fence (viewport-scoped) via parse → resolve → plotters-SVG → RGBA,
/// applies reveal-on-cursor, and otherwise hides the fence lines + emits a
/// `BlockWidget`. A fence whose data references an external `data:` file is
/// resolved through `resolver` (the Vault-backed [`crate::charts::VaultDataResolver`]),
/// which honors the vault sandbox + the same link semantics as wikilinks; an
/// inline-CSV (`---`) block needs no resolver. A fence that fails to render emits
/// nothing — the tinted-source `chart_decorations` mark is the fallback
/// (`widget-render-error-fallback`). `resolver` is `None` in read-only / embedded
/// hosts (canvas cards, hover previews): inline charts still render there;
/// external-data charts fall back to source. status: widget-chart-render
pub fn widget_decorations(
    state: &EditorState,
    theme: Option<&Theme>,
    viewport: Option<&std::ops::Range<usize>>,
    dpr: f32,
    cache: Option<&DiagramCacheCtx>,
    resolver: Option<&crate::charts::VaultDataResolver>,
) -> DecorationSet {
    let chart_theme = chart_theme_for(theme);
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
    for span in chart_spans(state, viewport) {
        let revealed = cursor_inside(state, &span.byte_range)
            || selection_overlaps(state, &span.byte_range);
        if revealed {
            continue;
        }
        let inner = &doc_text[span.inner_range.clone()];
        let data_csv = external_chart_csv(inner, resolver);
        match render_chart(inner, data_csv.as_deref(), &chart_theme, INLINE_CHART_SIZE, dpr, cache) {
            Ok(rendered) => {
                let widget_id = chart_widget_id(inner, &chart_theme, INLINE_CHART_SIZE, dpr);
                emit_block_widget(
                    state,
                    &span.byte_range,
                    Arc::new(ChartWidget { rendered, dpr, widget_id }),
                    total_lines,
                    &line_byte_end,
                    &mut entries,
                );
            }
            Err(e) => {
                // Malformed config / unresolved data / resolve diagnostic: emit
                // nothing so the tinted source stays visible for the user to fix.
                tracing::debug!(target: "hiker::chart", error = %e, "chart render failed; showing source");
            }
        }
    }
    RangeSet::from_iter(entries)
}

/// The chart theme for the active editor theme, or `hiker-charts`'s light preset
/// when the host supplied none (read-only previews). status: widget-chart-render
fn chart_theme_for(theme: Option<&Theme>) -> hiker_charts_core::theme::Theme {
    theme
        .map(crate::charts::hiker_to_chart_theme)
        .unwrap_or_else(hiker_charts_core::theme::Theme::light)
}

/// The external CSV text a chart block needs, or `None` when the block is
/// self-contained (carries an inline `---` data section) or references no `data:`
/// file. Cheap: a `---` split decides inline-vs-external without a full parse;
/// only an external block parses the config to read `spec.data` and resolves it.
fn external_chart_csv(
    inner: &str,
    resolver: Option<&crate::charts::VaultDataResolver>,
) -> Option<String> {
    // An inline `---` data section supplies the table; no resolver needed.
    if hiker_charts_core::block::split_block(inner).1.is_some() {
        return None;
    }
    let resolver = resolver?;
    let spec = hiker_charts_core::dsl::ChartSpec::from_yaml(inner).ok()?;
    resolver.resolve_text(spec.data.as_deref()?)
}

/// Where a clicked inline ```` ```chart ```` widget resolves to: the fence's
/// inner byte range and its body text, so the click handler can open the block
/// in the builder ([`crate::panels::charts_tab::open_block`]) and re-locate it
/// for save-back. status: chart-open-in-builder
#[derive(Clone, Debug)]
pub struct EditTarget {
    pub inner_range: std::ops::Range<usize>,
    pub inner: String,
}

/// Per-buffer map from a rendered chart widget's [`chart_widget_id`] to its
/// open-in-builder target. status: chart-open-in-builder
pub type EditTargets = HashMap<u64, EditTarget>;

/// Build this frame's chart open-in-builder target map: for every on-screen
/// ```` ```chart ```` fence, map its data-free [`chart_widget_id`] (the id a
/// body click on the rendered chart carries) to the block's inner range + body.
/// Raster-free — no parse, no data resolution, no resvg blit — so it's cheap to
/// recompute each frame. `viewport` scopes the scan like the widget provider.
/// status: chart-open-in-builder
pub fn edit_targets(
    state: &EditorState,
    theme: Option<&Theme>,
    viewport: Option<&std::ops::Range<usize>>,
    dpr: f32,
) -> EditTargets {
    let chart_theme = chart_theme_for(theme);
    let doc = state.doc.to_string();
    let mut targets = EditTargets::new();
    for span in chart_spans(state, viewport) {
        let inner = doc[span.inner_range.clone()].to_string();
        let id = chart_widget_id(&inner, &chart_theme, INLINE_CHART_SIZE, dpr);
        targets.insert(id, EditTarget { inner_range: span.inner_range.clone(), inner });
    }
    targets
}

#[cfg(test)]
mod tests {
    use editor_core::decoration::{BlockSide, Decoration, LineStyle, Set as DecorationSet};
    use editor_core::selection::Selection;
    use editor_core::state::Editor as EditorState;

    use super::{edit_targets, widget_decorations};
    use crate::panels::buffer::widgets::{
        DiagramRegionRegistry, WidgetClickBucket, classify_widget_click,
    };

    const DPR: f32 = 1.0;

    /// (above, below) block-widget counts for a decoration set.
    fn block_sides(set: &DecorationSet) -> (usize, usize) {
        let mut above = 0;
        let mut below = 0;
        for (_, d) in set.iter_all() {
            if let Decoration::BlockWidget { side, .. } = d {
                match side {
                    BlockSide::Above => above += 1,
                    BlockSide::Below => below += 1,
                }
            }
        }
        (above, below)
    }

    #[test]
    fn inline_chart_block_renders_to_block_widget() {
        // status: widget-chart-render — a self-contained ```chart block (config
        // + `---` + inline CSV) renders end-to-end (parse → resolve → plotters
        // SVG → RGBA) to one Above block widget with the fence lines hidden.
        let src =
            "intro\n\n```chart\nmark: bar\nx: cat\ny: val\n---\ncat,val\na,1\nb,2\n```\n\nmore\n";
        let state = EditorState::new(src);
        let set = widget_decorations(&state, None, None, DPR, None, None);
        let mut block = 0;
        let mut hides = 0;
        for (_, d) in set.iter_all() {
            match d {
                Decoration::BlockWidget { .. } => block += 1,
                Decoration::Line(LineStyle { hide: true, .. }) => hides += 1,
                _ => {}
            }
        }
        assert_eq!(block, 1, "one chart block widget");
        assert!(hides >= 3, "all fence lines of the block are hidden");
        assert_eq!(block_sides(&set), (1, 0), "in-place render is an Above block");
    }

    #[test]
    fn chart_source_shown_when_cursor_inside() {
        // status: widget-chart-render — cursor inside the fence reveals the
        // source: no in-place block, no hides (the tint fallback shows it).
        let src = "intro\n\n```chart\nmark: bar\nx: cat\ny: val\n---\ncat,val\na,1\n```\n";
        let mut state = EditorState::new(src);
        let inside = src.find("mark: bar").unwrap();
        state.selection = Selection::single(inside);
        let set = widget_decorations(&state, None, None, DPR, None, None);
        assert_eq!(block_sides(&set), (0, 0), "no in-place block when revealed");
    }

    #[test]
    fn chart_left_click_reveals_source_right_click_resolves_block() {
        // status: chart-open-in-builder — a chart's widget id resolves through
        // `edit_targets` to the block's inner range + body (used by the
        // right-click "Open in chart editor" menu), AND a left click classifies
        // as Edit (caret into source) like the other block widgets — because the
        // id is also a `widget_edit_targets` entry.
        let src = "intro\n\n```chart\nmark: bar\nx: cat\ny: val\n---\ncat,val\na,1\n```\n";
        let state = EditorState::new(src);
        let set = widget_decorations(&state, None, None, DPR, None, None);
        let widget_id = set
            .iter_all()
            .find_map(|(_, d)| match d {
                Decoration::BlockWidget { widget, .. } => Some(widget.widget_id()),
                _ => None,
            })
            .expect("a chart block widget");

        // Right-click target: the block's inner range + body.
        let targets = edit_targets(&state, None, None, DPR);
        let target = targets.get(&widget_id).expect("right-click resolves to a chart target");
        assert!(src[target.inner_range.clone()].starts_with("mark: bar"));

        // Left-click: the id is in `widget_edit_targets`, so it classifies as Edit
        // (place caret at `inner_range.start` → reveal source).
        let edit_targets =
            crate::panels::buffer::widgets::widget_edit_targets(&state, None, None, 15.0, DPR);
        assert_eq!(edit_targets.get(&widget_id), Some(&target.inner_range.start));
        assert_eq!(
            classify_widget_click(widget_id, &DiagramRegionRegistry::new(), &edit_targets),
            WidgetClickBucket::Edit,
        );
    }

    #[test]
    fn broken_chart_emits_nothing() {
        // A malformed config → render Err → no widget; the tinted source stays.
        let src = "intro\n\n```chart\nmark: : :\n---\na,b\n1,2\n```\n";
        let state = EditorState::new(src);
        let set = widget_decorations(&state, None, None, DPR, None, None);
        assert_eq!(block_sides(&set), (0, 0), "an unparseable chart emits no widget");
    }
}
