//! Decoration provider that turns a slice of [`Diagnostic`]s into a
//! [`DecorationSet`]: severity-colored underline `Mark`s over each diagnostic
//! range plus a per-line `gutter_marker`. See SPEC §9.7, IMPLEMENTATION §16.5.1.

use editor_core::decoration::Color;

use editor_core::decoration::Decoration;

use editor_core::decoration::Set as DecorationSet;
use editor_core::decoration::Diagnostic;

use editor_core::decoration::GutterMarker;

use editor_core::decoration::LineStyle;

use editor_core::decoration::MarkStyle;

use editor_core::rangeset::RangeSet;

use editor_core::rope::Rope;

use editor_core::decoration::Severity;

use editor_core::theme::Theme;
/// VSCode-ish severity colors.
pub const ERROR_COLOR: Color = Color::rgba(244, 71, 71, 255);
pub const WARNING_COLOR: Color = Color::rgba(228, 188, 48, 255);
pub const INFO_COLOR: Color = Color::rgba(75, 156, 211, 255);
pub const HINT_COLOR: Color = Color::rgba(160, 160, 160, 255);

/// Build a decoration set for the given diagnostics:
///   * a `Mark` with `underline: true` colored by severity over each
///     diagnostic's byte range,
///   * a `Line` decoration carrying `GutterMarker::Diagnostic(severity)` on
///     every line that the diagnostic overlaps.
///
/// When multiple diagnostics touch the same line, the highest-severity marker
/// (Error > Warning > Info > Hint) wins.
pub fn diagnostic_decorations(
    diags: &[Diagnostic],
    doc: &Rope,
    theme: Option<&Theme>,
) -> DecorationSet {
    let total_bytes = doc.len_bytes();
    let mut entries: Vec<(std::ops::Range<usize>, Decoration)> = Vec::new();
    // line index -> (severity rank, severity)
    let mut line_markers: std::collections::BTreeMap<usize, Severity> =
        std::collections::BTreeMap::new();

    for d in diags {
        let start = d.range.start.min(total_bytes);
        let end = d.range.end.min(total_bytes).max(start);
        let color = match theme {
            None => match d.severity {
                Severity::Error => ERROR_COLOR,
                Severity::Warning => WARNING_COLOR,
                Severity::Info => INFO_COLOR,
                Severity::Hint => HINT_COLOR,
            },
            Some(t) => match d.severity {
                Severity::Error => t.diagnostics.error,
                Severity::Warning => t.diagnostics.warning,
                Severity::Info => t.diagnostics.info,
                Severity::Hint => t.diagnostics.hint,
            },
        };

        // Underline mark over the diagnostic range.
        if end > start {
            entries.push((
                start..end,
                Decoration::Mark(MarkStyle {
                    underline: true,
                    fg: Some(color),
                    ..MarkStyle::default()
                }),
            ));
        }

        // Gutter markers on every line the diagnostic touches.
        let first_line = doc.byte_to_line(start);
        // `end` is exclusive; clamp to a valid byte for line lookup.
        let last_byte = if end > start { end - 1 } else { start };
        let last_line = doc.byte_to_line(last_byte.min(total_bytes.saturating_sub(1)));
        for line in first_line..=last_line {
            line_markers
                .entry(line)
                .and_modify(|existing| {
                    if severity_rank(d.severity) > severity_rank(*existing) {
                        *existing = d.severity;
                    }
                })
                .or_insert(d.severity);
        }
    }

    let total_lines = doc.len_lines();
    for (line, sev) in line_markers {
        if line >= total_lines {
            continue;
        }
        let line_start = doc.line_to_byte(line);
        let line_end = if line + 1 < total_lines {
            doc.line_to_byte(line + 1)
        } else {
            total_bytes
        };
        let range = if line_start == line_end {
            line_start..line_start + 1
        } else {
            line_start..line_end
        };
        entries.push((
            range,
            Decoration::Line(LineStyle {
                gutter_marker: Some(GutterMarker::Diagnostic(sev)),
                ..LineStyle::default()
            }),
        ));
    }

    // RangeSet::from_iter expects entries sorted by start.
    entries.sort_by_key(|(r, _)| r.start);
    RangeSet::from_iter(entries)
}

const fn severity_rank(s: Severity) -> u8 {
    match s {
        Severity::Hint => 0,
        Severity::Info => 1,
        Severity::Warning => 2,
        Severity::Error => 3,
    }
}
