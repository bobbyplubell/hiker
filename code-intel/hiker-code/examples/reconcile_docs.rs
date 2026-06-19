//! Reconcile inline `implements::/touches::/verifies:: [[code:hiker/<sym>]]` links authored in docs
//! into the link store (markdown-owns-edges → store baseline). Each link associates with the nearest
//! preceding `[slug]` anchor. Resolves `<sym>` (a short descriptor path) back to a SCIP moniker, then
//! merges on top of the comment-seeded store (dedup). status: spec-reconcile-on-save
//!
//! Bug rows are the second authoring surface (`tracker-relation-links`): a `bug_tracking.md` table
//! row whose notes carry `manifests-in::`/`verifies-fix::` fields binds those edges to the row's
//! own backticked slug — the row IS the anchor, struck (resolved) rows included, since a struck
//! row's `verifies-fix` baseline is the regression watch (`tracker-regression-watch`).
//!
//! Prune (the other half of markdown-owns-edges): store edges no longer claimed by any doc link
//! line OR `// status:` comment are STALE — deleting a doc line should delete the edge, or drift
//! reports slowly fill with zombies. Stale edges are listed every run; `--prune` drops them.
//!
//! Malformed-anchor lint: bracket tokens that LOOK like anchors but silently mis-associate link
//! lines — `[slug-a, slug-b]` comma lists (skipped by the anchor pick, so links bind to the
//! previous anchor) and prose slug tokens preceding the real anchor on its line (the first wins)
//! — warn with file:line when a link line actually binds under them. Warn-only, never fails.
//! Run: cargo run -p hiker-code --example reconcile_docs -- <scip> <repo_root> <docs_dir> [--prune]

use std::collections::{HashMap, HashSet};
use std::path::Path;

use hiker_code::governance::{
    bracket_tokens, bug_row, bug_row_links, code_link_bodies, is_slug, slug_in_line, walk_md,
};
use hiker_code::{comment_seeds, crate_qualified_sym, index_short_forms, short_sym, ScipAdapter};
use spec_engine::{AddOutcome, DerivedNodeSource, Link, LinkStore, NodeHandle, Resolution, SourceId};

/// Where `// status:` comment tags live: the prune keep-set must union both authoring surfaces
/// (doc link lines + comment tags), or pruning on doc links alone deletes every comment-claimed
/// edge.
const SCOPE: &[&str] = &["core", "app"];

/// Composite keep-set key — `(spec, relation, target)` joined unambiguously.
fn key(spec: &str, rel: &str, target: &str) -> String {
    format!("{spec}\u{1f}{rel}\u{1f}{target}")
}

/// `resolution: container` in the doc's `---` frontmatter block, if declared. This is the spec's
/// own altitude dial (`spec-resolution-c4`): it applies to the doc's `touches` links through the
/// relation floor (`Resolution::for_relation`); `implements`/`verifies` stay `Code` regardless —
/// so a doc can't coarsen the relations that carry the guarantees by twisting this knob.
fn declared_resolution(text: &str) -> Option<Resolution> {
    let mut lines = text.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    for line in lines {
        let t = line.trim();
        if t == "---" {
            break;
        }
        if let Some(v) = t.strip_prefix("resolution:") {
            return Some(Resolution::parse(v));
        }
    }
    None
}

/// A bracket token that LOOKS like an anchor list but isn't one: `[slug-a, slug-b]` — every
/// comma-separated half slug-shaped. `slug_in_line` skips it (the comma fails the charset), so
/// link lines below silently bind to the PREVIOUS anchor — the malformed-anchor class that has
/// shipped real wrong edges.
fn comma_anchor_token(line: &str) -> Option<String> {
    bracket_tokens(line).into_iter().find_map(|t| {
        let parts: Vec<&str> = t.split(',').collect();
        (parts.len() > 1 && parts.iter().all(|p| is_slug(p.trim()))).then(|| t.to_string())
    })
}

/// Where the binding anchor came from, for the malformed-anchor lint: which line defined it,
/// whether that line carried MORE slug-shaped tokens than the one that won (a prose bracket token
/// preceding the real anchor — the first token wins association), and any comma-list token
/// (`[slug-a, slug-b]`) skipped since. Warnings fire lazily, only when a link line actually
/// associates under the suspect anchor — a comma list or prose token with no link lines below it
/// mis-associates nothing.
#[derive(Default)]
struct AnchorLint {
    anchor_line: usize,
    extra_slugs: Vec<String>,
    multi_warned: bool,
    skipped_comma: Option<(usize, String)>,
}

impl AnchorLint {
    /// A new `[slug]` anchor bound on `lineno`; remember its sibling slug tokens for the lint.
    fn on_anchor(&mut self, lineno: usize, line: &str, slug: &str) {
        self.anchor_line = lineno;
        self.extra_slugs = bracket_tokens(line)
            .into_iter()
            .filter(|t| is_slug(t) && *t != slug)
            .map(str::to_string)
            .collect();
        self.multi_warned = false;
        self.skipped_comma = None;
    }

    /// A non-anchor line scanned; note any comma-list token a later link line would bind past.
    fn on_other_line(&mut self, lineno: usize, line: &str) {
        if self.skipped_comma.is_none() {
            if let Some(tok) = comma_anchor_token(line) {
                self.skipped_comma = Some((lineno, tok));
            }
        }
    }

    /// A link line just associated with `slug`; warn (file:line) if the association is suspect.
    fn on_link_line(&mut self, file: &Path, lineno: usize, slug: &str) {
        if let Some((cl, tok)) = &self.skipped_comma {
            eprintln!(
                "[anchor-lint] {}:{lineno}: link line binds to [{slug}] past the comma bracket token `[{tok}]` on line {cl} — not an anchor (split it into per-slug `[slug]` anchors)",
                file.display()
            );
        }
        if !self.multi_warned && !self.extra_slugs.is_empty() {
            self.multi_warned = true;
            eprintln!(
                "[anchor-lint] {}:{}: anchor line carries {} slug-shaped bracket tokens — the FIRST ([{slug}]) wins association; if [{}] was meant, move it to its own entry",
                file.display(),
                self.anchor_line,
                self.extra_slugs.len() + 1,
                self.extra_slugs.join("], ["),
            );
        }
    }
}

/// `(implements|touches|verifies):: [[code:hiker/A]], [[code:hiker/B]]` → (relation, [A, B]).
/// The bug-row relations don't appear here: they are authored inside bug rows only
/// ([`bug_row_links`]), never as anchored link lines.
fn link_line(line: &str) -> Option<(&'static str, Vec<String>)> {
    // Tolerate HTML-comment-wrapped dataview lines (`<!-- implements:: … -->`), an alt convention.
    let t = line.trim_start().trim_start_matches("<!--").trim_start();
    let rel = ["implements", "touches", "verifies"].into_iter().find(|r| {
        t.starts_with(r) && t[r.len()..].trim_start().starts_with("::")
    })?;
    let bodies = code_link_bodies(t);
    (!bodies.is_empty()).then_some((rel, bodies))
}

/// Mutable reconcile state threaded through every link binding — the store, the
/// prune keep-sets, and the outcome counters — so the two authoring surfaces
/// (anchored link lines, bug rows) share one resolve/upsert path.
struct Reconcile<'a> {
    by_short: &'a HashMap<String, Option<String>>,
    ad: &'a ScipAdapter,
    src: &'a SourceId,
    store: LinkStore,
    keep_full: HashSet<String>,
    keep_short: HashSet<String>,
    added: usize,
    rescoped: usize,
    unresolved: usize,
    ambiguous: usize,
    refused: usize,
}

impl Reconcile<'_> {
    /// Resolve each short `body` and upsert a `(spec, rel)` edge at the relation's
    /// floored resolution. Resolved bodies key the keep-set by full moniker;
    /// unresolved/ambiguous bodies key by their short path, so an edge whose
    /// symbol fell out of the index keeps its store entry and stays MISSING in
    /// drift — that's the user's signal to act, not staleness to collect.
    fn bind(&mut self, spec: &str, rel: &str, bodies: Vec<String>, declared: Option<Resolution>) {
        for body in bodies {
            match self.by_short.get(&body) {
                Some(Some(moniker)) => {
                    self.keep_full.insert(key(spec, rel, moniker));
                    let h = NodeHandle { source: self.src.clone(), id: moniker.clone() };
                    let res = Resolution::for_relation(rel, declared);
                    match self.store.add_link(spec, rel, &h, res, self.ad) {
                        AddOutcome::Added => self.added += 1,
                        AddOutcome::Existing => {}
                        AddOutcome::Rescoped => self.rescoped += 1,
                        AddOutcome::NoFingerprint => self.refused += 1,
                    }
                }
                Some(None) => {
                    self.keep_short.insert(key(spec, rel, &body));
                    self.ambiguous += 1;
                    eprintln!("[ambiguous] {spec} {rel}:: [[code:hiker/{body}]] — short name matches >1 symbol; qualify it");
                }
                None => {
                    self.keep_short.insert(key(spec, rel, &body));
                    self.unresolved += 1;
                    eprintln!("[unresolved] {spec} {rel}:: [[code:hiker/{body}]] — no such symbol in the index");
                }
            }
        }
    }
}

fn main() {
    let scip = std::env::args().nth(1).expect("scip");
    let repo = std::env::args().nth(2).expect("repo_root");
    let docs = std::env::args().nth(3).expect("docs_dir");
    let do_prune = std::env::args().any(|a| a == "--prune");
    let src = SourceId("hiker".into());
    let ad = ScipAdapter::load(Path::new(&scip), Path::new(&repo), src.clone()).expect("load");
    if let Some(w) = ad.grammar_gap_warning() {
        eprintln!("{w}");
    }

    // Short body form (descriptor path OR crate-qualified `<crate>/<short>`) -> full moniker; a
    // form that names TWO symbols is AMBIGUOUS and refused (binding an arbitrary winner would
    // baseline the wrong symbol — and HashMap order made the winner differ per run, so
    // re-reconciling kept flip-flopping new edges in). Shared builder with the adapter's own
    // `by_short` (`index_short_forms`), so authored bodies and `resolve` agree.
    let mut by_short: HashMap<String, Option<String>> = HashMap::new();
    for (id, _, _) in ad.entities() {
        index_short_forms(&mut by_short, id);
    }

    // The store lives in the REPO (durable, committable). Merge is preserving: existing edges keep
    // their verified-at fingerprint, so reconciling never resets drift (`code-cli ack` does that).
    let store_path = Path::new(&repo).join("links.json");
    let mut rec = Reconcile {
        by_short: &by_short,
        ad: &ad,
        src: &src,
        store: LinkStore::load(&store_path).expect("load link store"),
        keep_full: HashSet::new(),
        keep_short: HashSet::new(),
        added: 0,
        rescoped: 0,
        unresolved: 0,
        ambiguous: 0,
        refused: 0,
    };

    let mut md = Vec::new();
    walk_md(Path::new(&docs), &mut md);
    let mut unreadable_docs = 0usize;
    for f in md {
        let Ok(text) = std::fs::read_to_string(&f) else {
            unreadable_docs += 1;
            continue;
        };
        let declared = declared_resolution(&text);
        let mut slug: Option<String> = None;
        let mut lint = AnchorLint::default();
        for (lineno, line) in text.lines().enumerate() {
            let lineno = lineno + 1;
            if let Some(row) = bug_row(line) {
                // A bug row is its own one-line anchor scope: the slug in its
                // first cell binds the row's `manifests-in::`/`verifies-fix::`
                // fields, struck rows included (a resolved bug's `verifies-fix`
                // IS the regression watch). Rows never participate in
                // [slug]-anchor association. status: tracker-relation-links
                for (rel, bodies) in bug_row_links(line) {
                    rec.bind(&row.slug, rel, bodies, declared);
                }
            } else if let Some((rel, bodies)) = link_line(line) {
                let Some(spec) = &slug else { continue };
                lint.on_link_line(&f, lineno, spec);
                rec.bind(spec, rel, bodies, declared);
            } else if let Some(s) = slug_in_line(line) {
                lint.on_anchor(lineno, line, &s);
                slug = Some(s);
            } else {
                lint.on_other_line(lineno, line);
            }
        }
    }
    let Reconcile {
        mut store,
        mut keep_full,
        keep_short,
        added,
        rescoped,
        unresolved,
        ambiguous,
        refused,
        ..
    } = rec;

    // Union the comment-seeded edges (`// status:` markers) into the keep-set — they are authored
    // claims too, just authored in source instead of docs.
    let crawl = comment_seeds(&ad, Path::new(&repo), SCOPE);
    for s in &crawl.seeds {
        keep_full.insert(key(&s.slug, s.relation, &s.handle.id));
    }
    // File-level keep, staleness-proofing the seed tier: an index older than the working tree
    // stops resolving markers in edited files (shifted lines), which would turn their live edges
    // into prune candidates. A marker SEEN in a file keeps every edge of its slug whose target
    // locates to that same file — deleting the marker (or the symbol leaving the file) still
    // makes the edge prunable.
    let marker_files: HashSet<(String, String)> = crawl.markers.into_iter().collect();
    let kept = |l: &Link| {
        keep_full.contains(&key(&l.spec, &l.relation, &l.target))
            || keep_short.contains(&key(&l.spec, &l.relation, &short_sym(&l.target)))
            || crate_qualified_sym(&l.target)
                .is_some_and(|q| keep_short.contains(&key(&l.spec, &l.relation, &q)))
            || ad
                .locate(&NodeHandle { source: src.clone(), id: l.target.clone() })
                .is_some_and(|loc| marker_files.contains(&(l.spec.clone(), loc.file)))
    };
    let stale: Vec<String> = store
        .links
        .iter()
        .filter(|l| l.source == src.0 && !kept(l))
        .map(|l| format!("  [stale] {} {}:: {}", l.spec, l.relation, short_sym(&l.target)))
        .collect();
    let mut pruned = 0usize;
    if stale.is_empty() {
        // nothing to prune
    } else if unreadable_docs > 0 {
        // A doc that failed to READ is not a doc whose links were deleted — pruning on a partial
        // walk would silently drop its edges. Refuse.
        eprintln!(
            "[prune] {unreadable_docs} doc(s) unreadable — refusing to prune on a partial walk ({} stale candidate(s) kept)",
            stale.len()
        );
    } else if do_prune {
        pruned = store.prune(&src, &kept);
    } else {
        eprintln!("[prune] {} stale edge(s) no doc link or status comment claims:", stale.len());
        for s in &stale {
            eprintln!("{s}");
        }
        eprintln!("[prune] dry-run — re-run with --prune to drop them");
    }
    store.save(&store_path).expect("save");
    println!(
        "reconciled doc links: +{added} added, {rescoped} rescoped, {pruned} pruned, {unresolved} unresolved, {ambiguous} ambiguous, {refused} refused (no fingerprint) → {}",
        store_path.display()
    );
    println!("store now {} links", store.links.len());
}
