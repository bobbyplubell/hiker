//! Tree-sitter language adapter for egui_editor (SPEC §13.5, IMPLEMENTATION §16.6.16).
//!
//! This crate wraps `tree_sitter::Parser` into an editor-friendly shape:
//!
//! - [`TsLanguage`] bundles a `tree_sitter::Language` with its query files
//!   (`highlights.scm`, optional injections/indent).
//! - [`TsState`] holds the parsed tree plus a precomputed list of highlight
//!   ranges with their capture-tag (e.g. `"keyword"`, `"string.literal"`).
//! - [`parse`] does a full parse; [`reparse`] performs an incremental
//!   reparse given a previous state and a list of `tree_sitter::InputEdit`s.
//! - [`ts_decorations`] consumes a `TsState` and emits a
//!   `DecorationSet` of [`editor_core::decoration::Decoration::Mark`]s colored via the
//!   active [`editor_core::theme::Theme`]'s `tokens` map.
//!
//! # Language parsers are deferred
//!
//! The concrete `tree-sitter-{rust,python,javascript,…}` crates are *not*
//! pulled in here. They add significant build time and would block CI for
//! consumers that don't need them. Per-language bundles live in
//! [`languages`] behind cargo features (`lang-rust`, `lang-python`, …) and
//! currently panic with a TODO message. Hosts that need a specific
//! language should:
//!
//! 1. Add `tree-sitter-rust = "…"` (or similar) to their own `Cargo.toml`.
//! 2. Enable the matching feature on `editor-ts`.
//! 3. Fill in the matching arm of [`languages::bundle`] to use the upstream
//!    grammar + its bundled `highlights.scm`.
//!
//! Hosts that prefer a fully bespoke setup can also build a `TsLanguage`
//! by hand: combine any `tree_sitter::Language` with whatever query text
//! they like (loaded from disk, embedded with `include_str!`, downloaded
//! at runtime, …).
//!
//! # Markdown integration (future)
//!
//! `editor-md` will keep its pulldown-cmark based live-preview, but
//! fenced-code-block contents will be routed through `editor-ts` via
//! tree-sitter's "injection" mechanism: the markdown parser identifies
//! code-block byte ranges and the corresponding language tag, and this
//! crate scopes a per-language `TsState` to that range. That wiring lives
//! in `editor-md` and is not yet implemented.

pub mod parsing;
pub mod highlight;
pub mod languages;
