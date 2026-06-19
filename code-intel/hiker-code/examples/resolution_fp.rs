//! C4-resolution drift: fingerprint a target at each C4 level and show which levels a change trips.
//! `Code` hashes the symbol body; `Component` hashes its module's member set; `Container` hashes the
//! crate's public symbol surface. So a body edit drifts `Code` but not `Component`/`Container`; a
//! structural change (member added/removed) drifts `Component` up. status: spec-resolution-c4
//! Run: cargo run -p hiker-code --example resolution_fp -- <before.scip> <after.scip> <repo> <moniker-substr>

use std::collections::hash_map::DefaultHasher;
use std::collections::BTreeSet;
use std::hash::{Hash, Hasher};
use std::path::Path;

use hiker_code::ScipAdapter;
use spec_engine::{DerivedNodeSource, NodeHandle, SourceId};

#[derive(Clone, Copy, Debug)]
enum Res {
    Code,
    Component,
    Container,
}

fn h(s: &str) -> String {
    let mut x = DefaultHasher::new();
    s.hash(&mut x);
    format!("{:016x}", x.finish())
}

/// `…hiker-core 0.0.0 trails/ops/delete_trail().` → `…hiker-core 0.0.0 trails/ops/` (through last `/`).
fn module_prefix(m: &str) -> &str {
    &m[..m.rfind('/').map(|i| i + 1).unwrap_or(m.len())]
}

/// `rust-analyzer cargo hiker-core 0.0.0 …` → `rust-analyzer cargo hiker-core 0.0.0 ` (the crate).
fn container_prefix(m: &str) -> String {
    let p: Vec<&str> = m.splitn(5, ' ').collect();
    if p.len() == 5 {
        format!("{} {} {} {} ", p[0], p[1], p[2], p[3])
    } else {
        String::new()
    }
}

fn fp_at(ad: &ScipAdapter, ents: &[(String, String, String)], moniker: &str, r: Res) -> String {
    match r {
        // Body hash (the symbol's enclosing def) — the existing precise fingerprint.
        Res::Code => ad
            .fingerprint(&NodeHandle { source: SourceId("hiker".into()), id: moniker.into() })
            .map(|f| f.0)
            .unwrap_or_default(),
        // Structure: the set of (kind, name) sharing the module prefix — invariant to body edits.
        Res::Component => {
            let p = module_prefix(moniker);
            let set: BTreeSet<String> = ents
                .iter()
                .filter(|(id, ..)| id.starts_with(p))
                .map(|(_, k, n)| format!("{k} {n}"))
                .collect();
            h(&set.into_iter().collect::<Vec<_>>().join("\n"))
        }
        // API surface: the set of symbol names in the crate.
        Res::Container => {
            let p = container_prefix(moniker);
            let set: BTreeSet<String> =
                ents.iter().filter(|(id, ..)| id.starts_with(&p)).map(|(_, _, n)| n.clone()).collect();
            h(&set.into_iter().collect::<Vec<_>>().join("\n"))
        }
    }
}

fn ents(ad: &ScipAdapter) -> Vec<(String, String, String)> {
    ad.entities().map(|(i, k, n)| (i.clone(), k.to_string(), n.to_string())).collect()
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let (before, after, repo, query) = (&a[0], &a[1], &a[2], &a[3]);
    let src = SourceId("hiker".into());
    let ad_b = ScipAdapter::load(Path::new(before), Path::new(repo), src.clone()).expect("before");
    let ad_a = ScipAdapter::load(Path::new(after), Path::new(repo), src).expect("after");
    let (eb, ea) = (ents(&ad_b), ents(&ad_a));

    let moniker = eb
        .iter()
        .find(|(id, ..)| id.contains(query.as_str()))
        .map(|(id, ..)| id.clone())
        .unwrap_or_else(|| panic!("no symbol matching {query}"));
    println!("target: {moniker}\n");
    println!("{:12}  {:18}  {:18}  {}", "C4 level", "before", "after", "drift?");
    for r in [Res::Code, Res::Component, Res::Container] {
        let (fb, fa) = (fp_at(&ad_b, &eb, &moniker, r), fp_at(&ad_a, &ea, &moniker, r));
        let mark = if fb != fa { "DRIFTED" } else { "ok" };
        println!("{:12}  {fb:18}  {fa:18}  {mark}", format!("{r:?}"));
    }
}
