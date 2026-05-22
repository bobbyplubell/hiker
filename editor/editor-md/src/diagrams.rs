//! Mermaid code-block decorations.
//!
//! Detects fenced code blocks tagged ```` ```mermaid ```` and applies a tinted
//! per-line background across the entire block (including the fence lines).
//!
//! Actual diagram rendering is deferred — it would require a mermaid renderer
//! or a host-provided `BlockWidget`. This module only marks the region so
//! users see where their mermaid blocks live.

use editor_core::decoration::Color;

use editor_core::decoration::Decoration;

use editor_core::decoration::Set as DecorationSet;
use editor_core::state::Editor as EditorState;
use editor_core::decoration::LineStyle;

use editor_core::rangeset::RangeSet;

use editor_core::theme::Theme;
pub const COLOR_MERMAID_BG: Color = Color::rgba(120, 200, 180, 28);

pub fn mermaid_decorations(
    state: &EditorState,
    theme: Option<&Theme>,
    viewport: Option<&std::ops::Range<usize>>,
) -> DecorationSet {
    let bg = theme.map(|t| t.markdown.code_bg).unwrap_or(COLOR_MERMAID_BG);
    let text = state.doc.to_string();
    let mut entries: Vec<(std::ops::Range<usize>, Decoration)> = Vec::new();

    let total_lines = state.doc.len_lines();
    let doc_len = state.doc.len_bytes();
    let line_byte_end = |line: usize| -> usize {
        if line + 1 < total_lines {
            state.doc.line_to_byte(line + 1)
        } else {
            doc_len
        }
    };
    let line_str = |line: usize| -> &str {
        let s = state.doc.line_to_byte(line);
        let e = line_byte_end(line);
        let raw = &text[s..e];
        raw.strip_suffix('\n').unwrap_or(raw)
    };

    let line_range = match viewport {
        Some(vp) => editor_view::viewport_lines(&state.doc, vp),
        None => 0..total_lines,
    };
    let scan_end_line = line_range.end;
    let mut line = line_range.start;
    // Two trivial line-shape predicates used only here; inline so we don't
    // pay the `single_call_fn` lint for one-shot helpers.
    let is_mermaid_fence_open = |line_text: &str| -> bool {
        let trimmed = line_text.trim_start();
        let Some(rest) = trimmed.strip_prefix("```") else {
            return false;
        };
        rest.trim().eq_ignore_ascii_case("mermaid")
    };
    let is_fence_close = |line_text: &str| -> bool {
        let trimmed = line_text.trim_start();
        let Some(rest) = trimmed.strip_prefix("```") else {
            return false;
        };
        rest.trim().is_empty()
    };
    while line < scan_end_line {
        if is_mermaid_fence_open(line_str(line)) {
            let open_line = line;
            let mut close_line = open_line;
            let mut found_close = false;
            let mut probe = open_line + 1;
            while probe < total_lines {
                if is_fence_close(line_str(probe)) {
                    close_line = probe;
                    found_close = true;
                    break;
                }
                probe += 1;
            }
            if !found_close {
                close_line = total_lines.saturating_sub(1);
            }
            for l in open_line..=close_line {
                let s = state.doc.line_to_byte(l);
                let e = line_byte_end(l);
                entries.push((
                    s..e,
                    Decoration::Line(LineStyle {
                        bg: Some(bg),
                        ..LineStyle::default()
                    }),
                ));
            }
            line = close_line + 1;
            continue;
        }
        line += 1;
    }

    RangeSet::from_iter(entries)
}

