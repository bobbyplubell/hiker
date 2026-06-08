//! Inline-markdown → styled-run parsing for table cells (`widget-table-render`).
//!
//! A table cell's source is a small fragment of inline markdown: `**bold**`,
//! `*italic*` / `_italic_`, `~~strike~~`, `` `code` ``, `[text](url)` links, and
//! inline `$math$` markers. This module turns that fragment into a flat
//! [`Vec<StyledRun>`] — the egui-free, self-contained run list the
//! [`BlockPaint::RichText`] painter consumes — with the markers stripped.
//!
//! The parser is adapted from `hiker_mermaid`'s `label.rs` (which already emits
//! standalone styled segments for diagram labels) rather than
//! `editor_md::styling`, whose marks are *buffer-relative* `Decoration` spans
//! that would need re-anchoring per cell. Two cell-specific additions over the
//! label parser: GFM strikethrough (`~~x~~`) and theme-driven colors (the label
//! parser bakes a fixed link/code palette into SVG; a cell takes the editor
//! theme's colors so it matches surrounding prose).
//!
//! Inline math (`$…$`) is recognized and rendered as inline code styling of the
//! raw `$latex$` text for v1 — the cheap path the plan allows; an actual
//! rasterized formula in a cell is Phase B (block content in cells).

use editor_core::decoration::{Color, StyledRun};

/// Theme-derived colors for an inline run set: the base text color, the link
/// color, and the inline-code background.
#[derive(Clone, Copy)]
pub struct Colors {
    pub text: Color,
    pub link: Color,
    pub code_bg: Color,
}

/// Per-run style accumulated while walking nested emphasis.
#[derive(Clone, Copy, Default)]
struct Emphasis {
    bold: bool,
    italic: bool,
    strike: bool,
}

/// Parse an inline-markdown cell fragment into styled runs (markers stripped).
/// Whitespace inside the fragment is preserved as-is; the caller collapses /
/// wraps. An empty fragment yields an empty run list.
pub fn parse_runs(src: &str, colors: Colors) -> Vec<StyledRun> {
    let chars: Vec<char> = src.chars().collect();
    let mut out: Vec<StyledRun> = Vec::new();
    let mut buf = String::new();
    let mut emph = Emphasis::default();
    let mut i = 0;

    while i < chars.len() {
        if let Some(next) = scan_special(&chars, i, &mut buf, &mut emph, &mut out, colors) {
            i = next;
            continue;
        }
        buf.push(chars[i]);
        i += 1;
    }
    flush(&mut buf, emph, colors.text, &mut out);
    out
}

/// Try to consume a special construct at `i`. On a match it flushes any pending
/// plain text, emits / toggles as needed, and returns the index just past the
/// consumed input. Returns `None` when `chars[i]` is ordinary text.
fn scan_special(
    chars: &[char],
    i: usize,
    buf: &mut String,
    emph: &mut Emphasis,
    out: &mut Vec<StyledRun>,
    colors: Colors,
) -> Option<usize> {
    let c = chars[i];
    match c {
        '\\' if i + 1 < chars.len() => {
            // Backslash escape: the next char is literal.
            buf.push(chars[i + 1]);
            Some(i + 2)
        }
        '`' => scan_code(chars, i, buf, *emph, out, colors),
        '$' => scan_math(chars, i, buf, *emph, out, colors),
        '[' => scan_link(chars, i, buf, *emph, out, colors),
        '~' if double(chars, i, '~') => {
            toggle(buf, emph, colors.text, out, |e| &mut e.strike);
            Some(i + 2)
        }
        '*' | '_' if double(chars, i, c) => {
            toggle(buf, emph, colors.text, out, |e| &mut e.bold);
            Some(i + 2)
        }
        '*' | '_' => {
            toggle(buf, emph, colors.text, out, |e| &mut e.italic);
            Some(i + 1)
        }
        _ => None,
    }
}

/// Whether `chars[i]` and `chars[i+1]` are both `m` (a doubled marker).
fn double(chars: &[char], i: usize, m: char) -> bool {
    chars[i] == m && chars.get(i + 1) == Some(&m)
}

/// Flush pending plain text, then toggle the emphasis flag the selector points
/// at. The flush captures the run with the *pre-toggle* style.
fn toggle(
    buf: &mut String,
    emph: &mut Emphasis,
    text: Color,
    out: &mut Vec<StyledRun>,
    sel: impl Fn(&mut Emphasis) -> &mut bool,
) {
    flush(buf, *emph, text, out);
    let flag = sel(emph);
    *flag = !*flag;
}

/// Emit `buf` as a styled run under `emph` (if non-empty) and clear it.
fn flush(buf: &mut String, emph: Emphasis, text: Color, out: &mut Vec<StyledRun>) {
    if buf.is_empty() {
        return;
    }
    out.push(StyledRun {
        text: std::mem::take(buf).into(),
        color: text,
        bold: emph.bold,
        italic: emph.italic,
        strike: emph.strike,
        underline: false,
        code: false,
        bg: None,
    });
}

/// Inline code `` `code` ``: emit a monospace run with a faint background box.
/// An unterminated / empty span is treated as a literal backtick.
fn scan_code(
    chars: &[char],
    i: usize,
    buf: &mut String,
    emph: Emphasis,
    out: &mut Vec<StyledRun>,
    colors: Colors,
) -> Option<usize> {
    let close = chars[i + 1..].iter().position(|&c| c == '`').map(|p| i + 1 + p)?;
    if close == i + 1 {
        return None; // empty `` `` → literal
    }
    flush(buf, emph, colors.text, out);
    out.push(StyledRun {
        text: chars[i + 1..close].iter().collect::<String>().into(),
        color: colors.text,
        bold: emph.bold,
        italic: emph.italic,
        strike: emph.strike,
        underline: false,
        code: true,
        bg: Some(colors.code_bg),
    });
    Some(close + 1)
}

/// Inline math `$latex$`: v1 renders the raw `$latex$` as inline-code styling
/// (cheap path; a rasterized formula in a cell is Phase B). An escaped or
/// unterminated `$` is literal text.
fn scan_math(
    chars: &[char],
    i: usize,
    buf: &mut String,
    emph: Emphasis,
    out: &mut Vec<StyledRun>,
    colors: Colors,
) -> Option<usize> {
    let close = chars[i + 1..].iter().position(|&c| c == '$').map(|p| i + 1 + p)?;
    let inner: String = chars[i + 1..close].iter().collect();
    if inner.trim().is_empty() {
        return None;
    }
    flush(buf, emph, colors.text, out);
    out.push(StyledRun {
        text: format!("${inner}$").into(),
        color: colors.text,
        bold: emph.bold,
        italic: emph.italic,
        strike: emph.strike,
        underline: false,
        code: true,
        bg: Some(colors.code_bg),
    });
    Some(close + 1)
}

/// Markdown link `[text](url)`: emit the visible `text` underlined in the link
/// color (clickable in-cell links are a deferred follow-up — the url is dropped
/// here). A malformed `[…` stays literal.
fn scan_link(
    chars: &[char],
    i: usize,
    buf: &mut String,
    emph: Emphasis,
    out: &mut Vec<StyledRun>,
    colors: Colors,
) -> Option<usize> {
    let (text, _url, next) = parse_link(chars, i)?;
    flush(buf, emph, colors.text, out);
    out.push(StyledRun {
        text: text.into(),
        color: colors.link,
        bold: emph.bold,
        italic: emph.italic,
        strike: emph.strike,
        underline: true,
        code: false,
        bg: None,
    });
    Some(next)
}

/// Parse `[text](url)` starting at the `[` in `chars[start]`. Returns
/// `(text, url, index_past_close_paren)`. No nested brackets in the text; the
/// `(` must immediately follow `]`.
fn parse_link(chars: &[char], start: usize) -> Option<(String, String, usize)> {
    let mut j = start + 1;
    let mut text = String::new();
    while j < chars.len() && chars[j] != ']' {
        if chars[j] == '[' {
            return None;
        }
        text.push(chars[j]);
        j += 1;
    }
    if j >= chars.len() || text.is_empty() || chars.get(j + 1) != Some(&'(') {
        return None;
    }
    let mut k = j + 2;
    let mut url = String::new();
    while k < chars.len() && chars[k] != ')' {
        url.push(chars[k]);
        k += 1;
    }
    if k >= chars.len() {
        return None;
    }
    Some((text, url, k + 1))
}

/// The visible (rendered) text of a run list, markers stripped — the string the
/// egui-free layout heuristic measures / wraps against.
pub fn runs_text(runs: &[StyledRun]) -> String {
    runs.iter().map(|r| r.text.as_str()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const COLORS: Colors = Colors {
        text: Color::rgb(20, 20, 20),
        link: Color::rgb(0, 90, 200),
        code_bg: Color::rgba(200, 200, 200, 30),
    };

    fn parse(s: &str) -> Vec<StyledRun> {
        parse_runs(s, COLORS)
    }

    #[test]
    fn plain_text_is_one_run() {
        let runs = parse("hello world");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].text, "hello world");
        assert!(!runs[0].bold && !runs[0].italic && !runs[0].code);
    }

    #[test]
    fn empty_yields_no_runs() {
        assert!(parse("").is_empty());
    }

    #[test]
    fn bold_run_strips_markers() {
        let runs = parse("a **b** c");
        assert!(runs.iter().any(|r| r.text == "b" && r.bold), "{runs:?}");
        // No marker leaks into any run.
        assert!(runs.iter().all(|r| !r.text.contains('*')), "{runs:?}");
    }

    #[test]
    fn italic_both_markers() {
        for s in ["*i*", "_i_"] {
            let runs = parse(s);
            assert!(runs.iter().any(|r| r.text == "i" && r.italic && !r.bold), "{s}: {runs:?}");
        }
    }

    #[test]
    fn strikethrough_run() {
        let runs = parse("~~gone~~ here");
        assert!(runs.iter().any(|r| r.text == "gone" && r.strike), "{runs:?}");
        assert!(runs.iter().any(|r| r.text.contains("here") && !r.strike), "{runs:?}");
    }

    #[test]
    fn code_run_is_monospace_with_bg() {
        let runs = parse("run `cargo test` now");
        let code = runs.iter().find(|r| r.code).expect("a code run");
        assert_eq!(code.text, "cargo test");
        assert!(code.bg.is_some(), "code carries a bg box");
        assert!(runs.iter().all(|r| !r.text.contains('`')), "no backtick leaks: {runs:?}");
    }

    #[test]
    fn link_renders_visible_text_underlined_in_link_color() {
        let runs = parse("see [docs](http://x) now");
        let link = runs.iter().find(|r| r.underline).expect("a link run");
        assert_eq!(link.text, "docs");
        assert_eq!(link.color, COLORS.link);
        assert!(runs.iter().all(|r| !r.text.contains('[') && !r.text.contains("http")), "{runs:?}");
    }

    #[test]
    fn malformed_link_stays_literal() {
        let runs = parse("[not a link]");
        assert_eq!(runs_text(&runs), "[not a link]");
        assert!(runs.iter().all(|r| !r.underline));
    }

    #[test]
    fn nested_bold_italic() {
        // **_x_** → a run that is both bold and italic.
        let runs = parse("**_x_**");
        assert!(runs.iter().any(|r| r.text == "x" && r.bold && r.italic), "{runs:?}");
    }

    #[test]
    fn escaped_marker_is_literal() {
        let runs = parse(r"a \* b");
        assert_eq!(runs_text(&runs), "a * b");
        assert!(runs.iter().all(|r| !r.bold && !r.italic));
    }

    #[test]
    fn math_renders_as_code_styled_raw() {
        let runs = parse("E = $x^2$");
        let m = runs.iter().find(|r| r.code).expect("a code-styled math run");
        assert_eq!(m.text, "$x^2$");
    }

    #[test]
    fn runs_text_concatenates_visible() {
        let runs = parse("**a** `b` *c*");
        assert_eq!(runs_text(&runs), "a b c");
    }
}
