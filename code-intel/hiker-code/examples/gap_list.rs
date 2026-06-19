//! Per-subsystem gap list for the coverage fan-out, C4-aware (`spec-resolution-c4`). Three buckets,
//! each a different action:
//!  1. UNGOVERNED objects — no spec link on self, ancestors, or members: needs a spec (or a coarse
//!     `touches::` from the doc owning the area). Ranked by fan-in (in-degree), then size: a small
//!     type many things depend on outranks a big leaf.
//!  2. MEMBERS-ONLY containers — some members carry Code-level links but the container itself is
//!     unlinked: add ONE Component-level `touches:: [[code:…]]` on the container in its governing
//!     doc, and sibling members become governed + member-set drift fires on shape changes.
//!  3. PROMOTION candidates — high fan-in ungoverned functions/methods inside otherwise governed
//!     territory: the spec prose probably should name these directly (Code-level link).
//! Every row carries a ready-to-paste `[[code:hiker/<short>]]` body so an agent never guesses a
//! SCIP descriptor.
//! Run: cargo run -p hiker-code --example gap_list -- <scip> <repo_root> <subsystem e.g. core/trails> [limit]

use std::collections::HashSet;
use std::path::Path;

use hiker_code::{short_sym, ScipAdapter};
use spec_engine::{LinkStore, SourceId};

fn main() {
    let scip = std::env::args().nth(1).expect("scip");
    let repo = std::env::args().nth(2).expect("repo");
    let sub = std::env::args().nth(3).expect("subsystem e.g. core/trails");
    let limit: usize = std::env::args().nth(4).and_then(|s| s.parse().ok()).unwrap_or(40);
    // "core/trails" -> "core/src/trails"
    let file_prefix = sub.replacen('/', "/src/", 1);

    let ad = ScipAdapter::load(Path::new(&scip), Path::new(&repo), SourceId("hiker".into()))
        .expect("load");
    let g = ad.code_graph();
    let store_path = Path::new(&repo).join("links.json"); // durable baseline lives in the repo
    let store = LinkStore::load(&store_path).expect("load link store");
    let targets: HashSet<&str> = store.links.iter().map(|l| l.target.as_str()).collect();
    let n = g.nodes.len();
    let is_target = |i: usize| targets.contains(g.nodes[i].id.as_str());

    // governed = self or an ancestor (module/type) is a link target.
    let governed = |mut i: usize| -> bool {
        loop {
            if is_target(i) {
                return true;
            }
            match g.nodes[i].parent {
                Some(p) => i = p,
                None => return false,
            }
        }
    };

    // member-linked = some descendant is a link target (propagate up from each target); also count
    // linked members per container for the members-only report.
    let mut member_linked = vec![false; n];
    let mut linked_members = vec![0usize; n];
    for i in 0..n {
        if is_target(i) {
            let mut cur = g.nodes[i].parent;
            while let Some(j) = cur {
                member_linked[j] = true;
                linked_members[j] += 1;
                cur = g.nodes[j].parent;
            }
        }
    }

    // fan-in = in-degree over the whole repo graph (who depends on this).
    let mut fan_in = vec![0usize; n];
    for &(_, b, _) in &g.edges {
        fan_in[b] += 1;
    }

    let in_sub = |i: usize| g.nodes[i].file.starts_with(&file_prefix);

    // 1. ungoverned objects — need a spec.
    let mut ungoverned: Vec<usize> = (0..n)
        .filter(|&i| in_sub(i) && g.nodes[i].is_object() && !governed(i) && !member_linked[i])
        .collect();
    ungoverned.sort_by(|&a, &b| fan_in[b].cmp(&fan_in[a]).then(g.nodes[b].lines.cmp(&g.nodes[a].lines)));
    println!("# {sub} — UNGOVERNED objects ({} total, showing {}) — need a spec", ungoverned.len(), ungoverned.len().min(limit));
    println!("# fan-in | kind | name | file:line | ready link");
    for &i in ungoverned.iter().take(limit) {
        let nd = &g.nodes[i];
        println!(
            "  {:4}  {:8} {:28} {}:{}  ({} ln)  [[code:hiker/{}]]",
            fan_in[i], nd.kind.trim_start_matches("code:"), nd.name, nd.file, nd.start_line + 1, nd.lines, short_sym(&nd.id)
        );
    }

    // 2. members-only containers — add a Component `touches::` on the container.
    let mut members_only: Vec<usize> = (0..n)
        .filter(|&i| in_sub(i) && g.nodes[i].is_object() && !governed(i) && member_linked[i])
        .collect();
    members_only.sort_by(|&a, &b| linked_members[b].cmp(&linked_members[a]));
    println!(
        "\n# {sub} — MEMBERS-ONLY containers ({}) — members pinned, container unlinked: add `touches:: [[code:…]]` on the container",
        members_only.len()
    );
    for &i in members_only.iter().take(limit) {
        let nd = &g.nodes[i];
        println!(
            "  {:3} linked member(s)  {:8} {:28} {}:{}  [[code:hiker/{}]]",
            linked_members[i], nd.kind.trim_start_matches("code:"), nd.name, nd.file, nd.start_line + 1, short_sym(&nd.id)
        );
    }

    // 3. promotion candidates — high fan-in ungoverned functions/methods (spec should name them).
    let mut promote: Vec<usize> = (0..n)
        .filter(|&i| {
            in_sub(i)
                && matches!(g.nodes[i].kind.as_str(), "code:function" | "code:method")
                && !governed(i)
                && fan_in[i] > 0
        })
        .collect();
    promote.sort_by(|&a, &b| fan_in[b].cmp(&fan_in[a]));
    println!("\n# {sub} — PROMOTION candidates (top {}) — high fan-in, no direct link", 10.min(promote.len()));
    for &i in promote.iter().take(10) {
        let nd = &g.nodes[i];
        println!(
            "  {:4}  {:8} {:28} {}:{}  [[code:hiker/{}]]",
            fan_in[i], nd.kind.trim_start_matches("code:"), nd.name, nd.file, nd.start_line + 1, short_sym(&nd.id)
        );
    }
}
