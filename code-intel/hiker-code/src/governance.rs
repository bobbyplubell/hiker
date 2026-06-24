//! Spec-governance rollup for code-graph consumers (`code-graph-governance-overlay`):
//! load the repo-root `links.json` drift baseline, run `check_drift` over every linked
//! moniker, and fold the per-link reports into one per-symbol [`GovState`] — plus the
//! doc-side `status::` field scan (the same `[slug]`-anchor association reconcile's
//! link lines use, so the two can't disagree on what an anchor is), the
//! spec-lighting target/blast-radius sets (`code-graph-spec-lighting`,
//! `code-graph-status-badge`), and the bug-row scan + open-bugs rollup over
//! `manifests-in` edges (`tracker-relation-links`, `code-graph-bug-badge`).
//! Pure data: no egui, no rendering policy — the code graph panel maps these
//! states to fills/badges.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use spec_engine::{DerivedNodeSource, EdgeKind, LinkStore, NodeHandle, SourceId};

/// Anchor-token shape: lowercase/digit/dash, at least one dash. Shared with
/// reconcile (`examples/reconcile_docs.rs`) so doc scanning here and link
/// reconciling there agree on what an anchor is.
pub fn is_slug(tok: &str) -> bool {
    !tok.is_empty()
        && tok.contains('-')
        && tok.chars().all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
}

/// Every `[…]` bracket token in `line`, in order — the raw material the anchor pick
/// (and reconcile's malformed-anchor lint) reads. A `[[spec:…]]`/`[[code:…]]` wikilink
/// yields a token still carrying its `[`/`:` characters, which fails [`is_slug`] — so
/// wikilinks never masquerade as anchors.
pub fn bracket_tokens(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(o) = rest.find('[') {
        let after = &rest[o + 1..];
        let Some(c) = after.find(']') else { break };
        out.push(&after[..c]);
        rest = &after[c + 1..];
    }
    out
}

/// `[some-slug]` anchor token in a line (lowercase/digit/dash, must contain a dash), if any.
pub fn slug_in_line(line: &str) -> Option<String> {
    bracket_tokens(line).into_iter().find(|t| is_slug(t)).map(str::to_string)
}

/// Collect every `.md` under `root` (recursive), in directory-walk order.
pub fn walk_md(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(root) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk_md(&p, out);
        } else if p.extension().is_some_and(|x| x == "md") {
            out.push(p);
        }
    }
}

/// `status::` value per `[slug]` anchor across every `.md` under `docs_dir`. Same
/// nearest-preceding-anchor association as reconcile's link lines; the FIRST
/// `status::` line after an anchor wins (an anchor has one status — later ones in
/// the same entry would be malformed). A `status::` before any anchor binds to
/// nothing and is dropped. status: code-graph-status-badge
pub fn doc_statuses(docs_dir: &Path) -> HashMap<String, String> {
    let mut md = Vec::new();
    walk_md(docs_dir, &mut md);
    let mut out = HashMap::new();
    for f in md {
        let Ok(text) = std::fs::read_to_string(&f) else { continue };
        let mut slug: Option<String> = None;
        // Whether the current anchor has already taken its status:: line.
        let mut bound = true;
        for line in text.lines() {
            if let Some(s) = slug_in_line(line) {
                slug = Some(s);
                bound = false;
            } else if let Some(v) = line.trim().strip_prefix("status::") {
                if let (Some(s), false) = (&slug, bound) {
                    out.insert(s.clone(), v.trim().to_string());
                    bound = true;
                }
            }
        }
    }
    out
}

/// Whether a spec `status::` value flags its governed code with the "spec not fully
/// landed" badge: `planned` (code may predate the spec's claims) and `partial` (some
/// claims unbuilt). `done`/`draft`/`superseded`/`removed` don't badge — drift, not
/// status, is their signal.
pub fn status_flagged(status: &str) -> bool {
    matches!(status, "planned" | "partial")
}

/// The bug-row relations (`tracker-relation-links`): where a bug manifests, and the
/// regression test vouching for its fix. Authored inline on `bug_tracking.md` table
/// rows (the row IS the bug's whole entry — no `[slug]` anchor line); the relation
/// floor pins both at `Code` resolution, the same body-level claim as `verifies`.
pub const BUG_RELATIONS: [&str; 2] = ["manifests-in", "verifies-fix"];

/// A `bug_tracking.md` table row's identity: the backticked `bug-…` slug in its
/// first cell, and whether the row is struck (`~~`slug`~~` — resolved in place).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BugRow {
    pub slug: String,
    pub struck: bool,
}

/// Parse a bug-tracker table row: a `|`-led line whose FIRST cell carries a
/// backticked `bug-…` slug, optionally `~~`-struck. Header/divider rows, rows
/// whose first cell isn't a bug slug, and non-table lines are `None`.
/// status: tracker-relation-links
pub fn bug_row(line: &str) -> Option<BugRow> {
    let cell = line.trim().strip_prefix('|')?.split('|').next()?.trim();
    let struck = cell.starts_with("~~");
    let inner = cell.trim_start_matches('~').strip_prefix('`')?;
    let slug = &inner[..inner.find('`')?];
    (is_slug(slug) && slug.starts_with("bug-")).then(|| BugRow { slug: slug.to_string(), struck })
}

/// Every `[[code:hiker/<body>]]` body in `text`, in order — one parser shared by
/// reconcile's anchored link lines and the bug rows' inline link fields, so the
/// two authoring surfaces can't disagree on what a code link is.
pub fn code_link_bodies(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("[[code:hiker/") {
        let after = &rest[i + "[[code:hiker/".len()..];
        let Some(c) = after.find("]]") else { break };
        out.push(after[..c].trim().to_string());
        rest = &after[c + 2..];
    }
    out
}

/// The `manifests-in::`/`verifies-fix::` link fields on one bug-row line. A row is
/// a single line, so the fields sit mid-line in the Notes cell: each `rel::` token
/// claims the code links between it and the next token (or end of line). Bodies
/// before any token belong to no field and are ignored. status: tracker-relation-links
pub fn bug_row_links(line: &str) -> Vec<(&'static str, Vec<String>)> {
    let mut marks: Vec<(usize, &'static str)> = Vec::new();
    for rel in BUG_RELATIONS {
        let pat = format!("{rel}::");
        let mut from = 0;
        while let Some(i) = line[from..].find(&pat) {
            marks.push((from + i, rel));
            from += i + pat.len();
        }
    }
    marks.sort_unstable_by_key(|&(pos, _)| pos);
    let mut out = Vec::new();
    for (k, &(pos, rel)) in marks.iter().enumerate() {
        let end = marks.get(k + 1).map_or(line.len(), |&(p, _)| p);
        let bodies = code_link_bodies(&line[pos..end]);
        if !bodies.is_empty() {
            out.push((rel, bodies));
        }
    }
    out
}

/// The struck (resolved-in-place) bug slugs across every `.md` under `docs_dir` —
/// the open/closed signal for the open-bugs rollup: a `manifests-in` edge whose
/// row is struck stops counting as open (its regression watch lives on in drift).
/// A slug absent from every row reads as OPEN — a stale edge stays loud until
/// reconcile prunes it, instead of silently dropping off the badge.
/// status: code-graph-bug-badge
pub fn struck_bug_rows(docs_dir: &Path) -> HashSet<String> {
    let mut md = Vec::new();
    walk_md(docs_dir, &mut md);
    let mut out = HashSet::new();
    for f in md {
        let Ok(text) = std::fs::read_to_string(&f) else { continue };
        for line in text.lines() {
            if let Some(row) = bug_row(line) {
                if row.struck {
                    out.insert(row.slug);
                }
            }
        }
    }
    out
}

/// Folded governance state of one symbol. Variant order = severity: when a symbol
/// carries several links, the worst report wins (`Missing` > `Drifted` > `Ok`);
/// a symbol with no links at all is `Ungoverned`.
/// status: code-graph-governance-overlay
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum GovState {
    /// No spec link governs this symbol.
    Ungoverned,
    /// Linked and every baseline fingerprint is current.
    Ok,
    /// Linked and at least one target changed since its baseline.
    Drifted,
    /// Linked but at least one link can't fingerprint the symbol any more
    /// (gone from the source, or its file is unreadable).
    Missing,
}

/// Fold one symbol's per-link drift outcomes `(drifted, missing)` into a [`GovState`]
/// by severity; an empty iterator is `Ungoverned`.
pub fn classify(reports: impl IntoIterator<Item = (bool, bool)>) -> GovState {
    reports
        .into_iter()
        .map(|(drifted, missing)| match (drifted, missing) {
            (_, true) => GovState::Missing,
            (true, false) => GovState::Drifted,
            (false, false) => GovState::Ok,
        })
        .max()
        .unwrap_or(GovState::Ungoverned)
}

/// The edge kinds that count as blast radius for spec lighting: every code relation
/// the adapter derives (calls / type refs / imports / impls).
const BLAST_KINDS: [EdgeKind; 4] =
    [EdgeKind::Calls, EdgeKind::TypeRef, EdgeKind::Imports, EdgeKind::Implements];

/// The spec-governance rollup over one source's `links.json`: per-moniker drift
/// state, the governing specs per moniker, each spec's lighting targets, and the
/// doc `status::` values. Built once per view (drift checking reads + parses every
/// linked body), then queried per frame.
pub struct Governance {
    /// Folded drift state per linked moniker; a moniker absent here is ungoverned.
    states: HashMap<String, GovState>,
    /// Governing spec slugs per moniker (sorted, deduped) — any spec relation.
    /// Bug edges ([`BUG_RELATIONS`]) are deliberately excluded: bug slugs aren't
    /// lightable specs and don't belong in "Light spec" menus; they surface
    /// through the [`Self::open_bugs_of`] channel instead.
    specs: HashMap<String, Vec<String>>,
    /// `implements`/`touches` target monikers per spec (sorted) — the lighting
    /// roots. `verifies` targets (tests) are deliberately excluded: lighting shows
    /// where a spec lives, not what vouches for it.
    targets: BTreeMap<String, Vec<String>>,
    /// `status::` value per spec slug, from [`doc_statuses`].
    statuses: HashMap<String, String>,
    /// Open bug slugs per moniker (sorted, deduped): `manifests-in` edges whose
    /// bug row isn't struck. status: code-graph-bug-badge
    bugs: HashMap<String, Vec<String>>,
}

impl Governance {
    /// Build the rollup from a loaded store: one `check_drift` pass folded per
    /// target, plus the link-derived spec/target/bug indexes. `struck_bugs` is the
    /// resolved-row scan from [`struck_bug_rows`].
    pub fn build(
        store: &LinkStore,
        source: &SourceId,
        provider: &dyn DerivedNodeSource,
        statuses: HashMap<String, String>,
        struck_bugs: HashSet<String>,
    ) -> Self {
        let mut per_target: HashMap<String, Vec<(bool, bool)>> = HashMap::new();
        for r in store.check_drift(source, provider) {
            per_target.entry(r.target).or_default().push((r.drifted, r.missing));
        }
        let states = per_target.into_iter().map(|(t, v)| (t, classify(v))).collect();
        let mut specs: HashMap<String, Vec<String>> = HashMap::new();
        let mut targets: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut bugs: HashMap<String, Vec<String>> = HashMap::new();
        for l in store.links.iter().filter(|l| l.source == source.0) {
            if BUG_RELATIONS.contains(&l.relation.as_str()) {
                // Bug edges feed the open-bugs rollup, not the spec channels; a
                // struck row's manifestation is no longer open (its verifies-fix
                // regression watch still rides drift like any other link).
                if l.relation == "manifests-in" && !struck_bugs.contains(&l.spec) {
                    bugs.entry(l.target.clone()).or_default().push(l.spec.clone());
                }
                continue;
            }
            specs.entry(l.target.clone()).or_default().push(l.spec.clone());
            if matches!(l.relation.as_str(), "implements" | "touches") {
                targets.entry(l.spec.clone()).or_default().push(l.target.clone());
            }
        }
        for v in specs.values_mut().chain(targets.values_mut()).chain(bugs.values_mut()) {
            v.sort();
            v.dedup();
        }
        Self { states, specs, targets, statuses, bugs }
    }

    /// Load the repo-root `links.json` + the docs' `status::` fields and struck
    /// bug rows, and build the rollup. A missing `links.json` loads as an empty
    /// store (everything ungoverned) — the panel gates the overlay on the file's
    /// presence instead.
    pub fn load(
        repo_root: &Path,
        docs_dir: &Path,
        source: &SourceId,
        provider: &dyn DerivedNodeSource,
    ) -> std::io::Result<Self> {
        let store = LinkStore::load(&repo_root.join("links.json"))?;
        Ok(Self::build(&store, source, provider, doc_statuses(docs_dir), struck_bug_rows(docs_dir)))
    }

    /// The folded drift state of `moniker` (`Ungoverned` when no link names it).
    pub fn state_of(&self, moniker: &str) -> GovState {
        self.states.get(moniker).copied().unwrap_or(GovState::Ungoverned)
    }

    /// The spec slugs governing `moniker` (any relation), sorted; empty when ungoverned.
    pub fn specs_of(&self, moniker: &str) -> &[String] {
        self.specs.get(moniker).map_or(&[], Vec::as_slice)
    }

    /// The lightable specs (those with `implements`/`touches` targets), sorted.
    pub fn specs(&self) -> impl Iterator<Item = &String> {
        self.targets.keys()
    }

    /// The `implements`/`touches` code-target monikers of `spec` (sorted) — the
    /// spec→code edges the spec graph draws. Empty when `spec` has no targets.
    /// Adapter-free (folded at build), so the spec graph renders before the
    /// SCIP adapter binds. status: spec-graph-source
    pub fn targets_of(&self, spec: &str) -> &[String] {
        self.targets.get(spec).map_or(&[], Vec::as_slice)
    }

    /// The doc `status::` value of `spec`, if its anchor carries one.
    pub fn status_of(&self, spec: &str) -> Option<&str> {
        self.statuses.get(spec).map(String::as_str)
    }

    /// Whether any spec governing `moniker` is `status:: planned`/`partial` —
    /// the badge predicate.
    pub fn flagged(&self, moniker: &str) -> bool {
        self.specs_of(moniker)
            .iter()
            .any(|s| self.status_of(s).is_some_and(status_flagged))
    }

    /// The open bug slugs manifesting in `moniker` (non-struck rows with a
    /// `manifests-in` edge to it), sorted; empty when no open bug names it.
    /// status: code-graph-bug-badge
    pub fn open_bugs_of(&self, moniker: &str) -> &[String] {
        self.bugs.get(moniker).map_or(&[], Vec::as_slice)
    }

    /// The monikers `spec` lights up: its `implements`/`touches` targets plus their
    /// 1-hop blast radius via the provider's `neighbors` (all code edge kinds).
    /// status: code-graph-spec-lighting
    pub fn lighting(
        &self,
        spec: &str,
        provider: &dyn DerivedNodeSource,
        source: &SourceId,
    ) -> HashSet<String> {
        let mut out = HashSet::new();
        for t in self.targets.get(spec).map_or(&[][..], Vec::as_slice) {
            out.insert(t.clone());
            let h = NodeHandle { source: source.clone(), id: t.clone() };
            for n in provider.neighbors(&h, &BLAST_KINDS) {
                out.insert(n.id);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use spec_engine::{
        DerivedNodeSource, EdgeKind, Fingerprint, LinkStore, NodeHandle, Resolution, SourceCaps,
        SourceId, SourceLoc,
    };

    use super::{
        bug_row, bug_row_links, classify, doc_statuses, status_flagged, struck_bug_rows, BugRow,
        GovState, Governance,
    };

    /// Provider with fixed fingerprints + a symmetric neighbor map — enough to
    /// drive `check_drift` and `lighting` without a real index.
    struct Mock {
        fps: HashMap<String, Option<String>>,
        nbrs: HashMap<String, Vec<String>>,
    }

    impl Mock {
        fn new(fps: &[(&str, Option<&str>)], nbrs: &[(&str, &[&str])]) -> Self {
            Mock {
                fps: fps.iter().map(|(k, v)| (k.to_string(), v.map(String::from))).collect(),
                nbrs: nbrs
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.iter().map(ToString::to_string).collect()))
                    .collect(),
            }
        }
    }

    impl DerivedNodeSource for Mock {
        fn resolve(&self, _q: &str, _s: &SourceId) -> Option<NodeHandle> {
            None
        }
        fn locate(&self, _h: &NodeHandle) -> Option<SourceLoc> {
            None
        }
        fn content(&self, _h: &NodeHandle) -> Option<String> {
            None
        }
        fn fingerprint(&self, h: &NodeHandle) -> Option<Fingerprint> {
            self.fps.get(&h.id).cloned().flatten().map(Fingerprint)
        }
        fn neighbors(&self, h: &NodeHandle, _k: &[EdgeKind]) -> Vec<NodeHandle> {
            self.nbrs
                .get(&h.id)
                .map_or(&[][..], Vec::as_slice)
                .iter()
                .map(|n| NodeHandle { source: h.source.clone(), id: n.clone() })
                .collect()
        }
        fn capabilities(&self) -> SourceCaps {
            SourceCaps::default()
        }
    }

    fn src() -> SourceId {
        SourceId("src".into())
    }

    fn handle(id: &str) -> NodeHandle {
        NodeHandle { source: src(), id: id.into() }
    }

    /// Severity fold: missing beats drifted beats ok; nothing at all is ungoverned.
    #[test]
    fn classify_folds_by_severity() {
        assert_eq!(classify([]), GovState::Ungoverned);
        assert_eq!(classify([(false, false)]), GovState::Ok);
        assert_eq!(classify([(false, false), (true, false)]), GovState::Drifted);
        assert_eq!(classify([(true, false), (false, true)]), GovState::Missing);
        assert_eq!(classify([(false, true), (false, false)]), GovState::Missing);
    }

    /// Build folds multi-link targets through drift, keeps unlinked monikers
    /// ungoverned, and indexes governing specs per target.
    #[test]
    fn build_rolls_up_drift_per_target() {
        let mock = Mock::new(&[("ok", Some("v1")), ("hot", Some("v1")), ("gone", Some("v1"))], &[]);
        let mut store = LinkStore::default();
        store.add_link("s-ok", "implements", &handle("ok"), Resolution::Code, &mock);
        store.add_link("s-hot", "implements", &handle("hot"), Resolution::Code, &mock);
        store.add_link("s-hot2", "touches", &handle("hot"), Resolution::Code, &mock);
        store.add_link("s-gone", "implements", &handle("gone"), Resolution::Code, &mock);

        // "hot" changed, "gone" vanished; "ok" stayed.
        let mock = Mock::new(&[("ok", Some("v1")), ("hot", Some("v2")), ("gone", None)], &[]);
        let gov = Governance::build(&store, &src(), &mock, HashMap::new(), HashSet::new());
        assert_eq!(gov.state_of("ok"), GovState::Ok);
        assert_eq!(gov.state_of("hot"), GovState::Drifted);
        assert_eq!(gov.state_of("gone"), GovState::Missing);
        assert_eq!(gov.state_of("never-linked"), GovState::Ungoverned);
        assert_eq!(gov.specs_of("hot"), ["s-hot", "s-hot2"], "sorted governing specs");
    }

    /// Lighting = the spec's implements/touches targets plus their neighbor blast
    /// radius; verifies links never seed it.
    #[test]
    fn lighting_is_targets_plus_blast_radius() {
        let mock = Mock::new(
            &[("a", Some("v")), ("t", Some("v"))],
            &[("a", &["b", "c"][..])],
        );
        let mut store = LinkStore::default();
        store.add_link("spec-x", "implements", &handle("a"), Resolution::Code, &mock);
        store.add_link("spec-x", "verifies", &handle("t"), Resolution::Code, &mock);
        let gov = Governance::build(&store, &src(), &mock, HashMap::new(), HashSet::new());

        let lit = gov.lighting("spec-x", &mock, &src());
        assert!(lit.contains("a") && lit.contains("b") && lit.contains("c"));
        assert!(!lit.contains("t"), "verifies target is not a lighting root");
        assert!(gov.lighting("no-such-spec", &mock, &src()).is_empty());
        assert_eq!(gov.specs().collect::<Vec<_>>(), ["spec-x"], "only lightable specs listed");
    }

    /// Badge predicate: planned/partial statuses flag every moniker the spec
    /// governs; done (or no status) doesn't.
    #[test]
    fn flagged_follows_planned_and_partial_statuses() {
        assert!(status_flagged("planned") && status_flagged("partial"));
        assert!(!status_flagged("done") && !status_flagged("draft") && !status_flagged(""));

        let mock = Mock::new(&[("a", Some("v")), ("b", Some("v"))], &[]);
        let mut store = LinkStore::default();
        store.add_link("spec-p", "implements", &handle("a"), Resolution::Code, &mock);
        store.add_link("spec-d", "implements", &handle("b"), Resolution::Code, &mock);
        let statuses = HashMap::from([
            ("spec-p".to_string(), "partial".to_string()),
            ("spec-d".to_string(), "done".to_string()),
        ]);
        let gov = Governance::build(&store, &src(), &mock, statuses, HashSet::new());
        assert!(gov.flagged("a"), "partial spec badges its target");
        assert!(!gov.flagged("b"), "done spec doesn't");
        assert!(!gov.flagged("unlinked"));
    }

    /// The doc scan binds each `status::` to the nearest preceding `[slug]` anchor
    /// (first status wins), ignores pre-anchor statuses, and never mistakes a
    /// wikilink for an anchor.
    #[test]
    fn doc_statuses_bind_to_nearest_anchor() {
        let dir = std::env::temp_dir().join("hiker-code-doc-statuses-test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("spec.md"),
            "status:: orphan\n\
             - First feature. [feat-one]\n\
             status:: done\n\
             status:: planned\n\
             - See [[spec:feat-one]] for context. [feat-two]\n\
             status:: partial\n\
             note:: prose\n",
        )
        .unwrap();
        let statuses = doc_statuses(&dir);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(statuses.get("feat-one").map(String::as_str), Some("done"), "first wins");
        assert_eq!(statuses.get("feat-two").map(String::as_str), Some("partial"));
        assert!(!statuses.contains_key("orphan"), "pre-anchor status dropped");
        assert_eq!(statuses.len(), 2);
    }

    /// Bug-row identity: backticked `bug-…` first cell (prose suffix tolerated),
    /// `~~` strike detected; headers, dividers, non-bug slugs, and prose are not rows.
    #[test]
    fn bug_row_parses_slug_and_strike() {
        let open = "| `bug-canvas-save` (remainder) | `app/src/x.rs` | notes |";
        assert_eq!(
            bug_row(open),
            Some(BugRow { slug: "bug-canvas-save".into(), struck: false })
        );
        let struck = "| ~~`bug-rename-focus`~~ | n/a | **RESOLVED** |";
        assert_eq!(bug_row(struck), Some(BugRow { slug: "bug-rename-focus".into(), struck: true }));
        assert_eq!(bug_row("| Slug | File | Notes |"), None, "header");
        assert_eq!(bug_row("| ---- | ---- | ----- |"), None, "divider");
        assert_eq!(bug_row("| `sys-op-log` | x | y |"), None, "non-bug slug");
        assert_eq!(bug_row("prose with `bug-foo-bar` mentioned"), None, "not a table row");
    }

    /// One row line carries both fields: each `rel::` token claims the code links
    /// up to the next token; bodies before any token (and non-code links) bind to
    /// nothing.
    #[test]
    fn bug_row_links_split_relations_on_one_line() {
        let line = "| ~~`bug-x-y`~~ | f | fixed (see [[spec:other-thing]], [[code:hiker/stray]]) \
                    manifests-in:: [[code:hiker/mod/f]], [[code:hiker/mod/g]] · \
                    verifies-fix:: [[code:hiker/mod/tests/t]] |";
        let links = bug_row_links(line);
        assert_eq!(
            links,
            vec![
                ("manifests-in", vec!["mod/f".to_string(), "mod/g".to_string()]),
                ("verifies-fix", vec!["mod/tests/t".to_string()]),
            ],
            "stray pre-field body ignored; bodies split at the next field token"
        );
        assert!(bug_row_links("| `bug-plain` | f | no fields here |").is_empty());
    }

    /// The struck scan: only `~~`-struck rows land in the set.
    #[test]
    fn struck_bug_rows_collects_only_struck_slugs() {
        let dir = std::env::temp_dir().join("hiker-code-struck-bug-rows-test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("bug_tracking.md"),
            "| Slug | File | Notes |\n\
             | ---- | ---- | ----- |\n\
             | `bug-still-open` | x | notes |\n\
             | ~~`bug-fixed-one`~~ | n/a | RESOLVED |\n",
        )
        .unwrap();
        let struck = struck_bug_rows(&dir);
        std::fs::remove_dir_all(&dir).ok();
        assert!(struck.contains("bug-fixed-one"));
        assert!(!struck.contains("bug-still-open"));
        assert_eq!(struck.len(), 1);
    }

    /// The open-bugs rollup: non-struck `manifests-in` edges count per target;
    /// struck rows don't; `verifies-fix` never manifests; bug slugs stay out of
    /// the spec channels (`specs_of`).
    #[test]
    fn open_bugs_rollup_counts_non_struck_manifests_in() {
        let mock = Mock::new(&[("a", Some("v")), ("t", Some("v"))], &[]);
        let mut store = LinkStore::default();
        store.add_link("spec-x", "implements", &handle("a"), Resolution::Code, &mock);
        store.add_link("bug-open-one", "manifests-in", &handle("a"), Resolution::Code, &mock);
        store.add_link("bug-fixed-one", "manifests-in", &handle("a"), Resolution::Code, &mock);
        store.add_link("bug-fixed-one", "verifies-fix", &handle("t"), Resolution::Code, &mock);
        let struck = HashSet::from(["bug-fixed-one".to_string()]);
        let gov = Governance::build(&store, &src(), &mock, HashMap::new(), struck);

        assert_eq!(gov.open_bugs_of("a"), ["bug-open-one"], "struck row doesn't count");
        assert!(gov.open_bugs_of("t").is_empty(), "verifies-fix isn't a manifestation");
        assert_eq!(gov.specs_of("a"), ["spec-x"], "bug slugs stay out of the spec channel");
        assert_eq!(gov.state_of("t"), GovState::Ok, "bug edges still govern drift state");
    }
}
