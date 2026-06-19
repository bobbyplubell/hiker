//! [`LspAdapter`] — a lazy, focus-driven [`DerivedNodeSource`] over rust-analyzer.
//!
//! Unlike `ScipAdapter`, nothing is materialized up front: there is NO `code_graph()`. Every port
//! method drives a live LSP query and maps the result back. Because LSP navigation is *positional*
//! (there is no durable symbol moniker), a node handle encodes the symbol's **location**:
//! `"{uri}#{sl}:{sc}-{el}:{ec}"`. That makes handles fragile across edits, so
//! `capabilities().stable_identity = false` — the honest answer; durable spec-linking/drift over
//! LSP is explicitly out of scope for now (live navigation only).

use std::cell::RefCell;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use spec_engine::{
    DerivedNodeSource, EdgeKind, Fingerprint, NodeHandle, SourceCaps, SourceId, SourceLoc,
};

use crate::lifecycle::{self, DEFAULT_READY_TIMEOUT};
use crate::protocol::{
    CallHierarchyItem, IncomingCall, Location, OutgoingCall, Position, Range, ServerCapabilities,
    WorkspaceSymbol,
};
use crate::transport::{safe_join, LspClient};

/// A live, lazy `DerivedNodeSource` backed by a running rust-analyzer process.
///
/// The [`LspClient`] is single-threaded and needs `&mut` per request, but the port methods take
/// `&self`; we wrap it in a `RefCell` (the adapter is not `Sync`, which is fine — it owns a child
/// process and is used from one thread).
pub struct LspAdapter {
    client: RefCell<LspClient>,
    source: SourceId,
    repo_root: PathBuf,
    caps: ServerCapabilities,
}

/// A parsed location handle: which file + selection range it points at.
struct Handle {
    uri: String,
    range: Range,
}

impl LspAdapter {
    /// Spawn `program` (rust-analyzer) on `repo_root`, initialize it, and block until it is ready
    /// (poll `workspace/symbol` for `probe` until non-empty, default ~120s budget).
    pub fn spawn(
        program: &Path,
        repo_root: &Path,
        probe: &str,
        source: SourceId,
    ) -> io::Result<Self> {
        let mut client = LspClient::spawn(program, repo_root)?;
        let caps = lifecycle::initialize(&mut client, repo_root)?;
        let timeout = std::env::var("HIKER_LSP_READY_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .map(std::time::Duration::from_secs)
            .unwrap_or(DEFAULT_READY_TIMEOUT);
        lifecycle::wait_until_ready(&mut client, probe, timeout)?;
        Ok(LspAdapter { client: RefCell::new(client), source, repo_root: repo_root.to_path_buf(), caps })
    }

    /// Encode a handle from a uri + selection range: `"{uri}#{sl}:{sc}-{el}:{ec}"`.
    fn encode_handle(uri: &str, r: &Range) -> String {
        format!(
            "{uri}#{}:{}-{}:{}",
            r.start.line, r.start.character, r.end.line, r.end.character
        )
    }

    /// Parse a `"{uri}#{sl}:{sc}-{el}:{ec}"` handle id back into a uri + range.
    fn parse_handle(id: &str) -> Option<Handle> {
        let hash = id.rfind('#')?;
        let (uri, rest) = (&id[..hash], &id[hash + 1..]);
        let (start, end) = rest.split_once('-')?;
        let (sl, sc) = start.split_once(':')?;
        let (el, ec) = end.split_once(':')?;
        Some(Handle {
            uri: uri.to_string(),
            range: Range {
                start: Position { line: sl.parse().ok()?, character: sc.parse().ok()? },
                end: Position { line: el.parse().ok()?, character: ec.parse().ok()? },
            },
        })
    }

    fn node_handle(&self, id: String) -> NodeHandle {
        NodeHandle { source: self.source.clone(), id }
    }

    /// Map a `file://` URI to a repo-relative path string (no round-trip through the server).
    fn uri_to_rel(&self, uri: &str) -> String {
        let abs = uri.strip_prefix("file://").unwrap_or(uri);
        let abs_path = Path::new(abs);
        let stripped = match self.repo_root.canonicalize() {
            Ok(r) => abs_path.canonicalize().ok().and_then(|a| a.strip_prefix(&r).ok().map(Path::to_path_buf)),
            Err(_) => abs_path.strip_prefix(&self.repo_root).ok().map(Path::to_path_buf),
        };
        stripped
            .and_then(|p| p.to_str().map(str::to_string))
            .unwrap_or_else(|| abs.to_string())
    }

    /// Read the lines spanned by `range` from the file, clamped under the repo root via `safe_join`.
    fn read_range(&self, uri: &str, range: &Range) -> Option<String> {
        let rel = self.uri_to_rel(uri);
        let path = safe_join(&self.repo_root, &rel)?;
        let text = std::fs::read_to_string(path).ok()?;
        let lines: Vec<&str> = text.lines().collect();
        let start = range.start.line as usize;
        let end = (range.end.line as usize).min(lines.len().saturating_sub(1));
        if start > end || start >= lines.len() {
            return None;
        }
        Some(lines[start..=end].join("\n"))
    }

    /// Run `prepareCallHierarchy` at the handle's selection-range start; returns the first item.
    fn prepare_call_hierarchy(&self, h: &Handle) -> Option<CallHierarchyItem> {
        let mut client = self.client.borrow_mut();
        client.ensure_open(&h.uri).ok()?;
        let result = client
            .request(
                "textDocument/prepareCallHierarchy",
                json!({
                    "textDocument": { "uri": h.uri },
                    "position": { "line": h.range.start.line, "character": h.range.start.character }
                }),
            )
            .ok()?;
        serde_json::from_value::<Vec<CallHierarchyItem>>(result).ok()?.into_iter().next()
    }

    /// Incoming + outgoing call neighbors of `item`, each mapped to a location handle and deduped.
    fn call_neighbors(&self, item: &CallHierarchyItem) -> Vec<NodeHandle> {
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let item_v = serde_json::to_value(item).unwrap_or(Value::Null);
        let mut push = |uri: &str, r: &Range, this: &mut Vec<NodeHandle>| {
            let id = Self::encode_handle(uri, r);
            if seen.insert(id.clone()) {
                this.push(NodeHandle { source: self.source.clone(), id });
            }
        };
        let mut client = self.client.borrow_mut();
        if let Ok(v) = client.request("callHierarchy/incomingCalls", json!({ "item": item_v })) {
            for c in serde_json::from_value::<Vec<IncomingCall>>(v).unwrap_or_default() {
                push(&c.from.uri, &c.from.selection_range, &mut out);
            }
        }
        if let Ok(v) = client.request("callHierarchy/outgoingCalls", json!({ "item": item_v })) {
            for c in serde_json::from_value::<Vec<OutgoingCall>>(v).unwrap_or_default() {
                push(&c.to.uri, &c.to.selection_range, &mut out);
            }
        }
        out
    }

    /// `textDocument/implementation` at the handle position → location handles (for `Implements`).
    fn implementation_neighbors(&self, h: &Handle) -> Vec<NodeHandle> {
        let mut client = self.client.borrow_mut();
        if client.ensure_open(&h.uri).is_err() {
            return Vec::new();
        }
        let result = client.request(
            "textDocument/implementation",
            json!({
                "textDocument": { "uri": h.uri },
                "position": { "line": h.range.start.line, "character": h.range.start.character }
            }),
        );
        drop(client);
        let locs = match result {
            Ok(v) => locations_from(v),
            Err(_) => return Vec::new(),
        };
        let mut seen = std::collections::HashSet::new();
        locs.into_iter()
            .map(|l| Self::encode_handle(&l.uri, &l.range))
            .filter(|id| seen.insert(id.clone()))
            .map(|id| self.node_handle(id))
            .collect()
    }

    /// Pick the best `workspace/symbol` hit for `query`, deterministically: exact-name first, then
    /// **definitions before re-exports** (a `use`/`pub use` line is navigation noise, not a
    /// definition), then shortest name, then `(name, uri, line)`. `is_reexport` reads the hit's line
    /// to detect re-export entries (RA returns both the `pub use` and the real `struct`/`fn`).
    fn best_symbol(
        query: &str,
        mut syms: Vec<WorkspaceSymbol>,
        is_reexport: impl Fn(&Location) -> bool,
    ) -> Option<WorkspaceSymbol> {
        let ql = query.to_lowercase();
        syms.sort_by_cached_key(|s| {
            let exact = if s.name.to_lowercase() == ql { 0 } else { 1 };
            let reexport = if is_reexport(&s.location) { 1 } else { 0 };
            (exact, reexport, s.name.len(), s.name.clone(), s.location.uri.clone(), s.location.range.start.line)
        });
        syms.into_iter()
            .find(|s| s.name.to_lowercase() == ql || s.name.to_lowercase().contains(&ql))
    }

    /// True if the symbol's line is an import/re-export (`use ...`) rather than a definition.
    fn is_reexport(&self, loc: &Location) -> bool {
        let rel = self.uri_to_rel(&loc.uri);
        let Some(path) = safe_join(&self.repo_root, &rel) else { return false };
        let Ok(text) = std::fs::read_to_string(path) else { return false };
        match text.lines().nth(loc.range.start.line as usize) {
            Some(line) => {
                let t = line.trim_start();
                t.starts_with("use ") || t.starts_with("pub use ")
            }
            None => false,
        }
    }
}

/// Extract `Location`s from a definition/implementation result (single `Location`, an array, or
/// `LocationLink[]` with `targetUri`/`targetSelectionRange`). Tolerant of RA's shapes.
fn locations_from(v: Value) -> Vec<Location> {
    match v {
        Value::Null => Vec::new(),
        Value::Array(items) => items.into_iter().filter_map(location_one).collect(),
        single => location_one(single).into_iter().collect(),
    }
}

fn location_one(v: Value) -> Option<Location> {
    if let Ok(loc) = serde_json::from_value::<Location>(v.clone()) {
        return Some(loc);
    }
    // LocationLink shape.
    let uri = v.get("targetUri")?.as_str()?.to_string();
    let range = v.get("targetSelectionRange").or_else(|| v.get("targetRange"))?;
    Some(Location { uri, range: serde_json::from_value(range.clone()).ok()? })
}

impl DerivedNodeSource for LspAdapter {
    fn resolve(&self, query: &str, scope: &SourceId) -> Option<NodeHandle> {
        if scope != &self.source {
            return None;
        }
        let result = lifecycle::workspace_symbol(&mut self.client.borrow_mut(), query).ok()?;
        let syms: Vec<WorkspaceSymbol> = serde_json::from_value(result).ok()?;
        let best = Self::best_symbol(query, syms, |loc| self.is_reexport(loc))?;
        Some(self.node_handle(Self::encode_handle(&best.location.uri, &best.location.range)))
    }

    fn locate(&self, h: &NodeHandle) -> Option<SourceLoc> {
        let parsed = Self::parse_handle(&h.id)?;
        Some(SourceLoc {
            file: self.uri_to_rel(&parsed.uri),
            start_line: parsed.range.start.line,
            end_line: parsed.range.end.line,
        })
    }

    fn content(&self, h: &NodeHandle) -> Option<String> {
        let parsed = Self::parse_handle(&h.id)?;
        self.read_range(&parsed.uri, &parsed.range)
    }

    fn fingerprint(&self, h: &NodeHandle) -> Option<Fingerprint> {
        use std::hash::{Hash, Hasher};
        let content = self.content(h)?;
        let norm = content.lines().map(str::trim_end).collect::<Vec<_>>().join("\n");
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        norm.hash(&mut hasher);
        Some(Fingerprint(format!("{:016x}", hasher.finish())))
    }

    fn neighbors(&self, h: &NodeHandle, kinds: &[EdgeKind]) -> Vec<NodeHandle> {
        let Some(parsed) = Self::parse_handle(&h.id) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if kinds.contains(&EdgeKind::Calls) && self.caps.call_hierarchy {
            if let Some(item) = self.prepare_call_hierarchy(&parsed) {
                out.extend(self.call_neighbors(&item));
            }
        }
        // Implements / TypeRef: live `textDocument/implementation` when advertised, else stubbed
        // to [] behind the capability gate.
        if (kinds.contains(&EdgeKind::Implements) || kinds.contains(&EdgeKind::TypeRef))
            && self.caps.implementation
        {
            out.extend(self.implementation_neighbors(&parsed));
        }
        // EdgeKind::Imports / EdgeKind::Link: not modelled over LSP → ignored.
        let mut seen = std::collections::HashSet::new();
        out.retain(|n| seen.insert(n.id.clone()));
        out
    }

    fn capabilities(&self) -> SourceCaps {
        SourceCaps {
            resolution: self.caps.workspace_symbol,
            // LSP handles are positional → not durable across edits.
            stable_identity: false,
            drift: true,
            blast_radius: self.caps.call_hierarchy && self.caps.references,
            implementations: self.caps.implementation,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_round_trips() {
        let r = Range {
            start: Position { line: 10, character: 4 },
            end: Position { line: 10, character: 18 },
        };
        let id = LspAdapter::encode_handle("file:///repo/src/lib.rs", &r);
        assert_eq!(id, "file:///repo/src/lib.rs#10:4-10:18");
        let h = LspAdapter::parse_handle(&id).expect("parse");
        assert_eq!(h.uri, "file:///repo/src/lib.rs");
        assert_eq!(h.range, r);
    }

    #[test]
    fn best_symbol_prefers_exact() {
        let mk = |name: &str, line: u32| WorkspaceSymbol {
            name: name.to_string(),
            kind: 0,
            location: Location {
                uri: "file:///x.rs".into(),
                range: Range {
                    start: Position { line, character: 0 },
                    end: Position { line, character: 1 },
                },
            },
        };
        let syms = vec![mk("ScipAdapterBuilder", 1), mk("ScipAdapter", 2)];
        let best = LspAdapter::best_symbol("ScipAdapter", syms, |_| false).expect("hit");
        assert_eq!(best.name, "ScipAdapter");
    }

    #[test]
    fn best_symbol_prefers_definition_over_reexport() {
        let mk = |line: u32| WorkspaceSymbol {
            name: "ScipAdapter".to_string(),
            kind: 23,
            location: Location {
                uri: format!("file:///{line}.rs"),
                range: Range {
                    start: Position { line, character: 0 },
                    end: Position { line, character: 1 },
                },
            },
        };
        // Pretend the line-8 hit is a re-export, the line-147 hit a definition.
        let syms = vec![mk(8), mk(147)];
        let best = LspAdapter::best_symbol("ScipAdapter", syms, |loc| loc.uri.contains("/8.rs"))
            .expect("hit");
        assert_eq!(best.location.range.start.line, 147);
    }
}
