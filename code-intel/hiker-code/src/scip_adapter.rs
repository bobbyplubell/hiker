//! SCIP-consumer adapter: implements [`DerivedNodeSource`] over a `.scip` index.
//!
//! Per-tool strategy keyed off `metadata.tool_info.name` (verified divergence between
//! rust-analyzer and scip-python — see `docs/hiker-code.md`):
//! - **nodes** from definition occurrences; kind from `SymbolInformation.kind` (fine) with a
//!   descriptor-suffix fallback; name from `display_name` else the last descriptor.
//! - **call/ref edges** by `enclosing_range` containment (both tools).
//! - **impl edges** from `relationships.is_implementation` when populated (scip-python), else
//!   reconstructed from `impl#[Type][Trait]method` monikers (rust-analyzer).
//! - **content/fingerprint** read source from the working tree (`Document.text` is empty).

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use protobuf::Message;
use scip::symbol::parse_symbol;
use scip::types::{descriptor::Suffix, Index};

use spec_engine::{
    DerivedNodeSource, EdgeKind, Fingerprint, NodeHandle, SourceCaps, SourceId, SourceLoc,
};

const DEFINITION: i32 = 0x1; // SymbolRole::Definition

#[derive(Clone, Copy)]
struct Span {
    sl: u32,
    sc: u32,
    el: u32,
    ec: u32,
}

impl Span {
    /// SCIP ranges: `[startLine, startChar, endChar]` (single line) or
    /// `[startLine, startChar, endLine, endChar]` (multi-line).
    fn parse(r: &[i32]) -> Option<Span> {
        match r.len() {
            3 => Some(Span { sl: r[0] as u32, sc: r[1] as u32, el: r[0] as u32, ec: r[2] as u32 }),
            4 => Some(Span { sl: r[0] as u32, sc: r[1] as u32, el: r[2] as u32, ec: r[3] as u32 }),
            _ => None,
        }
    }
    fn contains(&self, o: &Span) -> bool {
        (self.sl, self.sc) <= (o.sl, o.sc) && (o.el, o.ec) <= (self.el, self.ec)
    }
    /// Smaller = tighter scope, for innermost-enclosing selection.
    fn extent(&self) -> u64 {
        ((self.el - self.sl) as u64) << 24 | self.ec.saturating_sub(self.sc) as u64
    }
}

#[derive(Clone)]
struct NodeData {
    name: String,
    kind: String,
    file: String,
    range: Span,
    enclosing: Option<Span>,
}

/// One node in the render-shaped [`CodeGraph`]. `id` is the SCIP moniker (stable handle);
/// `kind` is the `code:*` entity kind; `name`/`file`/`start_line` drive labels + tooltips.
#[derive(Debug, Clone)]
pub struct GraphNode {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub file: String,
    pub start_line: u32,
}

/// The adapter's in-memory graph flattened for rendering (`code_graph`). Edges are
/// `(from_idx, to_idx, kind)` over `nodes`; both endpoints are guaranteed to index into `nodes`.
#[derive(Debug, Clone)]
pub struct CodeGraph {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<(usize, usize, EdgeKind)>,
}

pub struct ScipAdapter {
    source: SourceId,
    repo_root: PathBuf,
    tool: String,
    impl_source: &'static str, // "relationships" | "monikers" | "none"
    nodes: HashMap<String, NodeData>,
    out_edges: HashMap<String, Vec<(String, EdgeKind)>>,
    in_edges: HashMap<String, Vec<(String, EdgeKind)>>,
    by_name: HashMap<String, Vec<String>>,
}

/// Map a `SymbolInformation.kind` (Debug name) to our node kind. Robust to the large Kind enum
/// by matching the variant's Debug string rather than importing every variant.
fn kind_from_si(dbg: &str) -> Option<&'static str> {
    Some(match dbg {
        "Function" => "code:function",
        "Method" | "Constructor" | "StaticMethod" | "AbstractMethod" => "code:method",
        "Class" | "Struct" | "Interface" | "Trait" | "Enum" | "TypeAlias" | "Type" | "Union"
        | "Protocol" => "code:type",
        "Module" | "Namespace" | "Package" => "code:module",
        "Macro" => "code:macro",
        "Constant" => "code:constant",
        "Field" | "Property" | "EnumMember" => "code:field",
        _ => return None, // Parameter / Variable / Unspecified / … → not a graph entity
    })
}

/// Descriptor-suffix fallback when `SymbolInformation.kind` is absent/unspecified.
fn kind_from_suffix(suffix: Suffix) -> Option<&'static str> {
    match suffix {
        Suffix::Type => Some("code:type"),
        Suffix::Method => Some("code:method"),
        Suffix::Term => Some("code:function"),
        Suffix::Namespace => Some("code:module"),
        Suffix::Macro => Some("code:macro"),
        _ => None,
    }
}

/// Derive (kind, name) for a symbol, or `None` if it is not a graph entity.
fn entity_kind(symbol: &str, display_name: &str, kind_dbg: Option<&str>) -> Option<(String, String)> {
    if symbol.starts_with("local ") {
        return None;
    }
    let parsed = parse_symbol(symbol).ok();
    let last = parsed.as_ref().and_then(|p| p.descriptors.last());

    let name = if !display_name.is_empty() {
        display_name.to_string()
    } else {
        last.map(|d| d.name.clone()).unwrap_or_default()
    };
    if name.is_empty() {
        return None;
    }

    let kind: &str = match kind_dbg {
        Some(d) if d != "UnspecifiedKind" => kind_from_si(d)?, // known non-entity → None → filtered
        _ => kind_from_suffix(last?.suffix.enum_value().ok()?)?,
    };
    Some((kind.to_string(), name))
}

/// Parse a rust-analyzer impl-method moniker `…impl#[Type][Trait]method…` → (type, trait, method).
fn parse_impl_moniker(symbol: &str) -> Option<(String, String, String)> {
    let rest = &symbol[symbol.find("impl#[")? + "impl#[".len()..];
    let (type_name, rest) = rest.split_once(']')?;
    let rest = rest.strip_prefix('[')?;
    let (trait_name, rest) = rest.split_once(']')?;
    let method: String = rest.chars().take_while(|c| !matches!(c, '(' | '.' | '#')).collect();
    if method.is_empty() {
        return None;
    }
    Some((type_name.to_string(), trait_name.to_string(), method))
}

fn add_edge(
    out: &mut HashMap<String, Vec<(String, EdgeKind)>>,
    inn: &mut HashMap<String, Vec<(String, EdgeKind)>>,
    from: &str,
    to: &str,
    kind: EdgeKind,
) {
    out.entry(from.to_string()).or_default().push((to.to_string(), kind));
    inn.entry(to.to_string()).or_default().push((from.to_string(), kind));
}

impl ScipAdapter {
    pub fn load(index_path: &Path, repo_root: &Path, source: SourceId) -> std::io::Result<Self> {
        let bytes = std::fs::read(index_path)?;
        let index = Index::parse_from_bytes(&bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let tool = index.metadata.tool_info.name.clone();

        let mut out_edges: HashMap<String, Vec<(String, EdgeKind)>> = HashMap::new();
        let mut in_edges: HashMap<String, Vec<(String, EdgeKind)>> = HashMap::new();

        // Pass 1: display names, kinds, and implementation relationships.
        let mut display: HashMap<String, String> = HashMap::new();
        let mut kind_dbg: HashMap<String, String> = HashMap::new();
        let mut has_impl_rel = false;
        let symbol_infos = index
            .documents
            .iter()
            .flat_map(|d| d.symbols.iter())
            .chain(index.external_symbols.iter());
        for si in symbol_infos {
            if !si.display_name.is_empty() {
                display.insert(si.symbol.clone(), si.display_name.clone());
            }
            kind_dbg.insert(si.symbol.clone(), format!("{:?}", si.kind.enum_value_or_default()));
            for rel in &si.relationships {
                if rel.is_implementation {
                    has_impl_rel = true;
                    add_edge(&mut out_edges, &mut in_edges, &si.symbol, &rel.symbol, EdgeKind::Implements);
                }
            }
        }

        // Pass 2: nodes from definition occurrences; call edges from references by containment.
        let mut nodes: HashMap<String, NodeData> = HashMap::new();
        for doc in &index.documents {
            let mut defs: Vec<(String, Span)> = Vec::new();
            for occ in &doc.occurrences {
                if occ.symbol_roles & DEFINITION == 0 {
                    continue;
                }
                let dn = display.get(&occ.symbol).map(String::as_str).unwrap_or("");
                let kd = kind_dbg.get(&occ.symbol).map(String::as_str);
                if let Some((kind, name)) = entity_kind(&occ.symbol, dn, kd) {
                    let Some(range) = Span::parse(&occ.range) else { continue };
                    let enclosing = Span::parse(&occ.enclosing_range);
                    nodes.entry(occ.symbol.clone()).or_insert(NodeData {
                        name,
                        kind,
                        file: doc.relative_path.clone(),
                        range,
                        enclosing,
                    });
                    if let Some(body) = enclosing {
                        defs.push((occ.symbol.clone(), body));
                    }
                }
            }
            for occ in &doc.occurrences {
                if occ.symbol_roles & DEFINITION != 0 || occ.symbol.starts_with("local ") {
                    continue;
                }
                let Some(rspan) = Span::parse(&occ.range) else { continue };
                let mut best: Option<(&str, u64)> = None;
                for (sym, body) in &defs {
                    if body.contains(&rspan) {
                        let e = body.extent();
                        if best.is_none_or(|(_, be)| e < be) {
                            best = Some((sym.as_str(), e));
                        }
                    }
                }
                if let Some((from, _)) = best {
                    if from != occ.symbol {
                        add_edge(&mut out_edges, &mut in_edges, from, &occ.symbol, EdgeKind::Calls);
                    }
                }
            }
        }

        let mut by_name: HashMap<String, Vec<String>> = HashMap::new();
        for (sym, nd) in &nodes {
            by_name.entry(nd.name.to_lowercase()).or_default().push(sym.clone());
        }

        // Pass 3: moniker-based impl recovery when relationships are absent (rust-analyzer).
        let mut moniker_impl = 0usize;
        if !has_impl_rel {
            let mut to_add: Vec<(String, String)> = Vec::new();
            for sym in nodes.keys() {
                if let Some((_t, tr, method)) = parse_impl_moniker(sym) {
                    let needle = format!("{tr}#");
                    if let Some(cands) = by_name.get(&method.to_lowercase()) {
                        if let Some(target) =
                            cands.iter().find(|c| c.contains(&needle) && c.as_str() != sym)
                        {
                            to_add.push((sym.clone(), target.clone()));
                        }
                    }
                }
            }
            moniker_impl = to_add.len();
            for (from, to) in to_add {
                add_edge(&mut out_edges, &mut in_edges, &from, &to, EdgeKind::Implements);
            }
        }

        let impl_source = if has_impl_rel {
            "relationships"
        } else if moniker_impl > 0 {
            "monikers"
        } else {
            "none"
        };

        Ok(Self {
            source,
            repo_root: repo_root.to_path_buf(),
            tool,
            impl_source,
            nodes,
            out_edges,
            in_edges,
            by_name,
        })
    }

    pub fn tool(&self) -> &str {
        &self.tool
    }
    pub fn impl_source(&self) -> &str {
        self.impl_source
    }
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }
    pub fn entities(&self) -> impl Iterator<Item = (&String, &str, &str)> {
        self.nodes.iter().map(|(s, nd)| (s, nd.kind.as_str(), nd.name.as_str()))
    }
    pub fn name_of(&self, id: &str) -> Option<&str> {
        self.nodes.get(id).map(|nd| nd.name.as_str())
    }

    /// The in-memory graph in render shape (`hiker-integration-plan.md` item A): a stable index
    /// per node + a flat edge list. Builds a `symbol -> index` map once, walks `out_edges`, and
    /// drops edges whose endpoints aren't local nodes (external/stdlib leaves). This is the data a
    /// `graph_view::Source` (or the standalone DOT/SVG renderer) consumes — no SCIP types leak out.
    pub fn code_graph(&self) -> CodeGraph {
        // Deterministic ordering so renders/layouts are reproducible across runs.
        let mut syms: Vec<&String> = self.nodes.keys().collect();
        syms.sort();
        let index: HashMap<&str, usize> =
            syms.iter().enumerate().map(|(i, s)| (s.as_str(), i)).collect();

        let nodes: Vec<GraphNode> = syms
            .iter()
            .map(|s| {
                let nd = &self.nodes[*s];
                GraphNode {
                    id: (*s).clone(),
                    name: nd.name.clone(),
                    kind: nd.kind.clone(),
                    file: nd.file.clone(),
                    start_line: nd.range.sl,
                }
            })
            .collect();

        let mut edges: Vec<(usize, usize, EdgeKind)> = Vec::new();
        for from_sym in &syms {
            let Some(outs) = self.out_edges.get(*from_sym) else { continue };
            let fi = index[from_sym.as_str()];
            for (to_sym, kind) in outs {
                // Drop edges to external/local endpoints not present as nodes.
                if let Some(&ti) = index.get(to_sym.as_str()) {
                    edges.push((fi, ti, *kind));
                }
            }
        }
        CodeGraph { nodes, edges }
    }

    fn handle(&self, id: &str) -> NodeHandle {
        NodeHandle { source: self.source.clone(), id: id.to_string() }
    }
    fn read_span(&self, nd: &NodeData) -> Option<String> {
        let span = nd.enclosing.unwrap_or(nd.range);
        let text = std::fs::read_to_string(self.repo_root.join(&nd.file)).ok()?;
        let lines: Vec<&str> = text.lines().collect();
        let start = span.sl as usize;
        let end = (span.el as usize).min(lines.len().saturating_sub(1));
        if start > end || start >= lines.len() {
            return None;
        }
        Some(lines[start..=end].join("\n"))
    }
}

impl DerivedNodeSource for ScipAdapter {
    fn resolve(&self, query: &str, scope: &SourceId) -> Option<NodeHandle> {
        if scope != &self.source {
            return None;
        }
        let q = query.to_lowercase();
        if let Some(v) = self.by_name.get(&q) {
            return v.first().map(|s| self.handle(s));
        }
        self.nodes
            .iter()
            .find(|(_, nd)| nd.name.to_lowercase().contains(&q))
            .map(|(s, _)| self.handle(s))
    }

    fn locate(&self, h: &NodeHandle) -> Option<SourceLoc> {
        let nd = self.nodes.get(&h.id)?;
        Some(SourceLoc {
            file: nd.file.clone(),
            start_line: nd.range.sl,
            end_line: nd.enclosing.map_or(nd.range.el, |e| e.el),
        })
    }

    fn content(&self, h: &NodeHandle) -> Option<String> {
        self.read_span(self.nodes.get(&h.id)?)
    }

    fn fingerprint(&self, h: &NodeHandle) -> Option<Fingerprint> {
        let content = self.content(h)?;
        let norm = content.lines().map(str::trim_end).collect::<Vec<_>>().join("\n");
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        norm.hash(&mut hasher);
        Some(Fingerprint(format!("{:016x}", hasher.finish())))
    }

    fn neighbors(&self, h: &NodeHandle, kinds: &[EdgeKind]) -> Vec<NodeHandle> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for map in [&self.out_edges, &self.in_edges] {
            if let Some(edges) = map.get(&h.id) {
                for (other, kind) in edges {
                    if kinds.contains(kind) && seen.insert(other.clone()) {
                        out.push(self.handle(other));
                    }
                }
            }
        }
        out
    }

    fn capabilities(&self) -> SourceCaps {
        SourceCaps {
            resolution: true,
            stable_identity: true,
            drift: true,
            blast_radius: true,
            implementations: self.impl_source != "none",
        }
    }
}
