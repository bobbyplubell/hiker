//! Decoration emission: turn a [`TsState`]'s highlight list into a
//! [`editor_core::DecorationSet`] of foreground `Mark`s.

use editor_core::{
    Color, Decoration, DecorationSet, EditorState, MarkStyle, RangeSet, Theme,
};
use smol_str::SmolStr;

use crate::state::TsState;

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
        let Some(color) = resolve_color(tag, theme) else { continue };
        let style = MarkStyle { fg: Some(color), ..MarkStyle::default() };
        entries.push((range.clone(), Decoration::Mark(style)));
    }
    RangeSet::from_iter(entries)
}

fn resolve_color(tag: &SmolStr, theme: Option<&Theme>) -> Option<Color> {
    if let Some(theme) = theme {
        if let Some(c) = theme.tokens.get(tag) {
            return Some(*c);
        }
        // Hierarchical fallback: drop trailing ".sub" components and retry.
        let mut s: &str = tag.as_str();
        while let Some(dot) = s.rfind('.') {
            s = &s[..dot];
            if let Some(c) = theme.tokens.get(&SmolStr::from(s)) {
                return Some(*c);
            }
        }
    }
    default_color(tag.as_str())
}

fn default_color(tag: &str) -> Option<Color> {
    let head = tag.split('.').next().unwrap_or(tag);
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
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_palette_covers_common_tags() {
        for tag in ["keyword", "string.literal", "comment.line", "function.builtin"] {
            assert!(default_color(tag).is_some(), "missing default for {tag}");
        }
    }

    #[test]
    fn unknown_tag_resolves_to_none_without_theme() {
        let tag = SmolStr::from("totally.unknown.tag");
        assert!(resolve_color(&tag, None).is_none());
    }
}
