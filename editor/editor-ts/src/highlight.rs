//! Decoration emission: turn a [`TsState`]'s highlight list into a
//! [`editor_core::decoration::Set`] of foreground `Mark`s.

use editor_core::decoration::Color;

use editor_core::decoration::Decoration;

use editor_core::decoration::Set as DecorationSet;
use editor_core::state::Editor as EditorState;
use editor_core::decoration::MarkStyle;

use editor_core::rangeset::RangeSet;

use editor_core::theme::Theme;
use smol_str::SmolStr;

use crate::parsing::TsState;

/// Convert tree-sitter highlights into a `DecorationSet`.
///
/// Each `(range, tag)` becomes a [`Decoration::Mark`] with `fg` set from
/// `theme.tokens[tag]`. If no theme is supplied (or the theme lacks the
/// tag), we fall back to a small hardcoded default palette that mirrors
/// the bundled light theme's syntax colors.
///
/// Tag lookup is hierarchical in spirit (e.g. `"string.literal"` falls
/// back to `"string"`), matching common tree-sitter capture conventions.
pub fn ts_decorations(
    _state: &EditorState,
    ts: &TsState,
    theme: Option<&Theme>,
) -> DecorationSet {
    let mut entries: Vec<(std::ops::Range<usize>, Decoration)> = Vec::with_capacity(
        ts.highlights.len(),
    );
    for (range, tag) in &ts.highlights {
        let color = 'resolve: {
            if let Some(theme) = theme {
                if let Some(c) = theme.tokens.get(tag) {
                    break 'resolve Some(*c);
                }
                let mut s: &str = tag.as_str();
                while let Some(dot) = s.rfind('.') {
                    s = &s[..dot];
                    if let Some(c) = theme.tokens.get(&SmolStr::from(s)) {
                        break 'resolve Some(*c);
                    }
                }
            }
            let head = tag.as_str().split('.').next().unwrap_or(tag.as_str());
            Some(match head {
                "keyword" => Color::rgb(170, 13, 145),
                "string" => Color::rgb(196, 26, 22),
                "number" => Color::rgb(28, 0, 207),
                "comment" => Color::rgb(0, 116, 0),
                "type" => Color::rgb(63, 110, 116),
                "function" => Color::rgb(58, 92, 138),
                "variable" => Color::rgb(40, 40, 40),
                "operator" | "punctuation" => Color::rgb(80, 80, 80),
                "constant" => Color::rgb(28, 0, 207),
                _ => break 'resolve None,
            })
        };
        let Some(color) = color else { continue };
        let style = MarkStyle { fg: Some(color), ..MarkStyle::default() };
        entries.push((range.clone(), Decoration::Mark(style)));
    }
    RangeSet::from_iter(entries)
}

// Tests live in tests/integration.rs; we can't construct a TsState here
// without depending on a concrete tree-sitter grammar.
