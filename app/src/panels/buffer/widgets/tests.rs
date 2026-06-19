//! Unit tests for the widget decoration providers in the parent module:
//! reveal-on-cursor for inline/display math, mermaid and wavedrom fences,
//! the diagram-region click registry, the click-to-edit target map and its
//! routing, and the block-height fit.
//!
//! Split out of `mod.rs` to keep that file under the length budget; the
//! parent includes it via `#[cfg(test)] mod tests;`.

use super::*;
use editor_core::decoration::{BlockSide, LineStyle};
use editor_core::selection::{SelRange, Selection};

const DPR: f32 = 1.0;
const FONT: f32 = 15.0;

fn deco_count(state: &EditorState) -> (usize, usize) {
    let set = math_widget_decorations(state, None, None, FONT, DPR, None);
    let mut inline = 0;
    let mut block = 0;
    for (_, d) in set.iter_all() {
        match d {
            Decoration::InlineWidget { .. } => inline += 1,
            Decoration::BlockWidget { .. } => block += 1,
            _ => {}
        }
    }
    (inline, block)
}

/// (above block widgets, below block widgets) for the math provider. The
/// in-place display-math render is an `Above` block; revealed spans emit no
/// block here (the live preview is the floating overlay, not a `Below`
/// block). status: widget-edit-popup-preview
fn math_block_sides(state: &EditorState) -> (usize, usize) {
    let set = math_widget_decorations(state, None, None, FONT, DPR, None);
    block_sides(&set)
}

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
fn inline_widget_emitted_when_cursor_elsewhere() {
    // Cursor at offset 0; the `$x^2$` is on a later line.
    let src = "para one\nhas $x^2$ math\n";
    let state = EditorState::new(src);
    let (inline, _) = deco_count(&state);
    assert_eq!(inline, 1, "inline math renders when cursor is off its line");
}

#[test]
fn inline_widget_suppressed_when_cursor_on_line() {
    let src = "para one\nhas $x^2$ math\n";
    let mut state = EditorState::new(src);
    // Move the cursor onto the math line.
    let pos = src.find("$x^2$").unwrap();
    state.selection = Selection::single(pos);
    let (inline, _) = deco_count(&state);
    assert_eq!(inline, 0, "inline math collapses to source on its line");
}

#[test]
fn inline_revealed_emits_no_in_place_widget() {
    // status: widget-edit-popup-preview — cursor on the inline span's line:
    // source stays inline (no inline widget) and this layer emits no block
    // either; the live render is the floating overlay (`edit_preview`), and
    // `active_preview_span` reports the span.
    let src = "para one\nhas $x^2$ math\n";
    let mut state = EditorState::new(src);
    let pos = src.find("$x^2$").unwrap();
    state.selection = Selection::single(pos);
    let (inline, _) = deco_count(&state);
    let (above, below) = math_block_sides(&state);
    assert_eq!(inline, 0, "revealed inline math keeps the source inline");
    assert_eq!((above, below), (0, 0), "no in-place / below block when revealed");
    let active = active_preview_span(&state, None).expect("a revealed span");
    assert_eq!(active.kind, PreviewKind::InlineMath);
}

#[test]
fn inline_not_revealed_no_preview() {
    // Cursor off the line: in-place inline widget, no preview block.
    let src = "para one\nhas $x^2$ math\n";
    let state = EditorState::new(src);
    let (inline, _) = deco_count(&state);
    let (above, below) = math_block_sides(&state);
    assert_eq!(inline, 1, "in-place inline widget when not revealed");
    assert_eq!((above, below), (0, 0), "no block widgets for inline math");
}

#[test]
fn display_widget_and_hide_emitted_when_cursor_elsewhere() {
    let src = "intro\n\n$$\n\\int_0^1 x\\,dx\n$$\n\nmore\n";
    let state = EditorState::new(src);
    let set = math_widget_decorations(&state, None, None, FONT, DPR, None);
    let mut block = 0;
    let mut hides = 0;
    for (_, d) in set.iter_all() {
        match d {
            Decoration::BlockWidget { .. } => block += 1,
            Decoration::Line(LineStyle { hide: true, .. }) => hides += 1,
            _ => {}
        }
    }
    assert_eq!(block, 1, "one display block widget");
    assert!(hides >= 3, "all source lines of the block are hidden");
    let (above, below) = block_sides(&set);
    assert_eq!((above, below), (1, 0), "in-place render is an Above block");
}

#[test]
fn display_widget_suppressed_when_cursor_inside() {
    // status: widget-edit-popup-preview — cursor inside the fence: source
    // lines stay visible (no hides) and this layer emits no block; the live
    // render is the floating overlay and `active_preview_span` reports it.
    let src = "intro\n\n$$\n\\int_0^1 x\\,dx\n$$\n\nmore\n";
    let mut state = EditorState::new(src);
    let inside = src.find("\\int").unwrap();
    state.selection = Selection::single(inside);
    let set = math_widget_decorations(&state, None, None, FONT, DPR, None);
    let mut hides = 0;
    for (_, d) in set.iter_all() {
        if let Decoration::Line(LineStyle { hide: true, .. }) = d {
            hides += 1;
        }
    }
    let (above, below) = block_sides(&set);
    assert_eq!(hides, 0, "revealed display math keeps its source lines");
    assert_eq!((above, below), (0, 0), "no in-place / below block when revealed");
    let active = active_preview_span(&state, None).expect("a revealed span");
    assert_eq!(active.kind, PreviewKind::DisplayMath);
}

#[test]
fn selection_overlap_reveals_inline() {
    let src = "has $x^2$ math\n";
    let mut state = EditorState::new(src);
    let start = src.find("$x^2$").unwrap();
    // A non-empty selection straddling the span.
    state.selection = Selection::from_range(SelRange::new(start - 1, start + 3));
    let (inline, _) = deco_count(&state);
    assert_eq!(inline, 0, "a selection overlapping the span reveals it");
}

/// (block widgets, hide lines) for the mermaid provider, mirroring
/// `deco_count`.
fn mermaid_counts(state: &EditorState) -> (usize, usize) {
    let set = mermaid_widget_decorations(state, None, None, FONT, DPR, None);
    let mut block = 0;
    let mut hides = 0;
    for (_, d) in set.iter_all() {
        match d {
            Decoration::BlockWidget { .. } => block += 1,
            Decoration::Line(LineStyle { hide: true, .. }) => hides += 1,
            _ => {}
        }
    }
    (block, hides)
}

#[test]
fn mermaid_block_emitted_when_cursor_elsewhere() {
    let src = "intro\n\n```mermaid\ngraph TD; A-->B\n```\n\nmore\n";
    let state = EditorState::new(src);
    let (block, hides) = mermaid_counts(&state);
    assert_eq!(block, 1, "one mermaid block widget when cursor is away");
    assert!(hides >= 3, "all fence lines of the block are hidden");
    let set = mermaid_widget_decorations(&state, None, None, FONT, DPR, None);
    let (above, below) = block_sides(&set);
    assert_eq!((above, below), (1, 0), "in-place render is an Above block");
}

#[test]
fn whole_doc_emits_diagram_below_viewport() {
    // Regression (`widget-render-cache`): the diagram widget layers must be
    // emitted whole-document so a pure scroll never has to rebuild them.
    // A mermaid block far below the top: with a viewport that excludes it
    // the old viewport-scoped path emitted nothing (forcing a rebuild when
    // it scrolled into view); with `None` (whole-doc, how the host now calls
    // it) it's always emitted, so the layer is stable across scroll.
    let mut src = String::new();
    for i in 0..200 {
        src.push_str(&format!("filler line {i}\n"));
    }
    src.push_str("\n```mermaid\ngraph TD; A-->B\n```\n");
    let state = EditorState::new(&src);

    // Viewport covering only the top of the doc (excludes the fence).
    let top_only = 0usize..50usize;
    let scoped = mermaid_widget_decorations(&state, None, Some(&top_only), FONT, DPR, None);
    assert_eq!(block_sides(&scoped), (0, 0), "viewport-scoped skips the off-screen fence");

    // Whole-doc (viewport=None): the fence is emitted regardless of scroll.
    let whole = mermaid_widget_decorations(&state, None, None, FONT, DPR, None);
    assert_eq!(block_sides(&whole), (1, 0), "whole-doc emits the off-screen fence");
}

#[test]
fn mermaid_revealed_emits_no_in_place_block() {
    // status: widget-edit-popup-preview — cursor inside the fence: this
    // layer emits no in-place block (the live render is the floating
    // overlay), never a panic — even for an unparseable body.
    let src = "intro\n\n```mermaid\nnot a real diagram type at all\n```\n";
    let mut state = EditorState::new(src);
    let inside = src.find("not a real").unwrap();
    state.selection = Selection::single(inside);
    let set = mermaid_widget_decorations(&state, None, None, FONT, DPR, None);
    let (above, below) = block_sides(&set);
    assert_eq!((above, below), (0, 0), "no in-place / below block when revealed");
}

#[test]
fn mermaid_source_shown_when_cursor_inside() {
    // status: widget-edit-popup-preview — cursor inside the fence: source
    // lines stay visible (no hides) and this layer emits no block; the live
    // render is the floating overlay and `active_preview_span` reports it.
    let src = "intro\n\n```mermaid\ngraph TD; A-->B\n```\n\nmore\n";
    let mut state = EditorState::new(src);
    let inside = src.find("graph TD").unwrap();
    state.selection = Selection::single(inside);
    let set = mermaid_widget_decorations(&state, None, None, FONT, DPR, None);
    let mut hides = 0;
    for (_, d) in set.iter_all() {
        if let Decoration::Line(LineStyle { hide: true, .. }) = d {
            hides += 1;
        }
    }
    let (above, below) = block_sides(&set);
    assert_eq!(hides, 0, "no hide lines when revealing the source");
    assert_eq!((above, below), (0, 0), "no in-place / below block when revealed");
    let active = active_preview_span(&state, None).expect("a revealed span");
    assert_eq!(active.kind, PreviewKind::Mermaid);
}

#[test]
fn unparseable_mermaid_emits_nothing() {
    // An unsupported / unparseable diagram body → render returns None →
    // no widget; the mermaid_decorations tint stays as the fallback.
    let src = "intro\n\n```mermaid\nnot a real diagram type at all\n```\n";
    let state = EditorState::new(src);
    let (block, hides) = mermaid_counts(&state);
    assert_eq!(block, 0, "an unparseable mermaid block emits no widget");
    assert_eq!(hides, 0, "and hides nothing, so the source stays visible");
}

#[test]
fn mermaid_click_region_and_registry_agree() {
    // status: widget-mermaid-links — a flowchart with a `click X "url"`
    // directive: the MermaidWidget emits a tagged click region for X, and
    // the per-buffer registry maps that same id → the link + tooltip.
    let src = "intro\n\n```mermaid\ngraph TD\n  A[Start]\n  click A \"https://example.com\" \"go\"\n```\n";
    let state = EditorState::new(src);

    let registry = mermaid_link_registry(&state, None, None, FONT, DPR);
    assert!(!registry.is_empty(), "registry has at least one linked region");
    let (&id, link) = registry
        .iter()
        .find(|(_, v)| v.link.as_deref() == Some("https://example.com"))
        .expect("registry maps the click link");
    assert_eq!(link.tooltip.as_deref(), Some("go"), "tooltip carried too");
    // The id carries the diagram-region tag, not the wikilink tag.
    assert_ne!(id & MERMAID_REGION_TAG, 0, "diagram-region tag set");
    assert_eq!(
        id & editor_md::links::WIKILINK_WIDGET_TAG,
        0,
        "not tagged as a wikilink"
    );

    // The decoration provider builds a MermaidWidget whose click_regions()
    // emits exactly that id for the linked node.
    let set = mermaid_widget_decorations(&state, None, None, FONT, DPR, None);
    let ids: Vec<u64> = set
        .iter_all()
        .filter_map(|(_, d)| match d {
            Decoration::BlockWidget { widget, .. } => Some(widget.click_regions(FONT, 400.0)),
            _ => None,
        })
        .flatten()
        .map(|r| r.id)
        .collect();
    assert!(
        ids.contains(&id),
        "the widget emits the registry's click id ({id:#x} not in {ids:?})"
    );
}

#[test]
fn edit_target_maps_mermaid_widget_id_into_its_span() {
    // status: widget-block-click-to-edit — a whole-widget (body) click on a
    // rendered mermaid block carries the widget's `content_hash`; the
    // edit-target map resolves it to an offset *inside* the fence span, so
    // placing the caret there reveals the source (`cursor_inside` true).
    let src = "intro\n\n```mermaid\ngraph TD; A-->B\n```\n\nmore\n";
    let state = EditorState::new(src);

    // The widget id the painter emits for a body click == the render hash.
    let set = mermaid_widget_decorations(&state, None, None, FONT, DPR, None);
    let widget_id = set
        .iter_all()
        .find_map(|(_, d)| match d {
            Decoration::BlockWidget { widget, .. } => Some(widget.widget_id()),
            _ => None,
        })
        .expect("a mermaid block widget");

    let targets = widget_edit_targets(&state, None, None, FONT, DPR);
    let &offset = targets.get(&widget_id).expect("body click resolves to a target");

    // The target sits inside the fence's byte range (so reveal fires).
    let span = mermaid_spans(&state, None)
        .into_iter()
        .next()
        .expect("one mermaid span");
    assert!(
        span.byte_range.contains(&offset),
        "target {offset} within byte range {:?}",
        span.byte_range
    );
    assert_eq!(offset, span.inner_range.start, "target is inner_range.start");

    // Placing the caret there reveals the span (cursor_inside is true).
    let mut revealed = state;
    revealed.selection = Selection::single(offset);
    assert!(
        cursor_inside(&revealed, &span.byte_range),
        "caret at the target reveals the span"
    );
}

#[test]
fn edit_target_maps_display_math_widget_id() {
    // status: widget-block-click-to-edit — display math is a block widget
    // too, so its body click routes through the same map.
    let src = "intro\n\n$$\n\\int_0^1 x\\,dx\n$$\n\nmore\n";
    let state = EditorState::new(src);
    let set = math_widget_decorations(&state, None, None, FONT, DPR, None);
    let widget_id = set
        .iter_all()
        .find_map(|(_, d)| match d {
            Decoration::BlockWidget { widget, .. } => Some(widget.widget_id()),
            _ => None,
        })
        .expect("a display-math block widget");
    let targets = widget_edit_targets(&state, None, None, FONT, DPR);
    let &offset = targets.get(&widget_id).expect("display-math body click resolves");
    let span = math_spans(&state, None)
        .into_iter()
        .find(|s| s.kind == SpanKind::Display)
        .expect("one display span");
    assert!(span.byte_range.contains(&offset), "target within the span");
}

#[test]
fn active_preview_span_steady_off_screen_caret() {
    // status: widget-edit-popup-preview (anti-flicker) — the reveal is
    // caret-driven, not viewport-scoped: a caret parked in a span is found
    // even when the passed viewport excludes the span entirely (the case
    // that made the popup blink as layout/scroll shifted the span a few px).
    let src = "intro\n\n```mermaid\ngraph TD; A-->B\n```\n\nmore\n";
    let mut state = EditorState::new(src);
    state.selection = Selection::single(src.find("graph TD").unwrap());
    // A viewport that does NOT include the fence's lines.
    let last = src.len();
    let bogus = last..last;
    let active = active_preview_span(&state, Some(&bogus)).expect("found despite viewport");
    assert_eq!(active.kind, PreviewKind::Mermaid);
}

#[test]
fn region_click_ids_are_collision_free_and_tagged() {
    // Distinct (hash, index) pairs map to distinct ids, all tagged and
    // clear of the wikilink tag bit.
    let a = region_click_id(0xDEAD_BEEF, 0);
    let b = region_click_id(0xDEAD_BEEF, 1);
    let c = region_click_id(0x1234_5678, 0);
    assert_ne!(a, b);
    assert_ne!(a, c);
    for id in [a, b, c] {
        assert_ne!(id & MERMAID_REGION_TAG, 0);
        assert_eq!(id & editor_md::links::WIKILINK_WIDGET_TAG, 0);
    }
}

#[test]
fn non_interactive_mermaid_registry_empty() {
    // A flowchart with no `click` directive surfaces no registry entries.
    let src = "```mermaid\ngraph TD; A-->B\n```\n";
    let state = EditorState::new(src);
    let registry = mermaid_link_registry(&state, None, None, FONT, DPR);
    assert!(registry.is_empty(), "no click directives → empty registry");
}

#[test]
fn body_click_hash_with_wikilink_bit_routes_to_edit_not_wikilink() {
    // Regression (widget-block-click-to-edit): a block widget's whole-widget
    // body-click id is a bare content hash. If it happens to set bit 62
    // (`WIKILINK_WIDGET_TAG`), the OLD bit-first routing stole the click into
    // the wikilink handler → "clicking the diagram does nothing". Membership
    // must win: an id present in `edit_targets` routes to Edit regardless of
    // its bits.
    let hash_with_wikilink_bit = 0x1234u64 | editor_md::links::WIKILINK_WIDGET_TAG;
    let registry = DiagramRegionRegistry::new();
    let mut edit_targets = WidgetEditTargets::new();
    edit_targets.insert(hash_with_wikilink_bit, 42);
    assert_eq!(
        classify_widget_click(hash_with_wikilink_bit, &registry, &edit_targets),
        WidgetClickBucket::Edit,
        "edit-target membership beats the coincidental wikilink bit",
    );
}

#[test]
fn genuine_wikilink_and_region_ids_route_correctly() {
    // A real wikilink pill id (tag | small index) is in neither map → Wikilink.
    // A region id present in the registry → Diagram. An unclaimed id → Other.
    let wikilink_id = editor_md::links::WIKILINK_WIDGET_TAG | 3;
    let region_id = region_click_id(0xABCD, 0);
    let mut registry = DiagramRegionRegistry::new();
    registry.insert(region_id, DiagramLink { link: Some("x".into()), tooltip: None });
    let edit_targets = WidgetEditTargets::new();
    assert_eq!(
        classify_widget_click(wikilink_id, &registry, &edit_targets),
        WidgetClickBucket::Wikilink,
    );
    assert_eq!(
        classify_widget_click(region_id, &registry, &edit_targets),
        WidgetClickBucket::Diagram,
    );
    assert_eq!(
        classify_widget_click(0x9999, &registry, &edit_targets),
        WidgetClickBucket::Other,
    );
}

#[test]
fn unrenderable_inline_emits_nothing() {
    // An unbalanced brace fails the math layout → no widget, source mark
    // remains the fallback.
    let src = "x \\frac{a is broken: $\\frac{$ done\n";
    // Use the second line to keep the cursor away.
    let src = format!("padding\n{src}");
    let state = EditorState::new(&src);
    let (inline, _) = deco_count(&state);
    assert_eq!(inline, 0, "a parse failure emits no widget");
}

#[test]
fn active_preview_span_inline_math_on_cursor_line() {
    // status: widget-edit-popup-preview — caret on the `$x^2$` line selects
    // that span (inline reveal is per-line; the caret need not be inside).
    let src = "para\nhas $x^2$ here\n";
    let mut state = EditorState::new(src);
    let pos = src.find("here").unwrap(); // on the line, beside the span
    state.selection = Selection::single(pos);
    let active = active_preview_span(&state, None).expect("inline span on the line");
    assert_eq!(active.kind, PreviewKind::InlineMath);
    assert_eq!(&src[active.inner_range.clone()], "x^2");
}

#[test]
fn active_preview_span_display_math() {
    let src = "intro\n\n$$\n\\int_0^1 x\\,dx\n$$\n\nmore\n";
    let mut state = EditorState::new(src);
    state.selection = Selection::single(src.find("\\int").unwrap());
    let active = active_preview_span(&state, None).expect("display span");
    assert_eq!(active.kind, PreviewKind::DisplayMath);
    assert!(src[active.inner_range.clone()].contains("\\int_0^1"));
}

#[test]
fn active_preview_span_mermaid_fence() {
    let src = "intro\n\n```mermaid\ngraph TD; A-->B\n```\n\nmore\n";
    let mut state = EditorState::new(src);
    state.selection = Selection::single(src.find("graph TD").unwrap());
    let active = active_preview_span(&state, None).expect("mermaid span");
    assert_eq!(active.kind, PreviewKind::Mermaid);
    assert!(src[active.inner_range.clone()].contains("graph TD"));
}

#[test]
fn active_preview_span_none_outside_any_span() {
    // Caret on a plain line, away from any widget span → no popup.
    let src = "plain line\n\nhas $x^2$ math\n\n```mermaid\ngraph TD; A-->B\n```\n";
    let mut state = EditorState::new(src);
    state.selection = Selection::single(src.find("plain line").unwrap());
    assert!(
        active_preview_span(&state, None).is_none(),
        "no revealed span → no preview"
    );
}

#[test]
fn active_preview_span_one_at_a_time_nearest_to_caret() {
    // Two display blocks; caret inside the second. Exactly one span is
    // returned, and it's the one containing the caret.
    let src = "$$\na\n$$\n\n$$\nb_2\n$$\n";
    let mut state = EditorState::new(src);
    let inside_second = src.rfind("b_2").unwrap();
    state.selection = Selection::single(inside_second);
    let active = active_preview_span(&state, None).expect("the second display span");
    assert_eq!(active.kind, PreviewKind::DisplayMath);
    assert_eq!(&src[active.inner_range.clone()], "\nb_2\n");
}

#[test]
fn fit_block_height_scales_wide_down_only() {
    // dpr 1 → natural 200x100. Into a 100-wide column: scale 0.5 → height 50.
    let wide = RenderedWidget {
        rgba: vec![0; 200 * 100 * 4],
        width: 200,
        height: 100,
        baseline: None,
        content_hash: 0,
    };
    assert!((fit_block_height(&wide, 1.0, 100.0) - 50.0).abs() < 1e-3, "wide scales to fit");
    // Narrow (natural 200 < 400 column) keeps natural height (no upscale).
    assert!((fit_block_height(&wide, 1.0, 400.0) - 100.0).abs() < 1e-3, "narrow unchanged");
    // Zero / nonsense width falls back to natural height.
    assert!((fit_block_height(&wide, 1.0, 0.0) - 100.0).abs() < 1e-3, "zero width → natural");
}

/// (block widgets, hide lines) for the wavedrom provider, mirroring
/// `mermaid_counts`.
fn wavedrom_counts(state: &EditorState) -> (usize, usize) {
    let set = wavedrom_widget_decorations(state, None, None, FONT, DPR, None);
    let mut block = 0;
    let mut hides = 0;
    for (_, d) in set.iter_all() {
        match d {
            Decoration::BlockWidget { .. } => block += 1,
            Decoration::Line(LineStyle { hide: true, .. }) => hides += 1,
            _ => {}
        }
    }
    (block, hides)
}

const WAVEDROM_SRC: &str =
    "intro\n\n```wavedrom\n{ signal: [{ name: 'clk', wave: 'p...' }] }\n```\n\nmore\n";

#[test]
fn wavedrom_block_emitted_when_cursor_elsewhere() {
    // status: widget-wavedrom-render — cursor away → render in place
    // (hide source + Above block), mirroring mermaid.
    let state = EditorState::new(WAVEDROM_SRC);
    let (block, hides) = wavedrom_counts(&state);
    assert_eq!(block, 1, "one wavedrom block widget when cursor is away");
    assert!(hides >= 3, "all fence lines of the block are hidden");
    let set = wavedrom_widget_decorations(&state, None, None, FONT, DPR, None);
    let (above, below) = block_sides(&set);
    assert_eq!((above, below), (1, 0), "in-place render is an Above block");
}

#[test]
fn wavedrom_source_shown_when_cursor_inside() {
    // Cursor inside the fence → source stays visible (no hides, no block);
    // the live render is the floating overlay and `active_preview_span`
    // reports WaveDrom.
    let mut state = EditorState::new(WAVEDROM_SRC);
    state.selection = Selection::single(WAVEDROM_SRC.find("signal").unwrap());
    let (block, hides) = wavedrom_counts(&state);
    assert_eq!((block, hides), (0, 0), "no in-place block / hides when revealed");
    let active = active_preview_span(&state, None).expect("a revealed span");
    assert_eq!(active.kind, PreviewKind::WaveDrom);
}

#[test]
fn wavedrom_revealed_by_selection_overlap() {
    let mut state = EditorState::new(WAVEDROM_SRC);
    let start = WAVEDROM_SRC.find("```wavedrom").unwrap();
    let end = WAVEDROM_SRC.find("more").unwrap();
    state.selection = Selection::from_range(SelRange::new(start, end));
    let (block, hides) = wavedrom_counts(&state);
    assert_eq!((block, hides), (0, 0), "a selection overlap reveals the source");
}

#[test]
fn unparseable_wavedrom_emits_nothing() {
    // Non-WaveJSON body → render returns None → no widget; the
    // wavedrom_decorations tint stays as the fallback.
    let src = "intro\n\n```wavedrom\nnot wavejson at all\n```\n";
    let state = EditorState::new(src);
    let (block, hides) = wavedrom_counts(&state);
    assert_eq!((block, hides), (0, 0), "unparseable wavedrom emits no widget");
}

#[test]
fn edit_target_maps_wavedrom_widget_id_into_its_span() {
    // status: widget-block-click-to-edit — a body click on a rendered
    // wavedrom block resolves (via its content_hash) to an offset inside the
    // fence span, so the caret lands there and reveals the source.
    let state = EditorState::new(WAVEDROM_SRC);
    let set = wavedrom_widget_decorations(&state, None, None, FONT, DPR, None);
    let widget_id = set
        .iter_all()
        .find_map(|(_, d)| match d {
            Decoration::BlockWidget { widget, .. } => Some(widget.widget_id()),
            _ => None,
        })
        .expect("a wavedrom block widget");
    let targets = widget_edit_targets(&state, None, None, FONT, DPR);
    let &offset = targets.get(&widget_id).expect("body click resolves to a target");
    let span = wavedrom_spans(&state, None).into_iter().next().expect("one span");
    assert_eq!(offset, span.inner_range.start, "target is inner_range.start");
    assert!(span.byte_range.contains(&offset), "target inside the fence span");
    // A wavedrom body click routes to the Edit bucket (no registry entry).
    assert_eq!(
        classify_widget_click(widget_id, &DiagramRegionRegistry::new(), &targets),
        WidgetClickBucket::Edit,
    );
}
