//! Smoke-probe for `ScipAdapter::def_at_line`: resolve a `file:line` (1-based, as grep reports)
//! to the symbol an annotation there tags.
//! Run: `cargo run -p hiker-code --example probe_def -- <index.scip> <repo_root> <file> <line1> ...`

use std::path::Path;

use hiker_code::ScipAdapter;
use spec_engine::{DerivedNodeSource, SourceId};

fn main() {
    let mut a = std::env::args().skip(1);
    let scip = a.next().expect("scip");
    let repo = a.next().expect("repo");
    let file = a.next().expect("file");
    let src = SourceId("hiker".into());
    let ad = ScipAdapter::load(Path::new(&scip), Path::new(&repo), src).expect("load");
    for l1 in a {
        let line1: u32 = l1.parse().expect("line");
        let line0 = line1.saturating_sub(1);
        match ad.def_at_line(&file, line0) {
            Some(h) => {
                let loc = ad.locate(&h).map(|l| format!("{}:{}", l.file, l.start_line + 1));
                println!(
                    "{file}:{line1} -> name={:?}  def@{}  [{}]",
                    ad.name_of(&h.id),
                    loc.unwrap_or_default(),
                    h.id
                );
            }
            None => println!("{file}:{line1} -> <none>"),
        }
    }
}
