//! Thin, **read-only** git metadata (`project-git-metadata`). The only git hiker-projects needs:
//! current commit (for `repo_id` provenance + staleness) and the remote URL / root-commit (for the
//! portable id). No history walking, no checkouts, no mutation. Implemented by shelling out to the
//! `git` binary for read commands; absence of git (or of a `.git`) degrades gracefully to `None`.

use std::path::Path;
use std::process::Command;

/// Run a read-only `git -C <root> <args…>` and return trimmed stdout on success.
fn git_read(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(root).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Current `HEAD` commit hash (`None` if not a git repo).
pub fn head_commit(root: &Path) -> Option<String> {
    git_read(root, &["rev-parse", "HEAD"])
}

/// The repo's first root-commit hash — the most portable, durable id (survives remote/url changes).
/// Takes the first line if history has multiple roots.
pub fn root_commit(root: &Path) -> Option<String> {
    let out = git_read(root, &["rev-list", "--max-parents=0", "HEAD"])?;
    out.lines().next().map(str::to_string)
}

/// The `origin` remote URL, normalized (strip a trailing `.git`, lowercase host left intact).
pub fn remote_url(root: &Path) -> Option<String> {
    let url = git_read(root, &["config", "--get", "remote.origin.url"])?;
    Some(url.strip_suffix(".git").unwrap_or(&url).to_string())
}

/// Whether the working tree has uncommitted changes (porcelain non-empty).
pub fn is_dirty(root: &Path) -> Option<bool> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["status", "--porcelain"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(!String::from_utf8_lossy(&out.stdout).trim().is_empty())
}

/// The portable, git-derived `repo_id` (`repo-id-git-derived`): prefer the root-commit hash (most
/// durable), else the normalized remote URL. `None` when `root` is not a git repo.
pub fn repo_id(root: &Path) -> Option<String> {
    root_commit(root).or_else(|| remote_url(root))
}
