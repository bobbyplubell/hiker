//! hiker-projects — the sourcing/binding layer (`docs/hiker-projects.md`).
//!
//! A **project** is a vault note (`hiker.kind: project`) whose frontmatter binds N external
//! **sources**: a code **repo** (→ `hiker-code`'s SCIP adapter), and later a read-only Jira
//! mirror or a docs folder. This crate owns *where* a source lives and *how to reach it* — never
//! *what it means* (that's each adapter + the spec-engine graph).
//!
//! v1 resolves the **repo** source into a typed descriptor (parse → `repo_id`/index/scope/backend)
//! and recognizes other source kinds as `Unsupported` placeholders. It is **dependency-light and
//! decoupled from code intelligence**: it never instantiates an adapter — a consumer reads the
//! descriptor and binds whatever the backend calls for (the `repo`→SCIP binding lives in the app,
//! not here). The parsing is UI-free (no hiker `core` dep), so the CLI and the app both reuse it.

pub mod git;
pub mod glob;
mod repo;

pub use repo::{Backend, RepoSource, Scope};

use std::path::{Path, PathBuf};

/// A bound project: a note path + its resolved source bindings.
#[derive(Debug, Clone)]
pub struct Project {
    pub note_path: PathBuf,
    pub sources: Vec<SourceBinding>,
}

/// One resolved source from a project note's `sources[]` list.
#[derive(Debug, Clone)]
pub enum SourceBinding {
    /// A git-anchored code source (`kind: repo`) → analyzed by hiker-code.
    Repo(RepoSource),
    /// A recognized but not-yet-implemented source kind (`jira`, `docs`, …). Kept so a project
    /// note can declare the full source set today; bind these as adapters land.
    Unsupported { kind: String },
}

/// Errors from loading/parsing a project note.
#[derive(Debug)]
pub enum ProjectError {
    Io(std::io::Error),
    /// No `---`-delimited YAML frontmatter block at the top of the note.
    MissingFrontmatter,
    /// Frontmatter parsed but `hiker.kind` was not `project`.
    NotAProject,
    Yaml(serde_yml::Error),
}

impl std::fmt::Display for ProjectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProjectError::Io(e) => write!(f, "io: {e}"),
            ProjectError::MissingFrontmatter => write!(f, "no `---` YAML frontmatter in note"),
            ProjectError::NotAProject => write!(f, "note is not `hiker.kind: project`"),
            ProjectError::Yaml(e) => write!(f, "frontmatter yaml: {e}"),
        }
    }
}
impl std::error::Error for ProjectError {}
impl From<std::io::Error> for ProjectError {
    fn from(e: std::io::Error) -> Self {
        ProjectError::Io(e)
    }
}

impl Project {
    /// Parse a project note: read its file, extract the leading `---` YAML frontmatter, validate
    /// `hiker.kind: project`, and resolve every `sources[]` entry. Repo `repo_id`s are resolved
    /// here (frontmatter value, else git-derived, else path-based fallback — see
    /// [`RepoSource`]/`repo-id-git-derived`), and `~`-prefixed paths are expanded.
    pub fn load(note_path: &Path) -> Result<Self, ProjectError> {
        let text = std::fs::read_to_string(note_path)?;
        Self::parse(&text, note_path)
    }

    /// Parse an already-read note body (separated from [`load`] for testability).
    pub fn parse(text: &str, note_path: &Path) -> Result<Self, ProjectError> {
        let fm = extract_frontmatter(text).ok_or(ProjectError::MissingFrontmatter)?;
        let raw: RawFrontmatter = serde_yml::from_str(fm).map_err(ProjectError::Yaml)?;
        if raw.kind() != Some("project") {
            return Err(ProjectError::NotAProject);
        }
        let sources = raw.sources.into_iter().map(SourceBinding::from_raw).collect();
        Ok(Project { note_path: note_path.to_path_buf(), sources })
    }

    /// Just the repo sources (the v1 graph sources).
    pub fn repo_sources(&self) -> impl Iterator<Item = &RepoSource> {
        self.sources.iter().filter_map(|s| match s {
            SourceBinding::Repo(r) => Some(r),
            _ => None,
        })
    }
}

impl SourceBinding {
    fn from_raw(raw: RawSource) -> SourceBinding {
        match raw.kind.as_str() {
            "repo" => SourceBinding::Repo(RepoSource::from_raw(raw)),
            other => SourceBinding::Unsupported { kind: other.to_string() },
        }
    }
}

/// Extract the YAML between a leading `---` fence and the next `---` line. Returns `None` if the
/// note does not start with a frontmatter fence (after optional leading blank lines / BOM).
fn extract_frontmatter(text: &str) -> Option<&str> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.lines();
    // First non-empty line must be the opening fence.
    let mut opened = false;
    for l in lines.by_ref() {
        if l.trim().is_empty() {
            continue;
        }
        opened = l.trim_end() == "---";
        break;
    }
    if !opened {
        return None;
    }
    // Find the byte offset of the body just past the opening fence, then the closing fence.
    let after_open = text.find("---")? + 3;
    let rest = &text[after_open..];
    let rest = rest.strip_prefix('\n').or_else(|| rest.strip_prefix("\r\n")).unwrap_or(rest);
    let close = rest.find("\n---")?;
    Some(&rest[..close])
}

// --- frontmatter wire types (private; mapped to the public model above) ---

#[derive(serde::Deserialize)]
struct RawFrontmatter {
    /// Flat dotted key (`hiker.kind: project`) — the form in this crate's docs/fixtures.
    #[serde(rename = "hiker.kind")]
    kind_flat: Option<String>,
    /// Nested map (`hiker:\n  kind: project`) — the form hiker's own notes use (its frontmatter
    /// index flattens it back to the dotted `hiker.kind` for queries). Accept both.
    hiker: Option<HikerMeta>,
    #[serde(default)]
    sources: Vec<RawSource>,
}

#[derive(serde::Deserialize)]
struct HikerMeta {
    kind: Option<String>,
}

impl RawFrontmatter {
    fn kind(&self) -> Option<&str> {
        self.kind_flat
            .as_deref()
            .or_else(|| self.hiker.as_ref().and_then(|h| h.kind.as_deref()))
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct RawSource {
    pub kind: String,
    pub root: Option<String>,
    pub repo_id: Option<String>,
    pub backend: Option<String>,
    pub index: Option<String>,
    #[serde(default)]
    pub scope: RawScope,
    /// Provenance for index-staleness: the commit the `.scip` was built at (`index-staleness-tracking`).
    pub index_commit: Option<String>,
}

#[derive(serde::Deserialize, Default)]
pub(crate) struct RawScope {
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLAT: &str = "---\nhiker.kind: project\nsources:\n  - kind: repo\n    root: /x\n    repo_id: rx\n    index: /x.scip\n---\nbody\n";
    const NESTED: &str = "---\nhiker:\n  kind: project\nsources:\n  - kind: repo\n    root: /x\n    repo_id: rx\n    index: /x.scip\n  - kind: docs\n    root: /x/docs\n---\nbody\n";

    #[test]
    fn parses_flat_and_nested_kind() {
        for text in [FLAT, NESTED] {
            let p = Project::parse(text, Path::new("p.md")).expect("parse");
            let repo = p.repo_sources().next().expect("repo source");
            assert_eq!(repo.repo_id, "rx");
        }
    }

    #[test]
    fn nested_keeps_other_sources() {
        let p = Project::parse(NESTED, Path::new("p.md")).unwrap();
        assert!(matches!(p.sources.get(1), Some(SourceBinding::Unsupported { kind }) if kind == "docs"));
    }

    #[test]
    fn rejects_non_project() {
        let text = "---\nhiker:\n  kind: board\n---\n";
        assert!(matches!(Project::parse(text, Path::new("p.md")), Err(ProjectError::NotAProject)));
    }
}
