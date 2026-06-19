//! hiker-lsp — a lazy, focus-driven [`spec_engine::DerivedNodeSource`] over rust-analyzer.
//!
//! UI-free and dependency-light: a hand-rolled, blocking JSON-RPC-over-stdio client (no async
//! runtime, no `lsp-types`/`tower-lsp`) drives a live `rust-analyzer` process. Nothing is
//! materialized up front — there is no `code_graph()`; every port method runs a live LSP query and
//! maps the result back. See `code-in-hiker-scratch.md` ("Item 4b").
//!
//! Handles are **positional** (`"{uri}#{sl}:{sc}-{el}:{ec}"`), so `stable_identity = false`: LSP is
//! live-navigation-only; durable spec-linking/drift over LSP is out of scope for now.

mod adapter;
mod lifecycle;
mod protocol;
mod transport;

pub use adapter::LspAdapter;
pub use lifecycle::DEFAULT_READY_TIMEOUT;
pub use transport::{safe_join, LspClient};
