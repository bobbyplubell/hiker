//! Diagram-diagnostics decoration layer: squiggle (underline + gutter marker)
//! decorations under the source of any ```` ```mermaid ```` / ```` ```wavedrom ````
//! fence whose body fails the shared `hiker-diagram` `check()` seam. Split out of
//! the widgets module so the render-widget code and the diagnostics code stay
//! separable. status: diagram-editor-diagnostics

use editor_core::decoration::Set as DecorationSet;
use editor_core::rangeset::RangeSet;
use editor_core::state::Editor as EditorState;
use editor_core::theme::Theme;
use editor_md::diagrams::{mermaid_spans, wavedrom_spans};

/// Build the diagram-diagnostics decoration layer for the current editor state:
/// a squiggle (severity-colored underline + gutter marker) under the source of
/// any ```` ```mermaid ```` / ```` ```wavedrom ```` fence whose body fails the
/// shared `hiker-diagram` `check()` seam. status: diagram-editor-diagnostics
///
/// Unlike the render-widget layers this does NOT depend on the `Render widgets`
/// toggle — a malformed block is exactly the one that fails to render, so its
/// squiggle must show even when the in-place render is off. For each fence the
/// inner source is checked via [`hiker_core::diagrams::check_diagram`]; every
/// returned [`hiker_diagram::Diagnostic`] is mapped into an
/// [`editor_core::decoration::Diagnostic`] whose range is the engine's local
/// span shifted into document byte coords (or the whole inner block when the
/// engine couldn't localize it), then handed to
/// [`editor_view::diagnostics::diagnostic_decorations`] for the underline +
/// gutter marks.
pub fn diagram_diagnostic_decorations(
    state: &EditorState,
    theme: Option<&Theme>,
    viewport: Option<&std::ops::Range<usize>>,
) -> DecorationSet {
    let doc_text = state.doc.to_string();
    let mut diags: Vec<editor_core::decoration::Diagnostic> = Vec::new();

    let mut collect = |lang: &'static str, byte_range: &std::ops::Range<usize>, inner: &std::ops::Range<usize>| {
        // An empty inner range (unterminated / empty fence) has nothing to
        // check; the render layer already falls back to the tinted source.
        if inner.start >= inner.end {
            return;
        }
        let src = &doc_text[inner.clone()];
        for d in hiker_core::diagrams::check_diagram(lang, src) {
            let range = match d.span {
                // Engine-local span → shift into document byte coords, clamped to
                // the inner block so a stray span can't escape the fence body.
                Some(s) => {
                    let start = (inner.start + s.start).min(inner.end);
                    let end = (inner.start + s.end).min(inner.end).max(start);
                    start..end
                }
                // No span → underline the whole inner block. When that is empty
                // (shouldn't happen here) fall back to the fence span so the
                // squiggle is still visible.
                None if inner.start < inner.end => inner.clone(),
                None => byte_range.clone(),
            };
            diags.push(editor_core::decoration::Diagnostic {
                range,
                severity: map_severity(d.severity),
                message: d.message.into(),
                source: lang.into(),
                code: None,
            });
        }
    };

    for span in mermaid_spans(state, viewport) {
        collect("mermaid", &span.byte_range, &span.inner_range);
    }
    for span in wavedrom_spans(state, viewport) {
        collect("wavedrom", &span.byte_range, &span.inner_range);
    }

    if diags.is_empty() {
        return RangeSet::empty();
    }
    editor_view::diagnostics::diagnostic_decorations(&diags, &state.doc, theme)
}

/// Map a `hiker-diagram` severity onto the editor's decoration severity. The
/// editor additionally has `Hint`, which the diagram engines never produce.
const fn map_severity(s: hiker_diagram::Severity) -> editor_core::decoration::Severity {
    match s {
        hiker_diagram::Severity::Error => editor_core::decoration::Severity::Error,
        hiker_diagram::Severity::Warning => editor_core::decoration::Severity::Warning,
        hiker_diagram::Severity::Info => editor_core::decoration::Severity::Info,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use editor_core::decoration::Decoration;

    /// Count the underline `Mark` decorations (the squiggles) and the
    /// diagnostic gutter `Line` markers a diagnostic set carries.
    fn diagnostic_counts(set: &DecorationSet) -> (usize, usize) {
        let mut marks = 0;
        let mut gutters = 0;
        for (_, d) in set.iter_all() {
            match d {
                Decoration::Mark(m) if m.underline => marks += 1,
                Decoration::Line(l) if l.gutter_marker.is_some() => gutters += 1,
                _ => {}
            }
        }
        (marks, gutters)
    }

    #[test]
    fn broken_mermaid_block_yields_diagnostic_over_inner_span() {
        // A malformed pie body (`: notanumber`) is a genuine mermaid syntax
        // error the check() seam surfaces; the squiggle layer underlines it.
        let src = "intro\n\n```mermaid\npie title\n: notanumber\n```\n";
        let state = EditorState::new(src);

        let set = diagram_diagnostic_decorations(&state, None, None);
        let (marks, gutters) = diagnostic_counts(&set);
        assert!(marks >= 1, "broken mermaid yields at least one squiggle mark");
        assert!(gutters >= 1, "and at least one gutter diagnostic marker");

        // Every underline mark sits within the fence body, never the prose
        // above it or the fence delimiters.
        let span = mermaid_spans(&state, None).into_iter().next().expect("one span");
        for (r, d) in set.iter_all() {
            if matches!(d, Decoration::Mark(m) if m.underline) {
                assert!(
                    r.start >= span.inner_range.start && r.end <= span.inner_range.end,
                    "mark {r:?} is inside inner {:?}",
                    span.inner_range
                );
            }
        }
    }

    #[test]
    fn valid_mermaid_block_yields_no_diagnostics() {
        let src = "intro\n\n```mermaid\ngraph TD\nA-->B\n```\n";
        let state = EditorState::new(src);
        let set = diagram_diagnostic_decorations(&state, None, None);
        assert_eq!(diagnostic_counts(&set), (0, 0), "valid mermaid: no squiggles");
    }

    #[test]
    fn broken_wavedrom_block_yields_diagnostic() {
        // Unterminated JSON5 object — a wavedrom syntax error.
        let src = "intro\n\n```wavedrom\n{signal:\n```\n";
        let state = EditorState::new(src);
        let set = diagram_diagnostic_decorations(&state, None, None);
        let (marks, _) = diagnostic_counts(&set);
        assert!(marks >= 1, "broken wavedrom yields a squiggle");
    }
}
