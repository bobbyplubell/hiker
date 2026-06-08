//! Diagram syntax-check dispatcher.
//!
//! One entry point, [`check_diagram`], that routes a fenced-block language tag
//! to the matching engine behind the shared [`hiker_diagram::DiagramRenderer`]
//! `check()` seam (mermaid / WaveDrom / math). This is the single place the
//! editor's squiggle provider and the agent's `check_diagram` tool both call, so
//! the language→engine mapping lives in exactly one spot.

use hiker_diagram::{Diagnostic, DiagramRenderer};
use hiker_math::Math;
use hiker_mermaid::Mermaid;
use hiker_wavedrom::WaveDrom;

/// Syntax-check a diagram `src` written in `lang`.
///
/// `lang` is matched case-insensitively after trimming. An empty result means
/// the source parses cleanly; a non-empty one carries the problems. An
/// unrecognized `lang` yields a single [`Diagnostic::error`] naming it (rather
/// than silently passing), so callers always get an actionable answer.
pub fn check_diagram(lang: &str, src: &str) -> Vec<Diagnostic> {
    match lang.trim().to_ascii_lowercase().as_str() {
        "mermaid" => Mermaid::check(src),
        "wavedrom" | "wavejson" => WaveDrom::check(src),
        "math" | "latex" => Math::check(src),
        other => vec![Diagnostic::error(format!(
            "unknown diagram language: {other}"
        ))],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hiker_diagram::Severity;

    #[test]
    fn mermaid_valid_is_empty() {
        assert!(check_diagram("mermaid", "graph TD\nA-->B").is_empty());
        // Case/whitespace insensitive.
        assert!(check_diagram("  Mermaid ", "pie\n\"A\" : 10").is_empty());
    }

    #[test]
    fn mermaid_broken_is_non_empty() {
        let diags = check_diagram("mermaid", "pie title\n: notanumber");
        assert!(!diags.is_empty());
        assert_eq!(diags[0].severity, Severity::Error);
    }

    #[test]
    fn wavedrom_valid_is_empty() {
        assert!(check_diagram("wavedrom", r#"{signal:[{name:"clk",wave:"p..."}]}"#).is_empty());
        assert!(check_diagram("wavejson", r#"{signal:[{name:"clk",wave:"p..."}]}"#).is_empty());
    }

    #[test]
    fn wavedrom_broken_is_non_empty() {
        let diags = check_diagram("wavedrom", "{signal:");
        assert!(!diags.is_empty());
        assert_eq!(diags[0].severity, Severity::Error);
    }

    #[test]
    fn math_valid_is_empty() {
        assert!(check_diagram("math", "x^2").is_empty());
        assert!(check_diagram("latex", r"\frac{1}{2}").is_empty());
    }

    #[test]
    fn math_broken_is_non_empty() {
        let diags = check_diagram("math", r"\frac{");
        assert!(!diags.is_empty());
        assert_eq!(diags[0].severity, Severity::Error);
    }

    #[test]
    fn unknown_lang_yields_one_diagnostic() {
        let diags = check_diagram("plantuml", "anything");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, Severity::Error);
        assert!(diags[0].message.contains("plantuml"));
    }
}
