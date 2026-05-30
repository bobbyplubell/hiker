//! Per-language syntax tokenization for fenced code blocks
//! (`editor-code-syntax-highlight`).
//!
//! Each supported language has a `tree_sitter_highlight::HighlightConfiguration`
//! built once per process (lazy, cached behind a `OnceLock<HashMap>`) and reused
//! for every block tokenization. [`tokenize_block`] runs the configuration over
//! the block body and emits `(byte_range, Color)` spans suitable for the
//! markdown decoration provider to attach as `MarkStyle::fg` decorations.
//!
//! Unknown info-string languages return an empty `Vec` so the block renders as
//! plain monospace, matching the spec's "no error, no fallback guess" rule.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::OnceLock;

use editor_core::decoration::Color;
use tree_sitter_highlight::{HighlightConfiguration, HighlightEvent, Highlighter};

/// One row of the highlight palette: the tree-sitter highlight name plus the
/// foreground color the markdown decoration provider should paint for tokens
/// that resolve to that name. Order in [`PALETTE`] is also the highlight index
/// the language configurations are told about via `configure(&names)`.
struct PaletteEntry {
    name: &'static str,
    color: Color,
}

/// Display palette aimed at hiker's default light theme (off-white background);
/// inspired by One Light / Catppuccin Latte. Names are the standard
/// tree-sitter highlight tags; adding a new tag is one row here.
const PALETTE: &[PaletteEntry] = &[
    PaletteEntry { name: "attribute", color: Color::rgb(152, 104, 1) },
    PaletteEntry { name: "comment", color: Color::rgb(160, 161, 167) },
    PaletteEntry { name: "constant", color: Color::rgb(193, 132, 1) },
    PaletteEntry { name: "constant.builtin", color: Color::rgb(193, 132, 1) },
    PaletteEntry { name: "constructor", color: Color::rgb(193, 132, 1) },
    PaletteEntry { name: "embedded", color: Color::rgb(64, 64, 64) },
    PaletteEntry { name: "function", color: Color::rgb(64, 120, 242) },
    PaletteEntry { name: "function.builtin", color: Color::rgb(64, 120, 242) },
    PaletteEntry { name: "function.macro", color: Color::rgb(64, 120, 242) },
    PaletteEntry { name: "function.method", color: Color::rgb(64, 120, 242) },
    PaletteEntry { name: "keyword", color: Color::rgb(166, 38, 164) },
    PaletteEntry { name: "label", color: Color::rgb(166, 38, 164) },
    PaletteEntry { name: "number", color: Color::rgb(152, 104, 1) },
    PaletteEntry { name: "operator", color: Color::rgb(64, 64, 64) },
    PaletteEntry { name: "property", color: Color::rgb(228, 86, 73) },
    PaletteEntry { name: "punctuation", color: Color::rgb(100, 100, 100) },
    PaletteEntry { name: "punctuation.bracket", color: Color::rgb(100, 100, 100) },
    PaletteEntry { name: "punctuation.delimiter", color: Color::rgb(100, 100, 100) },
    PaletteEntry { name: "punctuation.special", color: Color::rgb(166, 38, 164) },
    PaletteEntry { name: "string", color: Color::rgb(80, 161, 79) },
    PaletteEntry { name: "string.escape", color: Color::rgb(193, 132, 1) },
    PaletteEntry { name: "string.special", color: Color::rgb(80, 161, 79) },
    PaletteEntry { name: "tag", color: Color::rgb(228, 86, 73) },
    PaletteEntry { name: "type", color: Color::rgb(193, 132, 1) },
    PaletteEntry { name: "type.builtin", color: Color::rgb(193, 132, 1) },
    PaletteEntry { name: "variable", color: Color::rgb(64, 64, 64) },
    PaletteEntry { name: "variable.builtin", color: Color::rgb(228, 86, 73) },
    PaletteEntry { name: "variable.parameter", color: Color::rgb(193, 132, 1) },
];

/// The per-language `HighlightConfiguration` table. Built once on first call
/// and cached; each config is reused for every block tokenization. Adding a
/// language is one workspace dep + one `add` call in this initializer + one
/// alias in the canonical-language match below.
type ConfigMap = HashMap<&'static str, HighlightConfiguration>;

/// Tokenize the body of a fenced code block and return per-token color spans,
/// each shifted by `base_offset` so callers can hand the ranges straight to a
/// decoration set whose coordinates are document-relative.
///
/// Unknown / unsupported `lang` returns an empty `Vec`. Highlight names the
/// palette doesn't recognise are skipped (the span is dropped rather than
/// colored a generic fallback).
pub fn tokenize_block(
    lang: &str,
    content: &str,
    base_offset: usize,
) -> Vec<(Range<usize>, Color)> {
    // Resolve the info-string token to a canonical language key. Empty info
    // strings + unknown languages both fall out as `None`.
    let head = lang.split(|c: char| c.is_whitespace() || c == ',').next().unwrap_or("");
    let canonical = match head.trim().to_ascii_lowercase().as_str() {
        "rust" | "rs" => "rust",
        "python" | "py" => "python",
        "typescript" | "ts" => "typescript",
        "javascript" | "js" | "mjs" | "cjs" => "javascript",
        "bash" | "sh" | "shell" | "zsh" => "bash",
        "json" => "json",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "markdown" | "md" => "markdown",
        "sql" => "sql",
        _ => return Vec::new(),
    };
    static CACHE: OnceLock<ConfigMap> = OnceLock::new();
    let configs = CACHE.get_or_init(|| {
        let mut out: ConfigMap = HashMap::new();
        let names: Vec<&'static str> = PALETTE.iter().map(|p| p.name).collect();
        let mut add = |key: &'static str, cfg: Result<HighlightConfiguration, _>| {
            if let Ok(mut cfg) = cfg {
                cfg.configure(&names);
                out.insert(key, cfg);
            }
        };
        add("rust", HighlightConfiguration::new(
            tree_sitter_rust::LANGUAGE.into(), "rust",
            tree_sitter_rust::HIGHLIGHTS_QUERY,
            tree_sitter_rust::INJECTIONS_QUERY, ""));
        add("python", HighlightConfiguration::new(
            tree_sitter_python::LANGUAGE.into(), "python",
            tree_sitter_python::HIGHLIGHTS_QUERY, "", ""));
        add("typescript", HighlightConfiguration::new(
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(), "typescript",
            tree_sitter_typescript::HIGHLIGHTS_QUERY, "",
            tree_sitter_typescript::LOCALS_QUERY));
        add("javascript", HighlightConfiguration::new(
            tree_sitter_javascript::LANGUAGE.into(), "javascript",
            tree_sitter_javascript::HIGHLIGHT_QUERY,
            tree_sitter_javascript::INJECTIONS_QUERY,
            tree_sitter_javascript::LOCALS_QUERY));
        add("bash", HighlightConfiguration::new(
            tree_sitter_bash::LANGUAGE.into(), "bash",
            tree_sitter_bash::HIGHLIGHT_QUERY, "", ""));
        add("json", HighlightConfiguration::new(
            tree_sitter_json::LANGUAGE.into(), "json",
            tree_sitter_json::HIGHLIGHTS_QUERY, "", ""));
        add("toml", HighlightConfiguration::new(
            tree_sitter_toml_ng::LANGUAGE.into(), "toml",
            tree_sitter_toml_ng::HIGHLIGHTS_QUERY, "", ""));
        add("yaml", HighlightConfiguration::new(
            tree_sitter_yaml::LANGUAGE.into(), "yaml",
            tree_sitter_yaml::HIGHLIGHTS_QUERY, "", ""));
        add("markdown", HighlightConfiguration::new(
            tree_sitter_md::LANGUAGE.into(), "markdown",
            tree_sitter_md::HIGHLIGHT_QUERY_BLOCK,
            tree_sitter_md::INJECTION_QUERY_BLOCK, ""));
        add("sql", HighlightConfiguration::new(
            tree_sitter_sequel::LANGUAGE.into(), "sql",
            tree_sitter_sequel::HIGHLIGHTS_QUERY, "", ""));
        out
    });
    let Some(cfg) = configs.get(canonical) else {
        return Vec::new();
    };
    let mut highlighter = Highlighter::new();
    let Ok(events) = highlighter.highlight(cfg, content.as_bytes(), None, |_| None) else {
        return Vec::new();
    };
    let mut stack: Vec<usize> = Vec::new();
    let mut out: Vec<(Range<usize>, Color)> = Vec::new();
    for event in events {
        match event {
            Ok(HighlightEvent::HighlightStart(h)) => stack.push(h.0),
            Ok(HighlightEvent::HighlightEnd) => {
                stack.pop();
            }
            Ok(HighlightEvent::Source { start, end }) => {
                if start >= end {
                    continue;
                }
                if let Some(&top) = stack.last() {
                    if let Some(entry) = PALETTE.get(top) {
                        out.push((base_offset + start..base_offset + end, entry.color));
                    }
                }
            }
            Err(_) => return out,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_lang_emits_some_colored_spans() {
        let spans = tokenize_block("rust", "fn main() { let s = \"hi\"; }", 0);
        assert!(!spans.is_empty(), "rust block should produce colored spans");
        // Expect at least one keyword-tinted span (the `fn` / `let`) and one
        // string-tinted span (the `"hi"` literal). The palette assigns distinct
        // colors to keyword vs string, so the set of distinct colors should be
        // at least 2.
        let mut colors: Vec<Color> = spans.iter().map(|(_, c)| *c).collect();
        colors.sort_by_key(|c| (c.r, c.g, c.b));
        colors.dedup();
        assert!(colors.len() >= 2, "expected multiple distinct token colors, got {colors:?}");
    }

    #[test]
    fn unknown_lang_returns_empty() {
        let spans = tokenize_block("not-a-real-language", "anything goes", 0);
        assert!(spans.is_empty());
    }

    #[test]
    fn rs_alias_resolves_to_rust() {
        let a = tokenize_block("rust", "fn x() {}", 0);
        let b = tokenize_block("rs", "fn x() {}", 0);
        assert_eq!(a, b);
        assert!(!a.is_empty());
    }

    #[test]
    fn base_offset_is_added_to_every_range() {
        let src = "fn x() {}";
        let zero = tokenize_block("rust", src, 0);
        let shifted = tokenize_block("rust", src, 100);
        assert_eq!(zero.len(), shifted.len());
        for (a, b) in zero.iter().zip(shifted.iter()) {
            assert_eq!(b.0.start, a.0.start + 100);
            assert_eq!(b.0.end, a.0.end + 100);
            assert_eq!(a.1, b.1);
        }
    }
}
