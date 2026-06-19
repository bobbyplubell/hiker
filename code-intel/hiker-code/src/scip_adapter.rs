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
    DerivedNodeSource, EdgeKind, Fingerprint, NodeHandle, Resolution, SourceCaps, SourceId,
    SourceLoc,
};

const DEFINITION: i32 = 0x1; // SymbolRole::Definition

fn hash_str(s: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// The tree-sitter grammar for a source file, by extension. Only languages with a bundled grammar
/// get the structural fingerprint; everything else falls back to [`line_normalized`] — a
/// wrong-grammar parse misclassifies tokens and would silently *drop* changed text, i.e. drift
/// false negatives (reliable-or-absent applies to the drift path too).
fn grammar_for(file: &str) -> Option<tree_sitter::Language> {
    match Path::new(file).extension()?.to_str()? {
        "rs" => Some(tree_sitter_rust::LANGUAGE.into()),
        "py" => Some(tree_sitter_python::LANGUAGE.into()),
        _ => None,
    }
}

/// Fallback fingerprint input where no grammar applies: per-line `trim_end` only.
fn line_normalized(src: &str) -> String {
    src.lines().map(str::trim_end).collect::<Vec<_>>().join("\n")
}

/// Fingerprint-coverage gap among `files` (one entry per node): how many lack a tree-sitter
/// grammar, plus the distinct extensions involved (`<none>` for extensionless). Feeds the
/// one-time load warning — the per-target fallback is silent, and "looks governed, fingerprints
/// weaker" is the same false-comfort class the Python grammar fix closed.
fn grammar_gap_stats<'a>(files: impl IntoIterator<Item = &'a str>) -> (usize, Vec<String>) {
    let mut count = 0usize;
    let mut exts = std::collections::BTreeSet::new();
    for f in files {
        if grammar_for(f).is_none() {
            count += 1;
            let ext = Path::new(f).extension().and_then(|e| e.to_str()).unwrap_or("<none>");
            exts.insert(ext.to_string());
        }
    }
    (count, exts.into_iter().collect())
}

/// AST-normalized token stream of a snippet (`spec-ast-fingerprint`): node-kinds + the source text
/// of every **non-comment leaf**, with comments and whitespace dropped. So `fmt`, re-wrapping, and
/// comment edits don't change it, but logic / name / literal / operator changes do — in any grammar.
/// (Keeping *all* leaf text, not just `identifier`/`literal` kinds, matters: grammars name literal
/// kinds inconsistently — Python numbers are `integer`/`float` — and an allowlist silently ignores
/// real edits in the kinds it misses.) `None` if the grammar won't load or the parse fails.
fn ast_normalized(src: &str, lang: tree_sitter::Language) -> Option<String> {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&lang).ok()?;
    let tree = parser.parse(src, None)?;
    let mut out = String::new();
    ast_tokens(tree.root_node(), src, &mut out);
    Some(out)
}

/// The token walk behind [`ast_normalized`]: node kinds + the text of every non-comment leaf,
/// appended to `out`. Shared with the per-definition fingerprints of
/// [`named_def_fingerprints`], so the whole-snippet and symbol-grain hashes can't drift on
/// what "normalized" means.
fn ast_tokens(n: tree_sitter::Node, src: &str, out: &mut String) {
    let kind = n.kind();
    if kind.contains("comment") {
        return;
    }
    out.push_str(kind);
    if n.child_count() == 0 {
        // Keyword/punctuation text is constant per kind, so including it is harmless;
        // identifier/literal/operator text is what makes real edits change the hash.
        if let Ok(t) = n.utf8_text(src.as_bytes()) {
            out.push(':');
            out.push_str(t);
        }
    }
    out.push('\n');
    let mut c = n.walk();
    for child in n.children(&mut c) {
        ast_tokens(child, src, out);
    }
}

/// Per-name fingerprints of every named definition in `src`
/// (`code-graph-diff-symbol-level`): parse once, walk for definition-shaped nodes carrying a
/// `name` field (Rust `*_item` / `*_declaration` / `enum_variant`, Python `*_definition`),
/// and hash each node's [`ast_tokens`] stream — the drift fingerprint's normalization
/// applied per definition, located by NAME rather than index spans (spans are index-time; a
/// line-shifted symbol would misattribute through them). The value is the name's **sorted
/// fingerprint multiset**, so same-name namesakes (one method name across impls) compare as
/// a set instead of guessing which is which. `None` if the parse fails.
fn named_def_fingerprints(
    src: &str,
    lang: &tree_sitter::Language,
) -> Option<HashMap<String, Vec<String>>> {
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(lang).ok()?;
    let tree = parser.parse(src, None)?;
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    let mut stack = vec![tree.root_node()];
    while let Some(n) = stack.pop() {
        let kind = n.kind();
        let is_def = kind.ends_with("_item")
            || kind.ends_with("_declaration")
            || kind.contains("definition")
            || kind == "enum_variant";
        if is_def {
            if let Some(name) = n.child_by_field_name("name") {
                if let Ok(name) = name.utf8_text(src.as_bytes()) {
                    let mut tokens = String::new();
                    ast_tokens(n, src, &mut tokens);
                    out.entry(name.to_string()).or_default().push(hash_str(&tokens));
                }
            }
        }
        let mut c = n.walk();
        for child in n.children(&mut c) {
            stack.push(child);
        }
    }
    for v in out.values_mut() {
        v.sort();
    }
    Some(out)
}

/// Whether `name`'s definition differs between two [`named_def_fingerprints`] maps:
/// unchanged **only** when the name is present on both sides with identical fingerprint
/// multisets. Absence on either side — added / deleted / renamed, or a kind the walk can't
/// name-locate (e.g. Python module-level constants) — reads as changed. The conservative
/// core of [`symbol_changed_vs`].
fn def_changed(
    head: &HashMap<String, Vec<String>>,
    work: &HashMap<String, Vec<String>>,
    name: &str,
) -> bool {
    match (head.get(name), work.get(name)) {
        (Some(h), Some(w)) => h != w,
        _ => true,
    }
}

/// Whether the definition named `name` in `file` changed between `head_text` (the file's
/// content at HEAD, from `show(HEAD, path)`) and `worktree_text` (its content now) — the
/// drift AST fingerprint generalized from "vs. baseline" to "vs. HEAD"
/// (`code-graph-diff-symbol-level`).
///
/// Index spans are index-time, so the HEAD side is located by **name-anchored extraction**,
/// never spans: both texts are parsed whole and the named definition nodes compared, which
/// makes a pure line move unmisattributable by construction. The failure direction is
/// **over-flag, never silently dim**: returns `true` (changed) whenever it cannot *prove*
/// the body is HEAD-identical — same-name namesakes where any one changed, a name absent on
/// either side, no AST grammar for `file`, or a parse failure.
pub fn symbol_changed_vs(head_text: &str, worktree_text: &str, file: &str, name: &str) -> bool {
    let Some(lang) = grammar_for(file) else {
        return true; // no grammar → can't isolate a body; stay at file grain
    };
    match (
        named_def_fingerprints(head_text, &lang),
        named_def_fingerprints(worktree_text, &lang),
    ) {
        (Some(head), Some(work)) => def_changed(&head, &work, name),
        _ => true, // parse failure → unprovable
    }
}

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
/// `parent` is the index of the **containing** node (a type/module for a method/field, the module
/// for a free function), derived from moniker nesting — drives level-of-detail collapse.
#[derive(Debug, Clone)]
pub struct GraphNode {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub file: String,
    pub start_line: u32,
    /// Definition body size in lines, from the SCIP `enclosing_range` (the full
    /// def span, not just the identifier). `1` when the indexer emitted no
    /// enclosing range. Drives the optional "size by LOC" node weighting.
    pub lines: u32,
    pub parent: Option<usize>,
}

impl GraphNode {
    /// Whether this entity is a structural **object** (a container — type or module) vs. a **member**
    /// (method / function / field / constant / macro). The level-of-detail default shows objects
    /// only and reveals members on expand.
    pub fn is_object(&self) -> bool {
        matches!(self.kind.as_str(), "code:type" | "code:module")
    }
}

/// The adapter's in-memory graph flattened for rendering (`code_graph`). Edges are
/// `(from_idx, to_idx, kind)` over `nodes`; both endpoints are guaranteed to index into `nodes`.
#[derive(Debug, Clone)]
pub struct CodeGraph {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<(usize, usize, EdgeKind)>,
}

/// A [`CodeGraph`] collapsed to a visible subset for level-of-detail rendering: `nodes` holds the
/// original indices that are shown; `edges` are remapped to `0..nodes.len()`, with edges between
/// hidden nodes lifted up to their nearest visible ancestor (so "type A uses type B" still shows
/// even when their members are hidden).
#[derive(Debug, Clone)]
pub struct CollapsedGraph {
    pub nodes: Vec<usize>,
    pub edges: Vec<(usize, usize, EdgeKind)>,
}

/// Collapse `graph` to the nodes where `visible(i)` is true, lifting every hidden node's edges up its
/// `parent` chain to the nearest visible ancestor and de-duplicating the result. A **pure,
/// policy-free** helper — the consumer decides what's visible (objects-only, a granularity tier,
/// expanded objects, …). Engine-agnostic; reusable by any code-graph consumer.
pub fn collapse(graph: &CodeGraph, visible: impl Fn(usize) -> bool) -> CollapsedGraph {
    let n = graph.nodes.len();
    let vis: Vec<bool> = (0..n).map(visible).collect();
    // Nearest visible ancestor (self if visible), walking the parent chain with a cycle guard.
    let rep: Vec<Option<usize>> = (0..n)
        .map(|i| {
            let mut cur = Some(i);
            let mut steps = 0;
            while let Some(c) = cur {
                if vis[c] {
                    return Some(c);
                }
                cur = graph.nodes[c].parent;
                steps += 1;
                if steps > n {
                    break;
                }
            }
            None
        })
        .collect();
    let keep: Vec<usize> = (0..n).filter(|&i| vis[i]).collect();
    let mut local = vec![usize::MAX; n];
    for (l, &g) in keep.iter().enumerate() {
        local[g] = l;
    }
    let mut seen: HashSet<(usize, usize, EdgeKind)> = HashSet::new();
    let mut edges = Vec::new();
    for &(a, b, k) in &graph.edges {
        if let (Some(ra), Some(rb)) = (rep[a], rep[b]) {
            if ra != rb {
                let e = (local[ra], local[rb], k);
                if seen.insert(e) {
                    edges.push(e);
                }
            }
        }
    }
    CollapsedGraph { nodes: keep, edges }
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
    /// Short body form → moniker (`Some`), or `None` when the form is AMBIGUOUS (names >1
    /// symbol). Holds both the short descriptor path and the crate-qualified `<crate>/<short>`
    /// form ([`index_short_forms`]) — how doc wikilinks (`[[code:repo/<short>]]`) address
    /// symbols. Resolving an ambiguous form to an arbitrary winner would navigate (and baseline)
    /// the wrong symbol. status: spec-code-link · status: code-link-crate-qualified
    by_short: HashMap<String, Option<String>>,
}

/// Readable short form of a SCIP moniker — the descriptor path with the
/// `<scheme> <manager> <package> <version> ` prefix dropped and trailing punctuation trimmed:
/// `rust-analyzer cargo hiker-core 0.0.0 trails/ops/delete_trail().` → `trails/ops/delete_trail`.
/// This is the body format of `[[code:<repo_id>/<short>]]` wikilinks; the single source of truth
/// for authoring (seed/gap_list), reconciling, and resolving them. status: spec-code-link
pub fn short_sym(moniker: &str) -> String {
    let d = moniker.splitn(5, ' ').nth(4).unwrap_or(moniker);
    d.trim_end_matches(['.', '(', ')', '#', ' ', '/']).to_string()
}

/// Crate-qualified variant of [`short_sym`]: `<package>/<short>`, the moniker's `<package>` slot
/// (the crate name, which `short_sym` drops) prefixed onto the short descriptor path —
/// `rust-analyzer cargo hiker-core 0.0.0 trails/` → `hiker-core/trails`. This is the
/// disambiguating body form for `[[code:<repo_id>/<crate>/<short>]]` wikilinks when the short
/// path alone names a symbol in more than one crate (`trails` lives in both `hiker-core` and
/// `hiker-app`). `None` when the moniker has no package slot (or an unknown `.` package) to
/// qualify with. status: code-link-crate-qualified
pub fn crate_qualified_sym(moniker: &str) -> Option<String> {
    let mut parts = moniker.splitn(5, ' ');
    let pkg = parts.nth(2)?;
    let descriptor = parts.nth(1)?;
    if pkg.is_empty() || pkg == "." {
        return None;
    }
    Some(format!("{pkg}/{}", descriptor.trim_end_matches(['.', '(', ')', '#', ' ', '/'])))
}

/// Fold one moniker's short body forms — [`short_sym`] plus the crate-qualified
/// [`crate_qualified_sym`] — into a short→moniker index. A form that names TWO different symbols
/// flips to `None` (AMBIGUOUS): binding an arbitrary winner would navigate (and baseline) the
/// wrong symbol, so consumers refuse instead and the author qualifies the body. The one shared
/// builder for the adapter's `by_short` and reconcile's doc-link index, so the two can't drift
/// on what a body form is. status: code-link-crate-qualified
pub fn index_short_forms(index: &mut HashMap<String, Option<String>>, moniker: &str) {
    let mut add = |key: String| {
        index
            .entry(key)
            .and_modify(|v| {
                if v.as_deref() != Some(moniker) {
                    *v = None; // collision → ambiguous, refuse to pick a winner
                }
            })
            .or_insert_with(|| Some(moniker.to_string()));
    };
    add(short_sym(moniker));
    if let Some(q) = crate_qualified_sym(moniker) {
        add(q);
    }
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

    let descriptor_name = last.map(|d| d.name.clone()).unwrap_or_default();
    let mut name = if !display_name.is_empty() {
        display_name.to_string()
    } else {
        descriptor_name.clone()
    };
    // A crate/package ROOT frequently arrives with a generic name ("Crate") and
    // no real descriptor — its identity actually lives in the SCIP *package*
    // (the crate name). Surface that instead, so the node reads e.g. `my_crate`
    // rather than `Crate`. Falls back to the last descriptor first (in case an
    // indexer puts the crate name there), then the package name.
    if name.is_empty() || name.eq_ignore_ascii_case("crate") {
        let package = parsed
            .as_ref()
            .and_then(|s| s.package.as_ref())
            .map(|p| p.name.clone())
            .unwrap_or_default();
        if !descriptor_name.is_empty() && !descriptor_name.eq_ignore_ascii_case("crate") {
            name = descriptor_name;
        } else if !package.is_empty() {
            name = package;
        }
    }
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

/// Resolve a SCIP document path (repo-relative) under `root`, **refusing** absolute paths and any
/// `..`/root traversal so a malicious or buggy `.scip` can never read outside `root`. A defense-in-
/// depth canonicalize check additionally blocks symlink escapes when both paths exist. Returns
/// `None` to refuse the read. See docs/code.md ("path traversal is the one real `.scip` risk").
fn safe_join(root: &Path, file: &str) -> Option<PathBuf> {
    use std::path::Component;
    let rel = Path::new(file);
    if rel
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::RootDir | Component::Prefix(_)))
    {
        return None;
    }
    let joined = root.join(rel);
    if let (Ok(r), Ok(j)) = (root.canonicalize(), joined.canonicalize()) {
        if !j.starts_with(&r) {
            return None;
        }
    }
    Some(joined)
}

/// A collision-resistant signature of a descriptor chain (name + suffix per descriptor). Matching a
/// symbol's parent = its chain minus the last descriptor against another node's full chain.
fn descriptor_sig(ds: &[scip::types::Descriptor]) -> String {
    let mut s = String::new();
    for d in ds {
        s.push_str(&d.name);
        s.push('\u{1f}');
        s.push_str(&format!("{:?}", d.suffix.enum_value_or_default()));
        s.push('\u{1e}');
    }
    s
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
        // Deterministic resolution: candidate lists in moniker order, not HashMap order, so an
        // ambiguous bare name resolves to the same symbol on every run.
        for v in by_name.values_mut() {
            v.sort();
        }
        let mut by_short: HashMap<String, Option<String>> = HashMap::new();
        for sym in nodes.keys() {
            index_short_forms(&mut by_short, sym);
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
            by_short,
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
    pub fn kind_of(&self, id: &str) -> Option<&str> {
        self.nodes.get(id).map(|nd| nd.kind.as_str())
    }
    /// Targets whose `Code`-resolution fingerprint falls back to line hashing (no bundled grammar
    /// for their file type): `(node_count, distinct_extensions)`. `(0, [])` = full AST coverage.
    pub fn grammar_gaps(&self) -> (usize, Vec<String>) {
        grammar_gap_stats(self.nodes.values().map(|nd| nd.file.as_str()))
    }
    /// One-line warning for consumers to print at load when [`Self::grammar_gaps`] is non-empty.
    /// `None` when every indexed file has an AST grammar.
    pub fn grammar_gap_warning(&self) -> Option<String> {
        let (n, exts) = self.grammar_gaps();
        (n > 0).then(|| {
            format!(
                "[fingerprint] {n} target(s) without AST grammar (ext: {}) — line-hash fallback; comment/format edits will read as drift",
                exts.join(", ")
            )
        })
    }
    /// The definition an annotation at `file:line` refers to — the inverse of [`Self::locate`].
    /// Resolves a `// status: <slug>` comment to the symbol it tags: first the nearest definition
    /// whose identifier starts just below `line` (the annotate-the-thing-below convention, allowing
    /// a few lines for attributes / doc-comments), else the innermost definition whose body encloses
    /// `line` (annotation sitting inside a body). `line` is 0-based. status: spec-seed-from-comments
    pub fn def_at_line(&self, file: &str, line: u32) -> Option<NodeHandle> {
        const WINDOW: u32 = 4;
        // A: closest definition starting at/just below the annotation line.
        let below = self
            .nodes
            .iter()
            .filter(|(_, nd)| nd.file == file && nd.range.sl >= line && nd.range.sl - line <= WINDOW)
            .min_by_key(|(_, nd)| (nd.range.sl - line, nd.range.extent()));
        if let Some((sym, _)) = below {
            return Some(self.handle(sym));
        }
        // B: innermost definition whose body encloses the line.
        let point = Span { sl: line, sc: 0, el: line, ec: 0 };
        self.nodes
            .iter()
            .filter(|(_, nd)| nd.file == file && nd.enclosing.is_some_and(|e| e.contains(&point)))
            .min_by_key(|(_, nd)| nd.enclosing.unwrap().extent())
            .map(|(sym, _)| self.handle(sym))
    }
    /// The repo root a node's `file` (in [`code_graph`](Self::code_graph)) is
    /// relative to — vault-clamped at load. Joining the two yields the on-disk
    /// source path (e.g. to open the file from the graph).
    pub fn repo_root(&self) -> &std::path::Path {
        &self.repo_root
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

        let mut nodes: Vec<GraphNode> = syms
            .iter()
            .map(|s| {
                let nd = &self.nodes[*s];
                GraphNode {
                    id: (*s).clone(),
                    name: nd.name.clone(),
                    kind: nd.kind.clone(),
                    file: nd.file.clone(),
                    start_line: nd.range.sl,
                    lines: nd.enclosing.map_or(1, |e| e.el.saturating_sub(e.sl) + 1),
                    parent: None,
                }
            })
            .collect();

        // Containment: each node's enclosing node, from moniker descriptor nesting. The parent is
        // the symbol with the last descriptor dropped (`Type#method` → `Type#`; `mod/func` → `mod`).
        // Rust impl methods (`impl#[Type][Trait]method`) don't nest under a node, so fall back to the
        // implementing type by name. Used for level-of-detail collapse (members → their object).
        let by_sig: HashMap<String, usize> = syms
            .iter()
            .enumerate()
            .filter_map(|(i, s)| parse_symbol(s).ok().map(|p| (descriptor_sig(&p.descriptors), i)))
            .collect();
        for i in 0..nodes.len() {
            nodes[i].parent = self.parent_of(syms[i], i, &by_sig, &index, &nodes);
        }

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

    /// The containing node of `sym` (index into `nodes`): the symbol with its last descriptor
    /// dropped (moniker nesting), else — for a Rust impl method — the implementing type by name.
    fn parent_of(
        &self,
        sym: &str,
        self_idx: usize,
        by_sig: &HashMap<String, usize>,
        index: &HashMap<&str, usize>,
        nodes: &[GraphNode],
    ) -> Option<usize> {
        let parsed = parse_symbol(sym).ok()?;
        if parsed.descriptors.len() >= 2 {
            let sig = descriptor_sig(&parsed.descriptors[..parsed.descriptors.len() - 1]);
            if let Some(&pi) = by_sig.get(&sig) {
                if pi != self_idx {
                    return Some(pi);
                }
            }
        }
        if let Some((ty, _tr, _m)) = parse_impl_moniker(sym) {
            if let Some(cands) = self.by_name.get(&ty.to_lowercase()) {
                for c in cands {
                    if let Some(&ci) = index.get(c.as_str()) {
                        if ci != self_idx && nodes[ci].kind == "code:type" {
                            return Some(ci);
                        }
                    }
                }
            }
        }
        None
    }

    /// The monikers in `file` whose definition body changed between `head_text` (the file at
    /// HEAD) and the working tree — [`symbol_changed_vs`] batched per file (one parse per
    /// side covers all of the file's nodes). `None` when the refinement can't be trusted (no
    /// AST grammar, unreadable working-tree file, parse failure): the caller keeps the
    /// louder file-grain coloring rather than dimming on a guess.
    /// status: code-graph-diff-symbol-level
    pub fn changed_symbols_vs(&self, file: &str, head_text: &str) -> Option<HashSet<String>> {
        let lang = grammar_for(file)?;
        let path = safe_join(&self.repo_root, file)?;
        let worktree = std::fs::read_to_string(path).ok()?;
        let head = named_def_fingerprints(head_text, &lang)?;
        let work = named_def_fingerprints(&worktree, &lang)?;
        Some(
            self.nodes
                .iter()
                .filter(|(_, nd)| nd.file == file && def_changed(&head, &work, &nd.name))
                .map(|(id, _)| id.clone())
                .collect(),
        )
    }

    fn handle(&self, id: &str) -> NodeHandle {
        NodeHandle { source: self.source.clone(), id: id.to_string() }
    }
    fn read_span(&self, nd: &NodeData) -> Option<String> {
        let span = nd.enclosing.unwrap_or(nd.range);
        // Trust invariant (docs/code.md): a crafted `.scip` must not read outside `repo_root`.
        let path = safe_join(&self.repo_root, &nd.file)?;
        let text = std::fs::read_to_string(path).ok()?;
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
    /// Resolution order (`spec-code-link`): exact short body form — the short descriptor path
    /// (`trails/ops/delete_trail`) or its crate-qualified `<crate>/<short>` spelling
    /// (`hiker-core/trails`, the disambiguator when a short names symbols in >1 crate) — then
    /// bare name, then name substring. Every tier is deterministic (sorted candidates), and an
    /// ambiguous short path falls through to the name tiers rather than picking an arbitrary
    /// winner.
    fn resolve(&self, query: &str, scope: &SourceId) -> Option<NodeHandle> {
        if scope != &self.source {
            return None;
        }
        let trimmed = query.trim().trim_end_matches(['.', '(', ')', '#', ' ', '/']);
        if let Some(Some(sym)) = self.by_short.get(trimmed) {
            return Some(self.handle(sym));
        }
        let q = query.to_lowercase();
        if let Some(v) = self.by_name.get(&q) {
            return v.first().map(|s| self.handle(s));
        }
        self.nodes
            .iter()
            .filter(|(_, nd)| nd.name.to_lowercase().contains(&q))
            .min_by_key(|(s, _)| s.as_str())
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
        self.fingerprint_at(h, Resolution::Code)
    }

    /// Resolution-aware drift fingerprint (`spec-resolution-c4`):
    /// - `Code` — AST-normalized hash of the symbol body (comments/format don't drift).
    /// - `Component` — the enclosing module's member set (kind+name), so only structural change drifts.
    /// - `Container` / `Context` — the crate's symbol surface (names), so only API-set change drifts.
    fn fingerprint_at(&self, h: &NodeHandle, resolution: Resolution) -> Option<Fingerprint> {
        match resolution {
            Resolution::Code => {
                let nd = self.nodes.get(&h.id)?;
                let content = self.read_span(nd)?;
                let norm = grammar_for(&nd.file)
                    .and_then(|lang| ast_normalized(&content, lang))
                    .unwrap_or_else(|| line_normalized(&content));
                Some(Fingerprint(hash_str(&norm)))
            }
            Resolution::Component => {
                // Module = the moniker through its last `/`; hash the (kind, name) set under it.
                let m = &h.id;
                let prefix = &m[..m.rfind('/').map(|i| i + 1).unwrap_or(m.len())];
                let mut members: Vec<String> = self
                    .nodes
                    .iter()
                    .filter(|(id, _)| id.starts_with(prefix))
                    .map(|(_, nd)| format!("{} {}", nd.kind, nd.name))
                    .collect();
                members.sort();
                members.dedup();
                Some(Fingerprint(hash_str(&members.join("\n"))))
            }
            Resolution::Container | Resolution::Context => {
                // Crate = `<scheme> <mgr> <pkg> <ver> `; hash its symbol-name surface.
                let m = &h.id;
                let p: Vec<&str> = m.splitn(5, ' ').collect();
                if p.len() != 5 {
                    return self.fingerprint_at(h, Resolution::Code);
                }
                let prefix = format!("{} {} {} {} ", p[0], p[1], p[2], p[3]);
                let mut surface: Vec<String> = self
                    .nodes
                    .iter()
                    .filter(|(id, _)| id.starts_with(&prefix))
                    .map(|(_, nd)| nd.name.clone())
                    .collect();
                surface.sort();
                surface.dedup();
                Some(Fingerprint(hash_str(&surface.join("\n"))))
            }
        }
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

#[cfg(test)]
mod tests {
    use super::{
        ast_normalized, collapse, crate_qualified_sym, grammar_for, index_short_forms, safe_join,
        short_sym, symbol_changed_vs, ScipAdapter,
    };
    use spec_engine::{DerivedNodeSource, SourceId};
    use std::path::Path;

    /// Crate-qualified body forms (`code-link-crate-qualified`): a short path naming a module in
    /// two crates is ambiguous and refused, but each crate-qualified spelling still binds exactly
    /// its own symbol — the disambiguator `[[code:hiker/hiker-core/trails]]` rides on.
    #[test]
    fn crate_qualified_form_disambiguates_cross_crate_short_collisions() {
        let core = "rust-analyzer cargo hiker-core 0.0.0 trails/";
        let app = "rust-analyzer cargo hiker-app 0.0.0 trails/";
        assert_eq!(short_sym(core), "trails");
        assert_eq!(crate_qualified_sym(core).as_deref(), Some("hiker-core/trails"));
        assert_eq!(crate_qualified_sym(app).as_deref(), Some("hiker-app/trails"));
        // A moniker without the 5-part shape has no package slot to qualify with.
        assert_eq!(crate_qualified_sym("local 12"), None);

        let mut idx = std::collections::HashMap::new();
        index_short_forms(&mut idx, core);
        index_short_forms(&mut idx, app);
        assert_eq!(idx.get("trails"), Some(&None), "bare short collides → ambiguous");
        assert_eq!(idx.get("hiker-core/trails"), Some(&Some(core.to_string())));
        assert_eq!(idx.get("hiker-app/trails"), Some(&Some(app.to_string())));
        // Re-indexing the same moniker is idempotent — no self-collision.
        index_short_forms(&mut idx, core);
        assert_eq!(idx.get("hiker-core/trails"), Some(&Some(core.to_string())));
    }

    /// The wikilink→code-view path (`spec-code-link`): doc links carry SHORT DESCRIPTOR PATHS
    /// (`trails/ops/delete_trail`), and `resolve` must accept them — bare-name-only resolution
    /// shipped a "code symbol not found" toast for every authored doc link. Round-trip property:
    /// every unambiguous short path resolves back to exactly its own moniker.
    #[test]
    fn resolve_round_trips_short_descriptor_paths_on_pyproj() {
        let idx = Path::new("../fixtures/pyproj.scip");
        if !idx.exists() {
            return;
        }
        let src = SourceId("pyproj".into());
        let a = ScipAdapter::load(idx, Path::new("../fixtures/pyproj"), src.clone())
            .expect("load pyproj.scip");
        let ids: Vec<String> = a.entities().map(|(id, _, _)| id.clone()).collect();
        let mut shorts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for id in &ids {
            *shorts.entry(short_sym(id)).or_default() += 1;
        }
        let mut checked = 0;
        for id in &ids {
            let s = short_sym(id);
            if shorts[&s] > 1 {
                continue; // ambiguous shorts fall through to name resolution — not round-trippable
            }
            let h = a.resolve(&s, &src).unwrap_or_else(|| panic!("short path must resolve: {s}"));
            assert_eq!(&h.id, id, "short path resolves to its own moniker");
            checked += 1;
        }
        assert!(checked >= 5, "fixture exercised {checked} round-trips");
    }

    /// Bare names still resolve (the code-cli / autocomplete path), deterministically: an
    /// ambiguous name (`area` on Shape/Circle/Square) picks the lexicographically smallest
    /// moniker on every run — never HashMap order.
    #[test]
    fn resolve_bare_names_deterministically_on_pyproj() {
        let idx = Path::new("../fixtures/pyproj.scip");
        if !idx.exists() {
            return;
        }
        let src = SourceId("pyproj".into());
        let a = ScipAdapter::load(idx, Path::new("../fixtures/pyproj"), src.clone())
            .expect("load pyproj.scip");
        let first = a.resolve("area", &src).expect("ambiguous bare name resolves");
        for _ in 0..10 {
            assert_eq!(a.resolve("area", &src).unwrap().id, first.id, "stable across calls");
        }
        let all_area: Vec<String> = a
            .entities()
            .filter(|(_, _, name)| *name == "area")
            .map(|(id, _, _)| id.clone())
            .collect();
        assert!(all_area.len() > 1, "fixture has ambiguous `area`");
        assert_eq!(Some(&first.id), all_area.iter().min(), "smallest moniker wins");
    }

    /// Same contract against the committed hiker index, with the exact body shapes the seeded
    /// docs use: plain paths, `impl#[Type]method` forms, and test-function paths.
    #[test]
    fn resolve_doc_link_bodies_on_hiker_fixture() {
        let idx = Path::new("../fixtures/hiker.scip");
        if !idx.exists() {
            return;
        }
        let src = SourceId("hiker".into());
        let a = ScipAdapter::load(idx, Path::new("../.."), src.clone()).expect("load hiker.scip");
        for body in [
            "trails/ops/delete_trail",                // implements:: form
            "oplog/impl#[OpLog]ensure_loaded_in",     // impl-method form
            "trails/tests/parse/parse_trail_doc_round_trip", // verifies:: (test fn) form
        ] {
            let h = a
                .resolve(body, &src)
                .unwrap_or_else(|| panic!("doc-link body must resolve: {body}"));
            assert_eq!(short_sym(&h.id), body, "resolved the named symbol, not a name-alike");
        }
        // The crate-qualified form resolves to exactly the named crate's symbol — the
        // disambiguator for shorts that collide across crates (`code-link-crate-qualified`).
        let h = a
            .resolve("hiker-core/trails", &src)
            .expect("crate-qualified body must resolve");
        assert_eq!(crate_qualified_sym(&h.id).as_deref(), Some("hiker-core/trails"));
        assert_eq!(short_sym(&h.id), "trails");
    }

    fn fp(file: &str, src: &str) -> String {
        ast_normalized(src, grammar_for(file).expect("grammar")).expect("parse")
    }

    #[test]
    fn rust_fingerprint_ignores_comments_and_format_but_not_logic() {
        let base = fp("a.rs", "fn f(x: u32) -> u32 {\n    x + 1\n}\n");
        let commented = fp("a.rs", "/// docs\nfn f(x: u32) -> u32 {\n    // note\n    x + 1\n}\n");
        let reformatted = fp("a.rs", "fn f(x: u32) -> u32 { x + 1 }\n");
        let literal_edit = fp("a.rs", "fn f(x: u32) -> u32 {\n    x + 2\n}\n");
        let op_edit = fp("a.rs", "fn f(x: u32) -> u32 {\n    x - 1\n}\n");
        assert_eq!(base, commented, "comment edits must not drift");
        assert_eq!(base, reformatted, "formatting must not drift");
        assert_ne!(base, literal_edit, "literal change must drift");
        assert_ne!(base, op_edit, "operator change must drift");
    }

    #[test]
    fn python_fingerprint_ignores_comments_but_not_literals() {
        // Regression: under the old Rust-only grammar + identifier/literal kind allowlist,
        // Python literal edits did NOT change the hash (Python numbers are `integer`, not
        // `*_literal`) — a silent drift false negative.
        let base = fp("a.py", "def f(x):\n    return x + 1\n");
        let commented = fp("a.py", "def f(x):\n    # note\n    return x + 1\n");
        let literal_edit = fp("a.py", "def f(x):\n    return x + 2\n");
        let renamed = fp("a.py", "def f(y):\n    return y + 1\n");
        assert_eq!(base, commented, "comment edits must not drift");
        assert_ne!(base, literal_edit, "literal change must drift");
        assert_ne!(base, renamed, "identifier change must drift");
    }

    /// Symbol-grain diff classification (`code-graph-diff-symbol-level`): a body edit flags
    /// exactly the edited symbol; comment + formatting churn flags nothing.
    #[test]
    fn symbol_changed_vs_flags_body_edits_not_formatting() {
        let head = "fn alpha(x: u32) -> u32 {\n    x + 1\n}\n\nfn beta() -> u32 {\n    7\n}\n";
        let edited = "fn alpha(x: u32) -> u32 {\n    x * 2\n}\n\nfn beta() -> u32 {\n    7\n}\n";
        assert!(symbol_changed_vs(head, edited, "a.rs", "alpha"), "body edit flags");
        assert!(!symbol_changed_vs(head, edited, "a.rs", "beta"), "untouched sibling stays clean");
        let formatted =
            "/// docs\nfn alpha(x: u32) -> u32 { x + 1 }\n\n// note\nfn beta() -> u32 { 7 }\n";
        assert!(!symbol_changed_vs(head, formatted, "a.rs", "alpha"), "re-wrap must not flag");
        assert!(!symbol_changed_vs(head, formatted, "a.rs", "beta"), "comments must not flag");
    }

    /// The misattribution caveat: index spans are index-time, so a line-shifted symbol must
    /// not misread. The HEAD side is located by NAME, never span — a pure move (new code
    /// above) leaves the moved body unflagged; only the symbol absent at HEAD flags.
    #[test]
    fn symbol_changed_vs_is_span_free_so_pure_moves_do_not_misattribute() {
        let head = "fn alpha(x: u32) -> u32 {\n    x + 1\n}\n";
        let moved = "fn gamma() -> u32 {\n    9\n}\n\nfn alpha(x: u32) -> u32 {\n    x + 1\n}\n";
        assert!(!symbol_changed_vs(head, moved, "a.rs", "alpha"), "pure line move must not flag");
        assert!(symbol_changed_vs(head, moved, "a.rs", "gamma"), "absent at HEAD → changed");
    }

    /// Failure direction is over-flag, never silently dim: same-name namesakes where any one
    /// changed flag the whole name; a renamed symbol flags; a grammarless file stays at file
    /// grain (everything reads changed).
    #[test]
    fn symbol_changed_vs_overflags_whenever_unprovable() {
        let head = "impl A { fn go(&self) -> u32 { 1 } }\nimpl B { fn go(&self) -> u32 { 2 } }\n";
        let one = "impl A { fn go(&self) -> u32 { 1 } }\nimpl B { fn go(&self) -> u32 { 3 } }\n";
        assert!(symbol_changed_vs(head, one, "a.rs", "go"), "ambiguous namesakes over-flag");
        assert!(!symbol_changed_vs(head, head, "a.rs", "go"), "identical namesakes → clean");
        assert!(symbol_changed_vs(head, "fn went() -> u32 { 1 }\n", "a.rs", "go"), "renamed");
        assert!(symbol_changed_vs("f()", "f()", "a.go", "f"), "no grammar → file grain");
    }

    /// Containers fingerprint as their whole definition node: editing a method flags the
    /// method AND its class (the class body includes it) — Python side of the walk.
    #[test]
    fn symbol_changed_vs_python_containers_track_their_members() {
        let head = "class C:\n    def m(self):\n        return 1\n";
        let work = "class C:\n    def m(self):\n        return 2\n";
        assert!(symbol_changed_vs(head, work, "a.py", "m"), "edited method flags");
        assert!(symbol_changed_vs(head, work, "a.py", "C"), "container body includes members");
        assert!(!symbol_changed_vs(head, head, "a.py", "C"), "identical class → clean");
    }

    #[test]
    fn unknown_languages_get_no_grammar() {
        assert!(grammar_for("a.go").is_none());
        assert!(grammar_for("noext").is_none());
        assert!(grammar_for("a.rs").is_some());
        assert!(grammar_for("a.py").is_some());
    }

    #[test]
    fn grammar_gap_stats_counts_ungrammared_nodes() {
        let (n, exts) = super::grammar_gap_stats(["a.rs", "b.go", "c.go", "Makefile", "d.py"]);
        assert_eq!(n, 3, "two .go files + one extensionless");
        assert_eq!(exts, ["<none>", "go"]);
        let (n, exts) = super::grammar_gap_stats(["a.rs", "b.py"]);
        assert_eq!((n, exts.len()), (0, 0), "full coverage is silent");
    }

    /// Containment + collapse on the scip-python pyproj fixture (skipped if the gitignored `.scip`
    /// isn't present). Each `area` method should nest under its class; collapsing to objects-only
    /// must drop members yet keep the structural edges.
    #[test]
    fn containment_and_collapse_on_pyproj() {
        let idx = Path::new("../fixtures/pyproj.scip");
        if !idx.exists() {
            return;
        }
        let a = ScipAdapter::load(idx, Path::new("../fixtures/pyproj"), SourceId("pyproj".into()))
            .expect("load pyproj.scip");
        assert!(a.grammar_gap_warning().is_none(), "all-Python fixture has full AST coverage");
        let g = a.code_graph();
        assert!(g.nodes.iter().filter(|n| n.is_object()).count() >= 3, "Shape/Circle/Square");

        let areas: Vec<_> =
            g.nodes.iter().filter(|n| n.name == "area" && n.kind == "code:method").collect();
        assert!(!areas.is_empty(), "expected `area` methods");
        for m in &areas {
            let p = m.parent.expect("area method has a containing object");
            assert_eq!(g.nodes[p].kind, "code:type", "area nests under its class");
        }

        let c = collapse(&g, |i| g.nodes[i].is_object());
        assert!(c.nodes.len() < g.nodes.len(), "objects-only collapses the graph");
        assert!(c.nodes.iter().all(|&i| g.nodes[i].is_object()), "only objects survive");
    }

    #[test]
    fn safe_join_allows_in_repo_paths() {
        let root = Path::new("/repo");
        assert_eq!(safe_join(root, "src/main.rs"), Some(Path::new("/repo/src/main.rs").to_path_buf()));
        assert_eq!(safe_join(root, "a/b/c.py"), Some(Path::new("/repo/a/b/c.py").to_path_buf()));
    }

    #[test]
    fn safe_join_refuses_traversal_and_absolute() {
        let root = Path::new("/repo");
        assert_eq!(safe_join(root, "../etc/passwd"), None);
        assert_eq!(safe_join(root, "a/../../b"), None);
        assert_eq!(safe_join(root, "/etc/passwd"), None);
        assert_eq!(safe_join(root, "a/../b"), None); // conservative: any `..` refused
    }
}
