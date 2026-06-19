//! The `kind: repo` code source descriptor: resolve its `repo_id`/index/scope/backend
//! (`repo-source`, `repo-id-git-derived`, `index-location-policy`, `index-staleness-tracking`).
//!
//! This is a **pure descriptor** — it does not instantiate any code-intelligence adapter (that
//! would couple the generic projects layer to `hiker-code`). A consumer reads `index`/`root`/
//! `repo_id`/`backend` off this struct and binds whatever adapter the backend calls for.

use std::path::{Path, PathBuf};

use crate::{git, glob, RawScope, RawSource};

/// Which code backend analyzes the repo. SCIP (batch) is primary; LSP (live) is later.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Scip,
    Lsp,
}

impl Backend {
    fn parse(s: Option<&str>) -> Backend {
        match s {
            Some("lsp") => Backend::Lsp,
            _ => Backend::Scip, // default + explicit "scip"
        }
    }
}

/// Include/exclude path globs narrowing which subtree of a (possibly mono-)repo the source
/// analyzes (`repo-subtree-scope`). A monorepo is *one* repo with a scope filter, not many repos.
#[derive(Debug, Clone, Default)]
pub struct Scope {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

impl Scope {
    fn from_raw(raw: RawScope) -> Scope {
        Scope { include: raw.include, exclude: raw.exclude }
    }

    /// Whether a repo-relative path is in scope: matches an `include` (or include is empty) and no
    /// `exclude`. Lets a caller post-filter `code_graph()` nodes by their `file`.
    pub fn accepts(&self, rel_path: &str) -> bool {
        let included =
            self.include.is_empty() || self.include.iter().any(|p| glob::matches(p, rel_path));
        let excluded = self.exclude.iter().any(|p| glob::matches(p, rel_path));
        included && !excluded
    }

    pub const fn is_empty(&self) -> bool {
        self.include.is_empty() && self.exclude.is_empty()
    }
}

/// A resolved code-source binding. `repo_id` is always concrete (frontmatter, else git-derived,
/// else path-based); `root`/`index` are `~`-expanded absolute-ish paths.
#[derive(Debug, Clone)]
pub struct Source {
    pub root: PathBuf,
    pub repo_id: String,
    pub backend: Backend,
    pub index: PathBuf,
    pub scope: Scope,
    /// The commit the index was built at, if recorded (drives `is_stale`).
    pub index_commit: Option<String>,
}

impl Source {
    pub(crate) fn from_raw(raw: RawSource) -> Source {
        let root = expand_tilde(raw.root.as_deref().unwrap_or("."));
        let index = raw
            .index
            .as_deref()
            .map(expand_tilde)
            // Default index location: sidecar `<root>.scip` is a reasonable out-of-the-way guess,
            // but the policy is "configured path"; callers should set `index:` explicitly.
            .unwrap_or_else(|| root.with_extension("scip"));
        let repo_id = raw.repo_id.clone().unwrap_or_else(|| resolve_repo_id(&root));
        Source {
            root,
            repo_id,
            backend: Backend::parse(raw.backend.as_deref()),
            index,
            scope: Scope::from_raw(raw.scope),
            index_commit: raw.index_commit,
        }
    }

    /// `Some(true)` if the working tree's HEAD has moved past the commit the index was built at
    /// (or the tree is dirty); `Some(false)` if current; `None` if staleness can't be determined
    /// (no recorded `index_commit`, or `root` isn't a git repo — `non-git-repo-fallback`).
    pub fn is_stale(&self) -> Option<bool> {
        let built_at = self.index_commit.as_deref()?;
        let head = git::head_commit(&self.root)?;
        if head != built_at {
            return Some(true);
        }
        // HEAD matches; a dirty tree still means the index may be behind the working copy.
        Some(git::is_dirty(&self.root).unwrap_or(false))
    }
}

/// Resolve a repo id with no frontmatter value: prefer the git-derived portable id, else fall back
/// to a path-based id (`non-git-repo-fallback`) so a loose folder is still a (degraded) repo source.
fn resolve_repo_id(root: &Path) -> String {
    git::repo_id(root).unwrap_or_else(|| path_based_id(root))
}

/// Last path component (or the full path) as a stable-enough id for non-git folders.
fn path_based_id(root: &Path) -> String {
    root.file_name()
        .and_then(|s| s.to_str())
        .map(str::to_string)
        .unwrap_or_else(|| root.to_string_lossy().into_owned())
}

/// Expand a leading `~` / `~/` to `$HOME`. Leaves other paths untouched.
fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    } else if p == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home);
        }
    }
    PathBuf::from(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_includes_and_excludes() {
        let s = Scope {
            include: vec!["src/**".into()],
            exclude: vec!["src/generated/**".into()],
        };
        assert!(s.accepts("src/main.rs"));
        assert!(s.accepts("src/a/b.rs"));
        assert!(!s.accepts("src/generated/x.rs"));
        assert!(!s.accepts("tests/t.rs"));
    }

    #[test]
    fn empty_scope_accepts_all() {
        let s = Scope::default();
        assert!(s.accepts("anything/at/all.rs"));
        assert!(s.is_empty());
    }

    #[test]
    fn backend_defaults_to_scip() {
        assert_eq!(Backend::parse(None), Backend::Scip);
        assert_eq!(Backend::parse(Some("scip")), Backend::Scip);
        assert_eq!(Backend::parse(Some("lsp")), Backend::Lsp);
    }
}
