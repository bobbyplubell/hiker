//! Binary-artifact retention for versioned/captured sources
//! (`extract-artifact-retention`). The layered doc versions a sidecar's *text*, not
//! its bytes — it can't hold the prior PDF or the prior HTML archive. Whether
//! those binary artifacts are kept across captures is a per-source retention
//! policy, resolved as a **cascade** (lower wins): the vault default
//! `[extract].artifact_retention` → a per-crawl / per-feed / per-glob override
//! (stamped onto captured pages) → the per-source `hiker.artifact_retention`
//! frontmatter on the sidecar itself.
//!
//! Retained per-capture artifacts live hidden under
//! `.hiker/refs/<vault-relative-path>/`, one subdirectory per capture that
//! produced them, so a capture's artifact is recoverable alongside that note.
//! They are **device-local** cache: nothing here syncs.
//!
//! This module owns the *policy resolution* + the *refs store* + *pruning*. It
//! lives in `core`, keyed by the note's vault-relative path (consistent with
//! path-identity, mirroring how `core::snapshot` keys `.hiker/history/<rel>/`),
//! independent of the `hiker-extract` leaf crate — the host hands it the
//! produced artifact bytes and the resolved frontmatter, and it stores /
//! prunes under the policy.
//
// status: extract-artifact-retention

use std::path::{Path, PathBuf};

/// A parsed binary-artifact retention bound. Resolved from the cascade string
/// (`latest` / `keep:N` / `forever`).
///
/// status: extract-artifact-retention
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retention {
    /// Keep only the current capture's artifact (the default). Earlier
    /// per-op artifact dirs are pruned on each new capture.
    Latest,
    /// Keep the last `N` captures' artifacts; prune older.
    Keep(usize),
    /// Never prune an artifact.
    Forever,
}

impl Retention {
    /// Parse one retention level: `latest` / `keep:N` / `forever`
    /// (case-insensitive). An unrecognized string falls back to `Latest` — the
    /// vault default — so a typo never silently keeps unbounded blobs *or*
    /// loses the current artifact (which `Latest` always retains).
    pub fn parse(s: &str) -> Self {
        let s = s.trim();
        if s.eq_ignore_ascii_case("latest") {
            return Retention::Latest;
        }
        if s.eq_ignore_ascii_case("forever") {
            return Retention::Forever;
        }
        if let Some(n) = s
            .strip_prefix("keep:")
            .and_then(|n| n.trim().parse::<usize>().ok())
        {
            return Retention::Keep(n);
        }
        Retention::Latest
    }

    /// How many captures' artifacts this policy keeps. `Latest` → 1, `Keep(n)`
    /// → n, `Forever` → unbounded (`None`). Drives [`prune_refs`].
    const fn keep_count(self) -> Option<usize> {
        match self {
            Retention::Latest => Some(1),
            Retention::Keep(n) => Some(n),
            Retention::Forever => None,
        }
    }
}

/// Resolve the artifact-retention cascade for one source (lower wins):
///
/// 1. `vault_default` — the vault `[extract].artifact_retention` value.
/// 2. `override_value` — a per-crawl / per-feed / per-glob override stamped
///    onto captured pages (the crawl/feed job's `artifact_retention`).
/// 3. `per_source` — the sidecar's own `hiker.artifact_retention` frontmatter.
///
/// The first non-empty value walking *up* the priority (per-source, then the
/// override, then the vault default) wins — i.e. the most specific level set
/// takes precedence. Each level is an `Option<&str>`; `None` (or an empty
/// string) defers to the next-broader level. With nothing set anywhere the
/// vault default's own fallback (`Retention::parse` → `Latest`) applies.
///
/// status: extract-artifact-retention
pub fn resolve_retention(
    vault_default: &str,
    override_value: Option<&str>,
    per_source: Option<&str>,
) -> Retention {
    let pick = per_source
        .filter(|s| !s.trim().is_empty())
        .or_else(|| override_value.filter(|s| !s.trim().is_empty()))
        .unwrap_or(vault_default);
    Retention::parse(pick)
}

/// The `.hiker/refs/<rel_path>/` directory for a note's retained artifacts.
/// Keyed by the note's vault-relative path (path-identity), mirroring how
/// `core::snapshot` keys `.hiker/history/<rel>/`.
fn refs_dir(vault_root: &Path, rel_path: &str) -> PathBuf {
    vault_root.join(".hiker").join("refs").join(rel_path)
}

/// Store one capture's binary artifact under
/// `.hiker/refs/<rel_path>/<capture_id>/`, keyed by the note's vault-relative
/// path and the capture that produced it (re-imports come from the producer's
/// manifest under manifest-only ingest). `filename` is the artifact's name
/// within that capture's directory (e.g. `original.html`, `source.pdf`).
/// Returns the absolute path written. status: manifest-only-ingest
///
/// Device-local: nothing here syncs.
///
/// status: extract-artifact-retention
pub fn store_artifact(
    vault_root: &Path,
    rel_path: &str,
    capture_id: &str,
    filename: &str,
    bytes: &[u8],
) -> std::io::Result<PathBuf> {
    let dir = refs_dir(vault_root, rel_path).join(capture_id);
    std::fs::create_dir_all(&dir)?;
    let dest = dir.join(filename);
    let tmp = dir.join(format!("{filename}.tmp"));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, &dest)?;
    Ok(dest)
}

/// Prune `.hiker/refs/<rel_path>/` to the resolved retention policy,
/// newest-first by directory mtime: keep the N most-recent per-capture artifact
/// directories and remove the rest. `Forever` prunes nothing. Returns the
/// capture directory names that were removed.
///
/// Per-capture directory recency is read from each subdirectory's mtime (mtime
/// is used so a touched/restored dir sorts as recent). A missing refs dir
/// prunes nothing.
///
/// status: extract-artifact-retention
pub fn prune_refs(
    vault_root: &Path,
    rel_path: &str,
    retention: Retention,
) -> std::io::Result<Vec<String>> {
    let Some(keep) = retention.keep_count() else {
        return Ok(Vec::new()); // Forever
    };
    let dir = refs_dir(vault_root, rel_path);
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    // Collect (capture_dir_name, mtime) for each per-capture subdirectory.
    let mut entries: Vec<(String, std::time::SystemTime)> = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        entries.push((name, mtime));
    }
    if entries.len() <= keep {
        return Ok(Vec::new());
    }
    // Newest-first; the oldest beyond `keep` are pruned.
    entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.0.cmp(&a.0)));
    let mut removed = Vec::new();
    for (name, _) in entries.into_iter().skip(keep) {
        std::fs::remove_dir_all(dir.join(&name))?;
        removed.push(name);
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn parses_retention_levels() {
        // status: extract-artifact-retention
        assert_eq!(Retention::parse("latest"), Retention::Latest);
        assert_eq!(Retention::parse("LATEST"), Retention::Latest);
        assert_eq!(Retention::parse("forever"), Retention::Forever);
        assert_eq!(Retention::parse("keep:3"), Retention::Keep(3));
        assert_eq!(Retention::parse("keep: 5 "), Retention::Keep(5));
        // Unknown → fail-safe Latest (keeps the current artifact, bounds blobs).
        assert_eq!(Retention::parse("bogus"), Retention::Latest);
    }

    #[test]
    fn cascade_lower_level_wins() {
        // status: extract-artifact-retention
        // Per-source frontmatter beats the per-crawl override beats the vault default.
        assert_eq!(
            resolve_retention("latest", Some("keep:5"), Some("forever")),
            Retention::Forever
        );
        // Per-source empty → defer to the override.
        assert_eq!(
            resolve_retention("latest", Some("keep:5"), Some("")),
            Retention::Keep(5)
        );
        assert_eq!(
            resolve_retention("latest", Some("keep:5"), None),
            Retention::Keep(5)
        );
        // Nothing set above the vault default → the vault default.
        assert_eq!(
            resolve_retention("keep:2", None, None),
            Retention::Keep(2)
        );
        // Empty everywhere → the vault default's own parse fallback.
        assert_eq!(resolve_retention("latest", None, None), Retention::Latest);
    }

    /// Set a per-capture refs directory's mtime to a deterministic instant so
    /// the recency ordering in [`prune_refs`] is stable regardless of how coarse
    /// the filesystem's timestamp granularity is. Uses the std-stable
    /// `File::set_modified` (no extra dependency).
    fn touch_capture(root: &Path, rel_path: &str, cap: &str, secs: u64) {
        let capdir = root.join(".hiker/refs").join(rel_path).join(cap);
        let f = std::fs::File::open(&capdir).unwrap();
        let when = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs);
        f.set_modified(when).unwrap();
    }

    #[test]
    fn store_and_prune_to_latest_keeps_only_current() {
        // status: extract-artifact-retention
        let dir = tempdir().unwrap();
        let root = dir.path();
        // Path-keyed: a note in a subfolder, three captures oldest-first. The
        // nested `notes/page.md` rel-path exercises the path-identity keying.
        let rel = "notes/page.md";
        for (i, cap) in ["cap-a", "cap-b", "cap-c"].iter().enumerate() {
            store_artifact(root, rel, cap, "original.html", format!("v{i}").as_bytes())
                .unwrap();
            // Bump mtime monotonically so the recency ordering is deterministic
            // independent of filesystem timestamp granularity.
            touch_capture(root, rel, cap, 100 + i as u64);
        }
        let removed = prune_refs(root, rel, Retention::Latest).unwrap();
        // Only the newest (`cap-c`) survives.
        assert_eq!(removed.len(), 2);
        assert!(removed.contains(&"cap-a".to_string()));
        assert!(removed.contains(&"cap-b".to_string()));
        assert!(root.join(".hiker/refs/notes/page.md/cap-c/original.html").exists());
        assert!(!root.join(".hiker/refs/notes/page.md/cap-a").exists());
    }

    #[test]
    fn prune_keep_n_retains_n_newest() {
        // status: extract-artifact-retention
        let dir = tempdir().unwrap();
        let root = dir.path();
        for (i, op) in ["o1", "o2", "o3", "o4"].iter().enumerate() {
            store_artifact(root, "d", op, "a.bin", b"x").unwrap();
            touch_capture(root, "d", op, 10 + i as u64);
        }
        let removed = prune_refs(root, "d", Retention::Keep(2)).unwrap();
        assert_eq!(removed.len(), 2);
        assert!(removed.contains(&"o1".to_string()));
        assert!(removed.contains(&"o2".to_string()));
        assert!(root.join(".hiker/refs/d/o3").exists());
        assert!(root.join(".hiker/refs/d/o4").exists());
    }

    #[test]
    fn prune_forever_removes_nothing() {
        // status: extract-artifact-retention
        let dir = tempdir().unwrap();
        let root = dir.path();
        store_artifact(root, "d", "o1", "a.bin", b"x").unwrap();
        store_artifact(root, "d", "o2", "a.bin", b"x").unwrap();
        let removed = prune_refs(root, "d", Retention::Forever).unwrap();
        assert!(removed.is_empty());
        assert!(root.join(".hiker/refs/d/o1").exists());
        assert!(root.join(".hiker/refs/d/o2").exists());
    }
}
