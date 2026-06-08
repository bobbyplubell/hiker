//! Exploratory loader: dump real stats from a `.scip` index to validate the data shape
//! before writing the adapter. Run: `cargo run -p hiker-code --example dump_scip -- <index.scip>`

use std::collections::BTreeMap;

use protobuf::Message;
use scip::symbol::parse_symbol;
use scip::types::Index;

const DEFINITION: i32 = 0x1; // SymbolRole::Definition

fn main() {
    let path = std::env::args().nth(1).expect("usage: dump_scip <index.scip>");
    let bytes = std::fs::read(&path).expect("read index file");
    let index = Index::parse_from_bytes(&bytes).expect("parse scip index");

    let mut docs = 0usize;
    let mut docs_with_text = 0usize;
    let mut total_symbols = 0usize;
    let mut total_occ = 0usize;
    let mut defs = 0usize;
    let mut refs = 0usize;
    let mut with_enclosing = 0usize;
    let mut impl_rels = 0usize;
    let mut type_rels = 0usize;
    let mut suffix_hist: BTreeMap<String, usize> = BTreeMap::new();

    for doc in &index.documents {
        docs += 1;
        if !doc.text.is_empty() {
            docs_with_text += 1;
        }
        total_symbols += doc.symbols.len();
        for si in &doc.symbols {
            for r in &si.relationships {
                if r.is_implementation {
                    impl_rels += 1;
                }
                if r.is_type_definition {
                    type_rels += 1;
                }
            }
            if let Ok(sym) = parse_symbol(&si.symbol) {
                if let Some(last) = sym.descriptors.last() {
                    let suf = format!("{:?}", last.suffix.enum_value_or_default());
                    *suffix_hist.entry(suf).or_default() += 1;
                }
            }
        }
        for occ in &doc.occurrences {
            total_occ += 1;
            if occ.symbol_roles & DEFINITION != 0 {
                defs += 1;
            } else {
                refs += 1;
            }
            if !occ.enclosing_range.is_empty() {
                with_enclosing += 1;
            }
        }
    }

    println!("metadata: tool={:?}", index.metadata.tool_info.name);
    println!("documents: {docs}  (with embedded text: {docs_with_text})");
    println!(
        "symbols (in-doc): {total_symbols}   external_symbols: {}",
        index.external_symbols.len()
    );
    println!("occurrences: {total_occ}  defs: {defs}  refs: {refs}  with_enclosing_range: {with_enclosing}");
    println!("relationships: implementation={impl_rels}  type_definition={type_rels}");
    println!("descriptor suffix histogram (last descriptor): {suffix_hist:?}");

    println!("\n--- sample non-local definitions (path range  enclosing  name  symbol) ---");
    let mut shown = 0;
    'outer: for doc in &index.documents {
        let names: BTreeMap<&str, &str> = doc
            .symbols
            .iter()
            .map(|s| (s.symbol.as_str(), s.display_name.as_str()))
            .collect();
        for occ in &doc.occurrences {
            if occ.symbol_roles & DEFINITION != 0 && !occ.symbol.starts_with("local ") {
                let dn = names.get(occ.symbol.as_str()).copied().unwrap_or("");
                println!(
                    "{}  range={:?}  encl={:?}  name='{}'  sym='{}'",
                    doc.relative_path, occ.range, occ.enclosing_range, dn, occ.symbol
                );
                shown += 1;
                if shown >= 10 {
                    break 'outer;
                }
            }
        }
    }
}
