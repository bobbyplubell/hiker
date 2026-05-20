//! Diagnostics: structured lint/error reports attached to byte ranges.
//!
//! Diagnostics are produced by hosts (LSP, tree-sitter queries, custom
//! linters) and rendered by a built-in decoration provider as wavy-underlined
//! marks plus per-line gutter markers. See SPEC §9.7 and IMPLEMENTATION §16.5.1.

use smol_str::SmolStr;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Severity {
    Error,
    Warning,
    Info,
    Hint,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Diagnostic {
    /// Byte range in the document this diagnostic applies to.
    pub range: std::ops::Range<usize>,
    pub severity: Severity,
    pub message: SmolStr,
    /// Producer identifier, e.g. "rustc", "clippy", "tree-sitter".
    pub source: SmolStr,
    /// Optional machine-readable code, e.g. "E0308".
    pub code: Option<SmolStr>,
}
