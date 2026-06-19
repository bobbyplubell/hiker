//! Diagnostic: what does a `.scip` actually cover? Prints, per top-level directory, the document
//! count, and per crate package (from rust-analyzer monikers `rust-analyzer cargo <pkg> <ver>`) the
//! symbol count — so we can see which workspace members made it into the index.
//! Run: `cargo run -p hiker-code --example scip_coverage -- <index.scip>`

use std::collections::BTreeMap;

use protobuf::Message;
use scip::types::Index;

fn main() {
    let path = std::env::args().nth(1).expect("usage: scip_coverage <index.scip>");
    let bytes = std::fs::read(&path).expect("read index");
    let index = Index::parse_from_bytes(&bytes).expect("parse scip");

    // Docs per top-level directory.
    let mut dirs: BTreeMap<String, usize> = BTreeMap::new();
    for doc in &index.documents {
        let top = doc.relative_path.split('/').next().unwrap_or("").to_string();
        *dirs.entry(top).or_default() += 1;
    }
    println!("documents: {}  | top-level dirs:", index.documents.len());
    for (d, n) in &dirs {
        println!("  {n:5}  {d}");
    }

    // Symbols per cargo package (parsed from the moniker's 3rd token).
    let mut pkgs: BTreeMap<String, usize> = BTreeMap::new();
    for doc in &index.documents {
        for si in &doc.symbols {
            // moniker: "<scheme> <manager> <package> <version> <descriptors...>"
            let pkg = si
                .symbol
                .split(' ')
                .nth(2)
                .filter(|_| si.symbol.starts_with("rust-analyzer cargo "))
                .unwrap_or("<non-cargo>")
                .to_string();
            *pkgs.entry(pkg).or_default() += 1;
        }
    }
    println!("\ncrate packages (by in-doc symbol count):");
    let mut v: Vec<_> = pkgs.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    for (p, n) in &v {
        println!("  {n:6}  {p}");
    }
    println!("\n{} distinct packages", v.len());
}
