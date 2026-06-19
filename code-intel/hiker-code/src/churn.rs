//! Churn-vs-drift silence report (`code-cli-churn-vs-drift`) — the third leg of the
//! audit-the-silence posture (`spec-resolution-c4`): compare CODE CHURN (commits touching each
//! governed region over a rev window) against the DRIFT SIGNAL the link store raises, to expose
//! silently under-watched code. Two smells: a file with high churn and **no governing spec at
//! all** (silence), and a spec whose targets churned but whose links report zero drift
//! (governed-but-blind — usually a coarse altitude: a `Context`/`Container` touches link over a
//! hot file never fires on body edits). `BLIND(Code)` is the weak form: the file churned around
//! the pinned body without changing it (or drift was already acked) — worth a glance, not alarm.
//!
//! Pure data: the git window comes from `hiker-git`'s [`GitBackend`], the mapping rides the
//! [`CodeGraph`]'s containment (a link target governs its whole subtree's files, the same
//! propagate-down rule as the coverage report), and drift is the store's current `check_drift`
//! against stored baselines — historical, per-commit drift is out of scope.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use hiker_git::repo::{GitBackend, Libgit2Backend};
use hiker_git::GitError;
use spec_engine::{DerivedNodeSource, LinkStore, Resolution, SourceId};

use crate::CodeGraph;

/// One window commit and the repo-relative paths it touched (vs its first parent).
#[derive(Debug, Clone)]
pub struct CommitChurn {
    pub sha: String,
    pub subject: String,
    pub paths: Vec<String>,
}

/// One spec's churn-vs-drift row.
#[derive(Debug, Clone)]
pub struct SpecChurn {
    pub spec: String,
    /// Window commits touching any file under the spec's link targets.
    pub commits: usize,
    /// Drift events *expected*: links whose governed files churned in the window.
    pub expected: usize,
    /// Drift events *observed*: of those churned links, how many read DRIFTED/MISSING now.
    pub observed: usize,
    /// The finest altitude among the churned links — the dial that explains a silence:
    /// `observed == 0` with a `Context`/`Container`/`Component` altitude is governed-but-blind.
    pub altitude: Option<Resolution>,
}

impl SpecChurn {
    /// Governed-but-blind: the spec's targets churned but no watch fired.
    pub fn blind(&self) -> bool {
        self.expected > 0 && self.observed == 0
    }
}

/// An ungoverned file's churn row: the file is in the code index but no spec governs it.
#[derive(Debug, Clone)]
pub struct FileChurn {
    pub file: String,
    pub commits: usize,
}

/// The full report: per-spec rows and the ungoverned-file section, both sorted by churn
/// (then name, for determinism).
#[derive(Debug)]
pub struct ChurnReport {
    /// Window size actually analyzed (commits found, capped at the request).
    pub commits: usize,
    pub specs: Vec<SpecChurn>,
    pub ungoverned: Vec<FileChurn>,
    /// Distinct changed paths outside the code index (docs, config, lockfiles) — counted so
    /// the report says what it ignored instead of silently dropping it.
    pub unindexed: usize,
}

/// Collect the churn window from `backend`: the last `commits` commits (newest first), each
/// with its changed paths vs its **first parent**. A root commit has no parent to diff against
/// and contributes no churn (it is creation, not change); any other git failure propagates.
pub fn collect_window(
    backend: &dyn GitBackend,
    commits: usize,
) -> hiker_git::Result<Vec<CommitChurn>> {
    let mut out = Vec::new();
    for c in backend.log(commits)? {
        let paths = match backend.diff_paths(&format!("{}^", c.sha), Some(&c.sha)) {
            Ok(v) => v.into_iter().map(|(p, _)| p).collect(),
            // `sha^` didn't resolve: the root commit. Everything else is a real read error.
            Err(GitError::InvalidPath(_)) => Vec::new(),
            Err(e) => return Err(e),
        };
        out.push(CommitChurn { sha: c.sha, subject: c.subject, paths });
    }
    Ok(out)
}

/// [`collect_window`] over the repo at `repo_root`. Refuses a non-repo directory up front —
/// the backend's `open_or_init` would otherwise *create* one, a write this read-only report
/// must never perform.
pub fn churn_window(repo_root: &Path, commits: usize) -> hiker_git::Result<Vec<CommitChurn>> {
    if !repo_root.join(".git").exists() {
        return Err(GitError::Open(format!("{} is not a git repository", repo_root.display())));
    }
    let backend = Libgit2Backend::open_or_init(repo_root)?;
    collect_window(&backend, commits)
}

/// Per-spec accumulator while folding links.
#[derive(Default)]
struct Acc {
    commits: HashSet<usize>,
    expected: usize,
    observed: usize,
    altitude: Option<Resolution>,
}

/// Build the churn-vs-drift report: map each window commit's paths onto the graph's files,
/// onto the link targets governing them (a target governs its subtree — propagate down, the
/// coverage report's rule), and onto the specs holding those links; cross with the store's
/// current drift signal. status: code-cli-churn-vs-drift
pub fn churn_report(
    graph: &CodeGraph,
    store: &LinkStore,
    source: &SourceId,
    provider: &dyn DerivedNodeSource,
    window: &[CommitChurn],
) -> ChurnReport {
    let targets: HashSet<&str> = store
        .links
        .iter()
        .filter(|l| l.source == source.0)
        .map(|l| l.target.as_str())
        .collect();

    // Files governed by each target: a node's file belongs to every target on its
    // self-or-ancestor chain. A link target absent from the graph governs no files here —
    // it reads MISSING in drift, which is its own (louder) signal.
    let mut files_of: HashMap<&str, HashSet<&str>> = HashMap::new();
    let mut governed_files: HashSet<&str> = HashSet::new();
    for n in &graph.nodes {
        let mut cur = Some(n);
        while let Some(node) = cur {
            if targets.contains(node.id.as_str()) {
                files_of.entry(node.id.as_str()).or_default().insert(n.file.as_str());
                governed_files.insert(n.file.as_str());
            }
            cur = node.parent.map(|p| &graph.nodes[p]);
        }
    }

    // Changed file -> the window commits that touched it.
    let mut commits_of: HashMap<&str, Vec<usize>> = HashMap::new();
    for (ci, c) in window.iter().enumerate() {
        for p in &c.paths {
            commits_of.entry(p.as_str()).or_default().push(ci);
        }
    }

    // Current drift signal, folded per (spec, target): any DRIFTED/MISSING link fires.
    let mut fired: HashMap<(String, String), bool> = HashMap::new();
    for r in store.check_drift(source, provider) {
        *fired.entry((r.spec, r.target)).or_default() |= r.drifted || r.missing;
    }

    let mut by_spec: BTreeMap<&str, Acc> = BTreeMap::new();
    for l in store.links.iter().filter(|l| l.source == source.0) {
        let acc = by_spec.entry(l.spec.as_str()).or_default();
        let mut churned = false;
        for f in files_of.get(l.target.as_str()).into_iter().flatten() {
            if let Some(cs) = commits_of.get(f) {
                churned = true;
                acc.commits.extend(cs);
            }
        }
        if churned {
            acc.expected += 1;
            let key = (l.spec.clone(), l.target.clone());
            if fired.get(&key).copied().unwrap_or(false) {
                acc.observed += 1;
            }
            acc.altitude = Some(acc.altitude.map_or(l.resolution, |a| a.max(l.resolution)));
        }
    }
    let mut specs: Vec<SpecChurn> = by_spec
        .into_iter()
        .map(|(spec, a)| SpecChurn {
            spec: spec.to_string(),
            commits: a.commits.len(),
            expected: a.expected,
            observed: a.observed,
            altitude: a.altitude,
        })
        .collect();
    specs.sort_by(|a, b| b.commits.cmp(&a.commits).then(a.spec.cmp(&b.spec)));

    // The silence proper: churned files the index knows but no spec governs.
    let graph_files: HashSet<&str> = graph.nodes.iter().map(|n| n.file.as_str()).collect();
    let mut ungoverned: Vec<FileChurn> = commits_of
        .iter()
        .filter(|(f, _)| graph_files.contains(**f) && !governed_files.contains(**f))
        .map(|(f, cs)| FileChurn { file: (*f).to_string(), commits: cs.len() })
        .collect();
    ungoverned.sort_by(|a, b| b.commits.cmp(&a.commits).then(a.file.cmp(&b.file)));
    let unindexed = commits_of.keys().filter(|f| !graph_files.contains(**f)).count();

    ChurnReport { commits: window.len(), specs, ungoverned, unindexed }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;

    use hiker_git::meta::{Author, Trailers};
    use hiker_git::repo::{GitBackend, Libgit2Backend};
    use spec_engine::{
        DerivedNodeSource, EdgeKind, Fingerprint, LinkStore, NodeHandle, Resolution, SourceCaps,
        SourceId, SourceLoc,
    };

    use super::{churn_report, churn_window, collect_window, CommitChurn};
    use crate::{CodeGraph, GraphNode};

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, body).unwrap();
    }

    fn commit(backend: &Libgit2Backend, paths: &[&str], subject: &str) {
        let paths: Vec<String> = paths.iter().map(ToString::to_string).collect();
        backend
            .commit_paths(&paths, subject, &Trailers::authored(Author::User), false)
            .unwrap()
            .expect("a commit was produced");
    }

    /// The window walks newest-first, diffs each commit against its first parent, and reads
    /// the root commit as zero churn (creation, not change).
    #[test]
    fn collect_window_diffs_each_commit_against_its_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = Libgit2Backend::open_or_init(tmp.path()).unwrap();
        write(tmp.path(), "a.rs", "fn a() {}\n");
        commit(&backend, &["a.rs"], "root");
        write(tmp.path(), "a.rs", "fn a() { /* v2 */ }\n");
        commit(&backend, &["a.rs"], "edit a");
        write(tmp.path(), "b.rs", "fn b() {}\n");
        commit(&backend, &["b.rs"], "add b");

        let w = collect_window(&backend, 2).unwrap();
        assert_eq!(w.len(), 2, "window capped at the request");
        assert_eq!((w[0].subject.as_str(), &w[0].paths[..]), ("add b", &["b.rs".to_string()][..]));
        assert_eq!((w[1].subject.as_str(), &w[1].paths[..]), ("edit a", &["a.rs".to_string()][..]));

        let all = collect_window(&backend, 10).unwrap();
        assert_eq!(all.len(), 3, "short history yields what exists");
        assert!(all[2].paths.is_empty(), "root commit contributes no churn");

        // The path-based wrapper sees the same window; a non-repo dir is refused, not init'd.
        assert_eq!(churn_window(tmp.path(), 2).unwrap().len(), 2);
        let plain = tempfile::tempdir().unwrap();
        assert!(churn_window(plain.path(), 2).is_err());
        assert!(!plain.path().join(".git").exists(), "report must never create a repo");
    }

    /// Fixed-fingerprint provider, enough to drive `check_drift` (synthetic links).
    struct Mock {
        fps: HashMap<String, Option<String>>,
    }

    impl Mock {
        fn new(fps: &[(&str, Option<&str>)]) -> Self {
            Mock { fps: fps.iter().map(|(k, v)| (k.to_string(), v.map(String::from))).collect() }
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
        fn neighbors(&self, _h: &NodeHandle, _k: &[EdgeKind]) -> Vec<NodeHandle> {
            Vec::new()
        }
        fn capabilities(&self) -> SourceCaps {
            SourceCaps::default()
        }
    }

    fn node(id: &str, kind: &str, file: &str, parent: Option<usize>) -> GraphNode {
        GraphNode {
            id: id.to_string(),
            name: id.to_string(),
            kind: kind.to_string(),
            file: file.to_string(),
            start_line: 0,
            lines: 1,
            parent,
        }
    }

    fn src() -> SourceId {
        SourceId("src".into())
    }

    fn handle(id: &str) -> NodeHandle {
        NodeHandle { source: src(), id: id.into() }
    }

    /// The classification matrix over a synthetic graph + links: a coarse touches link whose
    /// subtree churned with no drift is BLIND at its altitude; a Code link that churned and
    /// fired is watched; an unlinked hot file lands in the ungoverned section; out-of-index
    /// paths are counted, not silently dropped.
    #[test]
    fn churn_report_flags_blind_specs_and_ungoverned_files() {
        // hot (module, Container touches target) contains hot_fn; watched_fn pinned at Code;
        // free_fn ungoverned.
        let graph = CodeGraph {
            nodes: vec![
                node("hot", "code:module", "src/hot.rs", None),
                node("hot_fn", "code:function", "src/hot.rs", Some(0)),
                node("watched_fn", "code:function", "src/watched.rs", None),
                node("free_fn", "code:function", "src/free.rs", None),
            ],
            edges: vec![],
        };
        let baseline =
            Mock::new(&[("hot", Some("surface-v1")), ("watched_fn", Some("body-v1"))]);
        let mut store = LinkStore::default();
        store.add_link("spec-coarse", "touches", &handle("hot"), Resolution::Container, &baseline);
        store.add_link("spec-code", "implements", &handle("watched_fn"), Resolution::Code, &baseline);

        // Now: the watched body really changed; the coarse surface did not (body edits are
        // invisible at Container grain) — the governed-but-blind shape.
        let now = Mock::new(&[("hot", Some("surface-v1")), ("watched_fn", Some("body-v2"))]);
        let window = vec![
            CommitChurn {
                sha: "c1".into(),
                subject: "edit hot + free".into(),
                paths: vec!["src/hot.rs".into(), "src/free.rs".into()],
            },
            CommitChurn {
                sha: "c2".into(),
                subject: "edit watched".into(),
                paths: vec!["src/watched.rs".into(), "docs/spec.md".into()],
            },
            CommitChurn {
                sha: "c3".into(),
                subject: "edit free again".into(),
                paths: vec!["src/free.rs".into()],
            },
        ];
        let report = churn_report(&graph, &store, &src(), &now, &window);

        assert_eq!(report.commits, 3);
        let coarse = report.specs.iter().find(|s| s.spec == "spec-coarse").unwrap();
        assert_eq!((coarse.commits, coarse.expected, coarse.observed), (1, 1, 0));
        assert!(coarse.blind(), "targets churned, watch never fired");
        assert_eq!(coarse.altitude, Some(Resolution::Container), "the dial explaining it");
        let code = report.specs.iter().find(|s| s.spec == "spec-code").unwrap();
        assert_eq!((code.commits, code.expected, code.observed), (1, 1, 1));
        assert!(!code.blind(), "Code-grain watch fired on the body edit");

        assert_eq!(report.ungoverned.len(), 1, "only the unlinked in-index file");
        assert_eq!(report.ungoverned[0].file, "src/free.rs");
        assert_eq!(report.ungoverned[0].commits, 2, "both free-file commits counted");
        assert_eq!(report.unindexed, 1, "docs/spec.md is outside the index, said not dropped");
    }

    /// End-to-end over a real temp repo: the git window feeds the same report.
    #[test]
    fn churn_report_over_a_real_repo_window() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = Libgit2Backend::open_or_init(tmp.path()).unwrap();
        write(tmp.path(), "src/hot.rs", "fn hot() {}\n");
        write(tmp.path(), "src/free.rs", "fn free() {}\n");
        commit(&backend, &["src/hot.rs", "src/free.rs"], "root");
        write(tmp.path(), "src/hot.rs", "fn hot() { /* edited */ }\n");
        commit(&backend, &["src/hot.rs"], "edit hot");
        write(tmp.path(), "src/free.rs", "fn free() { /* edited */ }\n");
        commit(&backend, &["src/free.rs"], "edit free");

        let graph = CodeGraph {
            nodes: vec![
                node("hot", "code:module", "src/hot.rs", None),
                node("free_fn", "code:function", "src/free.rs", None),
            ],
            edges: vec![],
        };
        let provider = Mock::new(&[("hot", Some("surface-v1"))]);
        let mut store = LinkStore::default();
        store.add_link("spec-hot", "touches", &handle("hot"), Resolution::Container, &provider);

        let window = churn_window(tmp.path(), 10).unwrap();
        let report = churn_report(&graph, &store, &src(), &provider, &window);
        assert_eq!(report.commits, 3);
        let hot = report.specs.iter().find(|s| s.spec == "spec-hot").unwrap();
        assert_eq!((hot.commits, hot.expected, hot.observed), (1, 1, 0));
        assert!(hot.blind());
        assert_eq!(report.ungoverned[0].file, "src/free.rs");
        assert_eq!(report.ungoverned[0].commits, 1, "root creation is not churn");
    }
}
