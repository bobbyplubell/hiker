//! Themes as data (SPEC §9.20, IMPLEMENTATION §16.6.12).
//!
//! A [`Theme`] is a pure-data bundle of colors consumed by decoration
//! providers (markdown, diff, diagnostics, …) instead of hardcoding RGB
//! values. Hosts store themes in a [`Compartment`](crate::Compartment) and
//! can switch between them via `reconfigure` without rebuilding extensions.
//!
//! Bundled themes: [`light_default`] and [`dark_default`]. Hosts can build
//! their own [`Theme`] from scratch or by mutating a clone of a bundled one.
//!
//! Decoration providers accept `Option<&Theme>`. `None` preserves the
//! historical hardcoded palette for backwards compatibility; `Some(theme)`
//! routes colors through the theme.

use std::collections::HashMap;

use smol_str::SmolStr;

use crate::decoration::Color;

/// Palette of general-purpose UI colors used by editor widgets and gutters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Palette {
    pub bg: Color,
    pub fg: Color,
    pub accent: Color,
    pub dim: Color,
    pub error: Color,
    pub warning: Color,
    pub info: Color,
    pub hint: Color,
    pub selection: Color,
    pub current_line: Color,
    pub gutter_fg: Color,
    pub gutter_bg: Color,
    pub border: Color,
}

/// Colors used by the diff decoration provider.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffColors {
    pub added_bg: Color,
    pub removed_bg: Color,
    pub modified_bg: Color,
    pub word_added: Color,
    pub word_removed: Color,
    pub hatched: Color,
}

/// Colors used by the diagnostic decoration provider, by severity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiagnosticColors {
    pub error: Color,
    pub warning: Color,
    pub info: Color,
    pub hint: Color,
}

/// Colors used by the markdown / wiki / callout / math / mermaid providers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkdownColors {
    pub heading: Color,
    pub link: Color,
    pub code_bg: Color,
    pub quote_bar: Color,
    pub quote_bg: Color,
    pub callout_note_bg: Color,
    pub callout_warning_bg: Color,
    pub callout_tip_bg: Color,
}

/// A data-only theme. Cheap to clone (no Arcs, just plain structs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Theme {
    pub name: SmolStr,
    pub palette: Palette,
    /// Syntax-tag → color map. Tags are unconstrained strings (e.g. `"keyword"`,
    /// `"string"`, `"comment.line"`); hosts and language extensions agree on the
    /// keys. Themes without an entry for a tag should fall back to
    /// `palette.fg`.
    pub tokens: HashMap<SmolStr, Color>,
    pub diff: DiffColors,
    pub diagnostics: DiagnosticColors,
    pub markdown: MarkdownColors,
}

/// Default light theme. Colors are chosen to match the historical
/// hardcoded values used by the decoration providers, with adjustments
/// for a light background.
pub fn light_default() -> Theme {
    let mut tokens: HashMap<SmolStr, Color> = HashMap::new();
    tokens.insert(SmolStr::from("keyword"), Color::rgb(170, 13, 145));
    tokens.insert(SmolStr::from("string"), Color::rgb(196, 26, 22));
    tokens.insert(SmolStr::from("number"), Color::rgb(28, 0, 207));
    tokens.insert(SmolStr::from("comment"), Color::rgb(0, 116, 0));
    tokens.insert(SmolStr::from("type"), Color::rgb(63, 110, 116));
    tokens.insert(SmolStr::from("function"), Color::rgb(58, 92, 138));

    Theme {
        name: SmolStr::from("light_default"),
        palette: Palette {
            bg: Color::rgb(255, 255, 255),
            fg: Color::rgb(40, 40, 40),
            accent: Color::rgb(0, 120, 215),
            dim: Color::rgb(140, 140, 140),
            error: Color::rgba(244, 71, 71, 255),
            warning: Color::rgba(228, 188, 48, 255),
            info: Color::rgba(75, 156, 211, 255),
            hint: Color::rgba(160, 160, 160, 255),
            selection: Color::rgba(0, 120, 215, 60),
            current_line: Color::rgba(0, 0, 0, 12),
            gutter_fg: Color::rgb(140, 140, 140),
            gutter_bg: Color::rgb(248, 248, 248),
            border: Color::rgb(220, 220, 220),
        },
        tokens,
        diff: DiffColors {
            added_bg: Color::rgba(46, 160, 67, 38),
            removed_bg: Color::rgba(248, 81, 73, 38),
            modified_bg: Color::rgba(46, 160, 67, 38),
            word_added: Color::rgba(46, 160, 67, 110),
            word_removed: Color::rgba(248, 81, 73, 110),
            hatched: Color::rgba(140, 140, 160, 70),
        },
        diagnostics: DiagnosticColors {
            error: Color::rgba(244, 71, 71, 255),
            warning: Color::rgba(228, 188, 48, 255),
            info: Color::rgba(75, 156, 211, 255),
            hint: Color::rgba(160, 160, 160, 255),
        },
        markdown: MarkdownColors {
            heading: Color::rgb(40, 40, 40),
            link: Color::rgb(86, 156, 214),
            code_bg: Color::rgba(120, 120, 120, 30),
            quote_bar: Color::rgb(140, 140, 160),
            quote_bg: Color::rgba(120, 120, 120, 20),
            callout_note_bg: Color::rgba(86, 156, 214, 30),
            callout_warning_bg: Color::rgba(220, 170, 60, 35),
            callout_tip_bg: Color::rgba(80, 200, 120, 35),
        },
    }
}

/// Default dark theme. Token + palette colors are tuned for a dark
/// background; the diff / diagnostics / markdown sub-palettes reuse the
/// same tints as light (they already have transparency-blended bg's that
/// work on both backgrounds).
pub fn dark_default() -> Theme {
    let mut tokens: HashMap<SmolStr, Color> = HashMap::new();
    tokens.insert(SmolStr::from("keyword"), Color::rgb(197, 134, 192));
    tokens.insert(SmolStr::from("string"), Color::rgb(206, 145, 120));
    tokens.insert(SmolStr::from("number"), Color::rgb(181, 206, 168));
    tokens.insert(SmolStr::from("comment"), Color::rgb(106, 153, 85));
    tokens.insert(SmolStr::from("type"), Color::rgb(78, 201, 176));
    tokens.insert(SmolStr::from("function"), Color::rgb(220, 220, 170));

    Theme {
        name: SmolStr::from("dark_default"),
        palette: Palette {
            bg: Color::rgb(30, 30, 30),
            fg: Color::rgb(212, 212, 212),
            accent: Color::rgb(86, 156, 214),
            dim: Color::rgb(120, 120, 120),
            error: Color::rgba(244, 71, 71, 255),
            warning: Color::rgba(228, 188, 48, 255),
            info: Color::rgba(75, 156, 211, 255),
            hint: Color::rgba(160, 160, 160, 255),
            selection: Color::rgba(86, 156, 214, 80),
            current_line: Color::rgba(255, 255, 255, 16),
            gutter_fg: Color::rgb(110, 110, 110),
            gutter_bg: Color::rgb(36, 36, 36),
            border: Color::rgb(60, 60, 60),
        },
        tokens,
        diff: DiffColors {
            added_bg: Color::rgba(46, 160, 67, 50),
            removed_bg: Color::rgba(248, 81, 73, 50),
            modified_bg: Color::rgba(46, 160, 67, 50),
            word_added: Color::rgba(46, 160, 67, 130),
            word_removed: Color::rgba(248, 81, 73, 130),
            hatched: Color::rgba(140, 140, 160, 80),
        },
        diagnostics: DiagnosticColors {
            error: Color::rgba(244, 71, 71, 255),
            warning: Color::rgba(228, 188, 48, 255),
            info: Color::rgba(75, 156, 211, 255),
            hint: Color::rgba(160, 160, 160, 255),
        },
        markdown: MarkdownColors {
            heading: Color::rgb(229, 229, 229),
            link: Color::rgb(86, 156, 214),
            code_bg: Color::rgba(200, 200, 200, 30),
            quote_bar: Color::rgb(140, 140, 160),
            quote_bg: Color::rgba(200, 200, 200, 18),
            callout_note_bg: Color::rgba(86, 156, 214, 45),
            callout_warning_bg: Color::rgba(220, 170, 60, 50),
            callout_tip_bg: Color::rgba(80, 200, 120, 50),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_have_distinct_names() {
        let l = light_default();
        let d = dark_default();
        assert_eq!(l.name, "light_default");
        assert_eq!(d.name, "dark_default");
        assert_ne!(l.palette.bg, d.palette.bg);
    }
}
