//! Per-language `TsLanguage` bundles, gated by cargo features.
//!
//! Each function below returns a fully-populated [`crate::TsLanguage`] for
//! one language. They are stubs in this crate — the actual
//! `tree-sitter-{rust,python,javascript,…}` crates are intentionally **not**
//! depended on, because pulling them all in roughly doubles CI build time
//! and most consumers want only one or two.
//!
//! # Wiring a real grammar (host instructions)
//!
//! In the host's `Cargo.toml`:
//!
//! ```toml
//! [dependencies]
//! editor-ts = { version = "…", features = ["lang-rust"] }
//! tree-sitter-rust = "0.21"
//! ```
//!
//! Then in this file uncomment the body of [`rust`] (and similar) to
//! replace the panic with the real bundle:
//!
//! ```ignore
//! TsLanguage {
//!     language: tree_sitter_rust::LANGUAGE.into(),
//!     highlights_query: tree_sitter_rust::HIGHLIGHTS_QUERY.to_string(),
//!     injections_query: Some(tree_sitter_rust::INJECTIONS_QUERY.to_string()),
//!     indent_query: None,
//! }
//! ```
//!
//! Hosts may also bypass these helpers entirely and construct a
//! [`crate::TsLanguage`] by hand — useful when a grammar lives in a
//! private crate or when the highlights query is customized.

#![allow(unused_imports, dead_code)]

use crate::TsLanguage;

const TODO: &str = "language bundle stub: add `tree-sitter-<lang>` dep and \
                    uncomment the body in `crates/editor-ts/src/languages.rs`";

#[cfg(feature = "lang-rust")]
pub fn rust() -> TsLanguage {
    panic!("{TODO} (rust)");
}

#[cfg(feature = "lang-python")]
pub fn python() -> TsLanguage {
    panic!("{TODO} (python)");
}

#[cfg(feature = "lang-javascript")]
pub fn javascript() -> TsLanguage {
    panic!("{TODO} (javascript)");
}

#[cfg(feature = "lang-typescript")]
pub fn typescript() -> TsLanguage {
    panic!("{TODO} (typescript)");
}

#[cfg(feature = "lang-bash")]
pub fn bash() -> TsLanguage {
    panic!("{TODO} (bash)");
}

#[cfg(feature = "lang-go")]
pub fn go() -> TsLanguage {
    panic!("{TODO} (go)");
}

#[cfg(feature = "lang-json")]
pub fn json() -> TsLanguage {
    panic!("{TODO} (json)");
}

#[cfg(feature = "lang-yaml")]
pub fn yaml() -> TsLanguage {
    panic!("{TODO} (yaml)");
}

#[cfg(feature = "lang-toml")]
pub fn toml() -> TsLanguage {
    panic!("{TODO} (toml)");
}

#[cfg(feature = "lang-html")]
pub fn html() -> TsLanguage {
    panic!("{TODO} (html)");
}

#[cfg(feature = "lang-css")]
pub fn css() -> TsLanguage {
    panic!("{TODO} (css)");
}
