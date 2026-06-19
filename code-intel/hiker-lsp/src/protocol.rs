//! Minimal LSP wire types — ONLY the fields we send/receive over the hand-rolled JSON-RPC
//! transport. Messy/unstable shapes are kept as `serde_json::Value` rather than fully modelled;
//! this keeps the crate dependency-light (no `lsp-types`) and resilient to RA's extra fields.

use serde::{Deserialize, Serialize};

/// LSP `Position` — zero-based line + UTF-16 character offset.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

/// LSP `Range` — half-open `[start, end)` over [`Position`]s.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

/// LSP `Location` — a `uri` plus a [`Range`] within it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Location {
    pub uri: String,
    pub range: Range,
}

/// A `workspace/symbol` result row. RA returns the modern `WorkspaceSymbol` shape (with a
/// `location` object), but the legacy `SymbolInformation` shape is identical for our needs, so the
/// single struct covers both. `kind` is the numeric LSP `SymbolKind`.
#[derive(Debug, Clone, Deserialize)]
pub struct WorkspaceSymbol {
    pub name: String,
    /// Numeric LSP `SymbolKind`; captured but not yet used for filtering.
    #[serde(default)]
    #[allow(dead_code)]
    pub kind: i32,
    pub location: Location,
}

/// LSP `CallHierarchyItem` — the node prepared at a position and returned by incoming/outgoing
/// call queries. `selection_range` is the identifier range (what we anchor handles to);
/// `range` is the full enclosing range.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallHierarchyItem {
    pub name: String,
    /// Numeric LSP `SymbolKind`; sent back verbatim to RA but not consumed by us.
    #[serde(default)]
    #[allow(dead_code)]
    pub kind: i32,
    pub uri: String,
    pub range: Range,
    pub selection_range: Range,
}

/// `callHierarchy/incomingCalls` row — `from` is the caller.
#[derive(Debug, Clone, Deserialize)]
pub struct IncomingCall {
    pub from: CallHierarchyItem,
}

/// `callHierarchy/outgoingCalls` row — `to` is the callee.
#[derive(Debug, Clone, Deserialize)]
pub struct OutgoingCall {
    pub to: CallHierarchyItem,
}

/// The subset of `ServerCapabilities` we gate behaviour on. Each provider field is `true` when RA
/// advertised the capability (either as a bare `true` or a richer options object). `definition`,
/// `hover`, and `document_symbol` are captured for completeness / future port methods but are not
/// gated on yet — kept so the advertised surface matches what `initialize` requested.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct ServerCapabilities {
    pub workspace_symbol: bool,
    pub call_hierarchy: bool,
    pub references: bool,
    pub definition: bool,
    pub implementation: bool,
    pub hover: bool,
    pub document_symbol: bool,
}

impl ServerCapabilities {
    /// An LSP capability advertises support as either `true`/`false` or an options *object* (which
    /// also means "supported"). `null`/absent means unsupported.
    fn provider(caps: &serde_json::Value, key: &str) -> bool {
        match caps.get(key) {
            Some(serde_json::Value::Bool(b)) => *b,
            Some(serde_json::Value::Object(_)) => true,
            _ => false,
        }
    }

    /// Parse the `capabilities` object from an `initialize` result.
    pub fn from_initialize(caps: &serde_json::Value) -> ServerCapabilities {
        ServerCapabilities {
            workspace_symbol: Self::provider(caps, "workspaceSymbolProvider"),
            call_hierarchy: Self::provider(caps, "callHierarchyProvider"),
            references: Self::provider(caps, "referencesProvider"),
            definition: Self::provider(caps, "definitionProvider"),
            implementation: Self::provider(caps, "implementationProvider"),
            hover: Self::provider(caps, "hoverProvider"),
            document_symbol: Self::provider(caps, "documentSymbolProvider"),
        }
    }
}
